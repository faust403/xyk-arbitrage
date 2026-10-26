//! Shred-alert poller: reads `/tmp/shred-*-alert` files written by
//! `unified_shred_tee.py` and fast-paths fresh oracle account fetches.
//!
//! The Python tee substring-scans every shred from Jito's ShredStream Proxy
//! for known program-id bytes (Pyth Push V2, Kamino, MarginFi, PumpSwap),
//! and writes the latest matching slot+timestamp to a single-line alert
//! file. This module spawns one task per alert file:
//!
//! 1. Poll mtime every 50 ms.
//! 2. When mtime advances, read `slot ts` from the file.
//! 3. Fire `getMultipleAccounts(processed)` for the bot's tracked
//!    accounts and emit synthetic `AccountUpdate`s into the same mpsc
//!    the LaserStream subscriber feeds.
//!
//! Net effect: oracle freshness arrives ~50-100 ms earlier than via
//! LaserStream (shreds are pre-confirmation; LaserStream account
//! updates are post-processed-block). The tick_processor downstream
//! doesn't care whether the AccountUpdate came from LaserStream or
//! from this fast-path — it just recomputes LTV and fires.

use base64::Engine;
use reqwest::Client;
use serde_json::json;
use solana_program::pubkey::Pubkey;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use tokio::time::interval;

use arb_stream::wss::AccountUpdate;

/// Spawn the native Rust shred subscriber and route matched events of
/// `filter_label` (e.g. "pyth") through `refetch_accounts`. Gated by
/// the `shredstream` feature so the file-bridge from
/// `unified_shred_tee.py` remains the fallback when this isn't compiled in.
#[cfg(feature = "shredstream")]
pub fn spawn_native_shred_refetch(
    endpoint: String,
    rpc_url: String,
    accounts: Vec<Pubkey>,
    tx: mpsc::Sender<AccountUpdate>,
    filter_label: &'static str,
) {
    let mut shred_rx = arb_stream::shred::spawn_shred_subscriber(
        endpoint,
        arb_stream::shred::default_filters(),
        512,
    );
    tokio::spawn(async move {
        let client = Client::builder()
            .timeout(Duration::from_millis(1500))
            .build()
            .unwrap_or_else(|_| Client::new());
        eprintln!(
            "  📡 native-shred[{filter_label}]: subscribed via Rust client ({} accounts)",
            accounts.len()
        );
        let mut alerts: u64 = 0;
        let mut emitted_total: u64 = 0;
        let start = std::time::Instant::now();
        while let Some(ev) = shred_rx.recv().await {
            if ev.label != filter_label {
                continue;
            }
            alerts += 1;
            let emitted = refetch_accounts(&client, &rpc_url, &accounts, ev.slot, &tx).await;
            emitted_total += emitted as u64;
            if alerts % 20 == 0 {
                let el = start.elapsed().as_secs_f64();
                eprintln!(
                    "  📡 native-shred[{filter_label}] stats: {alerts} alerts ({:.2}/s), {emitted_total} pushed",
                    alerts as f64 / el
                );
            }
        }
    });
}

/// Poll an alert file and fast-fetch accounts whenever the file advances.
///
/// * `alert_path` — e.g. `/tmp/shred-pyth-alert`
/// * `rpc_url`    — Helius RPC endpoint (with API key)
/// * `accounts`   — pubkeys to refetch on each alert (oracles / reserves)
/// * `tx`         — same mpsc the LaserStream subscriber feeds
/// * `label`      — short tag printed in heartbeat lines (e.g. "pyth")
///
/// Never returns. Spawn it as a tokio task. Tolerant of missing files,
/// RPC errors, and partial reads — the LaserStream path remains the
/// authoritative source if this stalls.
pub async fn poll_alert_then_refetch(
    alert_path: String,
    rpc_url: String,
    accounts: Vec<Pubkey>,
    tx: mpsc::Sender<AccountUpdate>,
    label: String,
) {
    let client = Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| Client::new());

    let mut tick = interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let mut last_mtime: SystemTime = SystemTime::UNIX_EPOCH;
    let mut alerts: u64 = 0;
    let mut emitted_total: u64 = 0;
    let start = std::time::Instant::now();
    let mut last_log = std::time::Instant::now();

    eprintln!(
        "  📡 shred-{label}: polling {} ({} accounts to refetch on each alert)",
        alert_path,
        accounts.len()
    );

    loop {
        tick.tick().await;

        let meta = match tokio::fs::metadata(Path::new(&alert_path)).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match meta.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if mtime <= last_mtime {
            continue;
        }
        last_mtime = mtime;

        let content = match tokio::fs::read_to_string(&alert_path).await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut parts = content.split_whitespace();
        let slot: u64 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if slot == 0 {
            // Initial sentinel written at tee startup — ignore.
            continue;
        }

        alerts += 1;
        let emitted = refetch_accounts(&client, &rpc_url, &accounts, slot, &tx).await;
        emitted_total += emitted as u64;

        if last_log.elapsed() > Duration::from_secs(30) {
            let el = start.elapsed().as_secs_f64();
            eprintln!(
                "  📡 shred-{label} stats: {alerts} alerts ({:.2}/s), {emitted_total} account-updates pushed",
                alerts as f64 / el
            );
            last_log = std::time::Instant::now();
        }
    }
}

/// Public: fetch the given accounts via `getMultipleAccounts(processed)`
/// and push them as synthetic `AccountUpdate`s into the supplied mpsc.
///
/// Used by both:
///   1. `poll_alert_then_refetch` — file-bridge from Python `unified_shred_tee.py`
///   2. The native Rust shred subscriber (`arb_stream::shred`) once enabled
///      via the `shredstream` cargo feature.
pub async fn refetch_accounts(
    client: &Client,
    rpc_url: &str,
    accounts: &[Pubkey],
    slot: u64,
    tx: &mpsc::Sender<AccountUpdate>,
) -> usize {
    let mut emitted = 0;
    for chunk in accounts.chunks(100) {
        let keys: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getMultipleAccounts",
            "params": [keys, {"encoding": "base64", "commitment": "processed"}],
        });
        let resp = match client.post(rpc_url).json(&body).send().await {
            Ok(r) => r,
            Err(_) => continue,
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => continue,
        };
        let vals = match json
            .pointer("/result/value")
            .and_then(|v| v.as_array())
        {
            Some(v) => v,
            None => continue,
        };
        for (i, val) in vals.iter().enumerate() {
            if val.is_null() {
                continue;
            }
            let owner_str = val
                .pointer("/owner")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let owner: Pubkey = match owner_str.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let data_b64 = val
                .pointer("/data")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if i >= chunk.len() {
                continue;
            }
            let upd = AccountUpdate {
                account: chunk[i],
                slot,
                data: bytes,
                owner,
            };
            if tx.send(upd).await.is_err() {
                return emitted;
            }
            emitted += 1;
        }
    }
    emitted
}
