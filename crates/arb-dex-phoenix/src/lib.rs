//! Phoenix — central limit orderbook DEX.
//!
//! Phoenix's quote walks bid or ask levels rather than evaluating an AMM
//! curve, so the crate exposes both `quote()` (best-effort fill across price
//! levels) and `walk_book()` for tighter sizing decisions in the pathfinder.
//! Source: <https://github.com/Ellipsis-Labs/phoenix-v1>.
//!
//! Phase 0 stub. Full integration lands in Phase 12 (Wave 3).
