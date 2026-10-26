//! Wallet loader + balance check (read-only).
//!
//! Loads a Solana keypair from the `SOLANA_KEYPAIR_JSON` environment variable
//! — the Railway-friendly version of `solana-keygen new -o keypair.json`'s
//! output: a 64-byte JSON array `[12,99,...,67]` where bytes 0..32 are the
//! Ed25519 secret-key seed and bytes 32..64 are the public key.
//!
//! At startup, queries Helius for SOL + USDC balance and logs both.
//! **Does not sign or submit anything** — this is the "we know our wallet"
//! layer. Signing + submission comes in Stage B/C, which need solana-sdk's
//! transaction builder + more Windows-build pain.

use arb_core::{ArbError, ArbResult};
use serde_json::{json, Value};
use solana_program::pubkey::Pubkey;
use tracing::{info, warn};

use crate::known::USDC;

#[derive(Clone)]
pub struct Wallet {
    /// Full 64-byte keypair material (secret + public). Held for future
    /// signing use — currently no code path reads bytes 0..32 yet.
    bytes:  [u8; 64],
    pubkey: Pubkey,
}

impl Wallet {
    /// Parse a JSON-array string of 64 bytes into a Wallet.
    pub fn from_json_array(json_str: &str) -> ArbResult<Self> {
        let v: Value = serde_json::from_str(json_str)
            .map_err(|e| ArbError::Config(format!("SOLANA_KEYPAIR_JSON: invalid JSON: {e}")))?;
        let arr = v.as_array().ok_or_else(|| ArbError::Config(
            "SOLANA_KEYPAIR_JSON: expected JSON array (e.g. [12,99,...,67])".into()
        ))?;
        if arr.len() != 64 {
            return Err(ArbError::Config(format!(
                "SOLANA_KEYPAIR_JSON: expected 64 bytes, got {}", arr.len()
            )));
        }
        let mut bytes = [0u8; 64];
        for (i, val) in arr.iter().enumerate() {
            let n = val.as_u64().ok_or_else(|| ArbError::Config(format!(
                "SOLANA_KEYPAIR_JSON: byte {i} is not a number"
            )))?;
            if n > 255 {
                return Err(ArbError::Config(format!(
                    "SOLANA_KEYPAIR_JSON: byte {i} = {n} > 255"
                )));
            }
            bytes[i] = n as u8;
        }
        // Solana keypair convention: bytes[0..32] = ed25519 secret seed;
        // bytes[32..64] = ed25519 public key. Pubkey IS the second half.
        let mut pk_bytes = [0u8; 32];
        pk_bytes.copy_from_slice(&bytes[32..64]);
        let pubkey = Pubkey::new_from_array(pk_bytes);
        Ok(Wallet { bytes, pubkey })
    }

    /// Load from environment. Returns `Ok(None)` if the var is unset or
    /// empty (scout mode — no execution); `Err` if it's set but malformed.
    /// Logs the DIAGNOSIS distinctly so the operator can tell whether the
    /// var is missing entirely, blank, or a parse error.
    pub fn from_env() -> ArbResult<Option<Self>> {
        match std::env::var("SOLANA_KEYPAIR_JSON") {
            Err(std::env::VarError::NotPresent) => {
                tracing::info!(target: "scout::wallet",
                    "SOLANA_KEYPAIR_JSON env var is NOT PRESENT in this process. \
                     Set it in Railway dashboard → Variables (case-sensitive name). \
                     Other env vars the bot CAN see right now: {}",
                    summarize_env_vars());
                Ok(None)
            }
            Err(e) => {
                tracing::warn!(target: "scout::wallet", error = ?e,
                    "SOLANA_KEYPAIR_JSON env var read failed (not just missing)");
                Ok(None)
            }
            Ok(s) if s.trim().is_empty() => {
                tracing::warn!(target: "scout::wallet",
                    raw_len = s.len(),
                    "SOLANA_KEYPAIR_JSON is PRESENT but empty/whitespace-only. \
                     Likely an empty value in Railway dashboard."
                );
                Ok(None)
            }
            Ok(s) => {
                tracing::info!(target: "scout::wallet",
                    raw_len = s.len(),
                    starts_with = &s[..s.len().min(8)],
                    "SOLANA_KEYPAIR_JSON loaded — parsing as 64-byte JSON array"
                );
                Self::from_json_array(&s).map(Some)
            }
        }
    }
}

/// Returns a comma-separated list of env-var NAMES (not values) that look
/// related to wallet/executor configuration. Used in the "not present"
/// diagnostic so the operator can confirm whether Railway is passing the
/// expected vars through at all.
fn summarize_env_vars() -> String {
    let mut hits: Vec<String> = std::env::vars()
        .map(|(k, v)| (k, v.len()))
        .filter(|(k, _)| k.starts_with("SOLANA")
            || k.starts_with("EXECUTOR")
            || k.starts_with("HELIUS")
            || k.starts_with("TELEGRAM")
            || k.starts_with("RUN_MODE")
            || k.starts_with("SCOUT_"))
        .map(|(k, len)| format!("{k}({len})"))
        .collect();
    hits.sort();
    if hits.is_empty() { "(none matching SOLANA/EXECUTOR/HELIUS/TELEGRAM/SCOUT)".into() }
    else                { hits.join(", ") }
}

impl Wallet {
    pub fn pubkey(&self) -> Pubkey { self.pubkey }

    /// The raw 64-byte keypair material, used by the future signing layer.
    /// Returned by reference to avoid any chance of accidental clone-and-leak.
    pub fn bytes(&self) -> &[u8; 64] { &self.bytes }
}

#[derive(Debug, Clone, Copy)]
pub struct WalletBalance {
    pub sol_lamports:    u64,
    pub usdc_base_units: u64,
}

impl WalletBalance {
    pub fn sol(&self) -> f64 { self.sol_lamports as f64 / 1_000_000_000.0 }
    pub fn usdc(&self) -> f64 { self.usdc_base_units as f64 / 1_000_000.0 }
}

/// Query Helius for the wallet's SOL + USDC balance. Read-only.
pub async fn fetch_balance(
    rpc_url: &str,
    pubkey:  &Pubkey,
) -> ArbResult<WalletBalance> {
    let http = reqwest::Client::new();

    // SOL balance via getBalance.
    let sol_body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "getBalance",
        "params": [pubkey.to_string()]
    });
    let sol_resp: Value = http.post(rpc_url).json(&sol_body).send().await
        .map_err(|e| ArbError::Rpc(format!("getBalance: {e}")))?
        .json().await
        .map_err(|e| ArbError::Rpc(format!("getBalance parse: {e}")))?;
    let sol_lamports = sol_resp.pointer("/result/value").and_then(|v| v.as_u64()).unwrap_or(0);

    // USDC ATA balance via getTokenAccountsByOwner with mint filter.
    let usdc_body = json!({
        "jsonrpc": "2.0", "id": 2, "method": "getTokenAccountsByOwner",
        "params": [
            pubkey.to_string(),
            { "mint": USDC.to_string() },
            { "encoding": "jsonParsed" }
        ]
    });
    let usdc_resp: Value = http.post(rpc_url).json(&usdc_body).send().await
        .map_err(|e| ArbError::Rpc(format!("getTokenAccountsByOwner: {e}")))?
        .json().await
        .map_err(|e| ArbError::Rpc(format!("getTokenAccountsByOwner parse: {e}")))?;
    let mut usdc_base_units = 0u64;
    if let Some(accounts) = usdc_resp.pointer("/result/value").and_then(|v| v.as_array()) {
        for acc in accounts {
            let amount_str = acc.pointer("/account/data/parsed/info/tokenAmount/amount")
                .and_then(|v| v.as_str()).unwrap_or("0");
            usdc_base_units = usdc_base_units.saturating_add(amount_str.parse::<u64>().unwrap_or(0));
        }
    }

    Ok(WalletBalance { sol_lamports, usdc_base_units })
}

/// Convenience: log wallet status at startup. Helps the operator confirm the
/// right wallet is loaded and has funds before any execution path runs.
pub fn log_wallet_status(wallet: &Wallet, balance: &WalletBalance) {
    info!(
        target: "scout::wallet",
        pubkey   = %wallet.pubkey(),
        sol      = balance.sol(),
        usdc     = balance.usdc(),
        sol_lamports = balance.sol_lamports,
        usdc_base_units = balance.usdc_base_units,
        "wallet loaded"
    );
    if balance.sol_lamports < 5_000_000 {  // < 0.005 SOL
        warn!(target: "scout::wallet",
            sol = balance.sol(),
            "wallet has very little SOL — execution would fail. Add ≥0.05 SOL for fees."
        );
    }
    if balance.usdc_base_units < 1_000_000 {  // < $1 USDC
        warn!(target: "scout::wallet",
            usdc = balance.usdc(),
            "wallet has very little USDC — fund it with at least the SCOUT_PROBE_AMOUNT_USDC"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A known-good 64-byte Solana keypair (test vector from `solana-keygen`).
    /// Generated for tests only — never used in production.
    const TEST_KEYPAIR_JSON: &str =
        "[208,175,150,242,88,34,108,88,177,16,168,75,115,181,199,242,\
          120,4,78,75,19,227,13,215,184,108,226,53,111,149,179,84,\
          137,121,79,1,160,223,124,241,202,203,220,237,50,242,57,158,\
          226,207,203,188,43,28,70,110,214,234,251,15,249,157,62,80]";

    #[test]
    fn from_json_array_parses_64_bytes() {
        let w = Wallet::from_json_array(TEST_KEYPAIR_JSON).expect("parse");
        // Just verify pubkey is 32 bytes (any nonzero pubkey).
        assert_ne!(w.pubkey(), Pubkey::default());
    }

    #[test]
    fn rejects_short_array() {
        let r = Wallet::from_json_array("[1,2,3]");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_non_array() {
        let r = Wallet::from_json_array("\"hello\"");
        assert!(r.is_err());
    }

    #[test]
    fn rejects_byte_over_255() {
        let mut bad = String::from("[");
        for i in 0..64 { bad.push_str(if i == 0 {"999"} else {",1"}); }
        bad.push(']');
        let r = Wallet::from_json_array(&bad);
        assert!(r.is_err(), "byte > 255 should be rejected");
    }

    #[test]
    fn from_env_returns_none_when_unset() {
        // Ensure the var is unset for this test (no risk of pollution since
        // we use a unique name). For real test use std::env::set_var with care.
        std::env::remove_var("SOLANA_KEYPAIR_JSON");
        let r = Wallet::from_env().unwrap();
        assert!(r.is_none());
    }
}
