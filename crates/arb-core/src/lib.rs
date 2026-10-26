//! Shared vocabulary for every other crate in the arbbot workspace.
//!
//! Per `INSTRUCTIONS.md` Phase 1, this crate defines:
//! * [`TokenMint`] / [`TokenAmount`] — typed, decimal-aware token quantities.
//! * [`Pool`] / [`PoolKind`] / [`PoolState`] — DEX-agnostic pool representation.
//! * [`Route`] / [`Hop`] / [`RouteId`] — ordered swap chains with stable ids.
//! * [`Opportunity`] / [`ExpectedProfit`] — pathfinder output + itemised P&L.
//! * [`DexQuoter`] — the trait every DEX integration implements.
//! * [`ArbError`] / [`ArbResult`] — unified error type.
//!
//! Phase −1 (the paper-trading scout) depends only on this crate plus its own
//! `arb-scout` binary. The full bot will pull in many more.

pub mod error;
pub mod opportunity;
pub mod pool;
pub mod quoter;
pub mod route;
pub mod token;

pub use error::{ArbError, ArbResult};
pub use opportunity::{ExpectedProfit, Opportunity};
pub use pool::{Pool, PoolKind, PoolState};
pub use quoter::{DexQuoter, QuoteContext};
pub use route::{Hop, Route, RouteId};
pub use token::{TokenAmount, TokenMint};
