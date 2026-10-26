#!/usr/bin/env python3
"""Swap a fixed amount of USDC to SOL via Jupiter v6 API.
Used to refill wallet SOL after a fee drain.
"""
import os, json, base64, base58
from solders.keypair import Keypair
from solders.transaction import VersionedTransaction
from solders.hash import Hash
from solders.message import to_bytes_versioned
import httpx

WALLET = '7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ'
USDC = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v'
SOL  = 'So11111111111111111111111111111111111111112'

AMOUNT_USDC = int(os.environ.get('SWAP_USDC', '10')) * 1_000_000  # default $10
SLIPPAGE_BPS = 100  # 1%

def load_wallet():
    v = os.environ['SOLANA_KEYPAIR_JSON']
    if v.strip().startswith('['):
        return Keypair.from_bytes(bytes(json.loads(v)))
    return Keypair.from_base58_string(v)

def main():
    wallet = load_wallet()
    print(f"wallet: {wallet.pubkey()}")

    # 1. Jupiter quote
    q_url = f"https://lite-api.jup.ag/swap/v1/quote?inputMint={USDC}&outputMint={SOL}&amount={AMOUNT_USDC}&slippageBps={SLIPPAGE_BPS}&swapMode=ExactIn&onlyDirectRoutes=false&maxAccounts=64"
    q = httpx.get(q_url, timeout=15).json()
    out_amount = int(q['outAmount'])
    print(f"quote: {AMOUNT_USDC/1e6} USDC → {out_amount/1e9} SOL (slippage {SLIPPAGE_BPS}bps)")

    # 2. Jupiter swap
    swap_body = {
        'quoteResponse': q,
        'userPublicKey': WALLET,
        'wrapAndUnwrapSol': True,
        'useSharedAccounts': True,
        'dynamicComputeUnitLimit': True,
        'prioritizationFeeLamports': 5000,
    }
    swap_resp = httpx.post('https://lite-api.jup.ag/swap/v1/swap', json=swap_body, timeout=15).json()
    tx_b64 = swap_resp['swapTransaction']
    raw = base64.b64decode(tx_b64)
    # 3. Sign — Jupiter returns a partially-signed v0 tx; we add our sig
    tx = VersionedTransaction.from_bytes(raw)
    # Re-sign: produce new tx with our signature
    new_tx = VersionedTransaction(tx.message, [wallet])
    raw_signed = bytes(new_tx)
    print(f"signed tx size: {len(raw_signed)} bytes")

    # 4. Submit via RPC
    helius_url = os.environ.get('HELIUS_RPC_URL', f"https://mainnet.helius-rpc.com/?api-key={os.environ.get('HELIUS_API_KEY','')}")
    r = httpx.post(helius_url, json={
        'jsonrpc':'2.0','id':1,'method':'sendTransaction',
        'params':[base58.b58encode(raw_signed).decode(), {'skipPreflight': False, 'maxRetries': 5, 'preflightCommitment':'processed'}],
    }, timeout=15).json()
    print(f"submit: {r}")
    if 'result' in r:
        print(f"https://solscan.io/tx/{r['result']}")

if __name__ == '__main__':
    main()
