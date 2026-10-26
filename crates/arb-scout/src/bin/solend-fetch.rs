//! One-shot helper: fetches Solend's USDC reserve account from mainnet,
//! decodes the addresses we need for `FLASH_LOAN_ENABLED`, and prints them
//! in env-var form.
//!
//! Run:
//! ```
//! cargo run --release --bin solend-fetch
//! ```
//!
//! Reads `HELIUS_API_KEY` from the environment (or `.env` via dotenvy).
//!
//! ## Solend Reserve layout (Solend v0.5)
//!
//! From `solendprotocol/solana-program-library/token-lending/program/src/state/reserve.rs`:
//!
//! ```text
//! offset  size  field
//!     0     1   version
//!     1     8   last_update.slot
//!     9     1   last_update.stale
//!    10    32   lending_market
//!    42    32   liquidity.mint_pubkey
//!    74     1   liquidity.mint_decimals
//!    75    32   liquidity.supply_pubkey                    ← we want this
//!   107    32   liquidity.pyth_oracle_pubkey               ← we want this
//!   139    32   liquidity.switchboard_oracle_pubkey        ← we want this
//!   ... (numeric fields)
//!   config.fee_receiver lives in the trailing config block ← we want this
//! ```
//!
//! Reserve total account size in production is ~619 bytes (older versions
//! could be 248 + collateral + config padding). The fee_receiver offset
//! shifts with version updates, so we PROBE several plausible offsets and
//! print all candidates — operator picks the one Solscan confirms is a
//! Token Account.

use std::str::FromStr;

use base64::Engine;
use solana_program::pubkey::Pubkey;

const SOLEND_PROGRAM:   &str = "So1endDq2YkqhipRh3WViPa8hdiSpxWy6z3Z6tMCpAo";
const LENDING_MARKET:   &str = "4UpD2fh7xH3VP9QQaXtsS1YY3bxzWhtfpks7FatyKvdY";
const USDC_RESERVE:     &str = "BgxfHJDzm44T7XG68MYKx7YisTjZu73tVovyZSjJMpmw";
const USDC_MINT:        &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

const O_LENDING_MARKET:      usize = 10;
const O_LIQUIDITY_MINT:      usize = 42;
const O_LIQUIDITY_SUPPLY:    usize = 75;
const O_PYTH_ORACLE:         usize = 107;
const O_SWITCHBOARD_ORACLE:  usize = 139;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Best-effort .env load.
    let _ = dotenvy::dotenv();
    let api_key = std::env::var("HELIUS_API_KEY")
        .map_err(|_| anyhow::anyhow!("HELIUS_API_KEY not set (try `dotenvy::dotenv()` or export it)"))?;
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    println!("Fetching Solend USDC reserve {} …", USDC_RESERVE);
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [
            USDC_RESERVE,
            {"encoding": "base64", "commitment": "confirmed"},
        ],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&body).send().await?
        .json().await?;

    let data_b64 = resp.pointer("/result/value/data/0")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("missing data field; full response: {resp}"))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
    println!("  → {} bytes", raw.len());

    let read_pk = |off: usize| -> Option<Pubkey> {
        if off + 32 > raw.len() { return None; }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(&raw[off..off + 32]);
        Some(Pubkey::new_from_array(buf))
    };

    // Sanity-check the fixed-position fields.
    let lm_in_data = read_pk(O_LENDING_MARKET).expect("lending_market");
    let mint_in_data = read_pk(O_LIQUIDITY_MINT).expect("liquidity_mint");
    println!();
    println!("Sanity:");
    println!("  lending_market in data: {}", lm_in_data);
    println!("  expected:               {}", LENDING_MARKET);
    println!("  match: {}", lm_in_data.to_string() == LENDING_MARKET);
    println!("  mint in data:           {}", mint_in_data);
    println!("  expected (USDC):        {}", USDC_MINT);
    println!("  match: {}", mint_in_data.to_string() == USDC_MINT);

    if lm_in_data.to_string() != LENDING_MARKET || mint_in_data.to_string() != USDC_MINT {
        anyhow::bail!("Sanity check failed — Solend's reserve layout has likely changed. \
                       The known offsets above are stale. Bailing rather than print bad addresses.");
    }

    let supply        = read_pk(O_LIQUIDITY_SUPPLY).unwrap();
    let pyth          = read_pk(O_PYTH_ORACLE).unwrap();
    let switchboard   = read_pk(O_SWITCHBOARD_ORACLE).unwrap();

    // PDA: derive lending_market_authority.
    let lm   = Pubkey::from_str(LENDING_MARKET)?;
    let prog = Pubkey::from_str(SOLEND_PROGRAM)?;
    let (lm_authority, _bump) = Pubkey::find_program_address(&[lm.as_ref()], &prog);

    // Pass the on-chain `switchboard_oracle` value VERBATIM, even when it
    // looks like the `nu1111…` null sentinel that Solend uses for USDC.
    // `RefreshReserve` does a strict key-equality check between the
    // passed-in account and `reserve.liquidity.switchboard_oracle`; the
    // stored value is the placeholder, so we MUST pass the same placeholder
    // or Solend rejects with `Custom(42) = InvalidOracleConfig`. Solana's
    // runtime treats non-existent accounts as default-empty (the
    // placeholder doesn't exist on-chain), and Solend's processor only
    // dereferences the account data when the key check would have failed —
    // so a non-existent account here is fine.
    println!();
    println!("# ─── Add to Railway env (or .env) ─────────────────────────────");
    println!("FLASH_LOAN_ENABLED=true");
    println!("SOLEND_LENDING_MARKET={LENDING_MARKET}");
    println!("SOLEND_LENDING_MARKET_AUTHORITY={lm_authority}");
    println!("SOLEND_USDC_RESERVE={USDC_RESERVE}");
    println!("SOLEND_USDC_RESERVE_LIQUIDITY_SUPPLY={supply}");
    println!("SOLEND_USDC_PYTH_ORACLE={pyth}");
    println!("SOLEND_USDC_SWITCHBOARD_ORACLE={switchboard}  # null sentinel — Solend strict-checks against this exact value");

    // Solend's config block layout shifts when new numeric fields are added
    // upstream. Rather than guess offsets, SWEEP every byte position in the
    // trailing region of the reserve account, and ask RPC which 32-byte
    // window decodes to a USDC token account. The collateral.mint_pubkey and
    // collateral.supply_pubkey will also match Token-Program ownership; we
    // distinguish fee_receiver by the fact that ITS mint is USDC, while the
    // collateral pubkeys point at the cToken (cUSDC) mint and supply.
    let token_program = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    let client = reqwest::Client::new();
    let mut verified: Option<Pubkey> = None;

    let sweep_start = 240_usize;
    let sweep_end = raw.len().saturating_sub(32);
    let candidates: Vec<(usize, Pubkey)> = (sweep_start..=sweep_end)
        .filter_map(|off| read_pk(off).map(|pk| (off, pk)))
        .filter(|(_, pk)| {
            let s = pk.to_string();
            s != "11111111111111111111111111111111"
                && !s.starts_with("nu1111")  // Solend null sentinel
        })
        .collect();

    println!();
    println!("Sweeping {} byte offsets in [{}..{}] for fee_receiver …",
             candidates.len(), sweep_start, sweep_end);

    // getMultipleAccounts batches up to 100.
    for chunk in candidates.chunks(100) {
        let pks: Vec<String> = chunk.iter().map(|(_, pk)| pk.to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getMultipleAccounts",
            "params": [pks, {"encoding": "base64", "commitment": "confirmed"}],
        });
        let r: serde_json::Value = client.post(&rpc_url).json(&body).send().await?.json().await?;
        let arr = match r.pointer("/result/value").and_then(|v| v.as_array()) {
            Some(a) => a,
            None => { eprintln!("getMultipleAccounts returned no array: {r}"); continue; }
        };
        for (i, acct) in arr.iter().enumerate() {
            if acct.is_null() { continue; }
            let owner = acct.get("owner").and_then(|v| v.as_str()).unwrap_or("");
            if owner != token_program { continue; }
            let data_b64 = acct.pointer("/data/0").and_then(|v| v.as_str()).unwrap_or("");
            let data = base64::engine::general_purpose::STANDARD.decode(data_b64).unwrap_or_default();
            if data.len() < 165 { continue; }  // SPL Token Account is 165 bytes
            let mut buf = [0u8; 32];
            buf.copy_from_slice(&data[..32]);
            let mint = Pubkey::new_from_array(buf).to_string();
            let (off, pk) = chunk[i];
            let is_usdc = mint == USDC_MINT;
            println!("  HIT  offset {:>4}  {}  mint={}{}",
                     off, pk, mint,
                     if is_usdc { "  ← USDC" } else { "" });
            if is_usdc && verified.is_none() {
                verified = Some(pk);
            }
        }
    }

    println!();
    match verified {
        Some(pk) => {
            println!("SOLEND_USDC_FEE_RECEIVER={pk}");
            println!("SOLEND_USDC_HOST_FEE_RECEIVER={pk}  # same is fine");
        }
        None => {
            println!("# WARNING: no candidate verified as a USDC token account.");
            println!("# Either Solend's layout shifted further, or the reserve has no");
            println!("# fee_receiver set. Skip flash loans until this is resolved.");
        }
    }

    Ok(())
}
