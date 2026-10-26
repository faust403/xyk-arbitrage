//! `assert_min_profit` — second of two independent layers of profit
//! enforcement (per `INSTRUCTIONS.md` § "Zero-Loss Mechanism").
//!
//! The program exposes a single instruction: [`check_min_balance`]. It reads
//! the SPL token account passed in `ctx`, compares its `amount` against the
//! `min_balance` argument, and returns an error when `amount < min_balance`.
//! Returning an error from the last instruction in a bundle reverts the
//! entire transaction, so even if pool state shifts between off-chain
//! simulation and execution, an unprofitable trade can never complete: the
//! flash-loan repay reverts together with everything else, and the bot pays
//! only the base fee per signature.
//!
//! The program intentionally has no state, no accounts beyond the read-only
//! token account, and no CPI. Every byte of complexity here is a potential
//! safety hole.

use anchor_lang::prelude::*;
use anchor_spl::token::TokenAccount;

declare_id!("FAr3CSAHe6XHcvY3xK8tcXtTNbNrEC2pZqQ8VYy7ZkWA");

#[program]
pub mod assert_min_profit {
    use super::*;

    /// Revert if `token_account.amount < min_balance`.
    ///
    /// On success: emits a single `msg!` with the actual and minimum values
    /// for log-trace correlation; returns `Ok(())`.
    /// On failure: returns [`AssertError::ProfitBelowThreshold`], which
    /// causes the runtime to roll back the whole transaction.
    pub fn check_min_balance(
        ctx: Context<CheckMinBalance>,
        min_balance: u64,
    ) -> Result<()> {
        let actual = ctx.accounts.token_account.amount;
        require!(actual >= min_balance, AssertError::ProfitBelowThreshold);
        msg!("assert_min_profit: actual={} >= min={}", actual, min_balance);
        Ok(())
    }
}

#[derive(Accounts)]
pub struct CheckMinBalance<'info> {
    /// The SPL token account whose `amount` is being checked. Marked
    /// non-mutable on purpose — this instruction must NEVER alter program
    /// state. Anchor's `Account<TokenAccount>` constraint also rejects
    /// accounts whose owner is not the SPL Token program, providing a
    /// type-system-level guard against accidentally pointing at a
    /// look-alike data account.
    pub token_account: Account<'info, TokenAccount>,
}

#[error_code]
pub enum AssertError {
    #[msg("Final balance is below the required minimum profit threshold")]
    ProfitBelowThreshold,
}
