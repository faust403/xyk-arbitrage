//! One-shot helper: dumps the on-chain MarginFi v2 USDC bank account so we
//! can extract the addresses + the origination-fee rate before committing
//! to the full flash-loan integration.
//!
//! ```text
//! cargo run --release --bin marginfi-probe
//! ```
//!
//! ## What it prints
//!
//! 1. Raw account size + Anchor discriminator (first 8 bytes).
//! 2. Every 32-byte chunk as both hex AND as-if-pubkey base58. Anything
//!    that looks like a real Pubkey is printed alongside its decoded value.
//! 3. The Bank struct's well-known fields at their guessed offsets:
//!    - mint (offset 8)
//!    - mint_decimals (offset 40)
//!    - group (offset 48)
//!    - vaults / vault authorities (later offsets)
//! 4. Sweep candidate I80F48 fixed-point values throughout the trailing
//!    config block. The `origination_fee_rate` is one of these — we
//!    identify it by being a small fraction (typical 0.0..0.01).

use std::str::FromStr;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::pubkey::Pubkey;

const MARGINFI_V2_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";

/// Well-known MarginFi v2 USDC bank on mainnet. If this turns out to be
/// wrong (the program got migrated or this is a stale id), the probe
/// exits with a clear error and the operator can pass a different bank
/// via the `MARGINFI_USDC_BANK` env var.
const DEFAULT_USDC_BANK: &str = "2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let api_key = std::env::var("HELIUS_API_KEY")
        .context("HELIUS_API_KEY missing (set in .env or environment)")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    let bank_pk = std::env::var("MARGINFI_USDC_BANK")
        .unwrap_or_else(|_| DEFAULT_USDC_BANK.to_string());
    let bank_pk = Pubkey::from_str(bank_pk.trim())?;
    println!("Probing MarginFi USDC bank: {bank_pk}");

    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[bank_pk.to_string(),{"encoding":"base64","commitment":"confirmed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(&rpc).json(&body).send().await?.json().await?;
    let value = r.pointer("/result/value")
        .ok_or_else(|| anyhow!("no result/value: {r}"))?;
    if value.is_null() {
        return Err(anyhow!(
            "Bank account does not exist on chain. Override with MARGINFI_USDC_BANK=<pubkey> if {DEFAULT_USDC_BANK} is wrong."
        ));
    }
    let owner = value.pointer("/owner").and_then(|v| v.as_str()).unwrap_or("");
    let data_b64 = value.pointer("/data/0").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing data: {r}"))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
    println!("Account owner: {owner}");
    println!("Account size:  {} bytes", raw.len());

    if owner != MARGINFI_V2_PROGRAM {
        eprintln!("⚠ owner ≠ MarginFi v2 program — this might not be a Bank account");
    }

    // ── Discriminator ────────────────────────────────────────────────
    let disc = &raw[0..8];
    println!();
    println!("Discriminator (hex): {}", hex(disc));
    let expected_disc = anchor_account_disc("Bank");
    println!("Expected for Bank:   {}", hex(&expected_disc));
    if disc == expected_disc {
        println!("  ✓ matches Anchor `Bank` account discriminator");
    } else {
        println!("  ! does not match expected Bank discriminator — layout below may be off");
    }

    // ── Pubkey scan ──────────────────────────────────────────────────
    println!();
    println!("Pubkey-shaped windows in account data (every 32-byte aligned offset):");
    let mut printed = 0;
    for off in (8..raw.len().saturating_sub(32)).step_by(1) {
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&raw[off..off + 32]);
        // Heuristic: most Solana pubkeys have a high-entropy distribution
        // of bytes (no long zero runs). Filter to skip obvious non-pubkeys.
        let non_zero = buf.iter().filter(|b| **b != 0).count();
        if non_zero < 20 { continue; }
        let pk = Pubkey::new_from_array(buf);
        let s = pk.to_string();
        // Only print plausibly-real ones (no `1111...` runs and not all zeros).
        if s.contains("1111111111111111111111") || s.contains("nu1111") { continue; }
        println!("  offset {:>4}  {s}", off);
        printed += 1;
        if printed > 30 { println!("  … (truncated)"); break; }
    }

    // ── I80F48 sweep ─────────────────────────────────────────────────
    // I80F48 = 128-bit signed fixed-point, 48 fractional bits. Values like
    // origination_fee_rate are typically very small (0.0001..0.01 range).
    println!();
    println!("Plausible I80F48 values (small positive, < 0.5) — origination_fee_rate is one of these:");
    let mut found = 0;
    for off in (8..raw.len().saturating_sub(16)).step_by(1) {
        let mut buf = [0u8; 16];
        buf.copy_from_slice(&raw[off..off + 16]);
        let raw_i128 = i128::from_le_bytes(buf);
        if raw_i128 <= 0 { continue; }
        // Convert to float: divide by 2^48
        let val = (raw_i128 as f64) / (1u128 << 48) as f64;
        if !val.is_finite() { continue; }
        if val <= 0.0 || val > 0.5 { continue; }
        // Common fee values are 0.0001..0.01 (1bp..100bp). Larger values
        // are likely interest rates (2-50% APR) or other params.
        let bps = val * 10_000.0;
        let label = if val < 0.001 { "<10bps (origination fee?)" }
                    else if val < 0.05 { "10-500bps (insurance/origination?)" }
                    else { "5%+ (interest rate / utilization?)" };
        println!("  offset {:>4}  raw={raw_i128:>30}  value={val:.10}  ({bps:.4}bps)  ← {label}", off);
        found += 1;
        if found > 25 { println!("  … (truncated)"); break; }
    }

    // ── Derive + verify the liquidity_vault PDA ──────────────────────
    let prog = Pubkey::from_str(MARGINFI_V2_PROGRAM)?;
    let candidate_seeds: &[(&[u8], &str)] = &[
        (b"liquidity_vault", "liquidity_vault"),
        (b"liquidity_vault_auth", "liquidity_vault_auth"),
        (b"insurance_vault", "insurance_vault"),
        (b"insurance_vault_auth", "insurance_vault_auth"),
        (b"fee_vault", "fee_vault"),
        (b"fee_vault_auth", "fee_vault_auth"),
    ];

    println!();
    println!("Derived PDAs (expected to be the actual vault addresses if seed is correct):");
    let http = reqwest::Client::new();
    let mut found_liquidity_vault: Option<Pubkey> = None;
    for (seed, label) in candidate_seeds {
        let (pda, bump) = Pubkey::find_program_address(&[seed, bank_pk.as_ref()], &prog);
        // Verify by fetching this PDA — if it exists and is a Token account
        // owned by SPL Token whose mint is USDC, it's the right one.
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
            "params":[pda.to_string(),{"encoding":"base64","commitment":"confirmed"}],
        });
        let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
        let value = r.pointer("/result/value");
        let info = match value {
            Some(v) if !v.is_null() => v,
            _ => {
                println!("  [{label:>22}]  {pda} bump={bump}  (DOES NOT EXIST on chain)");
                continue;
            }
        };
        let pda_owner = info.get("owner").and_then(|v| v.as_str()).unwrap_or("");
        let data_b64 = info.pointer("/data/0").and_then(|v| v.as_str()).unwrap_or("");
        let data = base64::engine::general_purpose::STANDARD.decode(data_b64).unwrap_or_default();
        let token_mint_at_0 = if data.len() >= 32 {
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&data[..32]);
            Pubkey::new_from_array(buf).to_string()
        } else { String::new() };
        let is_token_acct = pda_owner == "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
        let is_usdc = token_mint_at_0 == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
        let tag = if is_token_acct && is_usdc { " ← USDC TOKEN ACCOUNT (this is the vault)" }
                  else if is_token_acct { " ← Token account (different mint)" }
                  else { "" };
        println!("  [{label:>22}]  {pda} bump={bump}  owner={pda_owner}{tag}");
        if *label == "liquidity_vault" && is_token_acct && is_usdc {
            found_liquidity_vault = Some(pda);
        }
    }

    println!();
    if let Some(lv) = found_liquidity_vault {
        println!("══════════════════════════════════════════════════════════════════");
        println!("RESOLVED: MarginFi USDC liquidity_vault = {lv}");
        println!();
        println!("Add to Railway env:");
        println!("  MARGINFI_USDC_LIQUIDITY_VAULT={lv}");
        println!("══════════════════════════════════════════════════════════════════");
    } else {
        println!("⚠ Could not auto-resolve liquidity_vault via PDA. Look at the");
        println!("  pubkey list above and identify the USDC token account manually.");
    }
    println!();
    println!("Next steps:");
    println!("  • Identify which I80F48 entry above is `protocol_origination_fee` (typically ≤10bps).");
    println!("  • For mainnet the well-known MarginFi group is `4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG7`.");
    println!("  • If origination_fee < 5bps → MarginFi flash-loan implementation worth doing.");

    Ok(())
}

/// Anchor's account-discriminator preimage is `account:{StructName}`.
fn anchor_account_disc(name: &str) -> [u8; 8] {
    let preimage = format!("account:{name}");
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}
