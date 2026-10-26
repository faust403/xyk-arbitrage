//! Solend liquidation hunting — parsers, health calc, IX builder, worker.
//!
//! Layout references (Solend v0.5 / spl-token-lending fork):
//!   Reserve   = 619 bytes
//!   Obligation = 1300 bytes
//!
//! All Decimal fields are u128 little-endian, value × 1e18.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use solana_program::pubkey::Pubkey;
use solana_program::instruction::{AccountMeta, Instruction};
use base64::Engine;
use serde_json::json;
use tracing::info;

use arb_core::{ArbError, ArbResult};

pub const SOLEND_PROGRAM: Pubkey = solana_program::pubkey!("So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo");
pub const SOLEND_MAIN_MARKET: Pubkey = solana_program::pubkey!("4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY");

/// All active Save Finance lending markets (verified empirically by scanning
/// the program's 290-byte accounts and ranking by obligation count). Adding
/// more pools means more candidates per scan — useful since main pool is
/// the most heavily-watched by competing liquidators.
pub fn active_markets() -> Vec<Pubkey> {
    vec![
        SOLEND_MAIN_MARKET, // 271K obligations
        solana_program::pubkey!("HeVhqRY3i22om5a7WGYftAJ2NjJJ3Cg5jnmMCsfFhRG8"), // 6.9K
        solana_program::pubkey!("7RCz8wb6WXxUhAigok9ttgrVgDFFFbibcirECzWSBauM"), // 5.3K
        solana_program::pubkey!("BjsAGLZzAgBUsiaTTDQv7PWDUDL9dQfKvYwb4q6FoDuD"), // 2.3K
        solana_program::pubkey!("GnCfohzaT8uCYsn4ybcsfUZJ8BUuz9F82VivJSW9NMnj"), // 1.7K
        solana_program::pubkey!("616Kxm68sCsFJTzKfqJYt1o2Na5qrujgfhrhUJsSPXHx"), // 1.6K
        solana_program::pubkey!("GktVYgkstojYd8nVXGXKJHi7SstvgZ6pkQqQhUPD7y7Q"), // 1.6K — JLP pool
    ]
}

// ── Decimal helper ──────────────────────────────────────────────────────────
const WAD: u128 = 1_000_000_000_000_000_000; // 1e18

pub fn decimal_to_f64(bytes: &[u8]) -> f64 {
    if bytes.len() < 16 { return 0.0; }
    let mut arr = [0u8; 16];
    arr.copy_from_slice(&bytes[..16]);
    let v = u128::from_le_bytes(arr);
    // Keep precision: split high/low parts before division.
    let whole = (v / WAD) as f64;
    let frac  = (v % WAD) as f64 / WAD as f64;
    whole + frac
}

// ── Reserve ─────────────────────────────────────────────────────────────────
/// Subset of Solend Reserve we need for liquidation work.
#[derive(Debug, Clone)]
pub struct ReserveSummary {
    pub address:               Pubkey,
    pub lending_market:        Pubkey,
    pub liquidity_mint:        Pubkey,
    pub liquidity_decimals:    u8,
    pub liquidity_supply:      Pubkey,
    pub pyth_oracle:           Pubkey,
    pub switchboard_oracle:    Pubkey,
    pub market_price_usd:      f64,
    pub liquidation_threshold: u8, // %
    pub liquidation_bonus:     u8, // %
    pub collateral_mint:       Pubkey,
    pub collateral_supply:     Pubkey,
    pub fee_receiver:          Pubkey,
}

impl ReserveSummary {
    /// Parse from 619-byte Solend Reserve account data.
    pub fn parse(address: Pubkey, data: &[u8]) -> ArbResult<Self> {
        if data.len() < 619 {
            return Err(ArbError::Invariant("solend: reserve data too short"));
        }
        let pk = |off: usize| Pubkey::new_from_array(data[off..off+32].try_into().unwrap());
        // Offsets verified empirically against the USDC reserve by scanning
        // the 619-byte data for known pubkeys (c-USDC mint, fee_receiver).
        // Solend uses the BASE token-lending v0.5 layout WITHOUT the
        // accumulated_protocol_fees_wads field.
        //
        // Header (0-41) + Liquidity (42-184) + Collateral (227-298) + Config (299-).
        let lending_market    = pk(10);
        let liquidity_mint     = pk(42);
        let liquidity_decimals = data[74];
        let liquidity_supply   = pk(75);
        let pyth_oracle        = pk(107);
        let switchboard_oracle = pk(139);
        // Liquidity (42-227): mint(32)+decimals(1)+supply(32)+pyth(32)+sb(32)+
        //   available(8)+borrowed_wads(16)+cumul_rate(16)+market_price(16)
        let market_price_dec   = decimal_to_f64(&data[211..227]);
        // Collateral (211-282): mint(32) + total_supply(8) + supply(32)
        let collateral_mint   = pk(227);
        let collateral_supply = pk(267);
        // Config (299-..):
        //   299 optimal_util | 300 ltv | 301 bonus | 302 threshold
        //   ... | 339 fee_receiver(32)
        let liquidation_bonus     = data[301];
        let liquidation_threshold = data[302];
        let fee_receiver = pk(339);
        Ok(Self {
            address, lending_market, liquidity_mint, liquidity_decimals,
            liquidity_supply, pyth_oracle, switchboard_oracle,
            market_price_usd: market_price_dec,
            liquidation_threshold, liquidation_bonus,
            collateral_mint, collateral_supply, fee_receiver,
        })
    }
}

// ── Obligation ──────────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub struct ObligationCollateral {
    pub deposit_reserve:   Pubkey,
    pub deposited_amount:  u64,
    pub market_value_usd:  f64,
}

#[derive(Debug, Clone)]
pub struct ObligationLiquidity {
    pub borrow_reserve:        Pubkey,
    pub borrowed_amount_wads:  f64, // value × 1e18 → divide by mint decimals at use site
    pub market_value_usd:      f64,
}

#[derive(Debug, Clone)]
pub struct ObligationSummary {
    pub address:                Pubkey,
    pub lending_market:         Pubkey,
    pub owner:                  Pubkey,
    pub deposited_value_usd:    f64,
    pub borrowed_value_usd:     f64,
    pub allowed_borrow_value:   f64,
    pub unhealthy_borrow_value: f64,
    pub deposits:               Vec<ObligationCollateral>,
    pub borrows:                Vec<ObligationLiquidity>,
}

impl ObligationSummary {
    /// Parse from 1300-byte Solend Obligation account data.
    ///
    /// Solend v0.5+ on-chain layout (verified empirically against live obligation
    /// 29YTSiX5… via reserve-pubkey-scan):
    ///   0   version (1)
    ///   1   last_update (9)
    ///  10   lending_market (32)
    ///  42   owner (32)
    ///  74   deposited_value (16)
    ///  90   borrowed_value (16)
    /// 106   allowed_borrow_value (16)
    /// 122   unhealthy_borrow_value (16)
    /// 138-201  super_unhealthy + 3 more Decimals / flags / padding
    /// 202   deposits_len (1)
    /// 203   borrows_len (1)
    /// 204   deposits[] (88 each)
    ///  ...  borrows[] (112 each)
    pub fn parse(address: Pubkey, data: &[u8]) -> ArbResult<Self> {
        if data.len() < 204 {
            return Err(ArbError::Invariant("solend: obligation data too short"));
        }
        let pk = |off: usize| Pubkey::new_from_array(data[off..off+32].try_into().unwrap());
        let lending_market   = pk(10);
        let owner            = pk(42);
        let deposited_value  = decimal_to_f64(&data[74..90]);
        let borrowed_value   = decimal_to_f64(&data[90..106]);
        let allowed_borrow   = decimal_to_f64(&data[106..122]);
        let unhealthy_borrow = decimal_to_f64(&data[122..138]);
        let deposits_len = data[202] as usize;
        let borrows_len  = data[203] as usize;

        // Records start at 204; deposits first (88 bytes each), then borrows (112 bytes each).
        const COLLATERAL_LEN: usize = 88;
        const LIQUIDITY_LEN:  usize = 112;
        let mut off = 204;
        let mut deposits = Vec::with_capacity(deposits_len);
        for _ in 0..deposits_len {
            if data.len() < off + COLLATERAL_LEN { break; }
            let rec = &data[off..off+COLLATERAL_LEN];
            let deposit_reserve  = pk(off);            // rec[0..32]
            let mut amt = [0u8; 8];
            amt.copy_from_slice(&rec[32..40]);
            let deposited_amount = u64::from_le_bytes(amt);
            let market_value     = decimal_to_f64(&rec[40..56]);
            deposits.push(ObligationCollateral { deposit_reserve, deposited_amount, market_value_usd: market_value });
            off += COLLATERAL_LEN;
        }
        let mut borrows = Vec::with_capacity(borrows_len);
        for _ in 0..borrows_len {
            if data.len() < off + LIQUIDITY_LEN { break; }
            let rec = &data[off..off+LIQUIDITY_LEN];
            let borrow_reserve = pk(off);
            // skip cumulative_borrow_rate_wads (16 bytes) at rec[32..48]
            let borrowed_wads  = decimal_to_f64(&rec[48..64]);
            let market_value   = decimal_to_f64(&rec[64..80]);
            borrows.push(ObligationLiquidity { borrow_reserve, borrowed_amount_wads: borrowed_wads, market_value_usd: market_value });
            off += LIQUIDITY_LEN;
        }
        Ok(Self {
            address, lending_market, owner,
            deposited_value_usd: deposited_value,
            borrowed_value_usd:  borrowed_value,
            allowed_borrow_value: allowed_borrow,
            unhealthy_borrow_value: unhealthy_borrow,
            deposits, borrows,
        })
    }

    /// Returns the ratio borrowed/unhealthy. ≥1.0 = liquidatable. Higher = more underwater.
    pub fn liq_ratio(&self) -> f64 {
        if self.unhealthy_borrow_value < 0.000001 { return 0.0; }
        self.borrowed_value_usd / self.unhealthy_borrow_value
    }

    /// Phase 3d: LIVE liquidation ratio computed by re-pricing each deposit
    /// and borrow with the latest Pyth price (overlay on stored amounts).
    /// Solend's on-chain `market_value_usd` only updates when an explicit
    /// `RefreshObligation` IX runs against the obligation — without that,
    /// it stays at the last-refresh price. By scaling each stored value by
    /// `live_price / stored_price` we get the ratio Solend's IX would compute
    /// if it ran right now, exposing price-driven crossings that the stale
    /// `liq_ratio()` misses.
    ///
    /// Falls back to `liq_ratio()` for any reserve/feed we can't price live.
    pub fn live_ratio(
        &self,
        reserves: &ReserveCache,
        pyth: &crate::pyth_store::PythStore,
    ) -> f64 {
        let mut live_unhealthy: f64 = 0.0;
        let mut live_borrowed:  f64 = 0.0;
        for d in &self.deposits {
            let Some(r) = reserves.get(&d.deposit_reserve) else { return self.liq_ratio(); };
            let stored_price = r.market_price_usd.max(0.000001);
            let live_price = pyth.get(&r.pyth_oracle)
                .and_then(|bytes| arb_dex_obric::parse_pyth_price_any(&bytes))
                .filter(|p| p.trading)
                .map(|p| (p.price as f64) * 10f64.powi(p.exponent))
                .unwrap_or(stored_price);
            let scale = live_price / stored_price;
            let live_val = d.market_value_usd * scale;
            // liquidation_threshold is u8 percentage (typical 85 = 85%).
            live_unhealthy += live_val * (r.liquidation_threshold as f64 / 100.0);
        }
        for b in &self.borrows {
            let Some(r) = reserves.get(&b.borrow_reserve) else { return self.liq_ratio(); };
            let stored_price = r.market_price_usd.max(0.000001);
            let live_price = pyth.get(&r.pyth_oracle)
                .and_then(|bytes| arb_dex_obric::parse_pyth_price_any(&bytes))
                .filter(|p| p.trading)
                .map(|p| (p.price as f64) * 10f64.powi(p.exponent))
                .unwrap_or(stored_price);
            let scale = live_price / stored_price;
            live_borrowed += b.market_value_usd * scale;
        }
        if live_unhealthy < 0.000001 { return 0.0; }
        live_borrowed / live_unhealthy
    }

    /// Returns true if the obligation is currently liquidatable per on-chain state.
    pub fn is_liquidatable(&self) -> bool {
        self.liq_ratio() >= 1.0 && self.borrowed_value_usd > 0.01
    }
    /// Sanity gate: skips abandoned/un-refreshed obligations whose on-chain
    /// state is so stale that the values are meaningless. A real Solend
    /// position keeps borrowed/unhealthy values fresh via the Refresh ix.
    pub fn is_sane(&self) -> bool {
        self.unhealthy_borrow_value > 0.01
            && self.borrowed_value_usd < 10_000_000.0
            && self.deposited_value_usd > 0.01
            && self.deposited_value_usd < 100_000_000.0
    }
}

// ── RPC helpers ─────────────────────────────────────────────────────────────
async fn http_post(url: &str, body: &serde_json::Value) -> ArbResult<serde_json::Value> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| ArbError::Rpc(format!("solend: http build: {e}")))?;
    let r = client.post(url).json(body).send().await
        .map_err(|e| ArbError::Rpc(format!("solend: post: {e}")))?;
    let v: serde_json::Value = r.json().await
        .map_err(|e| ArbError::Rpc(format!("solend: json: {e}")))?;
    if let Some(err) = v.get("error") {
        return Err(ArbError::Rpc(format!("solend rpc err: {err}")));
    }
    Ok(v)
}

pub async fn fetch_reserve(rpc: &str, reserve: Pubkey) -> ArbResult<ReserveSummary> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [reserve.to_string(), {"encoding": "base64", "commitment": "confirmed"}]
    });
    let v = http_post(rpc, &body).await?;
    let data_b64 = v["result"]["value"]["data"][0].as_str()
        .ok_or(ArbError::Invariant("solend: missing reserve data"))?;
    let data = base64::engine::general_purpose::STANDARD.decode(data_b64)
        .map_err(|e| ArbError::Rpc(format!("solend: base64: {e}")))?;
    ReserveSummary::parse(reserve, &data)
}

/// Header-only obligation summary. No deposits/borrows lists — used by the
/// scanner to rank by health quickly (sub-MB payload).
#[derive(Debug, Clone)]
pub struct ObligationHeader {
    pub address:                Pubkey,
    pub lending_market:         Pubkey,
    pub owner:                  Pubkey,
    pub deposited_value_usd:    f64,
    pub borrowed_value_usd:     f64,
    pub allowed_borrow_value:   f64,
    pub unhealthy_borrow_value: f64,
}

impl ObligationHeader {
    pub fn liq_ratio(&self) -> f64 {
        if self.unhealthy_borrow_value < 0.000001 { return 0.0; }
        self.borrowed_value_usd / self.unhealthy_borrow_value
    }
    pub fn is_liquidatable(&self) -> bool {
        self.liq_ratio() >= 1.0 && self.borrowed_value_usd > 0.01
    }
    /// Sanity gate: skips abandoned obligations where the protocol already
    /// has bad debt (borrowed > deposited × 1.05 means there's nothing left
    /// to seize). Real liquidation opportunities have borrowed slightly above
    /// the unhealthy threshold AND collateral still backing it.
    pub fn is_sane(&self) -> bool {
        self.unhealthy_borrow_value > 0.01
            && self.borrowed_value_usd > 0.01
            && self.borrowed_value_usd < 10_000_000.0
            && self.deposited_value_usd > 0.01
            && self.deposited_value_usd < 100_000_000.0
            // borrowed/deposited > 1.05 = collateral already exhausted
            && self.borrowed_value_usd < self.deposited_value_usd * 1.05
    }
    /// Realistic bonus estimate: capped by both 20% repayment limit AND
    /// available collateral. Bonus assumed at 5% (Solend typical).
    pub fn est_bonus_usd(&self) -> f64 {
        let max_repay = self.borrowed_value_usd * 0.20;
        let max_seize = max_repay.min(self.deposited_value_usd);
        max_seize * 0.05
    }
}

/// Fetch all obligation HEADERS for a SPECIFIC lending market. Use
/// `fetch_all_obligations_multi` to query several markets in one call.
pub async fn fetch_all_obligations_for_market(rpc: &str, market: Pubkey) -> ArbResult<Vec<ObligationHeader>> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "getProgramAccounts",
        "params": [
            SOLEND_PROGRAM.to_string(),
            {
                "encoding": "base64",
                "commitment": "confirmed",
                "dataSlice": {"offset": 0, "length": 140},
                "filters": [
                    {"dataSize": 1300},
                    {"memcmp": {"offset": 10, "bytes": market.to_string()}}
                ]
            }
        ]
    });
    let v = http_post(rpc, &body).await?;
    let arr = v["result"].as_array()
        .ok_or(ArbError::Invariant("solend: missing result array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for item in arr {
        let addr_str = item["pubkey"].as_str().unwrap_or("");
        let data_b64 = item["account"]["data"][0].as_str().unwrap_or("");
        let Ok(addr) = Pubkey::from_str(addr_str) else { continue };
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else { continue };
        if data.len() < 138 { continue; }
        let pk = |off: usize| Pubkey::new_from_array(data[off..off+32].try_into().unwrap());
        let lending_market   = pk(10);
        let owner            = pk(42);
        let deposited_value  = decimal_to_f64(&data[74..90]);
        let borrowed_value   = decimal_to_f64(&data[90..106]);
        let allowed_borrow   = decimal_to_f64(&data[106..122]);
        let unhealthy_borrow = decimal_to_f64(&data[122..138]);
        if borrowed_value > 0.01 {
            out.push(ObligationHeader {
                address: addr, lending_market, owner,
                deposited_value_usd: deposited_value,
                borrowed_value_usd:  borrowed_value,
                allowed_borrow_value: allowed_borrow,
                unhealthy_borrow_value: unhealthy_borrow,
            });
        }
    }
    Ok(out)
}

/// Fetch obligations across multiple markets and combine. Markets are queried
/// sequentially (Helius getProgramAccounts is heavy — parallel would risk
/// rate limits). Returns deduped list (keyed by obligation address).
pub async fn fetch_all_obligations_multi(rpc: &str, markets: &[Pubkey]) -> ArbResult<Vec<ObligationHeader>> {
    let mut out: Vec<ObligationHeader> = Vec::new();
    for m in markets {
        match fetch_all_obligations_for_market(rpc, *m).await {
            Ok(mut v) => out.append(&mut v),
            Err(e) => tracing::warn!(target: "solend_liq",
                market = %m, error = %e, "skipping market — fetch failed"),
        }
    }
    Ok(out)
}

/// Back-compat shim: scan main market only.
pub async fn fetch_all_obligations(rpc: &str) -> ArbResult<Vec<ObligationHeader>> {
    fetch_all_obligations_for_market(rpc, SOLEND_MAIN_MARKET).await
}

/// Fetch full obligation (1300 bytes) for one specific address — used to
/// inspect deposits/borrows of a single candidate before liquidation.
/// Phase 3d: batch-fetch full obligation data for a SPECIFIC list of
/// addresses. Used by the scan loop to upgrade its "close" set from
/// header-only to full breakdown so we can compute live_ratio with Pyth.
///
/// Splits into chunks of 100 because that's the RPC's getMultipleAccounts
/// hard limit. Missing/un-parsable accounts are silently dropped.
pub async fn fetch_obligations_full_batch(
    rpc:       &str,
    addresses: &[Pubkey],
) -> ArbResult<Vec<ObligationSummary>> {
    let mut out: Vec<ObligationSummary> = Vec::with_capacity(addresses.len());
    for chunk in addresses.chunks(100) {
        let keys_json: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getMultipleAccounts",
            "params": [keys_json, {"encoding": "base64", "commitment": "confirmed"}]
        });
        let v = http_post(rpc, &body).await?;
        let arr = match v["result"]["value"].as_array() {
            Some(a) => a,
            None => continue,
        };
        for (item, addr) in arr.iter().zip(chunk.iter()) {
            let data_b64 = item.pointer("/data/0").and_then(|x| x.as_str()).unwrap_or("");
            if data_b64.is_empty() { continue; }
            let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else { continue };
            if let Ok(obli) = ObligationSummary::parse(*addr, &data) {
                out.push(obli);
            }
        }
    }
    Ok(out)
}

pub async fn fetch_obligation_full(rpc: &str, obligation: Pubkey) -> ArbResult<ObligationSummary> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [obligation.to_string(), {"encoding": "base64", "commitment": "confirmed"}]
    });
    let v = http_post(rpc, &body).await?;
    let data_b64 = v["result"]["value"]["data"][0].as_str()
        .ok_or(ArbError::Invariant("solend: missing obligation data"))?;
    let data = base64::engine::general_purpose::STANDARD.decode(data_b64)
        .map_err(|e| ArbError::Rpc(format!("solend: base64: {e}")))?;
    ObligationSummary::parse(obligation, &data)
}

// ── Liquidate IX ────────────────────────────────────────────────────────────
//
// Solend's instruction 12 = LiquidateObligationAndRedeemReserveCollateral.
// Layout (1 byte tag + 8 bytes amount):
//   data = [12u8, liquidity_amount: u64 LE]
//
// Accounts (per Solend v0.5):
//   [W]  0: source_liquidity (liquidator's debt-token ATA — we pay from here)
//   [W]  1: destination_collateral (liquidator's collateral-token ATA — we receive c-tokens here, then redeem)
//   [W]  2: destination_liquidity (liquidator's collateral-LIQUIDITY ATA — we receive the underlying token here after redeem)
//   [W]  3: repay_reserve
//   [W]  4: repay_reserve_liquidity_supply
//   [W]  5: withdraw_reserve
//   [W]  6: withdraw_reserve_collateral_mint
//   [W]  7: withdraw_reserve_collateral_supply
//   [W]  8: withdraw_reserve_liquidity_supply
//   [W]  9: withdraw_reserve_liquidity_fee_receiver
//   [W] 10: obligation
//   [-] 11: lending_market
//   [-] 12: lending_market_authority
//   [s] 13: user_transfer_authority (signer)
//   [-] 14: clock sysvar
//   [-] 15: token_program (SPL Token classic)
pub struct LiquidateAccounts {
    pub source_liquidity:                Pubkey, // liquidator's debt ATA
    pub destination_collateral:          Pubkey, // liquidator's c-token ATA
    pub destination_liquidity:           Pubkey, // liquidator's collateral-underlying ATA
    pub repay_reserve:                   Pubkey,
    pub repay_reserve_liquidity_supply:  Pubkey,
    pub withdraw_reserve:                Pubkey,
    pub withdraw_reserve_collateral_mint: Pubkey,
    pub withdraw_reserve_collateral_supply: Pubkey,
    pub withdraw_reserve_liquidity_supply: Pubkey,
    pub withdraw_reserve_liquidity_fee_receiver: Pubkey,
    pub obligation:                      Pubkey,
    pub lending_market:                  Pubkey,
    pub lending_market_authority:        Pubkey,
    pub user_transfer_authority:         Pubkey, // signer
}

pub fn build_liquidate_ix(accs: &LiquidateAccounts, repay_amount: u64) -> Instruction {
    let token_program = solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
    let mut data = Vec::with_capacity(9);
    // Save Finance's NEW discriminator for `LiquidateWithoutReceivingCtokens`.
    // The old 12 = `LiquidateObligationAndRedeemReserveCollateral` is now
    // routed internally to the deprecated `Liquidate Obligation` handler.
    // Verified by decoding the data bytes of successful on-chain liq txs.
    data.push(17u8); // instruction discriminator
    data.extend_from_slice(&repay_amount.to_le_bytes());
    // Save Finance / Solend production: NO Clock account in Liquidate IX.
    // Matches the on-chain CPI from successful liq tx (15 accounts).
    Instruction {
        program_id: SOLEND_PROGRAM,
        accounts: vec![
            AccountMeta::new(accs.source_liquidity, false),
            AccountMeta::new(accs.destination_collateral, false),
            AccountMeta::new(accs.destination_liquidity, false),
            AccountMeta::new(accs.repay_reserve, false),
            AccountMeta::new(accs.repay_reserve_liquidity_supply, false),
            AccountMeta::new(accs.withdraw_reserve, false),
            AccountMeta::new(accs.withdraw_reserve_collateral_mint, false),
            AccountMeta::new(accs.withdraw_reserve_collateral_supply, false),
            AccountMeta::new(accs.withdraw_reserve_liquidity_supply, false),
            AccountMeta::new(accs.withdraw_reserve_liquidity_fee_receiver, false),
            AccountMeta::new(accs.obligation, false),
            // Save Finance routes to "LiquidateWithoutReceivingCtokens" handler
            // when lending_market is WRITABLE. Readonly = old deprecated path.
            AccountMeta::new(accs.lending_market, false),
            AccountMeta::new_readonly(accs.lending_market_authority, false),
            AccountMeta::new(accs.user_transfer_authority, true),
            AccountMeta::new_readonly(token_program, false),
        ],
        data,
    }
}

/// Solend instruction 7 = RefreshObligation. The deployed Save Finance program
/// does NOT take Clock as an account — clock is read via Sysvar API internally.
/// Accounts (verified against live tx 2JwniTiY...):
///   [W] 0: obligation
///   [-] 1..n: each deposit_reserve, each borrow_reserve (readonly)
pub fn build_refresh_obligation_ix(
    obligation: Pubkey,
    deposit_reserves: &[Pubkey],
    borrow_reserves: &[Pubkey],
) -> Instruction {
    let mut accounts = vec![AccountMeta::new(obligation, false)];
    for r in deposit_reserves { accounts.push(AccountMeta::new_readonly(*r, false)); }
    for r in borrow_reserves  { accounts.push(AccountMeta::new_readonly(*r, false)); }
    Instruction {
        program_id: SOLEND_PROGRAM,
        accounts,
        data: vec![7u8],
    }
}

/// Solend instruction 3 = RefreshReserve. The deployed program takes 3
/// accounts (no Clock — read internally via Sysvar API).
/// Accounts (verified against live tx 2JwniTiY...):
///   [W] 0: reserve
///   [-] 1: pyth_oracle
///   [-] 2: switchboard_oracle (use null sentinel if not configured)
pub fn build_refresh_reserve_ix(
    reserve: Pubkey,
    pyth_oracle: Pubkey,
    switchboard_oracle: Pubkey,
) -> Instruction {
    Instruction {
        program_id: SOLEND_PROGRAM,
        accounts: vec![
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(pyth_oracle, false),
            AccountMeta::new_readonly(switchboard_oracle, false),
        ],
        data: vec![3u8],
    }
}

// ── Liquidation builder ─────────────────────────────────────────────────────

/// Select the best (deposit, borrow) pair from a candidate obligation:
///   - borrow: largest USD value (if `our_debt_mint` is `Some`, restricted
///     to that mint; if `None`, *any* mint — caller is responsible for
///     swapping flash-loan USDC → debt_mint before the liquidate ix)
///   - deposit: largest USD value with a different mint than `borrow`
///     (Save Finance rejects same-mint with custom error 0x2a)
///
/// Returns `None` if no compatible pair found.
pub fn select_liquidation_target(
    obligation: &ObligationSummary,
    reserves: &ReserveCache,
    our_debt_mint: Option<Pubkey>,
) -> Option<(ReserveSummary, ReserveSummary)> {
    // Find biggest borrow (restricted to `our_debt_mint` if specified).
    let mut best_borrow: Option<&ReserveSummary> = None;
    let mut best_borrow_value = 0.0;
    for b in &obligation.borrows {
        let Some(r) = reserves.get(&b.borrow_reserve) else { continue };
        if let Some(m) = our_debt_mint {
            if r.liquidity_mint != m { continue; }
        }
        if b.market_value_usd > best_borrow_value {
            best_borrow_value = b.market_value_usd;
            best_borrow = Some(r);
        }
    }
    let borrow_r = best_borrow?.clone();

    // Find biggest deposit whose mint DIFFERS from the borrow's mint.
    // Save Finance rejects liquidations where borrow_mint == collateral_mint
    // (custom program error 0x2a) — seizing USDC to repay USDC is net-zero.
    let mut best_deposit: Option<&ReserveSummary> = None;
    let mut best_deposit_value = 0.0;
    for d in &obligation.deposits {
        let Some(r) = reserves.get(&d.deposit_reserve) else { continue };
        if r.liquidity_mint == borrow_r.liquidity_mint { continue; }
        if d.market_value_usd > best_deposit_value {
            best_deposit_value = d.market_value_usd;
            best_deposit = Some(r);
        }
    }
    let deposit_r = best_deposit?.clone();

    Some((deposit_r, borrow_r))
}

/// Solend lending market authority is a PDA: seeds = [lending_market], program = Solend.
pub fn derive_lending_market_authority(lending_market: &Pubkey) -> Pubkey {
    let (pda, _) = Pubkey::find_program_address(&[lending_market.as_ref()], &SOLEND_PROGRAM);
    pda
}

/// Build the full liquidation instruction sequence. The Refresh Obligation
/// instruction requires the reserves IN THE ORDER they appear in the
/// obligation's deposits[] and borrows[] arrays — passing only the two
/// reserves we're liquidating fails with "Deposit reserve provided for
/// collateral N is not owned by the lending program".
///
///   [refresh_reserve(d_0)] ... [refresh_reserve(d_K)]
///   [refresh_reserve(b_0)] ... [refresh_reserve(b_L)]
///   [refresh_obligation(d_0..K, b_0..L)]
///   [liquidate_obligation_and_redeem_reserve_collateral(chosen pair)]
///
/// Caller must prepend compute budget IXs and ATA creates.
pub fn build_liquidation_sequence_full(
    obligation:        Pubkey,
    obli:              &ObligationSummary,
    reserves:          &ReserveCache,
    deposit_reserve:   &ReserveSummary,
    borrow_reserve:    &ReserveSummary,
    payer:             Pubkey,
    repay_amount:      u64,
) -> Vec<Instruction> {
    let lending_market = borrow_reserve.lending_market;
    let lending_market_authority = derive_lending_market_authority(&lending_market);
    let source_liquidity        = crate::raw_tx::derive_ata(&payer, &borrow_reserve.liquidity_mint);
    let destination_collateral  = crate::raw_tx::derive_ata(&payer, &deposit_reserve.collateral_mint);
    let destination_liquidity   = crate::raw_tx::derive_ata(&payer, &deposit_reserve.liquidity_mint);

    let mut ixs = Vec::with_capacity(4 + obli.deposits.len() + obli.borrows.len());

    // Refresh every deposit reserve in order
    let mut deposit_reserves_ordered = Vec::with_capacity(obli.deposits.len());
    for d in &obli.deposits {
        deposit_reserves_ordered.push(d.deposit_reserve);
        if let Some(r) = reserves.get(&d.deposit_reserve) {
            ixs.push(build_refresh_reserve_ix(
                r.address, r.pyth_oracle, r.switchboard_oracle,
            ));
        }
    }
    // Refresh every borrow reserve in order
    let mut borrow_reserves_ordered = Vec::with_capacity(obli.borrows.len());
    for b in &obli.borrows {
        borrow_reserves_ordered.push(b.borrow_reserve);
        if let Some(r) = reserves.get(&b.borrow_reserve) {
            ixs.push(build_refresh_reserve_ix(
                r.address, r.pyth_oracle, r.switchboard_oracle,
            ));
        }
    }
    // Refresh obligation with ALL reserves in correct order
    ixs.push(build_refresh_obligation_ix(
        obligation,
        &deposit_reserves_ordered,
        &borrow_reserves_ordered,
    ));
    // Liquidate + redeem (targets the chosen pair specifically)
    let accs = LiquidateAccounts {
        source_liquidity,
        destination_collateral,
        destination_liquidity,
        repay_reserve:                          borrow_reserve.address,
        repay_reserve_liquidity_supply:         borrow_reserve.liquidity_supply,
        withdraw_reserve:                       deposit_reserve.address,
        withdraw_reserve_collateral_mint:       deposit_reserve.collateral_mint,
        withdraw_reserve_collateral_supply:     deposit_reserve.collateral_supply,
        withdraw_reserve_liquidity_supply:      deposit_reserve.liquidity_supply,
        withdraw_reserve_liquidity_fee_receiver: deposit_reserve.fee_receiver,
        obligation,
        lending_market,
        lending_market_authority,
        user_transfer_authority: payer,
    };
    ixs.push(build_liquidate_ix(&accs, repay_amount));
    ixs
}

// ── Reserve cache ───────────────────────────────────────────────────────────
/// Maps reserve_pubkey → parsed summary. Built once at startup, refreshed
/// occasionally. The liquidator worker uses this to lookup oracle/mint info
/// for each obligation's reserves.
pub type ReserveCache = Arc<HashMap<Pubkey, ReserveSummary>>;

/// Bootstrap reserves for a SPECIFIC market.
pub async fn bootstrap_reserve_cache_for_market(rpc: &str, market: Pubkey) -> ArbResult<HashMap<Pubkey, ReserveSummary>> {
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "getProgramAccounts",
        "params": [
            SOLEND_PROGRAM.to_string(),
            {
                "encoding": "base64",
                "commitment": "confirmed",
                "filters": [
                    {"dataSize": 619},
                    {"memcmp": {"offset": 10, "bytes": market.to_string()}}
                ]
            }
        ]
    });
    let v = http_post(rpc, &body).await?;
    let arr = v["result"].as_array()
        .ok_or(ArbError::Invariant("solend: missing reserves array"))?;
    let mut map = HashMap::with_capacity(arr.len());
    for item in arr {
        let addr_str = item["pubkey"].as_str().unwrap_or("");
        let data_b64 = item["account"]["data"][0].as_str().unwrap_or("");
        let Ok(addr) = Pubkey::from_str(addr_str) else { continue };
        let Ok(data) = base64::engine::general_purpose::STANDARD.decode(data_b64) else { continue };
        if let Ok(r) = ReserveSummary::parse(addr, &data) {
            map.insert(addr, r);
        }
    }
    Ok(map)
}

/// Bootstrap reserves from multiple markets, combining into a unified cache.
pub async fn bootstrap_reserve_cache_multi(rpc: &str, markets: &[Pubkey]) -> ArbResult<ReserveCache> {
    let mut combined: HashMap<Pubkey, ReserveSummary> = HashMap::new();
    for m in markets {
        match bootstrap_reserve_cache_for_market(rpc, *m).await {
            Ok(map) => {
                for (k, v) in map { combined.insert(k, v); }
            }
            Err(e) => tracing::warn!(target: "solend_liq",
                market = %m, error = %e, "skipping market reserves"),
        }
    }
    info!(target: "solend_liq",
        reserves = combined.len(), markets = markets.len(),
        "✓ multi-market reserve cache bootstrapped");
    Ok(Arc::new(combined))
}

/// Back-compat shim: main market only.
pub async fn bootstrap_reserve_cache(rpc: &str) -> ArbResult<ReserveCache> {
    let map = bootstrap_reserve_cache_for_market(rpc, SOLEND_MAIN_MARKET).await?;
    info!(target: "solend_liq", reserves = map.len(), "✓ reserve cache bootstrapped");
    Ok(Arc::new(map))
}
