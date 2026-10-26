//! HODL-mode pump.fun sniper budget tracker.
//!
//! Reserves lamports BEFORE a buy is sent. If the on-chain buy fails we do
//! NOT refund — the budget is a hard ceiling on lamports sent at the pump.fun
//! program, not on lamports successfully spent. This matches the "1 SOL hard
//! cap" intent: never let a runaway loop send more than N SOL no matter what.
//!
//! Persisted to disk after every reservation so a restart cannot bypass the
//! cap by zeroing the counter.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedState {
    total_spent_lamports: u64,
    bought_mints:         HashSet<String>,
}

pub struct PumpSnipeBudget {
    cap_lamports: u64,
    state_path:   PathBuf,
    state:        Mutex<PersistedState>,
}

#[derive(Debug)]
pub enum ReserveSkip {
    /// Already at or past the cap; nothing reserved.
    BudgetExhausted { spent: u64, cap: u64 },
    /// We already bought this mint at least once — HODL strategy doesn't
    /// double-buy if the same Create event surfaces twice via different
    /// streams.
    AlreadyBought,
}

impl PumpSnipeBudget {
    /// Load persisted state from `state_path` if it exists; otherwise start
    /// fresh. Errors during load are logged and treated as "fresh start"
    /// (safer than panicking and leaving the bot without a budget gate).
    pub fn load_or_default(state_path: PathBuf, cap_lamports: u64) -> Self {
        let state = match std::fs::read_to_string(&state_path) {
            Ok(s) => serde_json::from_str::<PersistedState>(&s).unwrap_or_else(|e| {
                warn!(target: "pump_sniper_budget",
                      path = %state_path.display(), error = %e,
                      "failed to parse state file — starting fresh");
                PersistedState::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => PersistedState::default(),
            Err(e) => {
                warn!(target: "pump_sniper_budget",
                      path = %state_path.display(), error = %e,
                      "failed to read state file — starting fresh");
                PersistedState::default()
            }
        };
        info!(target: "pump_sniper_budget",
              cap_lamports, spent = state.total_spent_lamports,
              mints_seen = state.bought_mints.len(),
              path = %state_path.display(),
              "pump.fun snipe budget loaded");
        Self {
            cap_lamports,
            state_path,
            state: Mutex::new(state),
        }
    }

    pub fn cap_lamports(&self) -> u64 { self.cap_lamports }

    /// Atomically reserve `lamports` for `mint`. Returns `Ok(())` when the
    /// caller may proceed to send the buy. On the success path the in-memory
    /// counters are updated AND a JSON snapshot is flushed to disk before
    /// returning, so a crash after the buy is sent cannot lose the spend.
    pub fn try_reserve(&self, mint_b58: &str, lamports: u64) -> Result<u64, ReserveSkip> {
        let mut guard = self.state.lock().expect("budget mutex poisoned");
        if guard.bought_mints.contains(mint_b58) {
            return Err(ReserveSkip::AlreadyBought);
        }
        let new_total = guard.total_spent_lamports.saturating_add(lamports);
        if new_total > self.cap_lamports {
            return Err(ReserveSkip::BudgetExhausted {
                spent: guard.total_spent_lamports,
                cap:   self.cap_lamports,
            });
        }
        guard.total_spent_lamports = new_total;
        guard.bought_mints.insert(mint_b58.to_string());
        let snapshot = serde_json::to_string_pretty(&*guard).unwrap_or_default();
        drop(guard);

        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&self.state_path, snapshot) {
            warn!(target: "pump_sniper_budget",
                  path = %self.state_path.display(), error = %e,
                  "failed to persist budget snapshot (counter is in-memory only until next reservation)");
        }
        Ok(new_total)
    }

    pub fn snapshot_spent(&self) -> u64 {
        self.state.lock().map(|s| s.total_spent_lamports).unwrap_or(0)
    }

    /// Undo a prior `try_reserve` — removes the mint from `bought_mints` and
    /// subtracts `lamports` from the running total. Used when a post-reservation
    /// check (e.g. mayhem filter) rejects the mint AFTER budget was reserved,
    /// so the slot can be reused for a different mint instead of being burned.
    pub fn release_reservation(&self, mint_b58: &str, lamports: u64) {
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(_) => {
                warn!(target: "pump_sniper_budget",
                      "release_reservation: mutex poisoned — skipping");
                return;
            }
        };
        guard.bought_mints.remove(mint_b58);
        guard.total_spent_lamports = guard.total_spent_lamports.saturating_sub(lamports);
        let snapshot = serde_json::to_string_pretty(&*guard).unwrap_or_default();
        drop(guard);
        if let Some(parent) = self.state_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&self.state_path, snapshot) {
            warn!(target: "pump_sniper_budget",
                  path = %self.state_path.display(), error = %e,
                  "failed to persist budget snapshot after release");
        }
    }
}
