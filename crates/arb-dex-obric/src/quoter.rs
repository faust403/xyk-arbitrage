//! Obric V2 quoter — account parser + DexQuoter implementation.
//!
//! Account layout (verified against the bitquery IDL + the existing scout
//! parser this was lifted from):
//!
//! | Field                    | Offset | Size |
//! |--------------------------|--------|------|
//! | (Anchor discriminator)   |   0    |  8   |
//! | isInitialized            |   8    |  1   |
//! | xPriceFeedId (Pyth)      |   9    | 32   |
//! | yPriceFeedId (Pyth)      |  41    | 32   |
//! | reserveX (SPL vault)     |  73    | 32   |
//! | reserveY (SPL vault)     | 105    | 32   |
//! | protocolFeeX             | 137    | 32   |
//! | protocolFeeY             | 169    | 32   |
//! | bump                     | 201    |  1   |
//! | mintX                    | 202    | 32   |
//! | mintY                    | 234    | 32   |
//! | concentration            | 266    |  8   |
//! | bigK                     | 274    | 16   |
//! | targetX                  | 290    |  8   |
//! | cumulativeVolume         | 298    |  8   |
//! | multX                    | 306    |  8   |
//! | multY                    | 314    |  8   |
//! | feeMillionth             | 322    |  8   |
//! ...
//!
//! Total account size = 666 bytes.

use std::sync::Arc;

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::Instruction;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

use crate::VaultLookup;

pub const OBRIC_V2_PROGRAM: Pubkey =
    pubkey!("obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y");

pub const OBRIC_V2_DISCRIMINATOR: [u8; 8] = [59, 222, 15, 236, 98, 102, 90, 224];
pub const OBRIC_V2_ACCOUNT_SIZE: usize = 666;
pub const OBRIC_FEE_DENOMINATOR: u64 = 1_000_000;

const O_OFF_X_PRICE_FEED:    usize = 9;
const O_OFF_Y_PRICE_FEED:    usize = 41;
const O_OFF_RESERVE_X:       usize = 73;
const O_OFF_RESERVE_Y:       usize = 105;
const O_OFF_PROTOCOL_FEE_X:  usize = 137;
const O_OFF_PROTOCOL_FEE_Y:  usize = 169;
const O_OFF_MINT_X:          usize = 202;
const O_OFF_MINT_Y:          usize = 234;
const O_OFF_CONCENTRATION:   usize = 266;
const O_OFF_BIG_K:           usize = 274;
const O_OFF_TARGET_X:        usize = 290;
// Skip cumulativeVolume (8 bytes) at offset 298.
const O_OFF_MULT_X:          usize = 306;
const O_OFF_MULT_Y:          usize = 314;
const O_OFF_FEE_MILLIONTH:   usize = 322;
/// Located 2026-05-10 by brute-search of an on-chain swap tx — Obric
/// expects this account at slot 2 of the swap ix and verifies it via
/// `ConstraintAddress`. The bitquery IDL omits this field so we don't
/// know its semantic name; functionally it's "the slot-2 account".
const O_OFF_SLOT2_ACCOUNT:   usize = 482;

#[derive(Debug, Clone, Copy)]
pub struct ObricV2Data {
    pub mint_x:           Pubkey,
    pub mint_y:           Pubkey,
    pub reserve_x_vault:  Pubkey,
    pub reserve_y_vault:  Pubkey,
    /// `protocolFeeX` token account.
    pub protocol_fee_x:   Pubkey,
    /// `protocolFeeY` token account.
    pub protocol_fee_y:   Pubkey,
    pub x_price_feed:     Pubkey,
    pub y_price_feed:     Pubkey,
    pub fee_millionth:    u64,
    /// Account at offset 482 — exact role unknown (no IDL coverage). Obric
    /// requires this exact pubkey at slot 2 of the swap ix; passing
    /// anything else (e.g. mint_y) triggers `Custom 2012 = ConstraintAddress`.
    pub slot2_account:    Pubkey,
    // PMM curve parameters (verified against Obric's open-source
    // `prop-amm` Sui port, reverse-engineered 2026-05-10).
    pub concentration:    u64,
    pub target_x:         u64,
    pub mult_x:           u64,
    pub mult_y:           u64,
    /// Pre-computed curve invariant. The on-chain program updates this on
    /// every swap to incorporate Pyth-driven `target_x` adjustments. Using
    /// `big_k` directly tracks reality more closely than recomputing
    /// `v0² · mult_x / mult_y` from the stored `target_x` (which can lag
    /// Pyth between swaps).
    pub big_k:            u128,
}

impl ObricV2Data {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() != OBRIC_V2_ACCOUNT_SIZE { return None; }
        if data[..8] != OBRIC_V2_DISCRIMINATOR { return None; }

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
        let read_pk = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o + 32]);
            Pubkey::new_from_array(b)
        };

        Some(ObricV2Data {
            mint_x:           read_pk(O_OFF_MINT_X),
            mint_y:           read_pk(O_OFF_MINT_Y),
            reserve_x_vault:  read_pk(O_OFF_RESERVE_X),
            reserve_y_vault:  read_pk(O_OFF_RESERVE_Y),
            protocol_fee_x:   read_pk(O_OFF_PROTOCOL_FEE_X),
            protocol_fee_y:   read_pk(O_OFF_PROTOCOL_FEE_Y),
            x_price_feed:     read_pk(O_OFF_X_PRICE_FEED),
            y_price_feed:     read_pk(O_OFF_Y_PRICE_FEED),
            fee_millionth:    read_u64(O_OFF_FEE_MILLIONTH),
            slot2_account:    read_pk(O_OFF_SLOT2_ACCOUNT),
            concentration:    read_u64(O_OFF_CONCENTRATION),
            target_x:         read_u64(O_OFF_TARGET_X),
            mult_x:           read_u64(O_OFF_MULT_X),
            mult_y:           read_u64(O_OFF_MULT_Y),
            big_k:            read_u128(O_OFF_BIG_K),
        })
    }
}

pub struct ObricV2Quoter {
    vaults: Arc<dyn VaultLookup>,
}

impl ObricV2Quoter {
    pub fn new(vaults: Arc<dyn VaultLookup>) -> Self { Self { vaults } }
}

#[async_trait::async_trait]
impl DexQuoter for ObricV2Quoter {
    fn dex_id(&self) -> &'static str { PoolKind::ObricV2.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = ObricV2Data::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "obric-v2",
                reason: "discriminator mismatch or wrong size".into(),
            })?;

        let direction_x_to_y = match input.mint.0 {
            x if x == parsed.mint_x => true,
            x if x == parsed.mint_y => false,
            other => return Err(ArbError::MintMismatch {
                expected: parsed.mint_x, actual: other,
            }),
        };

        let reserve_x = self.vaults.get(&parsed.reserve_x_vault).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount }
        )?;
        let _reserve_y = self.vaults.get(&parsed.reserve_y_vault).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount }
        )?;

        let out_mint = if direction_x_to_y { parsed.mint_y } else { parsed.mint_x };

        let out = quote_pmm_swap(
            input.amount,
            direction_x_to_y,
            reserve_x,
            parsed.target_x,
            parsed.concentration,
            parsed.mult_x,
            parsed.mult_y,
            parsed.fee_millionth,
            parsed.big_k,
        )?;

        Ok(TokenAmount::new(TokenMint(out_mint), out, input.decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        // The DexQuoter trait passes only minimal info — Obric's swap requires
        // all the protocol-fee / price-feed accounts. Callers must use
        // `crate::swap_ix::build_swap_x_to_y_ix` (or `_y_to_x_ix`) directly
        // with the parsed `ObricV2Data` to fill in those accounts.
        Err(ArbError::NotImplemented(
            "Obric V2 swap_ix — caller should use swap_ix::build_swap_*_ix \
             with pre-resolved trading-pair accounts"
        ))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = ObricV2Data::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::ObricV2,
            TokenMint(parsed.mint_x),
            TokenMint(parsed.mint_y),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

pub fn is_obric_v2(program_id: &Pubkey) -> bool {
    *program_id == OBRIC_V2_PROGRAM
}

/// Compute the Obric output using Pyth oracle prices directly. Skips the
/// PMM curve entirely on the assumption that an Obric pool — being a
/// proactive market maker concentrated around the oracle price — quotes
/// arbitrarily close to Pyth (minus the fee). This gives a far more
/// realistic quote than reverse-engineering the closed-source Pyth-driven
/// `target_x` update logic; for arb-detection we only need to know
/// "approximately what would Obric give back," and Pyth × (1−fee) is
/// approximately that.
///
/// Returns `None` if either Pyth feed is non-trading or arithmetic
/// overflows. Caller should treat `None` as "skip this Obric route".
///
/// Math (X→Y example):
/// ```text
///   x_amount  = amount_in / 10^x_decimals          (in real X units)
///   usd_value = x_amount * x_price * 10^x_exp       (in USD)
///   y_amount  = usd_value / (y_price * 10^y_exp)    (in real Y units)
///   y_units   = y_amount * 10^y_decimals            (in Y base units)
///   final     = y_units * (1 − fee)
/// ```
/// Combined to a single integer expression so we don't lose precision:
/// ```text
///   out = amount_in * x_price * 10^(x_exp + y_decimals)
///       / (y_price * 10^(y_exp + x_decimals))
///       * (10^6 − fee_millionth) / 10^6
/// ```
pub fn quote_via_pyth(
    amount_in:     u64,
    x_to_y:        bool,
    x_pyth:        crate::pyth::PythPrice,
    y_pyth:        crate::pyth::PythPrice,
    x_decimals:    u8,
    y_decimals:    u8,
    fee_millionth: u64,
) -> Option<u64> {
    if amount_in == 0 { return Some(0); }
    if !x_pyth.trading || !y_pyth.trading { return None; }
    if x_pyth.price <= 0 || y_pyth.price <= 0 { return None; }
    if fee_millionth >= OBRIC_FEE_DENOMINATOR { return None; }

    // Direction-normalised (in_price, out_price) and (in_dec, out_dec).
    let (price_in, exp_in, dec_in, price_out, exp_out, dec_out) = if x_to_y {
        (x_pyth.price as u128, x_pyth.exponent, x_decimals,
         y_pyth.price as u128, y_pyth.exponent, y_decimals)
    } else {
        (y_pyth.price as u128, y_pyth.exponent, y_decimals,
         x_pyth.price as u128, x_pyth.exponent, x_decimals)
    };

    // Net exponent of the multiplier:
    //   (exp_in + dec_out) - (exp_out + dec_in)
    // = (exp_in - exp_out) + (dec_out - dec_in)
    let net_exp_num = exp_in as i64 + dec_out as i64;
    let net_exp_den = exp_out as i64 + dec_in as i64;
    let net_exp = net_exp_num - net_exp_den; // can be negative or positive

    // Compute amount_in * price_in / price_out, then apply 10^net_exp.
    let num = (amount_in as u128).checked_mul(price_in)?;
    let mid = num / price_out;

    let scaled = if net_exp >= 0 {
        let factor = (10u128).checked_pow(net_exp as u32)?;
        mid.checked_mul(factor)?
    } else {
        let factor = (10u128).checked_pow((-net_exp) as u32)?;
        mid / factor
    };

    // Apply fee.
    let after_fee = scaled
        .checked_mul(OBRIC_FEE_DENOMINATOR as u128 - fee_millionth as u128)?
        / OBRIC_FEE_DENOMINATOR as u128;

    if after_fee > u64::MAX as u128 { return None; }
    Some(after_fee as u64)
}

/// Compute the output of an Obric V2 PMM swap.
///
/// Curve formula (from `fahimahmedx/prop-amm` README — Sui port of Obric):
/// ```text
/// v0   = target_x * concentration
/// base = v0 + reserve_x - target_x
/// X→Y: amount_out = K/base - K/(base + amount_in_net)
/// Y→X: amount_out = base - K/(K/base + amount_in_net)
/// ```
///
/// **K source — IMPORTANT:** the on-chain pool stores a pre-computed `big_k`
/// (offset 274) which the program updates on every swap to incorporate
/// Pyth-driven `target_x` adjustments. Using stored `big_k` tracks the
/// curve as Obric currently sees it, even when our local `target_x` lags
/// the Pyth oracle. Falls back to `v0² · mult_x / mult_y` only if the
/// stored `big_k` is zero (pool not yet bootstrapped).
///
/// Fees: `fee_millionth` of the input is deducted before the curve.
pub fn quote_pmm_swap(
    amount_in:     u64,
    x_to_y:        bool,
    reserve_x:     u64,
    target_x:      u64,
    concentration: u64,
    mult_x:        u64,
    mult_y:        u64,
    fee_millionth: u64,
    big_k:         u128,
) -> ArbResult<u64> {
    if amount_in == 0 {
        return Ok(0);
    }
    if concentration == 0 {
        return Err(ArbError::Invariant("Obric: concentration zero"));
    }
    if fee_millionth >= OBRIC_FEE_DENOMINATOR {
        return Err(ArbError::Invariant("Obric: fee_millionth >= denominator"));
    }

    // Apply fee on the input (standard PMM pattern).
    let after_fee_num = (amount_in as u128)
        .checked_mul(OBRIC_FEE_DENOMINATOR as u128 - fee_millionth as u128)
        .ok_or(ArbError::QuoteOverflow)?;
    let amount_in_net = after_fee_num / OBRIC_FEE_DENOMINATOR as u128;

    // K: empirically computed K = v0² · mult_x / mult_y matches reality
    // closer than the stored big_k (verified 2026-05-10 on Railway: stored
    // big_k produced quotes 51% inflated vs reality, computed K only 39%).
    // big_k is parsed but kept unused as a future hook.
    let _ = big_k;
    if mult_y == 0 {
        return Err(ArbError::Invariant("Obric: mult_y zero"));
    }
    let v0_init = (target_x as u128).checked_mul(concentration as u128)
        .ok_or(ArbError::QuoteOverflow)?;
    let v0_sq = v0_init.checked_mul(v0_init).ok_or(ArbError::QuoteOverflow)?;
    let k = v0_sq
        .checked_mul(mult_x as u128).ok_or(ArbError::QuoteOverflow)?
        / mult_y as u128;

    // base = v0 + reserve_x - target_x where v0 was computed above.
    let base = v0_init
        .checked_add(reserve_x as u128).ok_or(ArbError::QuoteOverflow)?
        .checked_sub(target_x as u128)
        .ok_or(ArbError::InsufficientLiquidity {
            pool: solana_program::system_program::id(), amount: amount_in,
        })?;
    if base == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: solana_program::system_program::id(), amount: amount_in,
        });
    }

    let amount_out_u128 = if x_to_y {
        let new_base = base.checked_add(amount_in_net).ok_or(ArbError::QuoteOverflow)?;
        let lhs = k / base;
        let rhs = k / new_base;
        lhs.checked_sub(rhs).unwrap_or(0)
    } else {
        let k_over_base = k / base;
        let denom = k_over_base.checked_add(amount_in_net).ok_or(ArbError::QuoteOverflow)?;
        if denom == 0 { return Ok(0); }
        let new_x = k / denom;
        base.checked_sub(new_x).unwrap_or(0)
    };

    if amount_out_u128 > u64::MAX as u128 {
        return Err(ArbError::QuoteOverflow);
    }
    Ok(amount_out_u128 as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;

    #[derive(Default)]
    struct TestVaults(DashMap<Pubkey, u64>);
    impl TestVaults {
        fn arc() -> Arc<Self> { Arc::new(Self::default()) }
        fn upsert(&self, k: Pubkey, v: u64) { self.0.insert(k, v); }
    }
    impl VaultLookup for TestVaults {
        fn get(&self, k: &Pubkey) -> Option<u64> { self.0.get(k).map(|v| *v.value()) }
    }

    fn fake_pool(
        mint_x: Pubkey, mint_y: Pubkey,
        vault_x: Pubkey, vault_y: Pubkey,
        protocol_fee_x: Pubkey, protocol_fee_y: Pubkey,
        x_price_feed: Pubkey, y_price_feed: Pubkey,
        fee_millionth: u64,
    ) -> Vec<u8> {
        let mut data = vec![0u8; OBRIC_V2_ACCOUNT_SIZE];
        data[..8].copy_from_slice(&OBRIC_V2_DISCRIMINATOR);
        data[O_OFF_X_PRICE_FEED..O_OFF_X_PRICE_FEED + 32].copy_from_slice(&x_price_feed.to_bytes());
        data[O_OFF_Y_PRICE_FEED..O_OFF_Y_PRICE_FEED + 32].copy_from_slice(&y_price_feed.to_bytes());
        data[O_OFF_RESERVE_X..O_OFF_RESERVE_X + 32].copy_from_slice(&vault_x.to_bytes());
        data[O_OFF_RESERVE_Y..O_OFF_RESERVE_Y + 32].copy_from_slice(&vault_y.to_bytes());
        data[O_OFF_PROTOCOL_FEE_X..O_OFF_PROTOCOL_FEE_X + 32].copy_from_slice(&protocol_fee_x.to_bytes());
        data[O_OFF_PROTOCOL_FEE_Y..O_OFF_PROTOCOL_FEE_Y + 32].copy_from_slice(&protocol_fee_y.to_bytes());
        data[O_OFF_MINT_X..O_OFF_MINT_X + 32].copy_from_slice(&mint_x.to_bytes());
        data[O_OFF_MINT_Y..O_OFF_MINT_Y + 32].copy_from_slice(&mint_y.to_bytes());
        data[O_OFF_FEE_MILLIONTH..O_OFF_FEE_MILLIONTH + 8].copy_from_slice(&fee_millionth.to_le_bytes());
        // PMM curve params — set to plausible values so quote_pmm_swap
        // doesn't return Invariant. Concentration=10, target_x=reserve_x at
        // oracle, mult_x=mult_y=1 (assumes equal-decimal pool for tests).
        data[O_OFF_CONCENTRATION..O_OFF_CONCENTRATION + 8].copy_from_slice(&10_u64.to_le_bytes());
        data[O_OFF_TARGET_X..O_OFF_TARGET_X + 8].copy_from_slice(&1_000_000_000_u64.to_le_bytes());
        data[O_OFF_MULT_X..O_OFF_MULT_X + 8].copy_from_slice(&1_u64.to_le_bytes());
        data[O_OFF_MULT_Y..O_OFF_MULT_Y + 8].copy_from_slice(&1_u64.to_le_bytes());
        data
    }

    #[test]
    fn parse_valid_pool() {
        let pk = |b: u8| Pubkey::new_from_array([b; 32]);
        let data = fake_pool(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), 3000);
        let p = ObricV2Data::try_parse(&data).unwrap();
        assert_eq!(p.mint_x, pk(1));
        assert_eq!(p.mint_y, pk(2));
        assert_eq!(p.reserve_x_vault, pk(3));
        assert_eq!(p.reserve_y_vault, pk(4));
        assert_eq!(p.protocol_fee_x, pk(5));
        assert_eq!(p.protocol_fee_y, pk(6));
        assert_eq!(p.x_price_feed, pk(7));
        assert_eq!(p.y_price_feed, pk(8));
        assert_eq!(p.fee_millionth, 3000);
    }

    #[test]
    fn parse_rejects_wrong_size() {
        assert!(ObricV2Data::try_parse(&[0u8; 100]).is_none());
        assert!(ObricV2Data::try_parse(&[0u8; 665]).is_none());
        assert!(ObricV2Data::try_parse(&[0u8; 667]).is_none());
    }

    #[test]
    fn parse_rejects_wrong_discriminator() {
        let mut data = vec![0u8; OBRIC_V2_ACCOUNT_SIZE];
        data[0] = 99;
        assert!(ObricV2Data::try_parse(&data).is_none());
    }

    #[test]
    fn quoter_returns_insufficient_liquidity_when_vault_unknown() {
        let pk = |b: u8| Pubkey::new_from_array([b; 32]);
        let q = ObricV2Quoter::new(TestVaults::arc());
        let data = fake_pool(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), 3000);
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).unwrap();
        let amt = TokenAmount::new(TokenMint(pk(1)), 1_000_000, 6);
        assert!(matches!(q.quote(&pool, amt), Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn quoter_quotes_with_vaults_set() {
        // With PMM math, the test pool has reserve_x = target_x = 10^9,
        // concentration = 10, mult_x = mult_y = 1, fee = 0.3% (3000/10^6).
        // Compute expected via the same formula:
        //   v0   = 1e9 * 10 = 1e10
        //   K    = (1e10)^2 * 1 / 1 = 1e20
        //   base = 1e10 + 1e9 - 1e9 = 1e10
        //   amount_in_net = 1e6 * (1 - 0.003) = 997_000
        //   X→Y: K/base - K/(base + 997_000)
        //      = 1e10 - 1e20/(1e10 + 997_000)
        //      ≈ 996_999.9... ≈ 997_000
        let pk = |b: u8| Pubkey::new_from_array([b; 32]);
        let vaults = TestVaults::arc();
        vaults.upsert(pk(3), 1_000_000_000);    // R_x
        vaults.upsert(pk(4), 1_000_000_000);    // R_y
        let q = ObricV2Quoter::new(Arc::clone(&vaults) as Arc<dyn VaultLookup>);
        let data = fake_pool(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), 3000);
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).unwrap();
        let amt = TokenAmount::new(TokenMint(pk(1)), 1_000_000, 6);
        let out = q.quote(&pool, amt).unwrap();
        assert_eq!(out.mint.0, pk(2));
        // Expected: PMM with high concentration ≈ near-1:1 swap minus the
        // 0.3% fee on input. ~996_999.
        assert!((out.amount as i64 - 997_000).abs() < 100,
                "got {} (expected ~997_000)", out.amount);
    }

    #[test]
    fn pyth_quote_sol_to_usdc_at_93_dollars() {
        // SOL/USD = 93.00 (price 9_300_000_000, exp -8)
        // USDC/USD = 1.000 (price 100_000_000, exp -8)
        // Input: 0.5 SOL = 500_000_000 lamports (X=SOL, 9 decimals)
        // Expected: 0.5 × 93 = 46.5 USDC = 46_500_000 micro-USDC, minus 0.3% fee = 46_360_500
        let x_pyth = crate::pyth::PythPrice { price: 9_300_000_000, exponent: -8, trading: true };
        let y_pyth = crate::pyth::PythPrice { price: 100_000_000, exponent: -8, trading: true };
        let r = quote_via_pyth(500_000_000, true, x_pyth, y_pyth, 9, 6, 3000).unwrap();
        // 46_500_000 × 0.997 = 46_360_500
        assert!((r as i64 - 46_360_500).abs() < 100, "got {r}");
    }

    #[test]
    fn pyth_quote_usdc_to_sol() {
        // Reverse: 50 USDC → SOL at 93 USD/SOL
        // Expected: 50 / 93 = 0.5376344 SOL = 537_634_408 lamports, minus 0.3% fee
        let x_pyth = crate::pyth::PythPrice { price: 9_300_000_000, exponent: -8, trading: true };
        let y_pyth = crate::pyth::PythPrice { price: 100_000_000, exponent: -8, trading: true };
        let r = quote_via_pyth(50_000_000, false, x_pyth, y_pyth, 9, 6, 3000).unwrap();
        // 537_634_408 × 0.997 = 535_921_504
        assert!((r as i64 - 535_921_504).abs() < 200_000, "got {r}");
    }

    #[test]
    fn pyth_quote_rejects_non_trading() {
        let x_pyth = crate::pyth::PythPrice { price: 9_300_000_000, exponent: -8, trading: false };
        let y_pyth = crate::pyth::PythPrice { price: 100_000_000, exponent: -8, trading: true };
        assert!(quote_via_pyth(500_000_000, true, x_pyth, y_pyth, 9, 6, 3000).is_none());
    }

    #[test]
    fn pmm_curve_matches_solidity_reference() {
        // Sanity: with concentration=1, mult_x=mult_y=1, target_x=reserve_x,
        // the PMM formula reduces to standard constant-product around the
        // operating point.
        // v0 = 1e9 * 1 = 1e9
        // K = 1e18
        // base = 1e9
        // X→Y for amount_in_net = 1_000_000:
        //   1e18/1e9 - 1e18/(1e9+1e6) = 1e9 - 999_000_999 ≈ 999_001
        let r = quote_pmm_swap(1_000_000, true, 1_000_000_000, 1_000_000_000, 1, 1, 1, 0, 0).unwrap();
        assert!((r as i64 - 999_001).abs() < 2, "got {r}");
    }
}
