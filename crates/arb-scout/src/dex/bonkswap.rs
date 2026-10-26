//! Bonkswap quoter — constant-product AMM with multi-tier fee structure.
//!
//! Identified from the example tx 5RzMmTu9... where one of the legs invoked
//! program `BSwp6bEBihVLdqJRKGgzjcGLHkcTuzmSo1TQkHepzH8p`. Account layout
//! verified against [`carbon-bonkswap-decoder`] v0.12.0.
//!
//! Program ID:  `BSwp6bEBihVLdqJRKGgzjcGLHkcTuzmSo1TQkHepzH8p`
//! Pool disc:   `[241, 154, 109, 4, 17, 177, 109, 188]`   (legacy `Pool`)
//! PoolV2 disc: `[91, 12, 214, 87, 7, 185, 167, 55]`     (newer `PoolV2`)
//! Pool size:   417 bytes (legacy)  /  ~514 bytes (V2)
//!
//! ## Layout (Pool — V2 has the same prefix + extra trailing fields)
//!
//! Borsh-serialised after the 8-byte discriminator. All offsets relative to
//! start of `data`.
//!
//! | Field                | Offset | Size |
//! |----------------------|--------|------|
//! | (discriminator)      |   0    |  8   |
//! | token_x              |   8    | 32   |
//! | token_y              |  40    | 32   |
//! | pool_x_account       |  72    | 32   | (vault, BUT reserves stored inline below)
//! | pool_y_account       | 104    | 32   |
//! | admin                | 136    | 32   |
//! | project_owner        | 168    | 32   |
//! | token_x_reserve      | 200    |  8   | u64 — reserves are IN the pool account!
//! | token_y_reserve      | 208    |  8   |
//! | (intermediate fields)| 216    | 96   | shares, buybacks, project amounts, mercanti amounts
//! | lp_accumulator_x     | 312    | 16   |
//! | lp_accumulator_y     | 328    | 16   |
//! | const_k              | 344    | 16   |
//! | price                | 360    | 16   |
//! | lp_fee               | 376    | 16   | u128 in 10^12 fixed-point
//! | buyback_fee          | 392    | 16   |
//! | project_fee          | 408    | 16   |
//! | mercanti_fee         | 424    | 16   |
//!
//! Wait — recomputed: 200+8*10=280, then +16*10=440 + ...
//! Let me list fields exactly per the Codama struct (after disc=8):
//!   token_x[32] token_y[32] pool_x_account[32] pool_y_account[32]
//!   admin[32] project_owner[32]                              (192 bytes)
//!   token_x_reserve(Token=u64)[8] token_y_reserve[8]         (16)
//!   self_shares[8] all_shares[8]                             (16)
//!   buyback_amount_x[8] buyback_amount_y[8]                  (16)
//!   project_amount_x[8] project_amount_y[8]                  (16)
//!   mercanti_amount_x[8] mercanti_amount_y[8]                (16)
//!                                                            -- subtotal 272
//!   lp_accumulator_x(FixedPoint=u128)[16] lp_accumulator_y[16] (32)
//!   const_k(Product=u128)[16]                                (16)
//!   price(FixedPoint)[16]                                    (16)
//!   lp_fee[16] buyback_fee[16] project_fee[16] mercanti_fee[16] (64)
//!                                                            -- subtotal 400
//!   farm_count(u64)[8] bump(u8)[1]                           (9)
//!                                                            == 409 bytes data
//!   + disc(8) = 417 total ✓
//!
//! Re-derived offsets (incl. discriminator):
//!   token_x:           8
//!   token_y:          40
//!   pool_x_account:   72
//!   pool_y_account:  104
//!   token_x_reserve: 200
//!   token_y_reserve: 208
//!   lp_fee:          280 + 32 + 16 + 16 = 344    Wait recompute:
//!     200 + 16(reserves) + 16(self+all shares) + 16(buyback) + 16(project) + 16(mercanti) = 280
//!     + 32 (lp_accumulator_x+y) = 312
//!     + 16 (const_k)           = 328
//!     + 16 (price)             = 344
//!     lp_fee starts at 344, size 16
//!     buyback_fee:  360
//!     project_fee:  376
//!     mercanti_fee: 392
//!
//! Bonkswap reserves are u64 in raw token base units — same as Raydium AMM v4
//! quote math, but reserves are INSIDE the pool account (no vault wiring needed).

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::Instruction;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

pub const BONKSWAP_PROGRAM: Pubkey =
    pubkey!("BSwp6bEBihVLdqJRKGgzjcGLHkcTuzmSo1TQkHepzH8p");

pub const BONKSWAP_POOL_DISCRIMINATOR:    [u8; 8] = [241, 154, 109, 4, 17, 177, 109, 188];
pub const BONKSWAP_POOL_V2_DISCRIMINATOR: [u8; 8] = [91, 12, 214, 87, 7, 185, 167, 55];
pub const BONKSWAP_POOL_SIZE:    usize = 417;
/// PoolV2 has additional trailing fields (lp_token + bump + padding[u64;8])
/// so its size is ~514 bytes. We accept any size ≥ 417 if the V2 disc
/// matches (the relevant offsets are the same as legacy Pool).
pub const BONKSWAP_POOL_V2_MIN_SIZE: usize = 417;

const B_OFF_TOKEN_X:        usize = 8;
const B_OFF_TOKEN_Y:        usize = 40;
const B_OFF_VAULT_X:        usize = 72;
const B_OFF_VAULT_Y:        usize = 104;
const B_OFF_RESERVE_X:      usize = 200;
const B_OFF_RESERVE_Y:      usize = 208;
const B_OFF_LP_FEE:         usize = 344;
const B_OFF_BUYBACK_FEE:    usize = 360;
const B_OFF_PROJECT_FEE:    usize = 376;
const B_OFF_MERCANTI_FEE:   usize = 392;

/// Bonkswap's `FixedPoint` represents `value × 10^12` (verified by manual
/// inspection of mainnet pools — typical lp_fee values like 0.3% map to
/// `3_000_000_000` raw, which is 0.003 × 10^12).
pub const BONKSWAP_FIXED_POINT_SCALE: u128 = 1_000_000_000_000;

#[derive(Debug, Clone, Copy)]
pub struct BonkswapPoolData {
    pub token_x:         Pubkey,
    pub token_y:         Pubkey,
    pub vault_x:         Pubkey,
    pub vault_y:         Pubkey,
    pub reserve_x:       u64,
    pub reserve_y:       u64,
    pub lp_fee_raw:      u128,
    pub buyback_fee_raw: u128,
    pub project_fee_raw: u128,
    pub mercanti_fee_raw: u128,
}

impl BonkswapPoolData {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() < 8 { return None; }
        let disc = &data[..8];
        let valid = disc == BONKSWAP_POOL_DISCRIMINATOR
            || disc == BONKSWAP_POOL_V2_DISCRIMINATOR;
        if !valid { return None; }
        if data.len() < BONKSWAP_POOL_SIZE { return None; }

        let read_pk = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o + 32]);
            Pubkey::new_from_array(b)
        };
        let read_u64 = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[o..o + 8]);
            u64::from_le_bytes(b)
        };
        let read_u128 = |o: usize| {
            let mut b = [0u8; 16];
            b.copy_from_slice(&data[o..o + 16]);
            u128::from_le_bytes(b)
        };

        Some(BonkswapPoolData {
            token_x:          read_pk(B_OFF_TOKEN_X),
            token_y:          read_pk(B_OFF_TOKEN_Y),
            vault_x:          read_pk(B_OFF_VAULT_X),
            vault_y:          read_pk(B_OFF_VAULT_Y),
            reserve_x:        read_u64(B_OFF_RESERVE_X),
            reserve_y:        read_u64(B_OFF_RESERVE_Y),
            lp_fee_raw:       read_u128(B_OFF_LP_FEE),
            buyback_fee_raw:  read_u128(B_OFF_BUYBACK_FEE),
            project_fee_raw:  read_u128(B_OFF_PROJECT_FEE),
            mercanti_fee_raw: read_u128(B_OFF_MERCANTI_FEE),
        })
    }

    /// Total fee fraction (lp + buyback + project + mercanti). Returns 0.003
    /// (0.3%) for typical pools.
    pub fn total_fee_fraction(&self) -> f64 {
        let total = self.lp_fee_raw
            .saturating_add(self.buyback_fee_raw)
            .saturating_add(self.project_fee_raw)
            .saturating_add(self.mercanti_fee_raw);
        (total as f64) / (BONKSWAP_FIXED_POINT_SCALE as f64)
    }
}

/// Constant-product quote with reserves (in u128 to avoid overflow) and a
/// fractional fee.
pub fn quote_constant_product_with_fee(
    input:        u64,
    reserve_in:   u64,
    reserve_out:  u64,
    fee_fraction: f64,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(), amount: input,
        });
    }
    if !(0.0..=1.0).contains(&fee_fraction) {
        return Err(ArbError::Invariant("Bonkswap fee_fraction outside [0,1]"));
    }

    let amount_in_after_fee = (input as f64) * (1.0 - fee_fraction);
    let r_in   = reserve_in  as f64;
    let r_out  = reserve_out as f64;
    let out    = amount_in_after_fee * r_out / (r_in + amount_in_after_fee);

    if !out.is_finite() || out < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out as u64)
}

#[derive(Default)]
pub struct BonkswapQuoter;

impl BonkswapQuoter {
    pub fn new() -> Self { Self }
}

#[async_trait::async_trait]
impl DexQuoter for BonkswapQuoter {
    fn dex_id(&self) -> &'static str { PoolKind::Bonkswap.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = BonkswapPoolData::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "bonkswap",
                reason: "discriminator mismatch or short data".into(),
            })?;

        let direction_x_to_y = match input.mint.0 {
            x if x == parsed.token_x => true,
            x if x == parsed.token_y => false,
            other => return Err(ArbError::MintMismatch {
                expected: parsed.token_x, actual: other,
            }),
        };

        let (reserve_in, reserve_out, out_mint) = if direction_x_to_y {
            (parsed.reserve_x, parsed.reserve_y, parsed.token_y)
        } else {
            (parsed.reserve_y, parsed.reserve_x, parsed.token_x)
        };

        let fee = parsed.total_fee_fraction();
        let out_amount = quote_constant_product_with_fee(input.amount, reserve_in, reserve_out, fee)?;
        Ok(TokenAmount::new(TokenMint(out_mint), out_amount, input.decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        Err(ArbError::NotImplemented("Bonkswap swap_ix — Phase 12"))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = BonkswapPoolData::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::Bonkswap,
            TokenMint(parsed.token_x),
            TokenMint(parsed.token_y),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

pub fn is_bonkswap(program_id: &Pubkey) -> bool {
    *program_id == BONKSWAP_PROGRAM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn fake_pool(
        token_x: Pubkey, token_y: Pubkey,
        vault_x: Pubkey, vault_y: Pubkey,
        reserve_x: u64, reserve_y: u64,
        lp_fee_raw: u128, buyback: u128, project: u128, mercanti: u128,
    ) -> Vec<u8> {
        let mut data = vec![0u8; BONKSWAP_POOL_SIZE];
        data[..8].copy_from_slice(&BONKSWAP_POOL_DISCRIMINATOR);
        data[B_OFF_TOKEN_X..B_OFF_TOKEN_X + 32].copy_from_slice(&token_x.to_bytes());
        data[B_OFF_TOKEN_Y..B_OFF_TOKEN_Y + 32].copy_from_slice(&token_y.to_bytes());
        data[B_OFF_VAULT_X..B_OFF_VAULT_X + 32].copy_from_slice(&vault_x.to_bytes());
        data[B_OFF_VAULT_Y..B_OFF_VAULT_Y + 32].copy_from_slice(&vault_y.to_bytes());
        data[B_OFF_RESERVE_X..B_OFF_RESERVE_X + 8].copy_from_slice(&reserve_x.to_le_bytes());
        data[B_OFF_RESERVE_Y..B_OFF_RESERVE_Y + 8].copy_from_slice(&reserve_y.to_le_bytes());
        data[B_OFF_LP_FEE..B_OFF_LP_FEE + 16].copy_from_slice(&lp_fee_raw.to_le_bytes());
        data[B_OFF_BUYBACK_FEE..B_OFF_BUYBACK_FEE + 16].copy_from_slice(&buyback.to_le_bytes());
        data[B_OFF_PROJECT_FEE..B_OFF_PROJECT_FEE + 16].copy_from_slice(&project.to_le_bytes());
        data[B_OFF_MERCANTI_FEE..B_OFF_MERCANTI_FEE + 16].copy_from_slice(&mercanti.to_le_bytes());
        data
    }

    #[test]
    fn parse_valid_pool() {
        let tx = Pubkey::new_from_array([1u8; 32]);
        let ty = Pubkey::new_from_array([2u8; 32]);
        // FixedPoint scale = 10^12, so 0.2% = 0.002 × 10^12 = 2_000_000_000.
        // Building a pool with lp 0.2% + buyback 0.05% + project 0.05% + mercanti 0
        // → total 0.3% = 0.003 × 10^12 = 3_000_000_000.
        let data = fake_pool(tx, ty, Pubkey::default(), Pubkey::default(),
                              1_000_000, 100_000_000_000,
                              2_000_000_000, 500_000_000,
                              500_000_000, 0);
        let p = BonkswapPoolData::try_parse(&data).unwrap();
        assert_eq!(p.token_x, tx);
        assert_eq!(p.token_y, ty);
        assert_eq!(p.reserve_x, 1_000_000);
        assert_eq!(p.reserve_y, 100_000_000_000);
        assert!((p.total_fee_fraction() - 0.003).abs() < 1e-9, "got {}", p.total_fee_fraction());
    }

    #[test]
    fn parse_rejects_wrong_discriminator() {
        let mut data = vec![0u8; BONKSWAP_POOL_SIZE];
        // garbage discriminator
        data[0] = 99;
        assert!(BonkswapPoolData::try_parse(&data).is_none());
    }

    #[test]
    fn parse_accepts_v2_discriminator() {
        let mut data = vec![0u8; BONKSWAP_POOL_SIZE];
        data[..8].copy_from_slice(&BONKSWAP_POOL_V2_DISCRIMINATOR);
        // Other fields zero — parse should succeed (zero values are valid).
        assert!(BonkswapPoolData::try_parse(&data).is_some());
    }

    #[test]
    fn parse_rejects_short_data() {
        let data = vec![0u8; 100];
        assert!(BonkswapPoolData::try_parse(&data).is_none());
    }

    #[test]
    fn quote_constant_product_known_value() {
        // x=USDC reserves 100_000 (6dp = $0.10 worth), y=BONK reserves
        // 1_000_000_000 (raw). 0.3% fee. Input 1000 USDC (raw).
        // amount_in_after_fee = 1000 × (1 - 0.003) = 997
        // out = 997 × 1_000_000_000 / (100_000 + 997) ≈ 9_876_222
        let out = quote_constant_product_with_fee(1_000, 100_000, 1_000_000_000, 0.003).unwrap();
        // Using f64 math, expected near 9_876_222 (within rounding)
        let expected = 997.0 * 1_000_000_000.0 / (100_000.0 + 997.0);
        assert!((out as f64 - expected).abs() < 2.0, "got {out}, expected ~{expected}");
    }

    #[test]
    fn quote_zero_input() {
        let out = quote_constant_product_with_fee(0, 100, 100, 0.003).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn quote_zero_reserve_returns_insufficient() {
        let r = quote_constant_product_with_fee(100, 0, 1000, 0.003);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn quoter_full_path() {
        let q = BonkswapQuoter::new();
        let tx = Pubkey::new_from_array([1u8; 32]);
        let ty = Pubkey::new_from_array([2u8; 32]);
        // 0.3% total fee in 10^12 fixed-point → 3_000_000_000
        // Reserves 1_000_000 / 1_000_000_000.
        // amount_in_after_fee = 1_000_000 × 0.997 = 997_000
        // out = 997_000 × 1_000_000_000 / (1_000_000 + 997_000) ≈ 499_249
        let data = fake_pool(tx, ty, Pubkey::default(), Pubkey::default(),
                              1_000_000, 1_000_000_000,
                              3_000_000_000, 0, 0, 0);
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).unwrap();

        let in_x = TokenAmount::new(TokenMint(tx), 1_000_000, 6);
        let out = q.quote(&pool, in_x).unwrap();
        assert_eq!(out.mint.0, ty);
        let expected = 997_000.0_f64 * 1_000_000_000.0 / (1_000_000.0 + 997_000.0);
        assert!((out.amount as f64 - expected).abs() < 2.0, "got {}, expected ~{}", out.amount, expected);
    }
}
