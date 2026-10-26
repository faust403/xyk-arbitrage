//! End-to-end Kamino Lend liquidation monitor — read-only.
//!
//! Mirrors marginfi-monitor: load reserves, fetch obligations (chunked
//! to fit the 3.7GB Frankfurt box), filter to liquidatable per the
//! Obligation's CACHED health fields, emit top-N.
//!
//! ```text
//! cargo run --release --bin kamino-monitor
//! cargo run --release --bin kamino-monitor -- --top 30 --min-debt 100
//! cargo run --release --bin kamino-monitor -- --loop 60
//! ```
//!
//! ## Caveat
//!
//! Health values on each Obligation are STALE — they reflect the state
//! at the most recent `refresh_obligation` (often hours ago for inactive
//! accounts). For a fresh assessment, the hunter binary will
//! sim-refresh-then-liquidate. Use this monitor for opportunity sizing,
//! not for live execution decisions.

use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use solana_program::pubkey::Pubkey;

use arb_scout::kamino_liq::{
    fetch_all_obligation_pubkeys, fetch_obligations_with_debt, fetch_reserve_cache,
    find_liquidatable_obligations,
};

const KAMINO_MAIN_MARKET_STR: &str = "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6JSDqGAv5PfF";

const SF_SCALE_F64: f64 = (1u128 << 60) as f64;

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
    let mut min_debt_usd: f64 = 50.0;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--loop" => {
                loop_secs = Some(args.get(i + 1).ok_or_else(|| anyhow!("--loop needs N"))?.parse()?);
                i += 2;
            }
            "--top" => {
                top = args.get(i + 1).ok_or_else(|| anyhow!("--top needs N"))?.parse()?;
                i += 2;
            }
            "--min-debt" => {
                min_debt_usd = args.get(i + 1).ok_or_else(|| anyhow!("--min-debt needs USD"))?.parse()?;
                i += 2;
            }
            x => bail!("unknown arg: {x}"),
        }
    }

    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY missing")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    let main_market: Pubkey = KAMINO_MAIN_MARKET_STR.parse().unwrap();

    loop {
        let t0 = std::time::Instant::now();
        if let Err(e) = scan_once(&rpc, &main_market, top, min_debt_usd).await {
            eprintln!("scan failed: {e}");
        }
        match loop_secs {
            None => break,
            Some(s) => {
                let elapsed = t0.elapsed();
                let sleep_for = Duration::from_secs(s).saturating_sub(elapsed);
                println!("\n--- cycle took {:.1}s, sleeping {:.0}s ---",
                    elapsed.as_secs_f32(), sleep_for.as_secs_f32());
                tokio::time::sleep(sleep_for).await;
            }
        }
    }
    Ok(())
}

async fn scan_once(rpc: &str, main_market: &Pubkey, top: usize, min_debt_usd: f64) -> anyhow::Result<()> {
    println!("\n══════════════════════════════════════════════════════════════════");
    println!("  Kamino liquidation monitor — read-only scan");
    println!("══════════════════════════════════════════════════════════════════");

    let t = std::time::Instant::now();
    let cache = fetch_reserve_cache(rpc).await?;
    let main_reserves = cache.by_lending_market.get(main_market).map(|v| v.len()).unwrap_or(0);
    println!("✓ reserves: {} total, {} on main market  ({:.1}s)",
        cache.by_address.len(), main_reserves, t.elapsed().as_secs_f32());

    let t = std::time::Instant::now();
    let pks = fetch_all_obligation_pubkeys(rpc).await?;
    println!("✓ obligations: {} pubkeys  ({:.1}s)", pks.len(), t.elapsed().as_secs_f32());

    let t = std::time::Instant::now();
    let obligations = fetch_obligations_with_debt(rpc, &pks).await?;
    println!("✓ obligations with debt: {} parsed  ({:.1}s)",
        obligations.len(), t.elapsed().as_secs_f32());

    let t = std::time::Instant::now();
    let candidates = find_liquidatable_obligations(&obligations, min_debt_usd, &cache);
    println!("✓ liquidatable (cached): {}  ({:.1}s)",
        candidates.len(), t.elapsed().as_secs_f32());

    println!("\n=== top {} candidates by cached LTV ===", top.min(candidates.len()));
    for (i, o) in candidates.iter().take(top).enumerate() {
        let ratio = o.cached_ltv_ratio().unwrap_or(0.0);
        let debt_usd  = (o.borrow_factor_adjusted_debt_value_sf as f64) / SF_SCALE_F64;
        let coll_usd  = (o.deposited_value_sf as f64) / SF_SCALE_F64;
        let unhealthy = (o.unhealthy_borrow_value_sf as f64) / SF_SCALE_F64;
        println!(
            "  #{i:>3}  ltv={r:>6.3}  debt=${d:>10.2}  unhealthy=${u:>10.2}  coll=${c:>10.2}",
            i = i + 1, r = ratio, d = debt_usd, u = unhealthy, c = coll_usd,
        );
        println!("        obligation={}  owner={}", o.address, o.owner);
        for b in o.borrows.iter().take(2) {
            if let Some(rs) = cache.by_address.get(&b.borrow_reserve) {
                println!("        borrow: reserve={}  mint={}", b.borrow_reserve, rs.liquidity_mint);
            }
        }
        for d in o.deposits.iter().take(2) {
            if let Some(rs) = cache.by_address.get(&d.deposit_reserve) {
                println!("        deposit: reserve={}  mint={}", d.deposit_reserve, rs.liquidity_mint);
            }
        }
    }

    // Aggregate at-risk debt by repay reserve.
    use std::collections::HashMap;
    let mut by_reserve: HashMap<Pubkey, f64> = HashMap::new();
    for o in &candidates {
        let debt_usd = (o.borrow_factor_adjusted_debt_value_sf as f64) / SF_SCALE_F64;
        if let Some(b) = o.borrows.first() {
            *by_reserve.entry(b.borrow_reserve).or_insert(0.0) += debt_usd;
        }
    }
    if !by_reserve.is_empty() {
        let mut rows: Vec<(Pubkey, f64)> = by_reserve.into_iter().collect();
        rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        println!("\n=== aggregate at-risk debt by primary borrow reserve ===");
        for (reserve, total) in rows.iter().take(10) {
            let mint = cache.by_address.get(reserve)
                .map(|r| r.liquidity_mint.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            println!("  ${:>12.2}  reserve={reserve}  mint={mint}", total);
        }
    }
    Ok(())
}
