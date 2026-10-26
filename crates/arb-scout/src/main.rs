//! Phase −1 paper-trading scout binary.
//!
//! Per `INSTRUCTIONS.md`: connect to Helius Enhanced WebSockets, monitor a
//! curated list of pools, compute hypothetical arbitrage on every update,
//! log to JSONL, send hourly Telegram summaries. **Submits NOTHING.**
//!
//! Run:
//! ```bash
//! cp .env.example .env  # then fill in HELIUS_API_KEY etc.
//! cargo run --release --bin arb-scout -- config/scout-full.toml
//! ```

use std::path::PathBuf;

use arb_scout::config::ScoutConfig;
use arb_scout::scout::Scout;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logging: structured JSON to stdout if RUST_LOG_JSON=1, otherwise pretty.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,arb_scout=info"));
    if std::env::var("RUST_LOG_JSON").ok().as_deref() == Some("1") {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .compact()
            .init();
    }

    let config_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("config/scout-full.toml"));
    info!(?config_path, "loading scout config");

    let cfg = match ScoutConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "failed to load config");
            return Err(anyhow::anyhow!("{e}"));
        }
    };
    info!(
        pools  = cfg.pools.len(),
        rpc    = %scrub(&cfg.helius_rpc_url),
        wss    = %scrub(&cfg.helius_wss_url),
        log_dir = ?cfg.log_dir,
        "scout configured"
    );

    // Load wallet if present. Failure to parse a SET env var is fatal —
    // silent fallback would leave us thinking we have a wallet when we don't.
    let wallet = match arb_scout::wallet::Wallet::from_env() {
        Ok(Some(w)) => {
            let bal = arb_scout::wallet::fetch_balance(&cfg.helius_rpc_url, &w.pubkey()).await
                .unwrap_or(arb_scout::wallet::WalletBalance { sol_lamports: 0, usdc_base_units: 0 });
            arb_scout::wallet::log_wallet_status(&w, &bal);
            Some(w)
        }
        Ok(None) => {
            tracing::info!(target: "scout::wallet",
                "no SOLANA_KEYPAIR_JSON set — running in scout-only mode (no execution)");
            None
        }
        Err(e) => {
            tracing::error!(target: "scout::wallet", error = %e,
                "SOLANA_KEYPAIR_JSON is set but malformed — refusing to start");
            return Err(anyhow::anyhow!("{e}"));
        }
    };

    let scout = Scout::new_with_wallet(cfg, wallet)?;

    // tokio::try_join! per the example main.rs in INSTRUCTIONS.md Phase −1.
    let r = tokio::try_join!(
        scout.run_enhanced_websocket(),
        scout.run_telegram_summary(),
        scout.run_telegram_commands(),
        scout.run_jito_correlator(),
        scout.run_stats_dump(),
        scout.run_sol_price_refresh(),
        scout.run_jupiter_poll(),
        scout.run_pump_fun_detector(),
        scout.run_cross_stable_arb(),
        scout.run_sol_accumulator(),
        scout.run_solend_liquidator(),
        scout.run_pump_fun_executor(),
        scout.run_wallet_watch(),
        scout.run_pump_fun_tp_sl(),
        scout.run_shred_subscriber(),
    );
    match r {
        Ok(_) => Ok(()),
        Err(e) => {
            error!(error = %e, "scout exited with error");
            Err(anyhow::anyhow!("{e}"))
        }
    }
}

fn scrub(url: &str) -> String {
    if let Some(idx) = url.find("?api-key=") {
        format!("{}***", &url[..idx + "?api-key=".len()])
    } else {
        url.to_string()
    }
}
