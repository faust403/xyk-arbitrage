#!/usr/bin/env python3
"""Sell all pump.fun token positions via Jupiter swap to USDC.

Usage:
  python3 sell_pump_tokens.py                         # DRY-RUN: quotes only
  python3 sell_pump_tokens.py --execute               # SEND swaps for positions worth > min_usdc_out
  python3 sell_pump_tokens.py --execute --min 1.0     # only positions netting >= $1.00 USDC
  python3 sell_pump_tokens.py --execute --slippage 500  # 5% slippage (default 1%)

Env:
  HELIUS_API_KEY        (required)
  SOLANA_KEYPAIR_JSON   (required)

Token-2022 source mints are routed through Jupiter, which handles the
program-bridge automatically. USDC destination ATA uses classic SPL Token.

Pump.fun specifics:
- Pre-graduation: Jupiter routes via pump.fun bonding curve directly
- Post-graduation: Jupiter routes via Raydium/Meteora/etc.
- Tokens that have NO liquidity (graduated and pool drained, or rugged):
  Jupiter returns no route → we skip and report
"""

import os, sys, json, base64, time, argparse
import httpx, base58
from solders.keypair import Keypair
from solders.transaction import VersionedTransaction

WALLET = "7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
TOKEN_2022_PROGRAM = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
JUP_BASE = "https://lite-api.jup.ag/swap/v1"

def load_wallet():
    v = os.environ["SOLANA_KEYPAIR_JSON"]
    if v.strip().startswith("["):
        return Keypair.from_bytes(bytes(json.loads(v)))
    return Keypair.from_base58_string(v)

def rpc_call(url, method, params):
    return httpx.post(url, json={
        "jsonrpc": "2.0", "id": 1, "method": method, "params": params
    }, timeout=30).json()

def list_pump_positions(rpc, owner, program):
    r = rpc_call(rpc, "getTokenAccountsByOwner",
                 [owner, {"programId": program}, {"encoding": "jsonParsed"}])
    out = []
    for v in r["result"]["value"]:
        info = v["account"]["data"]["parsed"]["info"]
        amt = info["tokenAmount"]
        ui = float(amt["uiAmountString"] or 0)
        if ui == 0:
            continue
        mint = info["mint"]
        # Pump.fun mints end with "pump" or "CASH" (one edge case found)
        if not mint.endswith("pump") and not mint.endswith("CASH"):
            continue
        out.append({
            "mint": mint,
            "raw": int(amt["amount"]),
            "ui":  ui,
            "decimals": int(amt["decimals"]),
            "ata": v["pubkey"],
        })
    return out

def jupiter_quote(input_mint, output_mint, amount, slippage_bps):
    url = (f"{JUP_BASE}/quote?inputMint={input_mint}&outputMint={output_mint}"
           f"&amount={amount}&slippageBps={slippage_bps}&swapMode=ExactIn"
           f"&onlyDirectRoutes=false&restrictIntermediateTokens=true&maxAccounts=32")
    try:
        r = httpx.get(url, timeout=20)
        if r.status_code != 200:
            return None, f"HTTP {r.status_code}: {r.text[:200]}"
        j = r.json()
        if "error" in j:
            return None, j["error"]
        return j, None
    except Exception as e:
        return None, str(e)

def jupiter_swap_tx(quote, user):
    body = {
        "quoteResponse": quote,
        "userPublicKey": user,
        "wrapAndUnwrapSol": True,
        "useSharedAccounts": True,
        "dynamicComputeUnitLimit": True,
        "prioritizationFeeLamports": 5000,
    }
    r = httpx.post(f"{JUP_BASE}/swap", json=body, timeout=20).json()
    if "error" in r:
        return None, r["error"]
    return r["swapTransaction"], None

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--execute", action="store_true", help="Actually send txs (default is DRY-RUN)")
    ap.add_argument("--min", type=float, default=0.5, help="Skip positions netting < this USDC (default 0.5)")
    ap.add_argument("--slippage", type=int, default=300, help="Slippage in bps (default 300 = 3%)")
    ap.add_argument("--limit", type=int, default=0, help="Cap N positions per run (0 = no cap)")
    args = ap.parse_args()

    wallet = load_wallet()
    pubkey = str(wallet.pubkey())
    if pubkey != WALLET:
        print(f"WARNING: keypair pubkey {pubkey} != expected {WALLET}", file=sys.stderr)
    rpc = f"https://mainnet.helius-rpc.com/?api-key={os.environ['HELIUS_API_KEY']}"

    print(f"Listing pump.fun positions for {WALLET} …")
    pos22 = list_pump_positions(rpc, WALLET, TOKEN_2022_PROGRAM)
    pos_classic = list_pump_positions(rpc, WALLET, TOKEN_PROGRAM)
    positions = pos22 + pos_classic
    print(f"Found {len(positions)} pump.fun positions ({len(pos22)} Token-2022, {len(pos_classic)} classic)")

    if args.limit > 0:
        positions = positions[:args.limit]
        print(f"Limiting to first {args.limit}")
    print()

    print(f"{'MINT':46} {'BALANCE':>14}  {'EST USDC OUT':>13}  ACTION")
    print("-" * 100)

    total_out = 0.0
    sellable = []
    skipped_no_route = 0
    skipped_dust = 0

    for i, p in enumerate(positions):
        q, err = jupiter_quote(p["mint"], USDC, p["raw"], args.slippage)
        if err:
            print(f"{p['mint']:46} {p['ui']:>14.2f}  {'-':>13}  NO ROUTE: {err[:50]}")
            skipped_no_route += 1
            continue
        out_amt = int(q["outAmount"]) / 1e6  # USDC has 6 decimals
        action = "EXEC" if out_amt >= args.min else "skip (dust)"
        if out_amt < args.min:
            skipped_dust += 1
        else:
            sellable.append((p, q, out_amt))
            total_out += out_amt
        print(f"{p['mint']:46} {p['ui']:>14.2f}  ${out_amt:>11.4f}  {action}")
        time.sleep(0.1)  # gentle Jupiter rate-limit

    print()
    print(f"Sellable positions (≥ ${args.min}): {len(sellable)}")
    print(f"Skipped (no route):                {skipped_no_route}")
    print(f"Skipped (dust):                    {skipped_dust}")
    print(f"Total estimated USDC recoverable:  ${total_out:.4f}")

    if not args.execute:
        print()
        print("DRY-RUN. To execute, rerun with --execute")
        return

    if not sellable:
        print("Nothing to execute.")
        return

    print()
    print("=== EXECUTING SWAPS ===")
    sent = 0
    succeeded = 0
    failed = 0
    for i, (p, quote, est_out) in enumerate(sellable):
        print(f"\n[{i+1}/{len(sellable)}] selling {p['ui']:.2f} {p['mint'][:8]}… → est ${est_out:.4f}")
        tx_b64, err = jupiter_swap_tx(quote, WALLET)
        if err:
            print(f"  ✗ jupiter swap-tx failed: {err}")
            failed += 1
            continue
        raw = base64.b64decode(tx_b64)
        tx = VersionedTransaction.from_bytes(raw)
        new_tx = VersionedTransaction(tx.message, [wallet])
        signed = bytes(new_tx)
        print(f"  signed: {len(signed)} bytes")
        sr = rpc_call(rpc, "sendTransaction",
                      [base58.b58encode(signed).decode(),
                       {"skipPreflight": False, "maxRetries": 3, "preflightCommitment": "processed"}])
        if "result" in sr:
            sig = sr["result"]
            print(f"  🚀 sent: https://solscan.io/tx/{sig}")
            sent += 1
        elif "error" in sr:
            print(f"  ✗ send error: {sr['error']}")
            failed += 1
        time.sleep(0.5)  # gentle rate-limit between sends

    # Wait a bit then check final balance
    print()
    print(f"Sent: {sent}, failed: {failed}. Waiting 15s then checking final USDC balance…")
    time.sleep(15)
    bal = rpc_call(rpc, "getTokenAccountsByOwner",
                   [WALLET, {"mint": USDC}, {"encoding": "jsonParsed"}])
    for v in bal["result"]["value"]:
        ui = v["account"]["data"]["parsed"]["info"]["tokenAmount"]["uiAmountString"]
        print(f"  USDC ATA: {v['pubkey']}: {ui}")

if __name__ == "__main__":
    main()
