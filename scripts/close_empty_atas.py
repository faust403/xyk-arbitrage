#!/usr/bin/env python3
"""Close empty Associated Token Accounts to recover rent (~0.002 SOL each).
Used to bootstrap the wallet when SOL is too low to even create a wSOL ATA
for swapping in more SOL.
"""
import os, json, base64, base58
from solders.keypair import Keypair
from solders.pubkey import Pubkey
from solders.instruction import Instruction, AccountMeta
from solders.message import MessageV0
from solders.transaction import VersionedTransaction
from solders.hash import Hash
import httpx

WALLET = '7MuyonxJKBKqtgwhhYmLU11WZTNFiDeaNn9JBHnk4DWJ'
TOKEN_PROGRAM = Pubkey.from_string('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA')

def load_wallet():
    v = os.environ['SOLANA_KEYPAIR_JSON']
    if v.strip().startswith('['):
        return Keypair.from_bytes(bytes(json.loads(v)))
    return Keypair.from_base58_string(v)

def rpc(m, p):
    return httpx.post(os.environ['HELIUS_RPC_URL'],
        json={'jsonrpc':'2.0','id':1,'method':m,'params':p}, timeout=15).json()

def main():
    wallet = load_wallet()
    print(f"wallet: {wallet.pubkey()}")
    r = rpc('getTokenAccountsByOwner', [WALLET, {'programId': str(TOKEN_PROGRAM)}, {'encoding': 'jsonParsed'}])
    accts = r['result']['value']
    empties = []
    for a in accts:
        info = a['account']['data']['parsed']['info']
        amt = float(info['tokenAmount']['uiAmountString'] or 0)
        if amt == 0:
            empties.append((a['pubkey'], info['mint']))
    print(f"found {len(empties)} empty ATAs")
    if not empties:
        print("nothing to close")
        return

    # SPL Token CloseAccount: discriminator 9
    ixs = []
    for pk, mint in empties[:5]:  # cap at 5 to stay under tx size
        print(f"  closing {pk[:8]}... (mint={mint[:8]})")
        meta = [
            AccountMeta(Pubkey.from_string(pk), False, True),     # account to close
            AccountMeta(wallet.pubkey(), False, True),            # rent destination
            AccountMeta(wallet.pubkey(), True, False),            # owner authority (signer)
        ]
        ixs.append(Instruction(TOKEN_PROGRAM, bytes([9]), meta))

    bh = rpc('getLatestBlockhash', [{'commitment':'finalized'}])['result']['value']
    blockhash = Hash.from_string(bh['blockhash'])
    msg = MessageV0.try_compile(wallet.pubkey(), ixs, [], blockhash)
    tx = VersionedTransaction(msg, [wallet])
    raw = bytes(tx)
    print(f"tx size: {len(raw)} bytes")
    resp = rpc('sendTransaction', [base58.b58encode(raw).decode(), {'skipPreflight': False, 'preflightCommitment':'processed'}])
    print(f"submit: {resp}")
    if 'result' in resp:
        print(f"https://solscan.io/tx/{resp['result']}")

if __name__ == '__main__':
    main()
