//! Kamino Lend (klend) instruction builders.
//!
//! Kamino is a Solend fork with substantial extensions: e-mode groups,
//! referrer accounting, Scope/Switchboard/Pyth multi-oracle, optional
//! per-reserve farms, Token-2022 support per-side. The liquidator only
//! cares about three IXs:
//!
//! 1. `refresh_reserve` — refreshes a single reserve's price + interest.
//!    Mandatory before any borrow/withdraw/liquidate. Six accounts (one
//!    required + four optional oracle slots that use the klend program
//!    pubkey as the "None" sentinel).
//! 2. `refresh_obligation` — refreshes an obligation's total values from
//!    its already-refreshed reserves. Two fixed accounts plus per-position
//!    Reserve accounts as remaining_accounts.
//! 3. `liquidate_obligation_and_redeem_reserve_collateral` (V1) — the
//!    actual liquidate. 20 fixed accounts + per-obligation-reserve
//!    remaining_accounts. V2 handler adds farm accounts; V1 is sufficient
//!    when both repay and withdraw reserves have farm_collateral and
//!    farm_debt as default Pubkey.
//!
//! ## On data layouts
//!
//! Kamino's `Reserve` (8624 B) and `Obligation` (1792 B on-chain) are
//! `#[account(zero_copy)] #[repr(C)]` — DO NOT hand-pack offsets, use
//! `bytemuck::from_bytes` against the verbatim struct definitions. Parsers
//! live in `arb-scout/src/kamino_liq.rs`.
//!
//! Verified 2026-05-15 against `Kamino-Finance/klend` master.

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};

use crate::TOKEN_PROGRAM_ID;

/// Kamino Lend program id (mainnet).
pub const KAMINO_LEND_PROGRAM: Pubkey = solana_program::pubkey!(
    "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD"
);

/// Main Market on Kamino Lend mainnet. Most-active lending market — covers
/// USDC, USDT, SOL, JitoSOL, mSOL, JLP, etc. Other markets (JLP-only,
/// Altcoin, Jito) exist for niche assets but the Main Market is where the
/// liquidator finds its primary candidate pool.
///
/// **Verify on-chain via getAccountInfo + decode owner == KAMINO_LEND_PROGRAM
/// before mainnet use** — Kamino can spin up new markets.
pub const KAMINO_MAIN_MARKET: Pubkey = solana_program::pubkey!(
    "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF"
);

/// Anchor seed for the lending-market authority PDA. Derivation:
/// `find_program_address(&[LENDING_MARKET_AUTH, market.as_ref()], &KAMINO_LEND_PROGRAM)`.
/// Bump is stored in `LendingMarket.bump_seed` (offset 8 in struct).
pub const LENDING_MARKET_AUTH_SEED: &[u8] = b"lma";

/// Derive the `lending_market_authority` PDA for a given market.
pub fn derive_lending_market_authority(market: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[LENDING_MARKET_AUTH_SEED, market.as_ref()],
        &KAMINO_LEND_PROGRAM,
    ).0
}

/// Anchor instruction-discriminator helper. Same shape as in [`super::marginfi`].
pub fn discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("global:{name}");
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

/// Anchor account-discriminator helper (used by parsers to recognize an
/// on-chain account: `account:{struct_name}`).
pub fn account_discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("account:{name}");
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

// ─── refresh_reserve ─────────────────────────────────────────────

/// `refresh_reserve` — refresh a single Kamino reserve's price + accrued
/// interest. Mandatory before borrow/repay/withdraw/liquidate paths.
///
/// Accounts (verified 2026-05-15):
/// ```text
///  0  [W]   reserve
///  1  [R]   lending_market
///  2  [R]?  pyth_oracle           (KAMINO_LEND_PROGRAM = "None")
///  3  [R]?  switchboard_price     (KAMINO_LEND_PROGRAM = "None")
///  4  [R]?  switchboard_twap      (KAMINO_LEND_PROGRAM = "None")
///  5  [R]?  scope_prices          (KAMINO_LEND_PROGRAM = "None")
/// ```
///
/// **The "None" sentinel for optional oracle slots is the klend program
/// pubkey itself**, not `system_program::ID`. Passing the wrong sentinel
/// triggers `InvalidOracleConfig`.
///
/// Data: just the 8-byte discriminator. No args.
pub fn build_refresh_reserve_ix(
    reserve:               Pubkey,
    lending_market:        Pubkey,
    pyth_oracle:           Option<Pubkey>,
    switchboard_price:     Option<Pubkey>,
    switchboard_twap:      Option<Pubkey>,
    scope_prices:          Option<Pubkey>,
) -> Instruction {
    let none_sentinel = KAMINO_LEND_PROGRAM;
    let accounts = vec![
        AccountMeta::new(reserve, false),
        AccountMeta::new_readonly(lending_market, false),
        AccountMeta::new_readonly(pyth_oracle.unwrap_or(none_sentinel),       false),
        AccountMeta::new_readonly(switchboard_price.unwrap_or(none_sentinel), false),
        AccountMeta::new_readonly(switchboard_twap.unwrap_or(none_sentinel),  false),
        AccountMeta::new_readonly(scope_prices.unwrap_or(none_sentinel),      false),
    ];
    Instruction {
        program_id: KAMINO_LEND_PROGRAM,
        accounts,
        data: discriminator("refresh_reserve").to_vec(),
    }
}

// ─── refresh_obligation ─────────────────────────────────────────

/// `refresh_obligation` — refreshes the obligation's `deposited_value_sf`,
/// `borrow_factor_adjusted_debt_value_sf`, etc. from its (already-refreshed)
/// reserves.
///
/// Accounts:
/// ```text
///  0  [R]   lending_market
///  1  [W]   obligation
///  2..     remaining: deposits' reserves IN ORDER, then borrows' reserves
///           IN ORDER, then (if obligation.has_referrer()) borrows-count
///           ReferrerTokenState accounts.
/// ```
///
/// Must be called AFTER all relevant `refresh_reserve` ixs and BEFORE any
/// `liquidate_*` ix. Data: just the discriminator.
pub fn build_refresh_obligation_ix(
    lending_market:           Pubkey,
    obligation:               Pubkey,
    deposit_reserves:         &[Pubkey],
    borrow_reserves:          &[Pubkey],
    borrow_referrer_states:   &[Pubkey],
) -> Instruction {
    let mut accounts = vec![
        AccountMeta::new_readonly(lending_market, false),
        AccountMeta::new(obligation, false),
    ];
    for r in deposit_reserves     { accounts.push(AccountMeta::new_readonly(*r, false)); }
    for r in borrow_reserves      { accounts.push(AccountMeta::new_readonly(*r, false)); }
    for s in borrow_referrer_states {
        accounts.push(AccountMeta::new(*s, false));  // ReferrerTokenState is writable
    }
    Instruction {
        program_id: KAMINO_LEND_PROGRAM,
        accounts,
        data: discriminator("refresh_obligation").to_vec(),
    }
}

// ─── liquidate_obligation_and_redeem_reserve_collateral (V1) ────

/// Pubkey bundle for the V1 liquidate handler. Per-side token programs
/// (`repay_liquidity_token_program`, `withdraw_liquidity_token_program`)
/// may each be SPL Token or Token-2022 depending on each reserve's
/// `liquidity.token_program` field — read it from the parsed Reserve and
/// pass correctly here.
#[derive(Debug, Clone, Copy)]
pub struct KaminoLiquidateContext {
    pub obligation:                            Pubkey,
    pub lending_market:                        Pubkey,
    pub lending_market_authority:              Pubkey,  // derive via derive_lending_market_authority
    pub repay_reserve:                         Pubkey,
    pub repay_reserve_liquidity_mint:          Pubkey,
    pub repay_reserve_liquidity_supply:        Pubkey,  // reserve.liquidity.supply_vault
    pub withdraw_reserve:                      Pubkey,
    pub withdraw_reserve_liquidity_mint:       Pubkey,
    pub withdraw_reserve_collateral_mint:      Pubkey,  // reserve.collateral.mint_pubkey
    pub withdraw_reserve_collateral_supply:    Pubkey,  // reserve.collateral.supply_vault
    pub withdraw_reserve_liquidity_supply:     Pubkey,
    pub withdraw_reserve_liquidity_fee_receiver: Pubkey, // reserve.liquidity.fee_vault
    pub user_source_liquidity:                 Pubkey,  // liquidator's repay-mint ATA
    pub user_destination_collateral:           Pubkey,  // liquidator's c-token ATA
    pub user_destination_liquidity:            Pubkey,  // liquidator's withdraw-mint ATA
    pub collateral_token_program:              Pubkey,  // TOKEN_PROGRAM_ID (cTokens are SPL)
    pub repay_liquidity_token_program:         Pubkey,  // SPL or Token-2022
    pub withdraw_liquidity_token_program:      Pubkey,  // SPL or Token-2022
}

impl KaminoLiquidateContext {
    /// Construct a context with sensible defaults: lending_market_authority
    /// derived from `lending_market`, and both liquidity-token programs set
    /// to classic SPL Token. Callers MUST override the per-side token
    /// programs when either reserve uses Token-2022 (read each reserve's
    /// `liquidity.token_program` field).
    pub fn new(
        obligation:                          Pubkey,
        lending_market:                      Pubkey,
        repay_reserve:                       Pubkey,
        repay_reserve_liquidity_mint:        Pubkey,
        repay_reserve_liquidity_supply:      Pubkey,
        withdraw_reserve:                    Pubkey,
        withdraw_reserve_liquidity_mint:     Pubkey,
        withdraw_reserve_collateral_mint:    Pubkey,
        withdraw_reserve_collateral_supply:  Pubkey,
        withdraw_reserve_liquidity_supply:   Pubkey,
        withdraw_reserve_liquidity_fee_receiver: Pubkey,
        user_source_liquidity:               Pubkey,
        user_destination_collateral:         Pubkey,
        user_destination_liquidity:          Pubkey,
    ) -> Self {
        Self {
            obligation,
            lending_market,
            lending_market_authority: derive_lending_market_authority(&lending_market),
            repay_reserve, repay_reserve_liquidity_mint, repay_reserve_liquidity_supply,
            withdraw_reserve, withdraw_reserve_liquidity_mint,
            withdraw_reserve_collateral_mint, withdraw_reserve_collateral_supply,
            withdraw_reserve_liquidity_supply, withdraw_reserve_liquidity_fee_receiver,
            user_source_liquidity, user_destination_collateral, user_destination_liquidity,
            collateral_token_program:           TOKEN_PROGRAM_ID,
            repay_liquidity_token_program:      TOKEN_PROGRAM_ID,
            withdraw_liquidity_token_program:   TOKEN_PROGRAM_ID,
        }
    }
}

/// Build `liquidate_obligation_and_redeem_reserve_collateral` (V1 handler).
///
/// Args: `liquidity_amount, min_acceptable_received_liquidity_amount,
/// max_allowed_ltv_override_percent` (all u64).
/// Set `max_allowed_ltv_override_percent = 0` to use the reserve's own
/// `unhealthy_ltv` (canonical liquidate). Set to a small percent to
/// force-liquidate healthier accounts (e.g. autodeleverage paths) — not
/// commonly used by external liquidators.
///
/// Account order (verified 2026-05-15):
/// ```text
///  0  [W,S] liquidator (fee payer + auth)
///  1  [W]   obligation
///  2  [R]   lending_market
///  3  [R]   lending_market_authority  (PDA)
///  4  [W]   repay_reserve
///  5  [R]   repay_reserve_liquidity_mint
///  6  [W]   repay_reserve_liquidity_supply
///  7  [W]   withdraw_reserve
///  8  [R]   withdraw_reserve_liquidity_mint
///  9  [W]   withdraw_reserve_collateral_mint
/// 10  [W]   withdraw_reserve_collateral_supply
/// 11  [W]   withdraw_reserve_liquidity_supply
/// 12  [W]   withdraw_reserve_liquidity_fee_receiver
/// 13  [W]   user_source_liquidity
/// 14  [W]   user_destination_collateral
/// 15  [W]   user_destination_liquidity
/// 16  [R]   collateral_token_program  (SPL Token; cTokens always SPL)
/// 17  [R]   repay_liquidity_token_program
/// 18  [R]   withdraw_liquidity_token_program
/// 19  [R]   instruction_sysvar
/// 20..      remaining: Reserve accounts the obligation references
///            (deposits then borrows order), used by the program's
///            recompute pass. Last account may be a permissioning marker
///            if the market has the LIQUIDATE permissioned op bit set.
/// ```
pub fn build_liquidate_obligation_and_redeem_reserve_collateral_ix(
    ctx:                                       &KaminoLiquidateContext,
    liquidator:                                Pubkey,
    liquidity_amount:                          u64,
    min_acceptable_received_liquidity_amount:  u64,
    max_allowed_ltv_override_percent:          u64,
    obligation_referenced_reserves:            &[Pubkey],
) -> Instruction {
    let mut data = discriminator("liquidate_obligation_and_redeem_reserve_collateral").to_vec();
    data.extend_from_slice(&liquidity_amount.to_le_bytes());
    data.extend_from_slice(&min_acceptable_received_liquidity_amount.to_le_bytes());
    data.extend_from_slice(&max_allowed_ltv_override_percent.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new(liquidator, true),                                  // 0
        AccountMeta::new(ctx.obligation, false),                             // 1
        AccountMeta::new_readonly(ctx.lending_market, false),                // 2
        AccountMeta::new_readonly(ctx.lending_market_authority, false),      // 3
        AccountMeta::new(ctx.repay_reserve, false),                          // 4
        AccountMeta::new_readonly(ctx.repay_reserve_liquidity_mint, false),  // 5
        AccountMeta::new(ctx.repay_reserve_liquidity_supply, false),         // 6
        AccountMeta::new(ctx.withdraw_reserve, false),                       // 7
        AccountMeta::new_readonly(ctx.withdraw_reserve_liquidity_mint, false), // 8
        AccountMeta::new(ctx.withdraw_reserve_collateral_mint, false),       // 9
        AccountMeta::new(ctx.withdraw_reserve_collateral_supply, false),     // 10
        AccountMeta::new(ctx.withdraw_reserve_liquidity_supply, false),      // 11
        AccountMeta::new(ctx.withdraw_reserve_liquidity_fee_receiver, false), // 12
        AccountMeta::new(ctx.user_source_liquidity, false),                  // 13
        AccountMeta::new(ctx.user_destination_collateral, false),            // 14
        AccountMeta::new(ctx.user_destination_liquidity, false),             // 15
        AccountMeta::new_readonly(ctx.collateral_token_program, false),      // 16
        AccountMeta::new_readonly(ctx.repay_liquidity_token_program, false), // 17
        AccountMeta::new_readonly(ctx.withdraw_liquidity_token_program, false), // 18
        AccountMeta::new_readonly(sysvar::instructions::id(), false),        // 19
    ];
    for r in obligation_referenced_reserves {
        accounts.push(AccountMeta::new_readonly(*r, false));
    }

    Instruction { program_id: KAMINO_LEND_PROGRAM, accounts, data }
}

// ─── liquidate_obligation_and_redeem_reserve_collateral_v2 ──────

/// Per-side farms-account block for the V2 liquidate handler. Both
/// fields are `Option<AccountInfo>` in the Anchor struct — when farms
/// are NOT used on that side, pass the klend program pubkey as the
/// "None" sentinel (same convention as refresh_reserve's optional
/// oracle slots).
#[derive(Debug, Clone, Copy, Default)]
pub struct ObligationFarmsAccounts {
    /// User's per-farm state PDA. Pass `None` if the corresponding
    /// reserve has farm_collateral / farm_debt set to default.
    pub obligation_farm_user_state: Option<Pubkey>,
    /// Per-reserve farm state account. Same None-rule.
    pub reserve_farm_state:         Option<Pubkey>,
}

impl ObligationFarmsAccounts {
    /// Resolve both slots, substituting the klend program id where None.
    /// Both accounts are marked WRITABLE in the Anchor struct.
    pub fn to_metas(self) -> [AccountMeta; 2] {
        let s = KAMINO_LEND_PROGRAM;
        [
            AccountMeta::new(self.obligation_farm_user_state.unwrap_or(s), false),
            AccountMeta::new(self.reserve_farm_state.unwrap_or(s),         false),
        ]
    }
}

/// Build `liquidate_obligation_and_redeem_reserve_collateral_v2`.
///
/// Same args as V1 (`liquidity_amount, min_acceptable_received_liquidity_amount,
/// max_allowed_ltv_override_percent`). Five extra fixed accounts after the
/// 20 V1 slots:
/// ```text
/// 20  [W]?  collateral_farms.obligation_farm_user_state  (None=klend-pid)
/// 21  [W]?  collateral_farms.reserve_farm_state           (None=klend-pid)
/// 22  [W]?  debt_farms.obligation_farm_user_state
/// 23  [W]?  debt_farms.reserve_farm_state
/// 24  [R]   farms_program                                 (FarmsPZpWu9i7K...)
/// 25..      remaining: same as V1 — obligation-referenced reserves
/// ```
///
/// Use V2 when EITHER reserve has `farm_collateral` or `farm_debt`
/// non-default. V2 with all-None farms is functionally identical to V1
/// but costs ~2K extra CU + 5 byte slots; prefer V1 when farms are
/// confirmed absent. The hunter checks both reserves' farm fields and
/// routes accordingly.
///
/// **Farms program id (mainnet):** `FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr`.
pub fn build_liquidate_obligation_and_redeem_reserve_collateral_v2_ix(
    ctx:                                       &KaminoLiquidateContext,
    liquidator:                                Pubkey,
    liquidity_amount:                          u64,
    min_acceptable_received_liquidity_amount:  u64,
    max_allowed_ltv_override_percent:          u64,
    collateral_farms:                          ObligationFarmsAccounts,
    debt_farms:                                ObligationFarmsAccounts,
    farms_program:                             Pubkey,
    obligation_referenced_reserves:            &[Pubkey],
) -> Instruction {
    let mut data = discriminator("liquidate_obligation_and_redeem_reserve_collateral_v2").to_vec();
    data.extend_from_slice(&liquidity_amount.to_le_bytes());
    data.extend_from_slice(&min_acceptable_received_liquidity_amount.to_le_bytes());
    data.extend_from_slice(&max_allowed_ltv_override_percent.to_le_bytes());

    // 0..19 — exact V1 layout.
    let mut accounts = vec![
        AccountMeta::new(liquidator, true),                                  // 0
        AccountMeta::new(ctx.obligation, false),                             // 1
        AccountMeta::new_readonly(ctx.lending_market, false),                // 2
        AccountMeta::new_readonly(ctx.lending_market_authority, false),      // 3
        AccountMeta::new(ctx.repay_reserve, false),                          // 4
        AccountMeta::new_readonly(ctx.repay_reserve_liquidity_mint, false),  // 5
        AccountMeta::new(ctx.repay_reserve_liquidity_supply, false),         // 6
        AccountMeta::new(ctx.withdraw_reserve, false),                       // 7
        AccountMeta::new_readonly(ctx.withdraw_reserve_liquidity_mint, false), // 8
        AccountMeta::new(ctx.withdraw_reserve_collateral_mint, false),       // 9
        AccountMeta::new(ctx.withdraw_reserve_collateral_supply, false),     // 10
        AccountMeta::new(ctx.withdraw_reserve_liquidity_supply, false),      // 11
        AccountMeta::new(ctx.withdraw_reserve_liquidity_fee_receiver, false), // 12
        AccountMeta::new(ctx.user_source_liquidity, false),                  // 13
        AccountMeta::new(ctx.user_destination_collateral, false),            // 14
        AccountMeta::new(ctx.user_destination_liquidity, false),             // 15
        AccountMeta::new_readonly(ctx.collateral_token_program, false),      // 16
        AccountMeta::new_readonly(ctx.repay_liquidity_token_program, false), // 17
        AccountMeta::new_readonly(ctx.withdraw_liquidity_token_program, false), // 18
        AccountMeta::new_readonly(sysvar::instructions::id(), false),        // 19
    ];
    // 20..23 — farms accounts (writable).
    accounts.extend_from_slice(&collateral_farms.to_metas());
    accounts.extend_from_slice(&debt_farms.to_metas());
    // 24 — farms_program (readonly).
    accounts.push(AccountMeta::new_readonly(farms_program, false));
    // 25.. — remaining: obligation-referenced reserves.
    for r in obligation_referenced_reserves {
        accounts.push(AccountMeta::new_readonly(*r, false));
    }

    Instruction { program_id: KAMINO_LEND_PROGRAM, accounts, data }
}

/// Kamino Farms program id (mainnet). Used as the `farms_program`
/// account in V2 liquidate. Verify before mainnet by reading the
/// `farm_collateral` or `farm_debt` account's `owner` field — should
/// equal this pubkey.
pub const KAMINO_FARMS_PROGRAM: Pubkey = solana_program::pubkey!(
    "FarmsPZpWu9i7Kky8tPN37rs2TpmMrAZrC7S7vJa91Hr"
);

// ─── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }

    #[test]
    fn program_constants_resolve() {
        let _ = KAMINO_LEND_PROGRAM;
        let _ = KAMINO_MAIN_MARKET;
        let pda = derive_lending_market_authority(&KAMINO_MAIN_MARKET);
        assert_ne!(pda, Pubkey::default());
    }

    #[test]
    fn refresh_reserve_no_oracles_uses_klend_sentinel() {
        let ix = build_refresh_reserve_ix(pk(1), pk(2), None, None, None, None);
        assert_eq!(ix.program_id, KAMINO_LEND_PROGRAM);
        assert_eq!(ix.accounts.len(), 6);
        // Slots 2-5 are the optional-oracle "None" sentinels.
        for i in 2..6 {
            assert_eq!(ix.accounts[i].pubkey, KAMINO_LEND_PROGRAM,
                "slot {i} should be klend sentinel");
        }
        assert_eq!(ix.data.len(), 8);
    }

    #[test]
    fn refresh_reserve_with_pyth_keeps_other_slots_sentinel() {
        let pyth = pk(99);
        let ix = build_refresh_reserve_ix(pk(1), pk(2), Some(pyth), None, None, None);
        assert_eq!(ix.accounts[2].pubkey, pyth);
        assert_eq!(ix.accounts[3].pubkey, KAMINO_LEND_PROGRAM);
        assert_eq!(ix.accounts[4].pubkey, KAMINO_LEND_PROGRAM);
        assert_eq!(ix.accounts[5].pubkey, KAMINO_LEND_PROGRAM);
    }

    #[test]
    fn refresh_obligation_assembles_remaining_in_order() {
        let deps   = [pk(10), pk(11)];
        let borrs  = [pk(20)];
        let refs   = [pk(30)];
        let ix = build_refresh_obligation_ix(pk(1), pk(2), &deps, &borrs, &refs);
        // 2 fixed + 2 deps + 1 borrow + 1 referrer = 6 accounts
        assert_eq!(ix.accounts.len(), 6);
        assert_eq!(ix.accounts[2].pubkey, pk(10));
        assert_eq!(ix.accounts[3].pubkey, pk(11));
        assert_eq!(ix.accounts[4].pubkey, pk(20));
        assert_eq!(ix.accounts[5].pubkey, pk(30));
        assert!(ix.accounts[5].is_writable, "referrer state is writable");
        assert_eq!(ix.data.len(), 8);
    }

    #[test]
    fn liquidate_ix_account_count_and_data_size() {
        let ctx = KaminoLiquidateContext::new(
            pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), pk(9), pk(10),
            pk(11), pk(12), pk(13), pk(14),
        );
        let extra = [pk(20), pk(21)];
        let ix = build_liquidate_obligation_and_redeem_reserve_collateral_ix(
            &ctx, pk(50), 1_000_000, 0, 0, &extra,
        );
        // 20 fixed + 2 extra = 22 accounts
        assert_eq!(ix.accounts.len(), 22);
        // disc(8) + 3 × u64 = 32 bytes
        assert_eq!(ix.data.len(), 32);
        let liq_amt = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(liq_amt, 1_000_000);
        // liquidator at slot 0 is signer + writable
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[0].is_writable);
    }

    #[test]
    fn v2_liquidate_account_count_with_no_farms() {
        let ctx = KaminoLiquidateContext::new(
            pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), pk(9), pk(10),
            pk(11), pk(12), pk(13), pk(14),
        );
        let none_farm = ObligationFarmsAccounts::default();
        let extra = [pk(20)];
        let ix = build_liquidate_obligation_and_redeem_reserve_collateral_v2_ix(
            &ctx, pk(50), 1_000_000, 0, 0,
            none_farm, none_farm, KAMINO_FARMS_PROGRAM, &extra,
        );
        // 20 V1 + 4 farms (2 colla + 2 debt) + 1 farms_program + 1 extra = 26
        assert_eq!(ix.accounts.len(), 26);
        // disc(8) + 3 × u64 = 32 bytes
        assert_eq!(ix.data.len(), 32);
        // farms slots 20..23 should be klend-pid sentinel when None.
        for slot in 20..24 {
            assert_eq!(ix.accounts[slot].pubkey, KAMINO_LEND_PROGRAM,
                "slot {slot} should be klend-pid sentinel for absent farm");
            assert!(ix.accounts[slot].is_writable,
                "slot {slot} farm account should be writable");
        }
        // farms_program at slot 24 is readonly.
        assert_eq!(ix.accounts[24].pubkey, KAMINO_FARMS_PROGRAM);
        assert!(!ix.accounts[24].is_writable);
    }

    #[test]
    fn v2_liquidate_with_real_farm_pubkeys() {
        let ctx = KaminoLiquidateContext::new(
            pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), pk(9), pk(10),
            pk(11), pk(12), pk(13), pk(14),
        );
        let coll_farm = ObligationFarmsAccounts {
            obligation_farm_user_state: Some(pk(100)),
            reserve_farm_state:         Some(pk(101)),
        };
        let debt_farm = ObligationFarmsAccounts {
            obligation_farm_user_state: Some(pk(110)),
            reserve_farm_state:         None,  // mixed-case test
        };
        let ix = build_liquidate_obligation_and_redeem_reserve_collateral_v2_ix(
            &ctx, pk(50), 1, 0, 0,
            coll_farm, debt_farm, KAMINO_FARMS_PROGRAM, &[],
        );
        assert_eq!(ix.accounts[20].pubkey, pk(100));
        assert_eq!(ix.accounts[21].pubkey, pk(101));
        assert_eq!(ix.accounts[22].pubkey, pk(110));
        assert_eq!(ix.accounts[23].pubkey, KAMINO_LEND_PROGRAM); // None
    }

    #[test]
    fn liquidate_pda_derived_from_market_not_program() {
        // Regression test: PDA should be derived from MARKET pubkey, not
        // from the static KAMINO_LEND_PROGRAM. A bug here would make the
        // PDA wrong for non-main markets (JLP / Altcoin / Jito).
        let market_a = pk(1);
        let market_b = pk(2);
        assert_ne!(
            derive_lending_market_authority(&market_a),
            derive_lending_market_authority(&market_b),
        );
    }
}
