use solana_program::pubkey::Pubkey;

use crate::error::ArbResult;
use crate::pool::Pool;
use crate::token::TokenAmount;

/// Optional per-call context the quoter may use. Currently empty but reserved
/// so we can pass things like the current Pyth oracle price, the swap
/// direction override, or a clock slot without breaking the trait signature.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuoteContext {
    pub slot: Option<u64>,
}

/// Every DEX integration implements this trait.
///
/// `quote()` MUST be deterministic and match on-chain math. A 1-unit rounding
/// disagreement vs `simulateTransaction` is acceptable; anything larger is a
/// bug per `INSTRUCTIONS.md` Phase 5 acceptance criteria.
///
/// The scout calls `try_parse_account()` on every WebSocket update and `quote()`
/// on every candidate route. `build_swap_ix()` is unused in Phase −1 but is
/// part of the trait so the same implementations work for the full bot in
/// Phase 0+ without modification.
#[async_trait::async_trait]
pub trait DexQuoter: Send + Sync {
    /// Stable identifier (e.g. `"raydium-amm-v4"`). Matches `PoolKind::id()`.
    fn dex_id(&self) -> &'static str;

    /// Compute the output amount for a given input on this pool.
    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount>;

    /// Same as `quote` but with extra context. Default implementation ignores
    /// the context — overrides are optional.
    fn quote_with(
        &self,
        pool:  &Pool,
        input: TokenAmount,
        _ctx:  QuoteContext,
    ) -> ArbResult<TokenAmount> {
        self.quote(pool, input)
    }

    /// Build the swap instruction. Unused by the Phase −1 scout (which never
    /// submits transactions); required from Phase 5 onward.
    fn build_swap_ix(
        &self,
        pool:       &Pool,
        input:      TokenAmount,
        min_output: TokenAmount,
        user:       Pubkey,
    ) -> ArbResult<solana_program::instruction::Instruction>;

    /// Try to parse a raw account into a [`Pool`]. Returns `None` if this
    /// account does not belong to this DEX (wrong size / discriminator). The
    /// orchestrator dispatches updates by trying every parser until one
    /// returns `Some`.
    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ArbError;
    use crate::pool::{Pool, PoolKind, PoolState};
    use crate::token::{TokenAmount, TokenMint};
    use solana_program::instruction::Instruction;

    /// Compile-time + runtime check that a struct implementing the trait
    /// satisfies its full contract. Ensures changes to the trait surface
    /// surface as build errors, not silent behavior shifts.
    struct MockQuoter;

    #[async_trait::async_trait]
    impl DexQuoter for MockQuoter {
        fn dex_id(&self) -> &'static str { "mock" }

        fn quote(&self, _pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
            Ok(input)
        }

        fn build_swap_ix(
            &self,
            _pool:       &Pool,
            _input:      TokenAmount,
            _min_output: TokenAmount,
            _user:       Pubkey,
        ) -> ArbResult<Instruction> {
            Err(ArbError::NotImplemented("mock"))
        }

        fn try_parse_account(&self, _pk: &Pubkey, _data: &[u8]) -> Option<Pool> {
            None
        }
    }

    #[test]
    fn mock_dex_quoter_implements_trait() {
        let q: &dyn DexQuoter = &MockQuoter;
        assert_eq!(q.dex_id(), "mock");

        let mint = TokenMint(Pubkey::new_from_array([5u8; 32]));
        let pool = Pool::new(
            Pubkey::new_from_array([6u8; 32]),
            PoolKind::RaydiumAmmV4,
            mint, mint,
            PoolState::new(vec![]),
            0,
        );
        let amt = TokenAmount::new(mint, 100, 6);
        let out = q.quote(&pool, amt).unwrap();
        assert_eq!(out, amt);

        // try_parse returns None for unknown data.
        assert!(q.try_parse_account(&Pubkey::default(), &[]).is_none());
    }
}
