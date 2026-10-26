#!/usr/bin/env python3
"""Pyth-aware shred subscriber: parses Solana bincode Entries from
shredstream-proxy gRPC, finds Pyth Push v2 Receiver transactions,
extracts the PostUpdateAtomic instruction (vaa + merkle_price_update),
and writes structured dumps that the Rust arb-scout reads to build
atomic Pyth-paired liquidations.

Output format (binary file at /tmp/shred-pyth-atomic.bin):
  magic   : 4 bytes "PYTH"
  version : u8 = 1
  slot    : u64 LE
  ts_ms   : u64 LE
  oracle  : 32 bytes (price_update_account from the Pyth tx accounts)
  ix_len  : u32 LE
  ix_data : ix_len bytes (the PostUpdateAtomic instruction data — discrim + params)

Path is symlinked to /tmp/shred-pyth-latest.bin atomically.
"""
import os, sys, time, struct, signal
sys.path.insert(0, '/home/ubuntu/shred-client')
import grpc
import shredstream_pb2
import shredstream_pb2_grpc

PYTH_PUSH_V2_RECEIVER = 'pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT'

_B58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
def b58dec(s):
    n = 0
    for c in s: n = n * 58 + _B58.index(c)
    b = n.to_bytes((n.bit_length()+7)//8, 'big')
    return b'\x00'*(len(s)-len(s.lstrip('1'))) + b

PYTH_RECEIVER_BYTES = b58dec(PYTH_PUSH_V2_RECEIVER)
# Verified discriminators from sha256("global:<ix_name>")[:8]:
POST_UPDATE_ATOMIC_DISC = bytes.fromhex('31ac54c0afb434ea')
POST_UPDATE_DISC        = bytes.fromhex('855fcfaf0b4f762c')

def read_cu16(data, off):
    """Solana compact-u16 (varint, 1-3 bytes)."""
    b = data[off]; off += 1
    if b & 0x80 == 0: return b, off
    val = b & 0x7F
    b = data[off]; off += 1
    if b & 0x80 == 0: return val | (b << 7), off
    val |= (b & 0x7F) << 7
    b = data[off]; off += 1
    return val | (b << 14), off

def read_le_u64(data, off):
    return struct.unpack_from('<Q', data, off)[0], off + 8

def parse_versioned_tx(data, start):
    """Parse one VersionedTransaction from `data` starting at `start`.
    Returns (end_offset, account_keys: list[bytes], instructions: list[(pid_idx, accounts_idx_list, ix_data_bytes)]).
    Raises on malformed.
    """
    off = start
    # signatures
    sig_count, off = read_cu16(data, off)
    off += sig_count * 64
    # message version
    first = data[off]
    is_v0 = (first & 0x80) != 0
    if is_v0:
        off += 1  # consume version byte
    # msg_header
    if off + 3 > len(data):
        raise ValueError(f"short header at {off}")
    off += 3
    # account_keys
    n_acc, off = read_cu16(data, off)
    accounts = []
    for _ in range(n_acc):
        accounts.append(bytes(data[off:off+32]))
        off += 32
    # recent blockhash
    off += 32
    # instructions
    n_ix, off = read_cu16(data, off)
    ixs = []
    for _ in range(n_ix):
        pid_idx = data[off]; off += 1
        n_ai, off = read_cu16(data, off)
        ai = list(data[off:off+n_ai]); off += n_ai
        n_d, off = read_cu16(data, off)
        d = bytes(data[off:off+n_d]); off += n_d
        ixs.append((pid_idx, ai, d))
    if is_v0:
        # address_table_lookups
        n_alt, off = read_cu16(data, off)
        for _ in range(n_alt):
            off += 32  # account_key
            n_w, off = read_cu16(data, off)
            off += n_w
            n_r, off = read_cu16(data, off)
            off += n_r
    return off, accounts, ixs

def parse_entry_payload(payload):
    """Walk bincode Vec<Entry>; yields (account_keys, instructions) per tx."""
    off = 0
    n_entries, off = read_le_u64(payload, off)
    for _ in range(n_entries):
        # num_hashes (u64)
        off += 8
        # hash (32 bytes)
        off += 32
        # transactions: Vec<VersionedTransaction> with u64 LE count prefix
        n_tx, off = read_le_u64(payload, off)
        for _ in range(n_tx):
            try:
                new_off, accs, ixs = parse_versioned_tx(payload, off)
                yield accs, ixs
                off = new_off
            except Exception:
                return  # malformed — bail on this entry batch

def find_pyth_atomic_update(accs, ixs):
    """Returns (oracle_pubkey_bytes, ix_data_bytes) if this tx contains a
    Pyth PostUpdateAtomic call on an oracle account; else None.
    """
    for pid_idx, ai, data in ixs:
        if pid_idx >= len(accs): continue
        if accs[pid_idx] != PYTH_RECEIVER_BYTES: continue
        if len(data) < 8: continue
        if data[:8] != POST_UPDATE_ATOMIC_DISC and data[:8] != POST_UPDATE_DISC: continue
        # PostUpdateAtomic accounts (per pyth-solana-receiver-sdk v0.6):
        #   0: payer (signer)
        #   1: guardian_set
        #   2: config
        #   3: treasury
        #   4: price_update_account  ← this is what Solend reads
        #   5: system_program
        if len(ai) < 5: continue
        oracle = accs[ai[4]] if ai[4] < len(accs) else None
        if oracle is None: continue
        return oracle, data
    return None

OUT_DIR = '/tmp/shred-pyth-atomic'
OUT_LATEST = '/tmp/shred-pyth-atomic.bin'
os.makedirs(OUT_DIR, exist_ok=True)

def write_atomic_dump(slot, ts, oracle_bytes, ix_data):
    payload = b'PYTH' + bytes([1]) + struct.pack('<Q', slot) + struct.pack('<Q', int(ts*1000)) + oracle_bytes + struct.pack('<I', len(ix_data)) + ix_data
    fname = f"{OUT_DIR}/{slot}-{int(ts*1000)}.bin"
    with open(fname, 'wb') as f:
        f.write(payload)
    tmp = OUT_LATEST + '.new'
    try:
        with open(tmp, 'wb') as f: f.write(payload)
        os.rename(tmp, OUT_LATEST)
    except Exception:
        pass
    # Trim old.
    try:
        files = sorted(os.listdir(OUT_DIR))
        for old in files[:-32]: os.unlink(os.path.join(OUT_DIR, old))
    except Exception: pass

stats = {'entries': 0, 'pyth_txs': 0, 'atomic_dumps': 0, 'parse_errs': 0, 'start': time.time()}

def report():
    el = time.time() - stats['start']
    if el < 1: return
    print(f"[+{el:>6.1f}s] entries={stats['entries']} gate_hits={stats.get('gate_hits',0)} "
          f"pyth_txs={stats['pyth_txs']} atomic_dumps={stats['atomic_dumps']} parse_errs={stats['parse_errs']}", flush=True)

def main():
    chan = grpc.insecure_channel('localhost:9000', options=[
        ('grpc.max_receive_message_length', 64*1024*1024),
    ])
    stub = shredstream_pb2_grpc.ShredstreamProxyStub(chan)
    req = shredstream_pb2.SubscribeEntriesRequest()
    print("connecting to localhost:9000...", flush=True)
    last_report = time.time()
    for entry in stub.SubscribeEntries(req):
        stats['entries'] += 1
        now = time.time()
        if now - last_report > 5:
            report(); last_report = now
        # Quick gate: scan for the discriminator. The Pyth program ID itself
        # is often hidden behind an Address Lookup Table, so scanning for the
        # program pubkey misses most updates. The IX data bytes always carry
        # the discriminator regardless of ALT use.
        if POST_UPDATE_ATOMIC_DISC not in entry.entries and POST_UPDATE_DISC not in entry.entries:
            continue
        stats['gate_hits'] = stats.get('gate_hits', 0) + 1
        try:
            for accs, ixs in parse_entry_payload(entry.entries):
                hit = find_pyth_atomic_update(accs, ixs)
                if hit:
                    stats['pyth_txs'] += 1
                    oracle, ix_data = hit
                    now = time.time()
                    write_atomic_dump(entry.slot, now, oracle, ix_data)
                    stats['atomic_dumps'] += 1
                    print(f"  💎 PYTH-ATOMIC slot={entry.slot} oracle={oracle.hex()[:16]} ix_len={len(ix_data)}", flush=True)
        except Exception as e:
            stats['parse_errs'] += 1

if __name__ == '__main__':
    main()
