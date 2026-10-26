//! Lifinity v2 — proactive market maker with internal Pyth oracle anchoring.
//!
//! Phase 12 (Wave 3). The quote depends on the oracle price *and* current
//! pool inventory: as inventory drifts from the oracle midpoint, the
//! effective price worsens for trades that worsen the imbalance and
//! improves for trades that restore it. Implementations must read both
//! the pool account and the linked oracle.

pub mod lifinity;
pub use lifinity::*;
