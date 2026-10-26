//! Reverse-engineer the FLASHX program's instruction layout from real
//! successful transactions. Dumps:
//!   - The 8-byte instruction discriminator (Anchor convention)
//!   - The instruction data tail (likely a few u64 args)
//!   - The full ordered account list with writable/signer flags
//!   - Inner CPI calls (so we know which downstream programs are invoked)
//!
//! This is enough to rebuild a flashx_buy_ix() in Rust without an IDL.
//!
//! Usage:
//!   cargo run --release --bin flashx-decode -p arb-scout -- <SIG_1> [SIG_2 ...]

use std::str::FromStr;

const FLASHX_PROGRAM: &str = "FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9";
const HELIUS_RPC: &str = "https://api.mainnet-beta.solana.com";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let sigs: Vec<String> = std::env::args().skip(1).collect();
    if sigs.is_empty() {
        eprintln!("usage: flashx-decode <SIG_1> [SIG_2 ...]");
        std::process::exit(1);
    }

    let http = reqwest::Client::new();
    for sig in &sigs {
        println!("════════════════════════════════════════════════════════════");
        println!("TX: {}", sig);
        println!("════════════════════════════════════════════════════════════");

        // Fetch parsed form to get account names + IX args.
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
            "params": [sig, {
                "encoding": "jsonParsed",
                "maxSupportedTransactionVersion": 0,
                "commitment": "confirmed",
            }],
        });
        let resp: serde_json::Value = http.post(HELIUS_RPC).json(&body).send().await?
            .json().await?;
        let r = match resp.pointer("/result") {
            Some(v) if !v.is_null() => v,
            _ => { eprintln!("  not found"); continue; }
        };

        // Top-level IX list (parsed form).
        let ixs = r.pointer("/transaction/message/instructions")
            .and_then(|v| v.as_array());
        let inner_ixs = r.pointer("/meta/innerInstructions")
            .and_then(|v| v.as_array());

        if let Some(arr) = ixs {
            println!("\nTop-level IXs:");
            for (i, ix) in arr.iter().enumerate() {
                let pid = ix.get("programId").and_then(|v| v.as_str()).unwrap_or("?");
                let is_flashx = pid == FLASHX_PROGRAM;
                let marker = if is_flashx { " ◀── FLASHX" } else { "" };
                println!("  IX[{}] pid={}{}", i, pid, marker);

                if is_flashx {
                    // Get raw form to extract data bytes + account flags.
                    decode_flashx_ix(ix, r).await?;
                }

                // Show inner CPIs called by THIS top-level IX.
                if let Some(inners) = inner_ixs {
                    for group in inners {
                        if group.get("index").and_then(|v| v.as_u64()) == Some(i as u64) {
                            if let Some(arr) = group.get("instructions").and_then(|v| v.as_array()) {
                                for inner in arr {
                                    let ipid = inner.get("programId").and_then(|v| v.as_str()).unwrap_or("?");
                                    let parsed = inner.get("parsed");
                                    let kind = parsed.and_then(|p| p.get("type")).and_then(|v| v.as_str())
                                        .unwrap_or("raw");
                                    println!("    └─CPI→ {} ({})", &ipid[..ipid.len().min(44)], kind);
                                }
                            }
                        }
                    }
                }
            }
        }

        println!();
    }
    Ok(())
}

async fn decode_flashx_ix(
    parsed_ix: &serde_json::Value,
    full_tx: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    // In jsonParsed form, FLASHX shows as raw with `data` field base58-encoded.
    let data_b58 = parsed_ix.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let data = bs58::decode(data_b58).into_vec().unwrap_or_default();

    println!("    data len: {} bytes", data.len());
    if data.len() >= 8 {
        let disc = &data[..8];
        println!("    discriminator (hex): {}", hex_join(disc));
        // Anchor disc = first 8 bytes of sha256("global:<ix_name>"). Without
        // the IDL we can't reverse it, but we can show the rest as args.
        let rest = &data[8..];
        println!("    args ({} bytes, hex): {}", rest.len(), hex_join(rest));
        // Try common arg shapes.
        if rest.len() >= 8 {
            let u64_first = u64::from_le_bytes(rest[..8].try_into().unwrap());
            println!("    args[0] as u64 LE: {} (= {:.6} SOL if lamports)", u64_first, u64_first as f64 / 1e9);
        }
        if rest.len() >= 16 {
            let u64_second = u64::from_le_bytes(rest[8..16].try_into().unwrap());
            println!("    args[1] as u64 LE: {} (= {} bps if slippage)", u64_second, u64_second);
        }
    }

    // Account list with their writable/signer flags. In jsonParsed v0
    // accountKeys is a list of {pubkey, signer, writable, source} objects.
    let accounts = parsed_ix.get("accounts").and_then(|v| v.as_array())
        .ok_or("ix missing accounts")?;
    let all_keys = full_tx.pointer("/transaction/message/accountKeys")
        .and_then(|v| v.as_array()).ok_or("missing accountKeys")?;

    println!("\n    Account list ({} accounts):", accounts.len());
    for (i, acc_str) in accounts.iter().enumerate() {
        let pk = acc_str.as_str().unwrap_or("");
        // Look up the matching key object to get its flags.
        let key_obj = all_keys.iter().find(|k| {
            k.get("pubkey").and_then(|p| p.as_str()) == Some(pk)
                || k.as_str() == Some(pk)
        });
        let (writable, signer) = match key_obj {
            Some(obj) => (
                obj.get("writable").and_then(|v| v.as_bool()).unwrap_or(false),
                obj.get("signer").and_then(|v| v.as_bool()).unwrap_or(false),
            ),
            None => (false, false),
        };
        let flags = format!("{}{}",
            if writable { "W" } else { "-" },
            if signer { "S" } else { "-" },
        );
        // Annotate well-known pubkeys.
        let annotation = annotate_pubkey(pk);
        println!("      [{:2}] {} {}{}", i, flags, pk, annotation);
    }

    Ok(())
}

fn annotate_pubkey(pk: &str) -> String {
    match pk {
        "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P" => " (PUMP_FUN_PROGRAM)".into(),
        "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" => " (SPL_TOKEN)".into(),
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL" => " (ATA_PROGRAM)".into(),
        "11111111111111111111111111111111" => " (SYSTEM_PROGRAM)".into(),
        "ComputeBudget111111111111111111111111111111" => " (COMPUTE_BUDGET)".into(),
        "SysvarRent111111111111111111111111111111111" => " (RENT_SYSVAR)".into(),
        "Sysvar1nstructions1111111111111111111111111" => " (INSTRUCTIONS_SYSVAR)".into(),
        "So11111111111111111111111111111111111111112" => " (WSOL_MINT)".into(),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => " (USDC_MINT)".into(),
        _ => {
            if pk.ends_with("pump") { " (← pump.fun MINT)".into() }
            else { String::new() }
        }
    }
}

fn hex_join(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join("")
}
