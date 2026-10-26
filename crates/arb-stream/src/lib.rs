//! Real-time pool-state streaming + lock-free in-memory store.
//!
//! Per `INSTRUCTIONS.md` Phase 6, the bot supports two streaming backends.
//! Phase 6A — extracted from the scout — provides:
//! * [`wss`] — Helius Atlas Enhanced WebSocket subscriber. Same code the
//!   scout has been using; bundled with the Helius Developer plan.
//! * [`state`] — `PoolStateStore` (lock-free per-address pool snapshot).
//!
//! Phase 6B (the LaserStream gRPC backend) lands later, behind the
//! `laserstream` feature flag, sharing the same `PoolStateStore`.

#[cfg(feature = "laserstream")]
pub mod laserstream;
#[cfg(feature = "shredstream")]
pub mod shred;
pub mod state;
pub mod wss;

pub use state::*;
pub use wss::*;
