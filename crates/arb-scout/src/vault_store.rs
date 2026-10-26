//! Vault balance store: maps SPL Token Account address → current `amount`.
//!
//! Several DEX integrations (notably Raydium AMM v4) keep their reserves in
//! separate SPL Token vault accounts, NOT inside the pool account itself. The
//! quoter needs both the pool's parsed fields AND the vault balances at quote
//! time. This module provides the shared lookup surface.
//!
//! Population happens via the WSS subscriber: vault accounts are subscribed
//! alongside pool accounts, and updates whose owner is the SPL Token program
//! are decoded here instead of by a `DexQuoter`.

use std::sync::Arc;

use arb_dex_raydium::VaultLookup;
use dashmap::DashMap;
use solana_program::pubkey::Pubkey;

#[derive(Debug, Default)]
pub struct VaultStore {
    balances: DashMap<Pubkey, u64>,
}

impl VaultStore {
    pub fn new() -> Arc<Self> { Arc::new(Self::default()) }

    pub fn upsert(&self, vault: Pubkey, amount: u64) {
        self.balances.insert(vault, amount);
    }

    pub fn get(&self, vault: &Pubkey) -> Option<u64> {
        self.balances.get(vault).map(|v| *v.value())
    }

    pub fn len(&self) -> usize { self.balances.len() }
    pub fn is_empty(&self) -> bool { self.balances.is_empty() }
}

/// Lets the Raydium AMM v4 quoter (in arb-dex-raydium) read this store
/// without depending on scout-internal types.
impl VaultLookup for VaultStore {
    fn get(&self, vault: &Pubkey) -> Option<u64> {
        VaultStore::get(self, vault)
    }
}

/// Decode the `amount` field (u64 LE at offset 64) of a legacy SPL Token
/// account. Returns `None` if `data` is too short to be a token account.
///
/// Layout (relevant prefix; first 72 bytes):
/// - `[0..32)` mint
/// - `[32..64)` owner
/// - `[64..72)` amount (u64 LE)
///
/// Token-2022 accounts share this prefix layout, so the same offset works for
/// both. We don't try to validate the trailing fields.
pub fn decode_spl_token_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 72 { return None; }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&data[64..72]);
    Some(u64::from_le_bytes(buf))
}

/// SPL Token program IDs. The legacy program covers the vast majority of
/// pool vaults; Token-2022 is included for forward compatibility.
pub const SPL_TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const SPL_TOKEN_2022_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub fn is_spl_token_program(owner: &Pubkey) -> bool {
    *owner == SPL_TOKEN_PROGRAM_ID || *owner == SPL_TOKEN_2022_PROGRAM_ID
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_and_get_round_trip() {
        let s = VaultStore::default();
        let v = Pubkey::new_from_array([3u8; 32]);
        s.upsert(v, 12_345);
        assert_eq!(s.get(&v), Some(12_345));
        s.upsert(v, 99_999);
        assert_eq!(s.get(&v), Some(99_999));
    }

    #[test]
    fn decode_amount_at_correct_offset() {
        let mut data = vec![0u8; 165];
        // amount = 1_000_000_000 (1 SOL)
        data[64..72].copy_from_slice(&1_000_000_000u64.to_le_bytes());
        assert_eq!(decode_spl_token_amount(&data), Some(1_000_000_000));
    }

    #[test]
    fn decode_returns_none_on_short_data() {
        assert_eq!(decode_spl_token_amount(&[0u8; 50]), None);
    }

    #[test]
    fn is_spl_token_program_recognises_both_variants() {
        assert!(is_spl_token_program(&SPL_TOKEN_PROGRAM_ID));
        assert!(is_spl_token_program(&SPL_TOKEN_2022_PROGRAM_ID));
        assert!(!is_spl_token_program(&Pubkey::new_from_array([0u8; 32])));
    }
}
