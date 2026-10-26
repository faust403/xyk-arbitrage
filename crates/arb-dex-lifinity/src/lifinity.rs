//! Lifinity AMM v2 quoter (oracle-pegged proactive market maker).
//!
//! ## Source of truth
//!
//! Account layout reverse-engineered from
//! [`carbon-lifinity-amm-v2-decoder`](https://crates.io/crates/carbon-lifinity-amm-v2-decoder)
//! v0.12.0. Lifinity v2 itself is closed-source; the carbon decoder is the
//! community-maintained reference used by Jupiter and Carbon framework.
//!
//! Program ID:  `2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c`
//! Discriminator: `[0x8f, 0xf5, 0xc8, 0x11, 0x4a, 0xd6, 0xc4, 0x87]`
//! Account size: 927 bytes (8 disc + 919 struct)
//!
//! ## Layout (relative to start of `data`, i.e. inclusive of discriminator)
//!
//! | Field                          | Offset | Size |
//! |--------------------------------|--------|------|
//! | (discriminator)                |   0    |  8   |
//! | initializer_key + accounts ×2  |   8    | 96   |
//! | initializer_amount, taker      | 104    | 16   |
//! | bools/bumps                    | 120    |  6   |
//! | token_program_id               | 126    | 32   |
//! | token_a_account (vault A)      | 158    | 32   |
//! | token_b_account (vault B)      | 190    | 32   |
//! | pool_mint                      | 222    | 32   |
//! | token_a_mint                   | 254    | 32   |
//! | token_b_mint                   | 286    | 32   |
//! | fee_account                    | 318    | 32   |
//! | oracle_main_account (Pyth)     | 350    | 32   |
//! | oracle_sub_account             | 382    | 32   |
//! | oracle_pc_account              | 414    | 32   |
//! | AmmFees (64 bytes)             | 446    | 64   |
//! |   trade_fee_numerator          | 446    |  8   |
//! |   trade_fee_denominator        | 454    |  8   |
//! | AmmCurve (9 bytes)             | 510    |  9   |
//! | AmmConfig (248 bytes)          | 519    | 248  |
//! |   last_price                   | 519    |  8   |
//! |   last_balanced_price          | 527    |  8   |
//! |   config_denominator           | 535    |  8   |
//!
//! ## ⚠️ v1 quote math
//!
//! For opportunity *detection*, this quoter computes:
//!
//! ```text
//! price_raw = last_price / config_denominator   // raw_B per raw_A (approx)
//! out       = input × price_raw × (1 − trade_fee/trade_fee_denom)   for A→B
//! out       = input / price_raw × (1 − trade_fee/trade_fee_denom)   for B→A
//! ```
//!
//! `last_price` is updated on every swap from the Pyth oracle reading, so it
//! tracks oracle price closely. **NOT verified against `simulateTransaction`
//! yet** — production execution would need a full quote against the AMM's
//! inventory-adjustment math + Pyth confidence band. Acceptable for the
//! scout's "is the spread big enough to consider" question.

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::Instruction;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

pub const LIFINITY_V2_PROGRAM: Pubkey =
    pubkey!("2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c");

pub const LIFINITY_V2_DISCRIMINATOR: [u8; 8] = [0x8f, 0xf5, 0xc8, 0x11, 0x4a, 0xd6, 0xc4, 0x87];
/// Observed on mainnet: 911 bytes per pool (48 active pools as of 2026-05-10
/// via getProgramAccounts). The carbon decoder's source counts 927 — the
/// 16-byte difference is in AmmConfig (29 u64s × 8 = 232 bytes, not 248).
/// We accept either size to be robust against version shifts.
pub const LIFINITY_V2_ACCOUNT_SIZE_PRIMARY: usize = 911;
pub const LIFINITY_V2_ACCOUNT_SIZE_LEGACY:  usize = 927;

const L_OFF_TOKEN_A_VAULT:    usize = 158;
const L_OFF_TOKEN_B_VAULT:    usize = 190;
const L_OFF_TOKEN_A_MINT:     usize = 254;
const L_OFF_TOKEN_B_MINT:     usize = 286;
const L_OFF_ORACLE_MAIN:      usize = 350;
const L_OFF_TRADE_FEE_NUM:    usize = 446;
const L_OFF_TRADE_FEE_DEN:    usize = 454;
const L_OFF_LAST_PRICE:       usize = 519;
const L_OFF_CONFIG_DENOM:     usize = 535;

#[derive(Debug, Clone, Copy)]
pub struct LifinityV2Data {
    pub token_a_vault:        Pubkey,
    pub token_b_vault:        Pubkey,
    pub token_a_mint:         Pubkey,
    pub token_b_mint:         Pubkey,
    pub oracle_main:          Pubkey,
    pub trade_fee_numerator:  u64,
    pub trade_fee_denominator:u64,
    pub last_price:           u64,
    pub config_denominator:   u64,
}

impl LifinityV2Data {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() != LIFINITY_V2_ACCOUNT_SIZE_PRIMARY
           && data.len() != LIFINITY_V2_ACCOUNT_SIZE_LEGACY { return None; }
        if data[..8] != LIFINITY_V2_DISCRIMINATOR { return None; }

        let read_u64 = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[o..o + 8]);
            u64::from_le_bytes(b)
        };
        let read_pk = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o + 32]);
            Pubkey::new_from_array(b)
        };

        Some(LifinityV2Data {
            token_a_vault:         read_pk(L_OFF_TOKEN_A_VAULT),
            token_b_vault:         read_pk(L_OFF_TOKEN_B_VAULT),
            token_a_mint:          read_pk(L_OFF_TOKEN_A_MINT),
            token_b_mint:          read_pk(L_OFF_TOKEN_B_MINT),
            oracle_main:           read_pk(L_OFF_ORACLE_MAIN),
            trade_fee_numerator:   read_u64(L_OFF_TRADE_FEE_NUM),
            trade_fee_denominator: read_u64(L_OFF_TRADE_FEE_DEN),
            last_price:            read_u64(L_OFF_LAST_PRICE),
            config_denominator:    read_u64(L_OFF_CONFIG_DENOM),
        })
    }
}

/// Spot quote using `last_price / config_denominator` as raw_B/raw_A price.
pub fn spot_quote(
    input:                 u64,
    last_price:            u64,
    config_denominator:    u64,
    trade_fee_numerator:   u64,
    trade_fee_denominator: u64,
    direction_a_to_b:      bool,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if last_price == 0 || config_denominator == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: input,
        });
    }
    if trade_fee_denominator == 0 {
        return Err(ArbError::Invariant("trade_fee_denominator is zero"));
    }

    let price_raw = (last_price as f64) / (config_denominator as f64);
    if !price_raw.is_finite() || price_raw <= 0.0 {
        return Err(ArbError::Invariant("non-positive Lifinity price"));
    }

    let fee_factor = 1.0 - (trade_fee_numerator as f64) / (trade_fee_denominator as f64);
    if !(0.0..=1.0).contains(&fee_factor) {
        return Err(ArbError::Invariant("fee_factor outside [0,1]"));
    }

    let input_f = input as f64;
    let out_f = if direction_a_to_b {
        input_f * price_raw * fee_factor
    } else {
        input_f / price_raw * fee_factor
    };

    if !out_f.is_finite() || out_f < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out_f > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out_f as u64)
}

#[derive(Default)]
pub struct LifinityV2Quoter;

impl LifinityV2Quoter {
    pub fn new() -> Self { Self }
}

#[async_trait::async_trait]
impl DexQuoter for LifinityV2Quoter {
    fn dex_id(&self) -> &'static str { PoolKind::LifinityV2.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = LifinityV2Data::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "lifinity-v2",
                reason: "discriminator mismatch or wrong size".into(),
            })?;

        let direction_a_to_b = match input.mint.0 {
            x if x == parsed.token_a_mint => true,
            x if x == parsed.token_b_mint => false,
            other => return Err(ArbError::MintMismatch {
                expected: parsed.token_a_mint,
                actual:   other,
            }),
        };

        let out_amount = spot_quote(
            input.amount,
            parsed.last_price,
            parsed.config_denominator,
            parsed.trade_fee_numerator,
            parsed.trade_fee_denominator,
            direction_a_to_b,
        )?;

        let out_mint = if direction_a_to_b { parsed.token_b_mint } else { parsed.token_a_mint };
        Ok(TokenAmount::new(TokenMint(out_mint), out_amount, input.decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        Err(ArbError::NotImplemented("Lifinity v2 swap_ix — Phase 12"))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = LifinityV2Data::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::LifinityV2,
            TokenMint(parsed.token_a_mint),
            TokenMint(parsed.token_b_mint),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

pub fn is_lifinity_v2(program_id: &Pubkey) -> bool {
    *program_id == LIFINITY_V2_PROGRAM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn fake_pool(
        mint_a: Pubkey, mint_b: Pubkey, vault_a: Pubkey, vault_b: Pubkey,
        oracle_main: Pubkey,
        trade_fee_num: u64, trade_fee_den: u64,
        last_price: u64, config_denominator: u64,
    ) -> Vec<u8> {
        let mut data = vec![0u8; LIFINITY_V2_ACCOUNT_SIZE_LEGACY];
        data[..8].copy_from_slice(&LIFINITY_V2_DISCRIMINATOR);
        data[L_OFF_TOKEN_A_VAULT..L_OFF_TOKEN_A_VAULT + 32].copy_from_slice(&vault_a.to_bytes());
        data[L_OFF_TOKEN_B_VAULT..L_OFF_TOKEN_B_VAULT + 32].copy_from_slice(&vault_b.to_bytes());
        data[L_OFF_TOKEN_A_MINT..L_OFF_TOKEN_A_MINT + 32].copy_from_slice(&mint_a.to_bytes());
        data[L_OFF_TOKEN_B_MINT..L_OFF_TOKEN_B_MINT + 32].copy_from_slice(&mint_b.to_bytes());
        data[L_OFF_ORACLE_MAIN..L_OFF_ORACLE_MAIN + 32].copy_from_slice(&oracle_main.to_bytes());
        data[L_OFF_TRADE_FEE_NUM..L_OFF_TRADE_FEE_NUM + 8].copy_from_slice(&trade_fee_num.to_le_bytes());
        data[L_OFF_TRADE_FEE_DEN..L_OFF_TRADE_FEE_DEN + 8].copy_from_slice(&trade_fee_den.to_le_bytes());
        data[L_OFF_LAST_PRICE..L_OFF_LAST_PRICE + 8].copy_from_slice(&last_price.to_le_bytes());
        data[L_OFF_CONFIG_DENOM..L_OFF_CONFIG_DENOM + 8].copy_from_slice(&config_denominator.to_le_bytes());
        data
    }

    #[test]
    fn parse_valid_lifinity_pool() {
        let mx = Pubkey::new_from_array([1u8; 32]);
        let my = Pubkey::new_from_array([2u8; 32]);
        let oracle = Pubkey::new_from_array([5u8; 32]);
        let data = fake_pool(mx, my, Pubkey::default(), Pubkey::default(), oracle,
                              25, 10_000, 1_000_000, 1_000_000);
        let p = LifinityV2Data::try_parse(&data).unwrap();
        assert_eq!(p.token_a_mint, mx);
        assert_eq!(p.token_b_mint, my);
        assert_eq!(p.oracle_main, oracle);
        assert_eq!(p.trade_fee_numerator, 25);
        assert_eq!(p.trade_fee_denominator, 10_000);
        assert_eq!(p.last_price, 1_000_000);
        assert_eq!(p.config_denominator, 1_000_000);
    }

    #[test]
    fn parse_rejects_wrong_size() {
        assert!(LifinityV2Data::try_parse(&[0u8; 100]).is_none());
        assert!(LifinityV2Data::try_parse(&[0u8; 910]).is_none());
        assert!(LifinityV2Data::try_parse(&[0u8; 928]).is_none());
    }

    #[test]
    fn parse_accepts_both_911_and_927() {
        // Both observed sizes need to be accepted.
        for size in [911usize, 927] {
            let mut data = vec![0u8; size];
            data[..8].copy_from_slice(&LIFINITY_V2_DISCRIMINATOR);
            assert!(LifinityV2Data::try_parse(&data).is_some(),
                "should parse size {size}");
        }
    }

    #[test]
    fn parse_rejects_wrong_discriminator() {
        let mut data = vec![0u8; LIFINITY_V2_ACCOUNT_SIZE_LEGACY];
        // leave first 8 bytes as zero — wrong discriminator
        assert!(LifinityV2Data::try_parse(&data).is_none());
        data[..8].copy_from_slice(&LIFINITY_V2_DISCRIMINATOR);
        // Now passes (other fields zero but parse still works).
        assert!(LifinityV2Data::try_parse(&data).is_some());
    }

    #[test]
    fn spot_quote_at_price_1_subtracts_only_fee() {
        // last_price=1e6, denom=1e6 ⇒ price=1. Fee 25/10000 = 0.25%.
        let out = spot_quote(1_000_000, 1_000_000, 1_000_000, 25, 10_000, true).unwrap();
        assert!((out as i64 - 997_500).abs() <= 1, "got {out}");
    }

    #[test]
    fn spot_quote_zero_input_zero_output() {
        let out = spot_quote(0, 1_000_000, 1_000_000, 25, 10_000, true).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn spot_quote_zero_price_returns_insufficient_liquidity() {
        let r = spot_quote(1, 0, 1_000_000, 25, 10_000, true);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn spot_quote_round_trip_within_2x_fee() {
        let in0   = 1_000_000_u64;
        let out_b = spot_quote(in0,   1_000_000, 1_000_000, 25, 10_000, true).unwrap();
        let back  = spot_quote(out_b, 1_000_000, 1_000_000, 25, 10_000, false).unwrap();
        // 2× 0.25% = ~0.5% loss
        assert!(back < in0);
        assert!((in0 as i64 - back as i64) < (in0 as i64) / 100);
    }

    #[test]
    fn quoter_dispatches_correct_direction() {
        let mx = Pubkey::new_from_array([1u8; 32]);
        let my = Pubkey::new_from_array([2u8; 32]);
        let data = fake_pool(mx, my, Pubkey::default(), Pubkey::default(),
                              Pubkey::default(), 25, 10_000, 1_000_000, 1_000_000);
        let q = LifinityV2Quoter::new();
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).unwrap();

        let in_a = TokenAmount::new(TokenMint(mx), 1_000_000, 6);
        let out = q.quote(&pool, in_a).unwrap();
        assert_eq!(out.mint.0, my);

        let in_b = TokenAmount::new(TokenMint(my), 1_000_000, 6);
        let out = q.quote(&pool, in_b).unwrap();
        assert_eq!(out.mint.0, mx);
    }

    #[test]
    fn quoter_rejects_unknown_mint() {
        let mx = Pubkey::new_from_array([1u8; 32]);
        let my = Pubkey::new_from_array([2u8; 32]);
        let mc = Pubkey::new_from_array([3u8; 32]);
        let data = fake_pool(mx, my, Pubkey::default(), Pubkey::default(),
                              Pubkey::default(), 25, 10_000, 1_000_000, 1_000_000);
        let q = LifinityV2Quoter::new();
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).unwrap();
        let amt = TokenAmount::new(TokenMint(mc), 1, 6);
        assert!(matches!(q.quote(&pool, amt), Err(ArbError::MintMismatch { .. })));
    }
}
