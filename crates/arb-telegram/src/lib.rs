//! Operator-facing Telegram bot.
//!
//! Phase 9B — the canonical Telegram interface. The scout's
//! `telegram.rs` / `telegram_commands.rs` still own the production code
//! path (they reference `arb_scout::scout::ScoutMetrics`, which is
//! scout-specific). Once the orchestrator's `BotMetrics` lands in this
//! crate (post Phase 10), the scout's modules will be retired and their
//! command handlers will move here behind a `MetricsSource` trait.
