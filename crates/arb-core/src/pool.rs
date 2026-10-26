use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;

use crate::token::TokenMint;

/// Discriminator for the DEX a [`Pool`] belongs to. Used by the orchestrator
/// to dispatch the right [`crate::DexQuoter`] for parsing or quoting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PoolKind {
    RaydiumAmmV4,
    RaydiumClmm,
    RaydiumCpmm,
    OrcaWhirlpool,
    MeteoraDlmm,
    MeteoraDammV2,
    PhoenixMarket,
    LifinityV2,
    Bonkswap,
    ObricV2,
    SanctumLst,
    PumpFun,
}

impl PoolKind {
    /// Stable string ID, matching the corresponding `DexQuoter::dex_id`.
    pub fn id(&self) -> &'static str {
        match self {
            PoolKind::RaydiumAmmV4   => "raydium-amm-v4",
            PoolKind::RaydiumClmm    => "raydium-clmm",
            PoolKind::RaydiumCpmm    => "raydium-cpmm",
            PoolKind::OrcaWhirlpool  => "orca-whirlpool",
            PoolKind::MeteoraDlmm    => "meteora-dlmm",
            PoolKind::MeteoraDammV2  => "meteora-damm-v2",
            PoolKind::PhoenixMarket  => "phoenix",
            PoolKind::LifinityV2     => "lifinity-v2",
            PoolKind::Bonkswap       => "bonkswap",
            PoolKind::ObricV2        => "obric-v2",
            PoolKind::SanctumLst     => "sanctum-lst",
            PoolKind::PumpFun        => "pump-fun",
        }
    }
}

/// Raw on-chain account bytes for a pool. Held opaquely so each DEX can parse
/// its own layout on demand without forcing every other crate to know the
/// fields. Per Phase 1 design ("raw bytes + parser-on-demand").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PoolState {
    /// Raw bytes of the account `data` field, exactly as returned by the RPC.
    pub data: Vec<u8>,
}

impl PoolState {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data }
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// DEX-agnostic representation of a liquidity venue. The two `token_*` fields
/// are the *canonical* trade pair; richer DEXs (orderbooks, multi-asset pools)
/// still expose a primary pair for the pathfinder.
#[derive(Debug, Clone)]
pub struct Pool {
    pub address:           Pubkey,
    pub kind:              PoolKind,
    pub token_a:           TokenMint,
    pub token_b:           TokenMint,
    pub state:             PoolState,
    pub last_update_slot:  u64,
}

impl Pool {
    pub fn new(
        address:          Pubkey,
        kind:             PoolKind,
        token_a:          TokenMint,
        token_b:          TokenMint,
        state:            PoolState,
        last_update_slot: u64,
    ) -> Self {
        Self { address, kind, token_a, token_b, state, last_update_slot }
    }

    /// True if this pool quotes the given mint (as either side of the pair).
    pub fn supports(&self, mint: &TokenMint) -> bool {
        &self.token_a == mint || &self.token_b == mint
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pool_kind_id_round_trip() {
        // Each variant has a unique stable id.
        let kinds = [
            PoolKind::RaydiumAmmV4,
            PoolKind::RaydiumClmm,
            PoolKind::OrcaWhirlpool,
            PoolKind::MeteoraDlmm,
            PoolKind::PhoenixMarket,
            PoolKind::LifinityV2,
        ];
        let ids: Vec<&'static str> = kinds.iter().map(|k| k.id()).collect();
        // All distinct.
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), kinds.len());
    }

    #[test]
    fn test_pool_kind_serde() {
        for k in [
            PoolKind::RaydiumAmmV4,
            PoolKind::OrcaWhirlpool,
            PoolKind::MeteoraDlmm,
        ] {
            let s = serde_json::to_string(&k).unwrap();
            let back: PoolKind = serde_json::from_str(&s).unwrap();
            assert_eq!(k, back);
        }
    }

    #[test]
    fn test_pool_state_basic() {
        let st = PoolState::new(vec![1, 2, 3]);
        assert_eq!(st.len(), 3);
        assert!(!st.is_empty());
        assert_eq!(st.as_slice(), &[1, 2, 3]);
    }

    #[test]
    fn test_pool_supports_either_side() {
        let m_a = TokenMint(Pubkey::new_from_array([1u8; 32]));
        let m_b = TokenMint(Pubkey::new_from_array([2u8; 32]));
        let m_c = TokenMint(Pubkey::new_from_array([3u8; 32]));
        let pool = Pool::new(
            Pubkey::new_from_array([9u8; 32]),
            PoolKind::RaydiumAmmV4,
            m_a, m_b,
            PoolState::new(vec![]),
            42,
        );
        assert!(pool.supports(&m_a));
        assert!(pool.supports(&m_b));
        assert!(!pool.supports(&m_c));
    }
}
