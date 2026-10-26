//! One-shot helper: derive the MarginFi USDT bank's `liquidity_vault` PDA
//! and verify it exists on chain as an SPL Token account whose mint matches
//! USDT. Output is suitable to paste into Railway as
//! `MARGINFI_USDT_LIQUIDITY_VAULT=…`.
//!
//! ```
//! cargo run --release --bin marginfi-usdt-vault
//! ```

use std::str::FromStr;
use anyhow::{anyhow, Context};
use solana_program::pubkey::Pubkey;

const MARGINFI_V2_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";
const MARGINFI_USDT_BANK:  &str = "HmpMfL8942u22htC4EMiWgLX931g3sacXFR6KjuLgKLV";
const USDT_MINT:           &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY missing")?;
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    let prog = Pubkey::from_str(MARGINFI_V2_PROGRAM)?;
    let bank = Pubkey::from_str(MARGINFI_USDT_BANK)?;
    let usdt = Pubkey::from_str(USDT_MINT)?;

    println!("MarginFi USDT bank: {bank}");
    println!();

    // Derive the four PDAs we care about. Same seeds as USDC bank.
    let candidates: &[(&[u8], &str)] = &[
        (b"liquidity_vault",      "liquidity_vault"),
        (b"liquidity_vault_auth", "liquidity_vault_auth"),
        (b"insurance_vault",      "insurance_vault"),
        (b"fee_vault",            "fee_vault"),
    ];

    let client = reqwest::Client::new();
    let mut resolved: Option<Pubkey> = None;

    for (seed, label) in candidates {
        let (pda, bump) = Pubkey::find_program_address(&[seed, bank.as_ref()], &prog);
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
            "params":[pda.to_string(),{"encoding":"base64"}],
        });
        let resp: serde_json::Value = client.post(&rpc_url)
            .json(&body).send().await?.json().await?;

        let exists = resp.pointer("/result/value").map(|v| !v.is_null()).unwrap_or(false);
        let owner = resp.pointer("/result/value/owner").and_then(|v| v.as_str()).unwrap_or("");
        let is_token = owner == "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

        let mint_match = if is_token {
            // SPL Token account: mint at offset 0..32 of data.
            let data_b64 = resp.pointer("/result/value/data/0").and_then(|v| v.as_str()).unwrap_or("");
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64).unwrap_or_default();
            if bytes.len() >= 32 {
                let mut m = [0u8; 32];
                m.copy_from_slice(&bytes[..32]);
                Pubkey::new_from_array(m) == usdt
            } else { false }
        } else { false };

        println!(
            "  {label:<24} bump={bump:<3} pda={pda} exists={exists} owner_is_spl={is_token} mint=USDT? {mint_match}"
        );

        if *label == "liquidity_vault" && is_token && mint_match {
            resolved = Some(pda);
        }
    }

    println!();
    if let Some(lv) = resolved {
        println!("══════════════════════════════════════════════════════════════════");
        println!("RESOLVED: MarginFi USDT liquidity_vault = {lv}");
        println!();
        println!("Add to Railway env:");
        println!("  MARGINFI_USDT_LIQUIDITY_VAULT={lv}");
        println!("══════════════════════════════════════════════════════════════════");
        Ok(())
    } else {
        Err(anyhow!("could not auto-resolve liquidity_vault — manual inspection needed"))
    }
}
