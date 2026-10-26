//! Telemetry — JSONL opportunity / bundle attempt logging, P&L computation.
//!
//! Phase 9A — extracted from the scout — provides [`logger`] (`JsonlLogger`,
//! `OpportunityRecord`). The fuller `Telemetry` struct (atomic state writes,
//! P&L periods, bundle-attempt records) lands in a follow-up patch on this
//! crate once the scout's `state.rs` and `fees.rs` move here too.

pub mod logger;
pub use logger::*;
