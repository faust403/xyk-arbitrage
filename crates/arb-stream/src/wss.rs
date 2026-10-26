//! Helius Enhanced WebSocket subscriber.
//!
//! Uses standard Solana JSON-RPC `accountSubscribe` over WSS, which Helius
//! runs on its Atlas backend (bundled with the Developer plan, $49/mo). Each
//! tracked account — pool OR vault — gets its own subscription. The
//! consumer downstream distinguishes pools vs vaults by inspecting `owner`
//! on the resulting `AccountUpdate`.
//!
//! Per `INSTRUCTIONS.md` Phase 6:
//! * Exponential backoff on reconnect (200 ms → 30 s).
//! * After every reconnect, re-fetch full account state for ALL monitored
//!   addresses. That re-fetch happens in the scout layer (see
//!   `Scout::run_enhanced_websocket`), not here — this module only owns the
//!   single-connection lifecycle.

use std::collections::HashMap;

use arb_core::ArbError;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use solana_program::pubkey::Pubkey;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct AccountUpdate {
    pub account: Pubkey,
    pub slot:    u64,
    pub data:    Vec<u8>,
    pub owner:   Pubkey,
}

/// Yellowstone transaction notification. Emitted ~50-150 ms before the
/// corresponding account-write update lands, so the scout can react to a
/// swap before its effect is committed to the pool's state.
///
/// Used only via the LaserStream backend; the WSS backend doesn't surface
/// transactions in any standard way.
#[derive(Debug, Clone)]
pub struct TransactionUpdate {
    pub slot:             u64,
    pub signature:        [u8; 64],
    /// Writable accounts the tx touched. Filtering by overlap with the
    /// pool universe lets the scout know WHICH pools just had a swap.
    pub writable_accounts: Vec<Pubkey>,
    /// Program logs emitted by the tx. Whirlpool / Raydium CLMM swap-ix
    /// log the new sqrt_price / liquidity here, letting a future phase
    /// derive predictive pool state without waiting for account-write.
    pub log_messages:     Vec<String>,
    /// Full ordered list of account_keys from the tx's message. Index 0
    /// is the fee payer; signers come first (count = `num_required_signatures`),
    /// then non-signers. Used by the pump.fun create-detector to extract the
    /// new mint at `account_keys[1]` (= second signer in pump.fun Create).
    pub account_keys:     Vec<Pubkey>,
    /// Count of required signatures — i.e., `account_keys[..num_signers]` are signers.
    pub num_signers:      u8,
}

/// Reasons a connection cycle ended. The scout uses this to decide whether
/// to back off (transient error) or treat as clean shutdown.
#[derive(Debug)]
pub enum SessionEnd {
    /// Connection closed cleanly by either side. Safe to immediately retry.
    Closed,
    /// Connection failed mid-stream — apply backoff before retry.
    Errored(ArbError),
}

/// Run ONE connection cycle: connect, subscribe to all `accounts`, forward
/// updates onto `tx` until the socket closes or errors. Returns to the caller
/// (typically the scout's reconnect loop) — does NOT loop internally.
pub async fn run_one_session(
    wss_url: &str,
    accounts: &[Pubkey],
    tx:       &mpsc::Sender<AccountUpdate>,
) -> SessionEnd {
    info!(url = %scrub_key(wss_url), accounts = accounts.len(), "connecting to Helius WSS");
    let ws = match tokio_tungstenite::connect_async(wss_url).await {
        Ok((s, _)) => s,
        Err(e) => return SessionEnd::Errored(ArbError::WebSocket(format!("connect: {e}"))),
    };
    info!("WSS connected; subscribing to {} accounts", accounts.len());

    let (mut write, mut read) = ws.split();
    let mut sub_id_to_account: HashMap<u64, Pubkey> = HashMap::new();
    let mut pending_subs:      HashMap<u64, Pubkey> = HashMap::new();

    for (i, addr) in accounts.iter().enumerate() {
        let request_id = (i as u64) + 1;
        let req = json!({
            "jsonrpc": "2.0",
            "id":      request_id,
            "method":  "accountSubscribe",
            "params":  [
                addr.to_string(),
                { "encoding": "base64", "commitment": "confirmed" }
            ]
        });
        if let Err(e) = write.send(Message::Text(req.to_string())).await {
            return SessionEnd::Errored(ArbError::WebSocket(format!("send subscribe: {e}")));
        }
        pending_subs.insert(request_id, *addr);
    }

    while let Some(msg) = read.next().await {
        let msg = match msg {
            Ok(m)  => m,
            Err(e) => return SessionEnd::Errored(ArbError::WebSocket(format!("read: {e}"))),
        };
        let text = match msg {
            Message::Text(t)  => t,
            Message::Close(_) => return SessionEnd::Closed,
            _ => continue,
        };

        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { warn!(error = %e, "WSS message not JSON; skipping"); continue; }
        };

        // Subscribe response: `{"jsonrpc":"2.0","id":<id>,"result":<sub_id>}`
        if let (Some(req_id), Some(sub_id)) = (
            v.get("id").and_then(|x| x.as_u64()),
            v.get("result").and_then(|x| x.as_u64()),
        ) {
            if let Some(addr) = pending_subs.remove(&req_id) {
                sub_id_to_account.insert(sub_id, addr);
                debug!(account = %addr, sub = sub_id, "subscription confirmed");
            }
            continue;
        }

        // Notification: `{"method":"accountNotification", ...}`
        if v.get("method").and_then(|x| x.as_str()) == Some("accountNotification") {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let sub = params.get("subscription").and_then(|x| x.as_u64());
            let result = params.get("result").cloned().unwrap_or(Value::Null);
            let slot = result.get("context").and_then(|c| c.get("slot")).and_then(|s| s.as_u64()).unwrap_or(0);
            let val = result.get("value").cloned().unwrap_or(Value::Null);

            let owner_str = val.get("owner").and_then(|s| s.as_str()).unwrap_or("");
            let owner = owner_str.parse::<Pubkey>().unwrap_or_default();

            let data_arr = val.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default();
            if data_arr.len() < 2 { continue; }
            let b64 = data_arr[0].as_str().unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(b64) {
                Ok(b) => b,
                Err(e) => { warn!(error = %e, "base64 decode"); continue; }
            };

            let account = sub.and_then(|s| sub_id_to_account.get(&s)).copied();
            if let Some(account) = account {
                let upd = AccountUpdate { account, slot, data: bytes, owner };
                if tx.send(upd).await.is_err() {
                    return SessionEnd::Closed; // consumer dropped
                }
            }
        }
    }
    SessionEnd::Closed
}

/// Strip `?api-key=...` from a Helius URL before logging.
pub fn scrub_key(url: &str) -> String {
    if let Some(idx) = url.find("?api-key=") {
        let mut s = url[..idx + "?api-key=".len()].to_string();
        s.push_str("***");
        s
    } else {
        url.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_key_removes_api_key() {
        let s = scrub_key("wss://atlas-mainnet.helius-rpc.com/?api-key=SECRET-VALUE-HERE");
        assert!(!s.contains("SECRET"));
        assert!(s.contains("api-key=***"));
    }

    #[test]
    fn scrub_key_passes_through_when_no_key() {
        let s = scrub_key("wss://example.com/");
        assert_eq!(s, "wss://example.com/");
    }
}
