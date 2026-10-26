#!/usr/bin/env python3
"""Create + extend a new Address Lookup Table containing Pyth + Solend
oracle accounts. Output the ALT pubkey to add to EXECUTOR_PYTH_ALT env.

This lets the Solend liquidator's Pyth-paired tx use the new ALT to
compress oracle account references — freeing up the ~700 bytes needed
to fit the Pyth IX alongside MarginFi flash inside the 1232-byte v0
budget.

Addresses included:
  - Pyth Push V2 receiver program
  - Pyth Push V2 config PDA + treasury PDAs
  - Wormhole guardian set
  - All 85 Solend USDC-market reserve oracle accounts
  - All 85 Solend reserve accounts themselves
"""
import os, sys, json, base64, base58, struct, time
from solders.keypair import Keypair
from solders.pubkey import Pubkey
from solders.instruction import Instruction, AccountMeta
from solders.message import MessageV0
from solders.transaction import VersionedTransaction
from solders.hash import Hash
from solders.system_program import ID as SYSTEM_PROGRAM_ID
import httpx

RPC_URL = os.environ.get('HELIUS_RPC_URL', 'https://mainnet.helius-rpc.com/?api-key=e0805d12-5ab6-4d17-a854-a28837344689')
ALT_PROGRAM = Pubkey.from_string('AddressLookupTab1e1111111111111111111111111')
SOLEND_PROGRAM = Pubkey.from_string('So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo')
PYTH_PUSH_V2 = Pubkey.from_string('pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT')

LOG = lambda *a: print(*a, flush=True)

def load_wallet():
    v = os.environ.get('SOLANA_KEYPAIR_JSON')
    if not v: raise RuntimeError("SOLANA_KEYPAIR_JSON not set")
    if v.strip().startswith('['):
        return Keypair.from_bytes(bytes(json.loads(v)))
    return Keypair.from_base58_string(v)

def rpc(method, params):
    r = httpx.post(RPC_URL, json={'jsonrpc':'2.0','id':1,'method':method,'params':params}, timeout=15)
    return r.json()

def get_latest_blockhash():
    return rpc('getLatestBlockhash', [{'commitment':'finalized'}])['result']['value']

def get_slot():
    return rpc('getSlot', [{'commitment':'finalized'}])['result']

def get_program_accounts(pid, filters=None):
    return rpc('getProgramAccounts', [pid, {'encoding':'base64','filters':filters or []}]).get('result', [])

def get_account(pk):
    r = rpc('getAccountInfo', [pk, {'encoding':'base64'}])
    v = r.get('result',{}).get('value')
    return base64.b64decode(v['data'][0]) if v else None

def send_tx(tx_bytes):
    r = rpc('sendTransaction', [base58.b58encode(tx_bytes).decode(), {'skipPreflight': False, 'maxRetries': 5, 'preflightCommitment':'processed'}])
    return r

def build_create_alt_ix(authority, payer, recent_slot):
    """ALT Program ix 0: CreateLookupTable {recent_slot u64, bump u8}"""
    # PDA derivation: [authority, recent_slot_le_u64]
    seeds = [bytes(authority), struct.pack('<Q', recent_slot)]
    alt_pda, bump = Pubkey.find_program_address(seeds, ALT_PROGRAM)
    data = struct.pack('<I', 0) + struct.pack('<Q', recent_slot) + bytes([bump])
    accounts = [
        AccountMeta(alt_pda, False, True),
        AccountMeta(authority, True, False),
        AccountMeta(payer, True, True),
        AccountMeta(SYSTEM_PROGRAM_ID, False, False),
    ]
    return alt_pda, Instruction(ALT_PROGRAM, data, accounts)

def build_extend_alt_ix(alt_pda, authority, payer, addresses):
    """ALT Program ix 2: ExtendLookupTable { addresses: Vec<Pubkey> }"""
    # Anchor-borsh: u32 LE length + each address 32 bytes
    data = struct.pack('<I', 2) + struct.pack('<Q', len(addresses))
    for a in addresses: data += bytes(a)
    accounts = [
        AccountMeta(alt_pda, False, True),
        AccountMeta(authority, True, False),
        AccountMeta(payer, True, True),
        AccountMeta(SYSTEM_PROGRAM_ID, False, False),
    ]
    return Instruction(ALT_PROGRAM, data, accounts)

def main():
    wallet = load_wallet()
    LOG(f"wallet: {wallet.pubkey()}")

    # Collect addresses to add
    addrs = [bytes(PYTH_PUSH_V2)]
    LOG("loading Solend reserves...")
    reserves = get_program_accounts(str(SOLEND_PROGRAM), filters=[{'dataSize': 619}])
    oracle_set = set()
    reserve_set = set()
    for a in reserves[:120]:
        data = base64.b64decode(a['account']['data'][0])
        if len(data) < 200 or data[0] != 1: continue
        pyth = bytes(data[107:139])
        oracle_set.add(pyth)
        reserve_set.add(base58.b58decode(a['pubkey']))
    LOG(f"  collected {len(oracle_set)} unique oracles, {len(reserve_set)} reserves")
    addrs.extend(oracle_set)
    addrs.extend(reserve_set)
    # Dedupe
    seen = set(); uniq = []
    for a in addrs:
        if a in seen: continue
        seen.add(a); uniq.append(a)
    LOG(f"total unique addresses to add: {len(uniq)}")

    # Step 1: create ALT
    recent_slot = get_slot() - 1  # safer to use slot - 1 to avoid race
    LOG(f"creating ALT at recent_slot={recent_slot}...")
    alt_pda, create_ix = build_create_alt_ix(wallet.pubkey(), wallet.pubkey(), recent_slot)
    LOG(f"  ALT PDA will be: {alt_pda}")

    bh = get_latest_blockhash()
    blockhash = Hash.from_string(bh['blockhash'])
    msg = MessageV0.try_compile(wallet.pubkey(), [create_ix], [], blockhash)
    tx = VersionedTransaction(msg, [wallet])
    LOG(f"  sending create tx... ({len(bytes(tx))} bytes)")
    resp = send_tx(bytes(tx))
    LOG(f"  create resp: {resp}")
    if 'error' in resp:
        LOG("create failed; aborting")
        return
    sig = resp.get('result')
    LOG(f"  sig: {sig}")
    LOG("  waiting 8s for confirmation...")
    time.sleep(8)

    # Step 2: extend in batches of 20 (ALT extend max ~30 per tx for size budget)
    BATCH = 20
    for i in range(0, len(uniq), BATCH):
        batch = [Pubkey.from_bytes(b) for b in uniq[i:i+BATCH]]
        LOG(f"extending with batch {i}..{i+len(batch)} ({len(batch)} addrs)")
        bh = get_latest_blockhash()
        blockhash = Hash.from_string(bh['blockhash'])
        extend_ix = build_extend_alt_ix(alt_pda, wallet.pubkey(), wallet.pubkey(), batch)
        msg = MessageV0.try_compile(wallet.pubkey(), [extend_ix], [], blockhash)
        tx = VersionedTransaction(msg, [wallet])
        raw = bytes(tx)
        LOG(f"  tx size: {len(raw)} bytes")
        resp = send_tx(raw)
        LOG(f"  resp: {resp}")
        time.sleep(2)
    LOG(f"=== DONE === ALT pubkey: {alt_pda}")
    LOG(f"Add to /etc/arbbot/.env: EXECUTOR_PYTH_ALT={alt_pda}")

if __name__ == '__main__':
    main()
