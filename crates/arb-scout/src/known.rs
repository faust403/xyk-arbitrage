//! Well-known mainnet token mints. Source: Solana Token List + each protocol's
//! published documentation. These are addresses, not secrets — committed by
//! design.

use arb_core::TokenMint;
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

pub const USDC: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
pub const USDT: Pubkey = pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB");
pub const SOL:  Pubkey = pubkey!("So11111111111111111111111111111111111111112");
/// Wrapped BTC (Wormhole-bridged). 8 decimals. Most-used BTC mint on Solana.
/// Other variants on chain: cbBTC (Coinbase), tBTC (Threshold) — not supported.
pub const WBTC: Pubkey = pubkey!("3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh");
// CASH = Token-2022 regulated stablecoin (6 decimals).
// Has transferHook + permanentDelegate extensions — must be handled as Token-2022
// in any direct CPI; Jupiter swap_instructions already injects the right accounts.
pub const CASH: Pubkey = pubkey!("CASHx9KJUStyftLFWGvEVf59SGeG9sh5FfcnZMVPCASH");

pub fn usdc_mint() -> TokenMint { TokenMint(USDC) }
pub fn usdt_mint() -> TokenMint { TokenMint(USDT) }
pub fn sol_mint()  -> TokenMint { TokenMint(SOL) }
pub fn cash_mint() -> TokenMint { TokenMint(CASH) }

/// Decimals for the well-known mints. USDC/USDT/CASH = 6, SOL = 9, WBTC = 8.
pub fn known_decimals(mint: &Pubkey) -> Option<u8> {
    if *mint == USDC || *mint == USDT || *mint == CASH { Some(6) }
    else if *mint == SOL              { Some(9) }
    else if *mint == WBTC             { Some(8) }
    else                              { None }
}

/// Raydium AMM v4 program ID. From INSTRUCTIONS.md Phase 5A.
pub const RAYDIUM_AMM_V4_PROGRAM: Pubkey =
    pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

/// Orca Whirlpool program ID. From INSTRUCTIONS.md Phase 5B.
pub const ORCA_WHIRLPOOL_PROGRAM: Pubkey =
    pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// Meteora DLMM program ID. From Meteora documentation.
pub const METEORA_DLMM_PROGRAM: Pubkey =
    pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// The 8 canonical Jito tip accounts. Every Jito bundle includes a
/// SystemProgram::Transfer to one of these — the presence of such a transfer
/// is how the correlator identifies a transaction as part of a Jito bundle.
///
/// Source: <https://jito-labs.gitbook.io/mev/searcher-resources/sending-bundles>
pub const JITO_TIP_ACCOUNTS: [Pubkey; 8] = [
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    // #6 was previously wrong (`...pivKeVBBjNs5kyqMzjqJ` — incorrect
    // suffix). Verified 2026-05-10 against `docs.jito.wtf/lowlatencytxnsend`;
    // 12.5% of bundles were being rejected with "Bundles must write lock at
    // least one tip account to be eligible for the auction."
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

pub fn is_jito_tip_account(pk: &Pubkey) -> bool {
    JITO_TIP_ACCOUNTS.iter().any(|a| a == pk)
}

/// Helius Sender tip accounts. Helius requires a tip of at least
/// 200_000 lamports to one of these wallets for the stake-weighted
/// private-routing path (Business+ tier). Without it, /fast endpoint
/// returns -32602 "transaction must send a tip of at least 200000
/// lamports". Source: the error message itself (verified 2026-05-11).
pub const HELIUS_TIP_ACCOUNTS: [Pubkey; 11] = [
    pubkey!("4ACfpUFoaSD9bfPdeu6DBt89gB6ENTeHBXCAi87NhDEE"),
    pubkey!("2nyhqdwKcJZR2vcqCyrYsaPVdAnFoJjiksCXJ7hfEYgD"),
    pubkey!("3KCKozbAaF75qEU33jtzozcJ29yJuaLJTy2jFdzUY8bT"),
    pubkey!("4TQLFNWK8AovT1gFvda5jfw2oJeRMKEmw7aH6MGBJ3or"),
    pubkey!("9bnz4RShgq1hAnLnZbP8kbgBg1kEmcJBYQq3gQbmnSta"),
    pubkey!("5VY91ws6B2hMmBFRsXkoAAdsPHBJwRfBht4DXox3xkwn"),
    pubkey!("D2L6yPZ2FmmmTKPgzaMKdhu6EWZcTpLy1Vhx8uvZe7NZ"),
    pubkey!("2q5pghRs6arqVjRvT5gfgWfWcHWmw1ZuCzphgd5KfWGJ"),
    pubkey!("D1Mc6j9xQWgR1o1Z7yU5nVVXFQiAYx7FG9AW1aVfwrUM"),
    pubkey!("wyvPkWjVZz1M8fHQnMMCDTQDbkManefNNhweYk5WkcF"),
    pubkey!("4vieeGHPYPG2MmyPRcYjdiDmmhN3ww7hsFNap8pVN3Ey"),
];

pub const HELIUS_SENDER_MIN_TIP_LAMPORTS: u64 = 200_000;

/// System Program — used to identify SystemProgram::Transfer instructions.
pub const SYSTEM_PROGRAM: Pubkey =
    pubkey!("11111111111111111111111111111111");

/// Jupiter v6 aggregator program ID. Most MEV bundles that route across
/// multiple DEXs pass through this program — the correlator counts how many
/// competing bundles use it, as a hint about how much value comes from
/// Jupiter-routed paths vs custom router paths.
pub const JUPITER_V6_PROGRAM: Pubkey =
    pubkey!("JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4");

/// AlphaQ DEX program ID — identified via bitquery/solana-idl-lib but
/// no full account struct is publicly documented yet. Updates from this
/// program land in `no_parser_claimed`; the dedicated counter below lets
/// us track how often we'd benefit from adding a parser.
///
/// The bitquery IDL only includes the `Swap` instruction (12 accounts:
/// user, market, market_state, user_token_a/b, vault_token_a/b,
/// authorities, vendor_key, ...) but NOT the account-state layout. Writing
/// a parser requires reverse-engineering from on-chain bytes — deferred.
pub const ALPHAQ_PROGRAM: Pubkey =
    pubkey!("ALPHAQmeA7bjrVuccPsYPiCvsi428SNwte66Srvs4pHA");

/// All known-but-unparsed DEX programs. The scout counts updates from
/// these separately so the report can quantify "how much MEV are we
/// blind to because we recognise the program but can't quote it".
pub const KNOWN_UNPARSED_DEX_PROGRAMS: &[Pubkey] = &[
    ALPHAQ_PROGRAM,
];

pub fn is_known_unparsed_dex(program_id: &Pubkey) -> bool {
    KNOWN_UNPARSED_DEX_PROGRAMS.iter().any(|p| p == program_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_decimals_returns_for_known_and_none_for_unknown() {
        assert_eq!(known_decimals(&USDC), Some(6));
        assert_eq!(known_decimals(&USDT), Some(6));
        assert_eq!(known_decimals(&CASH), Some(6));
        assert_eq!(known_decimals(&SOL),  Some(9));
        assert_eq!(known_decimals(&Pubkey::new_from_array([0u8; 32])), None);
    }

    #[test]
    fn jito_tip_accounts_are_distinct() {
        let set: std::collections::HashSet<_> = JITO_TIP_ACCOUNTS.iter().collect();
        assert_eq!(set.len(), 8);
    }

    #[test]
    fn is_jito_tip_account_recognises_canonical_set() {
        for tip in &JITO_TIP_ACCOUNTS {
            assert!(is_jito_tip_account(tip));
        }
        assert!(!is_jito_tip_account(&Pubkey::new_from_array([0u8; 32])));
    }
}
