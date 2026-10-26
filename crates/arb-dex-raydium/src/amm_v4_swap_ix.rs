//! Native Raydium AMM v4 `SwapBaseIn` instruction builder.
//!
//! Builds the exact instruction Raydium's program expects against a specific
//! pool, bypassing Jupiter. This is the foundation for native arb execution:
//! scout finds edge in pool A vs pool B, the executor builds two
//! `SwapBaseIn` instructions and bundles them atomically via Jito.
//!
//! ## Account order — do not rearrange
//!
//! Raydium's program reads accounts by INDEX. The order below comes from
//! `raydium-amm` `processor.rs::process_swap_base_in`. Reordering will
//! silently route funds to the wrong vault.

use arb_core::{ArbError, ArbResult};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use crate::program_ids::{RAYDIUM_AMM_V4_PROGRAM, TOKEN_PROGRAM};

/// Raydium's instruction discriminator for `SwapBaseIn`.
pub const SWAP_BASE_IN_DISCRIMINATOR: u8 = 9;

/// Every Raydium AMM pool's authority is the SAME PDA: derived once from the
/// program ID with the seed `b"amm authority"` and a known nonce. The nonce
/// happens to be 252 across all v4 pools (verified empirically). We hard-code
/// the resulting Pubkey to skip the `find_program_address` cost on hot path.
pub const RAYDIUM_AMM_AUTHORITY: Pubkey =
    solana_program::pubkey!("5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1");

/// All 18 accounts a `SwapBaseIn` requires, in the exact order Raydium's
/// program reads them.
pub struct RaydiumSwapAccounts {
    pub amm:                       Pubkey,
    pub amm_open_orders:           Pubkey,
    pub amm_target_orders:         Pubkey,
    pub pool_coin_token_account:   Pubkey,
    pub pool_pc_token_account:     Pubkey,
    pub serum_program:             Pubkey,
    pub serum_market:              Pubkey,
    pub serum_bids:                Pubkey,
    pub serum_asks:                Pubkey,
    pub serum_event_queue:         Pubkey,
    pub serum_coin_vault:          Pubkey,
    pub serum_pc_vault:            Pubkey,
    pub serum_vault_signer:        Pubkey,
    pub user_source_token_account: Pubkey,
    pub user_dest_token_account:   Pubkey,
    pub user_owner:                Pubkey,
}

/// Build the `SwapBaseIn` instruction.
pub fn build_swap_base_in_ix(
    accs:           &RaydiumSwapAccounts,
    amount_in:      u64,
    min_amount_out: u64,
) -> ArbResult<Instruction> {
    if amount_in == 0 {
        return Err(ArbError::Invariant("SwapBaseIn: amount_in must be > 0"));
    }
    if accs.user_source_token_account == accs.user_dest_token_account {
        return Err(ArbError::Invariant(
            "SwapBaseIn: source == destination (would burn funds)"
        ));
    }

    let mut data = Vec::with_capacity(17);
    data.push(SWAP_BASE_IN_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    let metas = vec![
        AccountMeta::new_readonly(TOKEN_PROGRAM,                  false), // 0
        AccountMeta::new(accs.amm,                                false), // 1
        AccountMeta::new_readonly(RAYDIUM_AMM_AUTHORITY,          false), // 2
        AccountMeta::new(accs.amm_open_orders,                    false), // 3
        AccountMeta::new(accs.amm_target_orders,                  false), // 4
        AccountMeta::new(accs.pool_coin_token_account,            false), // 5
        AccountMeta::new(accs.pool_pc_token_account,              false), // 6
        AccountMeta::new_readonly(accs.serum_program,             false), // 7
        AccountMeta::new(accs.serum_market,                       false), // 8
        AccountMeta::new(accs.serum_bids,                         false), // 9
        AccountMeta::new(accs.serum_asks,                         false), // 10
        AccountMeta::new(accs.serum_event_queue,                  false), // 11
        AccountMeta::new(accs.serum_coin_vault,                   false), // 12
        AccountMeta::new(accs.serum_pc_vault,                     false), // 13
        AccountMeta::new_readonly(accs.serum_vault_signer,        false), // 14
        AccountMeta::new(accs.user_source_token_account,          false), // 15
        AccountMeta::new(accs.user_dest_token_account,            false), // 16
        AccountMeta::new_readonly(accs.user_owner,                true),  // 17 — signer
    ];

    Ok(Instruction {
        program_id: RAYDIUM_AMM_V4_PROGRAM,
        accounts:   metas,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_accounts() -> RaydiumSwapAccounts {
        let pk = |b: u8| Pubkey::new_from_array([b; 32]);
        RaydiumSwapAccounts {
            amm:                       pk(1),
            amm_open_orders:           pk(2),
            amm_target_orders:         pk(3),
            pool_coin_token_account:   pk(4),
            pool_pc_token_account:     pk(5),
            serum_program:             pk(6),
            serum_market:              pk(7),
            serum_bids:                pk(8),
            serum_asks:                pk(9),
            serum_event_queue:         pk(10),
            serum_coin_vault:          pk(11),
            serum_pc_vault:            pk(12),
            serum_vault_signer:        pk(13),
            user_source_token_account: pk(14),
            user_dest_token_account:   pk(15),
            user_owner:                pk(16),
        }
    }

    #[test]
    fn data_layout_matches_spec() {
        let ix = build_swap_base_in_ix(&dummy_accounts(), 1_000_000, 950_000).unwrap();
        assert_eq!(ix.data.len(), 17);
        assert_eq!(ix.data[0], SWAP_BASE_IN_DISCRIMINATOR);
        assert_eq!(&ix.data[1..9],  &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[9..17], &950_000u64.to_le_bytes());
    }

    #[test]
    fn account_order_and_writability_match_processor_layout() {
        let accs = dummy_accounts();
        let ix = build_swap_base_in_ix(&accs, 1, 0).unwrap();
        assert_eq!(ix.accounts.len(), 18);
        assert_eq!(ix.accounts[0].pubkey, TOKEN_PROGRAM);
        assert!(!ix.accounts[0].is_writable);
        assert_eq!(ix.accounts[1].pubkey, accs.amm);
        assert!(ix.accounts[1].is_writable);
        assert_eq!(ix.accounts[2].pubkey, RAYDIUM_AMM_AUTHORITY);
        assert!(!ix.accounts[2].is_writable);
        assert!(!ix.accounts[7].is_writable);
        assert!(ix.accounts[8].is_writable);
        assert_eq!(ix.accounts[17].pubkey, accs.user_owner);
        assert!(ix.accounts[17].is_signer);
        assert_eq!(ix.accounts.iter().filter(|a| a.is_signer).count(), 1);
    }

    #[test]
    fn rejects_zero_amount_in() {
        let r = build_swap_base_in_ix(&dummy_accounts(), 0, 0);
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn rejects_self_swap() {
        let mut accs = dummy_accounts();
        accs.user_dest_token_account = accs.user_source_token_account;
        let r = build_swap_base_in_ix(&accs, 100, 0);
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn program_id_is_raydium_amm_v4() {
        let ix = build_swap_base_in_ix(&dummy_accounts(), 1, 0).unwrap();
        assert_eq!(ix.program_id, RAYDIUM_AMM_V4_PROGRAM);
    }
}
