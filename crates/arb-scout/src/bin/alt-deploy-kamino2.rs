//! Deploy a SECOND Address Lookup Table dedicated to kamino-hunter's
//! flash-wrapped liquidate+swap txs.
//!
//! The existing `EXECUTOR_PYTH_ALT` hit Solana's 256-address cap when we
//! tried to add the 9 new LST/exotic banks (USDC/USDT/WBTC/wSOL plus
//! jupSOL/bSOL/jitoSOL/mSOL/cbBTC/USD1/USDS/tBTC/PYUSD). This bin
//! creates a fresh ALT and populates it with:
//!
//!   - All 13 MarginFi banks
//!   - For each: liquidity_vault, vault_authority, insurance_vault, oracle
//!   - The 8 hot Kamino reserves we observed firing repeatedly + their
//!     per-reserve addresses (mint, supply vaults, oracle, lending_market)
//!   - Kamino + MarginFi program ids, sysvars, common mints
//!
//! Total ~110 addresses, well under the 256 cap.
//!
//! After this runs once, set the printed pubkey as
//! `EXECUTOR_KAMINO_ALT2` in `/etc/arbbot/.env`, then restart
//! kamino-hunter — it loads both ALTs.

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
use arb_scout::kamino_liq::fetch_reserve_cache;
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

// MarginFi bank primary oracles (probed live 2026-05-16).
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

const HOT_RESERVES: &[&str] = &[
    "D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59",
    "6gTJfuPHEg6uRAijRkMqNc9kan4sVZejKMxmvx2grT1p",
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

    // ── Step 1: create the new ALT ────────────────────────────────
    let recent_slot = get_slot(&rpc_url).await?;
    let (alt_pk, bump) = Pubkey::find_program_address(
        &[payer.as_ref(), &recent_slot.to_le_bytes()],
        &ADDRESS_LOOKUP_TABLE_PROGRAM,
    );
    println!("→ creating new ALT: {alt_pk}");
    let create_ix = build_create_lookup_table_ix(alt_pk, payer, recent_slot, bump);
    send(&rpc_url, &wallet, vec![
        set_compute_unit_limit_ix(200_000),
        set_compute_unit_price_ix(2_000),
        create_ix,
    ]).await?;
    println!("✓ ALT created");

    // ── Step 2: collect addresses ─────────────────────────────────
    println!("→ loading Kamino reserve cache");
    let cache = fetch_reserve_cache(&rpc_url).await?;

    let mut addrs: Vec<Pubkey> = vec![
        KAMINO_LEND_PROGRAM, KAMINO_FARMS_PROGRAM,
        MARGINFI_V2_PROGRAM, MARGINFI_MAIN_GROUP,
        SPL_TOKEN_PROGRAM, SYSTEM_PROGRAM, ADDRESS_LOOKUP_TABLE_PROGRAM,
        sysvar::instructions::id(), sysvar::clock::id(),
        USDC, USDT, WBTC, WSOL,
        PYTH_USDC, PYTH_USDT, PYTH_WBTC, PYTH_WSOL,
        PYTH_JUPSOL, PYTH_BSOL, PYTH_JITOSOL, PYTH_MSOL,
        PYTH_USD1, PYTH_USDS, PYTH_PYUSD,
    ];
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
    for r_str in HOT_RESERVES {
        let r_pk = match Pubkey::from_str(r_str) {
            Ok(p) => p, Err(_) => continue,
        };
        let Some(r) = cache.by_address.get(&r_pk) else { continue; };
        addrs.push(r.address);
        addrs.push(r.liquidity_mint);
        addrs.push(r.liquidity_supply_vault);
        addrs.push(r.liquidity_fee_vault);
        addrs.push(r.collateral_mint);
        addrs.push(r.collateral_supply_vault);
        let (_, oracle) = r.active_oracle();
        if oracle != Pubkey::default() { addrs.push(oracle); }
        if r.scope_price_feed != Pubkey::default() && r.scope_price_feed != oracle {
            addrs.push(r.scope_price_feed);
        }
        addrs.push(r.lending_market);
    }

    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(*a));
    println!("→ {} unique addresses to add", addrs.len());

    // ── Step 3: chunked extend ────────────────────────────────────
    for (i, chunk) in addrs.chunks(28).enumerate() {
        println!("--- chunk {}/{} ({} addrs) ---", i+1, addrs.len().div_ceil(28), chunk.len());
        let extend_ix = build_extend_lookup_table_ix(alt_pk, payer, chunk);
        send(&rpc_url, &wallet, vec![
            set_compute_unit_limit_ix(200_000),
            set_compute_unit_price_ix(2_000),
            extend_ix,
        ]).await?;
    }

    println!();
    println!("════════════════════════════════════════════════════════");
    println!("  NEW ALT pubkey: {alt_pk}");
    println!("════════════════════════════════════════════════════════");
    println!();
    println!("Add to /etc/arbbot/.env:");
    println!("  EXECUTOR_KAMINO_ALT2={alt_pk}");
    println!("Then restart kamino-hunter.");
    Ok(())
}

async fn send(rpc_url: &str, wallet: &Wallet, ixs: Vec<Instruction>) -> anyhow::Result<()> {
    let payer = wallet.pubkey();
    let blockhash = fetch_latest_blockhash(rpc_url).await.map_err(|e| anyhow!("blockhash: {e}"))?;
    let signed_b64 = build_and_sign_legacy_tx_b64(&payer, &ixs, &blockhash, wallet.bytes())
        .map_err(|e| anyhow!("sign: {e}"))?;
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {"encoding":"base64","skipPreflight":false,"maxRetries":5,"preflightCommitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    if let Some(err) = r.get("error") { bail!("send: {err}"); }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send: missing sig"))?;
    println!("  ✓ sent: {sig}");
    tokio::time::sleep(Duration::from_secs(3)).await;
    let cbody = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
        "params":[[sig], {"searchTransactionHistory": true}],
    });
    let cr: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&cbody).send().await?.json().await?;
    let _ = base64::engine::general_purpose::STANDARD.decode(""); // keep base64 import live
    println!("    confirm: {}", cr.pointer("/result/value/0").map(|v| v.to_string()).unwrap_or_default());
    Ok(())
}

async fn get_slot(rpc_url: &str) -> anyhow::Result<u64> {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getSlot"});
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await?.json().await?;
    r.pointer("/result").and_then(|v| v.as_u64()).ok_or_else(|| anyhow!("getSlot bad response"))
}

fn build_create_lookup_table_ix(
    alt: Pubkey, payer: Pubkey, recent_slot: u64, bump: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&recent_slot.to_le_bytes());
    data.push(bump);
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt, false),
            AccountMeta::new_readonly(payer, true),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data,
    }
}

fn build_extend_lookup_table_ix(
    alt: Pubkey, payer: Pubkey, addrs: &[Pubkey],
) -> Instruction {
    let mut data = Vec::with_capacity(12 + addrs.len() * 32);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&(addrs.len() as u64).to_le_bytes());
    for a in addrs { data.extend_from_slice(a.as_ref()); }
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt, false),
            AccountMeta::new_readonly(payer, true),
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data,
    }
}
