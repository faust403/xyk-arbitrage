//! Extend an existing ALT with WBTC (Wormhole) MarginFi flash-loan addresses.
//!
//! Hard-coded for the current `9FP2vfuH…` ALT. Adds 5 addresses:
//!   - MarginFi WBTC bank
//!   - WBTC liquidity_vault (PDA)
//!   - WBTC liquidity_vault_auth (PDA)
//!   - WBTC Pyth Pull v2 oracle
//!   - WBTC mint
//!
//! Run once after bumping the executor with WBTC support:
//!
//! ```
//! cargo run --release --bin alt-extend-wbtc -p arb-scout
//! ```
//!
//! Cost: ~0.000005 SOL (one ExtendLookupTable tx).

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

use arb_flashloan::{
    derive_liquidity_vault, derive_liquidity_vault_authority, MARGINFI_WBTC_BANK,
};
use arb_scout::known::WBTC;
use arb_scout::raw_tx::{
    build_and_sign_legacy_tx_b64, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix,
};
use arb_scout::wallet::Wallet;

const ADDRESS_LOOKUP_TABLE_PROGRAM: Pubkey =
    pubkey!("AddressLookupTab1e1111111111111111111111111");

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

    let alt_pk_str = std::env::var("EXECUTOR_NATIVE_ALT")
        .unwrap_or_else(|_| "9FP2vfuHUMiKBfp9oLhmU7KNK1FJ88E3udDaxYk25HW9".to_string());
    let alt_pk = Pubkey::from_str(&alt_pk_str)?;
    println!("Extending ALT: {alt_pk}");

    let wbtc_pyth = Pubkey::from_str("4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo")?;
    let wbtc_vault = derive_liquidity_vault(&MARGINFI_WBTC_BANK);
    let wbtc_vault_auth = derive_liquidity_vault_authority(&MARGINFI_WBTC_BANK);

    let new_addrs = vec![
        MARGINFI_WBTC_BANK,
        wbtc_vault,
        wbtc_vault_auth,
        wbtc_pyth,
        WBTC,
    ];
    println!("Adding {} addresses:", new_addrs.len());
    for a in &new_addrs {
        println!("  {a}");
    }

    let extend_ix = build_extend_lookup_table_ix(alt_pk, payer, new_addrs);
    let ixs = vec![
        set_compute_unit_limit_ix(150_000),
        set_compute_unit_price_ix(1_000),
        extend_ix,
    ];

    let blockhash = fetch_latest_blockhash(&rpc_url).await?;
    let signed_b64 = build_and_sign_legacy_tx_b64(&payer, &ixs, &blockhash, wallet.bytes())?;

    println!();
    println!("Submitting ExtendLookupTable tx …");
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64,{
            "encoding":"base64","skipPreflight":false,
            "preflightCommitment":"confirmed",
        }],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&body).send().await?.json().await?;
    if let Some(err) = r.pointer("/error") {
        return Err(anyhow!("sendTransaction error: {err}"));
    }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing sig: {r}"))?;
    println!("  → sig: {sig}");

    // Wait for confirmation
    println!("Confirming …");
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
            "params":[[sig],{"searchTransactionHistory":false}],
        });
        let r: serde_json::Value = reqwest::Client::new()
            .post(&rpc_url).json(&body).send().await?.json().await?;
        let st = r.pointer("/result/value/0");
        if let Some(s) = st {
            if !s.is_null() {
                if let Some(err) = s.pointer("/err") {
                    if !err.is_null() {
                        return Err(anyhow!("tx failed: {err}"));
                    }
                }
                let conf = s.pointer("/confirmationStatus")
                    .and_then(|v| v.as_str()).unwrap_or("");
                if conf == "confirmed" || conf == "finalized" {
                    println!();
                    println!("✓ ALT extended with WBTC addresses");
                    println!("  Solscan: https://solscan.io/tx/{sig}");
                    return Ok(());
                }
            }
        }
    }
    Err(anyhow!("not confirmed within 45s — sig {sig}"))
}

fn build_extend_lookup_table_ix(
    alt: Pubkey, payer: Pubkey, new_addrs: Vec<Pubkey>,
) -> Instruction {
    // Layout: tag(u32 LE = 2 = Extend) + addresses(Vec<Pubkey> with u64 len prefix)
    let mut data = Vec::with_capacity(4 + 8 + new_addrs.len() * 32);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&(new_addrs.len() as u64).to_le_bytes());
    for a in &new_addrs {
        data.extend_from_slice(a.as_ref());
    }
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt, false),
            AccountMeta::new(payer, true),  // authority (signer)
            AccountMeta::new(payer, true),  // payer (signer)
            AccountMeta::new_readonly(solana_program::system_program::id(), false),
        ],
        data,
    }
}
