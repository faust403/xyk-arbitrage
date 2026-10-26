#!/usr/bin/env python3
"""Swap a single specific token to USDC via Jupiter. Targeted, no broad scan.

Usage:
  python3 sell_one_token.py <MINT> [--slippage 500] [--paid]

Env: HELIUS_API_KEY, SOLANA_KEYPAIR_JSON, JUPITER_API_KEY (optional for paid tier)
"""
import os, sys, json, base64, argparse, time
import httpx, base58
from solders.keypair import Keypair
from solders.transaction import VersionedTransaction

WALLET = "7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ"
USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"

def load_wallet():
    v = os.environ["SOLANA_KEYPAIR_JSON"]
    if v.strip().startswith("["):
        return Keypair.from_bytes(bytes(json.loads(v)))
    return Keypair.from_base58_string(v)

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mint", help="Source token mint")
    ap.add_argument("--slippage", type=int, default=500, help="bps (default 500 = 5%)")
    ap.add_argument("--paid", action="store_true", help="Use paid api.jup.ag with JUPITER_API_KEY")
    args = ap.parse_args()

    base = "https://api.jup.ag/swap/v1" if args.paid else "https://lite-api.jup.ag/swap/v1"
    headers = {}
    if args.paid:
        k = os.environ.get("JUPITER_API_KEY")
        if not k:
            print("--paid set but JUPITER_API_KEY not in env", file=sys.stderr); sys.exit(1)
        headers["X-API-Key"] = k

    wallet = load_wallet()
    rpc = f"https://mainnet.helius-rpc.com/?api-key={os.environ['HELIUS_API_KEY']}"

    # Get balance for the mint
    r = httpx.post(rpc, json={
        "jsonrpc":"2.0","id":1,"method":"getTokenAccountsByOwner",
        "params":[WALLET, {"mint": args.mint}, {"encoding":"jsonParsed"}],
    }, timeout=20).json()
    vals = r["result"]["value"]
    if not vals:
        print(f"No ATA for mint {args.mint} in wallet {WALLET}"); sys.exit(1)
    info = vals[0]["account"]["data"]["parsed"]["info"]
    amt = info["tokenAmount"]
    raw = int(amt["amount"])
    ui = float(amt["uiAmountString"] or 0)
    dec = int(amt["decimals"])
    print(f"Balance: {ui} (raw={raw}, decimals={dec})")
    if raw == 0:
        print("Zero balance, nothing to sell."); sys.exit(0)

    # Jupiter quote
    quote_url = (f"{base}/quote?inputMint={args.mint}&outputMint={USDC}"
                 f"&amount={raw}&slippageBps={args.slippage}&swapMode=ExactIn"
                 f"&onlyDirectRoutes=false&restrictIntermediateTokens=true&maxAccounts=32")
    qr = httpx.get(quote_url, headers=headers, timeout=20)
    if qr.status_code != 200:
        print(f"quote failed HTTP {qr.status_code}: {qr.text[:400]}"); sys.exit(1)
    quote = qr.json()
    out_usdc = int(quote["outAmount"]) / 1e6
    print(f"Quote: → ${out_usdc:.4f} USDC (slippage {args.slippage}bps)")

    # Build swap tx
    swap_body = {
        "quoteResponse": quote,
        "userPublicKey": WALLET,
        "wrapAndUnwrapSol": True,
        "useSharedAccounts": True,
        "dynamicComputeUnitLimit": True,
        "prioritizationFeeLamports": 10000,
    }
    sr = httpx.post(f"{base}/swap", json=swap_body, headers=headers, timeout=20).json()
    if "error" in sr:
        print(f"swap-tx error: {sr['error']}"); sys.exit(1)
    raw_tx = base64.b64decode(sr["swapTransaction"])
    tx = VersionedTransaction.from_bytes(raw_tx)
    new_tx = VersionedTransaction(tx.message, [wallet])
    signed = bytes(new_tx)
    print(f"Signed tx size: {len(signed)} bytes")

    # Submit
    resp = httpx.post(rpc, json={
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[base58.b58encode(signed).decode(),
                  {"skipPreflight": False, "maxRetries": 5, "preflightCommitment":"processed"}],
    }, timeout=20).json()
    if "result" in resp:
        sig = resp["result"]
        print(f"🚀 SENT: https://solscan.io/tx/{sig}")
        # Wait + confirm
        time.sleep(8)
        cr = httpx.post(rpc, json={
            "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
            "params":[[sig], {"searchTransactionHistory": True}],
        }, timeout=15).json()
        st = cr.get("result", {}).get("value", [None])[0]
        print(f"Confirm: {st}")
    elif "error" in resp:
        print(f"✗ send error: {resp['error']}"); sys.exit(1)

if __name__ == "__main__":
    main()
