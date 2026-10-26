//! Extend `EXECUTOR_PYTH_ALT` with MarginFi liquidation-specific
//! addresses so `marginfi-liquidate-once` v0 txs compress under the
//! 1232-byte limit.
//!
//! Run once after deploying the marginfi-liquidate-once binary:
//!
//! ```text
//! cargo run --release --bin alt-extend-marginfi-liq
//! ```
//!
//! Cost: one ExtendLookupTable tx, ~0.000005 SOL.
//!
//! What it adds (kept under 256-entry ALT cap):
//! - MarginFi v2 program
//! - MarginFi main group
//! - SPL Token program
//! - SystemProgram
//! - For each canonical bank (USDC, USDT, WBTC): bank + liquidity_vault
//!   + liquidity_vault_authority + insurance_vault + primary_oracle + mint
//!
//! Banks beyond these are handled by live discovery — the executor still
//! works for them, just with marginally larger tx size.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

use arb_flashloan::{
    derive_insurance_vault, derive_liquidity_vault, derive_liquidity_vault_authority,
    MARGINFI_USDC_BANK, MARGINFI_USDT_BANK, MARGINFI_WBTC_BANK,
    MARGINFI_V2_PROGRAM, MARGINFI_MAIN_GROUP,
};
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

// MarginFi USDC bank's primary oracle (Push V2). Source: bank cache scan
// 2026-05-15. The full set of oracle pubkeys per bank lives in the live
// `BankCache.by_bank[X].oracle_keys[0]` — we hard-code only the three
// most-used here for the ALT extend.
const USDC_PYTH_PUSH:    Pubkey = pubkey!("Dpw1EAVrSB1ibxiDQyTAW6Zip3J4Btk2x4SgApQCeFbX");
const USDT_PYTH_PUSH:    Pubkey = pubkey!("HT2PLQBcG5EiCcNSaMHAjSgd9F98ecpATbk4Sk5oYuM");
const WBTC_PYTH_PUSH:    Pubkey = pubkey!("4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo");

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

    // Collect addresses; dedupe via HashSet.
    let mut addrs: Vec<Pubkey> = vec![
        MARGINFI_V2_PROGRAM,
        MARGINFI_MAIN_GROUP,
        SPL_TOKEN_PROGRAM,
        SYSTEM_PROGRAM,
        USDC, USDT, WBTC,
        USDC_PYTH_PUSH, USDT_PYTH_PUSH, WBTC_PYTH_PUSH,
    ];
    for bank in [MARGINFI_USDC_BANK, MARGINFI_USDT_BANK, MARGINFI_WBTC_BANK] {
        addrs.push(bank);
        addrs.push(derive_liquidity_vault(&bank));
        addrs.push(derive_liquidity_vault_authority(&bank));
        addrs.push(derive_insurance_vault(&bank));
    }

    // Dedupe (preserve order)
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(*a));

    println!("Adding {} addresses:", addrs.len());
    for a in &addrs { println!("  {a}"); }

    // ALT extend is capped at ~30 addresses per ix to stay under tx
    // size; we have ~22 so a single ix is fine.
    let extend_ix = build_extend_lookup_table_ix(alt_pk, payer, addrs);
    let ixs = vec![
        set_compute_unit_limit_ix(200_000),
        set_compute_unit_price_ix(2_000),
        extend_ix,
    ];

    let blockhash = fetch_latest_blockhash(&rpc_url).await
        .map_err(|e| anyhow!("blockhash: {e}"))?;
    let signed_b64 = build_and_sign_legacy_tx_b64(
        &payer, &ixs, &blockhash, wallet.bytes(),
    ).map_err(|e| anyhow!("sign: {e}"))?;

    println!("\nSubmitting…");
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {"encoding":"base64","skipPreflight":false,"maxRetries":5,"preflightCommitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    if let Some(err) = r.get("error") {
        bail!("send: {err}");
    }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send: missing sig"))?;
    println!("✓ sent: {sig}");
    println!("  https://solscan.io/tx/{sig}");

    // Wait + confirm
    tokio::time::sleep(Duration::from_secs(5)).await;
    let cbody = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
        "params":[[sig], {"searchTransactionHistory": true}],
    });
    let cr: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&cbody).send().await?.json().await?;
    println!("confirm: {}", cr.pointer("/result/value/0").map(|v| v.to_string()).unwrap_or_default());

    Ok(())
}

/// AddressLookupTable::ExtendLookupTable instruction.
/// Layout: [u32(2) instruction_discriminator][u64(addr_count)][addr_count × 32 bytes pubkeys]
fn build_extend_lookup_table_ix(
    alt_pk:    Pubkey,
    payer:     Pubkey,
    new_addrs: Vec<Pubkey>,
) -> Instruction {
    let mut data = Vec::with_capacity(4 + 8 + new_addrs.len() * 32);
    data.extend_from_slice(&2u32.to_le_bytes());  // discriminator: Extend
    data.extend_from_slice(&(new_addrs.len() as u64).to_le_bytes());
    for a in &new_addrs {
        data.extend_from_slice(a.as_ref());
    }
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt_pk, false),                    // ALT (mut)
            AccountMeta::new(payer, true),                      // authority (signer)
            AccountMeta::new(payer, true),                      // payer (signer, dup OK)
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),   // system_program
        ],
        data,
    }
}
