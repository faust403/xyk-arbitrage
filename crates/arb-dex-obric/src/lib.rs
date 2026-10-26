//! Obric V2 — proprietary AMM with Pyth-oracle pricing + concentration.
//!
//! Phase 12 / native-execution path. The scout's quoter for Obric was already
//! in tree (originally `arb-scout::dex::obric`); this crate is the canonical
//! home, and adds the missing piece: a native swap-ix builder so the scout's
//! `native_executor` can route through Obric without going through Jupiter
//! (which triggers Jito's "bundles cannot lock any vote accounts" guard via
//! some Pyth/oracle-touching adapter Jupiter inserts).
//!
//! ## Source-of-truth for the swap instruction
//!
//! IDL: <https://github.com/bitquery/solana-idl-lib/blob/main/obric/obric_v2.json>
//! (program name in IDL: `obric_solana`, version `0.1.0`).
//!
//! The IDL defines three variants — `swapXToY`, `swapYToX`, `swap` (generic
//! with `isXToY: bool`). We use the explicit X→Y / Y→X variants because
//! their account-meta lists carry the correct `protocolFeeY` / `protocolFeeX`
//! account directly, avoiding a runtime decision the program would otherwise
//! make from the bool arg.

pub mod quoter;
pub mod pyth;
pub mod swap_ix;

pub use quoter::{
    ObricV2Data, ObricV2Quoter, quote_pmm_swap, quote_via_pyth,
    OBRIC_V2_ACCOUNT_SIZE, OBRIC_V2_DISCRIMINATOR, OBRIC_V2_PROGRAM,
    OBRIC_FEE_DENOMINATOR, is_obric_v2,
};
pub use pyth::{parse_pyth_price, parse_pyth_price_any, parse_pyth_pull_price, PythPrice};
pub use swap_ix::{
    build_swap_x_to_y_ix, build_swap_y_to_x_ix, ObricSwapAccounts,
    swap_x_to_y_discriminator, swap_y_to_x_discriminator,
};

/// Re-export so callers can pass the same `Arc<dyn VaultLookup>` they use
/// for the Raydium AMM v4 quoter.
pub use arb_dex_raydium::VaultLookup;
