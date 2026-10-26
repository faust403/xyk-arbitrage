//! Extend `EXECUTOR_PYTH_ALT` with Kamino + MarginFi-flash-specific
//! addresses so `kamino-hunter`'s flash-wrapped liquidate+swap txs
//! compress under the 1232-byte v0 limit.
//!
//! Live 2026-05-16 showed `tx_size=2224B` post multi-mint flash refactor
//! even with Jupiter `maxAccounts=12`. Refresh × N + liquidate + flash
//! envelope + Jupiter routes break the budget without ALT compression
//! covering the repeating reserve / oracle / vault accounts.
//!
//! Run once after deploying kamino-hunter:
//!
//! ```text
//! cargo run --release --bin alt-extend-kamino-flash
//! ```
//!
//! Cost: one ExtendLookupTable tx, ~0.000005 SOL. Re-runnable; the ALT
//! dedupes new entries.
//!
//! What it adds:
//! - Kamino + Kamino Farms program ids, sysvar instructions/clock
//! - MarginFi WSOL bank + its vault/authority/insurance (was missing
//!   from earlier alt-extend-marginfi-liq run)
//! - The four bank primary_oracle pubkeys (re-confirmed live on-chain
//!   2026-05-16; alt-extend-marginfi-liq had stale guesses)
//! - The eight hot Kamino reserves observed firing in production
//!   (D6q6.. USDC main, 6gTJ.. wSOL main, etc) plus per-reserve:
//!     reserve, liquidity_mint, liquidity_supply_vault,
//!     liquidity_fee_vault, collateral_mint, collateral_supply_vault,
//!     primary oracle (pyth or scope feed)
//!
//! Total ~70 addresses, well under the 256-entry ALT cap.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;
use solana_program::sysvar;

use arb_flashloan::{
    derive_insurance_vault, derive_liquidity_vault, derive_liquidity_vault_authority,
    KAMINO_LEND_PROGRAM, KAMINO_FARMS_PROGRAM,
    MARGINFI_USDC_BANK, MARGINFI_USDT_BANK, MARGINFI_WBTC_BANK, MARGINFI_WSOL_BANK,
    MARGINFI_JUPSOL_BANK, MARGINFI_BSOL_BANK, MARGINFI_JITOSOL_BANK, MARGINFI_MSOL_BANK,
    MARGINFI_CBBTC_BANK, MARGINFI_USD1_BANK, MARGINFI_USDS_BANK, MARGINFI_TBTC_BANK,
    MARGINFI_PYUSD_BANK,
    MARGINFI_V2_PROGRAM, MARGINFI_MAIN_GROUP,
};
use arb_scout::kamino_liq::{fetch_reserve_cache, ReserveSummary};
use arb_scout::known::{USDC, USDT, WBTC};
use arb_scout::raw_tx::{
    build_and_sign_legacy_tx_b64, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix,
};
use arb_scout::wallet::Wallet;

const ADDRESS_LOOKUP_TABLE_PROGRAM: Pubkey =
    pubkey!("AddressLookupTab1e1111111111111111111111111");
const SPL_TOKEN_PROGRAM: Pubkey =
    pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const SYSTEM_PROGRAM: Pubkey =
    pubkey!("11111111111111111111111111111111");
const WSOL: Pubkey =
    pubkey!("So11111111111111111111111111111111111111112");

// MarginFi bank primary oracles — VERIFIED LIVE 2026-05-16 by reading the
// bank's `config.oracle_keys[0]` at offset 8+296+314.
const PYTH_USDC:    Pubkey = pubkey!("2edzBDt34Ch9SvxCVcFyUhVjC63ngnkSEAeYcRVe23dR");
const PYTH_USDT:    Pubkey = pubkey!("8NXsAvnQWrqhUuBZHg1Tyee2QMWLXun7YLZMySemtSBZ");
const PYTH_WBTC:    Pubkey = pubkey!("ybTfQe6xqLU6a1Z6GjtqkqWLreeox61weq6kGQ1r8Wj");
const PYTH_WSOL:    Pubkey = pubkey!("6mbHvEwwvozvEF6Ni6jPyESXQV75KooJWKUzpKzSfetX");
const PYTH_JUPSOL:  Pubkey = pubkey!("89tHGNso9C5m5rzjiyYFJm6jaCVBAHwVVibEzueKSyVZ");
const PYTH_BSOL:    Pubkey = pubkey!("5tGP8vYVwWpmzZr5ZZqaYiVxFW3EzgZZhUbPwmUTEuPm");
const PYTH_JITOSOL: Pubkey = pubkey!("E8Jx6Jz7fZC6dsvrbqCjokSXhYEkEKeNjWoK94dQq8yd");
const PYTH_MSOL:    Pubkey = pubkey!("9ZTiPX9b8qN98cmEXAd3iP1TB1upq2Ts7nkmbJDLgbvo");
const PYTH_USD1:    Pubkey = pubkey!("drCboW4mD1B16n6g9PepjXyxC9UrDhFVpHZ6QHnF4L7");
const PYTH_USDS:    Pubkey = pubkey!("3edxkZkg9vHvkiSNcx7Cn2qgNuY7hAh1KJjPJMR1BjFX");
const PYTH_PYUSD:   Pubkey = pubkey!("8ohM44k6rV3cXFTBk9HDS6JyUgs4oXb6XEn31LVRTyBD");

// Hot reserves observed firing in production 2026-05-16. Pre-loading them
// + their associated vaults/oracles compresses tx by ~6 addresses × 32B
// per reserve = ~200 bytes per liquidation candidate.
const HOT_RESERVES: &[&str] = &[
    "D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59", // USDC main market
    "6gTJfuPHEg6uRAijRkMqNc9kan4sVZejKMxmvx2grT1p", // wSOL high-debt
    "F9HdecRG8GPs9LEn4S5VfeJVEZVqrDJFR6bvmQTi22na",
    "DQ126djx5db6SMCbejHLNxoosonVes3qW5eVVUQuT93v",
    "d4A2prbA2whesmvHaL88BH6Ewn5N4bTSU2Ze8P6Bc4Q",
    "r7Tyu7QswfZncQwmTQBkG1fDd8N5tJdi4b8S6KiBtBj",
    "BHUi32TrEsfN2U821G4FprKrR4hTeK4LCWtA3BFetuqA",
    "ESCkPWKHmgNE7Msf77n9yzqJd5kQVWWGy3o5Mgxhvavp",
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    let wallet = Wallet::from_env().context("loading wallet")?
        .ok_or_else(|| anyhow!("no wallet — set SOLANA_KEYPAIR_JSON"))?;
    let payer = wallet.pubkey();
    println!("Wallet: {payer}");

    let rpc_url = std::env::var("HELIUS_RPC_URL").or_else(|_| {
        std::env::var("HELIUS_API_KEY").map(|k|
            format!("https://mainnet.helius-rpc.com/?api-key={k}"))
    }).context("HELIUS_RPC_URL or HELIUS_API_KEY")?;

    let alt_pk_str = std::env::var("EXECUTOR_PYTH_ALT")
        .context("EXECUTOR_PYTH_ALT must be set")?;
    let alt_pk = Pubkey::from_str(&alt_pk_str)?;
    println!("Extending ALT: {alt_pk}");

    // Load reserves so we can expand each hot reserve into its 7-address
    // payload (mint, supply vault, fee vault, cToken mint, cToken supply
    // vault, oracle).
    println!("→ loading Kamino reserve cache (~462 reserves)…");
    let cache = fetch_reserve_cache(&rpc_url).await?;
    println!("✓ {} reserves cached", cache.by_address.len());

    let mut addrs: Vec<Pubkey> = vec![
        // Programs
        KAMINO_LEND_PROGRAM,
        KAMINO_FARMS_PROGRAM,
        MARGINFI_V2_PROGRAM,
        MARGINFI_MAIN_GROUP,
        SPL_TOKEN_PROGRAM,
        SYSTEM_PROGRAM,
        ADDRESS_LOOKUP_TABLE_PROGRAM,
        sysvar::instructions::id(),
        sysvar::clock::id(),
        // Mints
        USDC, USDT, WBTC, WSOL,
        // MarginFi bank primary oracles (13 total)
        PYTH_USDC, PYTH_USDT, PYTH_WBTC, PYTH_WSOL,
        PYTH_JUPSOL, PYTH_BSOL, PYTH_JITOSOL, PYTH_MSOL,
        PYTH_USD1, PYTH_USDS, PYTH_PYUSD,
        // (cbBTC + tBTC share PYTH_WBTC, no separate oracle)
    ];
    // MarginFi bank addresses — all 13 with their vault PDAs.
    for bank in [
        MARGINFI_USDC_BANK, MARGINFI_USDT_BANK, MARGINFI_WBTC_BANK, MARGINFI_WSOL_BANK,
        MARGINFI_JUPSOL_BANK, MARGINFI_BSOL_BANK, MARGINFI_JITOSOL_BANK, MARGINFI_MSOL_BANK,
        MARGINFI_CBBTC_BANK, MARGINFI_USD1_BANK, MARGINFI_USDS_BANK, MARGINFI_TBTC_BANK,
        MARGINFI_PYUSD_BANK,
    ] {
        addrs.push(bank);
        addrs.push(derive_liquidity_vault(&bank));
        addrs.push(derive_liquidity_vault_authority(&bank));
        addrs.push(derive_insurance_vault(&bank));
    }
    // Hot Kamino reserves — expand each into its 7-address payload.
    for r_str in HOT_RESERVES {
        let r_pk = match Pubkey::from_str(r_str) {
            Ok(p) => p,
            Err(_) => { eprintln!("  skip bad reserve: {r_str}"); continue; }
        };
        let Some(r) = cache.by_address.get(&r_pk) else {
            eprintln!("  reserve not in cache (missing from current discovery): {r_pk}");
            continue;
        };
        push_reserve(&mut addrs, r);
        // Also push the lending_market itself for completeness.
        if !addrs.contains(&r.lending_market) { addrs.push(r.lending_market); }
    }

    // Dedupe while preserving order.
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(*a));

    println!("\nAdding {} unique addresses (after dedupe):", addrs.len());
    for a in &addrs { println!("  {a}"); }

    if addrs.is_empty() {
        println!("nothing to extend — exit");
        return Ok(());
    }
    if addrs.len() > 30 {
        // ExtendLookupTable holds ~30 addresses per ix to fit in a tx.
        // Split into chunks of 28 to leave margin.
        println!("\n→ splitting {} addresses into chunks of 28", addrs.len());
        let mut total_sent = 0usize;
        for (i, chunk) in addrs.chunks(28).enumerate() {
            println!("\n--- chunk {}/{} ({} addresses) ---", i + 1, addrs.len().div_ceil(28), chunk.len());
            send_extend(&rpc_url, &wallet, alt_pk, chunk.to_vec()).await?;
            total_sent += chunk.len();
        }
        println!("\n✓ all {total_sent} addresses extended");
    } else {
        send_extend(&rpc_url, &wallet, alt_pk, addrs).await?;
    }

    Ok(())
}

fn push_reserve(out: &mut Vec<Pubkey>, r: &ReserveSummary) {
    out.push(r.address);
    out.push(r.liquidity_mint);
    out.push(r.liquidity_supply_vault);
    out.push(r.liquidity_fee_vault);
    out.push(r.collateral_mint);
    out.push(r.collateral_supply_vault);
    // Primary oracle — whichever slot is populated.
    let (_, oracle) = r.active_oracle();
    if oracle != Pubkey::default() {
        out.push(oracle);
    }
    // Also push the alternative oracle slots if non-default — refresh_reserve
    // walks all configured slots and counts them as accounts even when only
    // one is the live source. Empirically required for non-Pyth-only reserves.
    if r.pyth_price != Pubkey::default() && r.pyth_price != oracle { out.push(r.pyth_price); }
    if r.scope_price_feed != Pubkey::default() && r.scope_price_feed != oracle { out.push(r.scope_price_feed); }
    if r.switchboard_price_agg != Pubkey::default() { out.push(r.switchboard_price_agg); }
    if r.switchboard_twap_agg != Pubkey::default() { out.push(r.switchboard_twap_agg); }
}

async fn send_extend(rpc_url: &str, wallet: &Wallet, alt_pk: Pubkey, addrs: Vec<Pubkey>) -> anyhow::Result<()> {
    let payer = wallet.pubkey();
    let extend_ix = build_extend_lookup_table_ix(alt_pk, payer, addrs);
    let ixs = vec![
        set_compute_unit_limit_ix(200_000),
        set_compute_unit_price_ix(2_000),
        extend_ix,
    ];
    let blockhash = fetch_latest_blockhash(rpc_url).await
        .map_err(|e| anyhow!("blockhash: {e}"))?;
    let signed_b64 = build_and_sign_legacy_tx_b64(
        &payer, &ixs, &blockhash, wallet.bytes(),
    ).map_err(|e| anyhow!("sign: {e}"))?;

    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {"encoding":"base64","skipPreflight":false,"maxRetries":5,"preflightCommitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    if let Some(err) = r.get("error") {
        bail!("send: {err}");
    }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send: missing sig"))?;
    println!("  ✓ sent: {sig}");

    // Wait + confirm
    tokio::time::sleep(Duration::from_secs(4)).await;
    let cbody = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
        "params":[[sig], {"searchTransactionHistory": true}],
    });
    let cr: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&cbody).send().await?.json().await?;
    let status = cr.pointer("/result/value/0").map(|v| v.to_string()).unwrap_or_default();
    println!("    confirm: {status}");
    let _ = base64::engine::general_purpose::STANDARD.decode(""); // keep base64 import live
    Ok(())
}

fn build_extend_lookup_table_ix(
    alt_pk:    Pubkey,
    payer:     Pubkey,
    new_addrs: Vec<Pubkey>,
) -> Instruction {
    let mut data = Vec::with_capacity(4 + 8 + new_addrs.len() * 32);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&(new_addrs.len() as u64).to_le_bytes());
    for a in &new_addrs {
        data.extend_from_slice(a.as_ref());
    }
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt_pk, false),
            AccountMeta::new(payer, true),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data,
    }
}
