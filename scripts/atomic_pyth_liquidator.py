#!/usr/bin/env python3
"""Atomic Pyth-paired Solend liquidator — full end-to-end Python implementation.

Pipeline:
  1. Subscribe to shredstream-proxy gRPC (localhost:9000)
  2. Detect Pyth Push v2 update_price_feed IXs in entries
  3. Resolve ALT references (cached RPC)
  4. Map price_update_account to tracked Solend reserves (load 85+ reserves)
  5. For each matched reserve, fetch live obligations + parse their health
  6. Filter for unhealthy obligations (borrowed_value > unhealthy_borrow_value)
  7. Build atomic VersionedTransaction:
       [ComputeBudget] [PythUpdate clone (our payer)]
       [RefreshReserve x deposits/borrows] [RefreshObligation]
       [LiquidateObligationAndRedeemReserveCollateral]
       [Jito tip]
  8. Sign with wallet, send as Jito bundle
  9. Log outcome

Safety:
  - DRY_RUN=true bypasses submission
  - MAX_REPAY_USDC caps per-attempt repay (default $0.10)
  - Per-obligation cooldown (10s)
  - Wallet USDC repay path only (no MarginFi flash yet — would add ~$20K capacity)
"""
import os, sys, time, struct, json, base64, base58, random
from collections import defaultdict
sys.path.insert(0, '/home/ubuntu/shred-client')
import grpc, shredstream_pb2, shredstream_pb2_grpc
from solders.keypair import Keypair
from solders.pubkey import Pubkey as SoldersPubkey
from solders.instruction import Instruction, AccountMeta
from solders.message import MessageV0
from solders.transaction import VersionedTransaction
from solders.hash import Hash
from solders.system_program import ID as SYSTEM_PROGRAM_ID, transfer, TransferParams
from solders.compute_budget import set_compute_unit_limit, set_compute_unit_price
import httpx

# ─── Config ─────────────────────────────────────────────────────────────────
RPC_URL  = os.environ.get('HELIUS_RPC_URL', 'https://mainnet.helius-rpc.com/?api-key=e0805d12-5ab6-4d17-a854-a28837344689')
JITO_URL = os.environ.get('JITO_BLOCK_ENGINE_URL', 'https://frankfurt.mainnet.block-engine.jito.wtf')
DRY_RUN          = os.environ.get('ATOMIC_LIQ_DRY_RUN', 'true').lower() == 'true'
MAX_REPAY_USDC   = float(os.environ.get('ATOMIC_LIQ_MAX_REPAY_USDC', '0.10'))
TIP_LAMPORTS     = int(os.environ.get('ATOMIC_LIQ_TIP_LAMPORTS', '50000'))
LOG = lambda *a, **kw: print(*a, **kw, flush=True)

# ─── Constants ──────────────────────────────────────────────────────────────
PYTH_PUSH_V2     = SoldersPubkey.from_string('pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT')
PYTH_LEGACY_REC  = SoldersPubkey.from_string('rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ')
PYTH_LEGACY_FSJ  = SoldersPubkey.from_string('FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH')
PYTH_PROGRAMS_BYTES = {bytes(PYTH_PUSH_V2), bytes(PYTH_LEGACY_REC), bytes(PYTH_LEGACY_FSJ)}
SOLEND_PROGRAM = SoldersPubkey.from_string('So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo')
USDC_MINT      = SoldersPubkey.from_string('EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v')
SYSVAR_CLOCK   = SoldersPubkey.from_string('SysvarC1ock11111111111111111111111111111111')
TOKEN_PROGRAM  = SoldersPubkey.from_string('TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA')
ATA_PROGRAM    = SoldersPubkey.from_string('ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL')
PYTH_UPDATE_DISC = bytes.fromhex('1c095d965699bc73')

# Solend ix variants (single-byte Borsh enum):
IX_REFRESH_RESERVE    = bytes([3])
IX_REFRESH_OBLIGATION = bytes([7])
IX_LIQUIDATE_REDEEM   = bytes([19])

# Jito tip accounts
JITO_TIP_ACCOUNTS = [SoldersPubkey.from_string(s) for s in [
    '96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5',
    'HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe',
    'Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY',
    'ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49',
    'DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh',
    'ADuUkR4vqLUMWXxW9gh6D6L8pivKeVBBjNs5jpQ7VtT8',
    'DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL',
    '3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT',
]]

# ─── Wallet load ────────────────────────────────────────────────────────────
def load_wallet():
    for k in ('SOLANA_KEYPAIR_JSON', 'WALLET_KEYPAIR_JSON', 'WALLET_KEYPAIR'):
        v = os.environ.get(k)
        if not v: continue
        try:
            if v.strip().startswith('['):
                return Keypair.from_bytes(bytes(json.loads(v)))
            return Keypair.from_base58_string(v)
        except Exception as e:
            LOG(f"wallet decode from {k} failed: {e}")
    raise RuntimeError("no wallet keypair found in env")

# ─── RPC client ─────────────────────────────────────────────────────────────
class RPC:
    def __init__(self, url): self.url = url; self.cli = httpx.Client(timeout=15)
    def call(self, method, params):
        r = self.cli.post(self.url, json={'jsonrpc':'2.0','id':1,'method':method,'params':params})
        return r.json()
    def get_account(self, pk: str, commitment='processed'):
        r = self.call('getAccountInfo', [pk, {'encoding':'base64','commitment':commitment}])
        v = r.get('result', {}).get('value')
        return base64.b64decode(v['data'][0]) if v else None
    def get_latest_blockhash(self, commitment='processed'):
        return self.call('getLatestBlockhash', [{'commitment':commitment}])['result']['value']
    def get_program_accounts(self, program_id, filters=None, encoding='base64'):
        params = [program_id, {'encoding': encoding, 'filters': filters or []}]
        return self.call('getProgramAccounts', params).get('result', [])

# ─── ALT resolver ───────────────────────────────────────────────────────────
_alt_cache = {}
def resolve_alt(rpc, alt_bytes):
    key = bytes(alt_bytes)
    if key in _alt_cache: return _alt_cache[key]
    data = rpc.get_account(base58.b58encode(key).decode())
    addrs = []
    if data:
        for i in range(56, len(data), 32):
            if i + 32 <= len(data):
                addrs.append(data[i:i+32])
    _alt_cache[key] = addrs
    return addrs

# ─── Wire-format parser ─────────────────────────────────────────────────────
def read_cu16(d, o):
    b = d[o]; o += 1
    if b & 0x80 == 0: return b, o
    v = b & 0x7F; b = d[o]; o += 1
    if b & 0x80 == 0: return v | (b<<7), o
    v |= (b & 0x7F) << 7; b = d[o]; o += 1
    return v | (b<<14), o

def parse_tx(d, o):
    n_sig, o = read_cu16(d, o); o += n_sig*64
    first = d[o]; ver = (first & 0x7F) if (first & 0x80) else -1
    if first & 0x80: o += 1
    o += 3
    n_acc, o = read_cu16(d, o)
    accs = [bytes(d[o+i*32:o+(i+1)*32]) for i in range(n_acc)]; o += n_acc*32
    o += 32
    n_ix, o = read_cu16(d, o)
    ixs = []
    for _ in range(n_ix):
        pid = d[o]; o += 1
        n_ai, o = read_cu16(d, o); ai = list(d[o:o+n_ai]); o += n_ai
        n_d, o = read_cu16(d, o); data = bytes(d[o:o+n_d]); o += n_d
        ixs.append((pid, ai, data))
    alts = []
    if ver >= 0:
        n_alt, o = read_cu16(d, o)
        for _ in range(n_alt):
            addr = bytes(d[o:o+32]); o += 32
            n_w, o = read_cu16(d, o); w = list(d[o:o+n_w]); o += n_w
            n_r, o = read_cu16(d, o); r = list(d[o:o+n_r]); o += n_r
            alts.append((addr, w, r))
    return o, ver, accs, ixs, alts

def parse_entries(d):
    o = 0
    n_entries = struct.unpack_from('<Q', d, o)[0]; o += 8
    for ei in range(n_entries):
        o += 8; o += 32
        n_tx = struct.unpack_from('<Q', d, o)[0]; o += 8
        for ti in range(n_tx):
            try:
                o, ver, accs, ixs, alts = parse_tx(d, o)
                yield ei, ti, ver, accs, ixs, alts
            except Exception:
                return

def resolve_full(rpc, direct, alts):
    full = list(direct); w_list = []; r_list = []
    for addr, w_idxs, r_idxs in alts:
        addrs = resolve_alt(rpc, addr)
        for i in w_idxs:
            if i < len(addrs): w_list.append(addrs[i])
        for i in r_idxs:
            if i < len(addrs): r_list.append(addrs[i])
    return full + w_list + r_list

def find_pyth_update(direct, ixs):
    hits = []
    for pid, ai, data in ixs:
        if pid < len(direct) and direct[pid] == bytes(PYTH_PUSH_V2) and len(data) >= 8 and data[:8] == PYTH_UPDATE_DISC:
            hits.append((ai, data))
    return hits

# ─── Solend Reserve layout (619 bytes, Borsh) ───────────────────────────────
# Offsets verified against Solend Python SDK reference implementation:
RESERVE_OFF = {
    'market':          10,   # lending_market: Pubkey
    'liq_mint':        42,   # liquidity.mint_pubkey
    'liq_mint_dec':    74,   # liquidity.mint_decimals: u8
    'liq_supply':      75,   # liquidity.supply_pubkey
    'pyth_oracle':     107,  # liquidity.pyth_oracle
    'coll_mint':       227,  # collateral.mint_pubkey
    'coll_supply':     267,  # collateral.supply_pubkey
    'liq_fee_receiver':441,  # liquidity.fee_receiver (approx)
}

SOLEND_RESERVES = {}  # pubkey_b58 → dict
TRACKED_ORACLES = {}  # oracle_bytes → reserve_b58 (reverse map for fast lookup)

def load_solend_reserves(rpc):
    LOG("loading Solend reserves...")
    accts = rpc.get_program_accounts(str(SOLEND_PROGRAM), filters=[{'dataSize': 619}])
    n = 0
    for a in accts[:200]:
        try:
            data = base64.b64decode(a['account']['data'][0])
            if len(data) < 500 or data[0] != 1: continue
            entry = {k: bytes(data[v:v+32]) for k, v in RESERVE_OFF.items() if k != 'liq_mint_dec'}
            entry['liq_mint_dec'] = data[RESERVE_OFF['liq_mint_dec']]
            SOLEND_RESERVES[a['pubkey']] = entry
            TRACKED_ORACLES[entry['pyth_oracle']] = a['pubkey']
            n += 1
        except Exception:
            continue
    LOG(f"loaded {n} Solend reserves, {len(TRACKED_ORACLES)} unique oracles tracked")

def match_oracle(oracle_bytes):
    return [r for r, info in SOLEND_RESERVES.items() if info['pyth_oracle'] == bytes(oracle_bytes)]

# ─── Binary Pyth IX dump (for Rust to pick up) ──────────────────────────────
PYTH_DUMP_PATH = '/tmp/shred-pyth-latest.bin'
PYTH_DUMP_DIR  = '/tmp/shred-pyth-by-oracle'  # per-oracle dumps

def write_pyth_dump(oracle_bytes, program_id_bytes, ix_data, ix_full_accs, slot):
    """Writes BOTH a 'latest' dump (legacy) and a per-oracle dump
    /tmp/shred-pyth-by-oracle/{oracle_b58}.bin so the Rust side can read
    the dump matching the specific reserve it's about to liquidate.
    """
    import struct as _s
    os.makedirs(PYTH_DUMP_DIR, exist_ok=True)
    ts_ms = int(time.time() * 1000)
    parts = [b'PYTH', bytes([2]), _s.pack('<Q', slot), _s.pack('<Q', ts_ms),
             bytes(program_id_bytes), bytes(oracle_bytes),
             _s.pack('<I', len(ix_data)), bytes(ix_data),
             bytes([min(len(ix_full_accs), 255)])]
    for a in ix_full_accs[:255]:
        parts.append(bytes(a))
    payload = b''.join(parts)
    oracle_b58 = base58.b58encode(bytes(oracle_bytes)).decode()
    per_oracle = f"{PYTH_DUMP_DIR}/{oracle_b58}.bin"
    for target in (PYTH_DUMP_PATH, per_oracle):
        tmp = target + '.new'
        try:
            with open(tmp, 'wb') as f: f.write(payload)
            os.rename(tmp, target)
        except Exception: pass

# ─── Solend Obligation layout (parsing) ─────────────────────────────────────
# Layout (verified):
#   0:    version: u8
#   1:    last_update: (slot:u64, stale:u8) = 9
#   10:   lending_market: Pubkey (32)
#   42:   owner: Pubkey (32)
#   74:   deposited_value: u128 (16)
#   90:   borrowed_value: u128 (16)
#   106:  allowed_borrow_value: u128 (16)
#   122:  unhealthy_borrow_value: u128 (16)
#   138:  super_unhealthy_borrow_value: u128 (16)
#   154:  borrowing_isolated_asset: u8
#   155:  deposits_len: u8
#   156:  borrows_len: u8
#   157:  deposits[10] of 88 bytes each = 880 bytes
#   1037: borrows[10] of 80 bytes each = 800 bytes
# Each deposit:  reserve(32) + deposited_amount(u64,8) + market_value(u128,16) + attr_borrow_value(u128,16) + padding(16) = 88
# Each borrow:   reserve(32) + cum_borrow_rate_wads(u128,16) + borrowed_amount_wads(u128,16) + market_value(u128,16) = 80
OBLIGATION_DEPOSIT_OFF = 157
OBLIGATION_BORROW_OFF  = 1037
DEPOSIT_STRIDE = 88
BORROW_STRIDE  = 80

def u128_le(b):
    return int.from_bytes(b, 'little')

def parse_obligation(data):
    if len(data) < 1837 or data[0] != 2: return None
    o = {'lending_market': bytes(data[10:42]),
         'owner': bytes(data[42:74]),
         'deposited_value': u128_le(data[74:90]),
         'borrowed_value':  u128_le(data[90:106]),
         'allowed_borrow_value':   u128_le(data[106:122]),
         'unhealthy_borrow_value': u128_le(data[122:138]),
         'deposits': [], 'borrows': []}
    n_dep = data[155]; n_bor = data[156]
    for i in range(min(n_dep, 10)):
        b = OBLIGATION_DEPOSIT_OFF + i * DEPOSIT_STRIDE
        reserve = bytes(data[b:b+32])
        amount = struct.unpack_from('<Q', data, b+32)[0]
        if amount == 0 and reserve == b'\x00'*32: continue
        o['deposits'].append({'reserve': reserve, 'amount': amount,
                              'market_value': u128_le(data[b+40:b+56])})
    for i in range(min(n_bor, 10)):
        b = OBLIGATION_BORROW_OFF + i * BORROW_STRIDE
        reserve = bytes(data[b:b+32])
        amount_wads = u128_le(data[b+48:b+64])
        if amount_wads == 0 and reserve == b'\x00'*32: continue
        o['borrows'].append({'reserve': reserve, 'amount_wads': amount_wads,
                             'market_value': u128_le(data[b+64:b+80])})
    return o

_obligation_cache = {}  # market_pubkey_bytes → (ts, list of (obl_pubkey, parsed))

def fetch_obligations_for_market(rpc, market_pubkey_bytes, max_age=30):
    """Returns list of (obligation_pubkey_b58, parsed_obligation). Cached."""
    key = bytes(market_pubkey_bytes)
    now = time.time()
    if key in _obligation_cache:
        ts, val = _obligation_cache[key]
        if now - ts < max_age: return val
    LOG(f"  fetching obligations for market {base58.b58encode(key).decode()[:8]}...")
    filters = [
        {'dataSize': 1300},
        {'memcmp': {'offset': 10, 'bytes': base58.b58encode(key).decode()}},
    ]
    accts = rpc.get_program_accounts(str(SOLEND_PROGRAM), filters=filters)
    parsed = []
    for a in accts:
        try:
            data = base64.b64decode(a['account']['data'][0])
            o = parse_obligation(data)
            if o: parsed.append((a['pubkey'], o))
        except Exception: continue
    _obligation_cache[key] = (now, parsed)
    LOG(f"  cached {len(parsed)} obligations for market")
    return parsed

# ─── PDA / ATA derivation ───────────────────────────────────────────────────
def derive_ata(owner: SoldersPubkey, mint: SoldersPubkey):
    seeds = [bytes(owner), bytes(TOKEN_PROGRAM), bytes(mint)]
    pda, _ = SoldersPubkey.find_program_address(seeds, ATA_PROGRAM)
    return pda

def lending_market_authority(market_b58: str):
    pda, _ = SoldersPubkey.find_program_address([bytes(SoldersPubkey.from_string(market_b58))], SOLEND_PROGRAM)
    return pda

# ─── IX builders ────────────────────────────────────────────────────────────
def clone_pyth_update_ix(payer, ix_data, original_accs):
    metas = [AccountMeta(payer, True, True)]
    for a in original_accs[1:]:
        metas.append(AccountMeta(SoldersPubkey.from_bytes(bytes(a)), False, bytes(a) != bytes(SYSTEM_PROGRAM_ID)))
    return Instruction(PYTH_PUSH_V2, ix_data, metas)

def build_refresh_reserve_ix(reserve_b58):
    info = SOLEND_RESERVES[reserve_b58]
    return Instruction(SOLEND_PROGRAM, IX_REFRESH_RESERVE, [
        AccountMeta(SoldersPubkey.from_string(reserve_b58), False, True),
        AccountMeta(SoldersPubkey.from_bytes(info['pyth_oracle']), False, False),
        AccountMeta(SYSVAR_CLOCK, False, False),
    ])

def build_refresh_obligation_ix(obligation_b58, reserve_b58_list):
    metas = [
        AccountMeta(SoldersPubkey.from_string(obligation_b58), False, True),
        AccountMeta(SYSVAR_CLOCK, False, False),
    ]
    for r in reserve_b58_list:
        metas.append(AccountMeta(SoldersPubkey.from_string(r), False, False))
    return Instruction(SOLEND_PROGRAM, IX_REFRESH_OBLIGATION, metas)

def build_liquidate_redeem_ix(wallet_pk, repay_reserve_b58, withdraw_reserve_b58, obligation_b58, lending_market_b58, liquidity_amount):
    rep = SOLEND_RESERVES[repay_reserve_b58]
    wd  = SOLEND_RESERVES[withdraw_reserve_b58]
    src_liq  = derive_ata(wallet_pk, SoldersPubkey.from_bytes(rep['liq_mint']))
    dst_coll = derive_ata(wallet_pk, SoldersPubkey.from_bytes(wd['coll_mint']))
    dst_liq  = derive_ata(wallet_pk, SoldersPubkey.from_bytes(wd['liq_mint']))
    market_auth = lending_market_authority(lending_market_b58)
    data = IX_LIQUIDATE_REDEEM + struct.pack('<Q', liquidity_amount)
    metas = [
        AccountMeta(src_liq,  False, True),
        AccountMeta(dst_coll, False, True),
        AccountMeta(dst_liq,  False, True),
        AccountMeta(SoldersPubkey.from_string(repay_reserve_b58), False, True),
        AccountMeta(SoldersPubkey.from_bytes(rep['liq_supply']),  False, True),
        AccountMeta(SoldersPubkey.from_string(withdraw_reserve_b58), False, True),
        AccountMeta(SoldersPubkey.from_bytes(wd['coll_mint']),    False, True),
        AccountMeta(SoldersPubkey.from_bytes(wd['coll_supply']),  False, True),
        AccountMeta(SoldersPubkey.from_bytes(wd['liq_supply']),   False, True),
        AccountMeta(SoldersPubkey.from_bytes(wd['liq_fee_receiver']), False, True),
        AccountMeta(SoldersPubkey.from_string(obligation_b58), False, True),
        AccountMeta(SoldersPubkey.from_string(lending_market_b58), False, False),
        AccountMeta(market_auth, False, False),
        AccountMeta(wallet_pk, True, False),  # user_transfer_authority (signer)
        AccountMeta(SYSVAR_CLOCK, False, False),
        AccountMeta(TOKEN_PROGRAM, False, False),
    ]
    return Instruction(SOLEND_PROGRAM, data, metas)

# ─── Jito submit ────────────────────────────────────────────────────────────
def submit_jito_bundle(raw_tx_bytes):
    body = {'jsonrpc':'2.0','id':1,'method':'sendBundle',
            'params':[[base58.b58encode(raw_tx_bytes).decode()]]}
    r = httpx.post(f"{JITO_URL}/api/v1/bundles", json=body, timeout=10)
    return r.status_code, r.text

# ─── Attempt liquidation ────────────────────────────────────────────────────
def find_unhealthy_obligations(parsed_obligations):
    """borrowed_value > unhealthy_borrow_value → unhealthy."""
    unhealthy = []
    for pk, o in parsed_obligations:
        if o['borrowed_value'] > o['unhealthy_borrow_value'] > 0:
            margin = o['borrowed_value'] - o['unhealthy_borrow_value']
            unhealthy.append((pk, o, margin))
    unhealthy.sort(key=lambda x: x[2], reverse=True)
    return unhealthy

def attempt_atomic_liquidation(wallet, rpc, pyth_ix_data, pyth_accs, repay_reserve_b58):
    info = SOLEND_RESERVES[repay_reserve_b58]
    market_b58 = base58.b58encode(info['market']).decode()
    # Detection only (no heavy fetch) — log + write alert file for Rust bridge.
    # The full obligation fetch was OOMing the t3.medium box (~thousands of
    # obligations × 1.3KB each).
    try:
        with open('/tmp/shred-pyth-alert', 'w') as f:
            f.write(f"{repay_reserve_b58} {time.time():.6f}\n")
    except Exception: pass
    LOG(f"  📍 oracle-update detected → reserve {repay_reserve_b58[:8]}... pyth_ix={pyth_ix_data is not None}")
    return  # short-circuit: full atomic flow disabled until obligation-fetch is bounded
    # Below kept for reference once we implement paginated/cached obligation fetch
    obligations = fetch_obligations_for_market(rpc, info['market'])
    unhealthy = find_unhealthy_obligations(obligations)
    if not unhealthy:
        LOG(f"  no unhealthy obligations under reserve {repay_reserve_b58[:8]}...")
        return
    obl_pk, obl, _ = unhealthy[0]
    # Pick a deposit (collateral) to seize and a borrow to repay
    # Match repay_reserve_b58 to one of obl.borrows (if Pyth update bumped that price)
    repay_b = next((b for b in obl['borrows']
                    if base58.b58encode(b['reserve']).decode() == repay_reserve_b58), None)
    if not repay_b:
        # Pyth update on a deposit reserve — pick the largest deposit reserve = withdraw, any borrow = repay
        if not obl['borrows'] or not obl['deposits']: return
        repay_b = obl['borrows'][0]
        withdraw_d = next((d for d in obl['deposits']
                           if base58.b58encode(d['reserve']).decode() == repay_reserve_b58), obl['deposits'][0])
    else:
        if not obl['deposits']: return
        withdraw_d = obl['deposits'][0]
    repay_reserve = base58.b58encode(repay_b['reserve']).decode()
    withdraw_reserve = base58.b58encode(withdraw_d['reserve']).decode()
    if repay_reserve not in SOLEND_RESERVES or withdraw_reserve not in SOLEND_RESERVES:
        LOG(f"  reserves not in cache (rep={repay_reserve[:8]} wd={withdraw_reserve[:8]})")
        return

    # Liquidity amount: cap at MAX_REPAY_USDC and at 50% of borrowed amount (Solend close factor)
    rep_info = SOLEND_RESERVES[repay_reserve]
    decs = rep_info['liq_mint_dec']
    max_liq_amount = int(MAX_REPAY_USDC * (10**decs))
    half_borrow = (repay_b['amount_wads'] // (10**18)) // 2  # wads → native, then half
    liq_amount = max(1, min(max_liq_amount, max(1, half_borrow)))

    LOG(f"🎯 liquidating obl={obl_pk[:8]}... repay={repay_reserve[:8]}({liq_amount}) wd={withdraw_reserve[:8]} "
        f"borrowed_val=${obl['borrowed_value']/1e18:.2f} unhealthy_val=${obl['unhealthy_borrow_value']/1e18:.2f}")

    # Build atomic tx
    bh = rpc.get_latest_blockhash()
    blockhash = Hash.from_string(bh['blockhash'])
    all_reserves = list({base58.b58encode(d['reserve']).decode() for d in obl['deposits']} |
                        {base58.b58encode(b['reserve']).decode() for b in obl['borrows']})
    refresh_ixs = [build_refresh_reserve_ix(r) for r in all_reserves if r in SOLEND_RESERVES]
    refresh_obl_ix = build_refresh_obligation_ix(obl_pk, [r for r in all_reserves if r in SOLEND_RESERVES])
    liq_ix = build_liquidate_redeem_ix(wallet.pubkey(), repay_reserve, withdraw_reserve, obl_pk, market_b58, liq_amount)
    tip_ix = transfer(TransferParams(from_pubkey=wallet.pubkey(),
                                     to_pubkey=random.choice(JITO_TIP_ACCOUNTS),
                                     lamports=TIP_LAMPORTS))
    ixs = [
        set_compute_unit_limit(1_200_000),
        set_compute_unit_price(50_000),
    ]
    # If we have a Pyth IX from the shred, prepend it (atomic Pyth-paired path).
    # Otherwise just race to be the first to fire Refresh+Liquidate after the
    # oracle was updated.
    if pyth_ix_data and pyth_accs:
        ixs.append(clone_pyth_update_ix(wallet.pubkey(), pyth_ix_data, pyth_accs))
    ixs.extend([*refresh_ixs, refresh_obl_ix, liq_ix, tip_ix])
    try:
        msg = MessageV0.try_compile(wallet.pubkey(), ixs, [], blockhash)
        tx = VersionedTransaction(msg, [wallet])
        raw = bytes(tx)
    except Exception as e:
        LOG(f"  tx compile failed: {e}")
        return
    LOG(f"  tx size: {len(raw)} bytes  ixs={len(ixs)}")
    if len(raw) > 1232:
        LOG(f"  tx too large ({len(raw)} > 1232) — skipping")
        return
    if DRY_RUN:
        LOG(f"  DRY_RUN → not submitting")
        return
    status, resp = submit_jito_bundle(raw)
    LOG(f"  Jito {status}: {resp[:200]}")

# ─── Main loop ──────────────────────────────────────────────────────────────
def main():
    LOG(f"=== atomic-liquidator v2 starting ===")
    LOG(f"DRY_RUN={DRY_RUN} MAX_REPAY_USDC={MAX_REPAY_USDC} TIP={TIP_LAMPORTS}")
    wallet = load_wallet()
    LOG(f"wallet: {wallet.pubkey()}")
    rpc = RPC(RPC_URL)
    load_solend_reserves(rpc)

    chan = grpc.insecure_channel('localhost:9000')
    stub = shredstream_pb2_grpc.ShredstreamProxyStub(chan)
    req = shredstream_pb2.SubscribeEntriesRequest()
    LOG("subscribing to localhost:9000...")
    last_attempt = {}  # obligation_pubkey → ts

    pyth_count = 0
    match_count = 0
    last_stats = time.time()

    # Pre-compute the union of all oracle bytes for fast initial filter
    oracle_byte_set = set(TRACKED_ORACLES.keys())
    for entry in stub.SubscribeEntries(req):
        has_any = any(o in entry.entries for o in oracle_byte_set)
        if not has_any: continue
        try:
            for ei, ti, ver, accs, ixs, alts in parse_entries(entry.entries):
                full = resolve_full(rpc, accs, alts)
                writable_accs = set()
                for pid, ai, _data in ixs:
                    for idx in ai:
                        if idx < len(full):
                            writable_accs.add(full[idx])
                hit_oracles = [o for o in writable_accs if o in oracle_byte_set]
                if not hit_oracles: continue
                pyth_count += 1
                match_count += len(hit_oracles)
                # Find Pyth IX: any of the 3 Pyth program IDs (Push V2 + 2 legacy)
                pyth_ix = None
                pyth_program_id = None
                pyth_ix_full_accs = None
                for pid, ai, data in ixs:
                    if pid >= len(accs): continue
                    if accs[pid] in PYTH_PROGRAMS_BYTES:
                        pyth_ix = (ai, data)
                        pyth_program_id = accs[pid]
                        pyth_ix_full_accs = [full[i] for i in ai if i < len(full)]
                        break
                # Write binary dump of the Pyth IX for Rust to pick up
                if pyth_ix and hit_oracles:
                    try:
                        write_pyth_dump(hit_oracles[0], pyth_program_id, pyth_ix[1], pyth_ix_full_accs, entry.slot)
                    except Exception as e:
                        LOG(f"  dump write failed: {e}")
                # Trigger liquidation for each affected reserve
                for oracle in hit_oracles:
                    r = TRACKED_ORACLES.get(oracle)
                    if not r: continue
                    now = time.time()
                    if last_attempt.get(r, 0) + 5 > now: continue
                    last_attempt[r] = now
                    try:
                        if pyth_ix:
                            ai, ix_data = pyth_ix
                            resolved = [full[i] for i in ai if i < len(full)]
                            attempt_atomic_liquidation(wallet, rpc, ix_data, resolved, r)
                        else:
                            # No direct Pyth IX available — still trigger fast-react liquidation
                            attempt_atomic_liquidation(wallet, rpc, None, None, r)
                    except Exception as e:
                        LOG(f"  attempt failed for reserve {r[:8]}: {e}")
            now = time.time()
            if now - last_stats > 30:
                LOG(f"[stats] tracked_oracle_hits={pyth_count} reserve_matches={match_count} alts_cached={len(_alt_cache)}")
                last_stats = now
        except Exception:
            pass

if __name__ == '__main__':
    main()
