//! Meteora DLMM (Dynamic Liquidity Market Maker) — binned liquidity.
//!
//! Phase 12 (Wave 2). Source: <https://github.com/MeteoraAg/dlmm-sdk>.
//!
//! DLMM quote math walks active bins applying per-bin fees. Unlike Whirlpool
//! tick arrays, DLMM bins are individually-owned PDAs that must be fetched
//! and passed in `remaining_accounts`. The crate keeps a per-pool bin cache
//! to avoid round-trip reads on the hot path.

use arb_core::TokenMint;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

pub mod dlmm;
pub use dlmm::*;

pub const METEORA_DLMM_PROGRAM: Pubkey =
    pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// Decimals for the well-known mints. USDC/USDT = 6, SOL = 9.
/// Used by the DLMM quoter when the pool account doesn't itself carry the
/// info; mirrors `arb-scout::known::known_decimals` so the move stays
/// behavior-preserving.
pub fn known_decimals(mint: &Pubkey) -> Option<u8> {
    const USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
    const USDT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
    const SOL:  Pubkey = pubkey!("So11111111111111111111111111111111111111112");
    if *mint == USDC || *mint == USDT { Some(6) }
    else if *mint == SOL              { Some(9) }
    else                              { None }
}

/// Re-export for callers that want a typed mint.
pub fn known_decimals_for(mint: &TokenMint) -> Option<u8> { known_decimals(&mint.0) }
