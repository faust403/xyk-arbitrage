//! Fetch the FLASHX program's Anchor IDL from chain.
//!
//! Anchor stores IDL JSON in a PDA derived as:
//!   program_signer = find_program_address(&[], program_id).0
//!   idl_addr       = create_with_seed(program_signer, "anchor:idl", program_id)
//!
//! Layout of the IDL account:
//!   [0..8]    discriminator (anchor)
//!   [8..40]   authority pubkey
//!   [40..44]  data length (u32 LE)
//!   [44..56]  reserved
//!   [56..]    zlib-compressed JSON IDL
//!
//! Usage:
//!   cargo run --release --bin flashx-idl -p arb-scout
//! Output:
//!   flashx-idl.json (decompressed JSON)

use std::str::FromStr;

use solana_program::pubkey::Pubkey;

const FLASHX_PROGRAM: &str = "FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9";
const HELIUS_RPC: &str = "https://api.mainnet-beta.solana.com";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let program_id = Pubkey::from_str(FLASHX_PROGRAM)?;

    // Step 1: derive program_signer = find_program_address([], program_id).0
    let (program_signer, _bump) = Pubkey::find_program_address(&[], &program_id);
    println!("program_signer: {}", program_signer);

    // Step 2: idl_addr = create_with_seed(program_signer, "anchor:idl", program_id)
    let idl_addr = Pubkey::create_with_seed(&program_signer, "anchor:idl", &program_id)?;
    println!("idl_addr: {}", idl_addr);

    // Step 3: fetch the account from RPC.
    let http = reqwest::Client::new();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [idl_addr.to_string(),
            {"encoding": "base64", "commitment": "confirmed"}]
    });
    let resp: serde_json::Value = http.post(HELIUS_RPC).json(&body).send().await?
        .json().await?;
    let data_b64 = match resp.pointer("/result/value/data/0").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            println!("\nIDL account NOT FOUND. FLASHX may not have published its IDL on-chain.");
            println!("Full response: {}", serde_json::to_string_pretty(&resp)?);
            println!("\nFallback path:");
            println!("  1. Reverse-engineer from a real tx — see flashx-decode-tx.rs");
            println!("  2. OR check if IDL is published off-chain (their docs/website)");
            return Ok(());
        }
    };

    let data = base64::engine::Engine::decode(
        &base64::engine::general_purpose::STANDARD, data_b64,
    )?;
    println!("\nIDL account data: {} bytes", data.len());

    if data.len() < 44 {
        return Err("account too short for an Anchor IDL".into());
    }

    let authority_bytes: [u8; 32] = data[8..40].try_into()?;
    let authority = Pubkey::new_from_array(authority_bytes);
    let data_len = u32::from_le_bytes(data[40..44].try_into()?) as usize;
    println!("authority: {}", authority);
    println!("compressed IDL length: {} bytes", data_len);

    if 44 + data_len > data.len() {
        return Err("declared IDL length exceeds account data".into());
    }
    let compressed = &data[44..44 + data_len];

    // Step 4: zlib-decompress.
    use std::io::Read;
    let mut decoder = flate2::read::ZlibDecoder::new(compressed);
    let mut json_bytes = Vec::new();
    decoder.read_to_end(&mut json_bytes)?;
    let json_str = String::from_utf8(json_bytes)?;

    // Pretty-print and save.
    let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
    let pretty = serde_json::to_string_pretty(&parsed)?;
    std::fs::write("flashx-idl.json", &pretty)?;
    println!("\nSaved decompressed IDL to flashx-idl.json ({} bytes)", pretty.len());

    // Print summary: instruction names + account counts.
    if let Some(name) = parsed.get("name") {
        println!("\nProgram name: {}", name);
    }
    if let Some(version) = parsed.get("version") {
        println!("Version: {}", version);
    }
    if let Some(ixs) = parsed.get("instructions").and_then(|v| v.as_array()) {
        println!("\nInstructions ({} total):", ixs.len());
        for ix in ixs {
            let ix_name = ix.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            let n_accs = ix.get("accounts").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            let n_args = ix.get("args").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            println!("  {} — {} accounts, {} args", ix_name, n_accs, n_args);
        }
    }

    Ok(())
}
