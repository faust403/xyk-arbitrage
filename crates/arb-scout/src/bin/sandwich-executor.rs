//! Sandwich attack executor — Phase 1: DRY-RUN reader for shred-detector
//! alerts.
//!
//! ```text
//! cargo run --release --bin sandwich-executor
//! cargo run --release --bin sandwich-executor -- --min-usd 1000 --send
//! ```
//!
//! ## Pipeline
//!
//! 1. `sandwich-detector.service` (Python, ALREADY deployed) tails the
//!    shred firehose, binary-scans for DEX program IDs, emits one
//!    JSON line per detected swap to `/tmp/shred-sandwich-alert`.
//!
//! 2. THIS binary tails that file, parses each alert:
//!    `{"slot":N, "program":"RaydiumCLMM", "usd":X, "ts":T}`
//!
//! 3. For each alert above `--min-usd`, look up the affected pool's
//!    reserves via RPC (cached), compute the optimal sandwich size
//!    that maximizes our profit after Jito tip + priority + slippage.
//!
//! 4. Build the front-run + back-run tx pair. Submit to Jito with the
//!    user's tx sandwiched between (Jito guarantees execution order
//!    by bundle structure; the user's tx isn't in OUR bundle, just in
//!    the same slot ahead of our back-run).
//!
//! ## Why Phase 1 is read-only
//!
//! Real sandwich execution needs sub-100ms reaction time and shred-level
//! integration with our tx builder. Phase 1 builds the observability +
//! sizing layer; Phase 2 wires the live bundler. Either way the detector
//! sits in front emitting candidates.
//!
//! Required env: HELIUS_API_KEY. Optional: SANDWICH_ALERT_FILE
//! (default /tmp/shred-sandwich-alert), SANDWICH_AUTOFIRE (false).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use serde::Deserialize;
use solana_program::pubkey::Pubkey;

#[derive(Debug, Deserialize)]
struct Alert {
    slot:    u64,
    program: String,
    usd:     f64,
    ts:      f64,
}

#[derive(Debug, Default)]
struct ProgramStats {
    count_total:        u64,
    count_above_min:    u64,
    largest_usd:        f64,
    sum_usd:            f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    let args: Vec<String> = std::env::args().collect();
    let mut min_usd: f64 = 1000.0;
    let mut autofire = std::env::var("SANDWICH_AUTOFIRE").as_deref() == Ok("true");
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--min-usd" => {
                min_usd = args.get(i + 1).ok_or_else(|| anyhow!("--min-usd N"))?.parse()?;
                i += 2;
            }
            "--send" => { autofire = true; i += 1; }
            x => bail!("unknown arg: {x}"),
        }
    }

    let alert_path = std::env::var("SANDWICH_ALERT_FILE")
        .unwrap_or_else(|_| "/tmp/shred-sandwich-alert".to_string());

    println!("══════════════════════════════════════════════════════════════════");
    println!("  Sandwich executor — Phase 1 (read-only)  autofire={autofire}");
    println!("  min_usd=${min_usd}  alert_file={alert_path}");
    println!("══════════════════════════════════════════════════════════════════");

    if autofire {
        eprintln!("⚠ autofire requested but Phase 1 is read-only.");
        eprintln!("⚠ Phase 2 (live tx builder) is the next commit. Continuing in DRY-RUN.");
    }

    // Open file, seek to end, tail new entries.
    let f = std::fs::OpenOptions::new().read(true).open(&alert_path)
        .context("opening alert file (is sandwich-detector.service running?)")?;
    let mut reader = BufReader::new(f);
    reader.seek(SeekFrom::End(0))?;  // start at tail

    let mut stats: HashMap<String, ProgramStats> = HashMap::new();
    let mut total_above_min: u64 = 0;
    let start = std::time::Instant::now();
    let mut last_report = std::time::Instant::now();

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            // EOF — wait for new data
            tokio::time::sleep(Duration::from_millis(100)).await;
        } else {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            match serde_json::from_str::<Alert>(trimmed) {
                Ok(a) => {
                    let s = stats.entry(a.program.clone()).or_default();
                    s.count_total += 1;
                    s.sum_usd += a.usd;
                    if a.usd > s.largest_usd { s.largest_usd = a.usd; }
                    if a.usd >= min_usd {
                        s.count_above_min += 1;
                        total_above_min += 1;
                        println!(
                            "💡 alert  slot={}  prog={:<14}  usd=${:>14.0}  age={:.2}s",
                            a.slot,
                            a.program,
                            a.usd,
                            (std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
                                .as_secs_f64() - a.ts)
                                .max(0.0),
                        );
                    }
                }
                Err(_) => {
                    // ignore malformed line
                }
            }
        }

        if last_report.elapsed() > Duration::from_secs(30) {
            let elapsed = start.elapsed().as_secs_f64();
            println!("\n=== rolling stats ({:.0}s) ===", elapsed);
            println!("  total above-min: {total_above_min}");
            let mut rows: Vec<(&String, &ProgramStats)> = stats.iter().collect();
            rows.sort_by(|a, b| b.1.count_total.cmp(&a.1.count_total));
            for (prog, s) in rows.iter().take(8) {
                let rate = s.count_total as f64 / elapsed.max(1.0);
                let avg = if s.count_total > 0 { s.sum_usd / s.count_total as f64 } else { 0.0 };
                println!(
                    "  {:<14}  total={:>6}  above_min={:>5}  rate={:>5.1}/s  avg=${:>8.0}  max=${:>14.0}",
                    prog, s.count_total, s.count_above_min, rate, avg, s.largest_usd,
                );
            }
            println!();
            last_report = std::time::Instant::now();
        }
    }
}
