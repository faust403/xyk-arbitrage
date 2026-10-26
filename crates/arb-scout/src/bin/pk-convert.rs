//! One-shot helper: convert Phantom's exported base58 secret-key into the
//! 64-byte JSON-array format that `SOLANA_KEYPAIR_JSON` expects on Railway.
//!
//! Usage:
//!   1. In Phantom → Account menu → Show Private Key → copy the base58 string.
//!   2. Run: `cargo run --release --bin pk-convert -p arb-scout`
//!   3. Paste the base58 key when prompted (it WILL NOT echo to the terminal).
//!   4. Copy the printed `[1,2,...,64]` line and set it as the env var:
//!         railway variables --set SOLANA_KEYPAIR_JSON='[..paste..]'
//!
//! Security:
//!   - The base58 input is never echoed to the screen.
//!   - The pubkey is derived locally and printed so you can verify it matches
//!     the wallet you intend to use BEFORE pushing to Railway.
//!   - No network calls — the key never leaves your machine via this tool.

use std::io::{self, BufRead, Write};

fn main() {
    println!("Phantom → Railway private-key converter");
    println!("=======================================");
    println!();
    println!("In Phantom: ⋮ menu → 'Show private key' → password → copy.");
    println!("Paste the base58 secret here. It will not be echoed.");
    print!("\nbase58 key: ");
    let _ = io::stdout().flush();

    // Read one line from stdin (no echo suppression — Windows terminals
    // behave inconsistently with `rpassword`. The recommendation is to
    // run this from a private session and clear the scrollback after).
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line).expect("stdin read");
    let trimmed = line.trim();
    if trimmed.is_empty() {
        eprintln!("ERROR: empty input");
        std::process::exit(1);
    }

    // Decode base58 → 64 raw bytes.
    let bytes = match bs58::decode(trimmed).into_vec() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("\nERROR: base58 decode failed: {e}");
            eprintln!("Make sure you copied the FULL base58 string from Phantom (no spaces, no quotes).");
            std::process::exit(2);
        }
    };
    if bytes.len() != 64 {
        eprintln!("\nERROR: decoded length is {} bytes, expected 64.", bytes.len());
        eprintln!("Phantom should export 64 bytes (32 secret + 32 pubkey).");
        std::process::exit(3);
    }

    // Derive pubkey for verification (bytes[32..64] is the public-key half).
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&bytes[32..64]);
    let pubkey_b58 = bs58::encode(&pk).into_string();

    // Print the JSON array — this is what goes into Railway.
    let json: String = format!("[{}]",
        bytes.iter().map(|b| b.to_string()).collect::<Vec<_>>().join(","));

    println!("\n──── COPY THE LINE BELOW INTO RAILWAY ────────────────────────────");
    println!("{}", json);
    println!("──────────────────────────────────────────────────────────────────");
    println!();
    println!("Public key (verify this matches your Phantom wallet):");
    println!("  {}", pubkey_b58);
    println!();
    println!("Next steps:");
    println!("  1. Compare the pubkey above with the one Phantom shows.");
    println!("  2. railway variables --set SOLANA_KEYPAIR_JSON='<paste the JSON>'");
    println!("  3. Fund the pubkey from a CEX (NOT from the old wallet).");
    println!("  4. railway up");
}
