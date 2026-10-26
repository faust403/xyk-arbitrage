//! One-shot helper: creates a MarginfiAccount on mainnet for the wallet
//! configured in `.env` (`SOLANA_KEYPAIR_JSON`). Prints the resulting
//! account pubkey for use as the `MARGINFI_ACCOUNT` env var.
//!
//! ```
//! cargo run --release --bin marginfi-init
//! ```
//!
//! ## What it does
//!
//! MarginFi v2's `marginfi_account_initialize` instruction creates a new
//! `MarginfiAccount` PDA-style account that holds the user's positions
//! (zero positions on init). The account is allocated via the System
//! program with a fresh keypair, then handed off to MarginFi to populate.
//!
//! Because the account is freshly allocated, **two signatures are
//! required**: the user's wallet (authority + payer) AND the
//! marginfi_account keypair (consents to its own creation). Our
//! `raw_tx::sign_legacy_tx_b64` is single-sig only, so this binary
//! re-implements legacy-message signing inline for the 2-signer case.
//!
//! Cost: ~0.0017 SOL for the account's rent-exempt minimum + ~0.000005 SOL
//! base fee. Wallet must hold both that AND the SOL for ongoing fees.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use arb_scout::raw_tx::{encode_compact_u16, fetch_latest_blockhash, CompiledInstruction};
use arb_scout::wallet::Wallet;

const MARGINFI_V2_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";
const MARGINFI_MAIN_GROUP: &str = "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let api_key = std::env::var("HELIUS_API_KEY")
        .context("HELIUS_API_KEY missing")?;
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    let wallet = Wallet::from_env().context("loading wallet")?
        .ok_or_else(|| anyhow!("no wallet — set SOLANA_KEYPAIR_JSON"))?;
    let payer = wallet.pubkey();
    println!("Wallet: {payer}");

    // 1) Generate a fresh ed25519 keypair for the new MarginfiAccount.
    use rand::rngs::OsRng;
    use rand::RngCore;
    let mut secret_seed = [0u8; 32];
    OsRng.fill_bytes(&mut secret_seed);
    let mfi_signing = SigningKey::from_bytes(&secret_seed);
    let mfi_account = Pubkey::new_from_array(mfi_signing.verifying_key().to_bytes());
    let mut mfi_keypair_bytes = [0u8; 64];
    mfi_keypair_bytes[..32].copy_from_slice(&secret_seed);
    mfi_keypair_bytes[32..].copy_from_slice(&mfi_signing.verifying_key().to_bytes());

    println!();
    println!("Generated MarginfiAccount: {mfi_account}");
    println!("  (fresh keypair, used only for the init signature)");

    // 2) Build the init instruction.
    let group     = Pubkey::from_str(MARGINFI_MAIN_GROUP)?;
    let mfi_prog  = Pubkey::from_str(MARGINFI_V2_PROGRAM)?;
    let system    = solana_program::system_program::id();

    // Discriminator: sha256("global:marginfi_account_initialize")[..8]
    let disc = anchor_disc("global:marginfi_account_initialize");
    println!("  init discriminator: {}", hex(&disc));

    let init_ix = Instruction {
        program_id: mfi_prog,
        accounts: vec![
            AccountMeta::new_readonly(group, false),       // 0  marginfi_group
            AccountMeta::new(mfi_account, true),           // 1  marginfi_account (W,S — init)
            AccountMeta::new_readonly(payer, true),        // 2  authority (S)
            AccountMeta::new(payer, true),                 // 3  fee_payer (W,S)
            AccountMeta::new_readonly(system, false),      // 4  system_program
        ],
        data: disc.to_vec(),
    };

    // 3) Build a legacy message with TWO signers (payer + mfi_account).
    let blockhash = fetch_latest_blockhash(&rpc_url).await?;
    let signed_b64 = sign_legacy_2sig(&payer, &mfi_account, &init_ix, &blockhash, wallet.bytes(), &mfi_keypair_bytes)?;

    // 4) Submit.
    println!();
    println!("Submitting init tx …");
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64,{
            "encoding":"base64","skipPreflight":true,
            "preflightCommitment":"processed",
        }],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&body).send().await?.json().await?;
    if let Some(err) = r.pointer("/error") {
        return Err(anyhow!("sendTransaction error: {err}"));
    }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing sig: {r}"))?;
    println!("  → tx signature: {sig}");

    // 5) Wait for confirmation.
    println!("Waiting for confirmation …");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
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
                        return Err(anyhow!("tx failed: {err}\n  Signature: {sig}\n  Possible causes: wrong group pubkey, init disc, account-order mismatch."));
                    }
                }
                let conf = s.pointer("/confirmationStatus")
                    .and_then(|v| v.as_str()).unwrap_or("");
                if conf == "confirmed" || conf == "finalized" {
                    println!();
                    println!("══════════════════════════════════════════════════════════════════");
                    println!("MarginfiAccount created: {mfi_account}");
                    println!();
                    println!("Add to Railway env:");
                    println!("  MARGINFI_ACCOUNT={mfi_account}");
                    println!("══════════════════════════════════════════════════════════════════");
                    return Ok(());
                }
            }
        }
    }
    Err(anyhow!("tx not confirmed within 60s\n  Signature: {sig}\n  Check on Solscan: https://solscan.io/tx/{sig}"))
}

fn anchor_disc(preimage: &str) -> [u8; 8] {
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

/// Hand-rolled legacy message signing for the 2-signer init case.
/// Both `payer` and `mfi_account` are at the head of static_account_keys
/// (they're both writable signers). Returns base64 of `[2-byte sig count
/// | sig1 | sig2 | message]`.
fn sign_legacy_2sig(
    payer:        &Pubkey,
    mfi_account:  &Pubkey,
    ix:           &Instruction,
    blockhash:    &[u8; 32],
    payer_kp:     &[u8; 64],
    mfi_kp:       &[u8; 64],
) -> anyhow::Result<String> {
    use std::collections::HashMap;
    // Build the unique-account list with payer at slot 0, mfi_account at
    // slot 1 (both writable signers), then everything else.
    let mut writable_signers = vec![*payer, *mfi_account];
    let mut readonly_signers: Vec<Pubkey> = Vec::new();
    let mut writable_nonsigners: Vec<Pubkey> = Vec::new();
    let mut readonly_nonsigners: Vec<Pubkey> = Vec::new();

    let mut all: HashMap<Pubkey, (bool, bool)> = HashMap::new();
    all.insert(*payer, (true, true));
    all.insert(*mfi_account, (true, true));
    for am in &ix.accounts {
        let entry = all.entry(am.pubkey).or_insert((false, false));
        if am.is_signer { entry.0 = true; }
        if am.is_writable { entry.1 = true; }
    }
    all.entry(ix.program_id).or_insert((false, false));

    for (pk, (signer, writable)) in all.iter() {
        if *pk == *payer || *pk == *mfi_account { continue; }
        match (*signer, *writable) {
            (true, true)   => writable_signers.push(*pk),
            (true, false)  => readonly_signers.push(*pk),
            (false, true)  => writable_nonsigners.push(*pk),
            (false, false) => readonly_nonsigners.push(*pk),
        }
    }
    readonly_signers.sort();
    writable_nonsigners.sort();
    readonly_nonsigners.sort();

    let mut keys = Vec::with_capacity(
        writable_signers.len() + readonly_signers.len()
        + writable_nonsigners.len() + readonly_nonsigners.len()
    );
    keys.extend(writable_signers.iter().copied());
    keys.extend(readonly_signers.iter().copied());
    keys.extend(writable_nonsigners.iter().copied());
    keys.extend(readonly_nonsigners.iter().copied());

    let header_num_required = (2 + readonly_signers.len()) as u8;
    let header_ro_signed    = readonly_signers.len() as u8;
    let header_ro_unsigned  = readonly_nonsigners.len() as u8;

    let key_index: HashMap<Pubkey, u8> = keys.iter().enumerate()
        .map(|(i, k)| (*k, i as u8)).collect();

    let compiled = vec![CompiledInstruction {
        program_id_index: *key_index.get(&ix.program_id).unwrap(),
        account_indices: ix.accounts.iter().map(|am| *key_index.get(&am.pubkey).unwrap()).collect(),
        data: ix.data.clone(),
    }];

    // Serialize the legacy message.
    let mut msg = Vec::new();
    msg.push(header_num_required);
    msg.push(header_ro_signed);
    msg.push(header_ro_unsigned);
    msg.extend(encode_compact_u16(keys.len() as u16));
    for k in &keys { msg.extend_from_slice(k.as_ref()); }
    msg.extend_from_slice(blockhash);
    msg.extend(encode_compact_u16(compiled.len() as u16));
    for ci in &compiled {
        msg.push(ci.program_id_index);
        msg.extend(encode_compact_u16(ci.account_indices.len() as u16));
        msg.extend_from_slice(&ci.account_indices);
        msg.extend(encode_compact_u16(ci.data.len() as u16));
        msg.extend_from_slice(&ci.data);
    }

    // Sign with both keys.
    let payer_seed: [u8; 32] = payer_kp[..32].try_into().unwrap();
    let mfi_seed:   [u8; 32] = mfi_kp[..32].try_into().unwrap();
    let payer_signing = SigningKey::from_bytes(&payer_seed);
    let mfi_signing   = SigningKey::from_bytes(&mfi_seed);

    let sig_payer = payer_signing.sign(&msg);
    let sig_mfi   = mfi_signing.sign(&msg);

    // Order signatures by their key's slot in static_account_keys.
    // Payer is at slot 0, mfi at slot 1, so payer first.
    let mut tx = Vec::new();
    tx.extend(encode_compact_u16(2));
    tx.extend_from_slice(&sig_payer.to_bytes());
    tx.extend_from_slice(&sig_mfi.to_bytes());
    tx.extend(msg);
    Ok(base64::engine::general_purpose::STANDARD.encode(tx))
}
