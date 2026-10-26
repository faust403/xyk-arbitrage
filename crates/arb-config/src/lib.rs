//! Runtime configuration for the arbbot.
//!
//! Per `INSTRUCTIONS.md` Phase 2, this crate is the single source of truth
//! for everything the bot needs to start. The flow:
//!
//! 1. [`Config::load`] reads `.env` via [`dotenvy`] (best-effort — Railway
//!    sets vars directly, so a missing `.env` is fine).
//! 2. The TOML file is selected by `RUN_MODE` (`config/<mode>.toml`).
//! 3. `${VAR}` placeholders inside the TOML are substituted from the
//!    environment **before** `toml::from_str`, so the substituted values
//!    are typed normally rather than as raw strings.
//! 4. [`Config::validate`] runs every cross-field rule (Pubkey parses, URL
//!    schemes, min-profit > 0, mainnet-only requirements).
//!
//! Anything other than env-var-driven values lives in the TOML; secrets and
//! per-deployment values live in env vars.

use std::path::{Path, PathBuf};
use std::str::FromStr;

use arb_core::{ArbError, ArbResult};
use serde::Deserialize;
use solana_program::pubkey::Pubkey;

/// Runtime mode. Determines which TOML is loaded and which validation rules
/// apply (mainnet has stricter requirements than dryrun).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunMode {
    Scout,
    DryRun,
    Devnet,
    PaperTrading,
    Mainnet,
}

impl FromStr for RunMode {
    type Err = ArbError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "scout"        => Ok(Self::Scout),
            "dryrun"       => Ok(Self::DryRun),
            "devnet"       => Ok(Self::Devnet),
            "papertrading" => Ok(Self::PaperTrading),
            "mainnet"      => Ok(Self::Mainnet),
            other => Err(ArbError::Config(format!(
                "unknown RUN_MODE `{other}` (expected: scout, dryrun, devnet, papertrading, mainnet)"
            ))),
        }
    }
}

impl RunMode {
    /// Default config-file name for this mode (e.g. `Mainnet → mainnet.toml`).
    pub fn config_filename(&self) -> &'static str {
        match self {
            Self::Scout        => "scout.toml",
            Self::DryRun       => "dryrun.toml",
            Self::Devnet       => "devnet.toml",
            Self::PaperTrading => "papertrading.toml",
            Self::Mainnet      => "mainnet.toml",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RpcConfig {
    pub helius_api_key:        String,
    pub http_endpoint:         String,
    pub wss_endpoint:          String,
    /// LaserStream gRPC endpoint. Empty/None on Developer plan.
    #[serde(default)]
    pub laserstream_endpoint:  Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JitoConfig {
    pub block_engine_url:        String,
    #[serde(default = "default_jito_explorer")]
    pub explorer_url:            String,
    #[serde(default = "default_jito_tip")]
    pub default_tip_lamports:    u64,
}

fn default_jito_explorer() -> String { "https://bundles.jito.wtf".to_string() }
fn default_jito_tip() -> u64 { 10_000 }

#[derive(Debug, Clone, Deserialize)]
pub struct WalletConfig {
    /// JSON-array form of a 64-byte ed25519 keypair (`[1,2,...,64]`). Empty
    /// in scout/dryrun/papertrading; required in devnet/mainnet.
    #[serde(default)]
    pub keypair_json:        String,
    /// Deployed `assert_min_profit` program id. Placeholder is rejected in
    /// mainnet mode; tolerated in dryrun/scout.
    pub assert_program_id:   String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SolendConfig {
    pub lending_market:       String,
    pub usdc_reserve:         String,
    #[serde(default = "default_solend_fee_bps")]
    pub flash_loan_fee_bps:   u64,
}

fn default_solend_fee_bps() -> u64 { 9 }

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token:        Option<String>,
    #[serde(default)]
    pub operator_chat_id: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    pub min_profit_lamports:           u64,
    pub max_flashloan_amount_usdc:     u64,
    #[serde(default = "default_concurrent")]
    pub max_concurrent_opportunities:  u32,
}

fn default_concurrent() -> u32 { 4 }

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DexesConfig {
    /// `dex_id` strings (matching `PoolKind::id`) that are enabled in this
    /// run. Empty `[]` means "all configured DEXs are enabled".
    #[serde(default)]
    pub enabled: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MonitoredPool {
    pub name:    String,
    pub address: String,
    pub dex:     String,
}

/// Top-level loaded + validated configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub mode:     RunMode,
    pub rpc:      RpcConfig,
    pub jito:     JitoConfig,
    pub wallet:   WalletConfig,
    pub solend:   SolendConfig,
    #[serde(default)]
    pub telegram: TelegramConfig,
    pub risk:     RiskConfig,
    #[serde(default)]
    pub dexes:    DexesConfig,
    #[serde(default)]
    pub pools:    Vec<MonitoredPool>,
}

impl Config {
    /// Convenience: load using the default `config/` directory and the mode
    /// indicated by `RUN_MODE`.
    pub fn load() -> ArbResult<Self> {
        let mode_str = std::env::var("RUN_MODE")
            .map_err(|_| ArbError::Config("RUN_MODE env var not set".into()))?;
        let mode: RunMode = mode_str.parse()?;
        Self::load_from_dir("config", mode)
    }

    /// Load `<dir>/<mode>.toml`, substitute `${VAR}` against the environment,
    /// parse, and validate. Tests use this with a tempfile.
    pub fn load_from_dir(dir: impl AsRef<Path>, mode: RunMode) -> ArbResult<Self> {
        // .env is best-effort: missing on Railway is normal, malformed is
        // worth shouting about.
        match dotenvy::dotenv() {
            Ok(_)  => {}
            Err(dotenvy::Error::Io(io_err)) if io_err.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!(error = %e, ".env present but failed to fully parse"),
        }
        let path = PathBuf::from(dir.as_ref()).join(mode.config_filename());
        Self::load_from_file(&path)
    }

    /// Load a specific TOML path (used by tests; production uses `load`).
    pub fn load_from_file(path: &Path) -> ArbResult<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| ArbError::Config(format!("read {}: {e}", path.display())))?;
        let substituted = substitute_env(&raw)?;
        let cfg: Config = toml::from_str(&substituted)
            .map_err(|e| ArbError::Config(format!("parse {}: {e}", path.display())))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Cross-field validation. Aggressive on mainnet, lenient on dryrun/scout.
    pub fn validate(&self) -> ArbResult<()> {
        // 1. RPC URLs must have a scheme.
        if !self.rpc.http_endpoint.starts_with("http://")
            && !self.rpc.http_endpoint.starts_with("https://")
        {
            return Err(ArbError::Config(format!(
                "rpc.http_endpoint must be http:// or https://, got `{}`",
                self.rpc.http_endpoint
            )));
        }
        if !self.rpc.wss_endpoint.starts_with("ws://")
            && !self.rpc.wss_endpoint.starts_with("wss://")
        {
            return Err(ArbError::Config(format!(
                "rpc.wss_endpoint must be ws:// or wss://, got `{}`",
                self.rpc.wss_endpoint
            )));
        }

        // 2. Helius key must be present and not the placeholder for any mode
        //    that talks to a real network. Scout uses Helius too, so this
        //    applies everywhere except in test fixtures (which set a real
        //    placeholder string, not the literal `put-your-key-here`).
        if self.rpc.helius_api_key.is_empty()
            || self.rpc.helius_api_key.starts_with("put-")
        {
            return Err(ArbError::Config(
                "rpc.helius_api_key is empty or still the .env.example placeholder".into(),
            ));
        }

        // 3. min_profit_lamports must be > 0 (zero would let a worse-than-
        //    breakeven trade through).
        if self.risk.min_profit_lamports == 0 {
            return Err(ArbError::Config("risk.min_profit_lamports must be > 0".into()));
        }

        // 4. Solend pubkeys must parse (for any mode that may use flash loans;
        //    skipping for scout/dryrun keeps the field optional in test
        //    fixtures, but parse-failures are still surfaced).
        Pubkey::from_str(&self.solend.lending_market)
            .map_err(|e| ArbError::Config(format!("solend.lending_market: {e}")))?;
        Pubkey::from_str(&self.solend.usdc_reserve)
            .map_err(|e| ArbError::Config(format!("solend.usdc_reserve: {e}")))?;

        // 5. Pool addresses must parse; dex must be a known id. We don't
        //    re-validate `dex` here against PoolKind because arb-core already
        //    owns that mapping; downstream code converts at load time.
        for p in &self.pools {
            Pubkey::from_str(&p.address)
                .map_err(|e| ArbError::Config(format!("pool `{}`: {e}", p.name)))?;
        }

        // 6. Mainnet-only: keypair and assert program id must be real.
        if self.mode == RunMode::Mainnet {
            if self.wallet.keypair_json.is_empty() {
                return Err(ArbError::Config(
                    "mainnet mode requires wallet.keypair_json (set via env var)".into(),
                ));
            }
            if self.wallet.assert_program_id.starts_with("put-")
                || self.wallet.assert_program_id.is_empty()
            {
                return Err(ArbError::Config(
                    "mainnet mode requires a real wallet.assert_program_id (placeholder rejected)".into(),
                ));
            }
            // Also force the program-id to be a valid Pubkey at this point.
            Pubkey::from_str(&self.wallet.assert_program_id)
                .map_err(|e| ArbError::Config(format!("wallet.assert_program_id: {e}")))?;
        }

        // 7. Telegram chat id, if present, must be non-zero.
        if let Some(0) = self.telegram.operator_chat_id {
            return Err(ArbError::Config(
                "telegram.operator_chat_id is 0 — Telegram routing would silently misfire".into(),
            ));
        }
        Ok(())
    }
}

/// Replace every `${VAR}` in `input` with `std::env::var("VAR")`. Anything
/// that isn't a `${...}` placeholder passes through verbatim. A missing var
/// is a hard error — silently defaulting to `""` would let bad configs limp
/// into mainnet.
pub fn substitute_env(input: &str) -> ArbResult<String> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek().copied() == Some('{') {
            chars.next(); // consume '{'
            let mut var = String::new();
            let mut closed = false;
            for c2 in chars.by_ref() {
                if c2 == '}' { closed = true; break; }
                var.push(c2);
            }
            if !closed {
                return Err(ArbError::Config(format!(
                    "unclosed env-var reference `${{{var}` in config"
                )));
            }
            if var.is_empty() {
                return Err(ArbError::Config("empty `${{}}` in config".into()));
            }
            let val = std::env::var(&var).map_err(|_| {
                ArbError::Config(format!(
                    "config references env var `{var}` which is not set — \
                     check `.env` and Railway dashboard"
                ))
            })?;
            out.push_str(&val);
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Helper: write `body` to a temp file and load from it. Each test gets
    /// its own tempfile so they don't trample each other.
    fn load_str(body: &str) -> ArbResult<Config> {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        Config::load_from_file(f.path())
    }

    fn good_body() -> &'static str {
        r#"
mode = "dryrun"

[rpc]
helius_api_key  = "real-test-key-abc123"
http_endpoint   = "https://mainnet.helius-rpc.com/?api-key=x"
wss_endpoint    = "wss://atlas-mainnet.helius-rpc.com/?api-key=x"

[jito]
block_engine_url = "https://frankfurt.mainnet.block-engine.jito.wtf"

[wallet]
assert_program_id = "11111111111111111111111111111111"

[solend]
lending_market = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY"
usdc_reserve   = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw"

[risk]
min_profit_lamports       = 50000
max_flashloan_amount_usdc = 0
"#
    }

    #[test]
    fn test_run_mode_from_str_known_values() {
        assert_eq!("scout".parse::<RunMode>().unwrap(),       RunMode::Scout);
        assert_eq!("dryrun".parse::<RunMode>().unwrap(),      RunMode::DryRun);
        assert_eq!("Mainnet".parse::<RunMode>().unwrap(),     RunMode::Mainnet);
        assert_eq!("PAPERTRADING".parse::<RunMode>().unwrap(), RunMode::PaperTrading);
    }

    #[test]
    fn test_run_mode_from_str_rejects_unknown() {
        assert!("production".parse::<RunMode>().is_err());
    }

    #[test]
    fn test_config_load_dryrun_minimal() {
        let cfg = load_str(good_body()).unwrap();
        assert_eq!(cfg.mode, RunMode::DryRun);
        assert_eq!(cfg.risk.min_profit_lamports, 50_000);
        assert_eq!(cfg.jito.default_tip_lamports, 10_000); // default applied
        assert_eq!(cfg.solend.flash_loan_fee_bps, 9);      // default applied
    }

    #[test]
    fn test_config_substitutes_env_vars() {
        std::env::set_var("ARBBOT_TEST_KEY_42", "subst-value-XYZ");
        let body = good_body().replace(
            r#"helius_api_key  = "real-test-key-abc123""#,
            r#"helius_api_key  = "${ARBBOT_TEST_KEY_42}""#,
        );
        let cfg = load_str(&body).unwrap();
        assert_eq!(cfg.rpc.helius_api_key, "subst-value-XYZ");
        std::env::remove_var("ARBBOT_TEST_KEY_42");
    }

    #[test]
    fn test_config_missing_env_var_clear_error() {
        // No matching env var set.
        std::env::remove_var("ARBBOT_DEFINITELY_UNSET_VAR");
        let body = good_body().replace(
            r#"helius_api_key  = "real-test-key-abc123""#,
            r#"helius_api_key  = "${ARBBOT_DEFINITELY_UNSET_VAR}""#,
        );
        let err = load_str(&body).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("ARBBOT_DEFINITELY_UNSET_VAR"), "got: {msg}");
    }

    #[test]
    fn test_config_invalid_pubkey_rejected() {
        // Corrupt `solend.lending_market`, which is validated unconditionally
        // (assert_program_id is only validated in mainnet mode, so picking
        // that field would mean this test silently passes when run against
        // a dryrun fixture — exactly the gap that almost shipped here).
        let body = good_body().replace(
            "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY",
            "this-is-not-a-pubkey",
        );
        let err = load_str(&body).unwrap_err();
        assert!(format!("{err}").contains("lending_market"));
    }

    #[test]
    fn test_config_validates_min_profit_positive() {
        let body = good_body().replace(
            "min_profit_lamports       = 50000",
            "min_profit_lamports       = 0",
        );
        let err = load_str(&body).unwrap_err();
        assert!(format!("{err}").contains("min_profit_lamports"));
    }

    #[test]
    fn test_config_validates_url_format() {
        let body = good_body().replace(
            "https://mainnet.helius-rpc.com/?api-key=x",
            "tcp://nope",
        );
        let err = load_str(&body).unwrap_err();
        assert!(format!("{err}").contains("http_endpoint"));
    }

    #[test]
    fn test_config_mainnet_requires_assert_program_id() {
        // Mainnet mode + placeholder assert id → must reject.
        let body = good_body()
            .replace(r#"mode = "dryrun""#, r#"mode = "mainnet""#)
            .replace(
                r#"assert_program_id = "11111111111111111111111111111111""#,
                r#"assert_program_id = "put-deployed-program-id-here""#,
            );
        // For mainnet we also need a non-empty keypair, otherwise the
        // earlier check fires first; supply a non-empty bogus value so we
        // hit the assert-program check explicitly.
        let body = body.replace(
            "[wallet]",
            r#"[wallet]
keypair_json = "[1,2,3]""#,
        );
        let err = load_str(&body).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.to_lowercase().contains("assert_program_id"), "got: {msg}");
    }

    #[test]
    fn test_config_mainnet_requires_keypair() {
        let body = good_body().replace(r#"mode = "dryrun""#, r#"mode = "mainnet""#);
        let err = load_str(&body).unwrap_err();
        assert!(format!("{err}").to_lowercase().contains("keypair"));
    }

    #[test]
    fn test_config_telegram_zero_chat_id_rejected() {
        let body = format!(
            r#"{}

[telegram]
operator_chat_id = 0
"#, good_body()
        );
        let err = load_str(&body).unwrap_err();
        assert!(format!("{err}").contains("operator_chat_id"));
    }

    #[test]
    fn test_substitute_env_passes_through_when_no_placeholders() {
        let s = "no $variables here, just a literal { brace }";
        assert_eq!(substitute_env(s).unwrap(), s);
    }

    #[test]
    fn test_substitute_env_unclosed_placeholder_errors() {
        let err = substitute_env("hello ${UNCLOSED").unwrap_err();
        assert!(format!("{err}").contains("unclosed"));
    }

    #[test]
    fn test_substitute_env_empty_placeholder_errors() {
        let err = substitute_env("hello ${} world").unwrap_err();
        assert!(format!("{err}").contains("empty"));
    }
}
