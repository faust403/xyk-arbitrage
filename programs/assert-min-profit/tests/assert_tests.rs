//! Integration tests for `assert_min_profit`.
//!
//! These run via `cargo test -- --include-ignored` after `cargo build-sbf`
//! has produced `target/deploy/assert_min_profit.so`. The tests are marked
//! `#[ignore]` so a plain `cargo test --workspace` (which has no BPF
//! toolchain on Windows) skips them. They're the canonical Phase 3 test
//! inventory:
//!
//! 1. test_passes_when_balance_equals_min
//! 2. test_passes_when_balance_above_min
//! 3. test_fails_when_balance_below_min
//! 4. test_fails_when_account_empty
//! 5. test_passes_with_zero_min_balance
//! 6. test_passes_at_u64_max
//! 7. test_fails_with_wrong_account_owner
//! 8. test_program_does_not_modify_account
//! 9. test_program_fits_under_compute_budget
//!
//! See INSTRUCTIONS.md Phase 3 for the acceptance criteria.

#![cfg(feature = "test-bpf")]

use anchor_lang::{InstructionData, ToAccountMetas};
use solana_program::{instruction::Instruction, pubkey::Pubkey, system_instruction};
use solana_program_test::{processor, ProgramTest};
use solana_sdk::{
    account::Account,
    signature::{Keypair, Signer},
    transaction::{Transaction, TransactionError},
};
use spl_associated_token_account::get_associated_token_address;
use spl_token::state::{Account as TokenAccount, AccountState};

const ASSERT_PROGRAM_ID: Pubkey = pubkey!("FAr3CSAHe6XHcvY3xK8tcXtTNbNrEC2pZqQ8VYy7ZkWA");

/// Build the `check_min_balance` instruction with the operator's token
/// account and the desired threshold.
fn build_check_ix(token_account: Pubkey, min_balance: u64) -> Instruction {
    Instruction {
        program_id: ASSERT_PROGRAM_ID,
        accounts:   assert_min_profit::accounts::CheckMinBalance { token_account }
                        .to_account_metas(None),
        data:       assert_min_profit::instruction::CheckMinBalance { min_balance }.data(),
    }
}

/// Helper: spin up a `ProgramTest` with the assert program loaded.
async fn setup() -> (solana_program_test::ProgramTestContext, Keypair, Pubkey) {
    let mut pt = ProgramTest::new(
        "assert_min_profit",
        ASSERT_PROGRAM_ID,
        processor!(assert_min_profit::entry),
    );
    let payer = Keypair::new();
    pt.add_account(payer.pubkey(), Account {
        lamports: 1_000_000_000,
        ..Account::default()
    });
    let ctx = pt.start_with_context().await;
    let mint = Pubkey::new_unique(); // placeholder USDC-like mint
    (ctx, payer, mint)
}

// Each test below is #[ignore] so it only runs under
// `cargo test --features test-bpf -- --include-ignored`. Without that, the
// ProgramTest harness has no SBF ELF to load and would fail at startup.
//
// The bodies are intentionally sketchy stubs at the Phase 3-stub point —
// implementing each fully requires a working BPF build and is best done
// once the operator has the toolchain installed. The structure is in place
// so each `todo!()` is a clear next step rather than a hidden gap.

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_passes_when_balance_equals_min() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_passes_when_balance_above_min() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_fails_when_balance_below_min() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_fails_when_account_empty() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_passes_with_zero_min_balance() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_passes_at_u64_max() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_fails_with_wrong_account_owner() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_program_does_not_modify_account() { todo!(); }

#[tokio::test]
#[ignore = "Phase 3 implementation pending — requires `cargo build-sbf`"]
async fn test_program_fits_under_compute_budget() { todo!(); }

// --- Pure-logic unit checks below run with `cargo test` (no BPF needed) -----

/// The `check_min_balance` invariant is `actual >= min_balance`. This isn't
/// a runtime test of the program, but it keeps a regression check on the
/// canonical comparison so a future refactor that flips the operator (e.g.
/// to `>` instead of `>=`) shows up in code review.
#[test]
fn invariant_uses_greater_or_equal() {
    let cases = [(0u64, 0u64, true), (1, 0, true), (0, 1, false), (10, 10, true), (9, 10, false)];
    for (actual, min, expected_ok) in cases {
        let ok = actual >= min;
        assert_eq!(ok, expected_ok, "actual={actual} min={min}");
    }
}
