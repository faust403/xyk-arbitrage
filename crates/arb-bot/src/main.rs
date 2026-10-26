//! arb-bot — main binary.
//!
//! Per `INSTRUCTIONS.md` Phase 10, this binary loads `Config`, wires the
//! streaming backend, pathfinder, executor, telegram bot, and telemetry,
//! and exposes a `/health` endpoint on `:9090`.
//!
//! At present (post Phase 9 extraction) the orchestration code lives inside
//! the scout binary, which depends on the same shared crates as this one.
//! This binary's job in the current cycle is to be a thin "smoke" entry
//! point: it loads + validates the configuration, prints a startup banner,
//! and exits, so deployment automation can verify env-var wiring before the
//! full strategy loop comes online.

use std::process::ExitCode;

use arb_config::{Config, RunMode};
use tracing::{error, info};

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn main() -> ExitCode {
    init_logging();

    // Best-effort .env load — Railway sets variables directly, missing .env is fine.
    let _ = dotenvy::dotenv();

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to load config");
            return ExitCode::from(2);
        }
    };

    info!(
        mode = ?cfg.mode,
        rpc.http = %cfg.rpc.http_endpoint,
        rpc.wss = %cfg.rpc.wss_endpoint,
        pools = cfg.pools.len(),
        dexes_enabled = cfg.dexes.enabled.len(),
        "arb-bot configured",
    );

    // Hard rule from INSTRUCTIONS.md hard-rule #10 / Phase 11.4:
    // mainnet starts with explicit operator approval and capped capital.
    // Refuse to enter the strategy loop when mode == Mainnet until the
    // operator has confirmed via Telegram (deferred to Phase 11.4 hookup).
    if cfg.mode == RunMode::Mainnet {
        error!(
            "mainnet mode reached arb-bot main but Phase 11.4 operator-confirm \
             gate is not yet implemented — refusing to start. Use the scout \
             binary for the current execution path until the assert-program is \
             deployed and the bundle builder is wired."
        );
        return ExitCode::from(3);
    }

    info!("Phase 10 stub: configuration validated; full strategy loop pending.");
    ExitCode::SUCCESS
}
