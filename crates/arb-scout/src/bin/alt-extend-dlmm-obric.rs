//! Extend the native-executor ALT with DLMM + Obric global addresses so
//! routes that include those DEXes fit into the v0 1232-byte budget.
//!
//! Background: bundles with DLMM/Obric hops failed Jito with
//! `transaction #0 could not be decoded` because the ALT lacked the
//! DEX program ids + event-authority PDAs; the tx referenced them as
//! 32-byte static keys, blowing the size limit.
//!
//! ```text
//! cargo run --release --bin alt-extend-dlmm-obric
//! ```
//!
//! Reads EXECUTOR_NATIVE_ALT (the arbbot's runtime ALT, NOT the Pyth
//! liquidation ALT). One ExtendLookupTable tx, ~0.000005 SOL.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

use arb_scout::raw_tx::{
    build_and_sign_legacy_tx_b64, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix,
};
use arb_scout::wallet::Wallet;

const ADDRESS_LOOKUP_TABLE_PROGRAM: Pubkey =
    pubkey!("AddressLookupTab1e1111111111111111111111111");
const SYSTEM_PROGRAM: Pubkey =
    pubkey!("11111111111111111111111111111111");

// Globals referenced by every DLMM swap-IX:
const METEORA_DLMM_PROGRAM: Pubkey =
    pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const SPL_MEMO_PROGRAM: Pubkey =
    pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

// Globals referenced by every Obric swap-IX:
const OBRIC_V2_PROGRAM: Pubkey =
    pubkey!("obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y");

const DLMM_EVENT_AUTHORITY_SEED: &[u8] = b"__event_authority";

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
        .context("EXECUTOR_NATIVE_ALT must be set — the arbbot runtime ALT")?;
    let alt_pk = Pubkey::from_str(&alt_pk_str)?;
    println!("Extending ALT: {alt_pk}");

    // DLMM event_authority: ["__event_authority"] PDA under DLMM program.
    let (dlmm_event_authority, _bump) = Pubkey::find_program_address(
        &[DLMM_EVENT_AUTHORITY_SEED],
        &METEORA_DLMM_PROGRAM,
    );

    // 2026-05-15: live ALT inspection showed 3/4 globals already present
    // (DLMM_PROGRAM, SPL_MEMO, DLMM_EVT_AUTH) and only 1 slot free in
    // the 256-cap ALT. Restricted this run to OBRIC_PROGRAM only.
    let _ = (METEORA_DLMM_PROGRAM, SPL_MEMO_PROGRAM, dlmm_event_authority);
    let mut addrs: Vec<Pubkey> = vec![
        OBRIC_V2_PROGRAM,
    ];

    // Dedupe preserving order.
    let mut seen = std::collections::HashSet::new();
    addrs.retain(|a| seen.insert(*a));

    println!("\nAdding {} addresses:", addrs.len());
    for a in &addrs { println!("  {a}"); }
    println!("\n(dlmm_event_authority derived from DLMM_PROGRAM with seed \"__event_authority\")");

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
