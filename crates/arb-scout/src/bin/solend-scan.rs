//! One-shot reconnaissance: scans Solend main market obligations,
//! computes health ratios from on-chain (stale) values, prints top-20
//! closest to liquidation. Use to verify the parsers + see if there's
//! actual juice before turning on the liquidator worker.
//!
//! Run: `cargo run --release --bin solend-scan`

use arb_scout::solend_liq::{fetch_all_obligations, fetch_obligation_full, bootstrap_reserve_cache};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    let api_key = std::env::var("HELIUS_API_KEY")
        .map_err(|_| anyhow::anyhow!("HELIUS_API_KEY not set"))?;
    let rpc_url = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    println!("→ bootstrapping reserve cache...");
    let reserves = bootstrap_reserve_cache(&rpc_url).await?;
    println!("✓ {} reserves in cache", reserves.len());
    for (addr, r) in reserves.iter() {
        println!("  reserve={} mint={} liq_threshold={}% bonus={}% price=${:.4}",
                 addr, r.liquidity_mint, r.liquidation_threshold, r.liquidation_bonus,
                 r.market_price_usd);
    }

    println!("\n→ fetching all obligations (this can take 5-20 seconds)...");
    let obligations = fetch_all_obligations(&rpc_url).await?;
    println!("✓ {} obligations with non-zero debt", obligations.len());

    let sane: Vec<_> = obligations.iter().filter(|o| o.is_sane()).collect();
    println!("✓ {} obligations after sanity filter (skipped {} junk)",
             sane.len(), obligations.len() - sane.len());
    let mut with_ratio: Vec<_> = sane.iter()
        .map(|o| (o.liq_ratio(), *o))
        .filter(|(r, _)| *r > 0.0)
        .collect();
    with_ratio.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let liquidatable: Vec<_> = with_ratio.iter().filter(|(r, _)| *r >= 1.0).collect();
    println!("\n=== {} OBLIGATIONS CURRENTLY LIQUIDATABLE (on-chain stale state) ===",
             liquidatable.len());
    for (i, (ratio, o)) in liquidatable.iter().take(15).enumerate() {
        println!("  #{} ratio={:.3} borrowed=${:.2} unhealthy=${:.2} deposited=${:.2} owner={}",
                 i+1, ratio, o.borrowed_value_usd, o.unhealthy_borrow_value,
                 o.deposited_value_usd, o.owner);
        println!("     obligation={}", o.address);
        // Fetch full obligation for top 3 to show deposits/borrows
        if i < 3 {
            match fetch_obligation_full(&rpc_url, o.address).await {
                Ok(full) => {
                    for (d_i, d) in full.deposits.iter().enumerate() {
                        println!("     deposit[{}] reserve={} amount={} value=${:.2}",
                                 d_i, d.deposit_reserve, d.deposited_amount, d.market_value_usd);
                    }
                    for (b_i, b) in full.borrows.iter().enumerate() {
                        println!("     borrow [{}] reserve={} value=${:.2}",
                                 b_i, b.borrow_reserve, b.market_value_usd);
                    }
                }
                Err(e) => println!("     (could not fetch detail: {e})"),
            }
        }
    }

    println!("\n=== TOP 10 CLOSE-TO-LIQUIDATION (ratio 0.90-1.00) ===");
    let close: Vec<_> = with_ratio.iter()
        .filter(|(r, _)| *r >= 0.90 && *r < 1.00)
        .take(10)
        .collect();
    for (i, (ratio, o)) in close.iter().enumerate() {
        println!("  #{} ratio={:.3} borrowed=${:.2} unhealthy=${:.2} owner={}",
                 i+1, ratio, o.borrowed_value_usd, o.unhealthy_borrow_value, o.owner);
    }

    println!("\n=== summary ===");
    println!("  total obligations with debt:       {}", obligations.len());
    println!("  currently liquidatable (ratio≥1.0):{}", liquidatable.len());
    println!("  close (ratio 0.90-1.00):           {}",
             with_ratio.iter().filter(|(r,_)| *r >= 0.90 && *r < 1.0).count());
    println!("  total debt outstanding:            ${:.0}",
             obligations.iter().map(|o| o.borrowed_value_usd).sum::<f64>());

    // ── Debt-token breakdown: top 40 by ABSOLUTE bonus (not ratio) ──────
    println!("\n=== TOP 40 LIQUIDATABLE BY BONUS — debt-token breakdown ===");
    let usdc = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse::<solana_program::pubkey::Pubkey>()?;
    let usdt = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB".parse::<solana_program::pubkey::Pubkey>()?;
    let sol  = "So11111111111111111111111111111111111111112".parse::<solana_program::pubkey::Pubkey>()?;
    let mut by_bonus: Vec<_> = sane.iter()
        .filter(|o| o.is_liquidatable())
        .map(|o| (o.est_bonus_usd(), o))
        .collect();
    by_bonus.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut counts: std::collections::HashMap<solana_program::pubkey::Pubkey, (usize, f64)> = Default::default();
    let mut other = (0_usize, 0.0_f64);
    for (i, (bonus, o)) in by_bonus.iter().take(40).enumerate() {
        match fetch_obligation_full(&rpc_url, o.address).await {
            Ok(full) => {
                if i < 5 {
                    println!("  #{} bonus=${:.2} borrowed=${:.0} deposited=${:.0} obligation={}",
                             i+1, bonus, o.borrowed_value_usd, o.deposited_value_usd, o.address);
                }
                for b in &full.borrows {
                    if b.market_value_usd <= 0.001 { continue; }
                    if let Some(r) = reserves.get(&b.borrow_reserve) {
                        if i < 5 {
                            let tag = if r.liquidity_mint == usdc { "USDC" }
                                      else if r.liquidity_mint == usdt { "USDT" }
                                      else if r.liquidity_mint == sol { "SOL" }
                                      else { "????" };
                            println!("     {} borrowed: ${:.2}", tag, b.market_value_usd);
                        }
                        let e = counts.entry(r.liquidity_mint).or_insert((0, 0.0));
                        e.0 += 1;
                        e.1 += b.market_value_usd;
                    } else {
                        other.0 += 1; other.1 += b.market_value_usd;
                    }
                }
            }
            Err(_) => {}
        }
    }
    let mut entries: Vec<_> = counts.iter().collect();
    entries.sort_by(|a, b| b.1.1.partial_cmp(&a.1.1).unwrap_or(std::cmp::Ordering::Equal));
    for (mint, (n, total)) in entries.iter().take(10) {
        let tag = if **mint == usdc { "USDC" }
                  else if **mint == usdt { "USDT" }
                  else if **mint == sol  { "SOL " }
                  else { "????" };
        println!("  {} ({}): {} borrows totaling ${:.0}", tag, mint, n, total);
    }
    if other.0 > 0 { println!("  unknown-reserve: {} borrows totaling ${:.0}", other.0, other.1); }

    Ok(())
}
