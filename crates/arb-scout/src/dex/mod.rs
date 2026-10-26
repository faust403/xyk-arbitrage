//! DEX integrations used by the scout.
//!
//! Per `INSTRUCTIONS.md` Phase 5+, the canonical Raydium implementation now
//! lives in the `arb-dex-raydium` crate; this module re-exports it via
//! `all_quoters` and continues to host the DEXes that have not yet been
//! extracted (Orca, Meteora, Lifinity, Obric, Bonkswap, Whirlpool swap-ix).

use std::sync::Arc;

use arb_core::DexQuoter;
use arb_dex_lifinity::LifinityV2Quoter;
use arb_dex_meteora::MeteoraDlmmQuoter;
use arb_dex_obric::ObricV2Quoter;
use arb_dex_orca::OrcaWhirlpoolQuoter;
use arb_dex_raydium::{
    ClmmFeeStore, RaydiumAmmV4Quoter, RaydiumClmmQuoter, RaydiumCpmmQuoter, VaultLookup,
};

use crate::vault_store::VaultStore;

pub mod bonkswap;
pub mod openbook_market;

/// All quoters the scout uses, in dispatch order. The first one whose
/// `try_parse_account` returns `Some` claims the update.
///
/// External state passed in by the scout:
/// * `vaults` — shared SPL Token amount lookup (Raydium AMM v4 + Obric).
/// * `clmm_fees` — shared CLMM-pool → trade-fee-rate lookup (populated from
///   AmmConfig accounts during bootstrap).
pub fn all_quoters(
    vaults:    Arc<VaultStore>,
    clmm_fees: Arc<ClmmFeeStore>,
) -> Vec<Arc<dyn DexQuoter>> {
    // Both Raydium AMM v4 and Obric V2 take an `Arc<dyn VaultLookup>` rather
    // than the concrete `VaultStore`, so the same lookup object is shared
    // across both quoters at no extra cost.
    let vault_lookup: Arc<dyn VaultLookup> = vaults as Arc<dyn VaultLookup>;
    vec![
        Arc::new(RaydiumAmmV4Quoter::new(Arc::clone(&vault_lookup))),
        Arc::new(RaydiumClmmQuoter::new(clmm_fees)),
        Arc::new(RaydiumCpmmQuoter::new(Arc::clone(&vault_lookup))),
        Arc::new(OrcaWhirlpoolQuoter::new()),
        Arc::new(MeteoraDlmmQuoter::new()),
        Arc::new(LifinityV2Quoter::new()),
        Arc::new(bonkswap::BonkswapQuoter::new()),
        Arc::new(ObricV2Quoter::new(Arc::clone(&vault_lookup))),
    ]
}
