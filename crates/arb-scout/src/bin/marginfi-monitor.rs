//! End-to-end MarginFi liquidation monitor — read-only.
//!
//! ```text
//! cargo run --release --bin marginfi-monitor                # one-shot
//! cargo run --release --bin marginfi-monitor -- --loop 30   # every 30s
//! cargo run --release --bin marginfi-monitor -- --top 50    # cap output
//! cargo run --release --bin marginfi-monitor -- --min-debt 100  # USD filter
//! ```
//!
//! ## What it does
//!
//! Stitches together every piece of the marginfi_liq foundation into one
//! observable pipeline. No transactions are submitted — this is the
//! "scout loop in log-only mode" that should run on Frankfurt for a few
//! cycles before the live executor (#46 step 4) is enabled.
//!
//! 1. `fetch_bank_cache` (= getProgramAccounts on banks, dataSize=1864).
//! 2. `fetch_bank_prices` (= getMultipleAccounts on every primary oracle
//!    whose `oracle_setup.is_pyth_observable() == true`).
//! 3. getProgramAccounts on MarginfiAccount (dataSize=2312), parse with
//!    `parse_marginfi_account`.
//! 4. For each account with non-zero liability: `compute_health` →
//!    `Candidate { health_usd, total_asset_usd, total_liab_usd, ... }`.
//! 5. Sort by most-underwater and print top N.
//!
//! ## Why this lives as a separate bin from `marginfi-discover`
//!
//! `marginfi-discover` ranks by raw liability *share counts* — useful
//! when bank cache + prices aren't set up yet. This binary uses real
//! USD values and is the canonical "is there opportunity here?" check.
//! Keep both: discover for debugging the parser, monitor for the
//! operational signal.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::pubkey::Pubkey;

use arb_scout::marginfi_liq::{
    fetch_bank_cache, fetch_bank_prices, find_liquidatable, parse_marginfi_account,
    Candidate, MARGINFI_ACCOUNT_SIZE, MARGINFI_V2_PROGRAM_STR,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    let args: Vec<String> = std::env::args().collect();
    let mut loop_secs: Option<u64> = None;
    let mut top: usize = 30;
    let mut min_debt_usd: f64 = 0.0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--loop" => {
                loop_secs = Some(args.get(i + 1).ok_or_else(|| anyhow!("--loop needs seconds"))?.parse()?);
                i += 2;
            }
            "--top" => {
                top = args.get(i + 1).ok_or_else(|| anyhow!("--top needs a value"))?.parse()?;
                i += 2;
            }
            "--min-debt" => {
                min_debt_usd = args.get(i + 1).ok_or_else(|| anyhow!("--min-debt needs USD"))?.parse()?;
                i += 2;
            }
            x => return Err(anyhow!("unknown arg: {x}")),
        }
    }

    let api_key = std::env::var("HELIUS_API_KEY")
        .context("HELIUS_API_KEY missing (set in .env or environment)")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    loop {
        let t0 = std::time::Instant::now();
        if let Err(e) = scan_once(&rpc, top, min_debt_usd).await {
            tracing::error!(error = %e, "scan failed");
        }
        match loop_secs {
            None => break,
            Some(s) => {
                let elapsed = t0.elapsed();
                let sleep_for = Duration::from_secs(s).saturating_sub(elapsed);
                tracing::info!("scan took {:.1}s — sleeping {:.1}s",
                    elapsed.as_secs_f32(), sleep_for.as_secs_f32());
                tokio::time::sleep(sleep_for).await;
            }
        }
    }
    Ok(())
}

async fn scan_once(rpc: &str, top: usize, min_debt_usd: f64) -> anyhow::Result<()> {
    println!("\n══════════════════════════════════════════════════════════════════");
    println!("  MarginFi liquidation monitor — read-only scan");
    println!("══════════════════════════════════════════════════════════════════");

    // ── 1. Load bank cache ──
    let t = std::time::Instant::now();
    let cache = fetch_bank_cache(rpc).await?;
    let banks_observable = cache.by_bank.values()
        .filter(|b| b.oracle_setup.is_pyth_observable()).count();
    println!("✓ banks: {} total, {} Pyth-observable  ({:.1}s)",
             cache.by_bank.len(), banks_observable, t.elapsed().as_secs_f32());

    // ── 2. Fetch oracle prices ──
    let t = std::time::Instant::now();
    let prices = fetch_bank_prices(rpc, &cache).await?;
    println!("✓ prices fetched: {} banks priced  ({:.1}s)",
             prices.len(), t.elapsed().as_secs_f32());

    // ── 3. Load all MarginfiAccount records ──
    let t = std::time::Instant::now();
    let accounts = fetch_all_accounts(rpc).await?;
    println!("✓ accounts: {} parsed  ({:.1}s)",
             accounts.len(), t.elapsed().as_secs_f32());

    // ── 4. Find liquidatable ──
    let t = std::time::Instant::now();
    let raw_candidates: Vec<Candidate> = find_liquidatable(&accounts, &cache, &prices)
        .into_iter()
        .filter(|c| c.total_liab_usd >= min_debt_usd)
        .collect();
    // ── 4b. Filter to fully-Push-V2 candidates ──
    // For a tx to actually land, EVERY active balance on the underwater
    // account must be on a bank whose primary oracle is Push V2. The
    // MarginFi program walks all positions during liquidate and reverts
    // with SwitchboardStalePrice (6049) if ANY oracle is stale — and
    // Switchboard / PythLegacy / Wormhole-routed feeds aren't atomically
    // refreshable from a shred prepend.
    //
    // Build a quick `address → ParsedAccount` map so we can look up each
    // candidate's full balance set and check oracle setup per bank.
    let by_addr: HashMap<Pubkey, &arb_scout::marginfi_liq::ParsedAccount> =
        accounts.iter().map(|a| (a.address, a)).collect();
    let candidates: Vec<Candidate> = raw_candidates.into_iter()
        .filter(|c| {
            let Some(acct) = by_addr.get(&c.address) else { return false };
            acct.active_balances().all(|b| {
                cache.by_bank.get(&b.bank_pk)
                    .map(|bs| bs.oracle_setup.is_pyth_observable())
                    .unwrap_or(false)
            })
        })
        .collect();
    println!("✓ {} candidates fully Push-V2 + health < 0 + debt >= ${:.0}  (raw {} → filtered, {:.1}s)",
             candidates.len(),
             min_debt_usd,
             accounts.len(),
             t.elapsed().as_secs_f32());

    // ── 5. Report top N ──
    println!("\n=== top {} liquidatable accounts ===", top.min(candidates.len()));
    for (i, c) in candidates.iter().take(top).enumerate() {
        let asset_bank_str = c.best_asset_bank.map(|b| b.to_string())
            .unwrap_or_else(|| "—".to_string());
        let liab_bank_str = c.best_liab_bank.map(|b| b.to_string())
            .unwrap_or_else(|| "—".to_string());
        println!(
            "  #{i:>3}  health=${h:>10.2}  debt=${d:>10.2}  coll=${a:>10.2}",
            i = i + 1,
            h = c.health_usd,
            d = c.total_liab_usd,
            a = c.total_asset_usd,
        );
        println!("        acct={} auth={}", c.address, c.authority);
        println!("        seize_bank={} repay_bank={}", asset_bank_str, liab_bank_str);
    }

    // ── 6. Diagnostic: top banks by aggregate liability ──
    let mut bank_debt: HashMap<Pubkey, f64> = HashMap::new();
    for c in &candidates {
        if let Some(b) = c.best_liab_bank {
            *bank_debt.entry(b).or_insert(0.0) += c.total_liab_usd;
        }
    }
    if !bank_debt.is_empty() {
        let mut rows: Vec<(Pubkey, f64)> = bank_debt.into_iter().collect();
        rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        println!("\n=== aggregate at-risk debt by repay bank ===");
        for (bank, total) in rows.iter().take(10) {
            let mint_label = cache.by_bank.get(bank)
                .map(|b| format!("mint={}", b.mint))
                .unwrap_or_else(|| "unknown".to_string());
            println!("  ${:>10.2}  bank={bank}  {mint_label}", total);
        }
    }

    Ok(())
}

async fn fetch_all_accounts(rpc: &str) -> anyhow::Result<Vec<arb_scout::marginfi_liq::ParsedAccount>> {
    // Two-step pattern to keep peak RAM bounded on the 3.7 GB Frankfurt
    // box. A single getProgramAccounts that returns base64 bytes for
    // every MarginfiAccount blows past available memory (~30k+ accounts
    // × 2312 B + JSON overhead).
    //
    // Step 1: pubkey list only (dataSlice length=0). Tiny response.
    // Step 2: getMultipleAccounts in 100-account chunks; parse each
    //         chunk and drop accounts with no debt immediately so only
    //         a small residue survives across chunks.
    let client = reqwest::Client::new();

    // ── Step 1: pubkey list ──
    let list_body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
        "params":[MARGINFI_V2_PROGRAM_STR, {
            "encoding": "base64",
            "commitment": "confirmed",
            "filters": [{"dataSize": MARGINFI_ACCOUNT_SIZE}],
            "dataSlice": {"offset": 0, "length": 0},
        }],
    });
    let r: serde_json::Value = client.post(rpc).json(&list_body).send().await
        .context("gPA list http")?
        .error_for_status()?
        .json().await
        .context("gPA list json")?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no result array: {r}"))?;
    let pubkeys: Vec<Pubkey> = arr.iter()
        .filter_map(|e| e.get("pubkey").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
        .collect();
    println!("    (step 1: {} pubkeys)", pubkeys.len());

    // ── Step 2: chunked getMultipleAccounts ──
    let mut out = Vec::new();
    for chunk in pubkeys.chunks(100) {
        let keys: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getMultipleAccounts",
            "params":[keys, {
                "encoding":"base64",
                "commitment":"confirmed",
                "dataSlice":{"offset":0,"length":1736},
            }],
        });
        let r: serde_json::Value = client.post(rpc).json(&body).send().await
            .context("gMA chunk http")?
            .error_for_status()?
            .json().await
            .context("gMA chunk json")?;
        let vals = r.pointer("/result/value").and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("no result/value: {r}"))?;
        for (i, val) in vals.iter().enumerate() {
            if val.is_null() { continue; }
            let data_b64 = val.pointer("/data/0").and_then(|v| v.as_str()).unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b, Err(_) => continue,
            };
            if let Some(p) = parse_marginfi_account(chunk[i], &bytes) {
                if p.has_debt() { out.push(p); }
            }
        }
    }
    Ok(out)
}
