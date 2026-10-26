//! Program IDs and well-known constants used by the Raydium quoters/builders.

use solana_program::pubkey::Pubkey;

/// Raydium AMM v4 program ID.
pub const RAYDIUM_AMM_V4_PROGRAM: Pubkey =
    solana_program::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

/// SPL Token program — needed by swap instructions for the underlying transfers.
pub const TOKEN_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Raydium CPMM (constant-product market-maker) program — newer than AMM v4,
/// owns pools that AMM v4 cannot decode. Required after we observed
/// `JTO/SOL` listed on GeckoTerminal as Raydium but bootstrap-rejected
/// because the account owner was CPMM, not AMM v4.
pub const RAYDIUM_CPMM_PROGRAM: Pubkey =
    solana_program::pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");

// `RAYDIUM_CLMM_PROGRAM` is owned by `crate::clmm` and re-exported via lib.rs.
