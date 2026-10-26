//! Find the most active fee payers of the FLASHX program. These are
//! candidate KOL wallets for copy-trading — frequent FLASHX users are
//! professional snipers, and the ones whose wallets accumulate SOL over
//! time are the "smart money" you want to mirror.
//!
//! Strategy:
//!   1. Pull the last N FLASHX signatures.
//!   2. For each tx, fetch its fee_payer.
//!   3. Aggregate by frequency.
//!   4. For each top wallet, query its SOL balance (rough proxy for PnL).
//!   5. Print sorted by frequency + balance.
//!
//! Usage:
//!   cargo run --release --bin flashx-top-users -p arb-scout

use std::collections::HashMap;

const FLASHX_PROGRAM: &str = "FLASHX8DrLbgeR8FcfNV1F5krxYcYMUdBkrP1EPBtxB9";
const RPC: &str = "https://api.mainnet-beta.solana.com";
const N_SIGS: usize = 200;
const TOP_N: usize = 20;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    println!("Fetching last {} FLASHX signatures...", N_SIGS);
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getSignaturesForAddress",
        "params": [FLASHX_PROGRAM, {"limit": N_SIGS}]
    });
    let resp: serde_json::Value = http.post(RPC).json(&body).send().await?.json().await?;
    let sigs: Vec<String> = resp.pointer("/result")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter(|s| s.get("err").map(|e| e.is_null()).unwrap_or(true))
            .filter_map(|s| s.get("signature").and_then(|v| v.as_str()).map(String::from))
            .collect())
        .unwrap_or_default();
    println!("Got {} successful sigs. Fetching fee_payers...", sigs.len());

    let mut payer_counts: HashMap<String, u32> = HashMap::new();
    for (i, sig) in sigs.iter().enumerate() {
        if i % 20 == 0 { println!("  Progress: {}/{}", i, sigs.len()); }
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
            "params": [sig, {
                "encoding": "jsonParsed",
                "maxSupportedTransactionVersion": 0,
                "commitment": "confirmed",
            }]
        });
        let resp: serde_json::Value = match http.post(RPC).json(&body).send().await {
            Ok(r) => match r.json().await { Ok(v) => v, Err(_) => continue },
            Err(_) => continue,
        };
        let payer = resp.pointer("/result/transaction/message/accountKeys/0")
            .and_then(|v| v.get("pubkey"))
            .and_then(|v| v.as_str());
        if let Some(p) = payer {
            *payer_counts.entry(p.to_string()).or_insert(0) += 1;
        }
        // Be polite to public RPC.
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    }

    let mut ranked: Vec<(String, u32)> = payer_counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1));

    println!("\n=== Top {} FLASHX fee_payers by frequency ===", TOP_N);
    for (i, (payer, count)) in ranked.iter().take(TOP_N).enumerate() {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getBalance",
            "params": [payer]
        });
        let bal_resp: serde_json::Value = http.post(RPC).json(&body).send().await?.json().await?;
        let bal_lamports = bal_resp.pointer("/result/value")
            .and_then(|v| v.as_u64()).unwrap_or(0);
        let bal_sol = bal_lamports as f64 / 1e9;
        println!("  {:>2}. {} — {:>3} tx, balance = {:>9.3} SOL (${:>8.0})",
            i + 1, payer, count, bal_sol, bal_sol * 95.0);
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    }

    println!("\n=== Recommended for COPY_TRADE_WALLETS env var ===");
    let copy_list: String = ranked.iter()
        .take(10)
        .filter(|(_, c)| *c >= 3)  // need at least 3 FLASHX tx to qualify
        .map(|(p, _)| p.clone())
        .collect::<Vec<_>>()
        .join(",");
    println!("railway variables --set COPY_TRADE_WALLETS='{}'", copy_list);

    Ok(())
}
