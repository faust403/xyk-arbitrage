//! Jito explorer correlator: for every opportunity the scout logs, fetches
//! the on-chain block at that slot ~30 s later and checks whether any Jito
//! bundle landed there that touched the same pools. If yes, we'd have lost
//! the race; if no, we'd have been the only searcher and could have captured.
//!
//! Result is the **capture rate ceiling** — the headline number INSTRUCTIONS.md
//! Phase −1 Section 6 of the scout report needs.
//!
//! ## How a Jito bundle is identified in a block
//!
//! Every Jito bundle MUST include a SystemProgram::Transfer to one of 8
//! canonical Jito tip accounts. So for each transaction in the block we:
//! 1. Walk its top-level instructions.
//! 2. For each, check `program_id == SystemProgram` and instruction data
//!    decodes as `Transfer { lamports }` (discriminator u32 LE = 2 + lamports
//!    u64 LE).
//! 3. Check the transfer destination is in [`crate::known::JITO_TIP_ACCOUNTS`].
//!
//! If the same transaction also touches one of OUR pools (account_keys
//! intersection), it counts as a competing bundle and we record its tip.
//!
//! ## Output
//!
//! Each correlation result is appended to `logs/correlation-YYYY-MM-DD.jsonl`,
//! keyed by `opportunity_slot`. The `scout-report` binary joins this with
//! `scout-*.jsonl` to produce Section 6.

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use arb_core::{ArbError, ArbResult};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::known::{is_jito_tip_account, JUPITER_V6_PROGRAM, SYSTEM_PROGRAM};
use crate::rpc::{BlockTransaction, HeliusRpc};
use crate::scout::ScoutMetrics;

/// One logged opportunity awaiting block-level correlation.
#[derive(Debug, Clone)]
pub struct PendingCorrelation {
    pub slot:                  u64,
    pub pools:                 HashSet<Pubkey>,
    pub net_profit_usdc:       i64,
    pub opportunity_timestamp: DateTime<Utc>,
    pub queued_at:             Instant,
}

/// One correlation outcome, persisted as a JSONL line.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorrelationRecord {
    pub correlated_at:           DateTime<Utc>,
    pub opportunity_slot:        u64,
    pub opportunity_timestamp:   DateTime<Utc>,
    pub net_profit_usdc:         i64,
    /// `"lost"` (a competing Jito bundle landed in the same slot touching one
    /// of our pools) | `"uncontested"` (no competing bundle) | `"skipped"`
    /// (block fetch failed or slot was skipped — inconclusive).
    pub outcome:                 String,
    pub competing_bundles:       u32,
    pub competitor_tip_lamports: u64,
    /// Of the competing bundles, how many routed through Jupiter v6.
    /// Hint: high ratio ⇒ integrating Jupiter as our router would close
    /// most of the latency / pathfinding gap with successful searchers.
    #[serde(default)]
    pub competing_via_jupiter:   u32,
}

#[derive(Clone)]
pub struct JitoCorrelator {
    inner: Arc<JitoCorrelatorInner>,
}

struct JitoCorrelatorInner {
    rpc:           HeliusRpc,
    metrics:       Arc<ScoutMetrics>,
    pending:       Mutex<VecDeque<PendingCorrelation>>,
    log_dir:       PathBuf,
    delay:         Duration,
    poll_interval: Duration,
}

impl JitoCorrelator {
    pub fn new(
        rpc_url: String,
        metrics: Arc<ScoutMetrics>,
        log_dir: PathBuf,
    ) -> Self {
        Self {
            inner: Arc::new(JitoCorrelatorInner {
                rpc:           HeliusRpc::new(rpc_url),
                metrics,
                pending:       Mutex::new(VecDeque::new()),
                log_dir,
                delay:         Duration::from_secs(30),
                poll_interval: Duration::from_secs(5),
            }),
        }
    }

    /// Enqueue an opportunity for later block correlation. Cheap (lock + push).
    pub fn submit(
        &self,
        slot: u64,
        pools: HashSet<Pubkey>,
        net_profit_usdc: i64,
        opportunity_timestamp: DateTime<Utc>,
    ) {
        let p = PendingCorrelation {
            slot, pools, net_profit_usdc, opportunity_timestamp,
            queued_at: Instant::now(),
        };
        self.inner.pending.lock().unwrap().push_back(p);
    }

    /// Long-running task: every `poll_interval`, pops items aged past
    /// `delay`, fetches their slot's block, and records the correlation.
    pub async fn run(self) -> ArbResult<()> {
        info!(
            delay_secs = self.inner.delay.as_secs(),
            poll_secs  = self.inner.poll_interval.as_secs(),
            "Jito correlator started"
        );
        loop {
            tokio::time::sleep(self.inner.poll_interval).await;

            let ready: Vec<PendingCorrelation> = {
                let mut q = self.inner.pending.lock().unwrap();
                let mut out = Vec::new();
                while let Some(front) = q.front() {
                    if front.queued_at.elapsed() < self.inner.delay { break; }
                    out.push(q.pop_front().expect("front exists"));
                }
                out
            };

            for item in ready {
                if let Err(e) = self.correlate(item).await {
                    warn!(error = %e, "correlation tick failed");
                }
            }
        }
    }

    async fn correlate(&self, item: PendingCorrelation) -> ArbResult<()> {
        let block = match self.inner.rpc.get_block(item.slot).await {
            Ok(Some(b)) => b,
            Ok(None) => {
                // Slot skipped or block not yet retained by RPC — inconclusive.
                self.write_record(&item, "skipped", 0, 0, 0).await?;
                return Ok(());
            }
            Err(e) => {
                self.write_record(&item, "skipped", 0, 0, 0).await?;
                warn!(slot = item.slot, error = %e, "get_block failed; recording as skipped");
                return Ok(());
            }
        };

        let scan = scan_block_for_competitors(&block.transactions, &item.pools);

        let outcome = if scan.max_tip > 0 { "lost" } else { "uncontested" };
        self.write_record(&item, outcome, scan.bundle_count, scan.max_tip, scan.via_jupiter).await?;

        // Update aggregate metrics.
        self.inner.metrics.opps_correlated_total.fetch_add(1, Ordering::Relaxed);
        if scan.max_tip > 0 {
            self.inner.metrics.opps_lost_to_competitor.fetch_add(1, Ordering::Relaxed);
            self.inner.metrics.competitor_tips_total_lamports.fetch_add(scan.max_tip, Ordering::Relaxed);
        } else {
            self.inner.metrics.opps_uncontested.fetch_add(1, Ordering::Relaxed);
        }
        if scan.via_jupiter > 0 {
            self.inner.metrics.competing_via_jupiter_total
                .fetch_add(scan.via_jupiter as u64, Ordering::Relaxed);
        }

        debug!(
            slot = item.slot,
            outcome,
            competing_bundles = scan.bundle_count,
            competitor_tip_lamports = scan.max_tip,
            via_jupiter = scan.via_jupiter,
            "correlation complete"
        );
        Ok(())
    }

    async fn write_record(
        &self,
        item: &PendingCorrelation,
        outcome: &str,
        competing_bundles: u32,
        competitor_tip_lamports: u64,
        competing_via_jupiter: u32,
    ) -> ArbResult<()> {
        let now = Utc::now();
        let date = now.date_naive();
        let path = self.inner.log_dir.join(format!("correlation-{}.jsonl", date.format("%Y-%m-%d")));

        fs::create_dir_all(&self.inner.log_dir).await?;

        let record = CorrelationRecord {
            correlated_at:           now,
            opportunity_slot:        item.slot,
            opportunity_timestamp:   item.opportunity_timestamp,
            net_profit_usdc:         item.net_profit_usdc,
            outcome:                 outcome.to_string(),
            competing_bundles,
            competitor_tip_lamports,
            competing_via_jupiter,
        };
        let mut line = serde_json::to_string(&record)
            .map_err(|e| ArbError::Config(format!("serialize correlation: {e}")))?;
        line.push('\n');

        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        f.write_all(line.as_bytes()).await?;
        f.flush().await?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct BlockScanResult {
    /// Highest Jito-tip lamports across competing bundles.
    pub max_tip:      u64,
    /// Number of competing Jito bundles touching `our_pools`.
    pub bundle_count: u32,
    /// Of those, how many also invoked the Jupiter v6 program.
    pub via_jupiter:  u32,
}

/// Walks every transaction in `txs` and identifies competing Jito bundles —
/// transactions that (a) touch one of `our_pools` AND (b) include a
/// SystemProgram::Transfer to a Jito tip account. Also tags each as
/// "Jupiter-routed" if the program list includes Jupiter v6.
pub fn scan_block_for_competitors(
    txs:        &[BlockTransaction],
    our_pools:  &HashSet<Pubkey>,
) -> BlockScanResult {
    let mut out = BlockScanResult::default();

    for tx in txs {
        if tx.failed { continue; }
        let touches_pool = tx.account_keys.iter().any(|k| our_pools.contains(k));
        if !touches_pool { continue; }

        let tip = find_max_jito_tip_in_tx(tx);
        if tip > 0 {
            out.bundle_count += 1;
            if tip > out.max_tip { out.max_tip = tip; }
            if tx_uses_jupiter(tx) {
                out.via_jupiter += 1;
            }
        }
    }
    out
}

fn tx_uses_jupiter(tx: &BlockTransaction) -> bool {
    tx.account_keys.iter().any(|k| k == &JUPITER_V6_PROGRAM)
}

fn find_max_jito_tip_in_tx(tx: &BlockTransaction) -> u64 {
    let mut max_tip = 0u64;
    for ix in &tx.instructions {
        let pid = match tx.account_keys.get(ix.program_id_index as usize) {
            Some(p) => p,
            None    => continue,
        };
        if pid != &SYSTEM_PROGRAM { continue; }
        // SystemInstruction::Transfer = discriminator(u32 LE = 2) + lamports(u64 LE)
        if ix.data.len() < 12 { continue; }
        let disc = u32::from_le_bytes([ix.data[0], ix.data[1], ix.data[2], ix.data[3]]);
        if disc != 2 { continue; }
        let lamports = u64::from_le_bytes([
            ix.data[4], ix.data[5], ix.data[6], ix.data[7],
            ix.data[8], ix.data[9], ix.data[10], ix.data[11],
        ]);
        // Account indices for Transfer: [source, destination]
        if ix.account_indices.len() < 2 { continue; }
        let dest_idx = ix.account_indices[1] as usize;
        let dest = match tx.account_keys.get(dest_idx) {
            Some(d) => d,
            None    => continue,
        };
        if is_jito_tip_account(dest) && lamports > max_tip {
            max_tip = lamports;
        }
    }
    max_tip
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{BlockInstruction, BlockTransaction};
    use crate::known::JITO_TIP_ACCOUNTS;

    fn transfer_data(lamports: u64) -> Vec<u8> {
        let mut d = Vec::with_capacity(12);
        d.extend_from_slice(&2u32.to_le_bytes());     // discriminator: Transfer
        d.extend_from_slice(&lamports.to_le_bytes());
        d
    }

    fn make_tx_with_tip(pool: Pubkey, tip_account: Pubkey, tip_lamports: u64) -> BlockTransaction {
        // accounts: [0]=fee_payer, [1]=pool, [2]=tip_account, [3]=SystemProgram
        let account_keys = vec![
            Pubkey::new_from_array([99u8; 32]),
            pool,
            tip_account,
            SYSTEM_PROGRAM,
        ];
        let ixs = vec![BlockInstruction {
            program_id_index: 3,                    // SystemProgram
            account_indices:  vec![0, 2],           // fee_payer -> tip_account
            data:             transfer_data(tip_lamports),
        }];
        BlockTransaction { account_keys, instructions: ixs, failed: false, fee_lamports: 5000 }
    }

    #[test]
    fn detects_jito_bundle_touching_our_pool() {
        let pool = Pubkey::new_from_array([7u8; 32]);
        let our_pools: HashSet<Pubkey> = std::iter::once(pool).collect();
        let txs = vec![make_tx_with_tip(pool, JITO_TIP_ACCOUNTS[0], 50_000)];
        let r = scan_block_for_competitors(&txs, &our_pools);
        assert_eq!(r.max_tip, 50_000);
        assert_eq!(r.bundle_count, 1);
        assert_eq!(r.via_jupiter, 0);
    }

    #[test]
    fn ignores_jito_bundle_touching_unrelated_pool() {
        let our_pool      = Pubkey::new_from_array([7u8; 32]);
        let other_pool    = Pubkey::new_from_array([8u8; 32]);
        let our_pools: HashSet<Pubkey> = std::iter::once(our_pool).collect();
        let txs = vec![make_tx_with_tip(other_pool, JITO_TIP_ACCOUNTS[0], 50_000)];
        let r = scan_block_for_competitors(&txs, &our_pools);
        assert_eq!(r.max_tip, 0);
        assert_eq!(r.bundle_count, 0);
    }

    #[test]
    fn ignores_failed_transactions() {
        let pool = Pubkey::new_from_array([7u8; 32]);
        let our_pools: HashSet<Pubkey> = std::iter::once(pool).collect();
        let mut tx = make_tx_with_tip(pool, JITO_TIP_ACCOUNTS[0], 50_000);
        tx.failed = true;
        let r = scan_block_for_competitors(&[tx], &our_pools);
        assert_eq!(r.max_tip, 0);
        assert_eq!(r.bundle_count, 0);
    }

    #[test]
    fn ignores_transfer_to_non_jito_address() {
        let pool = Pubkey::new_from_array([7u8; 32]);
        let our_pools: HashSet<Pubkey> = std::iter::once(pool).collect();
        let random_dest = Pubkey::new_from_array([42u8; 32]);
        let txs = vec![make_tx_with_tip(pool, random_dest, 50_000)];
        let r = scan_block_for_competitors(&txs, &our_pools);
        assert_eq!(r.max_tip, 0);
        assert_eq!(r.bundle_count, 0);
    }

    #[test]
    fn returns_max_tip_across_multiple_competing_bundles() {
        let pool = Pubkey::new_from_array([7u8; 32]);
        let our_pools: HashSet<Pubkey> = std::iter::once(pool).collect();
        let txs = vec![
            make_tx_with_tip(pool, JITO_TIP_ACCOUNTS[0], 10_000),
            make_tx_with_tip(pool, JITO_TIP_ACCOUNTS[1], 75_000),
            make_tx_with_tip(pool, JITO_TIP_ACCOUNTS[2], 25_000),
        ];
        let r = scan_block_for_competitors(&txs, &our_pools);
        assert_eq!(r.max_tip, 75_000);
        assert_eq!(r.bundle_count, 3);
    }

    #[test]
    fn detects_jupiter_routed_bundle() {
        let pool = Pubkey::new_from_array([7u8; 32]);
        let our_pools: HashSet<Pubkey> = std::iter::once(pool).collect();
        let mut tx = make_tx_with_tip(pool, JITO_TIP_ACCOUNTS[0], 50_000);
        // Inject Jupiter v6 into the account_keys.
        tx.account_keys.push(JUPITER_V6_PROGRAM);
        let r = scan_block_for_competitors(&[tx], &our_pools);
        assert_eq!(r.bundle_count, 1);
        assert_eq!(r.via_jupiter, 1);
    }

    #[test]
    fn correlator_constructs() {
        let _ = JitoCorrelator::new(
            "https://example".into(),
            Arc::new(ScoutMetrics::default()),
            PathBuf::from("logs"),
        );
    }

    #[test]
    fn submit_then_pop_round_trips() {
        let c = JitoCorrelator::new(
            "https://example".into(),
            Arc::new(ScoutMetrics::default()),
            PathBuf::from("logs"),
        );
        let pool = Pubkey::new_from_array([1u8; 32]);
        let pools: HashSet<Pubkey> = std::iter::once(pool).collect();
        c.submit(12345, pools, 500_000, Utc::now());
        assert_eq!(c.inner.pending.lock().unwrap().len(), 1);
    }
}
