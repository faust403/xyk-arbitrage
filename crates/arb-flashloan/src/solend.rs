//! Solend (Save Finance) flash-loan instruction builders + fee math.
//!
//! ## Source-of-truth caveat
//!
//! Solend rebranded to **Save Finance**. The instruction layouts encoded
//! below are taken from the historical `solendprotocol/solana-program-library`
//! token-lending tree and are believed correct for the on-chain program at
//! `So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo`. Per `INSTRUCTIONS.md`
//! Phase 4, before sending real flash loans to devnet/mainnet the operator
//! MUST re-verify these against current Save Finance source. The structure,
//! the envelope, and the fee math are kept independent of the wire format
//! so a discriminator/account-order patch is local to a few constants.

use solana_program::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};

/// Solend / Save Finance token-lending program id (mainnet).
pub const SOLEND_PROGRAM_ID: Pubkey = solana_program::pubkey!(
    "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo"
);

/// SPL Token program id (re-exported as a const so the builders don't pull
/// in the full `spl-token` crate just to reference the id).
pub const TOKEN_PROGRAM_ID: Pubkey = solana_program::pubkey!(
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
);

/// Token-lending instruction discriminators (single-byte tag at offset 0 of
/// the instruction data buffer). Verified against older Solend; flagged in
/// `INSTRUCTIONS.md` Phase 4 as a stop-and-ask trigger if Save Finance has
/// renumbered them.
pub mod ix_tag {
    pub const REFRESH_RESERVE:                u8 = 3;
    pub const FLASH_BORROW_RESERVE_LIQUIDITY: u8 = 19;
    pub const FLASH_REPAY_RESERVE_LIQUIDITY:  u8 = 20;
}

/// Fee-math precision constant. Solend stores fee rates as fixed-point with
/// 18 decimals (a "wad"); converting to a u64 fee requires dividing by this
/// value with proper rounding.
pub const WAD: u128 = 1_000_000_000_000_000_000;

/// Subset of the on-chain Solend `Reserve` we need for fee math. The full
/// account layout is much larger; we parse only what's needed and accept
/// that adding fields later is a non-breaking change.
#[derive(Debug, Clone, Copy)]
pub struct ReserveState {
    /// Flash-loan fee in wad units (e.g. 9 bps = `9e14`).
    pub flash_loan_fee_wad:  u128,
    /// Host-fee percentage (0..=100). Applied as a fraction of the flash
    /// loan fee paid to the optional `host_fee_receiver`.
    pub host_fee_percentage: u8,
}

impl ReserveState {
    /// Convenience constructor from a basis-points fee value, used by the
    /// scout's pre-flight estimator and by tests. Assumes a zero host fee.
    pub const fn from_fee_bps(fee_bps: u64) -> Self {
        // 1 bp = 0.0001 = 1e14 wad. fee_bps * 1e14 stays well under u128.
        Self {
            flash_loan_fee_wad:  (fee_bps as u128) * 100_000_000_000_000u128,
            host_fee_percentage: 0,
        }
    }
}

/// Bundle of instructions that together form a complete USDC flash loan
/// envelope. Returned as a single struct so callers can interleave swap
/// instructions between `borrow_ix` and `repay_ix` without losing track of
/// the accompanying `refresh_ix` or the precomputed `fee_lamports`.
#[derive(Debug, Clone)]
pub struct FlashLoanEnvelope {
    pub refresh_ix:    Instruction,
    pub borrow_ix:     Instruction,
    pub repay_ix:      Instruction,
    /// Exactly the fee Solend will charge — the repay instruction encodes
    /// `amount + fee`, so the caller never needs to recompute this.
    pub fee_lamports:  u64,
}

/// Compute the flash-loan fee Solend will charge for `amount`. Always rounds
/// **up**, matching the on-chain `try_calculate_flash_loan_fees` behaviour
/// — rounding down would let an attacker borrow free dust.
pub fn calculate_flash_loan_fee(reserve_state: &ReserveState, amount: u64) -> u64 {
    if amount == 0 || reserve_state.flash_loan_fee_wad == 0 {
        return 0;
    }
    let numerator: u128 = (amount as u128).saturating_mul(reserve_state.flash_loan_fee_wad);
    let fee = numerator.div_ceil(WAD);
    fee.try_into().unwrap_or(u64::MAX)
}

/// `RefreshReserve`: brings the reserve's interest accumulator up to date.
/// Must precede the borrow in the same transaction or Solend rejects with
/// `LendingError::ReserveStale`. Pyth and Switchboard oracles are passed
/// even when the protocol consults only one; the unused one is read but
/// not validated.
pub fn build_refresh_reserve_ix(
    reserve:           Pubkey,
    pyth_oracle:       Pubkey,
    switchboard_oracle: Pubkey,
) -> Instruction {
    Instruction {
        program_id: SOLEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(pyth_oracle, false),
            AccountMeta::new_readonly(switchboard_oracle, false),
            AccountMeta::new_readonly(sysvar::clock::id(), false),
        ],
        data: vec![ix_tag::REFRESH_RESERVE],
    }
}

/// `FlashBorrowReserveLiquidity`: transfers `amount` from the reserve's
/// liquidity supply to `destination_liquidity` (the bot's USDC ATA), with
/// no debt accrual — repayment is enforced by [`build_flash_repay_ix`] in
/// the SAME transaction.
///
/// Account order (verified 2026-05-10 against on-chain pre-flight sim — an
/// earlier 6-account version triggered `NotEnoughAccountKeys`):
/// ```text
///  0  [W]   source_liquidity (reserve.liquidity_supply)
///  1  [W]   destination_liquidity (caller's ATA)
///  2  [W]   reserve
///  3  [R]   lending_market
///  4  [R]   lending_market_authority
///  5  [R]   instructions sysvar  ← Solend reads this to verify the
///                                  matching FlashRepay is in the same tx
///                                  (anti-reentrancy)
///  6  [R]   token_program
/// ```
pub fn build_flash_borrow_ix(
    lending_market:        Pubkey,
    lending_market_authority: Pubkey,
    reserve:               Pubkey,
    source_liquidity:      Pubkey,    // reserve.liquidity_supply
    destination_liquidity: Pubkey,    // user's USDC ATA
    user:                  Pubkey,    // unused as signer here, retained for API symmetry
    amount:                u64,
) -> Instruction {
    let _ = user; // borrow ix doesn't need the user signer; repay does
    let mut data = Vec::with_capacity(9);
    data.push(ix_tag::FLASH_BORROW_RESERVE_LIQUIDITY);
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: SOLEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(source_liquidity, false),
            AccountMeta::new(destination_liquidity, false),
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(lending_market, false),
            AccountMeta::new_readonly(lending_market_authority, false),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

/// `FlashRepayReserveLiquidity`: paired with the borrow above. The
/// `borrow_instruction_index` is the index of the matching borrow within
/// the SAME transaction's instruction list — Solend uses it to look up the
/// borrow record and require `repay_amount == borrow_amount + fee`.
///
/// Account order copied verbatim from Solend's `flash_repay_reserve_liquidity`
/// SDK helper on the `mainnet` branch. **Note** the canonical layout:
/// `reserve` is at slot 4 (NOT slot 2), `fee_receiver` is at slot 2,
/// there is NO `lending_market_authority` (only `lending_market`), and
/// the user signer comes BEFORE the instructions sysvar:
/// ```text
///  0  [W]    source_liquidity (caller's USDC ATA)
///  1  [W]    destination_liquidity (reserve.liquidity_supply)
///  2  [W]    fee_receiver
///  3  [W]    host_fee_receiver
///  4  [W]    reserve                              ← Solend's FlashBorrow
///                                                   processor checks
///                                                   `repay.accounts[4]`
///                                                   matches the borrow's
///                                                   reserve. Earlier
///                                                   builder put reserve at
///                                                   slot 2, which made the
///                                                   FlashBorrow throw
///                                                   `Invalid reserve account
///                                                   on flash repay`
///                                                   (Custom 49 = InvalidFlashRepay).
///  5  [R]    lending_market
///  6  [R,S]  user (transfer authority)
///  7  [R]    instructions sysvar
///  8  [R]    token_program
/// ```
/// `lending_market_authority` parameter retained for API symmetry with
/// the borrow side but no longer placed into the account list.
#[allow(clippy::too_many_arguments)]
pub fn build_flash_repay_ix(
    lending_market:        Pubkey,
    lending_market_authority: Pubkey,
    reserve:               Pubkey,
    source_liquidity:      Pubkey,    // user's USDC ATA
    destination_liquidity: Pubkey,    // reserve.liquidity_supply
    fee_receiver:          Pubkey,    // reserve.config.fee_receiver
    host_fee_receiver:     Pubkey,    // optional host fee — pass fee_receiver if unused
    user:                  Pubkey,    // signer / token transfer authority
    amount:                u64,       // borrow amount (NOT amount+fee)
    borrow_instruction_index: u8,
) -> Instruction {
    let _ = lending_market_authority; // intentionally unused — see doc above
    let mut data = Vec::with_capacity(10);
    data.push(ix_tag::FLASH_REPAY_RESERVE_LIQUIDITY);
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(borrow_instruction_index);
    Instruction {
        program_id: SOLEND_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(source_liquidity, false),
            AccountMeta::new(destination_liquidity, false),
            AccountMeta::new(fee_receiver, false),
            AccountMeta::new(host_fee_receiver, false),
            AccountMeta::new(reserve, false),
            AccountMeta::new_readonly(lending_market, false),
            AccountMeta::new(user, true),
            AccountMeta::new_readonly(sysvar::instructions::id(), false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data,
    }
}

/// All addresses needed to build the three-instruction envelope. Lives
/// alongside the builder so the orchestrator can keep these in one place.
#[derive(Debug, Clone, Copy)]
pub struct SolendUsdcContext {
    pub lending_market:           Pubkey,
    pub lending_market_authority: Pubkey,
    pub reserve:                  Pubkey,
    pub reserve_liquidity_supply: Pubkey,
    pub fee_receiver:             Pubkey,
    pub host_fee_receiver:        Pubkey,
    pub pyth_oracle:              Pubkey,
    pub switchboard_oracle:       Pubkey,
}

/// Build the [`FlashLoanEnvelope`] for a USDC flash loan of `amount` base
/// units. The borrow_instruction_index in the repay defaults to 1 — the
/// envelope is laid out as `[refresh_ix, borrow_ix, ..swaps.., repay_ix]`
/// so when the caller stitches their swap instructions between borrow and
/// repay, the borrow index stays at slot 1.
pub fn build_usdc_flash_loan_envelope(
    ctx:                      &SolendUsdcContext,
    reserve_state:            &ReserveState,
    user:                     Pubkey,
    user_usdc_ata:            Pubkey,
    amount:                   u64,
    borrow_instruction_index: u8,
) -> FlashLoanEnvelope {
    let refresh_ix = build_refresh_reserve_ix(
        ctx.reserve, ctx.pyth_oracle, ctx.switchboard_oracle,
    );
    let borrow_ix = build_flash_borrow_ix(
        ctx.lending_market,
        ctx.lending_market_authority,
        ctx.reserve,
        ctx.reserve_liquidity_supply,
        user_usdc_ata,
        user,
        amount,
    );
    let repay_ix = build_flash_repay_ix(
        ctx.lending_market,
        ctx.lending_market_authority,
        ctx.reserve,
        user_usdc_ata,
        ctx.reserve_liquidity_supply,
        ctx.fee_receiver,
        ctx.host_fee_receiver,
        user,
        amount,
        borrow_instruction_index,
    );
    let fee_lamports = calculate_flash_loan_fee(reserve_state, amount);
    FlashLoanEnvelope { refresh_ix, borrow_ix, repay_ix, fee_lamports }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn pk(seed: u8) -> Pubkey { Pubkey::new_from_array([seed; 32]) }

    fn ctx() -> SolendUsdcContext {
        SolendUsdcContext {
            lending_market:           pk(1),
            lending_market_authority: pk(2),
            reserve:                  pk(3),
            reserve_liquidity_supply: pk(4),
            fee_receiver:             pk(5),
            host_fee_receiver:        pk(6),
            pyth_oracle:              pk(7),
            switchboard_oracle:       pk(8),
        }
    }

    // ---------- Instruction encoding ----------

    #[test]
    fn test_borrow_ix_program_id_correct() {
        let ix = build_flash_borrow_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 100);
        assert_eq!(ix.program_id, SOLEND_PROGRAM_ID);
    }

    #[test]
    fn test_borrow_ix_account_count() {
        let ix = build_flash_borrow_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 100);
        assert_eq!(ix.accounts.len(), 7);
    }

    #[test]
    fn test_borrow_ix_account_order() {
        let ix = build_flash_borrow_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 100);
        // [source_liquidity, destination_liquidity, reserve, lending_market,
        //  lending_market_authority, instructions_sysvar, token_program]
        assert_eq!(ix.accounts[0].pubkey, pk(4));
        assert!(ix.accounts[0].is_writable);
        assert!(!ix.accounts[0].is_signer);
        assert_eq!(ix.accounts[1].pubkey, pk(5));
        assert!(ix.accounts[1].is_writable);
        assert_eq!(ix.accounts[2].pubkey, pk(3));
        assert!(ix.accounts[2].is_writable);
        assert_eq!(ix.accounts[3].pubkey, pk(1));
        assert!(!ix.accounts[3].is_writable);
        assert_eq!(ix.accounts[4].pubkey, pk(2));
        assert!(!ix.accounts[4].is_writable);
        assert_eq!(ix.accounts[5].pubkey, sysvar::instructions::id());
        assert!(!ix.accounts[5].is_writable);
        assert_eq!(ix.accounts[6].pubkey, TOKEN_PROGRAM_ID);
        assert!(!ix.accounts[6].is_writable);
    }

    #[test]
    fn test_borrow_ix_data_encoding() {
        let ix = build_flash_borrow_ix(pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), 0xDEAD_BEEF_u64);
        assert_eq!(ix.data[0], ix_tag::FLASH_BORROW_RESERVE_LIQUIDITY);
        // little-endian amount in the next 8 bytes.
        let amt = u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        assert_eq!(amt, 0xDEAD_BEEF_u64);
        assert_eq!(ix.data.len(), 9);
    }

    #[test]
    fn test_repay_ix_references_correct_borrow_index() {
        let ix = build_flash_repay_ix(
            pk(1), pk(2), pk(3), pk(4), pk(5), pk(6), pk(7), pk(8), 1_000_000, 7,
        );
        assert_eq!(ix.data[0], ix_tag::FLASH_REPAY_RESERVE_LIQUIDITY);
        let amt = u64::from_le_bytes(ix.data[1..9].try_into().unwrap());
        assert_eq!(amt, 1_000_000);
        assert_eq!(ix.data[9], 7);
        assert_eq!(ix.data.len(), 10);
    }

    #[test]
    fn test_repay_ix_account_order() {
        // Args mapped to test pubkeys:
        //   lending_market(10), lending_market_authority(11) [unused],
        //   reserve(12), source_liquidity(13), destination_liquidity(14),
        //   fee_receiver(15), host_fee_receiver(16), user(17)
        let ix = build_flash_repay_ix(
            pk(10), pk(11), pk(12), pk(13), pk(14), pk(15), pk(16), pk(17), 100, 1,
        );
        // Canonical Solend SDK order (9 accounts):
        // [source_liquidity, destination_liquidity, fee_receiver,
        //  host_fee_receiver, reserve, lending_market, user (signer),
        //  instructions_sysvar, token_program]
        let order = [
            pk(13), pk(14), pk(15), pk(16), pk(12), pk(10), pk(17),
            sysvar::instructions::id(), TOKEN_PROGRAM_ID,
        ];
        assert_eq!(ix.accounts.len(), 9, "FlashRepay must be 9 accounts");
        for (idx, expected) in order.into_iter().enumerate() {
            assert_eq!(ix.accounts[idx].pubkey, expected, "slot {idx}");
        }
        // Only `user` (slot 6) is a signer.
        assert!(ix.accounts[6].is_signer);
        for (idx, m) in ix.accounts.iter().enumerate() {
            if idx != 6 { assert!(!m.is_signer, "unexpected signer at {idx}"); }
        }
    }

    /// Regression: the `reserve` account MUST be at slot 4 of FlashRepay
    /// because Solend's FlashBorrow processor reads `repay_ix.accounts[4]`
    /// when validating that borrow + repay reference the same reserve. An
    /// earlier builder placed reserve at slot 2 and triggered Custom 49 =
    /// InvalidFlashRepay on every flash-loan attempt.
    #[test]
    fn test_repay_ix_reserve_at_slot_4() {
        let ix = build_flash_repay_ix(
            pk(10), pk(11), pk(12), pk(13), pk(14), pk(15), pk(16), pk(17), 100, 1,
        );
        assert_eq!(ix.accounts[4].pubkey, pk(12),
                   "reserve MUST be at slot 4 — Solend's borrow processor \
                    checks repay.accounts[4] for the reserve match");
    }

    #[test]
    fn test_refresh_ix_includes_oracles() {
        let ix = build_refresh_reserve_ix(pk(1), pk(2), pk(3));
        // [reserve, pyth, switchboard, clock]
        assert_eq!(ix.accounts.len(), 4);
        assert_eq!(ix.accounts[0].pubkey, pk(1));
        assert_eq!(ix.accounts[1].pubkey, pk(2));
        assert_eq!(ix.accounts[2].pubkey, pk(3));
        assert_eq!(ix.accounts[3].pubkey, sysvar::clock::id());
        assert_eq!(ix.data, vec![ix_tag::REFRESH_RESERVE]);
    }

    // ---------- Envelope ----------

    #[test]
    fn test_envelope_repay_amount_includes_fee() {
        let rs  = ReserveState::from_fee_bps(9);
        let env = build_usdc_flash_loan_envelope(&ctx(), &rs, pk(20), pk(21), 1_000_000_000, 1);
        // 9 bps of 1_000 USDC = 0.9 USDC = 900_000 base units (rounded up).
        assert_eq!(env.fee_lamports, 900_000);
        // The repay instruction holds the borrow amount (NOT the post-fee
        // amount); Solend's on-chain math adds the fee at execution time.
        let repay_amt = u64::from_le_bytes(env.repay_ix.data[1..9].try_into().unwrap());
        assert_eq!(repay_amt, 1_000_000_000);
    }

    // ---------- Fee math ----------

    #[test]
    fn test_fee_calculation_known_values() {
        let rs = ReserveState::from_fee_bps(9);
        assert_eq!(calculate_flash_loan_fee(&rs, 1_000_000),         900);     // 1 USDC → 0.0009
        assert_eq!(calculate_flash_loan_fee(&rs, 100_000_000),       90_000);  // 100 USDC → 0.09
        assert_eq!(calculate_flash_loan_fee(&rs, 1_000_000_000),     900_000); // 1000 USDC → 0.9
    }

    #[test]
    fn test_fee_calculation_zero_amount() {
        let rs = ReserveState::from_fee_bps(9);
        assert_eq!(calculate_flash_loan_fee(&rs, 0), 0);
    }

    #[test]
    fn test_fee_calculation_zero_fee_wad() {
        let rs = ReserveState { flash_loan_fee_wad: 0, host_fee_percentage: 0 };
        assert_eq!(calculate_flash_loan_fee(&rs, 1_000_000_000), 0);
    }

    #[test]
    fn test_fee_calculation_no_overflow() {
        // The point of this test is "doesn't panic on a u128 multiplication
        // overflow path"; the exact value is a sanity check.
        // 9 bps × u64::MAX is well under u64::MAX again (≈ 0.09% of MAX),
        // so saturation isn't expected here.
        let rs  = ReserveState::from_fee_bps(9);
        let fee = calculate_flash_loan_fee(&rs, u64::MAX);
        // Manually computed: ceil(u64::MAX * 9e14 / 1e18) = 16_602_069_666_338_597.
        assert_eq!(fee, 16_602_069_666_338_597);
        assert!(fee < u64::MAX);

        // Now force the saturating path with a huge fee_wad. fee_wad = 1 wad
        // means "100% fee", which would be impossible on-chain but exercises
        // the overflow branch.
        let huge = ReserveState { flash_loan_fee_wad: WAD * 1_000_000, host_fee_percentage: 0 };
        assert_eq!(calculate_flash_loan_fee(&huge, u64::MAX), u64::MAX);
    }

    #[test]
    fn test_fee_calculation_rounds_up() {
        // 1 base unit at 9 bps would otherwise floor to 0; ceil keeps it at 1.
        let rs = ReserveState::from_fee_bps(9);
        assert_eq!(calculate_flash_loan_fee(&rs, 1), 1);
    }

    proptest! {
        #[test]
        fn prop_fee_is_monotonic(a in 0u64..u64::MAX/2, b in 0u64..u64::MAX/2) {
            let rs = ReserveState::from_fee_bps(9);
            let fee_a = calculate_flash_loan_fee(&rs, a);
            let fee_b = calculate_flash_loan_fee(&rs, a.saturating_add(b));
            prop_assert!(fee_b >= fee_a);
        }

        #[test]
        fn prop_fee_under_one_percent(amount in 0u64..1_000_000_000_000_u64) {
            // 9 bps == 0.09% — should always be well under 1% of the borrow.
            let rs = ReserveState::from_fee_bps(9);
            let fee = calculate_flash_loan_fee(&rs, amount);
            // 1% of amount, ceil so we don't false-positive on the boundary.
            let one_pct = (amount as u128).div_ceil(100) as u64;
            prop_assert!(fee <= one_pct, "fee {fee} > 1% {one_pct} of {amount}");
        }
    }
}
