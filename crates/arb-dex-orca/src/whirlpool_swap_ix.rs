//! Native Orca Whirlpool `swap` instruction builder.
//!
//! Whirlpool is Solana's Uniswap-V3 — concentrated liquidity, tick-based.
//! The swap instruction needs:
//! - the whirlpool account
//! - both vaults + both user ATAs
//! - up to 3 `TickArray` accounts that span the price range the swap may
//!   touch (PDA-derived from whirlpool address + start tick index)
//! - the whirlpool's `oracle` PDA
//!
//! ## Tick-array selection (the hard part)
//!
//! Each `TickArray` covers `88 * tick_spacing` ticks. A swap that stays
//! INSIDE the current tick array needs only `tick_array_0`; one that
//! crosses out the side may need 1-2 more. Whirlpool requires exactly 3
//! tick array accounts in the instruction — but they are allowed to be
//! the same account passed multiple times (Whirlpool dedupes internally).
//!
//! Direction matters:
//! - **a→b** (selling A, price goes DOWN, tick decreases) ⇒ next arrays
//!   are at LOWER tick indices.
//! - **b→a** (selling B, price goes UP, tick increases) ⇒ HIGHER indices.
//!
//! For our `$750` probe in deep ($1M+ TVL) pools the swap stays in the
//! current tick array nine times out of ten. We pre-load the current array
//! AND the next one in the swap direction; if the swap actually crosses
//! more, the bundle reverts (atomic) and we lose just the tip — the same
//! risk profile as Raydium native.

use arb_core::{ArbError, ArbResult};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use arb_dex_raydium::TOKEN_PROGRAM;
use crate::ORCA_WHIRLPOOL_PROGRAM;

/// Anchor discriminator for `global:swap` (the v1 swap; not `swapV2`).
/// Computed as `sha256("global:swap")[0..8]`.
pub const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Number of ticks per `TickArray` account (Whirlpool constant).
pub const TICKS_PER_ARRAY: i32 = 88;

/// The smallest price Whirlpool will swap to (Q64.64). Used as the
/// `sqrt_price_limit` for an A→B swap when we want "no further bound".
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;

/// The largest price Whirlpool will swap to (Q64.64). Used as the
/// `sqrt_price_limit` for a B→A swap when we want "no further bound".
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// Compute the tick-array `start_tick_index` containing the given tick.
/// Each tick array spans `88 * tick_spacing` ticks; the start index is the
/// floor (toward negative infinity) of `current_tick / span`, multiplied
/// back. Negative ticks need careful rounding because Rust's `/` rounds
/// toward zero.
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let span = TICKS_PER_ARRAY * tick_spacing as i32;
    let mut div = tick / span;
    if tick < 0 && tick % span != 0 { div -= 1; }
    div * span
}

/// Derive a tick array PDA. Seeds: `["tick_array", whirlpool, ascii(start_tick)]`.
/// The start tick is encoded as a decimal ASCII string (no leading zero, no padding).
pub fn derive_tick_array_pda(whirlpool: &Pubkey, start_tick: i32) -> Pubkey {
    let s = start_tick.to_string();
    Pubkey::find_program_address(
        &[b"tick_array", whirlpool.as_ref(), s.as_bytes()],
        &ORCA_WHIRLPOOL_PROGRAM,
    ).0
}

/// Derive the per-pool `oracle` PDA. Seeds: `["oracle", whirlpool]`.
pub fn derive_oracle_pda(whirlpool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"oracle", whirlpool.as_ref()],
        &ORCA_WHIRLPOOL_PROGRAM,
    ).0
}

/// All accounts a Whirlpool swap requires, in the exact order the program reads them.
pub struct WhirlpoolSwapAccounts {
    pub whirlpool:      Pubkey,
    pub token_vault_a:  Pubkey,
    pub token_vault_b:  Pubkey,
    pub user_token_a:   Pubkey,   // user ATA for token A
    pub user_token_b:   Pubkey,   // user ATA for token B
    pub tick_array_0:   Pubkey,
    pub tick_array_1:   Pubkey,
    pub tick_array_2:   Pubkey,
    pub oracle:         Pubkey,
    pub user_owner:     Pubkey,   // signer
}

/// Build a Whirlpool `swap` instruction.
/// `amount_in` is what we're putting in; `min_amount_out` is the slippage
/// floor (the swap reverts if Whirlpool would deliver less).
/// `a_to_b = true` means input mint is `token_a`, output is `token_b`.
pub fn build_swap_ix(
    accs:           &WhirlpoolSwapAccounts,
    amount_in:      u64,
    min_amount_out: u64,
    a_to_b:         bool,
) -> ArbResult<Instruction> {
    if amount_in == 0 {
        return Err(ArbError::Invariant("Whirlpool swap: amount_in must be > 0"));
    }

    // Direction-dependent sqrt_price_limit (effectively "no further bound" in
    // the swap direction; the actual safety gate is `min_amount_out`).
    let sqrt_price_limit: u128 = if a_to_b { MIN_SQRT_PRICE_X64 } else { MAX_SQRT_PRICE_X64 };

    // Data layout: discriminator (8) + amount (8) + threshold (8) +
    // sqrt_price_limit (16) + amount_specified_is_input (1) + a_to_b (1) = 42 bytes.
    let mut data = Vec::with_capacity(42);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8);                  // amount_specified_is_input = true
    data.push(if a_to_b { 1 } else { 0 });

    // Account order matches Whirlpool's `Swap` instruction in
    // `programs/whirlpool/src/instructions/swap.rs`.
    let metas = vec![
        AccountMeta::new_readonly(TOKEN_PROGRAM,            false), // 0
        AccountMeta::new_readonly(accs.user_owner,          true),  // 1 — signer
        AccountMeta::new(accs.whirlpool,                    false), // 2
        AccountMeta::new(accs.user_token_a,                 false), // 3
        AccountMeta::new(accs.token_vault_a,                false), // 4
        AccountMeta::new(accs.user_token_b,                 false), // 5
        AccountMeta::new(accs.token_vault_b,                false), // 6
        AccountMeta::new(accs.tick_array_0,                 false), // 7
        AccountMeta::new(accs.tick_array_1,                 false), // 8
        AccountMeta::new(accs.tick_array_2,                 false), // 9
        AccountMeta::new(accs.oracle,                       false), // 10
    ];

    Ok(Instruction {
        program_id: ORCA_WHIRLPOOL_PROGRAM,
        accounts:   metas,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }
    fn dummy_accs() -> WhirlpoolSwapAccounts {
        WhirlpoolSwapAccounts {
            whirlpool:    pk(1),
            token_vault_a: pk(2),
            token_vault_b: pk(3),
            user_token_a:  pk(4),
            user_token_b:  pk(5),
            tick_array_0:  pk(6),
            tick_array_1:  pk(7),
            tick_array_2:  pk(8),
            oracle:        pk(9),
            user_owner:    pk(10),
        }
    }

    #[test]
    fn data_is_42_bytes_with_correct_layout() {
        let ix = build_swap_ix(&dummy_accs(), 1_000, 950, true).unwrap();
        assert_eq!(ix.data.len(), 42);
        assert_eq!(&ix.data[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(&ix.data[8..16],  &1_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &950u64.to_le_bytes());
        assert_eq!(&ix.data[24..40], &MIN_SQRT_PRICE_X64.to_le_bytes());
        assert_eq!(ix.data[40], 1);  // amount_specified_is_input = true
        assert_eq!(ix.data[41], 1);  // a_to_b = true
    }

    #[test]
    fn b_to_a_uses_max_sqrt_price() {
        let ix = build_swap_ix(&dummy_accs(), 1_000, 950, false).unwrap();
        assert_eq!(&ix.data[24..40], &MAX_SQRT_PRICE_X64.to_le_bytes());
        assert_eq!(ix.data[41], 0);
    }

    #[test]
    fn account_order_and_signer_flags_match_program() {
        let accs = dummy_accs();
        let ix = build_swap_ix(&accs, 1, 0, true).unwrap();
        assert_eq!(ix.accounts.len(), 11);
        // Token program is read-only, not signer
        assert_eq!(ix.accounts[0].pubkey, TOKEN_PROGRAM);
        assert!(!ix.accounts[0].is_writable);
        assert!(!ix.accounts[0].is_signer);
        // user_owner is the only signer
        assert_eq!(ix.accounts[1].pubkey, accs.user_owner);
        assert!(ix.accounts[1].is_signer);
        assert!(!ix.accounts[1].is_writable);
        assert_eq!(ix.accounts.iter().filter(|a| a.is_signer).count(), 1);
        // Whirlpool is mutable
        assert_eq!(ix.accounts[2].pubkey, accs.whirlpool);
        assert!(ix.accounts[2].is_writable);
        // Oracle at the end
        assert_eq!(ix.accounts[10].pubkey, accs.oracle);
    }

    #[test]
    fn rejects_zero_amount_in() {
        let r = build_swap_ix(&dummy_accs(), 0, 0, true);
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn tick_array_start_index_basic() {
        // tick_spacing=64, span=88*64=5632
        assert_eq!(tick_array_start_index(0,    64), 0);
        assert_eq!(tick_array_start_index(5631, 64), 0);       // last in array
        assert_eq!(tick_array_start_index(5632, 64), 5632);    // first in next
        assert_eq!(tick_array_start_index(-1,   64), -5632);   // floor toward -inf
        assert_eq!(tick_array_start_index(-5632,64), -5632);
        assert_eq!(tick_array_start_index(-5633,64), -11264);
    }

    #[test]
    fn pdas_are_deterministic() {
        let wp = pk(99);
        let a = derive_tick_array_pda(&wp, 0);
        let b = derive_tick_array_pda(&wp, 0);
        assert_eq!(a, b);
        let c = derive_tick_array_pda(&wp, 5632);
        assert_ne!(a, c);

        let o1 = derive_oracle_pda(&wp);
        let o2 = derive_oracle_pda(&wp);
        assert_eq!(o1, o2);
    }
}
