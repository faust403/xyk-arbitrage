//! Orca Whirlpools (concentrated liquidity).
//!
//! Per `INSTRUCTIONS.md` Phase 5B, the quote math walks tick arrays. Critical:
//! the tick arrays passed to a swap MUST be the ones the swap will actually
//! traverse — too few = guaranteed revert with `TickArrayOutOfBounds`. We
//! compute the expected traversal from the input amount + current pool state
//! and pass three arrays defensively.
//!
//! Source of truth: <https://github.com/orca-so/whirlpools>. Program ID
//! `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc`.

use solana_program::pubkey::Pubkey;

pub mod whirlpool;
pub mod whirlpool_event;
pub mod whirlpool_swap_ix;

pub const ORCA_WHIRLPOOL_PROGRAM: Pubkey =
    solana_program::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

pub use whirlpool::{max_safe_input, spot_quote, swap_quote, OrcaWhirlpoolQuoter, WhirlpoolData};
pub use whirlpool_event::{WhirlpoolTradedEvent, TRADED_EVENT_DISCRIMINATOR};
pub use whirlpool_swap_ix::{
    build_swap_ix, derive_oracle_pda, derive_tick_array_pda, tick_array_start_index,
    WhirlpoolSwapAccounts, SWAP_DISCRIMINATOR,
};
