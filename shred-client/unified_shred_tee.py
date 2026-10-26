#!/usr/bin/env python3
"""Unified shred tee — single subscriber to shredstream-proxy gRPC :9000.

Substring-scans every Entry for known program-id byte sequences. When a
match fires, atomically writes a one-line alert file that Rust hunters poll.

Outputs (atomic .tmp + rename, single most-recent event per file):
  /tmp/shred-pumpswap-alert     — slot ts                — PumpSwap activity
  /tmp/shred-pyth-alert         — slot ts                — Pyth Push V2 update
  /tmp/shred-kamino-alert       — slot ts                — Kamino lending activity
  /tmp/shred-marginfi-alert     — slot ts                — MarginFi lending activity

This is Phase A — substring detection only, no per-pool / per-oracle granularity.
Phase D (Rust shred client) will replace this with full tx deserialization.
"""
import os, sys, time, signal
sys.path.insert(0, '/home/ubuntu/shred-client')
import grpc, shredstream_pb2, shredstream_pb2_grpc

# Base58 (no external deps)
_B58 = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
def b58decode(s):
    n = 0
    for c in s: n = n * 58 + _B58.index(c)
    b = n.to_bytes((n.bit_length()+7)//8, 'big')
    pad = len(s) - len(s.lstrip('1'))
    return b'\x00' * pad + b

# Program IDs (binary form for substring match)
PROGRAMS = {
    'pumpswap':       'pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA',
    'pyth':           'pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT',
    'kamino':         'KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD',
    'marginfi':       'MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA',
}
PROGRAM_BYTES = {k: b58decode(v) for k, v in PROGRAMS.items()}

# Secondary filter: byte sequences that, in addition to the program-id match,
# distinguish a specific event we care about. Reduces alerts from "any activity"
# to "the activity that matters", so the Rust poller doesn't get flooded.
#
# - pumpswap_create: pool create_event byte-tag (account disc)
#   We emit BOTH the broad pumpswap alert AND a narrow pumpswap-create alert.
# - marginfi liquidate: Anchor ix disc for `lendingAccountLiquidate`
# - kamino liquidate:  Anchor ix disc for `liquidate_obligation_and_redeem`
SECONDARY = {
    'pumpswap-create':       bytes([0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc]),
    # marginfi: sha256("global:lending_account_liquidate")[..8] = 21 4e 0f ... etc.
    # We accept any marginfi activity as alert — the hunter filters further.
}

# Stats
stats = {k: 0 for k in PROGRAMS}
for k in SECONDARY: stats[k] = 0
stats['entries'] = 0
stats['slots'] = set()
stats['start'] = time.time()

# Atomic write — guarantees Rust never reads a partial alert
def atomic_write(path, content):
    tmp = path + '.tmp'
    try:
        with open(tmp, 'w') as f:
            f.write(content)
        os.replace(tmp, path)
    except Exception as e:
        # silently swallow — tee must never die on disk errors
        pass

def write_alert(name, slot, ts):
    atomic_write(f'/tmp/shred-{name}-alert', f'{slot} {ts:.6f}\n')

def report():
    el = time.time() - stats['start']
    if el < 1: return
    keys = list(PROGRAMS) + list(SECONDARY)
    rates = ' '.join(f'{k}={stats[k]}' for k in keys)
    print(f'[+{el:>6.1f}s] entries={stats["entries"]} slots={len(stats["slots"])} {rates}', flush=True)

def main():
    print(f'starting unified_shred_tee → localhost:9000', flush=True)
    print(f'watching programs: {list(PROGRAMS.keys())}', flush=True)

    chan = grpc.insecure_channel('localhost:9000')
    stub = shredstream_pb2_grpc.ShredstreamProxyStub(chan)
    req = shredstream_pb2.SubscribeEntriesRequest()

    # Initial alert files so Rust hunters' mtime baseline starts now
    now = time.time()
    for name in list(PROGRAMS) + list(SECONDARY):
        write_alert(name, 0, now)

    last_report = time.time()
    last_slot = {k: 0 for k in list(PROGRAMS) + list(SECONDARY)}

    try:
        for entry in stub.SubscribeEntries(req):
            stats['entries'] += 1
            stats['slots'].add(entry.slot)
            data = entry.entries

            for name, pbytes in PROGRAM_BYTES.items():
                if pbytes not in data:
                    continue
                stats[name] += 1
                # debounce: skip if we already wrote this slot for this program
                if last_slot[name] != entry.slot:
                    last_slot[name] = entry.slot
                    write_alert(name, entry.slot, time.time())

            # Secondary: only fires when BOTH the related program AND the
            # discriminator are in the entry. Reduces alerts to actual events.
            if PROGRAM_BYTES['pumpswap'] in data and SECONDARY['pumpswap-create'] in data:
                stats['pumpswap-create'] += 1
                if last_slot['pumpswap-create'] != entry.slot:
                    last_slot['pumpswap-create'] = entry.slot
                    write_alert('pumpswap-create', entry.slot, time.time())

            now = time.time()
            if now - last_report > 10:
                report()
                last_report = now
    except grpc.RpcError as e:
        print(f'gRPC error: {e}', flush=True)
        sys.exit(1)
    except KeyboardInterrupt:
        report()

def sig_handler(sig, _frame):
    report()
    sys.exit(0)

if __name__ == '__main__':
    signal.signal(signal.SIGTERM, sig_handler)
    signal.signal(signal.SIGINT, sig_handler)
    main()
