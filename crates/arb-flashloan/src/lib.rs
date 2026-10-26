//! Solend (Save Finance) flash-loan instruction builders.
//!
//! Per `INSTRUCTIONS.md` Phase 4, this crate exposes **pure-function**
//! instruction constructors only — no async, no RPC calls, no signing. The
//! orchestrator wires the resulting instructions into a `VersionedTransaction`
//! together with swap and assert instructions.
//!
//! Hard rules from the instructions:
//! 1. `borrow` and `repay` must be in the same transaction.
//! 2. They must be at the top level (Solend rejects flash loans called via CPI).
//! 3. Repay amount = borrow amount + fee, where fee math matches Solend exactly.
//! 4. `RefreshReserve` must precede the borrow.

pub mod solend;
pub mod marginfi;
pub mod kamino;

pub use solend::{
    build_flash_borrow_ix, build_flash_repay_ix, build_refresh_reserve_ix,
    build_usdc_flash_loan_envelope, calculate_flash_loan_fee,
    FlashLoanEnvelope, ReserveState, SolendUsdcContext,
    SOLEND_PROGRAM_ID, TOKEN_PROGRAM_ID,
};
pub use marginfi::{
    build_initialize_account_ix, build_start_flashloan_ix, build_borrow_ix,
    build_deposit_ix, build_end_flashloan_ix, build_liquidate_ix, build_marginfi_flash_loan,
    build_withdraw_ix,
    derive_insurance_vault, derive_liquidity_vault, derive_liquidity_vault_authority,
    discriminator as marginfi_discriminator,
    BankOracle, MarginFiUsdcContext, MarginFiFlashContext, MarginFiLiquidateContext,
    marginfi_bank_for_mint, MarginFiBankInfo,
    MARGINFI_V2_PROGRAM, MARGINFI_MAIN_GROUP, MARGINFI_USDC_BANK, MARGINFI_USDT_BANK,
    MARGINFI_WBTC_BANK, MARGINFI_WSOL_BANK,
    MARGINFI_JUPSOL_BANK, MARGINFI_BSOL_BANK, MARGINFI_JITOSOL_BANK, MARGINFI_MSOL_BANK,
    MARGINFI_CBBTC_BANK, MARGINFI_USD1_BANK, MARGINFI_USDS_BANK, MARGINFI_TBTC_BANK,
    MARGINFI_PYUSD_BANK,
};

pub use kamino::{
    build_refresh_reserve_ix as kamino_build_refresh_reserve_ix,
    build_refresh_obligation_ix as kamino_build_refresh_obligation_ix,
    build_liquidate_obligation_and_redeem_reserve_collateral_ix as kamino_build_liquidate_ix,
    build_liquidate_obligation_and_redeem_reserve_collateral_v2_ix as kamino_build_liquidate_v2_ix,
    derive_lending_market_authority as kamino_derive_lending_market_authority,
    KaminoLiquidateContext, ObligationFarmsAccounts as KaminoObligationFarmsAccounts,
    KAMINO_LEND_PROGRAM, KAMINO_MAIN_MARKET, KAMINO_FARMS_PROGRAM, LENDING_MARKET_AUTH_SEED,
};
