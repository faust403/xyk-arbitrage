//! Native Rust subscriber to Jito's ShredStream Proxy on `localhost:9000`.
//!
//! Replaces the Python `unified_shred_tee.py` file-bridge with a direct
//! gRPC stream into the same process as the consumer. Substring-scans the
//! serialised `Entry.entries` bytes for known program-id byte sequences
//! (same heuristic as the Python tee — full `solana-entry` deserialisation
//! is a future phase) and pushes `ShredEvent`s into an mpsc.
//!
//! Gated by the `shredstream` cargo feature so the Windows dev build,
//! which has no `protoc`, still links.

use std::time::Instant;

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub mod proto {
    #![allow(clippy::all)]
    #![allow(unknown_lints)]
    tonic::include_proto!("shredstream");
}

use proto::shredstream_proxy_client::ShredstreamProxyClient;
use proto::SubscribeEntriesRequest;

/// Pre-computed byte sequences we substring-scan for. Adding a new
/// program is one line. Order matters only insofar as you list the
/// 32-byte pubkey for substring detection.
#[derive(Clone, Debug)]
pub struct ShredFilter {
    pub label: &'static str,
    pub bytes: Vec<u8>,
    /// Optional 8-byte discriminator (Anchor account / ix tag). When set,
    /// emission requires BOTH `bytes` AND `disc` to appear in the entry.
    pub disc:  Option<Vec<u8>>,
}

/// Event pushed downstream when a filter matches.
#[derive(Clone, Debug)]
pub struct ShredEvent {
    pub label: &'static str,
    pub slot:  u64,
    /// Unix epoch milliseconds when this consumer first saw the matching shred.
    pub ts_ms: u64,
}

/// Build the canonical filter set for the bots that ship in this repo.
/// Returns: pyth, pumpswap, pumpswap-create, kamino, marginfi.
pub fn default_filters() -> Vec<ShredFilter> {
    // Helper: base58 decode (avoids pulling solana-sdk just for Pubkey parse here)
    fn b58(s: &str) -> Vec<u8> {
        bs58::decode(s).into_vec().unwrap_or_default()
    }
    vec![
        ShredFilter {
            label: "pyth",
            bytes: b58("pythWSnswVUd12oZpeFP8e9CVaEqJg25g1Vtc2biRsT"),
            disc:  None,
        },
        ShredFilter {
            label: "pumpswap",
            bytes: b58("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
            disc:  None,
        },
        ShredFilter {
            label: "pumpswap-create",
            bytes: b58("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA"),
            disc:  Some(vec![0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc]),
        },
        ShredFilter {
            label: "kamino",
            bytes: b58("KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"),
            disc:  None,
        },
        ShredFilter {
            label: "marginfi",
            bytes: b58("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA"),
            disc:  None,
        },
    ]
}

/// Subscribe to the ShredStream Proxy. Returns the mpsc receiver of
/// matched events. Reconnects with exponential backoff on transient
/// errors. The task it spawns owns the gRPC stream.
pub fn spawn_shred_subscriber(
    endpoint: String,
    filters: Vec<ShredFilter>,
    buffer: usize,
) -> mpsc::Receiver<ShredEvent> {
    let (tx, rx) = mpsc::channel(buffer);
    tokio::spawn(async move {
        let mut backoff_ms: u64 = 200;
        loop {
            match run_one_session(&endpoint, &filters, &tx).await {
                Ok(()) => {
                    warn!("shred stream ended cleanly — reconnecting in {backoff_ms}ms");
                }
                Err(e) => {
                    error!("shred stream error: {e:?} — reconnecting in {backoff_ms}ms");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            backoff_ms = (backoff_ms * 2).min(10_000);
        }
    });
    rx
}

async fn run_one_session(
    endpoint: &str,
    filters: &[ShredFilter],
    tx: &mpsc::Sender<ShredEvent>,
) -> Result<()> {
    info!("shred: connecting to {endpoint}");
    let mut client = ShredstreamProxyClient::connect(endpoint.to_string())
        .await
        .context("connect to shredstream-proxy")?;
    let mut stream = client
        .subscribe_entries(SubscribeEntriesRequest {})
        .await
        .context("subscribe_entries")?
        .into_inner();

    info!("shred: subscribed ({} filters active)", filters.len());

    let started = Instant::now();
    let mut entries: u64 = 0;
    let mut emitted: u64 = 0;
    let mut last_log = Instant::now();
    // Debounce per (filter, slot) — avoid spamming downstream when the same
    // program appears in many entries within a single slot.
    let mut last_slot: std::collections::HashMap<&'static str, u64> =
        std::collections::HashMap::with_capacity(filters.len());

    while let Some(entry) = stream.message().await.context("stream.message")? {
        entries += 1;
        let data = &entry.entries;
        let slot = entry.slot;

        for f in filters {
            // Cheap reject: program bytes must appear at all.
            if !contains_seq(data, &f.bytes) {
                continue;
            }
            if let Some(d) = &f.disc {
                if !contains_seq(data, d) {
                    continue;
                }
            }
            // Debounce same-slot duplicates.
            if last_slot.get(f.label) == Some(&slot) {
                continue;
            }
            last_slot.insert(f.label, slot);

            let ts_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            let ev = ShredEvent { label: f.label, slot, ts_ms };
            // try_send so we never block the gRPC consumer: if the
            // downstream is overwhelmed, drop rather than stall the stream.
            let _ = tx.try_send(ev);
            emitted += 1;
        }

        if last_log.elapsed() > std::time::Duration::from_secs(30) {
            let el = started.elapsed().as_secs_f64();
            info!(
                "shred: {entries} entries ({:.0}/s), {emitted} matches emitted",
                entries as f64 / el
            );
            last_log = Instant::now();
        }
    }
    Ok(())
}

/// Linear substring search. Equivalent to Python `needle in haystack`.
/// For 32-byte program-id needles this is ~O(n); for our entry sizes
/// (few hundred bytes typically) it's far cheaper than any setup cost
/// from Boyer–Moore. memmem::Finder would be faster but adds a dep.
#[inline]
fn contains_seq(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > hay.len() {
        return needle.is_empty();
    }
    hay.windows(needle.len()).any(|w| w == needle)
}
