use solana_program::pubkey::Pubkey;
use thiserror::Error;

/// Unified error type for the arbbot workspace.
///
/// Variants cover every layer (config, RPC, parsing, quoting, flash loans,
/// simulation). New variants are added as needed; downstream code matches
/// exhaustively.
#[derive(Debug, Error)]
pub enum ArbError {
    #[error("pool not found: {0}")]
    PoolNotFound(Pubkey),

    #[error("insufficient liquidity for amount {amount} in pool {pool}")]
    InsufficientLiquidity { pool: Pubkey, amount: u64 },

    #[error("quote computation overflow")]
    QuoteOverflow,

    #[error("quote computation underflow")]
    QuoteUnderflow,

    #[error("flash loan repayment shortfall: have {have}, need {need}")]
    RepaymentShortfall { have: u64, need: u64 },

    #[error("simulation failed: {0}")]
    SimulationFailed(String),

    #[error("rpc error: {0}")]
    Rpc(String),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("account parse error in {dex}: {reason}")]
    AccountParse { dex: &'static str, reason: String },

    #[error("token mint mismatch: expected {expected}, got {actual}")]
    MintMismatch { expected: Pubkey, actual: Pubkey },

    #[error("decimals mismatch: expected {expected}, got {actual}")]
    DecimalsMismatch { expected: u8, actual: u8 },

    #[error("not implemented: {0}")]
    NotImplemented(&'static str),

    #[error("invariant violated: {0}")]
    Invariant(&'static str),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type ArbResult<T> = Result<T, ArbError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_arb_error_display_messages() {
        // Every variant must format without panicking. Spot-check a few that
        // carry data to confirm the Display impl substitutes correctly.
        let pk = Pubkey::new_from_array([1u8; 32]);
        let cases: Vec<(ArbError, &str)> = vec![
            (ArbError::PoolNotFound(pk), "pool not found"),
            (
                ArbError::InsufficientLiquidity { pool: pk, amount: 42 },
                "insufficient liquidity",
            ),
            (ArbError::QuoteOverflow, "overflow"),
            (ArbError::QuoteUnderflow, "underflow"),
            (
                ArbError::RepaymentShortfall { have: 1, need: 2 },
                "shortfall",
            ),
            (ArbError::SimulationFailed("x".into()), "simulation failed"),
            (ArbError::Rpc("x".into()), "rpc error"),
            (ArbError::WebSocket("x".into()), "websocket"),
            (ArbError::Config("x".into()), "config"),
            (
                ArbError::AccountParse { dex: "raydium", reason: "x".into() },
                "raydium",
            ),
            (
                ArbError::MintMismatch { expected: pk, actual: pk },
                "mint mismatch",
            ),
            (
                ArbError::DecimalsMismatch { expected: 6, actual: 9 },
                "decimals mismatch",
            ),
            (ArbError::NotImplemented("x"), "not implemented"),
            (ArbError::Invariant("x"), "invariant"),
        ];
        for (err, needle) in cases {
            let s = format!("{err}");
            assert!(s.to_lowercase().contains(needle), "{s} missing {needle}");
        }
    }
}
