//! Raydium CLMM (Concentrated Liquidity Market Maker) quoter.
//!
//! ## Account layout — `PoolState` (1544 bytes total)
//!
//! Verified against [`raydium-io/raydium-clmm/programs/amm/src/states/pool.rs`].
//! Uses `#[account(zero_copy(unsafe))] #[repr(C, packed)]` so the on-wire
//! layout has no padding between fields.
//!
//! Offsets are from the start of `data` (including the 8-byte Anchor disc):
//!
//! | Field            | Offset | Size |
//! |------------------|--------|------|
//! | (discriminator)  |   0    |  8   |
//! | bump             |   8    |  1   |
//! | amm_config       |   9    | 32   |
//! | owner            |  41    | 32   |
//! | token_mint_0     |  73    | 32   |
//! | token_mint_1     | 105    | 32   |
//! | token_vault_0    | 137    | 32   |
//! | token_vault_1    | 169    | 32   |
//! | observation_key  | 201    | 32   |
//! | mint_decimals_0  | 233    |  1   |
//! | mint_decimals_1  | 234    |  1   |
//! | tick_spacing     | 235    |  2   |
//! | liquidity        | 237    | 16   |
//! | sqrt_price_x64   | 253    | 16   |
//! | tick_current     | 269    |  4   |
//!
//! ## Fee tier
//!
//! `trade_fee_rate` lives in the separate `AmmConfig` account referenced via
//! the `amm_config` field. The scout bootstraps this in two steps: (1) fetch
//! pools, (2) collect `amm_config` pubkeys from any CLMM pools just parsed,
//! (3) fetch those AmmConfigs and store the `trade_fee_rate` in
//! [`ClmmFeeStore`]. The quoter looks up the fee on each quote.
//!
//! ## Quote math — v1 spot-price approximation
//!
//! Same approach as the Orca Whirlpool quoter (see `orca.rs` module doc):
//! for opportunity *detection* the spot-price is sufficient; full
//! tick-walking math is a Phase-2 follow-up.

use std::sync::Arc;

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use dashmap::DashMap;
use solana_program::instruction::Instruction;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

pub const RAYDIUM_CLMM_PROGRAM: Pubkey =
    pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

pub const RAYDIUM_CLMM_ACCOUNT_SIZE: usize = 1544;

const CL_OFF_AMM_CONFIG:    usize = 9;
const CL_OFF_TOKEN_MINT_0:  usize = 73;
const CL_OFF_TOKEN_MINT_1:  usize = 105;
const CL_OFF_TOKEN_VAULT_0: usize = 137;
const CL_OFF_TOKEN_VAULT_1: usize = 169;
const CL_OFF_OBSERVATION:   usize = 201;
const CL_OFF_DECIMALS_0:    usize = 233;
const CL_OFF_DECIMALS_1:    usize = 234;
const CL_OFF_TICK_SPACING:  usize = 235;
const CL_OFF_LIQUIDITY:     usize = 237;
const CL_OFF_SQRT_PRICE:    usize = 253;
const CL_OFF_TICK_CURRENT:  usize = 269;

#[derive(Debug, Clone, Copy)]
pub struct RaydiumClmmData {
    pub amm_config:        Pubkey,
    pub token_mint_0:      Pubkey,
    pub token_mint_1:      Pubkey,
    pub token_vault_0:     Pubkey,
    pub token_vault_1:     Pubkey,
    pub observation_key:   Pubkey,
    pub mint_decimals_0:   u8,
    pub mint_decimals_1:   u8,
    pub tick_spacing:      u16,
    pub liquidity:         u128,
    pub sqrt_price_x64:    u128,
    pub tick_current:      i32,
}

impl RaydiumClmmData {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() != RAYDIUM_CLMM_ACCOUNT_SIZE { return None; }

        let read_pk = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o + 32]);
            Pubkey::new_from_array(b)
        };
        let read_u128 = |o: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&data[o..o + 16]);
            u128::from_le_bytes(b)
        };
        let read_i32 = |o: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&data[o..o + 4]);
            i32::from_le_bytes(b)
        };

        let read_u16 = |o: usize| u16::from_le_bytes([data[o], data[o+1]]);

        Some(RaydiumClmmData {
            amm_config:      read_pk(CL_OFF_AMM_CONFIG),
            token_mint_0:    read_pk(CL_OFF_TOKEN_MINT_0),
            token_mint_1:    read_pk(CL_OFF_TOKEN_MINT_1),
            token_vault_0:   read_pk(CL_OFF_TOKEN_VAULT_0),
            token_vault_1:   read_pk(CL_OFF_TOKEN_VAULT_1),
            observation_key: read_pk(CL_OFF_OBSERVATION),
            mint_decimals_0: data[CL_OFF_DECIMALS_0],
            mint_decimals_1: data[CL_OFF_DECIMALS_1],
            tick_spacing:    read_u16(CL_OFF_TICK_SPACING),
            liquidity:       read_u128(CL_OFF_LIQUIDITY),
            sqrt_price_x64:  read_u128(CL_OFF_SQRT_PRICE),
            tick_current:    read_i32(CL_OFF_TICK_CURRENT),
        })
    }
}

// --------------------------------------------------------------------------
// AmmConfig — separate account holding the fee tier.
// Layout (Anchor non-zero-copy, so 8-byte discriminator at start):
//   discriminator: 8
//   bump: 1                        @ 8
//   index: u16                     @ 9
//   owner: Pubkey                  @ 11
//   protocol_fee_rate: u32         @ 43
//   trade_fee_rate: u32            @ 47   <- THE NUMBER WE WANT
//   tick_spacing: u16              @ 51
//   fund_fee_rate: u32             @ 53
//   ... (padding follows)
// --------------------------------------------------------------------------
const AMMCONFIG_OFF_TRADE_FEE_RATE: usize = 47;

/// `trade_fee_rate` is denominated in millionths (1 ppm), matching Uniswap-V3
/// convention. Examples:
///   - 0.01% tier → 100
///   - 0.05% tier → 500
///   - 0.30% tier → 3000
///   - 1.00% tier → 10_000
pub const CLMM_FEE_DENOMINATOR: u64 = 1_000_000;

pub fn parse_trade_fee_rate(amm_config_data: &[u8]) -> Option<u32> {
    if amm_config_data.len() < AMMCONFIG_OFF_TRADE_FEE_RATE + 4 { return None; }
    let mut b = [0u8; 4];
    b.copy_from_slice(&amm_config_data[AMMCONFIG_OFF_TRADE_FEE_RATE..AMMCONFIG_OFF_TRADE_FEE_RATE + 4]);
    Some(u32::from_le_bytes(b))
}

// --------------------------------------------------------------------------
// Fee store: maps each CLMM POOL pubkey -> trade_fee_rate (in millionths).
// Populated by the scout's bootstrap after fetching AmmConfigs. Quoter reads
// this map on every quote.
// --------------------------------------------------------------------------
#[derive(Debug, Default)]
pub struct ClmmFeeStore {
    fees: DashMap<Pubkey, u32>,
}

impl ClmmFeeStore {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }
    pub fn upsert(&self, pool: Pubkey, fee_rate: u32) { self.fees.insert(pool, fee_rate); }
    pub fn get(&self, pool: &Pubkey) -> Option<u32> { self.fees.get(pool).map(|v| *v.value()) }
    pub fn len(&self) -> usize { self.fees.len() }
    pub fn is_empty(&self) -> bool { self.fees.is_empty() }
}

// --------------------------------------------------------------------------
// Quote math — spot price approximation
// --------------------------------------------------------------------------

/// Spot quote: assumes infinite depth at current sqrt_price. See module doc.
///
/// `direction_0_to_1`: input is token_0 if true, else input is token_1.
#[allow(clippy::too_many_arguments)]
pub fn spot_quote(
    input:                  u64,
    sqrt_price_x64:         u128,
    fee_rate_millionths:    u32,
    direction_0_to_1:       bool,
    decimals_0:             u8,
    decimals_1:             u8,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if sqrt_price_x64 == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: input,
        });
    }

    let sqrt_f = (sqrt_price_x64 as f64) / 18_446_744_073_709_551_616.0; // 2^64
    let raw_1_per_raw_0 = sqrt_f * sqrt_f;
    if raw_1_per_raw_0 <= 0.0 {
        return Err(ArbError::Invariant("non-positive price"));
    }

    let fee_factor = 1.0 - (fee_rate_millionths as f64) / CLMM_FEE_DENOMINATOR as f64;
    let input_f = input as f64;

    // Convert raw->raw price into a UI-amount price via decimals, then back
    // to raw output. This collapses to the same formula whether we're
    // 0->1 or 1->0; the direction just inverts.
    let dec_factor = 10f64.powi(decimals_1 as i32 - decimals_0 as i32);
    let ui_price_1_per_0 = raw_1_per_raw_0 / dec_factor; // (1 token_1 UI) per (1 token_0 UI)

    let out_f = if direction_0_to_1 {
        // input is token_0 (raw, dec_0). Output is token_1 (raw, dec_1).
        // input_ui = input / 10^dec_0
        // output_ui = input_ui * ui_price_1_per_0 * fee_factor
        // output_raw = output_ui * 10^dec_1
        input_f * raw_1_per_raw_0 * fee_factor
    } else {
        // input token_1, output token_0.
        if ui_price_1_per_0 <= 0.0 { return Err(ArbError::Invariant("zero price")); }
        input_f / raw_1_per_raw_0 * fee_factor
    };

    if !out_f.is_finite() || out_f < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out_f > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out_f as u64)
}

/// Raydium CLMM single-tick swap quote with proper price-impact math.
///
/// Same Uniswap V3 invariant as Whirlpool: as input is added, sqrt_price
/// moves AWAY from the entry edge so the average fill price is WORSE than
/// spot. Single-tick approximation; ignores tick crossings.
///
/// See `arb_dex_orca::swap_quote` for the full formula derivation.
#[allow(clippy::too_many_arguments)]
pub fn swap_quote(
    input:                  u64,
    sqrt_price_x64:         u128,
    liquidity:              u128,
    fee_rate_millionths:    u32,
    direction_0_to_1:       bool,
    tick_spacing:           u16,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if sqrt_price_x64 == 0 || liquidity == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: input,
        });
    }

    let q64    = 18_446_744_073_709_551_616.0_f64; // 2^64
    let sqrt_p = sqrt_price_x64 as f64;
    let l      = liquidity as f64;

    let fee_factor      = 1.0 - (fee_rate_millionths as f64) / CLMM_FEE_DENOMINATOR as f64;
    let input_after_fee = (input as f64) * fee_factor;

    // v47 "leave 1 unit" policy — see arb_dex_orca::swap_quote. When the
    // swap would saturate beyond the safe single-tick window, cap the quote
    // at the pool's in-tick MAX minus a small safety reserve. The min_out
    // we compute downstream is then realistic, so the on-chain swap doesn't
    // revert on AmountOutBelowMinimum.
    // v55: bumped default from 1 → 2 to match Whirlpool. Pool retains an
    // extra unit beyond the -1 cap, giving sim a wider tolerance window.
    let leave_units: f64 = std::env::var("CLMM_LEAVE_UNITS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(2.0);
    let max_ticks_per_leg: i32 = std::env::var("CLMM_QUOTE_MAX_TICKS_PER_LEG")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    let allowed_tick_move = max_ticks_per_leg.saturating_mul(tick_spacing as i32);
    let allowed_sqrt_ratio = 1.0001_f64.powi(allowed_tick_move).sqrt();
    let max_move_ratio = (allowed_sqrt_ratio - 1.0).abs();

    let out_f = if direction_0_to_1 {
        // Adding token_0 → sqrt_price decreases.
        let denom = l + input_after_fee * sqrt_p / q64;
        if denom <= 0.0 {
            return Err(ArbError::Invariant("clmm: zero denominator"));
        }
        let new_sqrt = l * sqrt_p / denom;
        if new_sqrt > sqrt_p {
            return Ok(0);
        }
        let single_tick_out = l * (sqrt_p - new_sqrt) / q64;
        let max_in_tick_out = (l * sqrt_p / q64 - leave_units).max(0.0);
        if max_move_ratio > 0.0 && (sqrt_p - new_sqrt) / sqrt_p > max_move_ratio {
            single_tick_out.min(max_in_tick_out)
        } else {
            single_tick_out
        }
    } else {
        // Adding token_1 → sqrt_price increases.
        let new_sqrt = sqrt_p + input_after_fee * q64 / l;
        let single_tick_out = {
            let num   = l * q64 * (new_sqrt - sqrt_p);
            let denom = sqrt_p * new_sqrt;
            if denom <= 0.0 {
                return Err(ArbError::Invariant("clmm: non-positive sqrt_price product"));
            }
            num / denom
        };
        let max_in_tick_out = (l * q64 / sqrt_p - leave_units).max(0.0);
        if max_move_ratio > 0.0 && (new_sqrt - sqrt_p) / sqrt_p > max_move_ratio {
            single_tick_out.min(max_in_tick_out)
        } else {
            single_tick_out
        }
    };

    if !out_f.is_finite() || out_f < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out_f > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out_f as u64)
}

/// Max input that keeps the swap within `max_tick_move` ticks of current
/// sqrt_price — analogous to `arb_dex_orca::max_safe_input`. See that fn's
/// docstring for the derivation. Returns 0 for unusable pools.
pub fn max_safe_input(
    sqrt_price_x64:    u128,
    liquidity:         u128,
    direction_0_to_1:  bool,
    tick_spacing:      u16,
    max_tick_move:     i32,
) -> u64 {
    if sqrt_price_x64 == 0 || liquidity == 0 { return 0; }
    let q64    = 18_446_744_073_709_551_616.0_f64;
    let sqrt_p = sqrt_price_x64 as f64;
    let l      = liquidity as f64;
    let total_ticks = max_tick_move.saturating_mul(tick_spacing as i32);
    let ratio = 1.0001_f64.powi(total_ticks).sqrt();
    let max_in_f = if direction_0_to_1 {
        l * (ratio - 1.0) * q64 / sqrt_p
    } else {
        l * sqrt_p * (ratio - 1.0) / q64
    };
    if !max_in_f.is_finite() || max_in_f <= 0.0 { return 0; }
    if max_in_f > u64::MAX as f64 { return u64::MAX; }
    max_in_f as u64
}

// --------------------------------------------------------------------------
// Quoter
// --------------------------------------------------------------------------

pub struct RaydiumClmmQuoter {
    fees: Arc<ClmmFeeStore>,
}

impl RaydiumClmmQuoter {
    pub fn new(fees: Arc<ClmmFeeStore>) -> Self { Self { fees } }
}

#[async_trait::async_trait]
impl DexQuoter for RaydiumClmmQuoter {
    fn dex_id(&self) -> &'static str { PoolKind::RaydiumClmm.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = RaydiumClmmData::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "raydium-clmm",
                reason: format!("expected {RAYDIUM_CLMM_ACCOUNT_SIZE} bytes, got {}", pool.state.len()),
            })?;

        let direction_0_to_1 = match input.mint.0 {
            x if x == parsed.token_mint_0 => true,
            x if x == parsed.token_mint_1 => false,
            other => return Err(ArbError::MintMismatch {
                expected: parsed.token_mint_0,
                actual:   other,
            }),
        };

        // No fee tier known yet — fail the route gracefully (caller skips).
        let fee_rate = self.fees.get(&pool.address).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount }
        )?;

        let out_amount = spot_quote(
            input.amount,
            parsed.sqrt_price_x64,
            fee_rate,
            direction_0_to_1,
            parsed.mint_decimals_0,
            parsed.mint_decimals_1,
        )?;

        let (out_mint, out_decimals) = if direction_0_to_1 {
            (parsed.token_mint_1, parsed.mint_decimals_1)
        } else {
            (parsed.token_mint_0, parsed.mint_decimals_0)
        };
        Ok(TokenAmount::new(TokenMint(out_mint), out_amount, out_decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        Err(ArbError::NotImplemented("Raydium CLMM swap_ix — Phase 12"))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = RaydiumClmmData::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::RaydiumClmm,
            TokenMint(parsed.token_mint_0),
            TokenMint(parsed.token_mint_1),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn fake_pool(amm_config: Pubkey, mint_0: Pubkey, mint_1: Pubkey,
                 vault_0: Pubkey, vault_1: Pubkey,
                 dec_0: u8, dec_1: u8,
                 sqrt_price_x64: u128, liquidity: u128) -> Vec<u8> {
        let mut data = vec![0u8; RAYDIUM_CLMM_ACCOUNT_SIZE];
        data[CL_OFF_AMM_CONFIG..CL_OFF_AMM_CONFIG + 32].copy_from_slice(&amm_config.to_bytes());
        data[CL_OFF_TOKEN_MINT_0..CL_OFF_TOKEN_MINT_0 + 32].copy_from_slice(&mint_0.to_bytes());
        data[CL_OFF_TOKEN_MINT_1..CL_OFF_TOKEN_MINT_1 + 32].copy_from_slice(&mint_1.to_bytes());
        data[CL_OFF_TOKEN_VAULT_0..CL_OFF_TOKEN_VAULT_0 + 32].copy_from_slice(&vault_0.to_bytes());
        data[CL_OFF_TOKEN_VAULT_1..CL_OFF_TOKEN_VAULT_1 + 32].copy_from_slice(&vault_1.to_bytes());
        data[CL_OFF_DECIMALS_0] = dec_0;
        data[CL_OFF_DECIMALS_1] = dec_1;
        data[CL_OFF_LIQUIDITY..CL_OFF_LIQUIDITY + 16].copy_from_slice(&liquidity.to_le_bytes());
        data[CL_OFF_SQRT_PRICE..CL_OFF_SQRT_PRICE + 16].copy_from_slice(&sqrt_price_x64.to_le_bytes());
        data
    }

    fn fake_amm_config(trade_fee_rate: u32) -> Vec<u8> {
        let mut data = vec![0u8; AMMCONFIG_OFF_TRADE_FEE_RATE + 8];
        data[AMMCONFIG_OFF_TRADE_FEE_RATE..AMMCONFIG_OFF_TRADE_FEE_RATE + 4]
            .copy_from_slice(&trade_fee_rate.to_le_bytes());
        data
    }

    const SQRT_PRICE_FOR_1_0: u128 = 18_446_744_073_709_551_616;

    #[test]
    fn parse_valid_clmm_account() {
        let amm  = Pubkey::new_from_array([5u8; 32]);
        let m0   = Pubkey::new_from_array([1u8; 32]);
        let m1   = Pubkey::new_from_array([2u8; 32]);
        let v0   = Pubkey::new_from_array([3u8; 32]);
        let v1   = Pubkey::new_from_array([4u8; 32]);
        let data = fake_pool(amm, m0, m1, v0, v1, 6, 6, SQRT_PRICE_FOR_1_0, 1_000_000);
        let p = RaydiumClmmData::try_parse(&data).unwrap();
        assert_eq!(p.amm_config, amm);
        assert_eq!(p.token_mint_0, m0);
        assert_eq!(p.token_mint_1, m1);
        assert_eq!(p.token_vault_0, v0);
        assert_eq!(p.token_vault_1, v1);
        assert_eq!(p.mint_decimals_0, 6);
        assert_eq!(p.mint_decimals_1, 6);
        assert_eq!(p.sqrt_price_x64, SQRT_PRICE_FOR_1_0);
        assert_eq!(p.liquidity, 1_000_000);
    }

    #[test]
    fn parse_rejects_wrong_size() {
        assert!(RaydiumClmmData::try_parse(&[0u8; 100]).is_none());
        assert!(RaydiumClmmData::try_parse(&[0u8; 1543]).is_none());
        assert!(RaydiumClmmData::try_parse(&[0u8; 1545]).is_none());
    }

    #[test]
    fn parse_amm_config_trade_fee() {
        let cfg = fake_amm_config(500);
        assert_eq!(parse_trade_fee_rate(&cfg), Some(500));
    }

    #[test]
    fn spot_quote_at_price_1_subtracts_only_fee() {
        // price=1, 0.05% fee (500 ppm): 1_000_000 in → ~999_500 out
        let out = spot_quote(1_000_000, SQRT_PRICE_FOR_1_0, 500, true, 6, 6).unwrap();
        assert!((out as i64 - 999_500).abs() <= 1, "got {out}");
    }

    #[test]
    fn spot_quote_zero_input_zero_output() {
        let out = spot_quote(0, SQRT_PRICE_FOR_1_0, 100, true, 6, 6).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn spot_quote_zero_sqrt_price_returns_insufficient() {
        let r = spot_quote(1_000, 0, 100, true, 6, 6);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn spot_quote_round_trip_within_2x_fee() {
        let in0   = 1_000_000;
        let out1  = spot_quote(in0, SQRT_PRICE_FOR_1_0, 500, true,  6, 6).unwrap();
        let back0 = spot_quote(out1, SQRT_PRICE_FOR_1_0, 500, false, 6, 6).unwrap();
        // Round-trip loses ~0.1% to 2× fee.
        assert!(back0 < in0);
        assert!((in0 as i64 - back0 as i64) < (in0 as i64) / 100);
    }

    #[test]
    fn fee_store_round_trip() {
        let s = ClmmFeeStore::new();
        let p = Pubkey::new_from_array([7u8; 32]);
        s.upsert(p, 500);
        assert_eq!(s.get(&p), Some(500));
    }

    #[test]
    fn quoter_returns_insufficient_liquidity_when_fee_unknown() {
        let fees = ClmmFeeStore::new();
        let q = RaydiumClmmQuoter::new(Arc::clone(&fees));
        let m0 = Pubkey::new_from_array([1u8; 32]);
        let m1 = Pubkey::new_from_array([2u8; 32]);
        let data = fake_pool(Pubkey::default(), m0, m1, Pubkey::default(), Pubkey::default(), 6, 6, SQRT_PRICE_FOR_1_0, 1);
        let pool = q.try_parse_account(&Pubkey::new_from_array([9u8; 32]), &data).unwrap();
        let amt = TokenAmount::new(TokenMint(m0), 1_000_000, 6);
        let r = q.quote(&pool, amt);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn quoter_quotes_with_fee_set() {
        let fees = ClmmFeeStore::new();
        let q = RaydiumClmmQuoter::new(Arc::clone(&fees));
        let m0 = Pubkey::new_from_array([1u8; 32]);
        let m1 = Pubkey::new_from_array([2u8; 32]);
        let pool_addr = Pubkey::new_from_array([9u8; 32]);
        let data = fake_pool(Pubkey::default(), m0, m1, Pubkey::default(), Pubkey::default(), 6, 6, SQRT_PRICE_FOR_1_0, 1);
        let pool = q.try_parse_account(&pool_addr, &data).unwrap();
        fees.upsert(pool_addr, 500);
        let amt = TokenAmount::new(TokenMint(m0), 1_000_000, 6);
        let out = q.quote(&pool, amt).unwrap();
        assert_eq!(out.mint.0, m1);
        assert!((out.amount as i64 - 999_500).abs() <= 1);
    }

    #[test]
    fn quoter_rejects_unknown_mint() {
        let fees = ClmmFeeStore::new();
        let q = RaydiumClmmQuoter::new(Arc::clone(&fees));
        let m0 = Pubkey::new_from_array([1u8; 32]);
        let m1 = Pubkey::new_from_array([2u8; 32]);
        let m2 = Pubkey::new_from_array([7u8; 32]);
        let pool_addr = Pubkey::new_from_array([9u8; 32]);
        let data = fake_pool(Pubkey::default(), m0, m1, Pubkey::default(), Pubkey::default(), 6, 6, SQRT_PRICE_FOR_1_0, 1);
        let pool = q.try_parse_account(&pool_addr, &data).unwrap();
        fees.upsert(pool_addr, 500);
        let amt = TokenAmount::new(TokenMint(m2), 1, 6);
        assert!(matches!(q.quote(&pool, amt), Err(ArbError::MintMismatch { .. })));
    }
}
