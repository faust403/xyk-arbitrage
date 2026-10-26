//! End-to-end pipeline smoke test: build, sign, send, confirm ONE real swap.
//!
//! Routes 0.1 USDC → USDT through a Raydium CLMM 0.01% pool. Stable pair,
//! single instruction, no flash loan, no wSOL wrapping. Either the wallet
//! ends up holding ~0.1 USDT and the transaction signature lands on chain
//! within 30 seconds, or we see exactly which step broke.
//!
//! Build + run:
//! ```
//! cargo run --release --bin swap-once
//! ```
//!
//! Required env:
//! - `SOLANA_KEYPAIR_JSON` — 64-byte JSON array of the wallet's keypair
//! - `HELIUS_API_KEY`      — Helius RPC API key (Business tier not required)
//!
//! ## What this proves end-to-end
//!
//! 1. Wallet loader parses the JSON keypair correctly.
//! 2. We can fetch + parse a live Raydium CLMM pool from Helius RPC.
//! 3. Our `build_swap_ix` produces a valid instruction the runtime accepts.
//! 4. Our `build_and_sign_legacy_tx_b64` builds a wire-format tx the
//!    network signs and propagates.
//! 5. `sendTransaction` + `getSignatureStatuses` round-trip works.
//! 6. The pool's `token_mint_0` / `token_mint_1` ordering logic is correct
//!    (zero_for_one direction picked right way around).
//!
//! ## What it does NOT prove
//!
//! - Jito bundle path (we send via plain RPC `sendTransaction`).
//! - Two-leg / N-hop atomic swaps.
//! - Flash-loan envelopes.
//! - ALT (v0) transactions.
//!
//! Those each have their own smoke tests once this baseline is green.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use solana_program::pubkey::Pubkey;

use arb_dex_raydium::{
    clmm_swap_ix::{
        build_swap_ix, derive_tick_array_pda, tick_array_start_index,
        RaydiumClmmSwapAccounts,
    },
    RaydiumClmmData,
};
use arb_scout::known::{USDC, USDT};
use arb_scout::raw_tx::{
    build_and_sign_legacy_tx_b64, create_ata_idempotent_ix, derive_ata,
    fetch_latest_blockhash, set_compute_unit_limit_ix, set_compute_unit_price_ix,
};
use arb_scout::wallet::Wallet;

/// Raydium CLMM USDC/USDT pool, 0.01% tier, ~$9M TVL.
/// `scout-pools.toml` line 79.
const POOL_ADDRESS: &str = "BZtgQEyS6eXUXicYPHecYQ7PybqodXQMvkjUbP4R8mUU";

/// 0.1 USDC (= 100 000 base units, since USDC has 6 decimals).
const AMOUNT_IN: u64 = 100_000;

/// 50 basis points slippage floor (very wide — we don't care about the
/// exact rate, only that the swap lands).
const SLIPPAGE_BPS: u64 = 50;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    // ── 1. Wallet + RPC ────────────────────────────────────────────────────
    let wallet = Wallet::from_env().context("loading wallet")?
        .ok_or_else(|| anyhow!("SOLANA_KEYPAIR_JSON missing or empty"))?;
    let payer = wallet.pubkey();
    let api_key = std::env::var("HELIUS_API_KEY")
        .context("HELIUS_API_KEY env var required")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");
    let http = reqwest::Client::new();

    println!("=== swap-once: USDC → USDT on Raydium CLMM ===");
    println!("Wallet:  {payer}");
    println!("Pool:    {POOL_ADDRESS}");
    println!("Amount:  {} USDC ({} base units)", AMOUNT_IN as f64 / 1e6, AMOUNT_IN);
    println!();

    // ── 2. Fetch + parse pool state ────────────────────────────────────────
    let pool_pk = Pubkey::from_str(POOL_ADDRESS)?;
    let pool_data = fetch_account_data(&http, &rpc, &pool_pk).await
        .context("fetching pool account")?;
    let pool = RaydiumClmmData::try_parse(&pool_data)
        .ok_or_else(|| anyhow!("pool account did not parse as RaydiumClmmData"))?;

    println!("Pool state parsed:");
    println!("  token_mint_0:    {}", pool.token_mint_0);
    println!("  token_mint_1:    {}", pool.token_mint_1);
    println!("  tick_spacing:    {}", pool.tick_spacing);
    println!("  tick_current:    {}", pool.tick_current);
    println!("  liquidity:       {}", pool.liquidity);
    println!("  sqrt_price_x64:  {}", pool.sqrt_price_x64);
    println!();

    // ── 3. Direction + slippage ────────────────────────────────────────────
    let (zero_for_one, input_vault, output_vault) =
        if pool.token_mint_0 == USDC && pool.token_mint_1 == USDT {
            (true,  pool.token_vault_0, pool.token_vault_1)
        } else if pool.token_mint_0 == USDT && pool.token_mint_1 == USDC {
            (false, pool.token_vault_1, pool.token_vault_0)
        } else {
            bail!("pool mints don't match USDC/USDT — got {} / {}",
                pool.token_mint_0, pool.token_mint_1);
        };

    // For a stable pair we expect output ≈ input. Apply slippage floor:
    // min_out = input * (10_000 - SLIPPAGE_BPS) / 10_000.
    let min_amount_out = AMOUNT_IN
        .saturating_mul(10_000 - SLIPPAGE_BPS)
        / 10_000;

    println!("Direction: {} (zero_for_one = {})",
        if zero_for_one { "USDC → USDT" } else { "USDT → USDC (unexpected)" },
        zero_for_one);
    println!("min_out:   {} ({} bps slippage tolerance)", min_amount_out, SLIPPAGE_BPS);
    println!();

    // ── 4. Derive remaining accounts ───────────────────────────────────────
    let start_tick = tick_array_start_index(pool.tick_current, pool.tick_spacing);
    let tick_array = derive_tick_array_pda(&pool_pk, start_tick);

    let usdc_ata = derive_ata(&payer, &USDC);
    let usdt_ata = derive_ata(&payer, &USDT);

    let (input_token_account, output_token_account) = if zero_for_one {
        (usdc_ata, usdt_ata)
    } else {
        (usdt_ata, usdc_ata)
    };

    println!("Accounts derived:");
    println!("  amm_config:    {}", pool.amm_config);
    println!("  tick_array @{}: {}", start_tick, tick_array);
    println!("  observation:   {}", pool.observation_key);
    println!("  usdc_ata:      {}", usdc_ata);
    println!("  usdt_ata:      {}", usdt_ata);
    println!();

    // ── 5. Build instructions ──────────────────────────────────────────────
    let swap_accs = RaydiumClmmSwapAccounts {
        payer,
        amm_config:           pool.amm_config,
        pool_state:           pool_pk,
        input_token_account,
        output_token_account,
        input_vault,
        output_vault,
        observation_state:    pool.observation_key,
        tick_array,
        tick_array_next:      None,
        tick_array_after:     None,
    };

    let swap_ix = build_swap_ix(&swap_accs, AMOUNT_IN, min_amount_out, zero_for_one)
        .context("building CLMM swap ix")?;

    let ixes = vec![
        set_compute_unit_limit_ix(120_000),
        set_compute_unit_price_ix(50_000),       // priority fee, ~0.000006 SOL
        create_ata_idempotent_ix(payer, payer, USDT),  // safe no-op if exists
        swap_ix,
    ];

    // ── 6. Sign + send ─────────────────────────────────────────────────────
    let blockhash = fetch_latest_blockhash(&rpc).await
        .context("fetching latest blockhash")?;
    println!("Blockhash: {}", bs58::encode(&blockhash).into_string());

    let tx_b64 = build_and_sign_legacy_tx_b64(&payer, &ixes, &blockhash, wallet.bytes())
        .context("building + signing legacy tx")?;
    println!("Tx size (b64): {} bytes", tx_b64.len());
    println!();

    println!("→ submitting via sendTransaction …");
    let sig = send_transaction(&http, &rpc, &tx_b64).await
        .context("sendTransaction")?;
    println!("✓ submitted, signature = {sig}");
    println!("   https://solscan.io/tx/{sig}");
    println!();

    // ── 7. Poll for confirmation ───────────────────────────────────────────
    println!("Waiting for confirmation (up to 60s)…");
    let confirmed = wait_for_confirmation(&http, &rpc, &sig, 60).await?;
    match confirmed {
        ConfirmationOutcome::Confirmed { slot } => {
            println!("✓ CONFIRMED at slot {slot}");
        }
        ConfirmationOutcome::Failed { err } => {
            bail!("tx failed on chain: {err}");
        }
        ConfirmationOutcome::Timeout => {
            bail!("timed out waiting for confirmation (60s)");
        }
    }

    Ok(())
}

// ─── RPC helpers ──────────────────────────────────────────────────────────

async fn fetch_account_data(
    http: &reqwest::Client,
    rpc:  &str,
    pk:   &Pubkey,
) -> anyhow::Result<Vec<u8>> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[pk.to_string(), {"encoding":"base64","commitment":"confirmed"}],
    });
    let r: Value = http.post(rpc).json(&body).send().await?.json().await?;
    let data_b64 = r.pointer("/result/value/data/0")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("getAccountInfo returned no data for {pk}: {r}"))?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        data_b64,
    )?;
    Ok(bytes)
}

async fn send_transaction(
    http:   &reqwest::Client,
    rpc:    &str,
    tx_b64: &str,
) -> anyhow::Result<String> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[tx_b64, {
            "encoding": "base64",
            "skipPreflight": false,
            "preflightCommitment": "confirmed",
            "maxRetries": 3
        }],
    });
    let r: Value = http.post(rpc).json(&body).send().await?.json().await?;
    if let Some(err) = r.get("error") {
        bail!("sendTransaction RPC error: {err}");
    }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("sendTransaction missing result: {r}"))?;
    Ok(sig.to_string())
}

enum ConfirmationOutcome {
    Confirmed { slot: u64 },
    Failed    { err: String },
    Timeout,
}

async fn wait_for_confirmation(
    http:    &reqwest::Client,
    rpc:     &str,
    sig:     &str,
    secs:    u64,
) -> anyhow::Result<ConfirmationOutcome> {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    let mut tries: u32 = 0;
    while std::time::Instant::now() < deadline {
        tries += 1;
        let body = json!({
            "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
            "params":[[sig], {"searchTransactionHistory": true}],
        });
        let r: Value = http.post(rpc).json(&body).send().await?.json().await?;
        let s = r.pointer("/result/value/0");
        if let Some(s) = s {
            if !s.is_null() {
                let slot = s.get("slot").and_then(|v| v.as_u64()).unwrap_or(0);
                if let Some(err) = s.get("err").and_then(|v| {
                    if v.is_null() { None } else { Some(v.to_string()) }
                }) {
                    return Ok(ConfirmationOutcome::Failed { err });
                }
                let conf_status = s.get("confirmationStatus")
                    .and_then(|v| v.as_str()).unwrap_or("");
                if conf_status == "confirmed" || conf_status == "finalized" {
                    return Ok(ConfirmationOutcome::Confirmed { slot });
                }
                if tries % 4 == 0 {
                    println!("  …status: {conf_status} (slot {slot})");
                }
            } else if tries % 4 == 0 {
                println!("  …not yet visible (try {tries})");
            }
        }
        tokio::time::sleep(Duration::from_millis(1_500)).await;
    }
    Ok(ConfirmationOutcome::Timeout)
}
