//! Deploy a THIRD Address Lookup Table dedicated to the marginfi-hunter
//! event-driven flow. Live observation 2026-05-16: with only
//! `EXECUTOR_KAMINO_ALT2` (13 main MarginFi banks) wired in, ~30% of
//! liquidatable accounts had banks NOT in any ALT and their txs hit the
//! 1232-byte v0 cap (`VersionedTransaction too large: 1968-2168 bytes`).
//!
//! This bin creates a fresh ALT and populates it with up to ~47 more
//! Push-V2-observable banks (each: bank + liquidity_vault +
//! vault_authority + insurance_vault + primary_oracle = 5 entries) plus
//! all common static addresses. Total stays under the 256-entry ALT cap.
//!
//! Bank selection: live `fetch_bank_cache`, drop the 13 already covered
//! by `EXECUTOR_KAMINO_ALT2`, sort remaining Push-V2 observable banks by
//! pubkey for determinism, take the first 47.
//!
//! After this runs once, set the printed pubkey as
//! `EXECUTOR_MARGINFI_ALT` in `/etc/arbbot/.env`, then restart
//! marginfi-hunter — attempt_liquidation already reads it (per commit
//! 886d7b0 in marginfi_liq.rs).

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;
use solana_program::sysvar;

use arb_flashloan::{
    derive_insurance_vault, derive_liquidity_vault, derive_liquidity_vault_authority,
    MARGINFI_USDC_BANK, MARGINFI_USDT_BANK, MARGINFI_WBTC_BANK, MARGINFI_WSOL_BANK,
    MARGINFI_JUPSOL_BANK, MARGINFI_BSOL_BANK, MARGINFI_JITOSOL_BANK, MARGINFI_MSOL_BANK,
    MARGINFI_CBBTC_BANK, MARGINFI_USD1_BANK, MARGINFI_USDS_BANK, MARGINFI_TBTC_BANK,
    MARGINFI_PYUSD_BANK,
    MARGINFI_V2_PROGRAM, MARGINFI_MAIN_GROUP,
};
use arb_scout::marginfi_liq::fetch_bank_cache;
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
const ATA_PROGRAM: Pubkey =
    pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const COMPUTE_BUDGET: Pubkey =
    pubkey!("ComputeBudget111111111111111111111111111111");
const MEMO_PROGRAM: Pubkey =
    pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
const JITO_TIP: Pubkey =
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5");

const MAX_ALT_ENTRIES: usize = 256;
const RESERVED_FOR_STATIC: usize = 30;
const MAX_BANKS: usize = (MAX_ALT_ENTRIES - RESERVED_FOR_STATIC) / 5;  // = 45

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

    // ── Step 1: load bank cache, choose banks ─────────────────────
    println!("→ loading MarginFi bank cache");
    let cache = fetch_bank_cache(&rpc_url).await?;
    let already_covered: HashSet<Pubkey> = [
        MARGINFI_USDC_BANK, MARGINFI_USDT_BANK, MARGINFI_WBTC_BANK, MARGINFI_WSOL_BANK,
        MARGINFI_JUPSOL_BANK, MARGINFI_BSOL_BANK, MARGINFI_JITOSOL_BANK, MARGINFI_MSOL_BANK,
        MARGINFI_CBBTC_BANK, MARGINFI_USD1_BANK, MARGINFI_USDS_BANK, MARGINFI_TBTC_BANK,
        MARGINFI_PYUSD_BANK,
    ].into_iter().collect();

    let mut candidate_banks: Vec<Pubkey> = cache.by_bank.iter()
        .filter(|(pk, b)| b.oracle_setup.is_pyth_observable() && !already_covered.contains(pk))
        .map(|(pk, _)| *pk).collect();
    // Deterministic order so re-runs land the same set.
    candidate_banks.sort();
    let banks: Vec<Pubkey> = candidate_banks.into_iter().take(MAX_BANKS).collect();
    println!("  ✓ chose {} banks (Push-V2 observable, not in KAMINO_ALT2)", banks.len());

    // ── Step 2: build address list ────────────────────────────────
    let mut addrs: Vec<Pubkey> = vec![
        MARGINFI_V2_PROGRAM, MARGINFI_MAIN_GROUP,
        SPL_TOKEN_PROGRAM, SYSTEM_PROGRAM, ATA_PROGRAM,
        COMPUTE_BUDGET, MEMO_PROGRAM, JITO_TIP,
        ADDRESS_LOOKUP_TABLE_PROGRAM,
        sysvar::instructions::id(), sysvar::clock::id(), sysvar::rent::id(),
    ];
    for bpk in &banks {
        let Some(bank) = cache.by_bank.get(bpk) else { continue; };
        addrs.push(*bpk);
        addrs.push(derive_liquidity_vault(bpk));
        addrs.push(derive_liquidity_vault_authority(bpk));
        addrs.push(derive_insurance_vault(bpk));
        let oracle = bank.primary_oracle();
        if oracle != Pubkey::default() { addrs.push(oracle); }
        // The mint is also useful for the liquidate ix's withdraw step.
        addrs.push(bank.mint);
    }

    // Dedupe (preserve insertion order)
    let mut seen = HashSet::new();
    addrs.retain(|a| seen.insert(*a));
    println!("→ {} unique addresses to add", addrs.len());
    if addrs.len() > MAX_ALT_ENTRIES {
        bail!("address count {} exceeds ALT cap 256", addrs.len());
    }

    // ── Step 3: create the new ALT ────────────────────────────────
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

    // ── Step 4: chunked extend ─────────────────────────────────────
    let total_chunks = addrs.len().div_ceil(28);
    for (i, chunk) in addrs.chunks(28).enumerate() {
        println!("--- chunk {}/{} ({} addrs) ---", i + 1, total_chunks, chunk.len());
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
    println!("  EXECUTOR_MARGINFI_ALT={alt_pk}");
    println!("Then restart marginfi-hunter.");
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
    let _ = base64::engine::general_purpose::STANDARD.decode("");  // keep base64 import live
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
