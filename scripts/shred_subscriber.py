#!/usr/bin/env python3
"""Subscribe to local shredstream-proxy gRPC on :9000 and bridge to arb-scout.

Phase A: prove shreds flow.
Phase B: detect Pyth/Solend program activity.
Phase C: dump raw Pyth tx entry payload so Rust can extract the VAA and
include it in an atomic Pyth-paired Solend liquidation bundle.
"""
import os, sys, time, struct, base64
sys.path.insert(0, '/home/ubuntu/shred-client')
import grpc
import shredstream_pb2
import shredstream_pb2_grpc

_B58_ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
def b58decode(s):
    n = 0
    for c in s: n = n * 58 + _B58_ALPHABET.index(c)
    b = n.to_bytes((n.bit_length()+7)//8, 'big')
    pad = len(s) - len(s.lstrip('1'))
    return b'\x00' * pad + b

PYTH_PROGRAMS_B58 = [
    'pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT',  # Pyth Push v2 Receiver
    'rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ',
    'FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH',
]
PYTH_PROGRAM_BYTES = [b58decode(p) for p in PYTH_PROGRAMS_B58]
SOLEND_PROGRAM_BYTES = b58decode('So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo')

stats = {'entries': 0, 'slots_seen': set(), 'bytes': 0, 'start': time.time(),
         'pyth_hits': 0, 'solend_hits': 0, 'dumps_written': 0}

def report():
    el = time.time() - stats['start']
    if el < 1: return
    print(f"[+{el:>6.1f}s] entries={stats['entries']} slots={len(stats['slots_seen'])} "
          f"bytes={stats['bytes']:,} pyth={stats['pyth_hits']} solend={stats['solend_hits']} "
          f"dumps={stats['dumps_written']}", flush=True)

def scan_for_pyth(data):
    return any(p in data for p in PYTH_PROGRAM_BYTES)

def scan_for_solend(data):
    return SOLEND_PROGRAM_BYTES in data

PYTH_DUMP_DIR = '/tmp/shred-pyth-dumps'
os.makedirs(PYTH_DUMP_DIR, exist_ok=True)

# Keep only the latest N dumps to bound disk usage.
MAX_DUMPS = 64

def write_pyth_dump(slot, ts, entry_bytes):
    """Write a binary frame the Rust side can read.

    Format: u64 LE slot | f64 LE ts | u32 LE payload_len | payload_bytes
    Filename includes slot+ts for ordering, latest-wins via /tmp/shred-pyth-latest symlink.
    """
    fname = f"{PYTH_DUMP_DIR}/{slot}-{int(ts*1000)}.bin"
    with open(fname, 'wb') as f:
        f.write(struct.pack('<Q', slot))
        f.write(struct.pack('<d', ts))
        f.write(struct.pack('<I', len(entry_bytes)))
        f.write(entry_bytes)
    # Atomic-ish symlink update so the Rust reader always finds the freshest dump.
    tmp_link = '/tmp/shred-pyth-latest.new'
    try:
        if os.path.lexists(tmp_link): os.unlink(tmp_link)
        os.symlink(fname, tmp_link)
        os.rename(tmp_link, '/tmp/shred-pyth-latest')
    except Exception:
        pass
    # Trim old dumps.
    try:
        dumps = sorted(os.listdir(PYTH_DUMP_DIR))
        for old in dumps[:-MAX_DUMPS]:
            os.unlink(os.path.join(PYTH_DUMP_DIR, old))
    except Exception:
        pass

def main():
    chan = grpc.insecure_channel('localhost:9000')
    stub = shredstream_pb2_grpc.ShredstreamProxyStub(chan)
    req = shredstream_pb2.SubscribeEntriesRequest()
    print(f"connecting to localhost:9000...", flush=True)
    last_report = time.time()
    try:
        for entry in stub.SubscribeEntries(req):
            stats['entries'] += 1
            stats['slots_seen'].add(entry.slot)
            stats['bytes'] += len(entry.entries)
            now = time.time()
            if scan_for_pyth(entry.entries):
                stats['pyth_hits'] += 1
                # Legacy alert file (compat).
                try:
                    with open('/tmp/shred-pyth-alert', 'w') as f:
                        f.write(f"{entry.slot} {now:.6f}\n")
                except Exception:
                    pass
                # Phase C: full binary dump for Rust VAA extraction.
                write_pyth_dump(entry.slot, now, entry.entries)
                stats['dumps_written'] += 1
                print(f"  🎯 PYTH slot={entry.slot} ts={now:.3f} dump-len={len(entry.entries)}", flush=True)
            if scan_for_solend(entry.entries):
                stats['solend_hits'] += 1
                try:
                    with open('/tmp/shred-solend-alert', 'w') as f:
                        f.write(f"{entry.slot} {now:.6f}\n")
                except Exception:
                    pass
                print(f"  💧 SOLEND slot={entry.slot} ts={now:.3f}", flush=True)
            if now - last_report > 5:
                report(); last_report = now
    except grpc.RpcError as e:
        print(f"gRPC error: {e}", flush=True)
    except KeyboardInterrupt:
        report()

if __name__ == '__main__':
    main()
