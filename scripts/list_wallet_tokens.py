#!/usr/bin/env python3
"""List all SPL token accounts in the wallet, flag pump.fun candidates."""
import sys, json, os, httpx

KNOWN = {
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": "USDC",
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": "USDT",
    "So11111111111111111111111111111111111111112":  "wSOL",
    "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh": "WBTC",
    "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs": "ETH",
    "cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij":  "cbBTC",
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn": "JitoSOL",
    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So":  "mSOL",
}

WALLET = "7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ"
key = os.environ["HELIUS_API_KEY"]
rpc = f"https://mainnet.helius-rpc.com/?api-key={key}"

# Token program — pump.fun tokens use classic Token program
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"

r = httpx.post(rpc, json={
    "jsonrpc": "2.0", "id": 1, "method": "getTokenAccountsByOwner",
    "params": [WALLET, {"programId": TOKEN_PROGRAM}, {"encoding": "jsonParsed"}],
}, timeout=30).json()

positions = []
for v in r["result"]["value"]:
    info = v["account"]["data"]["parsed"]["info"]
    amt = info["tokenAmount"]
    ui = float(amt["uiAmountString"] or 0)
    if ui == 0:
        continue
    mint = info["mint"]
    positions.append({
        "mint": mint,
        "ui": ui,
        "raw": amt["amount"],
        "decimals": int(amt["decimals"]),
        "ata": v["pubkey"],
        "label": KNOWN.get(mint, ""),
    })

print(f"{'MINT':50} {'BALANCE':>20} {'DEC':>4}  LABEL")
print("-" * 100)
for p in positions:
    print(f"{p['mint']:50} {p['ui']:>20.6f} {p['decimals']:>4}  {p['label']}")
print()
print(f"{len(positions)} non-zero token accounts total")
print()

unlabeled = [p for p in positions if not p["label"]]
print(f"Pump.fun candidates (unlabeled mints, may need to sell):  {len(unlabeled)}")
for p in unlabeled:
    print(f"  {p['mint']}  bal={p['ui']}  raw={p['raw']}  dec={p['decimals']}  ata={p['ata']}")
