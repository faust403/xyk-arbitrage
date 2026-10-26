//! Pathfinder: precomputes candidate arbitrage cycles, evaluates them on
//! pool-state updates.
//!
//! Per `INSTRUCTIONS.md` Phase 7, the algorithm finds 2-hop and 3-hop cycles
//! in USDC. Phase 7A — extracted from the scout — provides the core route
//! enumeration; the scout's existing optimal-size ternary search lives
//! alongside in `arb-scout::scout` for now and will move here in Phase 7B.

use arb_core::PoolKind;
use solana_program::pubkey::Pubkey;

pub mod known;
pub mod routes;
pub use routes::*;

/// Decoupled pool descriptor used by the route builder. Matches the *shape*
/// of `arb-config::MonitoredPool` and `arb-scout::config::MonitoredPool`
/// without depending on either crate, so the pathfinder stays a leaf.
#[derive(Debug, Clone)]
pub struct RoutablePool {
    pub address: Pubkey,
    pub kind:    PoolKind,
    pub token_a: Option<Pubkey>,
    pub token_b: Option<Pubkey>,
}
