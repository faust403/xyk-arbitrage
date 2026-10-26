//! Jito Block-Engine integration: bundle construction, tip-account rotation,
//! simulation, submission, status polling.
//!
//! Per `INSTRUCTIONS.md` Phase 8, the bundle builder must include the
//! `assert_min_profit` instruction (Phase 3) before the tip transfer. The
//! current `bundle_client` extracted from the scout submits arbitrary
//! payloads to Jito; the assert-wrapper `BundleBuilder` lands in a
//! follow-up commit on this same crate, gated by the Anchor program being
//! deployed (Phase 11.4 mainnet ready).

pub mod bundle_client;
pub mod tip_floor;
pub use bundle_client::*;
pub use tip_floor::*;
