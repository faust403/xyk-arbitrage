//! Opportunity buffer + select-best scheduler for the native executor.
//!
//! ## Why this exists
//!
//! The original native executor used a per-call time throttle: any opportunity
//! arriving within `min_interval` of the previous attempt was dropped. With
//! 5 ops/s pouring in and a 1 s throttle, ~80% of routes were thrown away
//! WITHOUT being considered — meaning we'd often fire on a $0.01 opportunity
//! while a $1.00 one in the same window vanished.
//!
//! The buffer-and-select model:
//!
//! 1. Scout pushes every native-eligible opportunity into a shared ring,
//!    tagged with its predicted net profit.
//! 2. A background worker wakes every `min_interval` ms.
//! 3. The worker drops stale entries (older than `max_age`), then picks
//!    the highest-predicted-net entry, runs `maybe_execute` on it, and
//!    discards the rest of the buffer.
//!
//! The ring is FIFO-bounded (`max_capacity`) so a burst can't OOM us.
//!
//! ## Metric semantics
//!
//! - `submitted` — every successful `submit()` call. Maps to scout's
//!   "native-eligible opportunities seen".
//! - `dropped_stale` — entries that aged out before being picked. Indicates
//!   `min_interval` is too high (worker can't keep up).
//! - `dropped_overflow` — entries pushed when buffer was at `max_capacity`
//!   and the oldest had to be evicted. Indicates burst beyond ring size.
//! - `selected` — entries the worker actually fired on.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arb_pathfinder::Route;

/// One opportunity awaiting evaluation by the worker.
#[derive(Debug, Clone)]
pub struct BufferedOpp {
    pub route:              Route,
    pub probe_usdc:         u64,
    /// Scout's predicted net profit (USDC base units, can be negative). The
    /// worker uses the MAX of this over the buffer to pick the best entry.
    pub predicted_net_usdc: i64,
    /// When the scout submitted this opportunity. Used for staleness checks.
    pub submitted_at:       Instant,
}

/// Atomic metrics owned by the buffer. `Arc`-friendly so the executor's
/// existing metric snapshot can read these without locks.
#[derive(Debug, Default)]
pub struct BufferMetrics {
    pub submitted:        AtomicU64,
    pub dropped_stale:    AtomicU64,
    pub dropped_overflow: AtomicU64,
    pub selected:         AtomicU64,
    /// High-water mark of buffer occupancy since process start. Useful for
    /// tuning `max_capacity`.
    pub max_size_seen:    AtomicU64,
}

pub struct OpportunityBuffer {
    inner:        Mutex<VecDeque<BufferedOpp>>,
    /// Max age before an entry is considered stale. Older entries get
    /// dropped on the next pick. 1.5 s is enough that a worker firing
    /// every ~1 s catches every fresh opportunity.
    pub max_age:  Duration,
    /// Hard cap on buffer length — protects against a burst of 1000s of
    /// opportunities OOM-ing us. Under normal traffic (5–20 ops/s) the
    /// buffer rarely holds more than ~10.
    pub max_capacity: usize,
    pub metrics:  BufferMetrics,
}

impl OpportunityBuffer {
    pub fn new(max_age: Duration, max_capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(max_capacity.min(64))),
            max_age,
            max_capacity,
            metrics: BufferMetrics::default(),
        }
    }

    /// Push an opportunity. If the buffer is full, evicts the oldest entry
    /// (and increments `dropped_overflow`).
    pub fn submit(&self, route: Route, probe_usdc: u64, predicted_net_usdc: i64) {
        let mut buf = match self.inner.lock() {
            Ok(g) => g,
            // Lock is poisoned — recover by clearing and continuing. Better
            // than panicking the whole scout.
            Err(p) => p.into_inner(),
        };
        if buf.len() >= self.max_capacity {
            buf.pop_front();
            self.metrics.dropped_overflow.fetch_add(1, Ordering::Relaxed);
        }
        buf.push_back(BufferedOpp {
            route, probe_usdc, predicted_net_usdc,
            submitted_at: Instant::now(),
        });
        self.metrics.submitted.fetch_add(1, Ordering::Relaxed);
        let len = buf.len() as u64;
        let prev_max = self.metrics.max_size_seen.load(Ordering::Relaxed);
        if len > prev_max {
            self.metrics.max_size_seen.store(len, Ordering::Relaxed);
        }
    }

    /// Drain stale entries, then pick and return the highest-predicted-net
    /// entry. Non-winners that are still within `max_age` are **retained**
    /// for the next pick — they keep competing until they age out, so the
    /// buffer functions as a rolling "best opportunity seen in the last
    /// max_age window", not a per-interval bucket.
    ///
    /// This matters when `max_age` is set larger than the worker interval:
    /// a high-quality opportunity that arrives between two picks doesn't
    /// get thrown away just because it wasn't the absolute best when its
    /// pick fired — it stays in contention until either (a) it wins a
    /// later pick, or (b) it ages out.
    pub fn pick_best(&self) -> Option<BufferedOpp> {
        let mut buf = match self.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let now = Instant::now();
        // 1. Filter stale (in-place, retains in age-order).
        let before = buf.len();
        buf.retain(|o| now.duration_since(o.submitted_at) < self.max_age);
        let after  = buf.len();
        let stale = before - after;
        if stale > 0 {
            self.metrics.dropped_stale.fetch_add(stale as u64, Ordering::Relaxed);
        }
        if buf.is_empty() {
            return None;
        }
        // 2. Find the index with the highest predicted_net. Tie-break by
        // FRESHEST `submitted_at` so when scout floods the buffer with
        // unranked candidates (predicted_net = 0), we still pick the most
        // recent — matching the latest pool state. Without this tiebreak
        // `max_by_key` would return the FIRST max in iter order = oldest.
        let best_idx = buf.iter().enumerate()
            .max_by_key(|(_, o)| (o.predicted_net_usdc, o.submitted_at))
            .map(|(i, _)| i)?;
        let best = buf.remove(best_idx)?;
        // 3. Non-winners are RETAINED. They'll either win a later pick or
        // age out naturally. (Old behaviour cleared them here, which made
        // a long `max_age` window meaningless — every pick still only
        // saw entries from the immediately preceding interval.)
        self.metrics.selected.fetch_add(1, Ordering::Relaxed);
        Some(best)
    }

    pub fn current_size(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::TokenMint;
    use arb_pathfinder::Hop;
    use solana_program::pubkey::Pubkey;

    fn fake_route(idx: u8) -> Route {
        let m = TokenMint(Pubkey::new_from_array([idx; 32]));
        Route { hops: vec![
            Hop { pool: Pubkey::new_from_array([idx; 32]), token_in: m, token_out: m },
            Hop { pool: Pubkey::new_from_array([idx; 32]), token_in: m, token_out: m },
        ]}
    }

    #[test]
    fn submit_then_pick_best_returns_highest_net() {
        let buf = OpportunityBuffer::new(Duration::from_secs(2), 100);
        buf.submit(fake_route(1), 100, 50);
        buf.submit(fake_route(2), 200, 500); // best
        buf.submit(fake_route(3), 300, 200);
        let best = buf.pick_best().expect("nonempty");
        assert_eq!(best.predicted_net_usdc, 500);
        assert_eq!(best.probe_usdc, 200);
        // v62: non-winners retained. Next pick gets the next-best.
        let second = buf.pick_best().expect("two non-winners remain");
        assert_eq!(second.predicted_net_usdc, 200);
    }

    #[test]
    fn pick_best_drops_stale_entries() {
        let buf = OpportunityBuffer::new(Duration::from_millis(50), 100);
        buf.submit(fake_route(1), 100, 999);
        std::thread::sleep(Duration::from_millis(80));
        // The high-net entry is stale; pick should return None.
        assert!(buf.pick_best().is_none());
        assert_eq!(buf.metrics.dropped_stale.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn submit_evicts_oldest_when_full() {
        let buf = OpportunityBuffer::new(Duration::from_secs(2), 3);
        buf.submit(fake_route(1), 100, 1);
        buf.submit(fake_route(2), 100, 2);
        buf.submit(fake_route(3), 100, 3);
        buf.submit(fake_route(4), 100, 4); // evicts route(1)
        assert_eq!(buf.metrics.dropped_overflow.load(Ordering::Relaxed), 1);
        let best = buf.pick_best().expect("nonempty");
        assert_eq!(best.predicted_net_usdc, 4);
    }

    #[test]
    fn pick_best_retains_non_winners_for_next_pick() {
        // v62: non-winners are no longer cleared — they keep competing in
        // subsequent picks until they age out. The winner is removed; the
        // remaining (still-fresh) entries persist.
        let buf = OpportunityBuffer::new(Duration::from_secs(2), 100);
        buf.submit(fake_route(1), 100, 10);
        buf.submit(fake_route(2), 100, 20);
        buf.submit(fake_route(3), 100, 30);
        let first = buf.pick_best().unwrap();
        assert_eq!(first.predicted_net_usdc, 30);
        // Two entries remain — they were not cleared.
        assert_eq!(buf.metrics.dropped_stale.load(Ordering::Relaxed), 0);
        assert_eq!(buf.current_size(), 2);
        // Next pick selects the next-best from what remained.
        let second = buf.pick_best().unwrap();
        assert_eq!(second.predicted_net_usdc, 20);
        assert_eq!(buf.current_size(), 1);
    }

    #[test]
    fn pick_best_with_negative_net_still_returns_max() {
        let buf = OpportunityBuffer::new(Duration::from_secs(2), 100);
        buf.submit(fake_route(1), 100, -100);
        buf.submit(fake_route(2), 100, -10); // best (least bad)
        buf.submit(fake_route(3), 100, -50);
        let best = buf.pick_best().expect("nonempty");
        assert_eq!(best.predicted_net_usdc, -10);
    }

    #[test]
    fn empty_buffer_pick_returns_none_no_metrics() {
        let buf = OpportunityBuffer::new(Duration::from_secs(2), 100);
        assert!(buf.pick_best().is_none());
        assert_eq!(buf.metrics.selected.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn max_size_seen_tracks_high_water_mark() {
        let buf = OpportunityBuffer::new(Duration::from_secs(2), 100);
        buf.submit(fake_route(1), 100, 1);
        buf.submit(fake_route(2), 100, 2);
        buf.submit(fake_route(3), 100, 3);
        assert_eq!(buf.metrics.max_size_seen.load(Ordering::Relaxed), 3);
        let _ = buf.pick_best();
        // Drain doesn't reduce max_size_seen.
        buf.submit(fake_route(4), 100, 4);
        assert_eq!(buf.metrics.max_size_seen.load(Ordering::Relaxed), 3);
    }
}
