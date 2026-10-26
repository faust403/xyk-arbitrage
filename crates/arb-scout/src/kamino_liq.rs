//! Kamino Lend liquidation scout: reserve + obligation discovery,
//! refresh-IX assembly, sim-as-filter candidate finder.
//!
//! ## Why this lives next to `marginfi_liq` and `solend_liq`
//!
//! Same shape as the MarginFi hunter: `getProgramAccounts` for the
//! universe → narrow to candidates with debt → simulate each → fire on
//! the ones the program reports as truly underwater. The protocol-specific
//! pieces are:
//!
//! - Reserve/Obligation are Anchor `#[account(zero_copy)] #[repr(C)]`
//!   structs with nested padded sub-types (BigFractionBytes, TokenInfo,
//!   WithdrawalCaps, etc). Per the source-of-truth audit (2026-05-15)
//!   the safe path is `bytemuck::from_bytes::<Reserve>(&data[8..])` —
//!   but that requires importing/reproducing every nested struct with
//!   `Pod+Zeroable`. We hand-extract only the small subset of fields
//!   the liquidator needs, using **offsets the Anchor source confirms
//!   are stable across versions** (mints + market + owner + a few
//!   liquidation-bonus bytes). The rest is left to bytemuck-via-the-real-
//!   struct in a follow-up commit.
//!
//! - Liquidate requires a `refresh_reserve` per obligation-referenced
//!   reserve + a `refresh_obligation` before the `liquidate` ix itself.
//!   IX builders live in `arb-flashloan/src/kamino.rs`.
//!
//! - Use `simulateTransaction` + parse `ObligationHealthy` (or absence
//!   thereof) from program logs as the truth filter — same pattern we
//!   use for MarginFi via `parse_pre_liquidation_health`.

use std::collections::HashMap;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::pubkey::Pubkey;

// ─── On-chain constants ─────────────────────────────────────────

pub const KAMINO_LEND_PROGRAM_STR: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";

/// Kamino Reserve account size on-chain (8 disc + 8616 struct).
pub const KAMINO_RESERVE_SIZE:    usize = 8624;
/// Kamino Obligation account size on-chain (8 disc + 3336 struct).
/// Verified 2026-05-15 against `static_assertions::const_assert_eq!`
/// in `utils/consts.rs::OBLIGATION_SIZE`.
pub const KAMINO_OBLIGATION_SIZE: usize = 3344;

// On-chain offsets in `Obligation` (data starts at +8 after Anchor disc):
//   8       tag: u64
//   16..32  last_update (16 B)
//   32..64  lending_market: Pubkey
//   64..96  owner: Pubkey
//   96..1184 deposits[8] (ObligationCollateral × 136)
//   1184..1192 lowest_reserve_deposit_liquidation_ltv: u64
//   1192..1208 deposited_value_sf: u128
//   1208..2208 borrows[5] (ObligationLiquidity × 200)
//   2208..2224 borrow_factor_adjusted_debt_value_sf: u128
//   2224..2240 borrowed_assets_market_value_sf: u128
//   2240..2256 allowed_borrow_value_sf: u128
//   2256..2272 unhealthy_borrow_value_sf: u128
//   ... padding/asset_tiers/flags ...
//   2287     has_debt: u8
//
// Liquidatable predicate (off-chain mirror of program check):
//   borrow_factor_adjusted_debt_value_sf >= unhealthy_borrow_value_sf
//   && deposited_value_sf > 0
//   && has_debt == 1
//
// All *_sf values share the SF=2^60 scale, so the >= compare doesn't need
// scaling. Verified 2026-05-15 against Kamino-Finance/klend master.
/// `last_update.slot` field — first 8 bytes of LastUpdate at offset 16
/// (after `tag: u64` at 8..16). LastUpdate layout: slot(u64), stale(u8), padding.
const OBL_OFF_LAST_UPDATE_SLOT: usize = 16;
const OBL_OFF_LENDING_MARKET: usize = 32;
const OBL_OFF_OWNER:          usize = 64;
const OBL_OFF_DEPOSITS_START: usize = 96;
const OBL_DEPOSIT_STRIDE:     usize = 136;
const OBL_MAX_DEPOSITS:       usize = 8;
const OBL_OFF_DEPOSITED_VALUE_SF: usize = 1192;
const OBL_OFF_BORROWS_START:  usize = 1208;  // corrected — was 1208, now confirmed
const OBL_BORROW_STRIDE:      usize = 200;
const OBL_MAX_BORROWS:        usize = 5;
const OBL_OFF_BORROW_FACTOR_ADJ_DEBT_SF: usize = 2208;
/// `borrowed_assets_market_value_sf` — raw debt USD value BEFORE applying
/// borrow_factor. Used together with `borrow_factor_adjusted_debt_value_sf`
/// to derive the obligation's effective borrow_factor ratio.
const OBL_OFF_BORROWED_ASSETS_MARKET_VALUE_SF: usize = 2224;
const OBL_OFF_UNHEALTHY_BORROW_VALUE_SF: usize = 2256;
const OBL_OFF_HAS_DEBT: usize = 2287;

// Per-balance: ObligationCollateral starts with deposit_reserve(32) then
// deposited_amount(u64) then market_value_sf(u128).
// ObligationLiquidity starts with borrow_reserve(32) then
// cumulative_borrow_rate_bsf(48) then last_borrowed_at_timestamp(u64) then
// borrowed_amount_sf(u128).
const BAL_DEP_OFF_RESERVE: usize = 0;
const BAL_DEP_OFF_AMOUNT:  usize = 32;  // u64
const BAL_BOR_OFF_RESERVE: usize = 0;
const BAL_BOR_OFF_AMOUNT_SF: usize = 32 + 48 + 8;  // u128 borrowed_amount_sf @ +88

// Reserve offsets verified against the Agent audit (payload offsets):
//   0     version: u64
//   8..24 last_update
//   24..56 lending_market
//   56..88 farm_collateral
//   88..120 farm_debt
//   120..152 liquidity.mint_pubkey
//   152..184 liquidity.supply_vault
//   184..216 liquidity.fee_vault
//   216..224 liquidity.total_available_amount: u64
//   224..240 liquidity.borrowed_amount_sf: u128
//   240..256 liquidity.market_price_sf: u128
//   256..264 liquidity.market_price_last_updated_ts: u64
//   264..272 liquidity.mint_decimals: u64
// On-chain offsets = payload + 8.
const RES_OFF_LENDING_MARKET:        usize = 8 + 24;
const RES_OFF_FARM_COLLATERAL:       usize = 8 + 56;
const RES_OFF_FARM_DEBT:             usize = 8 + 88;
const RES_OFF_LIQ_MINT:              usize = 8 + 120;
const RES_OFF_LIQ_SUPPLY_VAULT:      usize = 8 + 152;
const RES_OFF_LIQ_FEE_VAULT:         usize = 8 + 184;
const RES_OFF_LIQ_AVAILABLE:         usize = 8 + 216;
const RES_OFF_LIQ_BORROWED_SF:       usize = 8 + 224;
const RES_OFF_LIQ_MARKET_PRICE_SF:   usize = 8 + 240;
const RES_OFF_LIQ_PRICE_TS:          usize = 8 + 256;
const RES_OFF_LIQ_MINT_DECIMALS:     usize = 8 + 264;

// ReserveCollateral + ReserveConfig.TokenInfo offsets — VERIFIED 2026-05-16
// against LIVE on-chain bytes from Kamino USDC main-market reserve
// (D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59) via
// scripts/kamino_reserve_probe.py:
//
//   probe found Scope feed @ on-chain offset 5112 (owner = HFn8GnPA…
//   = Kamino Scope program). Working backward:
//
//   scope_price_feed = TokenInfo_start + 80 = 5112 → TokenInfo_start = 5032
//   token_info_start = config_start + 176 = 5032 → config_start = 4856
//                                                  struct config = 4848
//
//   Plus: collateral.mint at on-chain 2560 (probe-confirmed token Pubkey
//   owner=Tokenkeg) → struct collateral_start = 2552. With
//   liquidity_end at struct 1336 (header+24+96 Pubkeys+1216 liquidity),
//   that puts reserve_liquidity_padding at 1216 bytes (1336..2552), NOT
//   1200 as the earlier source-audit guessed.
//
//   With config_size=944 (asserted): struct end = 4848+944 = 5792, but
//   actual reserve struct is 8616. So 2824 bytes of trailer come after
//   config (likely emergency-mode padding + extension fields). Doesn't
//   affect our reads.
const RES_OFF_COLL_MINT:         usize = 2560;
const RES_OFF_COLL_TOTAL_SUPPLY: usize = 2592;  // = 2560 + 32 (u64)
const RES_OFF_COLL_SUPPLY_VAULT: usize = 2600;  // = 2560 + 40 (Pubkey)

const RES_OFF_CONFIG_START:       usize = 4856;
/// ReserveConfig.loan_to_value_pct (u8). Verified 2026-05-16 against USDC
/// main market reserve (= 80, matches Kamino docs).
const RES_OFF_LTV_PCT:            usize = 4856 + 16;  // = 4872
/// ReserveConfig.liquidation_threshold_pct (u8). Verified 2026-05-16
/// against USDC main market reserve (= 90, matches Kamino docs).
const RES_OFF_LIQ_THRESHOLD_PCT:  usize = 4856 + 17;  // = 4873
/// ReserveConfig.borrow_factor_pct (u64). Verified 2026-05-16 against
/// USDC main market reserve (= 100, i.e. 1.0x).
const RES_OFF_BORROW_FACTOR_PCT:  usize = 4856 + 152; // = 5008
const RES_OFF_TOKEN_INFO_START:   usize = 5032;  // = 4856 + 176
const RES_OFF_SCOPE_PRICE_FEED:   usize = 5112;  // = 5032 + 80
const RES_OFF_SB_PRICE_AGG:       usize = 5160;  // = 5032 + 128
const RES_OFF_SB_TWAP_AGG:        usize = 5192;  // = 5032 + 160
const RES_OFF_PYTH_PRICE:         usize = 5224;  // = 5032 + 192
/// `TokenInfo.scope_chain: [u16; 4]` — per-reserve indices into the
/// Scope `OraclePrices.prices[]` array. The first non-65535 entry is the
/// primary feed; subsequent entries form a composite price (e.g., kSOL =
/// kPrice × SOL). Single-token chains (chain=[id, 65535, 65535, 65535])
/// cover virtually all main-market reserves and are what `fresh_scope_*`
/// resolves today. Verified live 2026-05-16: USDC chain=[13,_,_,_],
/// wSOL chain=[3,_,_,_].
const RES_OFF_SCOPE_CHAIN:        usize = 5032 + 112;  // = 5144
const SCOPE_CHAIN_SENTINEL: u16 = u16::MAX;

/// I80F48 scale factor used by Kamino for `*_sf` fields.
const SF_SCALE: u128 = 1u128 << 60;

// ─── Reserve ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ReserveSummary {
    pub address:                 Pubkey,
    pub lending_market:          Pubkey,
    pub farm_collateral:         Pubkey,  // = default if none
    pub farm_debt:               Pubkey,
    pub liquidity_mint:          Pubkey,
    pub liquidity_supply_vault:  Pubkey,
    pub liquidity_fee_vault:     Pubkey,
    /// cToken mint (the synthetic collateral token). Used for
    /// `withdraw_reserve_collateral_mint` slot in the liquidate ix.
    pub collateral_mint:         Pubkey,
    /// cToken supply vault — `withdraw_reserve_collateral_supply` slot.
    pub collateral_supply_vault: Pubkey,
    /// Oracle pubkeys from `config.token_info`. Exactly one is normally
    /// non-default — that's the live price source. Use `active_oracle()`
    /// to pick the right one.
    pub scope_price_feed:        Pubkey,
    pub switchboard_price_agg:   Pubkey,
    pub switchboard_twap_agg:    Pubkey,
    pub pyth_price:              Pubkey,
    /// raw u128 SF (divide by 2^60 for the per-token f64 USD value)
    pub market_price_sf:         u128,
    pub market_price_ts:         u64,
    pub borrowed_amount_sf:      u128,
    /// `liquidity.available_amount` — raw u64 in token base units. Combined
    /// with `borrowed_amount_sf` gives the reserve's total liquidity, which
    /// divided by `collateral_total_supply` yields the cToken→liquidity
    /// exchange rate (compounds upward as interest accrues).
    pub total_available:         u64,
    /// `collateral.total_supply` — total cTokens minted for this reserve.
    /// Needed to convert obligation `deposited_amount` (cToken units) to
    /// underlying liquidity units in fresh-LTV math.
    pub collateral_total_supply: u64,
    pub mint_decimals:           u8,
    /// `config.loan_to_value_pct` — u8 percent (e.g. 80 = 80%). Used as
    /// the deposit weight in the FRESH-recompute path so we don't depend
    /// on the obligation's stale `deposited_value_sf`.
    /// `TokenInfo.scope_chain: [u16; 4]`. Indices into the Scope
    /// `OraclePrices.prices[]` array, in order. The chain is sentinel-
    /// terminated by `0xFFFF` (== `SCOPE_CHAIN_SENTINEL`). Single-entry
    /// chains identify the reserve's price directly; multi-entry chains
    /// multiply prices together for composite assets.
    pub scope_chain:                 [u16; 4],
    pub loan_to_value_pct:           u8,
    /// `config.liquidation_threshold_pct` — u8 percent (e.g. 90 = 90%).
    /// This is the threshold above which Kamino's check fails with
    /// ObligationHealthy. Fresh-recompute multiplies deposit market
    /// values by this to get the on-chain `unhealthy_borrow_value`.
    pub liquidation_threshold_pct:   u8,
    /// `config.borrow_factor_pct` — u64 percent, scaled by 100 (e.g.
    /// 100 = 1.0x, 150 = 1.5x for volatile assets). Fresh-recompute
    /// multiplies borrow market values by this to get the on-chain
    /// `borrow_factor_adjusted_debt_value`.
    pub borrow_factor_pct:           u64,
}

/// Which oracle source a reserve uses. None of the three is the active
/// one means the reserve is misconfigured (or uses a setup we don't yet
/// support, e.g. composite/staked).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KaminoOracleKind {
    Pyth,
    Switchboard,
    Scope,
    None,
}

impl ReserveSummary {
    /// Price as f64 dollars per token. Note: this is "smallest-unit price"
    /// scaled — the price.sf value is already in "per smallest-mint-unit"
    /// terms, so divide by 10^decimals AFTER multiplying by token raw amount.
    pub fn market_price_usd_per_token(&self) -> f64 {
        let p = (self.market_price_sf as f64) / (SF_SCALE as f64);
        p * 10f64.powi(self.mint_decimals as i32)
    }
    pub fn has_farm_collateral(&self) -> bool {
        self.farm_collateral != Pubkey::default()
    }
    pub fn has_farm_debt(&self) -> bool {
        self.farm_debt != Pubkey::default()
    }

    /// Convert a cToken (`deposited_amount`) into the underlying liquidity
    /// units it currently represents on this reserve. Kamino's exchange
    /// rate = (available_amount + borrowed_amount) / cToken_supply, and
    /// it climbs over time as borrow interest accrues. Without applying
    /// this, fresh-LTV math treats 1 cToken as 1 unit of underlying and
    /// underestimates deposit values, inflating LTV. None on overflow or
    /// zero supply.
    pub fn ctoken_to_liquidity(&self, ctoken_amount: u64) -> Option<u128> {
        if self.collateral_total_supply == 0 { return None; }
        let total_liquidity = (self.total_available as u128)
            .checked_add(self.borrowed_amount_sf >> 60)?;
        (ctoken_amount as u128)
            .checked_mul(total_liquidity)
            .map(|v| v / self.collateral_total_supply as u128)
    }

    /// Return the live oracle pubkey + which kind it is. None if all
    /// three TokenInfo slots are default (reserve misconfigured).
    pub fn active_oracle(&self) -> (KaminoOracleKind, Pubkey) {
        if self.pyth_price != Pubkey::default() {
            return (KaminoOracleKind::Pyth, self.pyth_price);
        }
        if self.switchboard_price_agg != Pubkey::default() {
            return (KaminoOracleKind::Switchboard, self.switchboard_price_agg);
        }
        if self.scope_price_feed != Pubkey::default() {
            return (KaminoOracleKind::Scope, self.scope_price_feed);
        }
        (KaminoOracleKind::None, Pubkey::default())
    }
}

pub fn parse_reserve(address: Pubkey, bytes: &[u8]) -> Option<ReserveSummary> {
    if bytes.len() < KAMINO_RESERVE_SIZE { return None; }
    Some(ReserveSummary {
        address,
        lending_market:         read_pubkey(bytes, RES_OFF_LENDING_MARKET)?,
        farm_collateral:        read_pubkey(bytes, RES_OFF_FARM_COLLATERAL)?,
        farm_debt:              read_pubkey(bytes, RES_OFF_FARM_DEBT)?,
        liquidity_mint:         read_pubkey(bytes, RES_OFF_LIQ_MINT)?,
        liquidity_supply_vault: read_pubkey(bytes, RES_OFF_LIQ_SUPPLY_VAULT)?,
        liquidity_fee_vault:    read_pubkey(bytes, RES_OFF_LIQ_FEE_VAULT)?,
        collateral_mint:        read_pubkey(bytes, RES_OFF_COLL_MINT)?,
        collateral_supply_vault: read_pubkey(bytes, RES_OFF_COLL_SUPPLY_VAULT)?,
        scope_price_feed:       read_pubkey(bytes, RES_OFF_SCOPE_PRICE_FEED)?,
        switchboard_price_agg:  read_pubkey(bytes, RES_OFF_SB_PRICE_AGG)?,
        switchboard_twap_agg:   read_pubkey(bytes, RES_OFF_SB_TWAP_AGG)?,
        pyth_price:             read_pubkey(bytes, RES_OFF_PYTH_PRICE)?,
        market_price_sf:        read_u128(bytes, RES_OFF_LIQ_MARKET_PRICE_SF)?,
        market_price_ts:        read_u64(bytes, RES_OFF_LIQ_PRICE_TS)?,
        borrowed_amount_sf:     read_u128(bytes, RES_OFF_LIQ_BORROWED_SF)?,
        total_available:        read_u64(bytes, RES_OFF_LIQ_AVAILABLE)?,
        collateral_total_supply: read_u64(bytes, RES_OFF_COLL_TOTAL_SUPPLY)?,
        // mint_decimals stored as u64 — only the low byte matters.
        mint_decimals: bytes.get(RES_OFF_LIQ_MINT_DECIMALS).copied()?,
        // ReserveConfig liquidation factors (Track A: fresh recompute).
        scope_chain: {
            let mut c = [SCOPE_CHAIN_SENTINEL; 4];
            for i in 0..4 {
                c[i] = read_u16(bytes, RES_OFF_SCOPE_CHAIN + i * 2)?;
            }
            c
        },
        loan_to_value_pct:         bytes.get(RES_OFF_LTV_PCT).copied()?,
        liquidation_threshold_pct: bytes.get(RES_OFF_LIQ_THRESHOLD_PCT).copied()?,
        borrow_factor_pct:         read_u64(bytes, RES_OFF_BORROW_FACTOR_PCT)?,
    })
}

// ─── Obligation ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct ObligationDeposit {
    pub deposit_reserve:  Pubkey,
    pub deposited_amount: u64,  // in collateral c-token units
}

#[derive(Debug, Clone, Copy)]
pub struct ObligationBorrow {
    pub borrow_reserve:      Pubkey,
    pub borrowed_amount_sf:  u128,  // I80F48-scaled token amount
}

#[derive(Debug, Clone)]
pub struct ParsedObligation {
    pub address:        Pubkey,
    pub lending_market: Pubkey,
    pub owner:          Pubkey,
    pub deposits:       Vec<ObligationDeposit>,  // only non-default-reserve entries
    pub borrows:        Vec<ObligationBorrow>,   // only non-default-reserve entries
    /// Cached/last-refreshed deposited collateral USD-equivalent (SF=2^60).
    pub deposited_value_sf:                    u128,
    /// Cached/last-refreshed risk-weighted debt value (SF=2^60).
    pub borrow_factor_adjusted_debt_value_sf:  u128,
    /// Cached/last-refreshed raw debt USD value BEFORE borrow_factor (SF=2^60).
    /// `borrow_factor_adjusted_debt_value_sf / borrowed_assets_market_value_sf`
    /// gives the effective composite borrow_factor — used in
    /// `is_liquidatable_fresh_via_cached_ratios` so per-reserve config drift
    /// (elevation-group overrides etc) doesn't matter.
    pub borrowed_assets_market_value_sf:       u128,
    /// Cached/last-refreshed liquidation threshold value (SF=2^60).
    pub unhealthy_borrow_value_sf:             u128,
    /// 1 = obligation has at least one borrow; 0 = no debt. The program
    /// uses this as a fast-reject in liquidate. Matches the on-chain byte.
    pub has_debt_flag:                         u8,
    /// Solana slot at which the obligation was last refreshed on-chain.
    /// `is_liquidatable_per_cache` uses cached values from that point;
    /// large gaps between this slot and current mean cache is stale.
    /// Live evidence 2026-05-15 21:30: obligations with stale snapshots
    /// gave us LTV 0.78 but fresh refresh recomputed 0.59 — 31% drift.
    pub last_update_slot:                      u64,
}

impl ParsedObligation {
    pub fn has_debt(&self) -> bool {
        self.has_debt_flag == 1 && self.borrows.iter().any(|b| b.borrowed_amount_sf > 0)
    }

    /// Off-chain mirror of Kamino's `check_liquidate_obligation`. When
    /// this returns true, the program will accept a liquidate ix; when
    /// false, it reverts with `ObligationHealthy`.
    ///
    /// CAVEAT: the fields used here are CACHED — they only reflect the
    /// state at the most recent `refresh_obligation`. For a fresh
    /// determination, callers must `refresh_reserve` every referenced
    /// reserve + `refresh_obligation` first (or use sim-as-filter).
    pub fn is_liquidatable_per_cache(&self) -> bool {
        self.is_liquidatable_per_cache_with_margin_bps(0)
    }

    /// Same as `is_liquidatable_per_cache` but requires the debt to be
    /// `margin_bps` BASIS POINTS above the unhealthy threshold. Each bp
    /// = 1/10000. Use this to filter borderline candidates that pass our
    /// stale cache but fail the on-chain refresh when Pyth moves between
    /// snapshot and tx — observed live 2026-05-15 21:00:
    ///   our_cache: LTV 0.920 → liquidatable
    ///   on-chain after refresh: LTV 0.9146 → ObligationHealthy (6016) revert
    ///
    /// Recommended margin: 100bps (1%) gives buffer for ~10s of Pyth drift
    /// on stables, ~30s on volatile assets. Set via NATIVE_KAMINO_HEALTH_MARGIN_BPS.
    pub fn is_liquidatable_per_cache_with_margin_bps(&self, margin_bps: u32) -> bool {
        if self.has_debt_flag != 1 || self.deposited_value_sf == 0 {
            return false;
        }
        // threshold = unhealthy * (1 + margin/10000)
        let scaled_threshold = (self.unhealthy_borrow_value_sf as u128)
            .saturating_mul(10_000 + margin_bps as u128)
            / 10_000;
        self.borrow_factor_adjusted_debt_value_sf >= scaled_threshold
    }

    /// Fresh-recompute predicate (Track A): mirrors Kamino's on-chain
    /// `check_liquidate_obligation` using CURRENT prices from the reserve
    /// cache instead of the obligation's frozen `*_value_sf` fields.
    ///
    /// For each deposit: market_value = amount × reserve.market_price_sf
    ///                   contribution = market_value × liq_threshold_pct
    /// For each borrow:  market_value = amount_sf × reserve.market_price_sf
    ///                   contribution = market_value × borrow_factor_pct
    /// Liquidatable iff sum(borrow_contribs) >= sum(deposit_contribs) × (1 + margin/10000)
    ///
    /// `reserve.market_price_sf` is itself "the price Kamino refreshed
    /// most recently for that reserve" — much fresher than the
    /// obligation's snapshot (reserves refresh on any user deposit/
    /// withdraw/borrow/repay; obligations only on owner action).
    ///
    /// Returns None when any referenced reserve is missing from cache;
    /// caller should treat that as "can't decide → skip".
    pub fn is_liquidatable_fresh_with_margin_bps(
        &self,
        cache:      &ReserveCache,
        margin_bps: u32,
    ) -> Option<bool> {
        if self.has_debt_flag != 1 { return Some(false); }

        // SF=2^60 scale, "USD × 2^60" output. Per-leg math:
        //   market_price_sf = USD per WHOLE token × 2^60
        //     (verified live on-chain 2026-05-16: USDC mps=1.15e18 → $1.0
        //      per whole USDC after /2^60; an earlier note said per-smallest
        //      but the on-chain values prove otherwise.)
        //   amount(smallest) × market_price_sf / 10^decimals = USD × 2^60
        //     (decompose: amount/10^dec = whole_tokens; × USD/whole × 2^60 = USD × 2^60)
        let mut deposit_unhealthy_sum: u128 = 0;
        for d in &self.deposits {
            let r = cache.by_address.get(&d.deposit_reserve)?;
            // deposit.deposited_amount is in cTOKEN units — convert to
            // underlying liquidity using the reserve's current exchange rate.
            let amount = r.ctoken_to_liquidity(d.deposited_amount)?;
            let dec_pow = 10u128.checked_pow(r.mint_decimals as u32)?;
            let market_value_sf = amount.checked_mul(r.market_price_sf)? / dec_pow;
            let weighted = market_value_sf
                .checked_mul(r.liquidation_threshold_pct as u128)? / 100;
            deposit_unhealthy_sum = deposit_unhealthy_sum.checked_add(weighted)?;
        }

        let mut borrow_debt_sum: u128 = 0;
        for b in &self.borrows {
            let r = cache.by_address.get(&b.borrow_reserve)?;
            let amount = b.borrowed_amount_sf >> 60;  // SF=2^60 → smallest units
            let dec_pow = 10u128.checked_pow(r.mint_decimals as u32)?;
            let market_value_sf = amount.checked_mul(r.market_price_sf)? / dec_pow;
            let weighted = market_value_sf
                .checked_mul(r.borrow_factor_pct as u128)? / 100;
            borrow_debt_sum = borrow_debt_sum.checked_add(weighted)?;
        }

        let scaled_threshold = deposit_unhealthy_sum
            .checked_mul(10_000 + margin_bps as u128)? / 10_000;
        Some(borrow_debt_sum >= scaled_threshold)
    }

    /// Event-driven variant of [`is_liquidatable_fresh_with_margin_bps`].
    ///
    /// Same recomputation logic, but for any reserve in `overrides` we
    /// substitute the supplied `market_price_sf` for the cached value.
    /// Use this when a Pyth tick has updated price X out-of-band of the
    /// reserve's own refresh — the cached `r.market_price_sf` is stale,
    /// but we know exactly what `refresh_reserve` will compute when the
    /// liquidate tx runs in the same atomic bundle.
    pub fn is_liquidatable_fresh_with_overrides(
        &self,
        cache:      &ReserveCache,
        overrides:  &HashMap<Pubkey, u128>,
        margin_bps: u32,
    ) -> Option<bool> {
        if self.has_debt_flag != 1 { return Some(false); }
        let price_of = |r: &ReserveSummary| -> u128 {
            *overrides.get(&r.address).unwrap_or(&r.market_price_sf)
        };

        let mut deposit_unhealthy_sum: u128 = 0;
        for d in &self.deposits {
            let r = cache.by_address.get(&d.deposit_reserve)?;
            let amount = r.ctoken_to_liquidity(d.deposited_amount)?;
            let price_sf = price_of(r);
            // market_price_sf is per-WHOLE-token × 2^60; divide by dec_pow
            // to scale amount(smallest) × USD/whole / 10^dec = USD × 2^60.
            let dec_pow = 10u128.checked_pow(r.mint_decimals as u32)?;
            let market_value_sf = amount.checked_mul(price_sf)? / dec_pow;
            let weighted = market_value_sf
                .checked_mul(r.liquidation_threshold_pct as u128)? / 100;
            deposit_unhealthy_sum = deposit_unhealthy_sum.checked_add(weighted)?;
        }
        let mut borrow_debt_sum: u128 = 0;
        for b in &self.borrows {
            let r = cache.by_address.get(&b.borrow_reserve)?;
            let amount = b.borrowed_amount_sf >> 60;
            let price_sf = price_of(r);
            let dec_pow = 10u128.checked_pow(r.mint_decimals as u32)?;
            let market_value_sf = amount.checked_mul(price_sf)? / dec_pow;
            let weighted = market_value_sf
                .checked_mul(r.borrow_factor_pct as u128)? / 100;
            borrow_debt_sum = borrow_debt_sum.checked_add(weighted)?;
        }
        let scaled_threshold = deposit_unhealthy_sum
            .checked_mul(10_000 + margin_bps as u128)? / 10_000;
        Some(borrow_debt_sum >= scaled_threshold)
    }

    /// Fresh-recompute predicate that AVOIDS the per-reserve config
    /// (`liquidation_threshold_pct`, `borrow_factor_pct`) entirely.
    /// Instead it derives the EFFECTIVE composite thresholds from the
    /// obligation's own cached values:
    ///
    ///   effective_liq_threshold = unhealthy_borrow_value_sf / deposited_value_sf
    ///   effective_borrow_factor = borrow_factor_adjusted_debt_value_sf / borrowed_assets_market_value_sf
    ///
    /// Then for each deposit/borrow we compute the FRESH raw market value
    /// (`amount × price`), sum them, and scale by the cached ratios.
    /// Liquidatable iff:
    ///   fresh_debt_bf × 10000 >= fresh_unhealthy × (10000 + margin_bps)
    ///
    /// Why this matters: Kamino's elevation groups override per-reserve
    /// `liquidation_threshold_pct` for obligations in the same group
    /// (e.g., kSOL+wSOL gets 95% effective threshold even though the
    /// reserve config says 55%). Reading the raw reserve config gave us
    /// LTV 1.67 vs on-chain 0.92 for a jitoSOL/wSOL pair. The obligation's
    /// cached unhealthy already encodes the elevation-group override —
    /// trust it.
    ///
    /// Returns None when:
    ///   - any referenced reserve is missing from cache
    ///   - the cached ratio denominators are zero (obligation never refreshed)
    ///   - arithmetic overflow
    pub fn is_liquidatable_fresh_via_cached_ratios(
        &self,
        cache:      &ReserveCache,
        overrides:  &HashMap<Pubkey, u128>,
        margin_bps: u32,
    ) -> Option<bool> {
        if self.has_debt_flag != 1 { return Some(false); }
        if self.deposited_value_sf == 0 { return None; }
        if self.borrowed_assets_market_value_sf == 0 { return None; }
        let price_of = |r: &ReserveSummary| -> u128 {
            *overrides.get(&r.address).unwrap_or(&r.market_price_sf)
        };

        // Sum fresh deposit market values (RAW, no liq_threshold weighting).
        let mut fresh_deposit_value: u128 = 0;
        for d in &self.deposits {
            let r = cache.by_address.get(&d.deposit_reserve)?;
            let amount = r.ctoken_to_liquidity(d.deposited_amount)?;
            let price_sf = price_of(r);
            let dec_pow = 10u128.checked_pow(r.mint_decimals as u32)?;
            let mv = amount.checked_mul(price_sf)? / dec_pow;
            fresh_deposit_value = fresh_deposit_value.checked_add(mv)?;
        }

        // Sum fresh borrow market values (RAW, no bf weighting).
        let mut fresh_borrow_value: u128 = 0;
        for b in &self.borrows {
            let r = cache.by_address.get(&b.borrow_reserve)?;
            let amount = b.borrowed_amount_sf >> 60;
            let price_sf = price_of(r);
            let dec_pow = 10u128.checked_pow(r.mint_decimals as u32)?;
            let mv = amount.checked_mul(price_sf)? / dec_pow;
            fresh_borrow_value = fresh_borrow_value.checked_add(mv)?;
        }

        // Scale cached weighted values by the price-driven shift.
        // fresh_unhealthy = cached_unhealthy × (fresh_deposit / cached_deposit)
        // fresh_debt_bf   = cached_debt_bf   × (fresh_borrow  / cached_borrow)
        //
        // Use u256-style sequencing to dodge u128 overflow on big values.
        let fresh_unhealthy = mul_div_u128(
            self.unhealthy_borrow_value_sf,
            fresh_deposit_value,
            self.deposited_value_sf,
        )?;
        let fresh_debt_bf = mul_div_u128(
            self.borrow_factor_adjusted_debt_value_sf,
            fresh_borrow_value,
            self.borrowed_assets_market_value_sf,
        )?;

        // liquidatable iff fresh_debt_bf >= fresh_unhealthy × (1 + margin)
        let scaled_threshold = fresh_unhealthy
            .checked_mul(10_000 + margin_bps as u128)? / 10_000;
        Some(fresh_debt_bf >= scaled_threshold)
    }

    pub fn referenced_reserves(&self) -> Vec<Pubkey> {
        let mut out = Vec::with_capacity(self.deposits.len() + self.borrows.len());
        for d in &self.deposits { out.push(d.deposit_reserve); }
        for b in &self.borrows  { out.push(b.borrow_reserve);  }
        out
    }

    /// Cached LTV ratio (debt/collateral) as f64. Useful for sorting
    /// candidates by how close they are to liquidation. >= 1.0 means
    /// already liquidatable per cache.
    pub fn cached_ltv_ratio(&self) -> Option<f64> {
        if self.deposited_value_sf == 0 { return None; }
        Some(
            (self.borrow_factor_adjusted_debt_value_sf as f64)
            / (self.deposited_value_sf as f64)
        )
    }
}

pub fn parse_obligation(address: Pubkey, bytes: &[u8]) -> Option<ParsedObligation> {
    if bytes.len() < KAMINO_OBLIGATION_SIZE { return None; }
    let lending_market = read_pubkey(bytes, OBL_OFF_LENDING_MARKET)?;
    let owner          = read_pubkey(bytes, OBL_OFF_OWNER)?;

    let mut deposits = Vec::with_capacity(OBL_MAX_DEPOSITS);
    for i in 0..OBL_MAX_DEPOSITS {
        let base = OBL_OFF_DEPOSITS_START + i * OBL_DEPOSIT_STRIDE;
        let reserve = read_pubkey(bytes, base + BAL_DEP_OFF_RESERVE)?;
        if reserve == Pubkey::default() { continue; }
        let amount  = read_u64(bytes, base + BAL_DEP_OFF_AMOUNT)?;
        if amount == 0 { continue; }
        deposits.push(ObligationDeposit { deposit_reserve: reserve, deposited_amount: amount });
    }

    let mut borrows = Vec::with_capacity(OBL_MAX_BORROWS);
    for i in 0..OBL_MAX_BORROWS {
        let base = OBL_OFF_BORROWS_START + i * OBL_BORROW_STRIDE;
        let reserve = read_pubkey(bytes, base + BAL_BOR_OFF_RESERVE)?;
        if reserve == Pubkey::default() { continue; }
        let amount_sf = read_u128(bytes, base + BAL_BOR_OFF_AMOUNT_SF)?;
        if amount_sf == 0 { continue; }
        borrows.push(ObligationBorrow { borrow_reserve: reserve, borrowed_amount_sf: amount_sf });
    }

    let deposited_value_sf = read_u128(bytes, OBL_OFF_DEPOSITED_VALUE_SF)?;
    let borrow_factor_adjusted_debt_value_sf = read_u128(bytes, OBL_OFF_BORROW_FACTOR_ADJ_DEBT_SF)?;
    let borrowed_assets_market_value_sf =
        read_u128(bytes, OBL_OFF_BORROWED_ASSETS_MARKET_VALUE_SF)?;
    let unhealthy_borrow_value_sf = read_u128(bytes, OBL_OFF_UNHEALTHY_BORROW_VALUE_SF)?;
    let has_debt_flag = bytes.get(OBL_OFF_HAS_DEBT).copied()?;
    let last_update_slot = read_u64(bytes, OBL_OFF_LAST_UPDATE_SLOT)?;

    Some(ParsedObligation {
        address, lending_market, owner, deposits, borrows,
        deposited_value_sf, borrow_factor_adjusted_debt_value_sf,
        borrowed_assets_market_value_sf,
        unhealthy_borrow_value_sf, has_debt_flag,
        last_update_slot,
    })
}

// ─── Reserve cache ─────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct ReserveCache {
    pub by_address:        HashMap<Pubkey, ReserveSummary>,
    pub by_lending_market: HashMap<Pubkey, Vec<Pubkey>>,
}

/// Convert a fresh Pyth price (raw integer + exponent) into the reserve's
/// SF=2^60 per-smallest-unit USD format. Mirrors what Kamino's
/// `refresh_reserve` will compute on-chain in the same atomic tx.
///
///   reserve.market_price_sf
///     = price_per_whole_token_USD × 2^60 / 10^mint_decimals
///     = (pyth_price × 10^pyth_exponent) × 2^60 / 10^mint_decimals
///     = pyth_price × 2^60 × 10^(pyth_exponent - mint_decimals)
///
/// Pyth exponent is typically negative (e.g. -8 for SOL). After subtracting
/// mint_decimals the net exponent is very negative — we divide rather than
/// multiply to stay in u128.
pub fn fresh_market_price_sf(
    pyth_price:     i64,
    pyth_exponent:  i32,
    mint_decimals:  u8,
) -> Option<u128> {
    if pyth_price <= 0 { return None; }
    let net_exp = pyth_exponent - mint_decimals as i32;
    let price_u: u128 = pyth_price as u128;
    let sf_scale: u128 = 1u128 << 60;
    if net_exp >= 0 {
        // pyth × 2^60 × 10^net_exp
        let pow = 10u128.checked_pow(net_exp as u32)?;
        price_u.checked_mul(sf_scale)?.checked_mul(pow)
    } else {
        // pyth × 2^60 / 10^(-net_exp)
        let neg = (-net_exp) as u32;
        let pow = 10u128.checked_pow(neg)?;
        price_u.checked_mul(sf_scale).map(|v| v / pow)
    }
}

/// Inverted indices built once at hunter-bootstrap. Lets the tick-driven
/// loop go pyth_feed → reserves → obligations in O(1) hash lookups instead
/// of O(N obligations) scans.
#[derive(Debug, Default)]
pub struct LiquidationIndex {
    /// Pyth pubkey → all reserve pubkeys that price-source from it.
    pub pyth_to_reserves: HashMap<Pubkey, Vec<Pubkey>>,
    /// Scope PRICES account → all reserve pubkeys whose `scope_price_feed`
    /// equals this account. Each Scope account holds 500+ token entries;
    /// many reserves share the same Scope account (typical: ~200 reserves
    /// → 1-3 unique Scope accounts). On a Scope account update we compute
    /// the fresh `market_price_sf` for each reserve via its `scope_chain`.
    pub scope_to_reserves: HashMap<Pubkey, Vec<Pubkey>>,
    /// Reserve pubkey → indices into `obligations` Vec for obligations
    /// that have a position (deposit or borrow) on that reserve.
    pub reserve_to_obls:  HashMap<Pubkey, Vec<usize>>,
}

/// Kamino's null-sentinel pubkey — base58 `nu11111111111111111111111111111111111111111`.
/// Reserves with a price source NOT configured for a given oracle slot
/// fill that slot with this sentinel instead of all-zeros. Index builders
/// must filter both `Pubkey::default()` (all zeros) AND this value.
pub fn kamino_null_pubkey() -> Pubkey {
    "nu11111111111111111111111111111111111111111".parse().unwrap()
}

impl LiquidationIndex {
    pub fn build(cache: &ReserveCache, obligations: &[ParsedObligation]) -> Self {
        let mut idx = Self::default();
        let null_sentinel = kamino_null_pubkey();
        for (addr, r) in cache.by_address.iter() {
            if r.pyth_price != Pubkey::default() && r.pyth_price != null_sentinel {
                idx.pyth_to_reserves.entry(r.pyth_price).or_default().push(*addr);
            }
            // Scope feed: index any reserve with a usable chain. Single-
            // entry chains resolve directly; composite chains (kSOL etc)
            // are now handled by fresh_market_price_sf_from_scope, which
            // multiplies all non-sentinel entries together. Reserves with
            // an all-sentinel chain still get reactive coverage via the
            // reserve subscription path.
            if r.scope_price_feed != Pubkey::default()
                && r.scope_price_feed != null_sentinel
                && crate::kamino_scope::chain_length(&r.scope_chain) > 0
            {
                idx.scope_to_reserves.entry(r.scope_price_feed).or_default().push(*addr);
            }
        }
        for (i, ob) in obligations.iter().enumerate() {
            for d in &ob.deposits {
                idx.reserve_to_obls.entry(d.deposit_reserve).or_default().push(i);
            }
            for b in &ob.borrows {
                idx.reserve_to_obls.entry(b.borrow_reserve).or_default().push(i);
            }
        }
        idx
    }

    /// All Pyth pubkeys the laserstream subscription should listen on.
    pub fn pyth_feeds(&self) -> Vec<Pubkey> {
        self.pyth_to_reserves.keys().copied().collect()
    }

    /// All Scope PRICES account pubkeys to subscribe to. Typically just
    /// a small set (1-3) since Kamino's main markets all share one big
    /// Scope account.
    pub fn scope_feeds(&self) -> Vec<Pubkey> {
        self.scope_to_reserves.keys().copied().collect()
    }

    /// All reserve pubkeys referenced by at least one obligation. Use
    /// alongside `pyth_feeds()` to also subscribe to reserves directly —
    /// covers oracles we haven't built parsers for (Scope, Switchboard
    /// composite) by reacting to the reserve's own `market_price_sf`
    /// changing after any refresh_reserve call.
    pub fn reserve_addresses(&self) -> Vec<Pubkey> {
        self.reserve_to_obls.keys().copied().collect()
    }
}

/// POST with retry on 429 — Helius rate-limits gPA / gMA bursts. Backoff
/// 500ms → 1s → 2s → 4s. Returns the parsed JSON on success; bubbles up
/// the last error after all retries.
async fn rpc_post_with_retry(
    client:  &reqwest::Client,
    rpc_url: &str,
    body:    &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    for attempt in 0..5u32 {
        match client.post(rpc_url).json(body).send().await {
            Ok(resp) => {
                if resp.status().as_u16() == 429 && attempt < 4 {
                    let wait_ms = 500u64 << attempt;
                    eprintln!("  429 — retry {} in {}ms", attempt + 1, wait_ms);
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                let resp = resp.error_for_status()?;
                return Ok(resp.json().await?);
            }
            Err(e) if attempt < 4 => {
                let wait_ms = 500u64 << attempt;
                eprintln!("  net err: {} — retry {} in {}ms", e, attempt + 1, wait_ms);
                tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(anyhow!("rpc_post_with_retry: exhausted retries"))
}

pub async fn fetch_reserve_cache(rpc_url: &str) -> anyhow::Result<ReserveCache> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
        "params":[KAMINO_LEND_PROGRAM_STR, {
            "encoding": "base64",
            "commitment": "confirmed",
            "filters": [{"dataSize": KAMINO_RESERVE_SIZE}],
        }],
    });
    let client = reqwest::Client::new();
    let r = rpc_post_with_retry(&client, rpc_url, &body).await
        .context("getProgramAccounts (kamino reserves)")?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no result array: {r}"))?;

    let mut cache = ReserveCache::default();
    for entry in arr {
        let pubkey = match entry.get("pubkey").and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Pubkey>().ok()) { Some(p) => p, None => continue };
        let data_b64 = entry.pointer("/account/data/0").and_then(|v| v.as_str()).unwrap_or("");
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(b) => b, Err(_) => continue,
        };
        let Some(r) = parse_reserve(pubkey, &bytes) else { continue };
        cache.by_lending_market.entry(r.lending_market).or_default().push(pubkey);
        cache.by_address.insert(pubkey, r);
    }
    Ok(cache)
}

// ─── Obligation fetch + filter ─────────────────────────────────

/// Two-step fetcher: pubkey list via gPA (dataSlice=0 keeps response
/// tiny), then chunked getMultipleAccounts. Mirrors the pattern in
/// marginfi-monitor that proved necessary on the 3.7GB Frankfurt box.
pub async fn fetch_all_obligation_pubkeys(rpc_url: &str) -> anyhow::Result<Vec<Pubkey>> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
        "params":[KAMINO_LEND_PROGRAM_STR, {
            "encoding":"base64","commitment":"confirmed",
            "filters":[{"dataSize": KAMINO_OBLIGATION_SIZE}],
            "dataSlice":{"offset":0,"length":0},
        }],
    });
    let client = reqwest::Client::new();
    let r = rpc_post_with_retry(&client, rpc_url, &body).await
        .context("getProgramAccounts (kamino obligations list)")?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no result array: {r}"))?;
    Ok(arr.iter()
        .filter_map(|e| e.get("pubkey").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
        .collect())
}

/// Split-by-market obligation fetcher. The single-call gPA above asks
/// Helius for ALL 63K Kamino obligations in one shot, which is heavy
/// enough that Helius rate-limits us sustained (429 for 7.5s+).
///
/// Workaround: iterate each known lending_market and use a memcmp filter
/// at offset 32 (= obligation.lending_market field) to scope the query.
/// Each market returns a few hundred to ~10k obligations — much friendlier.
///
/// Pace requests with a per-market delay (default 800ms, env
/// `KAMINO_MARKET_GPA_THROTTLE_MS`) so the cumulative load stays within
/// Helius's quota.
pub async fn fetch_obligation_pubkeys_by_market(
    rpc_url:           &str,
    lending_markets:   &[Pubkey],
) -> anyhow::Result<Vec<Pubkey>> {
    let client = reqwest::Client::new();
    let throttle_ms: u64 = std::env::var("KAMINO_MARKET_GPA_THROTTLE_MS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(800);
    let mut out: Vec<Pubkey> = Vec::new();
    for (i, mkt) in lending_markets.iter().enumerate() {
        let bytes_b58 = bs58::encode(mkt.as_ref()).into_string();
        // has_debt_flag=1 at offset OBL_OFF_HAS_DEBT (=2287). Filter on it
        // at gPA time so the obligation list only includes those with debt
        // — Helius gMA pressure drops 10× (from 131K to ~12K typical).
        // base58 of single byte [1] = "2" (we double-check the codec but
        // bs58::encode(&[1u8]).into_string() returns "2").
        let debt_byte_b58 = bs58::encode([1u8]).into_string();
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
            "params":[KAMINO_LEND_PROGRAM_STR, {
                "encoding":"base64","commitment":"confirmed",
                "filters":[
                    {"dataSize": KAMINO_OBLIGATION_SIZE},
                    {"memcmp": {"offset": 32, "bytes": bytes_b58}},
                    {"memcmp": {"offset": OBL_OFF_HAS_DEBT, "bytes": debt_byte_b58}},
                ],
                "dataSlice":{"offset":0,"length":0},
            }],
        });
        let r = match rpc_post_with_retry(&client, rpc_url, &body).await {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  market {} ({}/{}) gPA failed: {} — skipping",
                          mkt, i + 1, lending_markets.len(), e);
                continue;
            }
        };
        let Some(arr) = r.pointer("/result").and_then(|v| v.as_array()) else { continue };
        let start = out.len();
        out.extend(arr.iter()
            .filter_map(|e| e.get("pubkey").and_then(|v| v.as_str()).and_then(|s| s.parse::<Pubkey>().ok())));
        eprintln!("  market {} ({}/{}): +{} obligations",
                  mkt, i + 1, lending_markets.len(), out.len() - start);
        if throttle_ms > 0 && i + 1 < lending_markets.len() {
            tokio::time::sleep(std::time::Duration::from_millis(throttle_ms)).await;
        }
    }
    Ok(out)
}

/// Chunked fetcher — only returns obligations whose `has_debt_flag == 1`.
/// Skip-no-debt at parse time so peak in-memory is bounded.
pub async fn fetch_obligations_with_debt(
    rpc_url: &str,
    pubkeys: &[Pubkey],
) -> anyhow::Result<Vec<ParsedObligation>> {
    let client = reqwest::Client::new();
    let mut out = Vec::new();
    // Throttle to stay under Helius's ~5/sec cap on gMA (configurable).
    let throttle_ms: u64 = std::env::var("KAMINO_CHUNK_THROTTLE_MS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(250);
    for chunk in pubkeys.chunks(100) {
        let keys: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getMultipleAccounts",
            "params":[keys, {"encoding":"base64","commitment":"confirmed"}],
        });
        let r = rpc_post_with_retry(&client, rpc_url, &body).await?;
        let vals = r.pointer("/result/value").and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("no result/value"))?;
        for (i, val) in vals.iter().enumerate() {
            if val.is_null() { continue; }
            let data_b64 = val.pointer("/data/0").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b, Err(_) => continue,
            };
            if let Some(p) = parse_obligation(chunk[i], &bytes) {
                if p.has_debt() { out.push(p); }
            }
        }
        if throttle_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(throttle_ms)).await;
        }
    }
    Ok(out)
}

/// Returns obligations the cached health check flags as liquidatable,
/// sorted by most-underwater first (highest LTV ratio).
pub fn find_liquidatable_obligations(
    obligations: &[ParsedObligation],
    min_debt_usd: f64,
    cache:        &ReserveCache,
) -> Vec<ParsedObligation> {
    find_liquidatable_obligations_at_slot(obligations, min_debt_usd, cache, 0)
}

/// Same as `find_liquidatable_obligations` but additionally filters
/// obligations whose `last_update_slot` is more than `max_stale_slots`
/// behind `current_slot`. Cache freshness matters: live evidence shows
/// stale obligations can have 31% LTV drift from on-chain reality.
/// Pass `current_slot = 0` to disable the freshness check.
pub fn find_liquidatable_obligations_at_slot(
    obligations: &[ParsedObligation],
    min_debt_usd: f64,
    cache:        &ReserveCache,
    current_slot: u64,
) -> Vec<ParsedObligation> {
    let min_debt_sf = (min_debt_usd * (SF_SCALE as f64)) as u128;
    // Margin required above the on-chain unhealthy threshold. Filters
    // borderline candidates that pass our stale snapshot but fail
    // on-chain after refresh_obligation recomputes with current Pyth.
    let margin_bps: u32 = std::env::var("NATIVE_KAMINO_HEALTH_MARGIN_BPS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(100);
    // Max slots between the obligation's last on-chain refresh and the
    // current slot before we treat the cache as too stale. Default 1000
    // slots ≈ 7 minutes; older snapshots showed 30%+ LTV drift live.
    // Set to 0 (or current_slot=0) to disable. Set lower for safer filter.
    let max_stale: u64 = std::env::var("NATIVE_KAMINO_MAX_STALE_SLOTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(1000);
    // Track A: also require the FRESH recompute (using current reserve
    // prices) to agree the obligation is liquidatable. The obligation's
    // stored *_value_sf fields are frozen at last on-chain refresh and
    // can be 30%+ off vs reality. Reserve cache prices update whenever
    // ANY user touches the reserve, so they're typically ≤30s old.
    //
    // If the obligation has any reserves NOT in our cache, we can't
    // recompute — fall back to the cached check (less safe but at least
    // attempts something).
    let use_fresh: bool = std::env::var("NATIVE_KAMINO_FRESH_RECOMPUTE")
        .as_deref() != Ok("false"); // default ON
    let mut out: Vec<ParsedObligation> = obligations.iter()
        .filter(|o| {
            let cache_fresh = current_slot == 0
                || max_stale == 0
                || (current_slot.saturating_sub(o.last_update_slot) <= max_stale);
            if !cache_fresh { return false; }
            if !o.borrows.iter().all(|b| cache.by_address.contains_key(&b.borrow_reserve)) { return false; }
            if !o.deposits.iter().all(|d| cache.by_address.contains_key(&d.deposit_reserve)) { return false; }
            if o.borrow_factor_adjusted_debt_value_sf < min_debt_sf { return false; }
            if !o.is_liquidatable_per_cache_with_margin_bps(margin_bps) { return false; }
            // Track A: FRESH-recompute gate. None (overflow / missing reserve) → reject conservatively.
            if use_fresh {
                match o.is_liquidatable_fresh_with_margin_bps(cache, margin_bps) {
                    Some(true)  => true,
                    Some(false) => false,
                    None        => false,
                }
            } else {
                true
            }
        })
        .cloned()
        .collect();
    out.sort_by(|a, b| {
        let ra = a.cached_ltv_ratio().unwrap_or(0.0);
        let rb = b.cached_ltv_ratio().unwrap_or(0.0);
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

// ─── Raw byte helpers ──────────────────────────────────────────

fn read_pubkey(bytes: &[u8], off: usize) -> Option<Pubkey> {
    if off + 32 > bytes.len() { return None; }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&bytes[off..off + 32]);
    Some(Pubkey::new_from_array(buf))
}
fn read_u64(bytes: &[u8], off: usize) -> Option<u64> {
    if off + 8 > bytes.len() { return None; }
    Some(u64::from_le_bytes(bytes[off..off + 8].try_into().ok()?))
}
fn read_u16(bytes: &[u8], off: usize) -> Option<u16> {
    if off + 2 > bytes.len() { return None; }
    Some(u16::from_le_bytes(bytes[off..off + 2].try_into().ok()?))
}

/// `(a × b) / c` with overflow-safe sequencing. Computes the equivalent
/// of `(a * b) / c` but avoids u128 overflow when `a × b > u128::MAX` by
/// reducing the factors first via gcd-style cancellation.
fn mul_div_u128(a: u128, b: u128, c: u128) -> Option<u128> {
    if c == 0 { return None; }
    // Fast path: try direct multiply.
    if let Some(prod) = a.checked_mul(b) {
        return Some(prod / c);
    }
    // Fallback: split via gcd to keep intermediate values small.
    let g_ac = gcd_u128(a, c);
    let (a1, c1) = (a / g_ac, c / g_ac);
    if let Some(prod) = a1.checked_mul(b) {
        return Some(prod / c1);
    }
    let g_bc = gcd_u128(b, c1);
    let (b1, c2) = (b / g_bc, c1 / g_bc);
    a1.checked_mul(b1).map(|p| p / c2)
}

fn gcd_u128(mut a: u128, mut b: u128) -> u128 {
    while b != 0 { let t = b; b = a % b; a = t; }
    a
}
fn read_u128(bytes: &[u8], off: usize) -> Option<u128> {
    if off + 16 > bytes.len() { return None; }
    Some(u128::from_le_bytes(bytes[off..off + 16].try_into().ok()?))
}

// ─── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn obligation_layout_matches_klend_source() {
        // OBLIGATION_SIZE in klend is asserted to be 3336 (struct only,
        // pre-disc). On-chain = 3336 + 8 = 3344.
        assert_eq!(KAMINO_OBLIGATION_SIZE, 3344);
        // deposits 88..1176, lowest_ltv 1176..1184, deposited_value_sf
        // 1184..1200, borrows 1200..2200.
        let payload_borrows_start = OBL_OFF_BORROWS_START - 8;
        assert_eq!(payload_borrows_start, 1200);
        assert_eq!(payload_borrows_start + OBL_MAX_BORROWS * OBL_BORROW_STRIDE, 2200);
        // All health fields fit within the on-chain account size.
        assert!(OBL_OFF_HAS_DEBT < KAMINO_OBLIGATION_SIZE);
        assert!(OBL_OFF_UNHEALTHY_BORROW_VALUE_SF + 16 <= KAMINO_OBLIGATION_SIZE);
    }

    #[test]
    fn margin_100bps_filters_just_over_threshold() {
        // debt = unhealthy * 1.005 → 50bps over threshold.
        // With margin=0 it passes; with margin=100 it fails (requires ≥ 1%).
        let o = ParsedObligation {
            address: Pubkey::default(),
            lending_market: Pubkey::default(),
            owner: Pubkey::default(),
            deposits: vec![],
            borrows:  vec![],
            deposited_value_sf:                    1_000 * SF_SCALE,
            borrow_factor_adjusted_debt_value_sf:    804 * SF_SCALE,
            unhealthy_borrow_value_sf:               800 * SF_SCALE,
            borrowed_assets_market_value_sf: 0, has_debt_flag: 1, last_update_slot: 0,
        };
        assert!(o.is_liquidatable_per_cache_with_margin_bps(0));
        assert!(!o.is_liquidatable_per_cache_with_margin_bps(100));
    }

    #[test]
    fn margin_100bps_admits_2pct_over_threshold() {
        let o = ParsedObligation {
            address: Pubkey::default(),
            lending_market: Pubkey::default(),
            owner: Pubkey::default(),
            deposits: vec![],
            borrows:  vec![],
            deposited_value_sf:                    1_000 * SF_SCALE,
            borrow_factor_adjusted_debt_value_sf:    816 * SF_SCALE,
            unhealthy_borrow_value_sf:               800 * SF_SCALE,
            borrowed_assets_market_value_sf: 0, has_debt_flag: 1, last_update_slot: 0,
        };
        // 816/800 = 1.02 → 200bps over threshold → passes 100bps margin
        assert!(o.is_liquidatable_per_cache_with_margin_bps(100));
        assert!(!o.is_liquidatable_per_cache_with_margin_bps(300));
    }

    #[test]
    fn is_liquidatable_when_debt_exceeds_unhealthy_threshold() {
        let o = ParsedObligation {
            address: Pubkey::default(),
            lending_market: Pubkey::default(),
            owner: Pubkey::default(),
            deposits: vec![],
            borrows:  vec![],
            deposited_value_sf:                    1_000 * SF_SCALE,
            borrow_factor_adjusted_debt_value_sf:    900 * SF_SCALE,
            unhealthy_borrow_value_sf:               800 * SF_SCALE,
            borrowed_assets_market_value_sf: 0, has_debt_flag: 1, last_update_slot: 0,
        };
        assert!(o.is_liquidatable_per_cache());
        let ratio = o.cached_ltv_ratio().unwrap();
        assert!((ratio - 0.9).abs() < 1e-9);
    }

    #[test]
    fn healthy_obligation_not_liquidatable() {
        let o = ParsedObligation {
            address: Pubkey::default(),
            lending_market: Pubkey::default(),
            owner: Pubkey::default(),
            deposits: vec![],
            borrows:  vec![],
            deposited_value_sf:                    1_000 * SF_SCALE,
            borrow_factor_adjusted_debt_value_sf:    500 * SF_SCALE,
            unhealthy_borrow_value_sf:               800 * SF_SCALE,
            borrowed_assets_market_value_sf: 0, has_debt_flag: 1, last_update_slot: 0,
        };
        assert!(!o.is_liquidatable_per_cache());
    }

    #[test]
    fn obligation_with_has_debt_zero_skipped() {
        let o = ParsedObligation {
            address: Pubkey::default(),
            lending_market: Pubkey::default(),
            owner: Pubkey::default(),
            deposits: vec![],
            borrows:  vec![],
            deposited_value_sf: 1_000 * SF_SCALE,
            borrow_factor_adjusted_debt_value_sf: 9_999 * SF_SCALE,
            borrowed_assets_market_value_sf: 0,
            unhealthy_borrow_value_sf: 1 * SF_SCALE,
            has_debt_flag: 0, last_update_slot: 0,  // KEY: program rejects this even though math says liquidatable
        };
        assert!(!o.is_liquidatable_per_cache());
    }

    #[test]
    fn reserve_offset_for_lending_market_within_size() {
        assert!(RES_OFF_LIQ_MINT_DECIMALS + 8 <= KAMINO_RESERVE_SIZE);
    }

    #[test]
    fn sf_scale_decodes_one() {
        // 1.0 in I80F48 representation = 2^60.
        let one_sf: u128 = SF_SCALE;
        let f = (one_sf as f64) / (SF_SCALE as f64);
        assert!((f - 1.0).abs() < 1e-12);
    }
}
