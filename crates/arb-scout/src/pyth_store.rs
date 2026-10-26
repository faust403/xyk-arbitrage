//! Pyth price-feed cache. Mirrors `VaultStore`'s shape but stores raw
//! account bytes (the Obric quoter parses fields lazily — `parse_pyth_price`
//! is a few hundred ns and avoids re-fetching).
//!
//! Population is via the same WSS path as pools/vaults: every Obric pool's
//! `x_price_feed` and `y_price_feed` pubkeys are added to the WSS
//! subscription list at bootstrap, and updates land here. A periodic
//! background refresher (~5s) tops up any feeds that don't get WSS updates
//! quickly enough to stay fresh for quoting.

use std::sync::Arc;

use dashmap::DashMap;
use solana_program::pubkey::Pubkey;

#[derive(Debug, Default)]
pub struct PythStore {
    feeds: DashMap<Pubkey, Vec<u8>>,
}

impl PythStore {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    pub fn upsert(&self, pubkey: Pubkey, data: Vec<u8>) {
        self.feeds.insert(pubkey, data);
    }

    pub fn get(&self, pubkey: &Pubkey) -> Option<Vec<u8>> {
        self.feeds.get(pubkey).map(|v| v.value().clone())
    }

    pub fn len(&self) -> usize { self.feeds.len() }
    pub fn is_empty(&self) -> bool { self.feeds.is_empty() }
}

/// The Pyth price-account program-id. WSS updates from this program are
/// stored verbatim in the `PythStore`.
pub const PYTH_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("FsJ3A3u2vn5cTVofAjvy6y5kwABJAqYWpe4975bi2epH");
pub const PYTH_PROGRAM_ID_PULL: Pubkey =
    solana_program::pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_and_get_round_trip() {
        let s = PythStore::default();
        let pk = Pubkey::new_from_array([7u8; 32]);
        s.upsert(pk, vec![1, 2, 3]);
        assert_eq!(s.get(&pk), Some(vec![1, 2, 3]));
    }
}
