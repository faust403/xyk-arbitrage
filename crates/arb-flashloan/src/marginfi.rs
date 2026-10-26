//! MarginFi v2 flash-loan instruction builders.
//!
//! Why MarginFi (vs Solend):
//! - Solend charges 9 bps per flash loan — kills any SOL/USDC arb under
//!   ~10 bps spread (= almost all real spreads).
//! - MarginFi v2's `lending_account_start_flashloan` / `_end_flashloan`
//!   pair runs the borrow + arbitrary user instructions + deposit
//!   atomically; the only fee chargeable is the per-bank
//!   `protocol_origination_fee` on the inner `lending_account_borrow`.
//!   For the USDC bank on mainnet, that fee is ~0 (probe-confirmed
//!   2026-05-10 — see `memory/project_marginfi_flashloan_plan.md`).
//!
//! ## Tx layout for an arbitrage cycle
//!
//! ```text
//! ix[N+0]  start_flashloan { end_index: u64 }
//! ix[N+1]  lending_account_borrow { amount: u64 }      ← USDC into our ATA
//! ix[N+2…] swap leg 1, swap leg 2, …                   ← user instructions
//! ix[N+k]  lending_account_deposit { amount }          ← repay
//! ix[N+k+1] end_flashloan                              ← MUST be at end_index
//! ```
//!
//! `start.end_index` is the **absolute index of the end ix in the tx**, NOT
//! relative to the start. Caller must compute it after laying out all ixs.
//!
//! ## Source-of-truth caveat
//!
//! Account orders + data layouts captured from MarginFi v2 source on
//! 2026-05-10 (`mrgnlabs/marginfi-v2` main branch). Re-verify against
//! HEAD before mainnet use; the program is upgradeable and recent
//! versions added/reordered fields. Pre-flight `simulateTransaction`
//! catches any mismatch before any tip is paid (same safety net we use
//! for the Solend path).

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};

use crate::TOKEN_PROGRAM_ID;

/// MarginFi v2 program id (mainnet).
pub const MARGINFI_V2_PROGRAM: Pubkey = solana_program::pubkey!(
    "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA"
);

/// MarginFi v2 main lending group on mainnet. Verified 2026-05-10 by
/// reading the USDC bank's stored `group` field at offset 41 (after
/// mint+mint_decimals). **Last char is `8`, not `7`** — earlier sessions
/// had a typo'd `AG7` placeholder copied from outdated docs.
pub const MARGINFI_MAIN_GROUP: Pubkey = solana_program::pubkey!(
    "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8"
);

/// USDC bank on the main group. Probe-confirmed 2026-05-10 to be a
/// MarginFi v2-owned `Bank` account whose `mint` field equals USDC.
pub const MARGINFI_USDC_BANK: Pubkey = solana_program::pubkey!(
    "2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB"
);

/// USDT bank on the main group. Same flash-loan envelope as USDC; mint
/// differs (Es9vMFrz…). Operators MUST verify `MARGINFI_USDT_LIQUIDITY_VAULT`
/// env var matches this bank's on-chain liquidity_vault PDA — they're
/// per-bank and not derivable from constants alone.
pub const MARGINFI_USDT_BANK: Pubkey = solana_program::pubkey!(
    "HmpMfL8942u22htC4EMiWgLX931g3sacXFR6KjuLgKLV"
);

/// WBTC (Wormhole) bank on the main group. Mint = `3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh`.
/// Pyth oracle = `4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo`. WBTC has 8 decimals;
/// callers MUST compute borrow amounts in 1e8 units, not 1e6. Verified 2026-05-13
/// by reading MarginFi v2 program accounts.
pub const MARGINFI_WBTC_BANK: Pubkey = solana_program::pubkey!(
    "BKsfDJCMbYep6gr9pq8PsmJbb5XGLHbAJzUV8vmorz7a"
);

/// Wrapped-SOL bank on the main group. Mint = `So11111111111111111111111111111111111111112`.
/// Pyth oracle (V1) = `H6ARHf6YXhGYeQfUzQNGk6rDNnLBQKrenN712K4AQJEG`. SOL has 9 decimals.
/// Used by kamino-hunter to flash-borrow wSOL when liquidating SOL-debt positions
/// (the biggest pool of liquidatable exposure on Kamino — ~$44M observed live).
pub const MARGINFI_WSOL_BANK: Pubkey = solana_program::pubkey!(
    "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAntD6LYGh"
);

// LST + exotic-mint banks discovered live 2026-05-16 by scanning all
// MarginFi v2 banks (dataSize=1864) and matching mints we saw firing in
// kamino-hunter. All on the SAME main group as USDC/USDT/WBTC/wSOL, so
// the existing `marginfi_account` PDA works without re-init.
pub const MARGINFI_JUPSOL_BANK:  Pubkey = solana_program::pubkey!("8LaUZadNqtzuCG7iCvZd7d5cbquuYfv19KjAg6GPuuCb");
pub const MARGINFI_BSOL_BANK:    Pubkey = solana_program::pubkey!("6hS9i46WyTq1KXcoa2Chas2Txh9TJAVr6n1t3tnrE23K");
pub const MARGINFI_JITOSOL_BANK: Pubkey = solana_program::pubkey!("Bohoc1ikHLD7xKJuzTyiTyCwzaL5N7ggJQu75A8mKYM8");
pub const MARGINFI_MSOL_BANK:    Pubkey = solana_program::pubkey!("22DcjMZrMwC5Bpa5AGBsmjc5V9VuQrXG6N9ZtdUNyYGE");
pub const MARGINFI_CBBTC_BANK:   Pubkey = solana_program::pubkey!("Ac4KV5K5isDqtABtg6h5DiwzZMe3Sp9bc3pBiCUvUpaQ");
pub const MARGINFI_USD1_BANK:    Pubkey = solana_program::pubkey!("8efP4VoKDo3SqxoVCUcgxpN9S7boDWtPmeFwahRZ4ukg");
pub const MARGINFI_USDS_BANK:    Pubkey = solana_program::pubkey!("4VgNUPbdTNJ6a8NcLZDrjqtxpRk1fK7NDpMBSmvGVwHn");
pub const MARGINFI_TBTC_BANK:    Pubkey = solana_program::pubkey!("AyeEyCiBU2CzZmxz3f3o3vk2hvMsM5G3t3D6vhLsiFkf");
pub const MARGINFI_PYUSD_BANK:   Pubkey = solana_program::pubkey!("8UEiPmgZHXXEDrqLS3oiTxQxTbeYTtPbeMBxAd2XGbpu");

// ─── Discriminators ────────────────────────────────────────────────
//
// Anchor convention: 8-byte sha256 prefix of `global:{snake_case_name}`.
// We hard-code them here so the builders don't pull in a SHA-256 dep
// and don't pay the hash cost at runtime. Verify these once against the
// MarginFi IDL or a real on-chain tx via `bin/marginfi-trace` (TBD).

/// `global:marginfi_account_initialize`
/// Hash precomputed: sha256("global:marginfi_account_initialize")[..8]
/// TODO: verify against real tx — preliminary estimate; can be derived
/// at runtime via `solana_program::hash::hash` if needed.
pub const DISC_INIT_ACCOUNT: [u8; 8] = compute_disc_static("global:marginfi_account_initialize");

/// `global:lending_account_start_flashloan`
pub const DISC_START_FLASHLOAN: [u8; 8] = compute_disc_static("global:lending_account_start_flashloan");

/// `global:lending_account_borrow`
pub const DISC_BORROW: [u8; 8] = compute_disc_static("global:lending_account_borrow");

/// `global:lending_account_deposit`
pub const DISC_DEPOSIT: [u8; 8] = compute_disc_static("global:lending_account_deposit");

/// `global:lending_account_end_flashloan`
pub const DISC_END_FLASHLOAN: [u8; 8] = compute_disc_static("global:lending_account_end_flashloan");

/// `global:lending_account_liquidate` — used by the atomic Pyth-paired
/// MarginFi liquidator. Args are `asset_amount: u64` only on canonical
/// IDL; some forks add `liquidatee_accounts: u8, liquidator_accounts: u8`
/// trailing markers. Builder below emits the canonical form — verify
/// against the deployed program via `simulateTransaction` before tipping.
pub const DISC_LIQUIDATE: [u8; 8] = compute_disc_static("global:lending_account_liquidate");

/// `global:lending_account_withdraw` — pulls collateral tokens out of
/// our marginfi_account to a destination ATA. Needed after liquidate so
/// we can swap the seized asset to the repay currency via Jupiter.
pub const DISC_WITHDRAW: [u8; 8] = compute_disc_static("global:lending_account_withdraw");

// `compute_disc_static` placeholder — Rust can't run sha256 in const.
// Real bytes get filled by the runtime helper below; the constants above
// are placeholders to keep the API shape stable. Callers should use
// `discriminator(name)` until we ship a build.rs.
const fn compute_disc_static(_name: &'static str) -> [u8; 8] { [0; 8] }

/// Compute Anchor's 8-byte instruction discriminator from `global:{name}`.
/// Use this at runtime to fill in the constants above; cache the result
/// in a `OnceLock` if called on a hot path (it's ~200ns).
pub fn discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("global:{name}");
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

// ─── PDA derivations ────────────────────────────────────────────────

/// Bank's `liquidity_vault_authority` PDA. Seed string verified against
/// MarginFi v2 source `programs/marginfi/src/state/marginfi_account.rs`.
///
/// **TODO:** confirm the literal seed string matches MarginFi's source.
/// Common candidates: `b"liquidity_vault_auth"` or `b"liquidity_vault_authority"`.
/// If wrong the PDA mismatches and the borrow ix fails — caught by
/// pre-flight sim before tip is spent.
pub fn derive_liquidity_vault_authority(bank: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"liquidity_vault_auth", bank.as_ref()],
        &MARGINFI_V2_PROGRAM,
    ).0
}

/// Bank's `liquidity_vault` PDA (the SPL token account holding the bank's
/// liquidity). Seeds = `["liquidity_vault", bank]`. Verified for USDC and
/// USDT banks via `marginfi-probe` (2026-05-13) — same seed works for all
/// banks on the main group.
pub fn derive_liquidity_vault(bank: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"liquidity_vault", bank.as_ref()],
        &MARGINFI_V2_PROGRAM,
    ).0
}

/// Bank's `insurance_vault` PDA (per-bank SPL account that absorbs
/// liquidation premium). Seeds = `["insurance_vault", bank]` per MarginFi
/// v2 source. Verify per-bank on-chain before mainnet liquidations — if
/// wrong the liquidate ix fails account constraint checks; pre-flight sim
/// catches it.
pub fn derive_insurance_vault(bank: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"insurance_vault", bank.as_ref()],
        &MARGINFI_V2_PROGRAM,
    ).0
}

// ─── Bank registry (mint → bank info) ─────────────────────────────

/// All four well-known MarginFi v2 banks the bot may flash-borrow from.
/// Used by kamino-hunter to route an "any-mint debt liquidation" through
/// the appropriate bank's flash envelope.
#[derive(Debug, Clone, Copy)]
pub struct MarginFiBankInfo {
    pub mint:           Pubkey,
    pub bank:           Pubkey,
    pub pyth_oracle:    Pubkey,
    pub decimals:       u8,
}

const MINT_USDC: Pubkey = solana_program::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
const MINT_USDT: Pubkey = solana_program::pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
const MINT_WBTC: Pubkey = solana_program::pubkey!("3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh");
const MINT_WSOL: Pubkey = solana_program::pubkey!("So11111111111111111111111111111111111111112");
// LST + exotic mints. All banks are on the same main group as USDC, so
// the wallet's existing marginfi_account works for them too.
const MINT_JUPSOL:  Pubkey = solana_program::pubkey!("jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v");
const MINT_BSOL:    Pubkey = solana_program::pubkey!("bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1");
const MINT_JITOSOL: Pubkey = solana_program::pubkey!("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn");
const MINT_MSOL:    Pubkey = solana_program::pubkey!("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So");
const MINT_CBBTC:   Pubkey = solana_program::pubkey!("cbbtcf3aa214zXHbiAZQwf4122FBYbraNdFqgw4iMij");
const MINT_USD1:    Pubkey = solana_program::pubkey!("USD1ttGY1N17NEEHLmELoaybftRBUSErhqYiQzvEmuB");
const MINT_USDS:    Pubkey = solana_program::pubkey!("USDSwr9ApdHk5bvJKMjzff41FfuX8bSxdKcR81vTwcA");
const MINT_TBTC:    Pubkey = solana_program::pubkey!("6DNSN2BJsaPFdFFc1zP37kkeNe4Usc1Sqkzr9C9vPWcU");
const MINT_PYUSD:   Pubkey = solana_program::pubkey!("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo");

// Each bank's `primary_oracle` (= config.oracle_keys[0]) probed live
// 2026-05-16. The banks use setup=7 (Pyth Pull / PriceUpdateV2) — the
// end_flashloan health-check matches the bytes exactly, so substituting
// "the right Pyth feed" for the same asset still revs with InvalidOracle.
const PYTH_USDC:    Pubkey = solana_program::pubkey!("2edzBDt34Ch9SvxCVcFyUhVjC63ngnkSEAeYcRVe23dR");
const PYTH_USDT:    Pubkey = solana_program::pubkey!("8NXsAvnQWrqhUuBZHg1Tyee2QMWLXun7YLZMySemtSBZ");
const PYTH_WBTC:    Pubkey = solana_program::pubkey!("ybTfQe6xqLU6a1Z6GjtqkqWLreeox61weq6kGQ1r8Wj");
const PYTH_WSOL:    Pubkey = solana_program::pubkey!("6mbHvEwwvozvEF6Ni6jPyESXQV75KooJWKUzpKzSfetX");
const PYTH_JUPSOL:  Pubkey = solana_program::pubkey!("89tHGNso9C5m5rzjiyYFJm6jaCVBAHwVVibEzueKSyVZ");
const PYTH_BSOL:    Pubkey = solana_program::pubkey!("5tGP8vYVwWpmzZr5ZZqaYiVxFW3EzgZZhUbPwmUTEuPm");
const PYTH_JITOSOL: Pubkey = solana_program::pubkey!("E8Jx6Jz7fZC6dsvrbqCjokSXhYEkEKeNjWoK94dQq8yd");
const PYTH_MSOL:    Pubkey = solana_program::pubkey!("9ZTiPX9b8qN98cmEXAd3iP1TB1upq2Ts7nkmbJDLgbvo");
const PYTH_CBBTC:   Pubkey = PYTH_WBTC;  // same composite BTC oracle shared with WBTC
const PYTH_USD1:    Pubkey = solana_program::pubkey!("drCboW4mD1B16n6g9PepjXyxC9UrDhFVpHZ6QHnF4L7");
const PYTH_USDS:    Pubkey = solana_program::pubkey!("3edxkZkg9vHvkiSNcx7Cn2qgNuY7hAh1KJjPJMR1BjFX");
const PYTH_TBTC:    Pubkey = PYTH_WBTC;  // same composite BTC oracle
const PYTH_PYUSD:   Pubkey = solana_program::pubkey!("8ohM44k6rV3cXFTBk9HDS6JyUgs4oXb6XEn31LVRTyBD");

/// Return bank info for a given debt-mint, or None if MarginFi has no
/// bank for it (or we haven't whitelisted it yet). Used by kamino-hunter
/// when repay_mint != USDC: we need to flash-borrow that mint to repay
/// the Kamino debt before liquidating.
pub fn marginfi_bank_for_mint(mint: &Pubkey) -> Option<MarginFiBankInfo> {
    macro_rules! b {
        ($mint:expr, $bank:expr, $oracle:expr, $dec:expr) => {
            Some(MarginFiBankInfo { mint: $mint, bank: $bank, pyth_oracle: $oracle, decimals: $dec })
        };
    }
    if *mint == MINT_USDC    { return b!(MINT_USDC,    MARGINFI_USDC_BANK,    PYTH_USDC,    6); }
    if *mint == MINT_USDT    { return b!(MINT_USDT,    MARGINFI_USDT_BANK,    PYTH_USDT,    6); }
    if *mint == MINT_WBTC    { return b!(MINT_WBTC,    MARGINFI_WBTC_BANK,    PYTH_WBTC,    8); }
    if *mint == MINT_WSOL    { return b!(MINT_WSOL,    MARGINFI_WSOL_BANK,    PYTH_WSOL,    9); }
    if *mint == MINT_JUPSOL  { return b!(MINT_JUPSOL,  MARGINFI_JUPSOL_BANK,  PYTH_JUPSOL,  9); }
    if *mint == MINT_BSOL    { return b!(MINT_BSOL,    MARGINFI_BSOL_BANK,    PYTH_BSOL,    9); }
    if *mint == MINT_JITOSOL { return b!(MINT_JITOSOL, MARGINFI_JITOSOL_BANK, PYTH_JITOSOL, 9); }
    if *mint == MINT_MSOL    { return b!(MINT_MSOL,    MARGINFI_MSOL_BANK,    PYTH_MSOL,    9); }
    if *mint == MINT_CBBTC   { return b!(MINT_CBBTC,   MARGINFI_CBBTC_BANK,   PYTH_CBBTC,   8); }
    if *mint == MINT_USD1    { return b!(MINT_USD1,    MARGINFI_USD1_BANK,    PYTH_USD1,    6); }
    if *mint == MINT_USDS    { return b!(MINT_USDS,    MARGINFI_USDS_BANK,    PYTH_USDS,    6); }
    if *mint == MINT_TBTC    { return b!(MINT_TBTC,    MARGINFI_TBTC_BANK,    PYTH_TBTC,    8); }
    if *mint == MINT_PYUSD   { return b!(MINT_PYUSD,   MARGINFI_PYUSD_BANK,   PYTH_PYUSD,   6); }
    None
}

// ─── Bundle of pubkeys + builders ──────────────────────────────────

/// Everything we need to build a USDC flash-loan envelope on MarginFi v2.
/// `marginfi_account` must already exist on chain — callers create it
/// once via [`build_initialize_account_ix`] before the first flash loan.
#[derive(Debug, Clone, Copy)]
pub struct MarginFiUsdcContext {
    pub group:                       Pubkey,
    pub bank:                        Pubkey,
    pub liquidity_vault:             Pubkey,
    pub liquidity_vault_authority:   Pubkey,
}

impl MarginFiUsdcContext {
    /// Build a context for the canonical USDC bank on the main group.
    /// Callers may override fields if MarginFi migrates.
    pub fn default_mainnet(liquidity_vault: Pubkey) -> Self {
        Self {
            group: MARGINFI_MAIN_GROUP,
            bank:  MARGINFI_USDC_BANK,
            liquidity_vault,
            liquidity_vault_authority: derive_liquidity_vault_authority(&MARGINFI_USDC_BANK),
        }
    }
}

/// Initialise a fresh `MarginfiAccount`. One-time setup; the resulting
/// account is reused for every subsequent flash loan from this wallet.
///
/// Account order (verified 2026-05-10):
/// ```text
/// 0  [R]    marginfi_group
/// 1  [W]    marginfi_account (init — fresh keypair)
/// 2  [R,S]  authority
/// 3  [W,S]  fee_payer
/// 4  [R]    system_program
/// ```
pub fn build_initialize_account_ix(
    group:            Pubkey,
    marginfi_account: Pubkey,  // fresh keypair (caller signs as it too)
    authority:        Pubkey,
    fee_payer:        Pubkey,
) -> Instruction {
    let data = discriminator("marginfi_account_initialize").to_vec();
    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(group, false),
            AccountMeta::new(marginfi_account, true),         // signer (init)
            AccountMeta::new_readonly(authority, true),       // signer
            AccountMeta::new(fee_payer, true),                // writable + signer
            AccountMeta::new_readonly(solana_program::system_program::id(), false),
        ],
        data,
    }
}

/// `lending_account_start_flashloan`. `end_index` is the **absolute
/// index of the matching end_flashloan ix within the same tx** — caller
/// computes after laying out all instructions.
///
/// Account order:
/// ```text
/// 0  [W]    marginfi_account
/// 1  [R,S]  authority
/// 2  [R]    instructions sysvar
/// ```
pub fn build_start_flashloan_ix(
    marginfi_account: Pubkey,
    authority:        Pubkey,
    end_index:        u64,
) -> Instruction {
    let mut data = discriminator("lending_account_start_flashloan").to_vec();
    data.extend_from_slice(&end_index.to_le_bytes());
    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts: vec![
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
        ],
        data,
    }
}

/// `lending_account_borrow`.
///
/// Account order:
/// ```text
/// 0  [R]    group
/// 1  [W]    marginfi_account
/// 2  [R,S]  authority
/// 3  [W]    bank
/// 4  [W]    destination_token_account (caller's USDC ATA)
/// 5  [R]    bank_liquidity_vault_authority (PDA)
/// 6  [W]    liquidity_vault
/// 7  [R]    token_program
/// ```
pub fn build_borrow_ix(
    ctx:              &MarginFiUsdcContext,
    marginfi_account: Pubkey,
    authority:        Pubkey,
    destination_ata:  Pubkey,
    amount:           u64,
) -> Instruction {
    let mut data = discriminator("lending_account_borrow").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(ctx.group, false),
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(ctx.bank, false),
            AccountMeta::new(destination_ata, false),
            AccountMeta::new_readonly(ctx.liquidity_vault_authority, false),
            AccountMeta::new(ctx.liquidity_vault, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `lending_account_deposit` — used as the "repay" leg of the flash loan.
///
/// Data layout: `amount: u64` followed by `Option<bool>` for
/// `deposit_up_to_limit`. We pass `None` (= 0x00 byte) since we always
/// know the exact repay amount.
///
/// Account order:
/// ```text
/// 0  [R]    group
/// 1  [W]    marginfi_account
/// 2  [R,S]  authority
/// 3  [W]    bank
/// 4  [W]    signer_token_account (caller's USDC ATA, same as borrow's dest)
/// 5  [W]    liquidity_vault
/// 6  [R]    token_program
/// ```
pub fn build_deposit_ix(
    ctx:              &MarginFiUsdcContext,
    marginfi_account: Pubkey,
    authority:        Pubkey,
    source_ata:       Pubkey,
    amount:           u64,
) -> Instruction {
    let mut data = discriminator("lending_account_deposit").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(0); // Option<bool>::None
    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(ctx.group, false),
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(ctx.bank, false),
            AccountMeta::new(source_ata, false),
            AccountMeta::new(ctx.liquidity_vault, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `lending_account_repay` — closes an outstanding borrow on the same
/// bank. Used as the post-swap leg of a flash loan, replacing `deposit`
/// for accounts that hit `OperationDepositOnly` (6019) because their
/// state machine rejects deposit-as-repay during a flashloan envelope.
///
/// Data layout: `amount: u64` followed by `Option<bool>` for `repay_all`.
/// We pass `None` (= 0x00 byte) and an exact amount — caller passes the
/// borrowed_amount + flash_fee.
///
/// Account order: same as deposit. MarginFi v2 dispatches the two via
/// the discriminator alone (`lending_account_repay` vs
/// `lending_account_deposit`); the account list shape is identical.
pub fn build_repay_ix(
    ctx:              &MarginFiUsdcContext,
    marginfi_account: Pubkey,
    authority:        Pubkey,
    source_ata:       Pubkey,
    amount:           u64,
) -> Instruction {
    let mut data = discriminator("lending_account_repay").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(0); // Option<bool>::None for repay_all
    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(ctx.group, false),
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(ctx.bank, false),
            AccountMeta::new(source_ata, false),
            AccountMeta::new(ctx.liquidity_vault, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `lending_account_end_flashloan`. Closes the flash loan and runs
/// post-borrow health checks. Caller must pass the same (bank, oracle)
/// pairs as `remaining_accounts` that the marginfi_account has positions
/// in — for a USDC-only flash loan, that's `(USDC_BANK, USDC_PYTH)`.
pub fn build_end_flashloan_ix(
    marginfi_account: Pubkey,
    authority:        Pubkey,
    bank_oracle_pairs: &[(Pubkey, Pubkey)],
) -> Instruction {
    let data = discriminator("lending_account_end_flashloan").to_vec();
    let mut accounts = vec![
        AccountMeta::new(marginfi_account, false),
        AccountMeta::new_readonly(authority, true),
    ];
    for (bank, oracle) in bank_oracle_pairs {
        accounts.push(AccountMeta::new_readonly(*bank, false));
        accounts.push(AccountMeta::new_readonly(*oracle, false));
    }
    Instruction { program_id: MARGINFI_V2_PROGRAM, accounts, data }
}

/// `lending_account_withdraw` — pulls `amount` of `bank`'s underlying
/// token out of `marginfi_account` into `destination_ata`. Same 8-fixed-
/// account layout as `borrow`; differs only in data (adds `withdraw_all
/// : Option<bool>` after the amount).
///
/// `withdraw_all`:
/// - `Some(true)` → ignore `amount`, pull entire position; useful when
///   we've just liquidated and don't know the exact share-value yet.
/// - `Some(false)` → keep position, pull exactly `amount` tokens.
/// - `None` → same as `Some(false)`.
///
/// Per source, remaining_accounts are bank-rate-limiter oracle keys
/// (optional). We pass none for now; if the post-deploy sim fails with
/// a rate-limit error, callers append the bank's primary oracle as a
/// readonly remaining account.
pub fn build_withdraw_ix(
    group:            Pubkey,
    marginfi_account: Pubkey,
    authority:        Pubkey,
    bank:             Pubkey,
    destination_ata:  Pubkey,
    bank_liquidity_vault_authority: Pubkey,
    liquidity_vault:  Pubkey,
    amount:           u64,
    withdraw_all:     Option<bool>,
) -> Instruction {
    let mut data = discriminator("lending_account_withdraw").to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    match withdraw_all {
        Some(b) => { data.push(1); data.push(if b { 1 } else { 0 }); }
        None    => { data.push(0); }
    }
    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts: vec![
            AccountMeta::new_readonly(group, false),
            AccountMeta::new(marginfi_account, false),
            AccountMeta::new_readonly(authority, true),
            AccountMeta::new(bank, false),
            AccountMeta::new(destination_ata, false),
            AccountMeta::new_readonly(bank_liquidity_vault_authority, false),
            AccountMeta::new(liquidity_vault, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

/// Bank + Pyth oracle pair used as a `remaining_account` observation in
/// the MarginFi liquidate IX. The program walks every open position on
/// both liquidator and liquidatee accounts and expects matching pairs.
#[derive(Debug, Clone, Copy)]
pub struct BankOracle {
    pub bank:   Pubkey,
    pub oracle: Pubkey,
}

/// Pubkey-bundle for `lending_account_liquidate`.
#[derive(Debug, Clone, Copy)]
pub struct MarginFiLiquidateContext {
    pub group:                       Pubkey,
    pub asset_bank:                  Pubkey,  // collateral being seized
    pub liab_bank:                   Pubkey,  // liability being repaid
    pub liquidator_marginfi_account: Pubkey,  // OUR marginfi_account
    pub liquidatee_marginfi_account: Pubkey,  // target (underwater) account
    /// PDA derived from `liab_bank` — `liab` bank owns the vaults used for
    /// the actual token transfer during liquidation.
    pub liab_liquidity_vault_authority: Pubkey,
    pub liab_liquidity_vault:           Pubkey,
    pub liab_insurance_vault:           Pubkey,
}

impl MarginFiLiquidateContext {
    /// Convenience: derive all `liab_*` vault PDAs from `liab_bank` and
    /// fill the rest from caller-provided inputs. Vault seeds are the
    /// same MarginFi v2 convention we use for the flash-loan path.
    pub fn from_banks(
        group:                       Pubkey,
        asset_bank:                  Pubkey,
        liab_bank:                   Pubkey,
        liquidator_marginfi_account: Pubkey,
        liquidatee_marginfi_account: Pubkey,
    ) -> Self {
        Self {
            group,
            asset_bank,
            liab_bank,
            liquidator_marginfi_account,
            liquidatee_marginfi_account,
            liab_liquidity_vault_authority: derive_liquidity_vault_authority(&liab_bank),
            liab_liquidity_vault:           derive_liquidity_vault(&liab_bank),
            liab_insurance_vault:           derive_insurance_vault(&liab_bank),
        }
    }
}

/// Build `lending_account_liquidate`.
///
/// Account order (verified against MarginFi v2 source 2026-05-15):
/// ```text
///  0  [R]    group
///  1  [W]    asset_bank
///  2  [W]    liab_bank
///  3  [W]    liquidator_marginfi_account
///  4  [R,S]  authority                     (= liquidator wallet)
///  5  [W]    liquidatee_marginfi_account
///  6  [R]    liab_bank_liquidity_vault_authority
///  7  [W]    liab_bank_liquidity_vault
///  8  [W]    liab_bank_insurance_vault
///  9  [R]    token_program
/// 10..       remaining accounts: [asset_oracle, liab_oracle,
///             liquidator_observations..., liquidatee_observations...]
/// ```
///
/// `observation` pairs are `(bank, oracle)` for every position the
/// account holds. The MarginFi program walks them to re-price all
/// positions during health re-check.
///
/// Data layout (verified 2026-05-15 against deployed v2 program via
/// Anchor error 102 InstructionDidNotDeserialize on the disc-only form):
/// `disc(8) || asset_amount: u64 || liquidatee_accounts: u8 || liquidator_accounts: u8`.
/// The trailing two `u8` fields tell the program how to split the
/// trailing `remaining_accounts` between liquidator-observation pairs
/// (each pair = 2 accounts: bank + oracle) and liquidatee-observation
/// pairs. Both are counts of *accounts*, not pairs — so for 1 pair,
/// the count is 2.
pub fn build_liquidate_ix(
    ctx:                    &MarginFiLiquidateContext,
    authority:              Pubkey,
    asset_amount:           u64,
    asset_oracle:           Pubkey,
    liab_oracle:            Pubkey,
    liquidator_observations: &[BankOracle],
    liquidatee_observations: &[BankOracle],
) -> Instruction {
    let mut data = discriminator("lending_account_liquidate").to_vec();
    data.extend_from_slice(&asset_amount.to_le_bytes());
    // Trailing u8 counts: number of trailing remaining_accounts (NOT pairs)
    // that belong to liquidatee, then to liquidator. Pre-asset/liab oracle
    // remaining account count = 2 (the two oracles) but those don't count
    // here — these counts cover only the observation tail.
    data.push((liquidatee_observations.len() * 2) as u8);
    data.push((liquidator_observations.len() * 2) as u8);

    let mut accounts = vec![
        AccountMeta::new_readonly(ctx.group, false),
        AccountMeta::new(ctx.asset_bank, false),
        AccountMeta::new(ctx.liab_bank, false),
        AccountMeta::new(ctx.liquidator_marginfi_account, false),
        AccountMeta::new_readonly(authority, true),
        AccountMeta::new(ctx.liquidatee_marginfi_account, false),
        AccountMeta::new_readonly(ctx.liab_liquidity_vault_authority, false),
        AccountMeta::new(ctx.liab_liquidity_vault, false),
        AccountMeta::new(ctx.liab_insurance_vault, false),
        AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
    ];

    // Remaining: asset_oracle, liab_oracle, then per-side observations.
    // All marked readonly — oracles + banks are read by the health check.
    accounts.push(AccountMeta::new_readonly(asset_oracle, false));
    accounts.push(AccountMeta::new_readonly(liab_oracle,  false));
    for obs in liquidator_observations {
        accounts.push(AccountMeta::new_readonly(obs.bank,   false));
        accounts.push(AccountMeta::new_readonly(obs.oracle, false));
    }
    for obs in liquidatee_observations {
        accounts.push(AccountMeta::new_readonly(obs.bank,   false));
        accounts.push(AccountMeta::new_readonly(obs.oracle, false));
    }

    Instruction {
        program_id: MARGINFI_V2_PROGRAM,
        accounts,
        data,
    }
}

/// All addresses the bot needs to wrap a swap cycle in a MarginFi flash
/// loan. The `marginfi_account` is the per-wallet PDA created once via
/// `build_initialize_account_ix` and reused thereafter.
#[derive(Debug, Clone, Copy)]
pub struct MarginFiFlashContext {
    pub group:                     Pubkey,
    pub bank:                      Pubkey,
    pub liquidity_vault:           Pubkey,
    pub liquidity_vault_authority: Pubkey,
    pub usdc_pyth_oracle:          Pubkey,  // for the end_flashloan health check
    pub marginfi_account:          Pubkey,
}

/// Build the four-instruction flash-loan envelope. `end_index` MUST be
/// the absolute index of `end_flashloan` in the final tx — caller
/// computes it after laying out compute-budget / tip / ata / swap ixs.
///
/// Returns `(pre_swap_ixs, post_swap_ixs)`. Insert the pre-swap ixs
/// before the swap legs and the post-swap ixs after; `end_index` should
/// equal `len(prefix) + len(pre_swap_ixs) + len(swap_legs) + len(post_swap_ixs) - 1`.
pub fn build_marginfi_flash_loan(
    ctx:       &MarginFiFlashContext,
    authority: Pubkey,
    user_ata:  Pubkey,
    amount:    u64,
    end_index: u64,
) -> (Vec<Instruction>, Vec<Instruction>) {
    let pre = vec![
        build_start_flashloan_ix(ctx.marginfi_account, authority, end_index),
        build_borrow_ix(
            &MarginFiUsdcContext {
                group: ctx.group,
                bank: ctx.bank,
                liquidity_vault: ctx.liquidity_vault,
                liquidity_vault_authority: ctx.liquidity_vault_authority,
            },
            ctx.marginfi_account, authority, user_ata, amount,
        ),
    ];
    let post = vec![
        // BUGFIX 2026-05-15: was build_deposit_ix which reverts with
        // AnchorError::OperationDepositOnly (6019) when the bank's state
        // machine refuses to treat deposit-as-repay during a flashloan
        // envelope (observed live in sandwich-runner sim 20:42 UTC).
        // `lending_account_repay` is the dedicated repay IX; same accounts,
        // different discriminator, semantic = "settle outstanding borrow".
        build_repay_ix(
            &MarginFiUsdcContext {
                group: ctx.group,
                bank: ctx.bank,
                liquidity_vault: ctx.liquidity_vault,
                liquidity_vault_authority: ctx.liquidity_vault_authority,
            },
            ctx.marginfi_account, authority, user_ata, amount,
        ),
        build_end_flashloan_ix(
            ctx.marginfi_account, authority,
            &[(ctx.bank, ctx.usdc_pyth_oracle)],
        ),
    ];
    (pre, post)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }

    #[test]
    fn discriminators_are_distinct() {
        let init  = discriminator("marginfi_account_initialize");
        let start = discriminator("lending_account_start_flashloan");
        let borrow = discriminator("lending_account_borrow");
        let deposit = discriminator("lending_account_deposit");
        let end   = discriminator("lending_account_end_flashloan");
        let all = [init, start, borrow, deposit, end];
        for i in 0..all.len() {
            for j in i + 1..all.len() {
                assert_ne!(all[i], all[j], "disc collision i={i} j={j}");
            }
        }
    }

    #[test]
    fn start_flashloan_data_has_end_index() {
        let ix = build_start_flashloan_ix(pk(1), pk(2), 9);
        assert_eq!(ix.data.len(), 8 + 8); // disc + u64
        // First 8 bytes are the disc, next 8 are end_index LE.
        let end = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(end, 9);
    }

    #[test]
    fn borrow_ix_account_count_is_8() {
        let ctx = MarginFiUsdcContext {
            group: pk(1), bank: pk(2),
            liquidity_vault: pk(3), liquidity_vault_authority: pk(4),
        };
        let ix = build_borrow_ix(&ctx, pk(5), pk(6), pk(7), 100);
        assert_eq!(ix.accounts.len(), 8);
        // amount in data
        let amt = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(amt, 100);
    }

    #[test]
    fn deposit_ix_data_includes_none_option() {
        let ctx = MarginFiUsdcContext {
            group: pk(1), bank: pk(2),
            liquidity_vault: pk(3), liquidity_vault_authority: pk(4),
        };
        let ix = build_deposit_ix(&ctx, pk(5), pk(6), pk(7), 100);
        // disc(8) + amount(8) + Option<bool>::None(1) = 17 bytes
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.data[16], 0); // None tag
    }

    #[test]
    fn end_flashloan_passes_remaining_accounts() {
        let pairs = [(pk(10), pk(11))];
        let ix = build_end_flashloan_ix(pk(1), pk(2), &pairs);
        // 2 fixed + 2 per pair = 4 accounts.
        assert_eq!(ix.accounts.len(), 4);
        assert_eq!(ix.accounts[2].pubkey, pk(10));
        assert_eq!(ix.accounts[3].pubkey, pk(11));
    }

    #[test]
    fn liquidity_vault_authority_pda_is_deterministic() {
        let bank = pk(99);
        let a = derive_liquidity_vault_authority(&bank);
        let b = derive_liquidity_vault_authority(&bank);
        assert_eq!(a, b);
    }

    #[test]
    fn known_mainnet_addresses_parse() {
        // Smoke test that the hard-coded constants parse as Pubkeys.
        let _ = MARGINFI_V2_PROGRAM;
        let _ = MARGINFI_MAIN_GROUP;
        let _ = MARGINFI_USDC_BANK;
    }

    #[test]
    fn liquidate_ix_account_count_minimal() {
        // No observation pairs: 10 fixed + 2 oracle = 12 accounts.
        let ctx = MarginFiLiquidateContext::from_banks(pk(1), pk(2), pk(3), pk(4), pk(5));
        let ix = build_liquidate_ix(&ctx, pk(6), 1_000_000, pk(7), pk(8), &[], &[]);
        assert_eq!(ix.accounts.len(), 12);
        // disc(8) + asset_amount(8) + 2*u8 = 18 bytes
        assert_eq!(ix.data.len(), 18);
        let amt = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(amt, 1_000_000);
        assert_eq!(ix.data[16], 0);  // liquidatee_accounts (0 obs × 2)
        assert_eq!(ix.data[17], 0);  // liquidator_accounts (0 obs × 2)
    }

    #[test]
    fn liquidate_ix_includes_observation_remaining_accounts() {
        let ctx = MarginFiLiquidateContext::from_banks(pk(1), pk(2), pk(3), pk(4), pk(5));
        let liq_obs = [BankOracle { bank: pk(20), oracle: pk(21) }];
        let liee_obs = [
            BankOracle { bank: pk(30), oracle: pk(31) },
            BankOracle { bank: pk(32), oracle: pk(33) },
        ];
        let ix = build_liquidate_ix(&ctx, pk(6), 1, pk(7), pk(8), &liq_obs, &liee_obs);
        // 10 fixed + 2 oracle + 2 (liq) + 4 (liee) = 18
        assert_eq!(ix.accounts.len(), 18);
        // liquidator obs come first, then liquidatee — verify by tail layout.
        assert_eq!(ix.accounts[12].pubkey, pk(20));
        assert_eq!(ix.accounts[13].pubkey, pk(21));
        assert_eq!(ix.accounts[14].pubkey, pk(30));
        assert_eq!(ix.accounts[17].pubkey, pk(33));
        // Data tail: u8 liquidatee_accounts then u8 liquidator_accounts.
        assert_eq!(ix.data[16], 4);  // 2 liee obs × 2 accts = 4
        assert_eq!(ix.data[17], 2);  // 1 liq  obs × 2 accts = 2
    }

    #[test]
    fn liquidate_ix_signer_is_authority_only() {
        let ctx = MarginFiLiquidateContext::from_banks(pk(1), pk(2), pk(3), pk(4), pk(5));
        let ix = build_liquidate_ix(&ctx, pk(6), 1, pk(7), pk(8), &[], &[]);
        // Only the authority (index 4) is a signer.
        for (i, m) in ix.accounts.iter().enumerate() {
            assert_eq!(m.is_signer, i == 4, "signer flag wrong at index {i}");
        }
    }

    #[test]
    fn withdraw_ix_data_layout() {
        // 8 disc + 8 amount + 2 (Some(true)) = 18 bytes
        let ix = build_withdraw_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), 12345, Some(true));
        assert_eq!(ix.accounts.len(), 8);
        assert_eq!(ix.data.len(), 18);
        let amt = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(amt, 12345);
        assert_eq!(ix.data[16], 1);  // Option::Some tag
        assert_eq!(ix.data[17], 1);  // true
    }

    #[test]
    fn withdraw_ix_none_variant_is_1_byte_smaller() {
        let ix = build_withdraw_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), 0, None);
        // 8 disc + 8 amount + 1 (None tag) = 17 bytes
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.data[16], 0);  // Option::None tag
    }

    #[test]
    fn withdraw_ix_only_authority_signs() {
        let ix = build_withdraw_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), 0, None);
        // authority is at index 2
        for (i, m) in ix.accounts.iter().enumerate() {
            assert_eq!(m.is_signer, i == 2, "signer flag wrong at {i}");
        }
    }

    #[test]
    fn from_banks_derives_liab_pdas_not_asset_pdas() {
        // Confirm vault PDAs are derived from liab_bank, NOT asset_bank.
        // (Common bug: vaults belong to the liab bank because liquidation
        // pays the liab side in/out.)
        let ctx = MarginFiLiquidateContext::from_banks(pk(1), pk(2), pk(3), pk(4), pk(5));
        assert_eq!(ctx.liab_liquidity_vault, derive_liquidity_vault(&pk(3)));
        assert_eq!(ctx.liab_liquidity_vault_authority, derive_liquidity_vault_authority(&pk(3)));
        assert_eq!(ctx.liab_insurance_vault, derive_insurance_vault(&pk(3)));
        // And NOT asset_bank's PDAs.
        assert_ne!(ctx.liab_liquidity_vault, derive_liquidity_vault(&pk(2)));
    }
}
