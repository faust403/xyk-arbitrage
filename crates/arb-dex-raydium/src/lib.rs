//! Raydium AMM v4 + CLMM integration.
//!
//! Per `INSTRUCTIONS.md` Phase 5A this crate hosts:
//! * `RaydiumAmmV4Quoter` — constant-product math.
//!   Source of truth: <https://github.com/raydium-io/raydium-amm>.
//!   Program ID `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8`.
//! * `RaydiumClmmQuoter` — concentrated-liquidity math (also Phase 12 / Wave 2).
//! * Swap-instruction builders for both pool kinds.
//!
//! ## Lookup-trait pattern
//!
//! Raydium AMM v4 keeps its reserves in separate SPL Token vault accounts and
//! its CLMM fee tier in an `AmmConfig` PDA — neither lives inside the pool
//! account. To stay decoupled from any specific store implementation, the
//! quoter accepts [`VaultLookup`] / [`ClmmFeeLookup`] trait objects rather
//! than a concrete type. The scout's `VaultStore` implements both.

pub mod amm_v4;
pub mod amm_v4_swap_ix;
pub mod clmm;
pub mod clmm_swap_ix;
pub mod cpmm;
pub mod program_ids;

pub use amm_v4::{
    quote_with_reserves, RaydiumAmmV4Data, RaydiumAmmV4Quoter, RAYDIUM_AMM_V4_ACCOUNT_SIZE,
    is_raydium_amm_v4,
};
pub use amm_v4_swap_ix::{
    build_swap_base_in_ix, RaydiumSwapAccounts, RAYDIUM_AMM_AUTHORITY,
    SWAP_BASE_IN_DISCRIMINATOR,
};
pub use clmm::{
    parse_trade_fee_rate, spot_quote, swap_quote, ClmmFeeStore, RaydiumClmmData, RaydiumClmmQuoter,
    RAYDIUM_CLMM_ACCOUNT_SIZE, RAYDIUM_CLMM_PROGRAM,
};
pub use clmm_swap_ix::{
    build_swap_ix as build_clmm_swap_ix, derive_tick_array_pda,
    tick_array_start_index, RaydiumClmmSwapAccounts,
    MAX_SQRT_PRICE_X64, MIN_SQRT_PRICE_X64, TICK_ARRAY_SIZE,
};
pub use cpmm::{
    build_cpmm_swap_base_input_ix, cpmm_quote, derive_cpmm_authority,
    RaydiumCpmmData, RaydiumCpmmQuoter, RaydiumCpmmSwapAccounts,
    CPMM_SWAP_BASE_INPUT_DISC, RAYDIUM_CPMM_ACCOUNT_MIN_SIZE, RAYDIUM_CPMM_POOL_DISCRIMINATOR,
};
pub use program_ids::{RAYDIUM_AMM_V4_PROGRAM, RAYDIUM_CPMM_PROGRAM, TOKEN_PROGRAM};

use solana_program::pubkey::Pubkey;

/// Read-only abstraction over the SPL Token vault balance store. Required by
/// [`RaydiumAmmV4Quoter`] which can't keep reserves inside the pool account.
pub trait VaultLookup: Send + Sync {
    fn get(&self, vault: &Pubkey) -> Option<u64>;
}

/// Read-only abstraction over the CLMM AmmConfig (trade-fee-rate) store.
pub trait ClmmFeeLookup: Send + Sync {
    fn fee_rate(&self, amm_config: &Pubkey) -> Option<u32>;
}
