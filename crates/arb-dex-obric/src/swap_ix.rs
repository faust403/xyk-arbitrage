//! Native Obric V2 swap-instruction builders for the X→Y and Y→X variants.
//!
//! IDL source (verified 2026-05-10):
//! <https://github.com/bitquery/solana-idl-lib/blob/main/obric/obric_v2.json>
//!
//! Anchor program → discriminator = `sha256("global:{name}")[..8]`. Computed
//! lazily on first call (sub-microsecond, then memoised in a `OnceLock`).
//!
//! ## Account order — DO NOT REARRANGE
//!
//! Both variants take 12 accounts in identical positions, **except** for
//! account index 7:
//! * `swapXToY` → `protocolFeeY` (the side that COLLECTS the fee on X→Y is Y)
//! * `swapYToX` → `protocolFeeX`
//!
//! ```text
//! 0  tradingPair          (writable)
//! 1  mintX                (readonly)
//! 2  mintY                (readonly)
//! 3  reserveX             (writable)
//! 4  reserveY             (writable)
//! 5  userTokenAccountX    (writable)
//! 6  userTokenAccountY    (writable)
//! 7  protocolFeeY (X→Y)   (writable)         ← differs between variants
//!    OR protocolFeeX (Y→X)
//! 8  xPriceFeed           (readonly)
//! 9  yPriceFeed           (readonly)
//! 10 user                 (signer, readonly)
//! 11 tokenProgram         (readonly)
//! ```
//!
//! ## Data layout
//!
//! ```text
//! [discriminator: 8 bytes][input_amt: u64 LE][min_output_amt: u64 LE]
//! ```

use std::sync::OnceLock;

use arb_core::{ArbError, ArbResult};
use solana_program::hash::hash;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use crate::quoter::OBRIC_V2_PROGRAM;

/// SPL Token program — needed by the swap to perform the token transfers.
/// Re-export from arb-dex-raydium where it's already pinned.
pub use arb_dex_raydium::TOKEN_PROGRAM;

/// 8-byte Anchor discriminator for `global:{name}`.
fn anchor_discriminator(name: &str) -> [u8; 8] {
    let preimage = format!("global:{name}");
    let h = hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut d = [0u8; 8];
    d.copy_from_slice(&bytes[..8]);
    d
}

/// Hard-coded discriminator for Obric V2's `swap` instruction (the generic
/// one with a `is_x_to_y: bool` first byte in the payload). Captured from
/// a real on-chain swap tx via `bin/obric-trace` 2026-05-10.
///
/// We previously tried `swap2` (disc `41 4b 3f 4c eb 5b 5b 88`) which
/// dispatched but threw `InstructionDidNotDeserialize`. Switched to the
/// canonical `swap` (disc below) — same 12-account layout, same 25-byte
/// data layout, but Anchor accepts the deserialization.
pub const OBRIC_SWAP_DISCRIMINATOR: [u8; 8] =
    [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Retained for backwards-compatible API; both directions now use the same
/// discriminator and dispatch via the bool byte in `encode_swap_data`.
pub fn swap_x_to_y_discriminator() -> [u8; 8] { OBRIC_SWAP_DISCRIMINATOR }
pub fn swap_y_to_x_discriminator() -> [u8; 8] { OBRIC_SWAP_DISCRIMINATOR }

/// 12 accounts the on-chain `swap` instruction requires.
#[derive(Debug, Clone, Copy)]
pub struct ObricSwapAccounts {
    pub trading_pair:            Pubkey,
    pub mint_x:                  Pubkey,
    pub mint_y:                  Pubkey,
    pub reserve_x:               Pubkey,
    pub reserve_y:               Pubkey,
    pub user_token_account_x:    Pubkey,
    pub user_token_account_y:    Pubkey,
    pub protocol_fee_x:          Pubkey,
    pub protocol_fee_y:          Pubkey,
    /// The mystery account at pool data offset 482 (see
    /// `ObricV2Data::slot2_account`). Required at slot 2 of the swap ix.
    pub slot2_account:           Pubkey,
    pub x_price_feed:            Pubkey,
    pub y_price_feed:            Pubkey,
    pub user:                    Pubkey,
}

/// Encode the 25-byte data buffer for a swap.
/// Layout (verified via on-chain trace 2026-05-10 — RAW bytes from a live
/// inner-CPI swap captured `01 89 68 01 00 00 00 00 00 01 00 00 00 00 00 00 00`):
/// `[disc(8) | is_x_to_y: u8 | amount_in: u64_le(8) | min_output: u64_le(8)]`
///
/// Anchor's borsh deserializer reads fields in struct-declaration order;
/// the bool sits FIRST in the on-chain `Swap2Args` struct.
fn encode_swap_data(input_amt: u64, min_output_amt: u64, is_x_to_y: bool) -> Vec<u8> {
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&OBRIC_SWAP_DISCRIMINATOR);
    data.push(if is_x_to_y { 1 } else { 0 });
    data.extend_from_slice(&input_amt.to_le_bytes());
    data.extend_from_slice(&min_output_amt.to_le_bytes());
    data
}

/// Account list, verified slot-by-slot against on-chain `swap` trace
/// 2026-05-10. Direction-INDEPENDENT (the bool byte in the data tells the
/// program the direction; account positions are fixed):
/// ```text
///  0  [W]    trading_pair
///  1  [R]    protocol_fee_y    ← always Y, regardless of direction
///  2  [R]    slot2_account     ← pool data offset 482, role unknown
///  3  [W]    reserve_x_vault
///  4  [W]    reserve_y_vault
///  5  [W]    user_token_x
///  6  [W]    user_token_y
///  7  [W]    protocol_fee_x    ← always X
///  8  [W]    x_price_feed
///  9  [W]    y_price_feed
/// 10  [W,S]  user (transfer authority)
/// 11  [R]    token_program
/// ```
fn build_metas(accs: &ObricSwapAccounts) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new(accs.trading_pair,            false),  // 0  W
        AccountMeta::new_readonly(accs.protocol_fee_y, false),  // 1  R
        AccountMeta::new_readonly(accs.slot2_account,  false),  // 2  R
        AccountMeta::new(accs.reserve_x,               false),  // 3  W
        AccountMeta::new(accs.reserve_y,               false),  // 4  W
        AccountMeta::new(accs.user_token_account_x,    false),  // 5  W
        AccountMeta::new(accs.user_token_account_y,    false),  // 6  W
        AccountMeta::new(accs.protocol_fee_x,          false),  // 7  W
        AccountMeta::new(accs.x_price_feed,            false),  // 8  W
        AccountMeta::new(accs.y_price_feed,            false),  // 9  W
        AccountMeta::new(accs.user,                    true),   // 10 W,S
        AccountMeta::new_readonly(TOKEN_PROGRAM,       false),  // 11 R
    ]
}

fn validate_amounts(input_amt: u64, accs: &ObricSwapAccounts) -> ArbResult<()> {
    if input_amt == 0 {
        return Err(ArbError::Invariant("Obric swap: input_amt must be > 0"));
    }
    if accs.user_token_account_x == accs.user_token_account_y {
        return Err(ArbError::Invariant(
            "Obric swap: user_token_account_x == user_token_account_y (would burn funds)"
        ));
    }
    Ok(())
}

/// Build an X→Y swap (data byte 0 = 1). Caller passes BOTH protocol
/// fee accounts on `ObricSwapAccounts` — the builder routes them.
pub fn build_swap_x_to_y_ix(
    accs:           &ObricSwapAccounts,
    input_x:        u64,
    min_output_amt: u64,
) -> ArbResult<Instruction> {
    validate_amounts(input_x, accs)?;
    Ok(Instruction {
        program_id: OBRIC_V2_PROGRAM,
        accounts:   build_metas(accs),
        data:       encode_swap_data(input_x, min_output_amt, true),
    })
}

/// Build a Y→X swap (data byte 0 = 0).
pub fn build_swap_y_to_x_ix(
    accs:           &ObricSwapAccounts,
    input_y:        u64,
    min_output_amt: u64,
) -> ArbResult<Instruction> {
    validate_amounts(input_y, accs)?;
    Ok(Instruction {
        program_id: OBRIC_V2_PROGRAM,
        accounts:   build_metas(accs),
        data:       encode_swap_data(input_y, min_output_amt, false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }

    fn dummy_accounts() -> ObricSwapAccounts {
        ObricSwapAccounts {
            trading_pair:         pk(1),
            mint_x:               pk(2),
            mint_y:               pk(3),
            reserve_x:            pk(4),
            reserve_y:            pk(5),
            user_token_account_x: pk(6),
            user_token_account_y: pk(7),
            protocol_fee_x:       pk(8),
            protocol_fee_y:       pk(80),
            slot2_account:        pk(82),
            x_price_feed:         pk(9),
            y_price_feed:         pk(10),
            user:                 pk(11),
        }
    }

    #[test]
    fn discriminator_matches_on_chain_capture() {
        // `swap` (generic) instruction discriminator, captured 2026-05-10.
        assert_eq!(OBRIC_SWAP_DISCRIMINATOR,
                   [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8]);
    }

    #[test]
    fn x_to_y_data_layout() {
        let ix = build_swap_x_to_y_ix(&dummy_accounts(), 1_000_000, 950_000).unwrap();
        // Layout: [8 disc | 1 is_x_to_y | 8 amount_in | 8 min_out] = 25 bytes.
        assert_eq!(ix.data.len(), 25);
        assert_eq!(&ix.data[0..8],   &OBRIC_SWAP_DISCRIMINATOR);
        assert_eq!(ix.data[8], 1, "is_x_to_y byte FIRST in payload (= 1 for X→Y)");
        assert_eq!(&ix.data[9..17],  &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[17..25], &950_000u64.to_le_bytes());
    }

    #[test]
    fn y_to_x_data_layout() {
        let ix = build_swap_y_to_x_ix(&dummy_accounts(), 2_000_000, 1_900_000).unwrap();
        assert_eq!(ix.data.len(), 25);
        assert_eq!(&ix.data[0..8],   &OBRIC_SWAP_DISCRIMINATOR);
        assert_eq!(ix.data[8], 0, "is_x_to_y byte = 0 for Y→X");
        assert_eq!(&ix.data[9..17],  &2_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[17..25], &1_900_000u64.to_le_bytes());
    }

    #[test]
    fn account_meta_count_and_signer_position() {
        let accs = dummy_accounts();
        let ix = build_swap_x_to_y_ix(&accs, 1, 0).unwrap();
        assert_eq!(ix.accounts.len(), 12);
        // Slot 10 is the user signer.
        assert_eq!(ix.accounts[10].pubkey, accs.user);
        assert!(ix.accounts[10].is_signer);
        // Exactly one signer in the instruction.
        assert_eq!(ix.accounts.iter().filter(|a| a.is_signer).count(), 1);
    }

    #[test]
    fn writability_matches_on_chain_capture() {
        let accs = dummy_accounts();
        let ix = build_swap_x_to_y_ix(&accs, 1, 0).unwrap();
        assert!(ix.accounts[0].is_writable,   "trading_pair");
        assert!(!ix.accounts[1].is_writable,  "protocol_fee_y (R)");
        assert!(!ix.accounts[2].is_writable,  "slot2_account (R)");
        assert!(ix.accounts[3].is_writable,   "reserve_x");
        assert!(ix.accounts[4].is_writable,   "reserve_y");
        assert!(ix.accounts[5].is_writable,   "user_token_x");
        assert!(ix.accounts[6].is_writable,   "user_token_y");
        assert!(ix.accounts[7].is_writable,   "protocol_fee_x (W)");
        assert!(ix.accounts[8].is_writable,   "x_price_feed");
        assert!(ix.accounts[9].is_writable,   "y_price_feed");
        assert!(ix.accounts[10].is_writable,  "user");
        assert!(ix.accounts[10].is_signer,    "user signer");
        assert!(!ix.accounts[11].is_writable, "token_program");
    }

    #[test]
    fn fee_layout_is_direction_independent() {
        let accs = dummy_accounts();
        // Same slot pubkeys regardless of direction (verified on chain
        // 2026-05-10 — both bool=0 and bool=1 traces had the same layout).
        let xy = build_swap_x_to_y_ix(&accs, 1, 0).unwrap();
        let yx = build_swap_y_to_x_ix(&accs, 1, 0).unwrap();
        assert_eq!(xy.accounts[1].pubkey, accs.protocol_fee_y);
        assert_eq!(yx.accounts[1].pubkey, accs.protocol_fee_y);
        assert_eq!(xy.accounts[7].pubkey, accs.protocol_fee_x);
        assert_eq!(yx.accounts[7].pubkey, accs.protocol_fee_x);
        assert_eq!(xy.accounts[2].pubkey, accs.slot2_account);
        assert_eq!(yx.accounts[2].pubkey, accs.slot2_account);
    }

    #[test]
    fn rejects_zero_input() {
        let r = build_swap_x_to_y_ix(&dummy_accounts(), 0, 0);
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn rejects_self_swap() {
        let mut accs = dummy_accounts();
        accs.user_token_account_y = accs.user_token_account_x;
        let r = build_swap_x_to_y_ix(&accs, 100, 0);
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn program_id_is_obric_v2() {
        let ix = build_swap_x_to_y_ix(&dummy_accounts(), 1, 0).unwrap();
        assert_eq!(ix.program_id, OBRIC_V2_PROGRAM);
    }
}
