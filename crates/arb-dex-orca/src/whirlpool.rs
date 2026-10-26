//! Orca Whirlpool quoter.
//!
//! ## Account layout (Anchor `Whirlpool` account, total ~653 bytes)
//!
//! Offsets are relative to start of `data` and assume the standard 8-byte
//! Anchor discriminator. Source: orca-so/whirlpools `state/whirlpool.rs`.
//!
//! | Field                | Offset | Type   |
//! |----------------------|--------|--------|
//! | (Anchor discriminator) |   0  | [u8;8] |
//! | whirlpools_config    |   8    | Pubkey |
//! | whirlpool_bump       |  40    | [u8;1] |
//! | tick_spacing         |  41    | u16    |
//! | tick_spacing_seed    |  43    | [u8;2] |
//! | fee_rate             |  45    | u16    | (hundredths of a bp; 100 = 0.01%)
//! | protocol_fee_rate    |  47    | u16    |
//! | liquidity            |  49    | u128   |
//! | sqrt_price           |  65    | u128   | (Q64.64 fixed-point)
//! | tick_current_index   |  81    | i32    |
//! | protocol_fee_owed_a  |  85    | u64    |
//! | protocol_fee_owed_b  |  93    | u64    |
//! | token_mint_a         | 101    | Pubkey |
//! | token_vault_a        | 133    | Pubkey |
//! | fee_growth_global_a  | 165    | u128   |
//! | token_mint_b         | 181    | Pubkey |
//! | token_vault_b        | 213    | Pubkey |
//!
//! ## ⚠️ v1 quote: **spot-price approximation**, not full tick-walking math
//!
//! Implementing the full Uniswap-V3-style swap (tick traversal across
//! `TickArray` PDAs) requires fetching extra accounts the scout's WSS
//! subscriber doesn't currently track. To unblock Phase −1, this quoter
//! returns an **infinite-depth spot-price approximation**:
//!
//! ```text
//! price        = (sqrt_price / 2^64) ^ 2
//! output       = floor(input * price * (1 − fee_rate / 1_000_000))
//! ```
//!
//! For SMALL swaps within the current tick, this is close to truth (within a
//! few bps). For LARGE swaps (which would cross ticks and pay more slippage)
//! it OVERESTIMATES output. Acceptable for opportunity *detection* — the
//! relative price between two pools is what surfaces an arb. NOT acceptable
//! for production execution; the full tick-walking implementation is a
//! follow-up before Phase 5B integration tests.

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::ORCA_WHIRLPOOL_PROGRAM;

pub const WHIRLPOOL_DISCRIMINATOR: [u8; 8] = [63, 149, 209, 12, 225, 128, 99, 9];
pub const WHIRLPOOL_ACCOUNT_SIZE_MIN: usize = 245 + 32 + 16; // up to fee_growth_global_b

const O_TICK_SPACING:    usize = 41;
const O_FEE_RATE:        usize = 45;
const O_LIQUIDITY:       usize = 49;
const O_SQRT_PRICE:      usize = 65;
const O_TICK_CURRENT:    usize = 81;
const O_TOKEN_MINT_A:    usize = 101;
const O_TOKEN_VAULT_A:   usize = 133;
const O_TOKEN_MINT_B:    usize = 181;
const O_TOKEN_VAULT_B:   usize = 213;

#[derive(Debug, Clone, Copy)]
pub struct WhirlpoolData {
    pub tick_spacing:       u16,
    pub fee_rate:           u16,
    pub liquidity:          u128,
    pub sqrt_price:         u128,
    pub tick_current_index: i32,
    pub token_mint_a:       Pubkey,
    pub token_vault_a:      Pubkey,
    pub token_mint_b:       Pubkey,
    pub token_vault_b:      Pubkey,
}

impl WhirlpoolData {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() < WHIRLPOOL_ACCOUNT_SIZE_MIN { return None; }
        if data[..8] != WHIRLPOOL_DISCRIMINATOR { return None; }

        let read_u16  = |o: usize| u16::from_le_bytes([data[o], data[o+1]]);
        let read_u128 = |o: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&data[o..o+16]);
            u128::from_le_bytes(b)
        };
        let read_i32 = |o: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&data[o..o+4]);
            i32::from_le_bytes(b)
        };
        let read_pk  = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o+32]);
            Pubkey::new_from_array(b)
        };

        Some(WhirlpoolData {
            tick_spacing:       read_u16(O_TICK_SPACING),
            fee_rate:           read_u16(O_FEE_RATE),
            liquidity:          read_u128(O_LIQUIDITY),
            sqrt_price:         read_u128(O_SQRT_PRICE),
            tick_current_index: read_i32(O_TICK_CURRENT),
            token_mint_a:       read_pk(O_TOKEN_MINT_A),
            token_vault_a:      read_pk(O_TOKEN_VAULT_A),
            token_mint_b:       read_pk(O_TOKEN_MINT_B),
            token_vault_b:      read_pk(O_TOKEN_VAULT_B),
        })
    }
}

/// Constant-price spot quote — kept for backward compatibility with the
/// scout's dispatcher. Production paths should use `swap_quote` (with the
/// pool's `liquidity` field) for execution-grade math that includes
/// in-tick price impact.
pub fn spot_quote(
    input:            u64,
    sqrt_price_q64:   u128,
    fee_rate_hundredths_bp: u16,   // 100 = 0.01% = 1bp; 3000 = 0.30%
    direction_a_to_b: bool,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if sqrt_price_q64 == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: input,
        });
    }

    let sqrt_price_f64 = (sqrt_price_q64 as f64) / 18_446_744_073_709_551_616.0; // 2^64
    let price          = sqrt_price_f64 * sqrt_price_f64; // B per A
    let fee_factor     = 1.0 - (fee_rate_hundredths_bp as f64) / 1_000_000.0;
    let input_f64      = input as f64;

    let out_f64 = if direction_a_to_b {
        input_f64 * price * fee_factor
    } else {
        if price <= 0.0 {
            return Err(ArbError::Invariant("non-positive price"));
        }
        input_f64 / price * fee_factor
    };

    if !out_f64.is_finite() || out_f64 < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out_f64 > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out_f64 as u64)
}

/// Whirlpool single-tick swap quote with proper price-impact math.
///
/// Models the Uniswap V3 invariant `x * y = L^2` within the active tick:
/// as input is added, sqrt_price moves AWAY from the entry edge so the
/// average fill price is WORSE than the spot price.
///
/// Formula (a→b, price decreasing — adding token_a):
/// ```text
///   new_sqrt_price = L * sqrt_price / (L + amount_in_after_fee * sqrt_price / 2^64)
///   amount_out     = L * (sqrt_price - new_sqrt_price) / 2^64
/// ```
/// (b→a is symmetric: `new_sqrt = sqrt + amount_in_after_fee * 2^64 / L`,
/// and `amount_out` derived from `x = L / sqrt_price`.)
///
/// SINGLE-TICK approximation: ignores ticks that the swap would cross.
/// For deep pools and small swaps the active tick holds enough liquidity
/// to cover the trade, so the formula is exact. For oversized swaps it
/// overshoots; the executor's `min_out` + on-chain enforcement catches
/// that case (revert with no principal loss, no Jito tip charged).
///
/// Computed in `f64` rather than Q64.128 fixed-point. Relative precision
/// loss is < 1bp for typical inputs (< $10k) — well below the 50bp
/// slippage budget the executor applies on top.
pub fn swap_quote(
    input:            u64,
    sqrt_price_q64:   u128,
    liquidity:        u128,
    fee_rate_hundredths_bp: u16,   // 100 = 0.01% = 1bp; 3000 = 0.30%
    direction_a_to_b: bool,
    tick_spacing:     u16,         // pool's tick_spacing for the per-leg tick cap
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if sqrt_price_q64 == 0 || liquidity == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: input,
        });
    }

    let q64    = 18_446_744_073_709_551_616.0_f64; // 2^64
    let sqrt_p = sqrt_price_q64 as f64;
    let l      = liquidity as f64;

    let fee_factor      = 1.0 - (fee_rate_hundredths_bp as f64) / 1_000_000.0;
    let input_after_fee = (input as f64) * fee_factor;

    // v47 policy ("leave 1 unit in the pool"): when the swap would drain
    // the in-tick liquidity beyond what single-tick math can describe,
    // cap the quoted output at the in-tick MAXIMUM minus a small safety
    // (`WHIRLPOOL_LEAVE_UNITS`, default 1 base unit). This is what the pool
    // can ACTUALLY deliver — so the on-chain swap's min_out check passes
    // even at saturation. The cascade sees a realistic small output, and
    // shallow-pool routes that aren't profitable fail the below_min gate
    // (correctly), while routes with real depth proceed normally.
    //
    // `max_in_tick_out` = L * sqrt_p / Q (a_to_b) or L * Q / sqrt_p (b_to_a):
    // the asymptotic limit of single-tick math.
    // v55: bumped default from 1 → 2. Operator wants the pool to "profit
    // from us" by leaving one extra unit beyond what the math strictly
    // needs to avoid asking for the impossible. Lowers our quoted output
    // and thus the IX's min_out, making the on-chain swap even more
    // tolerant of routing / rounding drift.
    let leave_units: f64 = std::env::var("WHIRLPOOL_LEAVE_UNITS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(2.0);

    let max_ticks_per_leg: i32 = std::env::var("WHIRLPOOL_QUOTE_MAX_TICKS_PER_LEG")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    let allowed_tick_move = max_ticks_per_leg.saturating_mul(tick_spacing as i32);
    let allowed_sqrt_ratio = 1.0001_f64.powi(allowed_tick_move).sqrt();
    let max_move_ratio = (allowed_sqrt_ratio - 1.0).abs();

    let out_f64 = if direction_a_to_b {
        // Adding token_a → sqrt_price decreases.
        let denom = l + input_after_fee * sqrt_p / q64;
        if denom <= 0.0 {
            return Err(ArbError::Invariant("whirlpool: zero denominator"));
        }
        let new_sqrt = l * sqrt_p / denom;
        if new_sqrt > sqrt_p {
            return Ok(0); // numerical drift; treat as zero output
        }
        let single_tick_out = l * (sqrt_p - new_sqrt) / q64;
        // Saturation cap: max possible output for a_to_b is L * sqrt_p / Q
        // (the asymptote as input → ∞ in single-tick math).
        let max_in_tick_out = (l * sqrt_p / q64 - leave_units).max(0.0);
        if max_move_ratio > 0.0 && (sqrt_p - new_sqrt) / sqrt_p > max_move_ratio {
            // Beyond safe window → cap to what the pool can actually give.
            single_tick_out.min(max_in_tick_out)
        } else {
            single_tick_out
        }
    } else {
        // Adding token_b → sqrt_price increases.
        let new_sqrt = sqrt_p + input_after_fee * q64 / l;
        let single_tick_out = {
            let num   = l * q64 * (new_sqrt - sqrt_p);
            let denom = sqrt_p * new_sqrt;
            if denom <= 0.0 {
                return Err(ArbError::Invariant("whirlpool: non-positive sqrt_price product"));
            }
            num / denom
        };
        // For b_to_a, max output asymptotically approaches L * Q / sqrt_p
        // (the x = L / sqrt_p reserve at current price).
        let max_in_tick_out = (l * q64 / sqrt_p - leave_units).max(0.0);
        if max_move_ratio > 0.0 && (new_sqrt - sqrt_p) / sqrt_p > max_move_ratio {
            single_tick_out.min(max_in_tick_out)
        } else {
            single_tick_out
        }
    };

    if !out_f64.is_finite() || out_f64 < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out_f64 > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out_f64 as u64)
}

#[derive(Default)]
pub struct OrcaWhirlpoolQuoter;

impl OrcaWhirlpoolQuoter {
    pub fn new() -> Self { Self }
}

#[async_trait::async_trait]
impl DexQuoter for OrcaWhirlpoolQuoter {
    fn dex_id(&self) -> &'static str { PoolKind::OrcaWhirlpool.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = WhirlpoolData::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "orca-whirlpool",
                reason: "discriminator mismatch or short data".into(),
            })?;

        let direction_a_to_b = match input.mint.0 {
            x if x == parsed.token_mint_a => true,
            x if x == parsed.token_mint_b => false,
            other => return Err(ArbError::MintMismatch {
                expected: parsed.token_mint_a,
                actual:   other,
            }),
        };

        let out_amount = spot_quote(
            input.amount,
            parsed.sqrt_price,
            parsed.fee_rate,
            direction_a_to_b,
        )?;

        let out_mint = if direction_a_to_b { parsed.token_mint_b } else { parsed.token_mint_a };
        // Decimals: caller is responsible for tracking actual mint decimals; we
        // re-use input.decimals as a rough proxy. The scout's USDC/SOL flows
        // know the right values; this is fine for v1.
        Ok(TokenAmount::new(TokenMint(out_mint), out_amount, input.decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        Err(ArbError::NotImplemented("Orca swap_ix builder — Phase 5B"))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = WhirlpoolData::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::OrcaWhirlpool,
            TokenMint(parsed.token_mint_a),
            TokenMint(parsed.token_mint_b),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

/// Maximum input that keeps the swap inside `max_tick_move` ticks of the
/// current sqrt_price — i.e., the largest trade we can quote accurately with
/// single-tick math. Used by the executor to scale the probe per pool: deep
/// pools accept the full env-configured probe, shallow pools get a fraction
/// that fits their in-tick depth.
///
/// Formula (derive from swap_quote's `new_sqrt = L * sqrt_p / (L + in * sqrt_p / Q)`):
///   * a_to_b (sqrt decreasing): set `new_sqrt = sqrt_p / r` where
///     r = sqrt(1.0001^max_tick_move). Solving: `in = L * (r - 1) * Q / sqrt_p`.
///   * b_to_a (sqrt increasing): set `new_sqrt = sqrt_p * r`. Solving:
///     `in = L * sqrt_p * (r - 1) / Q`.
///
/// Returns 0 when the pool is unusable (zero liquidity / sqrt_price), so the
/// caller can skip the route without panicking.
pub fn max_safe_input(
    sqrt_price_q64:   u128,
    liquidity:        u128,
    direction_a_to_b: bool,
    tick_spacing:     u16,
    max_tick_move:    i32,
) -> u64 {
    if sqrt_price_q64 == 0 || liquidity == 0 { return 0; }
    let q64    = 18_446_744_073_709_551_616.0_f64;
    let sqrt_p = sqrt_price_q64 as f64;
    let l      = liquidity as f64;
    let total_ticks = max_tick_move.saturating_mul(tick_spacing as i32);
    let ratio = 1.0001_f64.powi(total_ticks).sqrt(); // sqrt_price ratio across window
    let max_in_f = if direction_a_to_b {
        l * (ratio - 1.0) * q64 / sqrt_p
    } else {
        l * sqrt_p * (ratio - 1.0) / q64
    };
    if !max_in_f.is_finite() || max_in_f <= 0.0 { return 0; }
    if max_in_f > u64::MAX as f64 { return u64::MAX; }
    max_in_f as u64
}

pub fn is_orca_whirlpool(program_id: &Pubkey) -> bool {
    *program_id == ORCA_WHIRLPOOL_PROGRAM
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_whirlpool(fee_rate: u16, liquidity: u128, sqrt_price: u128,
                      mint_a: Pubkey, mint_b: Pubkey) -> Vec<u8> {
        let mut data = vec![0u8; 653];
        data[..8].copy_from_slice(&WHIRLPOOL_DISCRIMINATOR);
        data[O_FEE_RATE..O_FEE_RATE+2].copy_from_slice(&fee_rate.to_le_bytes());
        data[O_LIQUIDITY..O_LIQUIDITY+16].copy_from_slice(&liquidity.to_le_bytes());
        data[O_SQRT_PRICE..O_SQRT_PRICE+16].copy_from_slice(&sqrt_price.to_le_bytes());
        data[O_TICK_CURRENT..O_TICK_CURRENT+4].copy_from_slice(&0i32.to_le_bytes());
        data[O_TOKEN_MINT_A..O_TOKEN_MINT_A+32].copy_from_slice(&mint_a.to_bytes());
        data[O_TOKEN_VAULT_A..O_TOKEN_VAULT_A+32].copy_from_slice(&[0u8; 32]);
        data[O_TOKEN_MINT_B..O_TOKEN_MINT_B+32].copy_from_slice(&mint_b.to_bytes());
        data[O_TOKEN_VAULT_B..O_TOKEN_VAULT_B+32].copy_from_slice(&[0u8; 32]);
        data
    }

    /// `sqrt_price_q64` for price = 1.0 (1 B per A): 2^64.
    const SQRT_PRICE_FOR_1_0: u128 = 18_446_744_073_709_551_616;

    #[test]
    fn parse_valid_whirlpool_account() {
        let mint_a = Pubkey::new_from_array([1u8; 32]);
        let mint_b = Pubkey::new_from_array([2u8; 32]);
        let data   = fake_whirlpool(3000, 1_000_000_000_000, SQRT_PRICE_FOR_1_0, mint_a, mint_b);
        let p = WhirlpoolData::try_parse(&data).unwrap();
        assert_eq!(p.fee_rate, 3000);
        assert_eq!(p.sqrt_price, SQRT_PRICE_FOR_1_0);
        assert_eq!(p.token_mint_a, mint_a);
        assert_eq!(p.token_mint_b, mint_b);
    }

    #[test]
    fn parse_rejects_wrong_discriminator() {
        let mut data = fake_whirlpool(3000, 1, SQRT_PRICE_FOR_1_0, Pubkey::default(), Pubkey::default());
        data[0] = 99; // garble disc
        assert!(WhirlpoolData::try_parse(&data).is_none());
    }

    #[test]
    fn parse_rejects_short_data() {
        let data = vec![0u8; 50];
        assert!(WhirlpoolData::try_parse(&data).is_none());
    }

    #[test]
    fn spot_quote_at_price_1_subtracts_only_fee() {
        // 1.0 price, 0.30% fee, A→B: 1_000_000 in → ~997_000 out.
        let out = spot_quote(1_000_000, SQRT_PRICE_FOR_1_0, 3000, true).unwrap();
        // 1 - 3000/1_000_000 = 0.997 → output ~997_000. Allow 1-unit rounding.
        assert!((out as i64 - 997_000).abs() <= 1, "got {out}");
    }

    #[test]
    fn spot_quote_zero_input_zero_output() {
        let out = spot_quote(0, SQRT_PRICE_FOR_1_0, 3000, true).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn spot_quote_zero_sqrt_price_returns_insufficient() {
        let r = spot_quote(1_000, 0, 3000, true);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn spot_quote_b_to_a_is_inverse_minus_fees() {
        // Round-trip through both directions returns within 2*fee of input.
        let in_a    = 1_000_000_u64;
        let out_b   = spot_quote(in_a,  SQRT_PRICE_FOR_1_0, 3000, true).unwrap();
        let back_a  = spot_quote(out_b, SQRT_PRICE_FOR_1_0, 3000, false).unwrap();
        // Round-trip loses ~0.6% to fees. Confirm we land within 1% to allow rounding.
        assert!((in_a as i64 - back_a as i64).abs() < (in_a as i64) / 100);
        assert!(back_a < in_a);
    }

    #[test]
    fn quoter_dispatches_correct_direction() {
        let mint_a = Pubkey::new_from_array([1u8; 32]);
        let mint_b = Pubkey::new_from_array([2u8; 32]);
        let data   = fake_whirlpool(3000, 1_000_000_000_000, SQRT_PRICE_FOR_1_0, mint_a, mint_b);
        let pool   = OrcaWhirlpoolQuoter::new()
            .try_parse_account(&Pubkey::new_unique(), &data).unwrap();
        let quoter = OrcaWhirlpoolQuoter::new();

        let in_a = TokenAmount::new(TokenMint(mint_a), 1_000_000, 6);
        let out  = quoter.quote(&pool, in_a).unwrap();
        assert_eq!(out.mint.0, mint_b, "A → B should output B");

        let in_b = TokenAmount::new(TokenMint(mint_b), 1_000_000, 6);
        let out  = quoter.quote(&pool, in_b).unwrap();
        assert_eq!(out.mint.0, mint_a, "B → A should output A");
    }

    #[test]
    fn quoter_rejects_unknown_mint() {
        let mint_a = Pubkey::new_from_array([1u8; 32]);
        let mint_b = Pubkey::new_from_array([2u8; 32]);
        let mint_c = Pubkey::new_from_array([3u8; 32]);
        let data   = fake_whirlpool(3000, 1, SQRT_PRICE_FOR_1_0, mint_a, mint_b);
        let pool   = OrcaWhirlpoolQuoter::new()
            .try_parse_account(&Pubkey::new_unique(), &data).unwrap();
        let in_c = TokenAmount::new(TokenMint(mint_c), 1, 6);
        let r = OrcaWhirlpoolQuoter::new().quote(&pool, in_c);
        assert!(matches!(r, Err(ArbError::MintMismatch { .. })));
    }
}
