#!/usr/bin/env python3
"""Sandwich attack detector — extends shred-subscriber pattern for DEX swaps.

Subscribes to local shredstream-proxy gRPC on :9000 (already running),
binary-scans entry payloads for swap-program IDs (Raydium AMM v4 / CLMM,
Orca Whirlpool, Meteora DLMM, Meteora DAMM, Pump.fun bonding curve).

When a swap-mentioning entry is detected, it tries to parse the tx to
extract: pool pubkey, swap amount, in/out mints. Sandwich-eligible
candidates (large enough swap, eligible pool) are written to
/tmp/shred-sandwich-alert for the Rust executor to consume.

Phase 1 (THIS FILE, DRY-RUN):
  - Counts swap detections per program per minute
  - Emits a TOP-N rolling summary of slot+pool+approx-size every 30s
  - Validates that detectable opportunities exist at our scale

Phase 2 (next): Rust executor reads the alert file, builds front-run +
back-run tx pair, bundles via Jito with high tip.

Why: tx-level mempool visibility = the only way to actually MOVE the
market (vs. waiting for spreads to widen). User-confirmed strategy.
"""
import sys, time, os, json
sys.path.insert(0, '/home/ubuntu/shred-client')
import grpc
import shredstream_pb2
import shredstream_pb2_grpc

# ─── Base58 helpers (no external deps) ──────────────────────────
_B58_ALPHABET = '123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz'
def b58decode(s):
    n = 0
    for c in s: n = n * 58 + _B58_ALPHABET.index(c)
    b = n.to_bytes((n.bit_length()+7)//8, 'big')
    pad = len(s) - len(s.lstrip('1'))
    return b'\x00' * pad + b

def b58encode(b):
    n = int.from_bytes(b, 'big')
    out = ''
    while n > 0:
        n, r = divmod(n, 58)
        out = _B58_ALPHABET[r] + out
    return ('1' * (len(b) - len(b.lstrip(b'\x00')))) + out

# ─── DEX program IDs we care about ──────────────────────────────
DEX_PROGRAMS = {
    'RaydiumAMMv4': '675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8',
    'RaydiumCLMM':  'CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK',
    'Whirlpool':    'whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc',
    'MeteoraDLMM':  'LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo',
    'MeteoraDAMM':  'Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB',
    'PumpFunBC':    '6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P',
    'Phoenix':      'PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY',
}
DEX_BYTES = {name: b58decode(pk) for name, pk in DEX_PROGRAMS.items()}

# ─── Known mints for USD conversion (rough) ─────────────────────
USD_MINTS = {
    'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v': ('USDC', 6, 1.0),
    'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB': ('USDT', 6, 1.0),
    'So11111111111111111111111111111111111111112':  ('SOL',  9, 90.0),   # approx
    '3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh': ('WBTC', 8, 110000.0),
    'cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij':  ('cbBTC', 8, 110000.0),
    'J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn': ('JitoSOL', 9, 110.0),
    'mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So':  ('mSOL', 9, 105.0),
}

# Minimum USD size for a swap to be "sandwich-worthy". Smaller swaps
# move price too little to make front/back run profitable after fees.
MIN_SWAP_USD = float(os.environ.get('SANDWICH_MIN_SWAP_USD', '500'))

# ─── Stats ──────────────────────────────────────────────────────
stats = {
    'entries': 0,
    'slots_seen': set(),
    'bytes': 0,
    'start': time.time(),
    'hits_by_program': {name: 0 for name in DEX_PROGRAMS},
    'sandwich_alerts': 0,
}
recent_alerts = []  # rolling list of recent (slot, program, size_usd)

def report():
    el = time.time() - stats['start']
    if el < 1: return
    hits_str = ' '.join(f"{n}={c}" for n, c in stats['hits_by_program'].items() if c > 0)
    rate_per_sec = stats['entries'] / el if el > 0 else 0
    print(f"[+{el:>7.1f}s] entries={stats['entries']} ({rate_per_sec:.0f}/s) "
          f"slots={len(stats['slots_seen'])} bytes={stats['bytes']:,} "
          f"alerts={stats['sandwich_alerts']} hits: {hits_str}", flush=True)
    if recent_alerts:
        print(f"  recent (top 5):", flush=True)
        for a in recent_alerts[-5:]:
            print(f"    slot={a['slot']} {a['program']} approx=${a['usd']:.0f}", flush=True)

def scan_entry(data: bytes, slot: int):
    """Binary-scan an entry payload for DEX program-ID bytes. Returns
    a list of (program_name, position) hits. We don't parse the full
    tx structure — for phase-1 just count hits."""
    hits = []
    for name, prog_bytes in DEX_BYTES.items():
        pos = 0
        while True:
            idx = data.find(prog_bytes, pos)
            if idx < 0: break
            hits.append((name, idx))
            pos = idx + 32
    return hits

# ─── Raydium AMM v4 swap-IX parser ─────────────────────────────
# IX layout (well-known):
#   byte 0   instruction discriminator (9 = swap_base_in, 11 = swap_base_out)
#   bytes 1-8   u64 amount_in    (or amount_out_max for swap_base_out)
#   bytes 9-16  u64 min_amount_out (or amount_in_max)
# Total: 17 bytes
RAYDIUM_SWAP_DISCRIMINATORS = {9, 11}

def try_parse_raydium_swap(window: bytes, dex_program_pos: int) -> int:
    """Search for a Raydium AMM v4 swap-IX data pattern in the window.
    Returns the swap amount (raw, in token base units) or 0 if not found.

    A swap IX is 17 bytes:  [disc 9 or 11] + [u64 amount] + [u64 min]
    """
    # Scan every byte for the pattern. Real tx layout has the IX data
    # immediately after some compact-u16 length bytes — we don't know
    # exactly where without decoding the tx structure, so brute-force
    # scan with sanity checks on the resulting amounts.
    best = 0
    for off in range(max(0, dex_program_pos - 512), min(len(window) - 17, dex_program_pos + 512)):
        if window[off] not in RAYDIUM_SWAP_DISCRIMINATORS:
            continue
        amount = int.from_bytes(window[off+1:off+9], 'little')
        min_out = int.from_bytes(window[off+9:off+17], 'little')
        # Sanity: amount must be plausible (between $0.01 worth of a 6-dec
        # token = 10000 and $10M worth of a 9-dec SOL = 10**16).
        if 1_000 < amount < 10**16 and 0 < min_out < amount * 2:
            if amount > best: best = amount
    return best

# ─── Whirlpool swap-IX parser (Anchor) ─────────────────────────
# Anchor discriminator for `swap`:
#   sha256("global:swap")[..8]  = bytes [0xf8 0xc6 0x9e 0x91 0xe1 0x75 0x87 0xc8]
# IX data layout:
#   [8-byte disc][u64 amount][u64 other_amount_threshold][u128 sqrt_price_limit][bool exact_in][bool a_to_b]
WHIRLPOOL_SWAP_DISC = bytes([0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8])

def try_parse_whirlpool_swap(window: bytes) -> int:
    idx = window.find(WHIRLPOOL_SWAP_DISC)
    if idx < 0: return 0
    if idx + 8 + 8 > len(window): return 0
    amount = int.from_bytes(window[idx+8:idx+16], 'little')
    if 1_000 < amount < 10**16: return amount
    return 0

def maybe_alert(data: bytes, slot: int, hits: list):
    """For each unique DEX program hit, attempt to decode the swap-IX
    to extract the REAL swap amount, then check it against USD-mint
    references in the same window to assign a USD value."""
    if not hits: return
    seen_programs = set()
    for name, idx in hits:
        if name in seen_programs: continue
        seen_programs.add(name)
        window = data[max(0, idx-512):idx+512]

        # Try to extract a real amount via per-DEX IX decoder.
        amount_raw = 0
        if name == 'RaydiumAMMv4':
            amount_raw = try_parse_raydium_swap(window, 512)  # idx in window
        elif name in ('Whirlpool',):
            amount_raw = try_parse_whirlpool_swap(window)
        # CLMM, MeteoraDLMM, Pump.fun all use Anchor discs we haven't
        # mapped yet — fall back to heuristic for those for now.

        # Find USD-mint references in the window to convert raw → USD.
        usd_value = 0
        for mint_b58, (sym, dec, px) in USD_MINTS.items():
            if b58decode(mint_b58) in window:
                if amount_raw > 0:
                    amt = amount_raw / (10 ** dec)
                    usd_value = max(usd_value, amt * px)
                else:
                    # Fallback heuristic for un-decoded DEXes. TIGHTENED:
                    # cap at $10M (= 10^13 for a 6-dec stable token raw amount).
                    # Without proper IX parsing we can't know the real size,
                    # so we report nothing — better to skip than emit
                    # fake $billion alerts that drown the executor.
                    largest = 0
                    for off in range(0, len(window) - 8, 8):
                        v = int.from_bytes(window[off:off+8], 'little')
                        # cap at 10^13 (=$10M of a 6-dec stable, or $10B
                        # of a SOL-like 9-dec — still nonsense for 9-dec,
                        # so we only trust this for stable mints)
                        if sym in ('USDC', 'USDT') and 10_000 < v < 10**13:
                            if v > largest: largest = v
                    if largest > 0:
                        amt = largest / (10 ** dec)
                        usd_value = max(usd_value, amt * px)

        if usd_value >= MIN_SWAP_USD:
            alert = {
                'slot': slot, 'program': name, 'usd': usd_value,
                'amount_raw': amount_raw, 'parsed': amount_raw > 0,
                'ts': time.time(),
            }
            recent_alerts.append(alert)
            if len(recent_alerts) > 100:
                recent_alerts.pop(0)
            stats['sandwich_alerts'] += 1
            with open('/tmp/shred-sandwich-alert', 'a') as f:
                f.write(json.dumps(alert) + '\n')

def main():
    chan = grpc.insecure_channel('localhost:9000')
    stub = shredstream_pb2_grpc.ShredstreamProxyStub(chan)
    req = shredstream_pb2.SubscribeEntriesRequest()
    print(f"sandwich-detector connecting to localhost:9000 (min ${MIN_SWAP_USD}) …", flush=True)
    last_report = time.time()
    try:
        for entry in stub.SubscribeEntries(req):
            stats['entries'] += 1
            stats['slots_seen'].add(entry.slot)
            stats['bytes'] += len(entry.entries)
            hits = scan_entry(entry.entries, entry.slot)
            for name, _ in hits:
                stats['hits_by_program'][name] += 1
            if hits:
                maybe_alert(entry.entries, entry.slot, hits)
            now = time.time()
            if now - last_report > 30:
                report()
                last_report = now
    except grpc.RpcError as e:
        print(f"gRPC error: {e}", flush=True)
    except KeyboardInterrupt:
        report()

if __name__ == '__main__':
    main()
