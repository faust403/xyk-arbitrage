//! MarginFi v2 liquidation scout: account + bank parsing, health math,
//! and a candidate-finder that pairs with the atomic-Pyth shred bridge.
//!
//! ## Why this lives next to `solend_liq`
//!
//! Same shape: getProgramAccounts → parse → rank → health-check → fire.
//! Different programs, different account layouts. Solend uses legacy
//! Pyth (Wormhole-guarded) which is invisible to top-level shreds;
//! MarginFi v2 supports `PythPushOracle` (oracle_setup variant 3) which
//! IS top-level — so the atomic Pyth prepend we already built for Solend
//! works here without modification.
//!
//! ## What's implemented vs deferred
//!
//! Implemented (this module):
//! - `parse_bank` / `parse_marginfi_account` — verified offsets
//!   against `mrgnlabs/marginfi-v2` main 2026-05-15 (Bank=1864 on-chain,
//!   MarginfiAccount=2312, balances at +72 each 104 B).
//! - `BankCache` — single getProgramAccounts pass, indexed by bank
//!   pubkey and by `oracle_keys[0]` so a Pyth tick can fan out to all
//!   banks that depend on that oracle.
//! - `compute_health` — sum(asset_value × asset_weight_maint)
//!   − sum(liability_value × liability_weight_maint), in USD.
//!
//! Deferred (separate task):
//! - Oracle price reader. The Pyth update IX captured by the shred
//!   bridge contains the new price inline; decoding it lets us run
//!   health math against the price we're ABOUT to prepend. For now
//!   `compute_health` takes prices as an input map so the caller (a
//!   future scout-loop integration) can supply them however it wants.
//! - Live executor wiring (build + sign + send the v0 tx).

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use arb_dex_obric::parse_pyth_price_any;
use crate::raw_tx::{
    derive_ata, set_compute_unit_limit_ix, set_compute_unit_price_ix, system_transfer_ix,
};

// ─── On-chain constants (verified 2026-05-15) ───────────────────────

pub const MARGINFI_V2_PROGRAM_STR: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

pub const MARGINFI_ACCOUNT_SIZE: usize = 2312;
pub const MARGINFI_BANK_SIZE:    usize = 1864;

pub const MAX_BALANCES: usize = 16;
pub const BALANCE_SIZE: usize = 104;
pub const BALANCES_OFFSET: usize = 72;

pub const MAX_ORACLE_KEYS: usize = 5;

// Bank on-chain offsets (data starts at byte 8 after Anchor disc).
const BANK_OFF_MINT:               usize = 8;
const BANK_OFF_MINT_DECIMALS:      usize = 40;
const BANK_OFF_GROUP:              usize = 41;
const BANK_OFF_ASSET_SHARE_VALUE:  usize = 80;
const BANK_OFF_LIAB_SHARE_VALUE:   usize = 96;
const BANK_OFF_LIQUIDITY_VAULT:    usize = 112;
const BANK_OFF_INSURANCE_VAULT:    usize = 146;
const BANK_OFF_FEE_VAULT:          usize = 200;
const BANK_OFF_CONFIG:             usize = 296;

// BankConfig offsets (config starts at byte 296 on-chain).
const CFG_OFF_ASSET_WEIGHT_INIT:   usize = 0;
const CFG_OFF_ASSET_WEIGHT_MAINT:  usize = 16;
const CFG_OFF_LIAB_WEIGHT_INIT:    usize = 32;
const CFG_OFF_LIAB_WEIGHT_MAINT:   usize = 48;
const CFG_OFF_DEPOSIT_LIMIT:       usize = 64;
const CFG_OFF_ORACLE_SETUP:        usize = 313;
const CFG_OFF_ORACLE_KEYS:         usize = 314;  // [Pubkey; 5] = 160 B
const CFG_OFF_BORROW_LIMIT:        usize = 480;
const CFG_OFF_RISK_TIER:           usize = 488;

/// MarginFi oracle-setup variants. We only care about the variants that
/// resolve to a single Pyth feed accessible via shred-bridge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OracleSetup {
    None              = 0,
    PythLegacy        = 1,
    SwitchboardV2     = 2,
    PythPushOracle    = 3,
    SwitchboardPull   = 4,
    StakedWithPythPush = 5,
    Other             = 255,
}

impl OracleSetup {
    fn from_u8(b: u8) -> Self {
        match b {
            0 => Self::None,
            1 => Self::PythLegacy,
            2 => Self::SwitchboardV2,
            3 => Self::PythPushOracle,
            4 => Self::SwitchboardPull,
            5 => Self::StakedWithPythPush,
            _ => Self::Other,
        }
    }
    /// True for setups whose price-update IX appears at the TOP LEVEL of
    /// a Pyth tx and is therefore observable via the shred-bridge.
    /// `PythLegacy` is deliberately excluded — those updates flow
    /// through Wormhole guardian CPI (same structural blocker that
    /// killed the Solend atomic flow on 2026-05-14). Push V2 + staked
    /// variants ARE top-level.
    pub fn is_pyth_observable(self) -> bool {
        matches!(self, Self::PythPushOracle | Self::StakedWithPythPush)
    }
}

// ─── Bank ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BankState {
    pub address:               Pubkey,
    pub mint:                  Pubkey,
    pub mint_decimals:         u8,
    pub group:                 Pubkey,
    pub asset_share_value:     f64,  // I80F48 native scale (1.0 = 1 token)
    pub liability_share_value: f64,
    pub liquidity_vault:       Pubkey,
    pub insurance_vault:       Pubkey,
    pub fee_vault:             Pubkey,
    pub asset_weight_init:     f64,
    pub asset_weight_maint:    f64,
    pub liability_weight_init: f64,
    pub liability_weight_maint: f64,
    pub deposit_limit:         u64,
    pub borrow_limit:          u64,
    pub oracle_setup:          OracleSetup,
    /// First oracle_key is the primary feed; others are fallback /
    /// secondary feeds (relevant for staked-collateral setups).
    pub oracle_keys:           [Pubkey; MAX_ORACLE_KEYS],
    pub risk_tier:             u8,
}

impl BankState {
    pub fn primary_oracle(&self) -> Pubkey { self.oracle_keys[0] }
}

pub fn parse_bank(address: Pubkey, bytes: &[u8]) -> Option<BankState> {
    if bytes.len() < MARGINFI_BANK_SIZE { return None; }
    let mint              = read_pubkey(bytes, BANK_OFF_MINT)?;
    let mint_decimals     = bytes[BANK_OFF_MINT_DECIMALS];
    let group             = read_pubkey(bytes, BANK_OFF_GROUP)?;
    let asset_share_value     = wrapped_i80f48(bytes, BANK_OFF_ASSET_SHARE_VALUE)?;
    let liability_share_value = wrapped_i80f48(bytes, BANK_OFF_LIAB_SHARE_VALUE)?;
    let liquidity_vault   = read_pubkey(bytes, BANK_OFF_LIQUIDITY_VAULT)?;
    let insurance_vault   = read_pubkey(bytes, BANK_OFF_INSURANCE_VAULT)?;
    let fee_vault         = read_pubkey(bytes, BANK_OFF_FEE_VAULT)?;

    let cfg = BANK_OFF_CONFIG;
    let asset_weight_init      = wrapped_i80f48(bytes, cfg + CFG_OFF_ASSET_WEIGHT_INIT)?;
    let asset_weight_maint     = wrapped_i80f48(bytes, cfg + CFG_OFF_ASSET_WEIGHT_MAINT)?;
    let liability_weight_init  = wrapped_i80f48(bytes, cfg + CFG_OFF_LIAB_WEIGHT_INIT)?;
    let liability_weight_maint = wrapped_i80f48(bytes, cfg + CFG_OFF_LIAB_WEIGHT_MAINT)?;
    let deposit_limit  = read_u64(bytes, cfg + CFG_OFF_DEPOSIT_LIMIT)?;
    let borrow_limit   = read_u64(bytes, cfg + CFG_OFF_BORROW_LIMIT)?;
    let oracle_setup   = OracleSetup::from_u8(bytes[cfg + CFG_OFF_ORACLE_SETUP]);
    let risk_tier      = bytes[cfg + CFG_OFF_RISK_TIER];

    let mut oracle_keys = [Pubkey::default(); MAX_ORACLE_KEYS];
    for i in 0..MAX_ORACLE_KEYS {
        oracle_keys[i] = read_pubkey(bytes, cfg + CFG_OFF_ORACLE_KEYS + i * 32)?;
    }

    Some(BankState {
        address, mint, mint_decimals, group,
        asset_share_value, liability_share_value,
        liquidity_vault, insurance_vault, fee_vault,
        asset_weight_init, asset_weight_maint,
        liability_weight_init, liability_weight_maint,
        deposit_limit, borrow_limit, oracle_setup, oracle_keys, risk_tier,
    })
}

// ─── MarginfiAccount ──────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Balance {
    pub active:           bool,
    pub bank_pk:          Pubkey,
    pub asset_shares:     f64,
    pub liability_shares: f64,
}

#[derive(Debug, Clone)]
pub struct ParsedAccount {
    pub address:   Pubkey,
    pub group:     Pubkey,
    pub authority: Pubkey,
    pub balances:  [Balance; MAX_BALANCES],
}

impl ParsedAccount {
    pub fn active_balances(&self) -> impl Iterator<Item = &Balance> {
        self.balances.iter().filter(|b| b.active)
    }
    /// True if any active balance has a non-zero liability — only such
    /// accounts can ever become liquidation targets.
    pub fn has_debt(&self) -> bool {
        self.active_balances().any(|b| b.liability_shares > 0.0)
    }
}

/// Minimum bytes parse_marginfi_account needs — through the end of the
/// 16th balance. Callers that pass `dataSlice` to getProgramAccounts to
/// save memory should keep at least this many bytes; the trailing
/// account_flags / emissions / health_cache fields are unused here.
pub const MARGINFI_ACCOUNT_PARSED_PREFIX: usize = BALANCES_OFFSET + MAX_BALANCES * BALANCE_SIZE;  // 1736

pub fn parse_marginfi_account(address: Pubkey, bytes: &[u8]) -> Option<ParsedAccount> {
    if bytes.len() < MARGINFI_ACCOUNT_PARSED_PREFIX { return None; }
    let group     = read_pubkey(bytes, 8)?;
    let authority = read_pubkey(bytes, 40)?;
    let mut balances: [Balance; MAX_BALANCES] = [Balance {
        active: false, bank_pk: Pubkey::default(),
        asset_shares: 0.0, liability_shares: 0.0,
    }; MAX_BALANCES];
    for i in 0..MAX_BALANCES {
        let off = BALANCES_OFFSET + i * BALANCE_SIZE;
        balances[i] = parse_balance(&bytes[off..off + BALANCE_SIZE])?;
    }
    Some(ParsedAccount { address, group, authority, balances })
}

fn parse_balance(b: &[u8]) -> Option<Balance> {
    Some(Balance {
        active:           b[0] == 1,
        bank_pk:          read_pubkey(b, 1)?,
        asset_shares:     wrapped_i80f48(b, 40)?,
        liability_shares: wrapped_i80f48(b, 56)?,
    })
}

// ─── BankCache ────────────────────────────────────────────────────

/// Snapshot of every Bank account on the program, indexed two ways so
/// the liquidator can resolve `bank_pk → BankState` (per-balance lookup)
/// AND `oracle → Vec<bank>` (Pyth-tick fan-out).
#[derive(Debug, Default)]
pub struct BankCache {
    pub by_bank:   HashMap<Pubkey, BankState>,
    pub by_oracle: HashMap<Pubkey, Vec<Pubkey>>,
}

impl BankCache {
    pub fn banks_for_oracle(&self, oracle: &Pubkey) -> &[Pubkey] {
        self.by_oracle.get(oracle).map(|v| v.as_slice()).unwrap_or(&[])
    }
}

pub async fn fetch_bank_cache(rpc_url: &str) -> anyhow::Result<BankCache> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
        "params":[MARGINFI_V2_PROGRAM_STR, {
            "encoding": "base64",
            "commitment": "confirmed",
            "filters": [{"dataSize": MARGINFI_BANK_SIZE}],
        }],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await
        .context("getProgramAccounts (banks) http")?
        .error_for_status()?
        .json().await
        .context("getProgramAccounts (banks) json")?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no result array: {r}"))?;

    let mut cache = BankCache::default();
    for entry in arr {
        let pubkey = match entry.get("pubkey").and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Pubkey>().ok()) { Some(p) => p, None => continue };
        let data_b64 = entry.pointer("/account/data/0").and_then(|v| v.as_str()).unwrap_or("");
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let Some(bank) = parse_bank(pubkey, &bytes) else { continue };
        // Index by primary oracle only — secondary keys are fallbacks
        // that don't drive price-discovery events. If we ever care about
        // staked-collateral fallbacks, extend here.
        let oracle = bank.primary_oracle();
        if oracle != Pubkey::default() {
            cache.by_oracle.entry(oracle).or_default().push(pubkey);
        }
        cache.by_bank.insert(pubkey, bank);
    }
    Ok(cache)
}

// ─── Health math ─────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Candidate {
    pub address:            Pubkey,
    pub authority:          Pubkey,
    pub health_usd:         f64,
    pub total_asset_usd:    f64,
    pub total_liab_usd:     f64,
    /// Best asset bank to seize (highest USD value among the account's
    /// active asset positions). The liquidate IX needs one asset_bank
    /// pick per call.
    pub best_asset_bank:    Option<Pubkey>,
    /// Best liability bank to repay (highest USD value among the
    /// account's active liability positions).
    pub best_liab_bank:     Option<Pubkey>,
}

/// Compute account health in USD using maintenance weights.
///
/// `oracle_prices` maps `bank_address → price_usd_per_token` (NOT per
/// unit — caller scales by mint_decimals). The function ignores any
/// active balance whose bank is missing from `cache` or whose price is
/// missing from `oracle_prices`.
pub fn compute_health(
    account:       &ParsedAccount,
    cache:         &BankCache,
    oracle_prices: &HashMap<Pubkey, f64>,
) -> Candidate {
    let mut total_asset_usd: f64 = 0.0;
    let mut total_liab_usd:  f64 = 0.0;
    let mut health_usd:      f64 = 0.0;
    let mut best_asset: Option<(Pubkey, f64)> = None;
    let mut best_liab:  Option<(Pubkey, f64)> = None;

    for bal in account.active_balances() {
        let Some(bank) = cache.by_bank.get(&bal.bank_pk) else { continue };
        let Some(&price_usd) = oracle_prices.get(&bal.bank_pk) else { continue };
        let dec_factor = 10f64.powi(bank.mint_decimals as i32);

        if bal.asset_shares > 0.0 {
            let tokens = bal.asset_shares * bank.asset_share_value;
            let usd    = tokens * price_usd / dec_factor;
            total_asset_usd += usd;
            health_usd += usd * bank.asset_weight_maint;
            if best_asset.map_or(true, |(_, v)| usd > v) {
                best_asset = Some((bal.bank_pk, usd));
            }
        }
        if bal.liability_shares > 0.0 {
            let tokens = bal.liability_shares * bank.liability_share_value;
            let usd    = tokens * price_usd / dec_factor;
            total_liab_usd += usd;
            health_usd -= usd * bank.liability_weight_maint;
            if best_liab.map_or(true, |(_, v)| usd > v) {
                best_liab = Some((bal.bank_pk, usd));
            }
        }
    }

    Candidate {
        address:         account.address,
        authority:       account.authority,
        health_usd,
        total_asset_usd,
        total_liab_usd,
        best_asset_bank: best_asset.map(|(b, _)| b),
        best_liab_bank:  best_liab.map(|(b, _)| b),
    }
}

/// Convenience: scan a slice of parsed accounts, return only those with
/// `health_usd < 0` sorted by most-underwater first.
pub fn find_liquidatable(
    accounts:      &[ParsedAccount],
    cache:         &BankCache,
    oracle_prices: &HashMap<Pubkey, f64>,
) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = accounts.iter()
        .filter(|a| a.has_debt())
        .map(|a| compute_health(a, cache, oracle_prices))
        .filter(|c| c.health_usd < 0.0)
        .collect();
    out.sort_by(|a, b| a.health_usd.partial_cmp(&b.health_usd).unwrap_or(std::cmp::Ordering::Equal));
    out
}

// ─── Oracle price fetch ──────────────────────────────────────────

/// Batch-fetch Push V2 price-update accounts for a set of oracles via
/// `getMultipleAccounts` (single RPC round-trip, chunked in 100s to
/// respect Helius limits). Returns `oracle_pubkey → price_usd_per_token`.
///
/// Price = raw_price × 10^exponent (price is "1 token = $X" already, no
/// further decimal scaling — callers apply mint_decimals themselves in
/// compute_health).
///
/// Only includes oracles whose Push V2 parse succeeds AND whose feed
/// reports `trading=true`. Non-trading / stale feeds are dropped — the
/// caller treats a missing key as "unknown price, skip this balance".
pub async fn fetch_oracle_prices(
    rpc_url: &str,
    oracles: &[Pubkey],
) -> anyhow::Result<HashMap<Pubkey, f64>> {
    let client = reqwest::Client::new();
    let mut out: HashMap<Pubkey, f64> = HashMap::new();
    for chunk in oracles.chunks(100) {
        let keys: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getMultipleAccounts",
            "params":[keys, {"encoding":"base64","commitment":"confirmed"}],
        });
        let r: serde_json::Value = client.post(rpc_url).json(&body).send().await
            .context("getMultipleAccounts http")?
            .error_for_status()?
            .json().await
            .context("getMultipleAccounts json")?;
        let arr = r.pointer("/result/value").and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("no result/value: {r}"))?;
        for (i, val) in arr.iter().enumerate() {
            if val.is_null() { continue; }
            let data_b64 = val.pointer("/data/0").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b, Err(_) => continue,
            };
            let Some(p) = parse_pyth_price_any(&bytes) else { continue };
            if !p.trading { continue; }
            let usd = (p.price as f64) * 10f64.powi(p.exponent);
            if usd.is_finite() && usd > 0.0 {
                out.insert(chunk[i], usd);
            }
        }
    }
    Ok(out)
}

/// Convenience: builds the `bank → price` map that `compute_health`
/// expects, by walking the cache, fetching every Push-observable
/// oracle, and reverse-mapping back via `by_oracle`. Banks whose
/// primary oracle isn't Push V2 (e.g. `PythLegacy`, `SwitchboardV2`)
/// are SILENTLY DROPPED — health math then ignores those balances.
/// That's deliberate: we only want to act on positions we can price
/// from a shred-observable oracle.
pub async fn fetch_bank_prices(
    rpc_url: &str,
    cache:   &BankCache,
) -> anyhow::Result<HashMap<Pubkey, f64>> {
    let oracles: Vec<Pubkey> = cache.by_bank.values()
        .filter(|b| b.oracle_setup.is_pyth_observable())
        .map(|b| b.primary_oracle())
        .collect();
    let oracle_prices = fetch_oracle_prices(rpc_url, &oracles).await?;

    let mut bank_prices: HashMap<Pubkey, f64> = HashMap::new();
    for (bank_addr, bank) in &cache.by_bank {
        if !bank.oracle_setup.is_pyth_observable() { continue; }
        if let Some(&p) = oracle_prices.get(&bank.primary_oracle()) {
            bank_prices.insert(*bank_addr, p);
        }
    }
    Ok(bank_prices)
}

// ─── Liquidation tx planner ──────────────────────────────────────

/// Inputs to `plan_liquidation_tx`. All amounts are in the smallest
/// units (lamports / token base units). All observation pairs are
/// `(bank_address, primary_oracle_address)` and the program walks them
/// during health re-check inside both `liquidate` AND `end_flashloan`.
#[derive(Debug, Clone)]
pub struct LiquidationPlanInputs<'a> {
    /// Wallet that signs the tx (= the liquidator's MarginFi account
    /// authority).
    pub liquidator_authority: Pubkey,
    /// OUR per-wallet MarginFi account (created once via
    /// `build_initialize_account_ix`).
    pub liquidator_account:   Pubkey,
    /// The underwater account we're liquidating.
    pub liquidatee_account:   Pubkey,
    pub asset_bank:           &'a BankState,
    pub liab_bank:            &'a BankState,
    /// How many tokens of `asset_bank.mint` to seize (in base units).
    pub asset_amount:         u64,
    /// How many tokens of `liab_bank.mint` to flash-borrow (base units).
    /// Bounded by what we expect to repay after the swap. Set this to
    /// `(asset_amount × asset_price ÷ liab_price)` worth of liab tokens
    /// minus a slippage cushion.
    pub flash_amount:         u64,
    /// `(bank, oracle)` for every position our marginfi_account holds.
    pub liquidator_observations: Vec<(Pubkey, Pubkey)>,
    /// `(bank, oracle)` for every position the liquidatee's account
    /// holds. The MarginFi program iterates these inside `liquidate` to
    /// price the full health snapshot.
    pub liquidatee_observations: Vec<(Pubkey, Pubkey)>,
    /// Optional Pyth update IX (cloned from shred-bridge dump) to
    /// prepend INSIDE the flash envelope so liquidate runs against a
    /// price we just submitted, defeating any oracle race.
    pub pyth_prepend:         Option<Instruction>,
    /// Pre-built Jupiter swap IXs (asset_bank.mint → liab_bank.mint).
    /// Empty when no swap is needed (e.g. asset_mint == liab_mint).
    pub jupiter_swap_ixs:     Vec<Instruction>,
    pub compute_unit_limit:   u32,
    pub priority_micro_lamports: u64,
    /// `(tip_account, lamports)` — when set we prepend a system
    /// transfer to the Jito tip account before the flash envelope.
    pub jito_tip:             Option<(Pubkey, u64)>,
}

/// IX layout this planner emits (positions 0-indexed):
///
/// ```text
/// 0  set_compute_unit_limit
/// 1  set_compute_unit_price
/// [optional] jito_tip system_transfer
/// P  lending_account_start_flashloan(end_index)
/// P+1 lending_account_borrow(liab_bank, flash_amount) → liab_ata
/// [optional] PYTH_PREPEND
/// X  lending_account_liquidate(asset_amount)
/// X+1 lending_account_withdraw(asset_bank, all=true) → asset_ata
/// X+2..X+1+N  jupiter_swap leg(s)
/// X+2+N lending_account_deposit(liab_bank, flash_amount) ← repays flash
///                                                          AND clears
///                                                          post-liquidate
///                                                          liab position
/// END lending_account_end_flashloan(bank_oracle_pairs)
/// ```
///
/// Where `P` = prefix length, `X` = position of `liquidate` (after the
/// optional pyth prepend), `N` = jupiter swap IX count, `END` = the
/// 0-based index passed to `start_flashloan` as its required
/// `end_index` argument. The MarginFi program enforces that the
/// instruction at exactly `end_index` is `end_flashloan` — get this
/// wrong and the whole tx reverts with `IxsSysvarMissingReturnData`.
pub fn plan_liquidation_tx(p: &LiquidationPlanInputs) -> Vec<Instruction> {
    use arb_flashloan::marginfi as mfi;

    let mut prefix: Vec<Instruction> = Vec::with_capacity(3);
    prefix.push(set_compute_unit_limit_ix(p.compute_unit_limit));
    prefix.push(set_compute_unit_price_ix(p.priority_micro_lamports));
    if let Some((tip_acct, lamports)) = p.jito_tip {
        prefix.push(system_transfer_ix(p.liquidator_authority, tip_acct, lamports));
    }
    let prefix_len = prefix.len();

    let pyth_count = p.pyth_prepend.is_some() as usize;
    let jupiter_count = p.jupiter_swap_ixs.len();

    // Envelope layout (relative to prefix_len):
    //   0  start_flashloan
    //   1  borrow
    //   2  [pyth_prepend?]
    //   2 + pyth_count  liquidate
    //   3 + pyth_count  withdraw
    //   4 + pyth_count  jupiter[0..N]
    //   4 + pyth_count + N  deposit
    //   5 + pyth_count + N  end_flashloan
    let envelope_len = 6 + pyth_count + jupiter_count;
    let end_index = (prefix_len + envelope_len - 1) as u64;

    let liab_ata  = derive_ata(&p.liquidator_authority, &p.liab_bank.mint);
    let asset_ata = derive_ata(&p.liquidator_authority, &p.asset_bank.mint);

    let liab_ctx = mfi::MarginFiUsdcContext {
        group:                     p.liab_bank.group,
        bank:                      p.liab_bank.address,
        liquidity_vault:           p.liab_bank.liquidity_vault,
        liquidity_vault_authority: mfi::derive_liquidity_vault_authority(&p.liab_bank.address),
    };

    let liq_ctx = mfi::MarginFiLiquidateContext::from_banks(
        p.asset_bank.group,
        p.asset_bank.address,
        p.liab_bank.address,
        p.liquidator_account,
        p.liquidatee_account,
    );

    let liquidator_obs: Vec<mfi::BankOracle> = p.liquidator_observations.iter()
        .map(|(b, o)| mfi::BankOracle { bank: *b, oracle: *o }).collect();
    let liquidatee_obs: Vec<mfi::BankOracle> = p.liquidatee_observations.iter()
        .map(|(b, o)| mfi::BankOracle { bank: *b, oracle: *o }).collect();

    let mut out = prefix;
    out.push(mfi::build_start_flashloan_ix(
        p.liquidator_account, p.liquidator_authority, end_index,
    ));
    out.push(mfi::build_borrow_ix(
        &liab_ctx, p.liquidator_account, p.liquidator_authority,
        liab_ata, p.flash_amount,
    ));
    if let Some(ix) = p.pyth_prepend.clone() { out.push(ix); }
    out.push(mfi::build_liquidate_ix(
        &liq_ctx, p.liquidator_authority, p.asset_amount,
        p.asset_bank.primary_oracle(), p.liab_bank.primary_oracle(),
        &liquidator_obs, &liquidatee_obs,
    ));
    out.push(mfi::build_withdraw_ix(
        p.asset_bank.group, p.liquidator_account, p.liquidator_authority,
        p.asset_bank.address, asset_ata,
        mfi::derive_liquidity_vault_authority(&p.asset_bank.address),
        p.asset_bank.liquidity_vault,
        0, Some(true),
    ));
    out.extend(p.jupiter_swap_ixs.iter().cloned());
    out.push(mfi::build_deposit_ix(
        &liab_ctx, p.liquidator_account, p.liquidator_authority,
        liab_ata, p.flash_amount,
    ));

    // end_flashloan needs (bank, oracle) for every position our acct
    // touched. Union of: pre-existing liquidator observations + the
    // asset_bank + the liab_bank. Dedup by bank address since liquidate
    // may have already given us a position in asset_bank.
    let mut end_pairs: Vec<(Pubkey, Pubkey)> = p.liquidator_observations.clone();
    if !end_pairs.iter().any(|(b, _)| *b == p.asset_bank.address) {
        end_pairs.push((p.asset_bank.address, p.asset_bank.primary_oracle()));
    }
    if !end_pairs.iter().any(|(b, _)| *b == p.liab_bank.address) {
        end_pairs.push((p.liab_bank.address, p.liab_bank.primary_oracle()));
    }
    out.push(mfi::build_end_flashloan_ix(
        p.liquidator_account, p.liquidator_authority, &end_pairs,
    ));

    debug_assert_eq!(out.len() as u64 - 1, end_index,
        "envelope size miscomputed: out.len={} end_index={}", out.len(), end_index);
    out
}

// ─── Per-candidate executor ──────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub enum LiquidationMode {
    /// Build + sign + simulate only. Never sends.
    DryRun,
    /// Build + sign + simulate, then sendTransaction iff simulation
    /// succeeded AND `program_pre_liquidation_health < 0`.
    SendIfTrulyUnderwater,
}

#[derive(Debug, Clone)]
pub struct AttemptConfig {
    pub liquidator_account:    Pubkey,
    pub pyth_alt:              Option<Pubkey>,
    pub close_factor:          f64,
    pub bonus:                 f64,
    pub jito_tip_lamports:     u64,
    pub jito_tip_account:      Pubkey,
    pub compute_unit_limit:    u32,
    pub priority_micro_lamports: u64,
    pub jupiter_max_accounts:  u32,
    pub jupiter_slippage_bps:  u32,
    pub mode:                  LiquidationMode,
}

#[derive(Debug, Clone)]
pub struct AttemptResult {
    pub candidate:              Pubkey,
    pub naive_health_usd:       f64,
    pub asset_bank:             Option<Pubkey>,
    pub liab_bank:              Option<Pubkey>,
    pub tx_size_bytes:          Option<usize>,
    pub program_pre_liquidation_health: Option<f64>,
    pub sim_err:                Option<String>,
    pub send_signature:         Option<String>,
    pub skip_reason:            Option<String>,
}

/// Build, sign, simulate (and conditionally send) a single MarginFi
/// liquidation tx. Reusable from both the one-shot `marginfi-liquidate-once`
/// binary and the `marginfi-hunter` loop.
///
/// Returns early with `skip_reason` set when:
/// - candidate isn't underwater per our naive math
/// - asset or liab oracle isn't Push-V2 observable
/// - any active balance bank uses a non-Push-V2 oracle (would revert 6049)
/// - bank-cache miss for any of the chosen banks
pub async fn attempt_liquidation(
    rpc:        &str,
    wallet:     &crate::wallet::Wallet,
    target:     Pubkey,
    cache:      &BankCache,
    prices:     &HashMap<Pubkey, f64>,
    config:     &AttemptConfig,
) -> anyhow::Result<AttemptResult> {
    let mut result = AttemptResult {
        candidate: target,
        naive_health_usd: 0.0,
        asset_bank: None, liab_bank: None,
        tx_size_bytes: None,
        program_pre_liquidation_health: None,
        sim_err: None, send_signature: None, skip_reason: None,
    };

    // 1. Fetch + parse the candidate.
    let bytes = rpc_get_account_data_inline(rpc, &target).await?;
    let Some(parsed) = parse_marginfi_account(target, &bytes) else {
        result.skip_reason = Some("could not parse MarginfiAccount".to_string());
        return Ok(result);
    };

    // 2. Naive health + bank picks.
    let cand = compute_health(&parsed, cache, prices);
    result.naive_health_usd = cand.health_usd;
    if cand.health_usd >= 0.0 {
        result.skip_reason = Some("naive health >= 0".to_string());
        return Ok(result);
    }
    let Some(asset_bank_pk) = cand.best_asset_bank else {
        result.skip_reason = Some("no Push-V2 asset bank".to_string());
        return Ok(result);
    };
    let Some(liab_bank_pk) = cand.best_liab_bank else {
        result.skip_reason = Some("no Push-V2 liab bank".to_string());
        return Ok(result);
    };
    result.asset_bank = Some(asset_bank_pk);
    result.liab_bank  = Some(liab_bank_pk);

    let Some(asset_bank) = cache.by_bank.get(&asset_bank_pk) else {
        result.skip_reason = Some(format!("asset bank {asset_bank_pk} not in cache"));
        return Ok(result);
    };
    let Some(liab_bank) = cache.by_bank.get(&liab_bank_pk) else {
        result.skip_reason = Some(format!("liab bank {liab_bank_pk} not in cache"));
        return Ok(result);
    };
    let Some(&asset_price) = prices.get(&asset_bank_pk) else {
        result.skip_reason = Some("no price for asset bank".to_string());
        return Ok(result);
    };
    let Some(&liab_price) = prices.get(&liab_bank_pk) else {
        result.skip_reason = Some("no price for liab bank".to_string());
        return Ok(result);
    };

    // 3. Reject non-Push-V2 observation banks (would revert 6049).
    let mut liquidatee_obs: Vec<(Pubkey, Pubkey)> = Vec::new();
    for bal in parsed.active_balances() {
        let Some(bs) = cache.by_bank.get(&bal.bank_pk) else {
            result.skip_reason = Some(format!("obs bank {} not in cache", bal.bank_pk));
            return Ok(result);
        };
        if !bs.oracle_setup.is_pyth_observable() {
            result.skip_reason = Some(format!(
                "obs bank {} non-PushV2 ({:?})", bs.address, bs.oracle_setup));
            return Ok(result);
        }
        liquidatee_obs.push((bal.bank_pk, bs.primary_oracle()));
    }

    // 4. Sizing.
    let liab_usd_to_repay = cand.total_liab_usd * config.close_factor;
    let liab_tokens = liab_usd_to_repay / liab_price;
    let flash_amount = (liab_tokens * 10f64.powi(liab_bank.mint_decimals as i32)) as u64;
    let asset_usd_to_seize = liab_usd_to_repay * (1.0 + config.bonus);
    let asset_tokens = asset_usd_to_seize / asset_price;
    let asset_amount = (asset_tokens * 10f64.powi(asset_bank.mint_decimals as i32)) as u64;

    // 5. Pyth prepend best-effort.
    let payer = wallet.pubkey();
    let pyth_prepend = crate::pyth_shred_dump::build_atomic_pyth_ix(&liab_bank.primary_oracle(), &payer);

    // 6. Jupiter quote + ixs.
    // JupiterSwapClient reads these env vars per-call. Force the values
    // we need here regardless of what /etc/arbbot/.env or systemd
    // EnvironmentFile= set (arbbot expects `restrict=false` for niche
    // routes, but the lite-api free tier rejects that, and the paid
    // endpoint returns over-large routes that don't fit our 1232-byte
    // budget — pinning to `true` forces curated intermediates only).
    std::env::set_var("EXECUTOR_MAX_ACCOUNTS", config.jupiter_max_accounts.to_string());
    std::env::set_var("JUPITER_RESTRICT_INTERMEDIATES", "true");
    let jup = crate::jupiter_swap::JupiterSwapClient::new();
    let quote = match jup.quote(&asset_bank.mint, &liab_bank.mint, asset_amount, config.jupiter_slippage_bps).await {
        Ok(q) => q,
        Err(e) => {
            result.skip_reason = Some(format!("jupiter quote: {e}"));
            return Ok(result);
        }
    };
    let jup_ixs = match jup.swap_instructions(&quote, &payer, false).await {
        Ok(i) => i,
        Err(e) => {
            result.skip_reason = Some(format!("jupiter swap-ixs: {e}"));
            return Ok(result);
        }
    };
    let mut jupiter_swap_ixs: Vec<Instruction> = Vec::new();
    jupiter_swap_ixs.extend(jup_ixs.setup_ixs.iter().cloned());
    jupiter_swap_ixs.push(jup_ixs.swap_ix.clone());
    if let Some(c) = &jup_ixs.cleanup_ix { jupiter_swap_ixs.push(c.clone()); }

    // 7. Plan + build.
    let liquidator_obs: Vec<(Pubkey, Pubkey)> = Vec::new();
    let inputs = LiquidationPlanInputs {
        liquidator_authority: payer,
        liquidator_account:   config.liquidator_account,
        liquidatee_account:   target,
        asset_bank, liab_bank,
        asset_amount, flash_amount,
        liquidator_observations: liquidator_obs,
        liquidatee_observations: liquidatee_obs,
        pyth_prepend,
        jupiter_swap_ixs,
        compute_unit_limit:      config.compute_unit_limit,
        priority_micro_lamports: config.priority_micro_lamports,
        jito_tip: Some((config.jito_tip_account, config.jito_tip_lamports)),
    };
    let ixs = plan_liquidation_tx(&inputs);

    // 8. Collect ALTs.
    //
    // We compress against three classes of ALT:
    //   1. config.pyth_alt (typically EXECUTOR_PYTH_ALT, 245+ Pyth feeds)
    //   2. extras read from env (EXECUTOR_KAMINO_ALT2, EXECUTOR_MARGINFI_ALT,
    //      …) — each may contain MarginFi bank PDAs + oracles that the
    //      liquidate ix's observation list otherwise blows the tx past 1232 B.
    //      Live observation 2026-05-16: without alt2 the tx is 2508-2576 B
    //      and every fire reverts with "VersionedTransaction too large".
    //   3. Jupiter's own ALTs (returned with the swap quote)
    let mut alts: Vec<(Pubkey, Vec<Pubkey>)> = Vec::new();
    if let Some(alt_pk) = config.pyth_alt {
        match crate::alt::fetch_alt_addresses(rpc, &alt_pk).await {
            Ok(addrs) => alts.push((alt_pk, addrs)),
            Err(e) => {
                result.skip_reason = Some(format!("fetch pyth ALT: {e}"));
                return Ok(result);
            }
        }
    }
    for env_key in ["EXECUTOR_KAMINO_ALT2", "EXECUTOR_MARGINFI_ALT", "EXECUTOR_MARGINFI_ALT2"] {
        let Ok(s) = std::env::var(env_key) else { continue };
        let Ok(pk) = s.trim().parse::<Pubkey>() else { continue };
        if alts.iter().any(|(p, _)| *p == pk) { continue; }  // dedup
        if let Ok(addrs) = crate::alt::fetch_alt_addresses(rpc, &pk).await {
            if !addrs.is_empty() { alts.push((pk, addrs)); }
        }
    }
    for jup_alt_pk in &jup_ixs.address_lookup_tables {
        match crate::alt::fetch_alt_addresses(rpc, jup_alt_pk).await {
            Ok(addrs) => alts.push((*jup_alt_pk, addrs)),
            Err(e) => {
                result.skip_reason = Some(format!("fetch jupiter ALT {jup_alt_pk}: {e}"));
                return Ok(result);
            }
        }
    }

    // 9. v0 message + sign.
    let blockhash = crate::raw_tx::fetch_latest_blockhash(rpc).await
        .map_err(|e| anyhow::anyhow!("blockhash: {e}"))?;
    let msg = crate::v0_tx::build_v0_message_multi(&payer, &ixs, &blockhash, &alts, &[]);
    let signed_b64 = crate::v0_tx::sign_v0_tx_b64(&msg, wallet.bytes())
        .map_err(|e| anyhow::anyhow!("sign: {e}"))?;
    let signed_bytes = base64::engine::general_purpose::STANDARD.decode(&signed_b64).unwrap();
    result.tx_size_bytes = Some(signed_bytes.len());

    // 10. Simulate.
    let sim = rpc_simulate_inline(rpc, &signed_b64).await?;
    result.sim_err = sim.err.clone();
    result.program_pre_liquidation_health = parse_pre_liquidation_health(&sim.logs);

    // 11. Send conditionally.
    if matches!(config.mode, LiquidationMode::SendIfTrulyUnderwater) {
        let truly_underwater = result.program_pre_liquidation_health.map(|h| h < 0.0).unwrap_or(false);
        if sim.err.is_none() && truly_underwater {
            let sig = rpc_send_inline(rpc, &signed_b64).await?;
            result.send_signature = Some(sig);
        }
    }

    Ok(result)
}

/// Parse `pre_liquidation_health: X.XXXX (asset_w - liab_w)` from
/// MarginFi program logs. Returns None if the line isn't present.
pub fn parse_pre_liquidation_health(logs: &[String]) -> Option<f64> {
    for line in logs {
        // Format: "Program log: pre_liquidation_health: 3066.773594404618997 (...)"
        if let Some(idx) = line.find("pre_liquidation_health:") {
            let rest = &line[idx + "pre_liquidation_health:".len()..].trim();
            // Take up to the next space or '('
            let end = rest.find(|c: char| c == ' ' || c == '(').unwrap_or(rest.len());
            return rest[..end].trim().parse().ok();
        }
    }
    None
}

// Inline RPC helpers (duplicated from the bins to avoid a circular dep).
async fn rpc_get_account_data_inline(rpc: &str, pk: &Pubkey) -> anyhow::Result<Vec<u8>> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[pk.to_string(), {"encoding":"base64","commitment":"confirmed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.error_for_status()?.json().await?;
    let data_b64 = r.pointer("/result/value/data/0").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no account data at {pk}: {r}"))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(data_b64)?)
}

struct InlineSim {
    err: Option<String>,
    logs: Vec<String>,
}

async fn rpc_simulate_inline(rpc: &str, signed_b64: &str) -> anyhow::Result<InlineSim> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
        "params":[signed_b64, {
            "encoding":"base64", "sigVerify": false, "replaceRecentBlockhash": true,
            "commitment":"processed",
        }],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.error_for_status()?.json().await?;
    if let Some(err) = r.get("error") {
        anyhow::bail!("simulate RPC error: {err}");
    }
    let val = r.pointer("/result/value")
        .ok_or_else(|| anyhow::anyhow!("simulate: missing result.value: {r}"))?;
    let err = val.get("err").and_then(|e| if e.is_null() { None } else { Some(format!("{e}")) });
    let logs = val.get("logs").and_then(|l| l.as_array()).cloned().unwrap_or_default()
        .into_iter().filter_map(|l| l.as_str().map(String::from)).collect();
    Ok(InlineSim { err, logs })
}

async fn rpc_send_inline(rpc: &str, signed_b64: &str) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {
            "encoding":"base64", "skipPreflight": false, "maxRetries": 5,
            "preflightCommitment":"processed",
        }],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.error_for_status()?.json().await?;
    if let Some(err) = r.get("error") {
        anyhow::bail!("send RPC error: {err}");
    }
    Ok(r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("send: missing sig"))?
        .to_string())
}

// ─── Raw byte helpers ────────────────────────────────────────────

fn read_pubkey(bytes: &[u8], off: usize) -> Option<Pubkey> {
    if off + 32 > bytes.len() { return None; }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&bytes[off..off + 32]);
    Some(Pubkey::new_from_array(buf))
}

fn read_u64(bytes: &[u8], off: usize) -> Option<u64> {
    if off + 8 > bytes.len() { return None; }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes[off..off + 8]);
    Some(u64::from_le_bytes(buf))
}

fn read_i128(bytes: &[u8], off: usize) -> Option<i128> {
    if off + 16 > bytes.len() { return None; }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&bytes[off..off + 16]);
    Some(i128::from_le_bytes(buf))
}

/// I80F48 = signed fixed-point with 48 fractional bits, stored as i128 LE.
fn wrapped_i80f48(bytes: &[u8], off: usize) -> Option<f64> {
    let raw = read_i128(bytes, off)?;
    Some((raw as f64) / (1u128 << 48) as f64)
}

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }

    fn make_bank(addr: u8, decimals: u8, asset_w: f64, liab_w: f64) -> BankState {
        BankState {
            address: pk(addr), mint: pk(addr + 100), mint_decimals: decimals,
            group: pk(0), asset_share_value: 1.0, liability_share_value: 1.0,
            liquidity_vault: pk(0), insurance_vault: pk(0), fee_vault: pk(0),
            asset_weight_init: asset_w, asset_weight_maint: asset_w,
            liability_weight_init: liab_w, liability_weight_maint: liab_w,
            deposit_limit: 0, borrow_limit: 0,
            oracle_setup: OracleSetup::PythPushOracle,
            oracle_keys: [pk(addr + 200); MAX_ORACLE_KEYS],
            risk_tier: 0,
        }
    }

    fn make_account(bals: &[(u8, f64, f64)]) -> ParsedAccount {
        let mut balances: [Balance; MAX_BALANCES] = [Balance {
            active: false, bank_pk: Pubkey::default(),
            asset_shares: 0.0, liability_shares: 0.0,
        }; MAX_BALANCES];
        for (i, (bank_id, asset, liab)) in bals.iter().enumerate() {
            balances[i] = Balance {
                active: true, bank_pk: pk(*bank_id),
                asset_shares: *asset, liability_shares: *liab,
            };
        }
        ParsedAccount { address: pk(99), group: pk(0), authority: pk(98), balances }
    }

    #[test]
    fn oracle_setup_from_u8_covers_known_variants() {
        assert_eq!(OracleSetup::from_u8(0), OracleSetup::None);
        assert_eq!(OracleSetup::from_u8(1), OracleSetup::PythLegacy);
        assert_eq!(OracleSetup::from_u8(3), OracleSetup::PythPushOracle);
        assert_eq!(OracleSetup::from_u8(99), OracleSetup::Other);
        assert!(OracleSetup::PythPushOracle.is_pyth_observable());
        assert!(OracleSetup::StakedWithPythPush.is_pyth_observable());
        // PythLegacy is Wormhole-routed → NOT shred-observable (same
        // blocker that killed Solend atomic flow 2026-05-14).
        assert!(!OracleSetup::PythLegacy.is_pyth_observable());
        assert!(!OracleSetup::SwitchboardV2.is_pyth_observable());
    }

    #[test]
    fn wrapped_i80f48_decodes_one() {
        // 1.0 in I80F48 = 1 << 48.
        let mut buf = [0u8; 16];
        buf[..16].copy_from_slice(&(1i128 << 48).to_le_bytes());
        assert!((wrapped_i80f48(&buf, 0).unwrap() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn healthy_account_has_positive_health() {
        // $100 deposit at 0.9 weight, $50 borrow at 1.0 weight.
        // health = 100*0.9 - 50*1.0 = 90 - 50 = 40 > 0.
        let asset_bank = make_bank(1, 0, 0.9, 1.0);
        let liab_bank  = make_bank(2, 0, 0.9, 1.0);
        let mut cache = BankCache::default();
        cache.by_bank.insert(pk(1), asset_bank);
        cache.by_bank.insert(pk(2), liab_bank);
        let acct = make_account(&[(1, 100.0, 0.0), (2, 0.0, 50.0)]);
        let mut prices = HashMap::new();
        prices.insert(pk(1), 1.0);
        prices.insert(pk(2), 1.0);
        let c = compute_health(&acct, &cache, &prices);
        assert!((c.health_usd - 40.0).abs() < 1e-9, "got {}", c.health_usd);
        assert_eq!(c.best_asset_bank, Some(pk(1)));
        assert_eq!(c.best_liab_bank,  Some(pk(2)));
    }

    #[test]
    fn underwater_account_negative_health() {
        // $100 deposit at 0.5 weight, $80 borrow at 1.0 weight.
        // health = 50 - 80 = -30 < 0.
        let asset_bank = make_bank(1, 0, 0.5, 1.0);
        let liab_bank  = make_bank(2, 0, 0.5, 1.0);
        let mut cache = BankCache::default();
        cache.by_bank.insert(pk(1), asset_bank);
        cache.by_bank.insert(pk(2), liab_bank);
        let acct = make_account(&[(1, 100.0, 0.0), (2, 0.0, 80.0)]);
        let mut prices = HashMap::new();
        prices.insert(pk(1), 1.0);
        prices.insert(pk(2), 1.0);
        let c = compute_health(&acct, &cache, &prices);
        assert!(c.health_usd < 0.0);
        assert!((c.health_usd - (-30.0)).abs() < 1e-9);
    }

    #[test]
    fn find_liquidatable_filters_and_sorts() {
        let mut cache = BankCache::default();
        cache.by_bank.insert(pk(1), make_bank(1, 0, 0.5, 1.0));
        let healthy   = make_account(&[(1, 100.0, 40.0)]);     // 50 - 40 = +10
        let underwater_a = make_account(&[(1, 100.0, 80.0)]);  // 50 - 80 = -30
        let underwater_b = make_account(&[(1, 100.0, 60.0)]);  // 50 - 60 = -10
        let mut prices = HashMap::new();
        prices.insert(pk(1), 1.0);
        let out = find_liquidatable(&[healthy, underwater_a, underwater_b], &cache, &prices);
        assert_eq!(out.len(), 2);
        // Most underwater first.
        assert!(out[0].health_usd < out[1].health_usd);
        assert!((out[0].health_usd - (-30.0)).abs() < 1e-9);
    }

    #[test]
    fn balance_struct_size_matches_layout() {
        // Sanity-check that the on-chain layout constants line up.
        assert_eq!(BALANCES_OFFSET + MAX_BALANCES * BALANCE_SIZE, 72 + 16 * 104);
        assert_eq!(72 + 16 * 104, 1736);  // balances region end
    }

    fn dummy_bank(addr: u8) -> BankState {
        make_bank(addr, 6, 0.8, 1.0)
    }

    fn dummy_pyth_ix() -> Instruction {
        Instruction { program_id: pk(50), accounts: vec![], data: vec![1, 2, 3] }
    }

    fn dummy_jup_ix() -> Instruction {
        Instruction { program_id: pk(60), accounts: vec![], data: vec![9, 9, 9] }
    }

    #[test]
    fn plan_envelope_end_index_matches_end_flashloan_position() {
        let asset = dummy_bank(10);
        let liab  = dummy_bank(20);
        let inp = LiquidationPlanInputs {
            liquidator_authority: pk(1), liquidator_account: pk(2),
            liquidatee_account: pk(3),
            asset_bank: &asset, liab_bank: &liab,
            asset_amount: 100, flash_amount: 200,
            liquidator_observations: vec![],
            liquidatee_observations: vec![],
            pyth_prepend: None, jupiter_swap_ixs: vec![],
            compute_unit_limit: 800_000, priority_micro_lamports: 2_000,
            jito_tip: None,
        };
        let ixs = plan_liquidation_tx(&inp);
        // 2 prefix + 0 tip + 1 start + 1 borrow + 0 pyth + 1 liquidate
        //   + 1 withdraw + 0 jupiter + 1 deposit + 1 end = 8
        assert_eq!(ixs.len(), 8);
        // last ix must be end_flashloan (program_id = marginfi)
        assert_eq!(ixs.last().unwrap().program_id, arb_flashloan::marginfi::MARGINFI_V2_PROGRAM);
        // start_flashloan is ix[2] (after 2 prefix ixs); its data should
        // encode end_index = ixs.len()-1 = 7
        let start_ix = &ixs[2];
        let end_idx_from_data = u64::from_le_bytes(start_ix.data[8..16].try_into().unwrap());
        assert_eq!(end_idx_from_data, 7);
    }

    #[test]
    fn plan_envelope_with_pyth_and_jupiter_shifts_indices() {
        let asset = dummy_bank(10);
        let liab  = dummy_bank(20);
        let inp = LiquidationPlanInputs {
            liquidator_authority: pk(1), liquidator_account: pk(2),
            liquidatee_account: pk(3),
            asset_bank: &asset, liab_bank: &liab,
            asset_amount: 100, flash_amount: 200,
            liquidator_observations: vec![],
            liquidatee_observations: vec![],
            pyth_prepend: Some(dummy_pyth_ix()),
            jupiter_swap_ixs: vec![dummy_jup_ix(), dummy_jup_ix(), dummy_jup_ix()],
            compute_unit_limit: 800_000, priority_micro_lamports: 2_000,
            jito_tip: Some((pk(99), 500_000)),
        };
        let ixs = plan_liquidation_tx(&inp);
        // 2 cu + 1 tip + 1 start + 1 borrow + 1 pyth + 1 liquidate
        //   + 1 withdraw + 3 jupiter + 1 deposit + 1 end = 13
        assert_eq!(ixs.len(), 13);
        let start_ix = &ixs[3];  // after cu_limit, cu_price, tip
        let end_idx = u64::from_le_bytes(start_ix.data[8..16].try_into().unwrap());
        assert_eq!(end_idx, 12);
        // Verify pyth prepend landed between borrow (ix[4]) and liquidate
        // (which should be ix[6] = after pyth at ix[5])
        assert_eq!(ixs[5].program_id, pk(50));  // dummy pyth program
    }

    #[test]
    fn plan_end_flashloan_dedupes_asset_and_liab_bank_pairs() {
        let asset = dummy_bank(10);
        let liab  = dummy_bank(20);
        // Pass asset bank as a liquidator observation — end_flashloan
        // should NOT duplicate it.
        let inp = LiquidationPlanInputs {
            liquidator_authority: pk(1), liquidator_account: pk(2),
            liquidatee_account: pk(3),
            asset_bank: &asset, liab_bank: &liab,
            asset_amount: 100, flash_amount: 200,
            liquidator_observations: vec![(asset.address, asset.primary_oracle())],
            liquidatee_observations: vec![],
            pyth_prepend: None, jupiter_swap_ixs: vec![],
            compute_unit_limit: 800_000, priority_micro_lamports: 2_000,
            jito_tip: None,
        };
        let ixs = plan_liquidation_tx(&inp);
        let end_ix = ixs.last().unwrap();
        // end_flashloan accounts = 2 fixed + N*2 pairs.
        // Pairs should be {asset_bank, liab_bank} = 2 pairs → 6 accounts.
        assert_eq!(end_ix.accounts.len(), 2 + 2 * 2);
    }

    #[test]
    fn missing_bank_or_price_is_skipped_not_panic() {
        let cache = BankCache::default();
        let prices = HashMap::new();
        let acct = make_account(&[(1, 100.0, 80.0)]);
        let c = compute_health(&acct, &cache, &prices);
        // No data → zero health, zero totals (don't crash, don't false-positive).
        assert_eq!(c.total_asset_usd, 0.0);
        assert_eq!(c.total_liab_usd,  0.0);
        assert_eq!(c.health_usd,      0.0);
    }
}
