//! Well-known mainnet token mints used by the route builder.
//!
//! Mirrors `arb-scout::known` — addresses are committed by design (no
//! secrets). Kept here so the pathfinder doesn't need to depend on the
//! scout crate.

use arb_core::TokenMint;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

pub const USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
pub const SOL:  Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub fn usdc_mint() -> TokenMint { TokenMint(USDC) }
pub fn usdt_mint() -> TokenMint { TokenMint(USDT) }
pub fn sol_mint()  -> TokenMint { TokenMint(SOL) }
