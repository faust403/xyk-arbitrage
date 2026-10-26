//! Library surface of the Phase −1 paper-trading scout.
//!
//! `main.rs` provides the binary entry point but most logic lives in modules
//! that are also reachable from integration tests via this `lib.rs` re-export.

pub mod config;
pub mod dex;
pub mod executor;
pub mod fees;
pub mod jito_correlator;
pub mod jupiter;
pub mod jupiter_swap;
pub mod known;
pub mod native_buffer;
pub mod native_executor;
pub mod pump_fun;
pub mod pump_sniper_budget;
pub mod raw_tx;
pub mod alt;
pub mod rpc;
pub mod v0_tx;
pub mod scout;
pub mod solend_liq;
pub mod marginfi_liq;
pub mod kamino_liq;
pub mod kamino_scope;
pub mod tpu_send;
pub mod pyth_shred_dump;
pub mod shred_alert;
pub mod signing;
pub mod telegram;
pub mod telegram_commands;
pub mod vault_store;
pub mod pyth_store;
pub mod wallet;

pub use scout::{Scout, ScoutMetrics};
