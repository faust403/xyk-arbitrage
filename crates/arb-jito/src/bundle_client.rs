//! Jito Block Engine `sendBundle` + `simulateBundle` client.
//!
//! A Jito bundle is **atomic**: every transaction in it succeeds or every
//! transaction reverts. This is the safety guarantee we rely on to avoid
//! "leg1 swapped my USDC into SOL but leg2 failed and now I'm stuck".
//!
//! ## Multi-region fallback
//!
//! Jito Block Engines are deployed in five regions. Public access at each
//! is rate-limited (~1 sendBundle/sec/IP). When one region returns the
//! `Network congested. Endpoint is globally rate limited.` error
//! (`code: -32097`), we retry the next region. With 5 endpoints and
//! independent rate-limit windows we get ~5/sec effective throughput
//! before hitting the global Jito plane limit.
//!
//! Region order is configurable via `JITO_BLOCK_ENGINE_URL`
//! (comma-separated). Default order targets the geographically-distributed
//! pool with Amsterdam first (Helius routes most EU/US traffic through
//! there); operators in other regions should reorder accordingly.

use std::time::Duration;

use arb_core::{ArbError, ArbResult};
use serde_json::{json, Value};

/// Default region rotation order. The list was picked so that:
///  * Amsterdam first — closest to Helius's primary US-East/EU-West dual
///    routing, lowest p50 latency from Railway's default region.
///  * NY second — covers East-Coast validators.
///  * Frankfurt third — EU fallback.
///  * Tokyo + SLC for Pacific / US-West coverage as last-resort.
const DEFAULT_BLOCK_ENGINES: &[&str] = &[
    "https://amsterdam.mainnet.block-engine.jito.wtf",
    "https://ny.mainnet.block-engine.jito.wtf",
    "https://frankfurt.mainnet.block-engine.jito.wtf",
    "https://tokyo.mainnet.block-engine.jito.wtf",
    "https://slc.mainnet.block-engine.jito.wtf",
];

#[derive(Clone)]
pub struct JitoBundleClient {
    /// Ordered list of Block Engine base URLs. `send_bundle` walks the list
    /// in order and retries the next entry on rate-limit (`-32097`).
    block_engine_urls: Vec<String>,
    http:              reqwest::Client,
}

impl Default for JitoBundleClient {
    fn default() -> Self {
        // Allow operator override via env: comma-separated URLs.
        let urls: Vec<String> = match std::env::var("JITO_BLOCK_ENGINE_URLS") {
            Ok(s) if !s.trim().is_empty() => s.split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty()).collect(),
            _ => DEFAULT_BLOCK_ENGINES.iter().map(|s| s.to_string()).collect(),
        };
        Self::with_urls(urls)
    }
}

impl JitoBundleClient {
    /// Single-endpoint constructor — kept for backward compatibility with
    /// callers that pass `JitoBundleClient::new("https://...")`.
    pub fn new(block_engine_url: String) -> Self {
        Self::with_urls(vec![block_engine_url])
    }

    /// Multi-endpoint constructor. `urls` must be non-empty.
    pub fn with_urls(urls: Vec<String>) -> Self {
        let urls = if urls.is_empty() {
            DEFAULT_BLOCK_ENGINES.iter().map(|s| s.to_string()).collect()
        } else { urls };
        Self {
            block_engine_urls: urls,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .build().expect("reqwest"),
        }
    }

    /// Primary endpoint — kept for callers that just want a URL to log.
    pub fn primary_url(&self) -> &str {
        self.block_engine_urls.first().map(|s| s.as_str()).unwrap_or("")
    }

    /// Simulate a bundle WITHOUT sending. Hits the primary endpoint only —
    /// simulation isn't bound by the sendBundle rate limit, so no failover.
    pub async fn simulate_bundle(&self, tx_b64s: &[String]) -> ArbResult<Value> {
        let url = format!("{}/api/v1/bundles", self.primary_url());
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "simulateBundle",
            "params": [tx_b64s, { "encoding": "base64" }],
        });
        let resp: Value = self.http.post(&url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("simulateBundle: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("simulateBundle parse: {e}")))?;
        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("simulateBundle error: {err}")));
        }
        Ok(resp.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Submit a single tx via Helius Sender — bypasses both Jito's public
    /// rate limit AND validator-side mempool delays by routing through
    /// Helius's stake-weighted private path. Available on Business+ tier.
    ///
    /// Endpoint: `HELIUS_SENDER_URL` (e.g. `https://sender.helius-rpc.com/fast`).
    /// Returns the tx signature on success.
    ///
    /// Use this for atomic Jupiter swaps that don't need bundle semantics
    /// (single tx with flash loan wrap = one atomic operation). For multi-tx
    /// bundles, `send_bundle` to Jito is still required.
    pub async fn send_via_helius_sender(
        &self, sender_url: &str, api_key: &str, tx_b64: &str,
    ) -> ArbResult<String> {
        let url = format!("{sender_url}?api-key={api_key}");
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "sendTransaction",
            "params": [tx_b64, {
                "encoding": "base64",
                "skipPreflight": true,        // Sender pre-validates upstream
                "maxRetries": 0,              // We retry at the executor layer
            }],
        });
        let resp: Value = self.http.post(&url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("helius sender: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("helius sender parse: {e}")))?;
        if let Some(err) = resp.get("error") {
            return Err(ArbError::Rpc(format!("helius sender error: {err}")));
        }
        resp.get("result").and_then(|v| v.as_str()).map(|s| s.to_string())
            .ok_or_else(|| ArbError::Rpc(format!("helius sender: missing result: {resp}")))
    }

    /// Submit a bundle to Jito, falling over across regions on rate limit.
    ///
    /// On `code: -32097` ("Network congested. Endpoint is globally rate
    /// limited.") we move to the next endpoint in the list. Any other error
    /// (auth, parse, network) fails fast — those mean something other than
    /// throughput, and retrying a different region won't help.
    ///
    /// Returns the bundle UUID — does NOT confirm landing.
    pub async fn send_bundle(&self, tx_b64s: &[String]) -> ArbResult<String> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "sendBundle",
            "params": [tx_b64s, { "encoding": "base64" }],
        });
        let mut last_err: Option<ArbError> = None;
        for (i, base) in self.block_engine_urls.iter().enumerate() {
            let url = format!("{}/api/v1/bundles", base);
            let resp_result = self.http.post(&url).json(&body).send().await;
            let resp: Value = match resp_result {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = Some(ArbError::Rpc(format!("sendBundle parse [{i}]: {e}")));
                        continue;
                    }
                },
                Err(e) => {
                    last_err = Some(ArbError::Rpc(format!("sendBundle send [{i}]: {e}")));
                    continue;
                }
            };
            if let Some(err) = resp.get("error") {
                // -32097 = rate-limited. Try next region.
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                let msg  = err.get("message").and_then(|m| m.as_str()).unwrap_or("");
                if code == -32097 || msg.contains("rate limit") {
                    last_err = Some(ArbError::Rpc(format!(
                        "sendBundle rate-limited at {base}: {err}"
                    )));
                    continue;
                }
                // Any other error code → fail fast (not a region problem).
                return Err(ArbError::Rpc(format!("sendBundle error at {base}: {err}")));
            }
            return resp.get("result").and_then(|v| v.as_str()).map(|s| s.to_string())
                .ok_or_else(|| ArbError::Rpc(format!("sendBundle: missing result: {resp}")));
        }
        Err(last_err.unwrap_or_else(|| ArbError::Rpc(
            "sendBundle: all regions rate-limited or unreachable".to_string()
        )))
    }

    /// Poll bundle status. Tries each region until one returns a status (the
    /// bundle UUID is global — any region can answer). Returns
    /// `Some(landed_slot)` once landed, `None` if still pending.
    pub async fn bundle_status(&self, bundle_id: &str) -> ArbResult<Option<u64>> {
        let body = json!({
            "jsonrpc": "2.0", "id": 1, "method": "getBundleStatuses",
            "params": [[bundle_id]],
        });
        for base in &self.block_engine_urls {
            let url = format!("{}/api/v1/bundles", base);
            let Ok(r) = self.http.post(&url).json(&body).send().await else { continue; };
            let Ok(resp): Result<Value, _> = r.json().await else { continue; };
            let arr = resp.pointer("/result/value").and_then(|v| v.as_array()).cloned()
                .unwrap_or_default();
            for entry in arr {
                if let Some(slot) = entry.get("slot").and_then(|v| v.as_u64()) {
                    let status = entry.get("confirmation_status")
                        .and_then(|v| v.as_str()).unwrap_or("");
                    if matches!(status, "confirmed" | "finalized") {
                        return Ok(Some(slot));
                    }
                }
            }
            // First successful response wins — no need to try more regions.
            return Ok(None);
        }
        Err(ArbError::Rpc("bundle_status: all regions unreachable".to_string()))
    }
}

/// Build a Jito tip transfer: stub for now. See `system_transfer_ix` in
/// `arb-scout/raw_tx.rs` for the actual builder used by both executors.
pub fn build_tip_transfer_tx_b64(
    _payer:           &solana_program::pubkey::Pubkey,
    _tip_account:     &solana_program::pubkey::Pubkey,
    _lamports:        u64,
    _recent_blockhash: &[u8; 32],
) -> ArbResult<String> {
    Err(ArbError::NotImplemented(
        "build_tip_transfer_tx_b64 — replaced by system_transfer_ix in arb-scout"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_constructs() { let _ = JitoBundleClient::default(); }

    #[test]
    fn multi_region_default_uses_five_endpoints() {
        let c = JitoBundleClient::default();
        assert_eq!(c.block_engine_urls.len(), 5);
        assert!(c.primary_url().contains("amsterdam"));
    }

    #[test]
    fn build_tip_returns_not_implemented_for_now() {
        use solana_program::pubkey::Pubkey;
        let r = build_tip_transfer_tx_b64(
            &Pubkey::default(), &Pubkey::default(), 10_000, &[0u8; 32]
        );
        assert!(matches!(r, Err(ArbError::NotImplemented(_))));
    }
}
