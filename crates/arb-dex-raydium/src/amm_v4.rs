//! Raydium AMM v4 quoter — constant-product (`x * y = k`) with on-chain swap fee.
//!
//! ## Account layout reference
//!
//! Field offsets in the 752-byte pool account, taken from
//! `LIQUIDITY_STATE_LAYOUT_V4` in raydium-sdk-v2:
//!
//! | Field                | Offset | Type   |
//! |----------------------|--------|--------|
//! | swap_fee_numerator   | 176    | u64    |
//! | swap_fee_denominator | 184    | u64    |
//! | base_vault           | 336    | Pubkey |
//! | quote_vault          | 368    | Pubkey |
//! | base_mint            | 400    | Pubkey |
//! | quote_mint           | 432    | Pubkey |
//! | open_orders          | 496    | Pubkey |
//! | market_id            | 528    | Pubkey |
//! | market_program_id    | 560    | Pubkey |
//! | target_orders        | 592    | Pubkey |
//!
//! ## Vault wiring
//!
//! AMM v4 keeps reserves in **separate SPL Token vault accounts**. The quoter
//! holds an `Arc<dyn VaultLookup>` (typically the scout's `VaultStore`); on
//! `quote()` we look up the two vault balances and feed them to
//! [`quote_with_reserves`]. If either vault is missing we return
//! `InsufficientLiquidity` and let the caller skip the route.

use std::sync::Arc;

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::program_ids::RAYDIUM_AMM_V4_PROGRAM;
use crate::VaultLookup;

/// Expected serialised account size of `LIQUIDITY_STATE_LAYOUT_V4`.
pub const RAYDIUM_AMM_V4_ACCOUNT_SIZE: usize = 752;

const OFFSET_SWAP_FEE_NUM:    usize = 176;
const OFFSET_SWAP_FEE_DEN:    usize = 184;
const OFFSET_BASE_VAULT:      usize = 336;
const OFFSET_QUOTE_VAULT:     usize = 368;
const OFFSET_BASE_MINT:       usize = 400;
const OFFSET_QUOTE_MINT:      usize = 432;
const OFFSET_OPEN_ORDERS:     usize = 496;
const OFFSET_MARKET_ID:       usize = 528;
const OFFSET_MARKET_PROGRAM:  usize = 560;
const OFFSET_TARGET_ORDERS:   usize = 592;

#[derive(Debug, Clone, Copy)]
pub struct RaydiumAmmV4Data {
    pub swap_fee_numerator:   u64,
    pub swap_fee_denominator: u64,
    pub base_vault:           Pubkey,
    pub quote_vault:          Pubkey,
    pub base_mint:            Pubkey,
    pub quote_mint:           Pubkey,
    pub open_orders:          Pubkey,
    pub market_id:            Pubkey,
    pub market_program:       Pubkey,
    pub target_orders:        Pubkey,
}

impl RaydiumAmmV4Data {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() != RAYDIUM_AMM_V4_ACCOUNT_SIZE { return None; }
        let read_u64 = |off: usize| -> u64 {
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&data[off..off + 8]);
            u64::from_le_bytes(buf)
        };
        let read_pk = |off: usize| -> Pubkey {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&data[off..off + 32]);
            Pubkey::new_from_array(buf)
        };
        Some(RaydiumAmmV4Data {
            swap_fee_numerator:   read_u64(OFFSET_SWAP_FEE_NUM),
            swap_fee_denominator: read_u64(OFFSET_SWAP_FEE_DEN),
            base_vault:           read_pk(OFFSET_BASE_VAULT),
            quote_vault:          read_pk(OFFSET_QUOTE_VAULT),
            base_mint:            read_pk(OFFSET_BASE_MINT),
            quote_mint:           read_pk(OFFSET_QUOTE_MINT),
            open_orders:          read_pk(OFFSET_OPEN_ORDERS),
            market_id:            read_pk(OFFSET_MARKET_ID),
            market_program:       read_pk(OFFSET_MARKET_PROGRAM),
            target_orders:        read_pk(OFFSET_TARGET_ORDERS),
        })
    }
}

/// Constant-product quote with explicit reserves and fee.
///
/// `out = (input * (fee_den - fee_num)) * reserve_out
///         / (reserve_in * fee_den + input * (fee_den - fee_num))`
pub fn quote_with_reserves(
    input:                u64,
    reserve_in:           u64,
    reserve_out:          u64,
    swap_fee_numerator:   u64,
    swap_fee_denominator: u64,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if reserve_in == 0 || reserve_out == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: input,
        });
    }
    if swap_fee_denominator == 0 {
        return Err(ArbError::Invariant("swap_fee_denominator is zero"));
    }

    let input        = input as u128;
    let reserve_in   = reserve_in  as u128;
    let reserve_out  = reserve_out as u128;
    let fee_num      = swap_fee_numerator   as u128;
    let fee_den      = swap_fee_denominator as u128;

    let amount_in_with_fee = input
        .checked_mul(fee_den.checked_sub(fee_num).ok_or(ArbError::QuoteOverflow)?)
        .ok_or(ArbError::QuoteOverflow)?;
    let numerator   = amount_in_with_fee.checked_mul(reserve_out).ok_or(ArbError::QuoteOverflow)?;
    let denominator = reserve_in
        .checked_mul(fee_den).ok_or(ArbError::QuoteOverflow)?
        .checked_add(amount_in_with_fee).ok_or(ArbError::QuoteOverflow)?;

    let out = numerator / denominator;
    u64::try_from(out).map_err(|_| ArbError::QuoteOverflow)
}

pub struct RaydiumAmmV4Quoter {
    vaults: Arc<dyn VaultLookup>,
}

impl RaydiumAmmV4Quoter {
    pub fn new(vaults: Arc<dyn VaultLookup>) -> Self { Self { vaults } }
}

#[async_trait::async_trait]
impl DexQuoter for RaydiumAmmV4Quoter {
    fn dex_id(&self) -> &'static str { PoolKind::RaydiumAmmV4.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = RaydiumAmmV4Data::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "raydium-amm-v4",
                reason: format!("expected {RAYDIUM_AMM_V4_ACCOUNT_SIZE} bytes, got {}", pool.state.len()),
            })?;

        let base_reserve = self.vaults.get(&parsed.base_vault).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount }
        )?;
        let quote_reserve = self.vaults.get(&parsed.quote_vault).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount }
        )?;

        let (reserve_in, reserve_out, out_mint) = if input.mint.0 == parsed.base_mint {
            (base_reserve, quote_reserve, parsed.quote_mint)
        } else if input.mint.0 == parsed.quote_mint {
            (quote_reserve, base_reserve, parsed.base_mint)
        } else {
            return Err(ArbError::MintMismatch {
                expected: parsed.base_mint,
                actual:   input.mint.0,
            });
        };

        let out_amount = quote_with_reserves(
            input.amount,
            reserve_in,
            reserve_out,
            parsed.swap_fee_numerator,
            parsed.swap_fee_denominator,
        )?;
        Ok(TokenAmount::new(TokenMint(out_mint), out_amount, input.decimals))
    }

    fn build_swap_ix(
        &self,
        _pool:       &Pool,
        _input:      TokenAmount,
        _min_output: TokenAmount,
        _user:       Pubkey,
    ) -> ArbResult<Instruction> {
        Err(ArbError::NotImplemented(
            "Raydium AMM v4 swap_ix — caller should use crate::amm_v4_swap_ix::build_swap_base_in_ix \
             with pre-resolved Serum/OpenBook accounts"
        ))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = RaydiumAmmV4Data::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::RaydiumAmmV4,
            TokenMint(parsed.base_mint),
            TokenMint(parsed.quote_mint),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

pub fn is_raydium_amm_v4(program_id: &Pubkey) -> bool {
    *program_id == RAYDIUM_AMM_V4_PROGRAM
}

#[cfg(test)]
mod tests {
    use super::*;
    use dashmap::DashMap;

    /// Tiny in-test VaultLookup implementation. Mirrors the scout's VaultStore
    /// without dragging that crate's dependency in for tests.
    #[derive(Default)]
    struct TestVaults(DashMap<Pubkey, u64>);
    impl TestVaults {
        fn arc() -> Arc<Self> { Arc::new(Self::default()) }
        fn upsert(&self, k: Pubkey, v: u64) { self.0.insert(k, v); }
    }
    impl VaultLookup for TestVaults {
        fn get(&self, k: &Pubkey) -> Option<u64> { self.0.get(k).map(|v| *v.value()) }
    }

    fn fake_account(swap_fee_num: u64, swap_fee_den: u64,
                    base_mint: Pubkey, quote_mint: Pubkey,
                    base_vault: Pubkey, quote_vault: Pubkey) -> Vec<u8> {
        let mut data = vec![0u8; RAYDIUM_AMM_V4_ACCOUNT_SIZE];
        data[OFFSET_SWAP_FEE_NUM..OFFSET_SWAP_FEE_NUM + 8].copy_from_slice(&swap_fee_num.to_le_bytes());
        data[OFFSET_SWAP_FEE_DEN..OFFSET_SWAP_FEE_DEN + 8].copy_from_slice(&swap_fee_den.to_le_bytes());
        data[OFFSET_BASE_VAULT..OFFSET_BASE_VAULT + 32].copy_from_slice(&base_vault.to_bytes());
        data[OFFSET_QUOTE_VAULT..OFFSET_QUOTE_VAULT + 32].copy_from_slice(&quote_vault.to_bytes());
        data[OFFSET_BASE_MINT..OFFSET_BASE_MINT + 32].copy_from_slice(&base_mint.to_bytes());
        data[OFFSET_QUOTE_MINT..OFFSET_QUOTE_MINT + 32].copy_from_slice(&quote_mint.to_bytes());
        data
    }

    #[test]
    fn test_parse_amm_v4_account_valid() {
        let bm = Pubkey::new_from_array([1u8; 32]);
        let qm = Pubkey::new_from_array([2u8; 32]);
        let bv = Pubkey::new_from_array([3u8; 32]);
        let qv = Pubkey::new_from_array([4u8; 32]);
        let data = fake_account(25, 10_000, bm, qm, bv, qv);
        let parsed = RaydiumAmmV4Data::try_parse(&data).expect("parse");
        assert_eq!(parsed.swap_fee_numerator, 25);
        assert_eq!(parsed.swap_fee_denominator, 10_000);
        assert_eq!(parsed.base_mint,   bm);
        assert_eq!(parsed.quote_mint,  qm);
        assert_eq!(parsed.base_vault,  bv);
        assert_eq!(parsed.quote_vault, qv);
    }

    #[test]
    fn test_parse_amm_v4_account_wrong_size() {
        let data = vec![0u8; 100];
        assert!(RaydiumAmmV4Data::try_parse(&data).is_none());
    }

    #[test]
    fn test_quote_constant_product_known_value() {
        let out = quote_with_reserves(1_000, 100_000, 1_000_000_000, 25, 10_000).unwrap();
        assert_eq!(out, 9_876_482);
    }

    #[test]
    fn test_quote_zero_input() {
        let out = quote_with_reserves(0, 100, 100, 25, 10_000).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn test_quote_zero_reserve_returns_insufficient_liquidity() {
        let r = quote_with_reserves(100, 0, 1000, 25, 10_000);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn test_quote_fee_applied() {
        let high_fee = quote_with_reserves(1000, 100_000, 1_000_000_000, 100, 10_000).unwrap();
        let low_fee  = quote_with_reserves(1000, 100_000, 1_000_000_000, 25,  10_000).unwrap();
        assert!(low_fee > high_fee);
    }

    #[test]
    fn test_quote_max_input_no_overflow() {
        let r = quote_with_reserves(u64::MAX / 1_000_000, 1_000_000_000_000, 1_000_000_000_000, 25, 10_000);
        assert!(r.is_ok());
    }

    #[test]
    fn quoter_parses_account_into_pool() {
        let q = RaydiumAmmV4Quoter::new(TestVaults::arc());
        let bm = Pubkey::new_from_array([1u8; 32]);
        let qm = Pubkey::new_from_array([2u8; 32]);
        let data = fake_account(25, 10_000, bm, qm, Pubkey::default(), Pubkey::default());
        let pool = q.try_parse_account(&Pubkey::new_from_array([9u8; 32]), &data).expect("parse");
        assert_eq!(pool.kind, PoolKind::RaydiumAmmV4);
        assert_eq!(pool.token_a.0, bm);
        assert_eq!(pool.token_b.0, qm);
    }

    #[test]
    fn quoter_returns_insufficient_liquidity_when_vault_unknown() {
        let vaults = TestVaults::arc();
        let q = RaydiumAmmV4Quoter::new(Arc::clone(&vaults) as Arc<dyn VaultLookup>);
        let bm = Pubkey::new_from_array([1u8; 32]);
        let qm = Pubkey::new_from_array([2u8; 32]);
        let bv = Pubkey::new_from_array([3u8; 32]);
        let qv = Pubkey::new_from_array([4u8; 32]);
        let data = fake_account(25, 10_000, bm, qm, bv, qv);
        let pool = q.try_parse_account(&Pubkey::new_from_array([9u8; 32]), &data).expect("parse");
        let amt = TokenAmount::new(TokenMint(bm), 1_000_000, 6);
        let r = q.quote(&pool, amt);
        assert!(matches!(r, Err(ArbError::InsufficientLiquidity { .. })));
    }

    #[test]
    fn quoter_returns_correct_output_when_vaults_known() {
        let vaults = TestVaults::arc();
        let bm = Pubkey::new_from_array([1u8; 32]);
        let qm = Pubkey::new_from_array([2u8; 32]);
        let bv = Pubkey::new_from_array([3u8; 32]);
        let qv = Pubkey::new_from_array([4u8; 32]);
        vaults.upsert(bv, 100_000);
        vaults.upsert(qv, 1_000_000_000);

        let q = RaydiumAmmV4Quoter::new(Arc::clone(&vaults) as Arc<dyn VaultLookup>);
        let data = fake_account(25, 10_000, bm, qm, bv, qv);
        let pool = q.try_parse_account(&Pubkey::new_from_array([9u8; 32]), &data).expect("parse");

        let amt = TokenAmount::new(TokenMint(bm), 1_000, 6);
        let out = q.quote(&pool, amt).expect("quote");
        assert_eq!(out.mint.0, qm);
        assert_eq!(out.amount, 9_876_482);
    }

    #[test]
    fn quoter_rejects_unknown_mint() {
        let vaults = TestVaults::arc();
        let q = RaydiumAmmV4Quoter::new(Arc::clone(&vaults) as Arc<dyn VaultLookup>);
        let bm = Pubkey::new_from_array([1u8; 32]);
        let qm = Pubkey::new_from_array([2u8; 32]);
        let mc = Pubkey::new_from_array([7u8; 32]);
        let bv = Pubkey::new_from_array([3u8; 32]);
        let qv = Pubkey::new_from_array([4u8; 32]);
        vaults.upsert(bv, 100);
        vaults.upsert(qv, 100);
        let data = fake_account(25, 10_000, bm, qm, bv, qv);
        let pool = q.try_parse_account(&Pubkey::default(), &data).expect("parse");
        let amt = TokenAmount::new(TokenMint(mc), 1, 6);
        assert!(matches!(q.quote(&pool, amt), Err(ArbError::MintMismatch { .. })));
    }

    use proptest::prelude::*;
    proptest! {
        #[test]
        fn prop_quote_monotonic_in_input(
            input1 in 1u64..1_000_000_000,
            input2 in 1u64..1_000_000_000,
        ) {
            let r1 = quote_with_reserves(input1.min(input2), 1_000_000_000_000, 1_000_000_000_000, 25, 10_000).unwrap();
            let r2 = quote_with_reserves(input1.max(input2), 1_000_000_000_000, 1_000_000_000_000, 25, 10_000).unwrap();
            prop_assert!(r2 >= r1);
        }

        #[test]
        fn prop_quote_no_overflow(
            input in 0u64..u64::MAX/1_000_000,
            r_in  in 1u64..u64::MAX/1_000_000,
            r_out in 1u64..u64::MAX/1_000_000,
        ) {
            let _ = quote_with_reserves(input, r_in, r_out, 25, 10_000);
        }
    }
}
