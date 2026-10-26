#!/usr/bin/env python3
"""Parse Vec<solana_entry::Entry> bytes (from shredstream-proxy gRPC).

Format (verified empirically against live shreds):
  - Outer Vec<Entry>: bincode default (u64 LE length + elements)
  - Entry { num_hashes: u64, hash: [u8;32], transactions: Vec<VersionedTransaction> }
    - Inner Vec<VersionedTransaction>: u64 LE length (also bincode)
  - VersionedTransaction itself: serialized in WIRE format (compact-u16, version byte)
    - This matches solana_sdk's custom serde impl that produces network-wire bytes
"""
import struct, sys, hashlib

PYTH_RECEIVER = bytes.fromhex('0c4aa0128e95d3e1622aa501c585a9eb07b37354c108ea0b791b456dc7eea336')

def u64(d, o): return struct.unpack_from('<Q', d, o)[0]

def read_cu16(d, o):
    b = d[o]; o += 1
    if b & 0x80 == 0: return b, o
    val = b & 0x7F
    b = d[o]; o += 1
    if b & 0x80 == 0: return val | (b << 7), o
    val |= (b & 0x7F) << 7
    b = d[o]; o += 1
    return val | (b << 14), o

def parse_versioned_tx(d, o):
    """Returns (off, version, accounts, ixs).
       version: 0 for legacy, 1+ for v0..."""
    n_sig, o = read_cu16(d, o)
    o += n_sig * 64
    # peek version byte
    first = d[o]
    if first & 0x80:
        version = (first & 0x7F)
        o += 1
    else:
        version = -1  # legacy, no version byte
    # MessageHeader: 3 bytes
    o += 3
    # account_keys
    n_acc, o = read_cu16(d, o)
    accs = [bytes(d[o+i*32:o+(i+1)*32]) for i in range(n_acc)]
    o += n_acc * 32
    # recent blockhash
    o += 32
    # instructions
    n_ix, o = read_cu16(d, o)
    ixs = []
    for _ in range(n_ix):
        pid = d[o]; o += 1
        n_ai, o = read_cu16(d, o)
        ai = list(d[o:o+n_ai]); o += n_ai
        n_data, o = read_cu16(d, o)
        data = bytes(d[o:o+n_data]); o += n_data
        ixs.append((pid, ai, data))
    if version >= 0:  # v0+ has ALTs
        n_alt, o = read_cu16(d, o)
        for _ in range(n_alt):
            o += 32
            n_w, o = read_cu16(d, o)
            o += n_w
            n_r, o = read_cu16(d, o)
            o += n_r
    return o, version, accs, ixs

def parse_entries(d):
    o = 0
    n_entries = u64(d, o); o += 8
    for ei in range(n_entries):
        o += 8  # num_hashes
        o += 32  # hash
        n_tx = u64(d, o); o += 8
        for ti in range(n_tx):
            o, version, accs, ixs = parse_versioned_tx(d, o)
            yield ei, ti, version, accs, ixs

if __name__ == '__main__':
    path = sys.argv[1]
    data = open(path, 'rb').read()
    print(f"# {path} size={len(data)}")
    pyth_count = 0
    txs_seen = 0
    for ei, ti, ver, accs, ixs in parse_entries(data):
        txs_seen += 1
        for pid_idx, ai, ix_data in ixs:
            if pid_idx < len(accs) and accs[pid_idx] == PYTH_RECEIVER:
                pyth_count += 1
                disc = ix_data[:8] if len(ix_data) >= 8 else b''
                oracle = accs[ai[4]] if len(ai) >= 5 and ai[4] < len(accs) else None
                print(f"  entry={ei} tx={ti} ver={ver} disc={disc.hex()} ix_len={len(ix_data)} n_accs={len(accs)} ix_acc_count={len(ai)}")
                if oracle: print(f"    oracle = {oracle.hex()[:16]}...  full={oracle.hex()}")
    print(f"# total txs seen: {txs_seen}, Pyth hits: {pyth_count}")
