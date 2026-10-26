//! Native Raydium CLMM `swap` instruction builder (V1).
//!
//! Raydium CLMM is a Uniswap-V3 fork — concentrated liquidity, ticks, the
//! whole stack. The instruction layout differs from Whirlpool in two ways:
//!
//! 1. `tick_array_*` PDAs use `start_tick_index.to_le_bytes()` (4 raw bytes)
//!    as a seed, NOT the ASCII string Whirlpool uses.
//! 2. Each tick array spans `60 * tick_spacing` ticks (Whirlpool: 88).
//! 3. Only ONE tick array is named in the instruction; if the swap crosses
//!    out, additional arrays are passed as `remaining_accounts`. For our
//!    small probe sizes we pass only the current tick array — bundle reverts
//!    atomically if more are needed (no fund loss, only tip wasted).
//! 4. The pool stores its own `observation_key` (read directly from state),
//!    no PDA derivation needed.

use arb_core::{ArbError, ArbResult};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use crate::clmm::RAYDIUM_CLMM_PROGRAM;
use crate::program_ids::TOKEN_PROGRAM;

/// Anchor discriminator for `global:swap`. Same byte sequence as Whirlpool's
/// (Anchor's `sha256("global:swap")[0..8]` is identical regardless of program).
pub const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Number of ticks per `TickArray` account in Raydium CLMM.
pub const TICK_ARRAY_SIZE: i32 = 60;

/// Same Q64.64 sqrt-price bounds Whirlpool uses (ticks have the same range).
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// Compute the tick-array `start_tick_index` containing the given tick.
/// Each tick array spans `60 * tick_spacing` ticks.
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let span = TICK_ARRAY_SIZE * tick_spacing as i32;
    let mut div = tick / span;
    if tick < 0 && tick % span != 0 { div -= 1; }
    div * span
}

/// Derive a CLMM tick-array PDA. Verified 2026-05-10 against Raydium CLMM
/// source `programs/amm/src/states/tick_array.rs` — seeds are
/// `["tick_array", pool_id, start_tick_index.to_be_bytes()]`.
///
/// **Important:** the prefix is `"tick_array"` (NOT `"pool_tick_array"`)
/// and the index is BIG-endian (NOT little-endian). An earlier version
/// had both wrong, producing PDAs that didn't exist on chain → Anchor
/// `ConstraintRaw` (Custom 3007) on every CLMM swap because the empty
/// PDA's owner = System Program ≠ CLMM program.
pub fn derive_tick_array_pda(pool: &Pubkey, start_tick: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start_tick.to_be_bytes()],
        &RAYDIUM_CLMM_PROGRAM,
    ).0
}

pub struct RaydiumClmmSwapAccounts {
    pub payer:                 Pubkey,   // signer + writable (fee payer)
    pub amm_config:            Pubkey,
    pub pool_state:            Pubkey,
    pub input_token_account:   Pubkey,
    pub output_token_account:  Pubkey,
    pub input_vault:           Pubkey,
    pub output_vault:          Pubkey,
    pub observation_state:     Pubkey,
    pub tick_array:            Pubkey,
    /// Optional adjacent tick arrays (next in swap direction). Pass `None`
    /// for swaps that stay inside one tick_array; pass `Some` when the
    /// swap may cross boundaries. Raydium CLMM expects up to 3 tick_arrays
    /// as remaining accounts; passing fewer than needed reverts with
    /// AnchorError::NotEnoughTickArrayAccount (6027).
    pub tick_array_next:       Option<Pubkey>,
    pub tick_array_after:      Option<Pubkey>,
}

/// Build the `swap` instruction (V1).
/// `amount_in` is the input amount; `min_amount_out` is the slippage floor
/// (the program reverts if it would deliver less). `zero_for_one = true`
/// means we're selling token_0 for token_1 (price decreases).
pub fn build_swap_ix(
    accs:           &RaydiumClmmSwapAccounts,
    amount_in:      u64,
    min_amount_out: u64,
    zero_for_one:   bool,
) -> ArbResult<Instruction> {
    if amount_in == 0 {
        return Err(ArbError::Invariant("CLMM swap: amount_in must be > 0"));
    }

    // Direction-dependent sqrt_price_limit (effectively "no further bound").
    // Raydium CLMM uses a STRICT inequality at swap.rs:144:
    //   zero_for_one  → require sqrt_price_limit > MIN_SQRT_PRICE_X64
    //   !zero_for_one → require sqrt_price_limit < MAX_SQRT_PRICE_X64
    // Passing exactly the boundary trips Custom(6013) SqrtPriceLimitOverflow.
    // Offset by 1 to satisfy the strict bound while remaining effectively
    // unbounded for any real swap. (Whirlpool's check is inclusive, so its
    // IX builder passes the raw boundary — different program, different rule.)
    let sqrt_price_limit: u128 = if zero_for_one {
        MIN_SQRT_PRICE_X64 + 1
    } else {
        MAX_SQRT_PRICE_X64 - 1
    };

    // Data: discriminator (8) + amount (8) + threshold (8) + limit (16) +
    // is_base_input (1) = 41 bytes. (`zero_for_one` is implicit from which
    // vault is `input_vault`; it's NOT in the args.)
    let mut data = Vec::with_capacity(41);
    data.extend_from_slice(&SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8);  // is_base_input = true (we specify input amount)

    // Account order matches Raydium CLMM's `Swap` instruction. Indices
    // 9..N are tick_arrays — the program walks them in order until the
    // swap completes. Passing only 1 reverts with NotEnoughTickArrayAccount
    // (6027) whenever the swap crosses a tick-array boundary.
    let mut metas = vec![
        AccountMeta::new(accs.payer,                       true),  // 0 — signer
        AccountMeta::new_readonly(accs.amm_config,         false), // 1
        AccountMeta::new(accs.pool_state,                  false), // 2
        AccountMeta::new(accs.input_token_account,         false), // 3
        AccountMeta::new(accs.output_token_account,        false), // 4
        AccountMeta::new(accs.input_vault,                 false), // 5
        AccountMeta::new(accs.output_vault,                false), // 6
        AccountMeta::new(accs.observation_state,           false), // 7
        AccountMeta::new_readonly(TOKEN_PROGRAM,           false), // 8
        AccountMeta::new(accs.tick_array,                  false), // 9 — first tick array
    ];
    if let Some(t2) = accs.tick_array_next {
        metas.push(AccountMeta::new(t2, false));
    }
    if let Some(t3) = accs.tick_array_after {
        metas.push(AccountMeta::new(t3, false));
    }

    Ok(Instruction {
        program_id: RAYDIUM_CLMM_PROGRAM,
        accounts:   metas,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }
    fn dummy_accs() -> RaydiumClmmSwapAccounts {
        RaydiumClmmSwapAccounts {
            payer:                 pk(1),
            amm_config:            pk(2),
            pool_state:            pk(3),
            input_token_account:   pk(4),
            output_token_account:  pk(5),
            input_vault:           pk(6),
            output_vault:          pk(7),
            observation_state:     pk(8),
            tick_array:            pk(9),
            tick_array_next:       None,
            tick_array_after:      None,
        }
    }

    #[test]
    fn data_layout_is_41_bytes() {
        let ix = build_swap_ix(&dummy_accs(), 1_000, 950, true).unwrap();
        assert_eq!(ix.data.len(), 41);
        assert_eq!(&ix.data[..8], &SWAP_DISCRIMINATOR);
        assert_eq!(&ix.data[8..16],  &1_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &950u64.to_le_bytes());
        assert_eq!(&ix.data[24..40], &(MIN_SQRT_PRICE_X64 + 1).to_le_bytes());
        assert_eq!(ix.data[40], 1);
    }

    #[test]
    fn one_for_zero_uses_max_sqrt_price() {
        let ix = build_swap_ix(&dummy_accs(), 1_000, 950, false).unwrap();
        assert_eq!(&ix.data[24..40], &(MAX_SQRT_PRICE_X64 - 1).to_le_bytes());
    }

    #[test]
    fn account_order_and_signer() {
        let accs = dummy_accs();
        let ix = build_swap_ix(&accs, 1, 0, true).unwrap();
        assert_eq!(ix.accounts.len(), 10);
        // Payer is the only signer (and writable, since they pay fees)
        assert_eq!(ix.accounts[0].pubkey, accs.payer);
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[0].is_writable);
        assert_eq!(ix.accounts.iter().filter(|a| a.is_signer).count(), 1);
        // amm_config is readonly
        assert!(!ix.accounts[1].is_writable);
        // Token program at slot 8
        assert_eq!(ix.accounts[8].pubkey, TOKEN_PROGRAM);
        assert!(!ix.accounts[8].is_writable);
    }

    #[test]
    fn rejects_zero_amount() {
        assert!(matches!(
            build_swap_ix(&dummy_accs(), 0, 0, true),
            Err(ArbError::Invariant(_))
        ));
    }

    #[test]
    fn tick_array_start_basic() {
        // tick_spacing=60, span=60*60=3600
        assert_eq!(tick_array_start_index(0,    60), 0);
        assert_eq!(tick_array_start_index(3599, 60), 0);
        assert_eq!(tick_array_start_index(3600, 60), 3600);
        assert_eq!(tick_array_start_index(-1,   60), -3600);
        assert_eq!(tick_array_start_index(-3600,60), -3600);
        assert_eq!(tick_array_start_index(-3601,60), -7200);
    }

    #[test]
    fn pdas_are_deterministic_and_distinct_per_seed() {
        let pool = pk(99);
        let a = derive_tick_array_pda(&pool, 0);
        let b = derive_tick_array_pda(&pool, 0);
        assert_eq!(a, b);
        let c = derive_tick_array_pda(&pool, 3600);
        assert_ne!(a, c);
    }
}
