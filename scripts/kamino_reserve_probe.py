#!/usr/bin/env python3
"""Probe a known Kamino Reserve account for the actual oracle offset.

We hypothesized TokenInfo starts at on-chain offset 7856 with scope@+80,
switchboard@+128, pyth@+192. Live sim says active_oracle reads as
Pubkey::default() → either the offsets are off or these reserves use
some packing we don't expect.

This probe:
1. Fetches a reserve we know is on Kamino main market (USDC reserve)
2. Dumps every Pubkey-shaped 32-byte window from offset 7000 to 8400
3. Filters to "looks like a real pubkey" (not all-zeros, not all-1s,
   not classic system program)
4. Cross-checks against Kamino's Scope program owner via getAccountInfo
"""
import os, sys, json, base64
import httpx, base58

# Kamino Main Market USDC reserve (confirmed from monitor output)
USDC_RESERVE = "D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59"
# Top other reserves seen in obligations
SOL_RESERVE  = "d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q"
USDT_RESERVE = "H3t6qZ1JkguCNTi9uzVKqQ7dvt2cum4XiXWom6Gn5e5S"

KEY = os.environ["HELIUS_API_KEY"]
RPC = f"https://mainnet.helius-rpc.com/?api-key={KEY}"

# Known Kamino oracle programs (for owner-of-pubkey lookups)
KAMINO_SCOPE_PROGRAM = "HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ"
PYTH_PUSH_PROGRAM    = "pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT"
PYTH_LEGACY_FsJ3     = "FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH"
PYTH_LEGACY_rec5     = "rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ"
SWITCHBOARD          = "SW1TCH7qEPTdLsDHRgPuMQjbQxKdH2aBStViMFnt64f"

def rpc(method, params):
    return httpx.post(RPC, json={"jsonrpc":"2.0","id":1,"method":method,"params":params}, timeout=20).json()

def probe(reserve_pk, label):
    r = rpc("getAccountInfo", [reserve_pk, {"encoding":"base64","commitment":"confirmed"}])
    data_b64 = r["result"]["value"]["data"][0]
    data = base64.b64decode(data_b64)
    print(f"\n=== {label}: {reserve_pk} ===")
    print(f"size: {len(data)} bytes (expected 8624)")

    # 1. Known fields from current parser (sanity check)
    def pk(off):
        return base58.b58encode(data[off:off+32]).decode()
    print(f"\nKnown-good offsets per current parser:")
    print(f"  +8  (disc end) lending_market field-start: not direct readable")
    print(f"  +32 lending_market: {pk(32)}")
    print(f"  +128 liquidity.mint_pubkey: {pk(128)}")
    print(f"  +160 liquidity.supply_vault: {pk(160)}")

    # 2. Scan EVERY 8-aligned offset (broader) for plausible Pubkeys
    print(f"\nScanning all 8-aligned offsets for plausible Pubkey-shaped data:")
    plausibles = []
    for off in range(0, len(data) - 32, 8):
        chunk = data[off:off+32]
        if chunk == b"\x00"*32: continue
        if chunk == b"\xff"*32: continue
        non_zero = sum(1 for b in chunk if b != 0)
        if non_zero < 25: continue  # tighter threshold
        plausibles.append((off, base58.b58encode(chunk).decode()))

    print(f"Found {len(plausibles)} plausible pubkey-shaped windows total:")
    for off, p58 in plausibles[:80]:
        print(f"  off={off:>5}: {p58}")

    # 3. For each plausible pubkey, query its owner to identify Scope/Pyth/etc.
    print(f"\nIdentifying account owners (top 30 plausibles):")
    for off, p58 in plausibles[:30]:
        try:
            ar = rpc("getAccountInfo", [p58, {"encoding":"base64","commitment":"confirmed"}])
            val = ar.get("result", {}).get("value")
            if val is None:
                print(f"  off={off:>5}: {p58[:8]}... (does not exist)")
                continue
            owner = val.get("owner", "?")
            label = ""
            if owner == KAMINO_SCOPE_PROGRAM:  label = " ← Kamino SCOPE"
            elif owner == PYTH_PUSH_PROGRAM:   label = " ← Pyth Push"
            elif owner == PYTH_LEGACY_FsJ3 or owner == PYTH_LEGACY_rec5: label = " ← Pyth Legacy"
            elif owner == SWITCHBOARD:         label = " ← Switchboard"
            print(f"  off={off:>5}: {p58[:8]}... owner={owner[:8]}...{label}")
        except Exception as e:
            print(f"  off={off:>5}: {p58[:8]}... err={e}")

if __name__ == "__main__":
    probe(USDC_RESERVE, "USDC main-market")
