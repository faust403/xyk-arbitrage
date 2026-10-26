#!/usr/bin/env python3
"""Burn the balance + close ATAs for dead pump.fun tokens.

Recovers ~2,039,280 lamports rent per closed ATA. For 90 dead ATAs that's
~0.18 SOL ≈ $16 at current SOL price.

Process per ATA:
  1. Token Burn IX (disc 8, u64 amount) — zeroes out balance
  2. Token CloseAccount IX (disc 9) — closes ATA, lamports back to wallet

Batches multiple tokens per tx (default 5) to amortize tx fees.

Usage:
  python3 burn_and_close_dead_atas.py                # DRY-RUN
  python3 burn_and_close_dead_atas.py --execute      # actually do it
  python3 burn_and_close_dead_atas.py --execute --batch 4 --skip CASHx9...,EPjFW...
"""
import os, sys, json, base58, argparse, time
import httpx
from solders.keypair import Keypair
from solders.pubkey import Pubkey
from solders.instruction import Instruction, AccountMeta
from solders.message import MessageV0
from solders.transaction import VersionedTransaction
from solders.hash import Hash

WALLET = "7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ"
TOKEN_2022_PROGRAM = Pubkey.from_string("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb")
TOKEN_PROGRAM = Pubkey.from_string("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA")

STABLES = {
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": "USDC",
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": "USDT",
    "So11111111111111111111111111111111111111112":  "wSOL",
    "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh": "WBTC",
    "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs": "ETH",
    "cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij":  "cbBTC",
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn": "JitoSOL",
    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So":  "mSOL",
}

def load_wallet():
    v = os.environ["SOLANA_KEYPAIR_JSON"]
    if v.strip().startswith("["):
        return Keypair.from_bytes(bytes(json.loads(v)))
    return Keypair.from_base58_string(v)

def rpc(url, method, params):
    return httpx.post(url, json={"jsonrpc":"2.0","id":1,"method":method,"params":params}, timeout=20).json()

def get_positions(rpc_url, program):
    r = rpc(rpc_url, "getTokenAccountsByOwner",
            [WALLET, {"programId": str(program)}, {"encoding":"jsonParsed"}])
    out = []
    for v in r["result"]["value"]:
        info = v["account"]["data"]["parsed"]["info"]
        amt = info["tokenAmount"]
        raw = int(amt["amount"])
        mint = info["mint"]
        out.append({
            "mint": mint, "raw": raw, "ui": float(amt["uiAmountString"] or 0),
            "decimals": int(amt["decimals"]), "ata": v["pubkey"], "program": program,
        })
    return out

def build_burn_ix(ata, mint, authority, amount, token_program):
    """SPL Token Burn — disc 8, u64 amount."""
    data = bytes([8]) + int(amount).to_bytes(8, "little")
    return Instruction(token_program, data, [
        AccountMeta(Pubkey.from_string(ata), False, True),
        AccountMeta(Pubkey.from_string(mint), False, True),
        AccountMeta(authority, True, False),
    ])

def build_close_ix(ata, destination, authority, token_program):
    """SPL Token CloseAccount — disc 9."""
    return Instruction(token_program, bytes([9]), [
        AccountMeta(Pubkey.from_string(ata), False, True),
        AccountMeta(destination, False, True),
        AccountMeta(authority, True, False),
    ])

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--execute", action="store_true")
    ap.add_argument("--batch", type=int, default=5)
    ap.add_argument("--skip", default="", help="comma-sep mint addrs to skip")
    args = ap.parse_args()

    wallet = load_wallet()
    rpc_url = f"https://mainnet.helius-rpc.com/?api-key={os.environ['HELIUS_API_KEY']}"
    skip = set([s.strip() for s in args.skip.split(",") if s.strip()])
    skip.update(STABLES.keys())

    print("Loading all token positions …")
    pos = []
    pos += get_positions(rpc_url, TOKEN_2022_PROGRAM)
    pos += get_positions(rpc_url, TOKEN_PROGRAM)
    print(f"Found {len(pos)} ATAs total")

    # Eligible = non-stable, non-skipped, has lamports backing (always true for ATAs)
    eligible = [p for p in pos if p["mint"] not in skip]
    print(f"Eligible for burn+close: {len(eligible)}")
    print()
    print(f"{'MINT':46} {'BALANCE':>16}  PROG")
    print("-" * 80)
    for p in eligible[:5]:
        prog = "T22" if p["program"] == TOKEN_2022_PROGRAM else "TKN"
        print(f"{p['mint']:46} {p['ui']:>16.6f}  {prog}")
    if len(eligible) > 5:
        print(f"... and {len(eligible)-5} more")
    print()

    if not args.execute:
        # estimate
        rent_per = 2_039_280  # standard ATA rent
        total_lamports = rent_per * len(eligible)
        n_txs = (len(eligible) + args.batch - 1) // args.batch
        cost_lamports = n_txs * 15_000  # base + priority
        net = total_lamports - cost_lamports
        print(f"DRY-RUN summary:")
        print(f"  ATAs to close:     {len(eligible)}")
        print(f"  Txs to send:       {n_txs}  (batch={args.batch})")
        print(f"  Rent recovered:    {total_lamports:>15,} lamports = {total_lamports/1e9:.4f} SOL")
        print(f"  Tx fees:           {cost_lamports:>15,} lamports = {cost_lamports/1e9:.4f} SOL")
        print(f"  Net recovery:      {net:>15,} lamports = {net/1e9:.4f} SOL")
        print()
        print("Rerun with --execute to actually burn+close.")
        return

    # Execute
    print("=== EXECUTING burn+close ===")
    authority = wallet.pubkey()
    success = 0
    failure = 0
    for batch_idx in range(0, len(eligible), args.batch):
        batch = eligible[batch_idx:batch_idx + args.batch]
        ixs = []
        for p in batch:
            tp = p["program"]
            if p["raw"] > 0:
                ixs.append(build_burn_ix(p["ata"], p["mint"], authority, p["raw"], tp))
            ixs.append(build_close_ix(p["ata"], authority, authority, tp))

        # Get blockhash
        bh = rpc(rpc_url, "getLatestBlockhash", [{"commitment":"finalized"}])["result"]["value"]
        blockhash = Hash.from_string(bh["blockhash"])
        msg = MessageV0.try_compile(authority, ixs, [], blockhash)
        tx = VersionedTransaction(msg, [wallet])
        raw_tx = bytes(tx)
        print(f"\n[batch {batch_idx//args.batch + 1}] {len(batch)} tokens, {len(ixs)} ixs, {len(raw_tx)} bytes")
        for p in batch:
            print(f"  {p['mint'][:8]}…  bal={p['ui']:.2f}")

        resp = rpc(rpc_url, "sendTransaction",
                   [base58.b58encode(raw_tx).decode(),
                    {"skipPreflight": False, "maxRetries": 3, "preflightCommitment":"processed"}])
        if "result" in resp:
            sig = resp["result"]
            print(f"  🚀 sig: {sig}")
            success += len(batch)
        elif "error" in resp:
            print(f"  ✗ send error: {resp['error']}")
            failure += len(batch)
        time.sleep(0.5)

    print()
    print(f"Done. Success: {success}, failure: {failure}")
    print("Wait ~30s then check SOL balance to confirm rent recovery.")

if __name__ == "__main__":
    main()
