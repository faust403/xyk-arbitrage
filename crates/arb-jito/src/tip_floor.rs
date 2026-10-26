//! Dynamic Jito tip-floor poller.
//!
//! Jito publishes recent tip percentiles (25/50/75/95/99) at a public REST
//! endpoint. Polling every ~15 s gives the executor a live read on the
//! competitive tip floor: a bundle that tips below the 50th percentile of
//! recent landed tips will lose every contested auction in the cluster.
//!
//! ## Algorithm rationale
//!
//! The naive approach is "tip a fixed N lamports" (`EXECUTOR_JITO_TIP_LAMPORTS`).
//! That fights the wrong battle: in calm slots N is too high (overpaying);
//! in busy slots N is too low (every bundle drops). The poller exposes
//! `p25/p50/p75/p99` as `Arc<AtomicU64>` so the executor can:
//!
//! 1. Floor the tip at `p50` (so we're at least mid-pack).
//! 2. Cap the tip at a configurable `% of net profit` (so we never pay
//!    more in tip than we earn).
//! 3. Resolve ties by preferring `p75` when profit allows — paying ~50%
//!    more than half the cluster lifts our land rate from ~50% → ~75%.
//!
//! ## Endpoint
//!
//! `https://bundles.jito.wtf/api/v1/bundles/tip_floor`
//!
//! Returns a JSON array of one entry, with float lamport amounts at each
//! percentile. We round to u64 lamports.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tracing::{debug, warn};

const TIP_FLOOR_URL: &str = "https://bundles.jito.wtf/api/v1/bundles/tip_floor";

/// Default poll interval. Jito's data updates every block (~400ms) but the
/// percentiles only meaningfully shift over many slots; 15s is the sweet
/// spot between freshness and HTTP load.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// Conservative starter values used until the first poll succeeds. p50 of
/// 1000 lamports (~$0.0001) is the bare Jito minimum that will land
/// uncontested slots. Bumped to 10_000 for p75 / p99 so we don't tip too
/// low if the endpoint is down on startup.
pub const STARTER_P25: u64 = 1_000;
pub const STARTER_P50: u64 = 1_000;
pub const STARTER_P75: u64 = 5_000;
pub const STARTER_P95: u64 = 10_000;
pub const STARTER_P99: u64 = 50_000;

/// Percentiles published by Jito for landed tips. Field names mirror the
/// REST response so deserialisation is direct.
#[derive(Debug, Clone, Deserialize)]
struct TipFloorRow {
    landed_tips_25th_percentile: f64,
    landed_tips_50th_percentile: f64,
    landed_tips_75th_percentile: f64,
    landed_tips_95th_percentile: f64,
    landed_tips_99th_percentile: f64,
}

/// Atomic snapshot of the latest tip percentiles in lamports. Cheap to
/// clone (`Arc`-internal) so each executor can read without a mutex.
#[derive(Debug, Clone, Default)]
pub struct TipFloor {
    /// 25th percentile of recently-landed tips (lamports).
    pub p25: Arc<AtomicU64>,
    /// 50th percentile (median) — landed tip you need to beat to be in
    /// the upper half of the auction.
    pub p50: Arc<AtomicU64>,
    /// 75th percentile — tip required to land ~3 of 4 contested slots.
    pub p75: Arc<AtomicU64>,
    /// 95th percentile — near-aggressive landing, used as the dynamic
    /// ceiling when profit margin is generous.
    pub p95: Arc<AtomicU64>,
    /// 99th percentile — max tip we'd ever rationally pay.
    pub p99: Arc<AtomicU64>,
    /// Unix-millis of the last successful poll. Zero until first success.
    pub last_update_ms: Arc<AtomicU64>,
    /// Number of poll *failures* (HTTP error, parse error). Increments
    /// monotonically; useful for stat lines.
    pub failures: Arc<AtomicU64>,
}

impl TipFloor {
    /// Build with conservative starter values. Use `start_polling` to keep
    /// the values fresh.
    pub fn new() -> Self {
        let f = Self::default();
        f.p25.store(STARTER_P25, Ordering::Relaxed);
        f.p50.store(STARTER_P50, Ordering::Relaxed);
        f.p75.store(STARTER_P75, Ordering::Relaxed);
        f.p95.store(STARTER_P95, Ordering::Relaxed);
        f.p99.store(STARTER_P99, Ordering::Relaxed);
        f
    }

    /// Read the current 50th-percentile tip (median landed).
    pub fn p50_lamports(&self) -> u64 { self.p50.load(Ordering::Relaxed) }
    pub fn p75_lamports(&self) -> u64 { self.p75.load(Ordering::Relaxed) }
    pub fn p95_lamports(&self) -> u64 { self.p95.load(Ordering::Relaxed) }
    pub fn p99_lamports(&self) -> u64 { self.p99.load(Ordering::Relaxed) }

    /// Compute a dynamic tip given a profit (in lamports) and a profit-share
    /// percentage. Algorithm:
    ///
    /// 1. `desired = profit_lamports * tip_pct / 100`
    /// 2. `floor   = self.p50` (must be at least mid-pack to land)
    /// 3. `cap     = profit_lamports * 90 / 100` (never pay more than 90% of profit —
    ///    leaves at least 10% net for us; raised from 50% so we can outbid harder)
    /// 4. Return `desired.max(floor).min(cap)`. If `floor > cap` (profit too
    ///    small to outbid even the median), return `Err(())` — caller should skip.
    pub fn dynamic_tip(&self, profit_lamports: u64, tip_pct: u32) -> Result<u64, ()> {
        self.dynamic_tip_with_floor(profit_lamports, tip_pct, self.p50_lamports())
    }

    /// Same as `dynamic_tip` but takes an explicit floor (lamports). The
    /// floor is the minimum tip the caller is willing to pay — typically one
    /// of `p50_lamports()` (mid-pack, ~50% land rate), `p75_lamports()`
    /// (~75% land rate), or `p95_lamports()` (~95% land rate). At our
    /// volumes (0 bundles landed of 8 sent), flooring at p75 trades a
    /// slightly larger tip for a meaningful jump in land probability.
    pub fn dynamic_tip_with_floor(&self, profit_lamports: u64, tip_pct: u32, floor: u64) -> Result<u64, ()> {
        if profit_lamports == 0 { return Err(()); }
        let desired = (profit_lamports as u128 * tip_pct as u128 / 100) as u64;
        let cap     = (profit_lamports as u128 * 90 / 100) as u64;
        if floor > cap {
            return Err(());
        }
        Ok(desired.max(floor).min(cap))
    }

    /// Resolve a percentile name (25/50/75/95/99) to the corresponding
    /// latest-polled lamport value. Unknown percentiles fall back to p75.
    pub fn percentile_lamports(&self, pctile: u32) -> u64 {
        match pctile {
            25 => self.p25.load(Ordering::Relaxed),
            50 => self.p50_lamports(),
            75 => self.p75_lamports(),
            95 => self.p95_lamports(),
            99 => self.p99_lamports(),
            _  => self.p75_lamports(),
        }
    }

    /// Apply one fetched tip-floor row.
    fn apply(&self, row: &TipFloorRow) {
        // BUGFIX 2026-05-15: Jito returns tip percentiles in SOL units
        // (e.g. p50=3.282e-6 = 3,282 lamports), not in lamports. The
        // prior `f as u64` cast clobbered every percentile to 0 because
        // every value was a tiny fraction < 1. Result: floor=0 for the
        // bot's entire history; bundles never had a real auction floor.
        // Convert SOL → lamports by multiplying by 1e9 before casting.
        let sol_to_lamports = |sol: f64| -> u64 {
            if !sol.is_finite() || sol < 0.0 { 0 } else { (sol * 1_000_000_000.0) as u64 }
        };
        self.p25.store(sol_to_lamports(row.landed_tips_25th_percentile), Ordering::Relaxed);
        self.p50.store(sol_to_lamports(row.landed_tips_50th_percentile), Ordering::Relaxed);
        self.p75.store(sol_to_lamports(row.landed_tips_75th_percentile), Ordering::Relaxed);
        self.p95.store(sol_to_lamports(row.landed_tips_95th_percentile), Ordering::Relaxed);
        self.p99.store(sol_to_lamports(row.landed_tips_99th_percentile), Ordering::Relaxed);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64).unwrap_or(0);
        self.last_update_ms.store(now_ms, Ordering::Relaxed);
    }
}

/// Spawn a background task that polls Jito's tip-floor every `interval`
/// seconds and updates `floor`. Runs forever; intended to live for the
/// process lifetime.
///
/// The first poll fires immediately so the executor doesn't run for 15s
/// on the starter defaults.
pub fn start_polling(floor: TipFloor, interval: Duration) {
    tokio::spawn(async move {
        // Reqwest client is created once and reused — cheap to clone, so
        // future enhancements can hand it out to other Jito helpers.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        let mut tick = tokio::time::interval(interval);
        // The first tick fires immediately by default; explicitly reset so
        // the first iteration runs at t=0.
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tick.tick().await;
            match poll_once(&client).await {
                Ok(row) => {
                    floor.apply(&row);
                    debug!(target: "jito::tip",
                           p50 = floor.p50_lamports(),
                           p75 = floor.p75_lamports(),
                           p99 = floor.p99_lamports(),
                           "tip floor updated");
                }
                Err(e) => {
                    floor.failures.fetch_add(1, Ordering::Relaxed);
                    // Most failures are transient (DNS, 503); WARN at INFO
                    // cadence to avoid log spam if the endpoint is down for
                    // an hour. The starter values keep the bot competitive.
                    warn!(target: "jito::tip", error = %e,
                          "tip floor poll failed — keeping previous values");
                }
            }
        }
    });
}

/// Single poll of the tip-floor endpoint. Returns the freshest row.
async fn poll_once(client: &reqwest::Client) -> Result<TipFloorRow, String> {
    let resp = client
        .get(TIP_FLOOR_URL)
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("status: {}", resp.status()));
    }
    let rows: Vec<TipFloorRow> = resp.json().await
        .map_err(|e| format!("parse: {e}"))?;
    rows.into_iter().next()
        .ok_or_else(|| "tip-floor response empty array".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_values_set() {
        let f = TipFloor::new();
        assert_eq!(f.p50_lamports(), STARTER_P50);
        assert_eq!(f.p75_lamports(), STARTER_P75);
        assert_eq!(f.p99_lamports(), STARTER_P99);
    }

    #[test]
    fn dynamic_tip_returns_floor_when_profit_allows() {
        let f = TipFloor::new();
        f.p50.store(5_000, Ordering::Relaxed);
        // profit=1_000_000 lamports, tip_pct=10% → desired=100_000
        // floor=5_000, cap=500_000. Result: max(100k, 5k).min(500k) = 100_000
        assert_eq!(f.dynamic_tip(1_000_000, 10).unwrap(), 100_000);
    }

    #[test]
    fn dynamic_tip_floors_when_pct_below_p50() {
        let f = TipFloor::new();
        f.p50.store(50_000, Ordering::Relaxed);
        // profit=1_000_000 lamports, tip_pct=1% → desired=10_000
        // floor=50_000 → clamps up to 50_000.
        assert_eq!(f.dynamic_tip(1_000_000, 1).unwrap(), 50_000);
    }

    #[test]
    fn dynamic_tip_rejects_when_profit_too_small() {
        let f = TipFloor::new();
        f.p50.store(100_000, Ordering::Relaxed);
        // profit=100_000 lamports, cap=50_000, floor=100_000 → can't satisfy.
        assert_eq!(f.dynamic_tip(100_000, 25), Err(()));
    }

    #[test]
    fn dynamic_tip_caps_at_90pct_profit() {
        let f = TipFloor::new();
        f.p50.store(1_000, Ordering::Relaxed);
        // profit=100_000, tip_pct=95% → desired=95_000
        // cap = 90_000 → clamped down.
        assert_eq!(f.dynamic_tip(100_000, 95).unwrap(), 90_000);
    }

    #[test]
    fn dynamic_tip_zero_profit_rejected() {
        let f = TipFloor::new();
        assert_eq!(f.dynamic_tip(0, 25), Err(()));
    }

    #[test]
    fn apply_clamps_negative_or_nan() {
        // Inputs are in SOL (Jito API format); apply() converts to lamports.
        let f = TipFloor::new();
        let bad = TipFloorRow {
            landed_tips_25th_percentile: -100.0,         // negative → 0
            landed_tips_50th_percentile: f64::NAN,       // NaN → 0
            landed_tips_75th_percentile: 0.000025,       // 25k lamports
            landed_tips_95th_percentile: 0.0001,         // 100k lamports
            landed_tips_99th_percentile: f64::INFINITY,  // infinite → 0
        };
        f.apply(&bad);
        assert_eq!(f.p25.load(Ordering::Relaxed), 0);
        assert_eq!(f.p50.load(Ordering::Relaxed), 0);
        assert_eq!(f.p75.load(Ordering::Relaxed), 25_000);
        assert_eq!(f.p95.load(Ordering::Relaxed), 100_000);
        assert_eq!(f.p99.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn apply_converts_sol_input_to_lamports() {
        // Real Jito values from 2026-05-15 16:31 UTC.
        let f = TipFloor::new();
        let row = TipFloorRow {
            landed_tips_25th_percentile: 1.798e-6,           // 1,798 lamports
            landed_tips_50th_percentile: 3.282e-6,           // 3,282 lamports
            landed_tips_75th_percentile: 8.984e-6,           // 8,984 lamports
            landed_tips_95th_percentile: 0.000384,           // 384,000 lamports
            landed_tips_99th_percentile: 0.012576,           // 12,576,000 lamports
        };
        f.apply(&row);
        assert_eq!(f.p25.load(Ordering::Relaxed), 1_798);
        assert_eq!(f.p50.load(Ordering::Relaxed), 3_282);
        assert_eq!(f.p75.load(Ordering::Relaxed), 8_984);
        assert_eq!(f.p95.load(Ordering::Relaxed), 384_000);
        assert_eq!(f.p99.load(Ordering::Relaxed), 12_576_000);
    }
}
