//! Configuration loading for the scout.
//!
//! Loads `.env` (via `dotenvy`) and a TOML file pointed to by the
//! `SCOUT_CONFIG` env var (default `config/scout-full.toml`). Validates
//! aggressively at startup so we fail at boot rather than mid-run.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use arb_core::{ArbError, ArbResult, PoolKind};
use serde::Deserialize;
use solana_program::pubkey::Pubkey;

#[derive(Debug, Clone, Deserialize)]
pub struct ScoutConfigFile {
    #[serde(default)]
    pub pools: Vec<MonitoredPoolEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MonitoredPoolEntry {
    pub name:     String,
    pub address:  String,
    pub dex:      String,
    /// Optional override of the pool's first token mint. Most parsers can
    /// derive this from the on-chain account, so leaving it blank is fine.
    pub token_a:  Option<String>,
    pub token_b:  Option<String>,
}

/// Fully validated scout configuration ready for use at runtime.
#[derive(Debug, Clone)]
pub struct ScoutConfig {
    pub helius_api_key:        String,
    pub helius_rpc_url:        String,
    pub helius_wss_url:        String,
    pub jito_explorer_url:     String,

    pub telegram_bot_token:    Option<String>,
    pub telegram_chat_id:      Option<i64>,

    pub solend_flash_fee_bps:  u64,

    pub priority_fee_microlamports_per_cu: u64,
    pub compute_unit_limit:    u64,
    pub base_tip_lamports:     u64,
    pub profit_tip_percent:    u64,
    pub probe_amount_usdc:     u64,

    /// Cycle length cap for route enumeration (2..=5). Default 4. Higher values
    /// surface more candidate cycles at the cost of CPU during route rebuild
    /// (5 hops with 17 pools ≈ 50k cycles per WSS update).
    pub max_hops:              usize,
    /// Min input size for the optimal-size ternary search, in USDC base units.
    pub size_search_min_usdc:  u64,
    /// Max input size for the optimal-size ternary search, in USDC base units.
    pub size_search_max_usdc:  u64,

    pub pools:                 Vec<MonitoredPool>,

    pub log_dir:               PathBuf,
}

#[derive(Debug, Clone)]
pub struct MonitoredPool {
    pub name:     String,
    pub address:  Pubkey,
    pub kind:     PoolKind,
    pub token_a:  Option<Pubkey>,
    pub token_b:  Option<Pubkey>,
}

impl ScoutConfig {
    /// Load `.env`, then the TOML file at `path`, then merge & validate.
    pub fn load(path: impl AsRef<Path>) -> ArbResult<Self> {
        // Best-effort: .env may not exist in production (Railway sets vars
        // directly), that's OK. But if it DOES exist and parsing fails partway,
        // surface the error loudly — silent partial loads have wasted hours
        // before (a stray backslash in a Windows path stops dotenvy).
        match dotenvy::dotenv() {
            Ok(path) => tracing::info!(?path, "loaded .env"),
            Err(dotenvy::Error::Io(io_err)) if io_err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!("no .env file found (using OS environment only)");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "failed to fully parse .env — vars defined AFTER the bad line will be missing. \
                     Common cause on Windows: backslashes in paths (use forward slashes or quote \
                     the value). Look for the offending line near where parsing stopped."
                );
            }
        }

        let toml_str = std::fs::read_to_string(path.as_ref())
            .map_err(|e| ArbError::Config(format!(
                "could not read scout config {}: {e}", path.as_ref().display()
            )))?;
        let file: ScoutConfigFile = toml::from_str(&toml_str)
            .map_err(|e| ArbError::Config(format!("scout config parse: {e}")))?;

        let helius_api_key = env_required("HELIUS_API_KEY")?;
        let helius_rpc_url = env_or_default(
            "HELIUS_RPC_URL",
            &crate::rpc::helius_rpc_url(&helius_api_key),
        );
        // Per INSTRUCTIONS.md Phase −1: the scout uses Helius Atlas Enhanced
        // WebSockets (`wss://atlas-mainnet.helius-rpc.com`), bundled with the
        // Developer plan ($49/mo). NOT the same host as the standard mainnet
        // WSS — Atlas is the Enhanced surface.
        let helius_wss_url = env_or_default(
            "HELIUS_ENHANCED_WSS_URL",
            &format!("wss://atlas-mainnet.helius-rpc.com/?api-key={helius_api_key}"),
        );

        let pools = file.pools.into_iter().map(parse_pool).collect::<ArbResult<_>>()?;

        let cfg = ScoutConfig {
            helius_api_key,
            helius_rpc_url,
            helius_wss_url,
            jito_explorer_url: env_or_default("JITO_EXPLORER_URL", "https://bundles.jito.wtf"),

            telegram_bot_token: parse_telegram_token(),
            telegram_chat_id:   parse_telegram_chat_id(),

            solend_flash_fee_bps: env_u64("SOLEND_FLASH_LOAN_FEE_BPS", 9)?,

            priority_fee_microlamports_per_cu:
                env_u64("SCOUT_PRIORITY_FEE_MICROLAMPORTS_PER_CU", 50_000)?,
            compute_unit_limit: env_u64("SCOUT_COMPUTE_UNIT_LIMIT", 600_000)?,
            base_tip_lamports:  env_u64("SCOUT_BASE_TIP_LAMPORTS",  10_000)?,
            profit_tip_percent: env_u64("SCOUT_PROFIT_TIP_PERCENT", 30)?,
            probe_amount_usdc:  env_u64("SCOUT_PROBE_AMOUNT_USDC",  100_000_000)?,

            max_hops:             env_u64("SCOUT_MAX_HOPS", 4)?.clamp(2, 8) as usize,
            size_search_min_usdc: env_u64("SCOUT_SIZE_SEARCH_MIN_USDC", 1_000_000)?,           // $1
            // Lowered from $100K → $5K. With our spot-price approximations
            // (Whirlpool/CLMM/DLMM/Lifinity), oversized inputs make the
            // quote optimistic (no slippage modelled past current tick/bin).
            // The earlier $100K cap caused the ternary search to overshoot
            // and report inflated profits — every recorded opportunity
            // landed in the ≥$20 bucket. $5K is a more honest probe ceiling
            // until full tick/bin-walking math is in place.
            size_search_max_usdc: env_u64("SCOUT_SIZE_SEARCH_MAX_USDC", 5_000_000_000)?,      // $5K

            pools,

            log_dir: PathBuf::from(env_or_default("SCOUT_LOG_DIR", "logs")),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> ArbResult<()> {
        if self.helius_api_key.is_empty() || self.helius_api_key.starts_with("put-") {
            return Err(ArbError::Config(
                "HELIUS_API_KEY is unset or still the placeholder".into(),
            ));
        }
        if !self.helius_wss_url.starts_with("wss://") && !self.helius_wss_url.starts_with("ws://") {
            return Err(ArbError::Config(format!(
                "HELIUS_ENHANCED_WSS_URL must be ws:// or wss://, got {}", self.helius_wss_url
            )));
        }
        if self.profit_tip_percent > 100 {
            return Err(ArbError::Config(
                "SCOUT_PROFIT_TIP_PERCENT must be 0..=100".into(),
            ));
        }
        if self.compute_unit_limit == 0 || self.compute_unit_limit > 1_400_000 {
            return Err(ArbError::Config(
                "SCOUT_COMPUTE_UNIT_LIMIT must be 1..=1_400_000".into(),
            ));
        }
        if self.pools.is_empty() {
            return Err(ArbError::Config(
                "no pools configured — supply at least one entry under [[pools]]".into(),
            ));
        }
        Ok(())
    }
}

/// Parse `TELEGRAM_BOT_TOKEN` with explicit logging on each rejection reason
/// so the operator can see why Telegram delivery is disabled.
fn parse_telegram_token() -> Option<String> {
    match std::env::var("TELEGRAM_BOT_TOKEN") {
        Err(_) => {
            tracing::warn!("Telegram disabled: TELEGRAM_BOT_TOKEN env var is not set");
            None
        }
        Ok(s) if s.is_empty() => {
            tracing::warn!("Telegram disabled: TELEGRAM_BOT_TOKEN is empty");
            None
        }
        Ok(s) if s.starts_with("put-") => {
            tracing::warn!("Telegram disabled: TELEGRAM_BOT_TOKEN still has the placeholder value (`put-...`)");
            None
        }
        Ok(s) => {
            // Token format: <bot_id>:<35-char-secret>
            let trimmed = s.trim();
            if trimmed != s {
                tracing::warn!("TELEGRAM_BOT_TOKEN had leading/trailing whitespace; using trimmed value");
            }
            if !trimmed.contains(':') {
                tracing::warn!(
                    "Telegram disabled: TELEGRAM_BOT_TOKEN doesn't look like `<bot_id>:<secret>` (missing colon)"
                );
                return None;
            }
            tracing::info!("Telegram bot token loaded ({}…, {} chars)",
                &trimmed[..trimmed.len().min(8)], trimmed.len());
            Some(trimmed.to_string())
        }
    }
}

fn parse_telegram_chat_id() -> Option<i64> {
    match std::env::var("TELEGRAM_OPERATOR_CHAT_ID") {
        Err(_) => {
            tracing::warn!("Telegram disabled: TELEGRAM_OPERATOR_CHAT_ID env var is not set");
            None
        }
        Ok(raw) => {
            let s = raw.trim().trim_matches(|c| c == '"' || c == '\'');
            if s.is_empty() {
                tracing::warn!("Telegram disabled: TELEGRAM_OPERATOR_CHAT_ID is empty");
                return None;
            }
            if s.starts_with("put-") {
                tracing::warn!("Telegram disabled: TELEGRAM_OPERATOR_CHAT_ID still has the placeholder value (`put-...`)");
                return None;
            }
            match s.parse::<i64>() {
                Err(e) => {
                    tracing::warn!(
                        value = %s, error = %e,
                        "Telegram disabled: TELEGRAM_OPERATOR_CHAT_ID doesn't parse as i64. \
                         Group chats are negative (-100…); private chats are positive. \
                         Get yours by sending a message to @userinfobot or @RawDataBot."
                    );
                    None
                }
                Ok(0) => {
                    tracing::warn!("Telegram disabled: TELEGRAM_OPERATOR_CHAT_ID is 0");
                    None
                }
                Ok(v) => {
                    tracing::info!("Telegram chat id loaded: {v}");
                    Some(v)
                }
            }
        }
    }
}

fn env_required(key: &str) -> ArbResult<String> {
    std::env::var(key).map_err(|_| ArbError::Config(format!("missing required env var: {key}")))
}

fn env_or_default(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_u64(key: &str, default: u64) -> ArbResult<u64> {
    match std::env::var(key) {
        Ok(s) => s.parse::<u64>().map_err(|e| ArbError::Config(format!("{key}: {e}"))),
        Err(_) => Ok(default),
    }
}

fn parse_pool(e: MonitoredPoolEntry) -> ArbResult<MonitoredPool> {
    // Move the name out up front so we can both quote it from the error closure
    // (by capturing a reference) and place it into the result struct without a
    // borrow conflict.
    let MonitoredPoolEntry { name, address, dex, token_a, token_b } = e;

    let address = Pubkey::from_str(&address)
        .map_err(|err| ArbError::Config(format!("bad pubkey for pool {name}: {err}")))?;
    let kind = match dex.as_str() {
        "raydium-amm-v4"  => PoolKind::RaydiumAmmV4,
        "raydium-clmm"    => PoolKind::RaydiumClmm,
        "raydium-cpmm"    => PoolKind::RaydiumCpmm,
        "orca-whirlpool"  => PoolKind::OrcaWhirlpool,
        "meteora-dlmm"    => PoolKind::MeteoraDlmm,
        "meteora-damm-v2" => PoolKind::MeteoraDammV2,
        "phoenix"         => PoolKind::PhoenixMarket,
        "lifinity-v2"     => PoolKind::LifinityV2,
        "bonkswap"        => PoolKind::Bonkswap,
        "obric-v2"        => PoolKind::ObricV2,
        "sanctum-lst"     => PoolKind::SanctumLst,
        "pump-fun"        => PoolKind::PumpFun,
        other => return Err(ArbError::Config(format!(
            "unknown dex `{other}` for pool {name} — see PoolKind::id"
        ))),
    };
    let parse_opt = |s: Option<String>, label: &str| -> ArbResult<Option<Pubkey>> {
        s.map(|s| Pubkey::from_str(&s).map_err(|err| ArbError::Config(
            format!("bad {label} for pool {name}: {err}")
        ))).transpose()
    };
    let token_a_pk = parse_opt(token_a, "token_a")?;
    let token_b_pk = parse_opt(token_b, "token_b")?;
    Ok(MonitoredPool {
        name,
        address,
        kind,
        token_a: token_a_pk,
        token_b: token_b_pk,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pool_recognises_known_dex_ids() {
        let entry = MonitoredPoolEntry {
            name:    "test".into(),
            address: "11111111111111111111111111111111".into(),
            dex:     "raydium-amm-v4".into(),
            token_a: None,
            token_b: None,
        };
        let p = parse_pool(entry).unwrap();
        assert_eq!(p.kind, PoolKind::RaydiumAmmV4);
    }

    #[test]
    fn parse_pool_rejects_unknown_dex() {
        let entry = MonitoredPoolEntry {
            name:    "test".into(),
            address: "11111111111111111111111111111111".into(),
            dex:     "made-up-dex".into(),
            token_a: None,
            token_b: None,
        };
        assert!(parse_pool(entry).is_err());
    }

    #[test]
    fn parse_pool_rejects_bad_pubkey() {
        let entry = MonitoredPoolEntry {
            name:    "test".into(),
            address: "not-a-pubkey".into(),
            dex:     "raydium-amm-v4".into(),
            token_a: None,
            token_b: None,
        };
        assert!(parse_pool(entry).is_err());
    }
}
