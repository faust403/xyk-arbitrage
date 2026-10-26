//! Helius LaserStream / Yellowstone gRPC subscriber.
//!
//! Replacement for Atlas WSS that runs over gRPC and delivers account
//! updates ~100-200 ms earlier (Helius runs the Yellowstone Geyser plugin
//! adjacent to validators). Emits the same `AccountUpdate` type as WSS so
//! the scout's dispatcher swaps backends with one env knob.
//!
//! ## Pinning
//!
//! Bound to `yellowstone-grpc-client = =1.9.0+solana.1.16.1`. That version
//! exposes `.subscribe()` returning `(Sink, Stream)` — older form than
//! v1.10's `subscribe_with_request()`. We send the filter through the
//! sink immediately after opening the stream, which works on both APIs.
//!
//! ## Activation
//!
//! 1. Build with `--features laserstream` (Dockerfile does so).
//! 2. Set `LASERSTREAM_GRPC_URL` (e.g. `https://laserstream-mainnet-ams.helius-rpc.com`).
//! 3. `HELIUS_API_KEY` reused (Business tier required).

use std::collections::HashMap;

use arb_core::ArbError;
use solana_program::pubkey::Pubkey;
use tokio::sync::mpsc;
use tracing::{debug, info};

use yellowstone_grpc_client::GeyserGrpcClient;
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel,
    SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterAccountsFilter,
    SubscribeRequestFilterAccountsFilterMemcmp,
    subscribe_request_filter_accounts_filter::Filter as AccFilter,
    subscribe_request_filter_accounts_filter_memcmp::Data as MemcmpData,
    SubscribeRequestFilterTransactions,
};

use crate::wss::{AccountUpdate, SessionEnd, TransactionUpdate};

/// Run one LaserStream session: connect, subscribe, forward updates to `tx`.
/// Returns to the scout's reconnect loop on disconnect or error.
pub async fn run_one_session(
    endpoint: &str,
    api_key:  &str,
    accounts: &[Pubkey],
    tx:       &mpsc::Sender<AccountUpdate>,
    tx_stream: Option<&mpsc::Sender<TransactionUpdate>>,
) -> SessionEnd {
    info!(
        endpoint = %scrub(endpoint),
        accounts = accounts.len(),
        "connecting to Helius LaserStream (Yellowstone gRPC)"
    );

    // ── v1.9 constructor: (endpoint, x_token, tls_config) ───────────────
    let mut client = match GeyserGrpcClient::connect::<_, String>(
        endpoint.to_string(),
        Some(api_key.to_string()),
        None,
    ) {
        Ok(c) => c,
        Err(e) => {
            return SessionEnd::Errored(ArbError::WebSocket(
                format!("laserstream connect: {e:?}")
            ));
        }
    };
    info!("LaserStream connected");

    // ── Build the subscribe request ─────────────────────────────────────
    let pubkey_strings: Vec<String> = accounts.iter().map(|p| p.to_string()).collect();
    let mut accounts_filter: HashMap<String, SubscribeRequestFilterAccounts> = HashMap::new();
    accounts_filter.insert(
        "pools".to_string(),
        SubscribeRequestFilterAccounts {
            account: pubkey_strings.clone(),
            owner:   vec![],
            filters: vec![],
        },
    );

    // Optional tx subscription — fires when `tx_stream` was provided AND the
    // operator opts in via env. Subscribes to any tx whose writable accounts
    // include ANY of our pool addresses (`account_required`), filtering out
    // votes and failed txs. These notifications arrive ~50-150 ms before the
    // corresponding account-write, giving the executor a head-start window.
    let mut tx_filter: HashMap<String, SubscribeRequestFilterTransactions> = HashMap::new();
    if tx_stream.is_some() {
        // Yellowstone semantics (corrected from v57): `account_required` is
        // an AND filter — tx must touch ALL listed accounts. That's the
        // wrong semantic for "any pool we care about". Use `account_include`
        // which is the OR — tx must touch AT LEAST ONE of the listed
        // accounts. With 49 pool addresses, this catches every swap on
        // anything in our universe.
        tx_filter.insert("pool_txs".to_string(), SubscribeRequestFilterTransactions {
            vote:             Some(false),
            failed:           Some(false),
            signature:        None,
            account_include:  pubkey_strings,
            account_exclude:  vec![],
            account_required: vec![],
        });
        info!("LaserStream: transaction subscription enabled");
    }

    let request = SubscribeRequest {
        accounts:     accounts_filter,
        transactions: tx_filter,
        // Phase 6h: Processed = leader-included, ~200-400ms faster than
        // Confirmed. Helius LaserStream WSS docs: "updates arrive at the
        // processed commitment level in the order the validator produced
        // them." For Pyth race we need every ms — switch the default.
        commitment:   Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    // ── Open bidi stream — v1.9 returns (Sink, Stream) without taking the
    //    request as an argument. Send the filter through the sink right
    //    after opening so the server starts streaming updates.
    let stream_result = client.subscribe().await;
    let (mut subscribe_tx, mut stream) = match stream_result {
        Ok(s) => s,
        Err(e) => {
            return SessionEnd::Errored(ArbError::WebSocket(
                format!("laserstream subscribe: {e:?}")
            ));
        }
    };

    use futures::SinkExt;
    if let Err(e) = subscribe_tx.send(request).await {
        return SessionEnd::Errored(ArbError::WebSocket(
            format!("laserstream send filter: {e:?}")
        ));
    }

    // ── Forward updates ────────────────────────────────────────────────
    use futures::StreamExt;
    while let Some(msg) = stream.next().await {
        let upd = match msg {
            Ok(u) => u,
            Err(e) => {
                return SessionEnd::Errored(ArbError::WebSocket(
                    format!("laserstream stream: {e:?}")
                ));
            }
        };
        match upd.update_oneof {
            Some(UpdateOneof::Account(acc_upd)) => {
                let account_info = match acc_upd.account {
                    Some(a) => a,
                    None => continue,
                };
                let pubkey_bytes: [u8; 32] = match account_info.pubkey.as_slice().try_into() {
                    Ok(b) => b,
                    Err(_) => { debug!("malformed pubkey on LaserStream account update"); continue; }
                };
                let owner_bytes: [u8; 32] = match account_info.owner.as_slice().try_into() {
                    Ok(b) => b,
                    Err(_) => { debug!("malformed owner on LaserStream account update"); continue; }
                };
                let update = AccountUpdate {
                    account: Pubkey::new_from_array(pubkey_bytes),
                    slot:    acc_upd.slot,
                    data:    account_info.data,
                    owner:   Pubkey::new_from_array(owner_bytes),
                };
                if tx.send(update).await.is_err() {
                    return SessionEnd::Closed; // consumer dropped
                }
            }
            Some(UpdateOneof::Transaction(t)) => {
                let Some(tx_chan) = tx_stream else { continue };
                let Some(info) = t.transaction else { continue };
                if info.is_vote { continue; }
                // Extract signature (64 bytes).
                let sig: [u8; 64] = match info.signature.as_slice().try_into() {
                    Ok(s) => s,
                    Err(_) => { debug!("malformed signature on tx update"); continue; }
                };
                // Pull writable account keys from the message + meta.
                let mut writable: Vec<Pubkey> = Vec::new();
                let mut all_accounts: Vec<Pubkey> = Vec::new();
                let mut num_signers_u8: u8 = 0;
                if let Some(sol_tx) = info.transaction.as_ref() {
                    if let Some(msg) = sol_tx.message.as_ref() {
                        let n_signers = msg.header.as_ref()
                            .map(|h| h.num_required_signatures as usize).unwrap_or(0);
                        num_signers_u8 = n_signers.min(255) as u8;
                        let n_read_signed = msg.header.as_ref()
                            .map(|h| h.num_readonly_signed_accounts as usize).unwrap_or(0);
                        let n_read_unsigned = msg.header.as_ref()
                            .map(|h| h.num_readonly_unsigned_accounts as usize).unwrap_or(0);
                        // Writable = signers minus readonly_signed, plus
                        // unsigned-writable minus readonly_unsigned.
                        let total = msg.account_keys.len();
                        let w_signed_end = n_signers.saturating_sub(n_read_signed);
                        for (i, k) in msg.account_keys.iter().enumerate() {
                            if let Ok(arr) = <[u8; 32]>::try_from(k.as_slice()) {
                                let pk = Pubkey::new_from_array(arr);
                                all_accounts.push(pk);
                                let is_writable = i < w_signed_end
                                    || (i >= n_signers && i < total.saturating_sub(n_read_unsigned));
                                if is_writable {
                                    writable.push(pk);
                                }
                            }
                        }
                    }
                }
                // Pull program logs.
                let logs = info.meta.as_ref()
                    .map(|m| m.log_messages.clone())
                    .unwrap_or_default();
                let update = TransactionUpdate {
                    slot:              t.slot,
                    signature:         sig,
                    writable_accounts: writable,
                    log_messages:      logs,
                    account_keys:      all_accounts,
                    num_signers:       num_signers_u8,
                };
                // Non-blocking send — drop on backpressure so the stream
                // keeps flowing even if the consumer is slow.
                let _ = tx_chan.try_send(update);
            }
            Some(UpdateOneof::Ping(_)) => {
                // Yellowstone keepalive — silent.
            }
            None => continue,
            other => debug!(?other, "LaserStream: non-account update"),
        }
    }
    SessionEnd::Closed
}

/// Run a LaserStream session that subscribes by program OWNER (and optional
/// data-size + memcmp filter) instead of by individual account pubkeys.
///
/// Use when you want every account owned by a program (a DEX / Lend
/// program) without pre-listing addresses. Example: PumpSwap pool
/// creation watcher — subscribe to owner=PUMPSWAP_PROGRAM, dataSize=301,
/// memcmp(offset=0, bytes=pool_discriminator) to catch every new pool
/// account write in real time.
pub async fn run_one_session_by_owner(
    endpoint:      &str,
    api_key:       &str,
    owner_program: &Pubkey,
    data_size:     Option<u64>,
    memcmp:        Option<(u64, Vec<u8>)>,
    tx:            &mpsc::Sender<AccountUpdate>,
) -> SessionEnd {
    info!(
        endpoint = %scrub(endpoint),
        owner = %owner_program,
        data_size = ?data_size,
        memcmp = ?memcmp.as_ref().map(|(o, b)| (o, b.len())),
        "connecting to Helius LaserStream (owner-mode)"
    );

    let mut client = match GeyserGrpcClient::connect::<_, String>(
        endpoint.to_string(),
        Some(api_key.to_string()),
        None,
    ) {
        Ok(c) => c,
        Err(e) => {
            return SessionEnd::Errored(ArbError::WebSocket(
                format!("laserstream connect: {e:?}")
            ));
        }
    };
    info!("LaserStream connected (owner-mode)");

    // Build filters. Yellowstone's account filter accepts data_size and memcmp
    // (one of each per filter object). For our owner-only use, both are
    // optional — when None the filter degrades to "any account owned by
    // owner_program".
    let mut filters: Vec<SubscribeRequestFilterAccountsFilter> = Vec::new();
    if let Some(sz) = data_size {
        filters.push(SubscribeRequestFilterAccountsFilter {
            filter: Some(AccFilter::Datasize(sz)),
        });
    }
    if let Some((offset, bytes)) = memcmp {
        filters.push(SubscribeRequestFilterAccountsFilter {
            filter: Some(AccFilter::Memcmp(SubscribeRequestFilterAccountsFilterMemcmp {
                offset,
                data: Some(MemcmpData::Bytes(bytes)),
            })),
        });
    }

    let mut accounts_filter: HashMap<String, SubscribeRequestFilterAccounts> = HashMap::new();
    accounts_filter.insert(
        "by_owner".to_string(),
        SubscribeRequestFilterAccounts {
            account: vec![],
            owner:   vec![owner_program.to_string()],
            filters,
        },
    );

    let request = SubscribeRequest {
        accounts:     accounts_filter,
        transactions: HashMap::new(),
        commitment:   Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    let (mut subscribe_tx, mut stream) = match client.subscribe().await {
        Ok(s) => s,
        Err(e) => {
            return SessionEnd::Errored(ArbError::WebSocket(
                format!("laserstream subscribe: {e:?}")
            ));
        }
    };

    use futures::SinkExt;
    if let Err(e) = subscribe_tx.send(request).await {
        return SessionEnd::Errored(ArbError::WebSocket(
            format!("laserstream send filter: {e:?}")
        ));
    }

    use futures::StreamExt;
    while let Some(msg) = stream.next().await {
        let upd = match msg {
            Ok(u) => u,
            Err(e) => return SessionEnd::Errored(ArbError::WebSocket(
                format!("laserstream stream: {e:?}")
            )),
        };
        if let Some(UpdateOneof::Account(acc_upd)) = upd.update_oneof {
            let account_info = match acc_upd.account {
                Some(a) => a,
                None => continue,
            };
            let pubkey_bytes: [u8; 32] = match account_info.pubkey.as_slice().try_into() {
                Ok(b) => b,
                Err(_) => { debug!("malformed pubkey on LaserStream account update"); continue; }
            };
            let owner_bytes: [u8; 32] = match account_info.owner.as_slice().try_into() {
                Ok(b) => b,
                Err(_) => { debug!("malformed owner on LaserStream account update"); continue; }
            };
            let update = AccountUpdate {
                account: Pubkey::new_from_array(pubkey_bytes),
                slot:    acc_upd.slot,
                data:    account_info.data,
                owner:   Pubkey::new_from_array(owner_bytes),
            };
            if tx.send(update).await.is_err() {
                return SessionEnd::Closed;
            }
        }
    }
    SessionEnd::Closed
}

fn scrub(url: &str) -> String {
    match url.find("?api-key=") {
        Some(idx) => format!("{}?api-key=***", &url[..idx]),
        None      => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrub_strips_api_key() {
        let s = scrub("https://x.helius-rpc.com/?api-key=secret");
        assert!(!s.contains("secret"));
    }
    #[test]
    fn scrub_passthrough() {
        let s = scrub("https://x.helius-rpc.com");
        assert_eq!(s, "https://x.helius-rpc.com");
    }
}
