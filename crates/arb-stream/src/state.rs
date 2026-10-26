//! Lock-free pool state store. Simplified version of the full bot's
//! `PoolStateStore` — no broadcast channel, no pool ARC swapping; the scout
//! is single-evaluator so a simple `DashMap<Pubkey, Arc<Pool>>` suffices.

use std::sync::Arc;

use arb_core::Pool;
use dashmap::DashMap;
use solana_program::pubkey::Pubkey;

#[derive(Debug, Default)]
pub struct PoolStateStore {
    pools: DashMap<Pubkey, Arc<Pool>>,
}

impl PoolStateStore {
    pub fn new() -> Self { Self::default() }

    pub fn upsert(&self, pool: Pool) {
        self.pools.insert(pool.address, Arc::new(pool));
    }

    pub fn get(&self, address: &Pubkey) -> Option<Arc<Pool>> {
        self.pools.get(address).map(|r| Arc::clone(r.value()))
    }

    pub fn len(&self) -> usize {
        self.pools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pools.is_empty()
    }

    /// Snapshot every pool currently in the store. Cloned out so callers don't
    /// hold a `DashMap` shard lock while doing work.
    pub fn snapshot(&self) -> Vec<std::sync::Arc<Pool>> {
        self.pools.iter().map(|r| std::sync::Arc::clone(r.value())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_core::{PoolKind, PoolState, TokenMint};

    fn make_pool(seed: u8) -> Pool {
        let mint = TokenMint(Pubkey::new_from_array([1u8; 32]));
        Pool::new(
            Pubkey::new_from_array([seed; 32]),
            PoolKind::RaydiumAmmV4,
            mint, mint,
            PoolState::new(vec![seed]),
            0,
        )
    }

    #[test]
    fn upsert_inserts_and_get_returns() {
        let s = PoolStateStore::new();
        let p = make_pool(7);
        let addr = p.address;
        s.upsert(p);
        assert_eq!(s.len(), 1);
        let got = s.get(&addr).unwrap();
        assert_eq!(got.address, addr);
    }

    #[test]
    fn upsert_replaces_existing() {
        let s = PoolStateStore::new();
        let p1 = make_pool(7);
        let mut p2 = make_pool(7);
        p2.last_update_slot = 999;
        s.upsert(p1);
        s.upsert(p2);
        assert_eq!(s.len(), 1);
        assert_eq!(s.get(&Pubkey::new_from_array([7u8; 32])).unwrap().last_update_slot, 999);
    }
}
