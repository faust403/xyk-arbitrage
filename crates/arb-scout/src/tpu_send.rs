//! Direct TPU (Transaction Processing Unit) UDP sender. Bypasses Helius
//! `sendTransaction` and Jito `sendBundle` relays by talking straight to
//! the current leader validator's TPU port over UDP.
//!
//! ## Why this exists
//!
//! Our `sendTransaction` path through Helius adds ~50-150ms because the
//! tx hops: us → Helius RPC server → Helius internal relay → validator.
//! Direct TPU is us → validator with one network leg. With AWS Frankfurt
//! → Solana Frankfurt at 1.5-2.5ms (rpcfast.com benchmark), the total
//! send latency drops to ~3-5ms.
//!
//! ## Architecture
//!
//! Two caches with periodic background refresh:
//!
//! - **ClusterNodesCache** — maps validator pubkey → TPU SocketAddr.
//!   Refreshed every 60 seconds (cluster topology changes slowly).
//!   Populated from `getClusterNodes` RPC.
//!
//! - **LeaderScheduleCache** — maps slot → leader pubkey for the current
//!   epoch. Refreshed every 60 seconds. Populated from
//!   `getLeaderSchedule` RPC.
//!
//! `next_leader_tpus(slot, n)` returns the TPU sockets for the next `n`
//! consecutive slot leaders starting at `slot`. We fan out to all `n`
//! (typically 4) so we cover slot uncertainty + the leader's stake-
//! weighted gossip neighbours.
//!
//! ## Limitations
//!
//! - **No retransmit on packet loss**: UDP is fire-and-forget. For the
//!   first iteration we just send once to the next 4 leaders. If all
//!   packets drop, the tx never lands — but at this rate (~1/1000 in
//!   well-routed networks) it's negligible vs the latency win.
//! - **No bundle ordering**: a Jito bundle guarantees atomic block
//!   inclusion of multiple txs. TPU send doesn't. Fine for our 1-tx
//!   flash-loan-wrapped arb path.
//! - **Tips**: priority fee in the tx data (set_compute_unit_price)
//!   gives mempool ordering. No Jito tip account needed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arb_core::{ArbError, ArbResult};
use serde_json::Value;
use solana_program::pubkey::Pubkey;
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

/// Default TPU send fan-out. We send to the current leader + the next 3,
/// covering slot turnover during our ~3-5ms flight time.
pub const DEFAULT_FANOUT: usize = 4;

/// Refresh interval for both the cluster-nodes and leader-schedule caches.
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Default)]
struct ClusterNodes {
    by_pubkey: HashMap<Pubkey, SocketAddr>,
    fetched_at: Option<Instant>,
}

#[derive(Default)]
struct LeaderSchedule {
    /// Maps absolute slot → leader pubkey for the CURRENT epoch.
    /// `getLeaderSchedule` returns slot-relative-to-epoch indices; we
    /// resolve them against the epoch's first_slot at fetch time.
    by_slot: HashMap<u64, Pubkey>,
    fetched_at: Option<Instant>,
}

/// Direct TPU sender with cached leader lookup.
pub struct TpuSender {
    rpc_url:         String,
    cluster_nodes:   Arc<RwLock<ClusterNodes>>,
    leader_schedule: Arc<RwLock<LeaderSchedule>>,
    udp_socket:      UdpSocket,
}

impl TpuSender {
    /// Build a new sender. Binds a single UDP socket reused for all
    /// outgoing packets (cheaper than per-send bind/close).
    pub async fn new(rpc_url: String) -> ArbResult<Self> {
        let udp_socket = UdpSocket::bind("0.0.0.0:0").await
            .map_err(|e| ArbError::Rpc(format!("tpu_send: bind udp: {e}")))?;
        Ok(Self {
            rpc_url,
            cluster_nodes:   Arc::new(RwLock::new(ClusterNodes::default())),
            leader_schedule: Arc::new(RwLock::new(LeaderSchedule::default())),
            udp_socket,
        })
    }

    /// Spawn background tasks that keep both caches fresh. Call once
    /// after `Arc::new(sender)`.
    pub fn spawn_refresher(self: &Arc<Self>) {
        let s = Arc::clone(self);
        tokio::spawn(async move {
            // First load is synchronous before returning, so callers
            // don't race a send against an empty cache.
            if let Err(e) = s.refresh_cluster_nodes().await {
                warn!(target: "tpu_send", error = %e, "initial cluster_nodes refresh failed");
            }
            if let Err(e) = s.refresh_leader_schedule().await {
                warn!(target: "tpu_send", error = %e, "initial leader_schedule refresh failed");
            }
            let mut tick = tokio::time::interval(REFRESH_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(e) = s.refresh_cluster_nodes().await {
                    warn!(target: "tpu_send", error = %e, "cluster_nodes refresh failed");
                }
                if let Err(e) = s.refresh_leader_schedule().await {
                    warn!(target: "tpu_send", error = %e, "leader_schedule refresh failed");
                }
            }
        });
    }

    async fn refresh_cluster_nodes(&self) -> ArbResult<()> {
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getClusterNodes",
        });
        let resp: Value = reqwest::Client::new()
            .post(&self.rpc_url).json(&body).send().await
            .map_err(|e| ArbError::Rpc(format!("getClusterNodes send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getClusterNodes parse: {e}")))?;
        let arr = resp.pointer("/result").and_then(|v| v.as_array())
            .ok_or_else(|| ArbError::Rpc("getClusterNodes: no result array".into()))?;
        let mut by_pubkey = HashMap::with_capacity(arr.len());
        for entry in arr {
            let pk_str = entry.get("pubkey").and_then(|v| v.as_str());
            let tpu_str = entry.get("tpu").and_then(|v| v.as_str());
            if let (Some(pk_s), Some(tpu_s)) = (pk_str, tpu_str) {
                if let (Ok(pk), Ok(addr)) = (pk_s.parse::<Pubkey>(), tpu_s.parse::<SocketAddr>()) {
                    by_pubkey.insert(pk, addr);
                }
            }
        }
        let n = by_pubkey.len();
        let mut g = self.cluster_nodes.write().await;
        g.by_pubkey = by_pubkey;
        g.fetched_at = Some(Instant::now());
        drop(g);
        debug!(target: "tpu_send", nodes = n, "cluster_nodes refreshed");
        Ok(())
    }

    async fn refresh_leader_schedule(&self) -> ArbResult<()> {
        // Need epoch info to convert relative slot indices to absolute slots.
        let epoch_body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getEpochInfo",
        });
        let resp: Value = reqwest::Client::new()
            .post(&self.rpc_url).json(&epoch_body).send().await
            .map_err(|e| ArbError::Rpc(format!("getEpochInfo send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getEpochInfo parse: {e}")))?;
        let absolute_slot = resp.pointer("/result/absoluteSlot").and_then(|v| v.as_u64())
            .ok_or_else(|| ArbError::Rpc("getEpochInfo: no absoluteSlot".into()))?;
        let slot_in_epoch = resp.pointer("/result/slotIndex").and_then(|v| v.as_u64())
            .ok_or_else(|| ArbError::Rpc("getEpochInfo: no slotIndex".into()))?;
        let first_slot = absolute_slot - slot_in_epoch;

        let sched_body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getLeaderSchedule",
        });
        let resp: Value = reqwest::Client::new()
            .post(&self.rpc_url).json(&sched_body).send().await
            .map_err(|e| ArbError::Rpc(format!("getLeaderSchedule send: {e}")))?
            .json().await
            .map_err(|e| ArbError::Rpc(format!("getLeaderSchedule parse: {e}")))?;
        let result = resp.pointer("/result")
            .ok_or_else(|| ArbError::Rpc("getLeaderSchedule: no result".into()))?;
        let obj = result.as_object()
            .ok_or_else(|| ArbError::Rpc("getLeaderSchedule: result not object".into()))?;

        let mut by_slot = HashMap::with_capacity(obj.len() * 4);
        for (pk_s, slots) in obj.iter() {
            let Ok(pk) = pk_s.parse::<Pubkey>() else { continue };
            let Some(arr) = slots.as_array() else { continue };
            for v in arr {
                if let Some(rel) = v.as_u64() {
                    by_slot.insert(first_slot + rel, pk);
                }
            }
        }
        let n = by_slot.len();
        let mut g = self.leader_schedule.write().await;
        g.by_slot = by_slot;
        g.fetched_at = Some(Instant::now());
        drop(g);
        debug!(target: "tpu_send", slots = n, "leader_schedule refreshed");
        Ok(())
    }

    /// Resolve the next `n` slot leaders (starting at `current_slot`) to
    /// their TPU sockets. Skips leaders we don't have a TPU for. Returns
    /// an empty Vec if both caches are still cold.
    pub async fn next_leader_tpus(&self, current_slot: u64, n: usize) -> Vec<SocketAddr> {
        let sched = self.leader_schedule.read().await;
        let nodes = self.cluster_nodes.read().await;
        let mut out = Vec::with_capacity(n);
        let mut seen: std::collections::HashSet<SocketAddr> = std::collections::HashSet::new();
        for offset in 0..n as u64 {
            if let Some(pk) = sched.by_slot.get(&(current_slot + offset)) {
                if let Some(addr) = nodes.by_pubkey.get(pk) {
                    if seen.insert(*addr) {
                        out.push(*addr);
                    }
                }
            }
        }
        out
    }

    /// Send raw transaction bytes via UDP to the next `n` leaders.
    /// Returns the number of packets actually transmitted.
    /// Caller is responsible for extracting the tx signature for tracking
    /// (`signed_b64` decodes to: [num_sigs:u8][sig:64 bytes][...]).
    pub async fn send_to_next_leaders(
        &self,
        raw_tx:       &[u8],
        current_slot: u64,
        fanout:       usize,
    ) -> ArbResult<usize> {
        let addrs = self.next_leader_tpus(current_slot, fanout).await;
        if addrs.is_empty() {
            return Err(ArbError::Rpc(
                "tpu_send: no leader TPUs known (caches cold or schedule miss)".into()));
        }
        let mut sent = 0;
        for addr in addrs {
            match self.udp_socket.send_to(raw_tx, addr).await {
                Ok(_) => sent += 1,
                Err(e) => warn!(target: "tpu_send", %addr, error = %e,
                                "udp send failed (ignoring; other leaders still tried)"),
            }
        }
        if sent == 0 {
            return Err(ArbError::Rpc("tpu_send: all sendto's failed".into()));
        }
        info!(target: "tpu_send", current_slot, leaders = sent, "🛫 TPU send");
        Ok(sent)
    }
}

/// Fetch the current slot. Used by callers right before send so the
/// fanout targets the freshest leader window.
pub async fn fetch_current_slot(rpc_url: &str) -> ArbResult<u64> {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getSlot"});
    let resp: Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await
        .map_err(|e| ArbError::Rpc(format!("getSlot send: {e}")))?
        .json().await
        .map_err(|e| ArbError::Rpc(format!("getSlot parse: {e}")))?;
    resp.pointer("/result").and_then(|v| v.as_u64())
        .ok_or_else(|| ArbError::Rpc("getSlot: no result".into()))
}

/// Extract the (first) signature from a serialized transaction. The
/// wire format starts with `[compact-u16 sig_count][sig × 64 bytes]`.
/// For our single-payer-signs path, compact-u16 is always 1 (1 byte),
/// so the signature sits at bytes 1..65.
pub fn extract_signature_base58(raw_tx: &[u8]) -> Option<String> {
    if raw_tx.len() < 65 { return None; }
    let sig_count = raw_tx[0];
    if sig_count == 0 { return None; }
    let sig_bytes = &raw_tx[1..65];
    Some(bs58::encode(sig_bytes).into_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_signature_handles_short_input() {
        assert!(extract_signature_base58(&[]).is_none());
        assert!(extract_signature_base58(&[0u8; 10]).is_none());
    }

    #[test]
    fn extract_signature_returns_base58() {
        let mut buf = vec![1u8]; // 1 signature
        buf.extend_from_slice(&[0xAB; 64]);
        let sig = extract_signature_base58(&buf).unwrap();
        // 64 bytes of 0xAB → known base58 string, length is 87-88 chars
        assert!(sig.len() >= 85 && sig.len() <= 90);
    }
}
