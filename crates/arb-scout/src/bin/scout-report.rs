//! `scout-report` — generates `docs/SCOUT_REPORT.md` from accumulated JSONL.
//!
//! Per `INSTRUCTIONS.md` Phase −1 deliverables, the report has 8 sections:
//!   1. Total opportunities observed
//!   2. Distribution of net profit (≥$0.10, ≥$1, ≥$5, ≥$20)
//!   3. Average net profit per opportunity
//!   4. Time-of-day distribution
//!   5. DEX-pair distribution
//!   6. Capture rate (placeholder — needs Jito correlator implementation)
//!   7. Streaming uptime (estimated from observed timestamp gaps)
//!   8. Estimated monthly net profit
//!
//! Usage:
//! ```bash
//! cargo run --release --bin scout-report -- [logs_dir] [output_path]
//! # defaults: logs/  →  docs/SCOUT_REPORT.md
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use arb_scout::jito_correlator::CorrelationRecord;
use arb_telemetry::OpportunityRecord;
use chrono::{DateTime, Duration as ChronoDuration, Timelike, Utc};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let logs_dir = args.get(1).cloned().unwrap_or_else(|| "logs".into());
    let out_path = args.get(2).cloned().unwrap_or_else(|| "docs/SCOUT_REPORT.md".into());

    let records = read_all_records(&logs_dir)?;
    let correlations = read_all_correlations(&logs_dir)?;
    if records.is_empty() {
        eprintln!("⚠️  No JSONL records found in {logs_dir}/. \
                   Either the scout hasn't run yet or no opportunities have been logged.");
    }
    let report = build_report(&records, &correlations);

    if let Some(parent) = PathBuf::from(&out_path).parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&out_path, report)?;
    println!("✅ Wrote {} ({} records analysed)", out_path, records.len());
    Ok(())
}

fn read_all_records(dir: &str) -> anyhow::Result<Vec<OpportunityRecord>> {
    let mut out = Vec::new();
    let path = PathBuf::from(dir);
    if !path.exists() { return Ok(out); }
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with("scout-") || !name.ends_with(".jsonl") { continue; }
        let f = fs::File::open(&p)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<OpportunityRecord>(&line) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warning: {p:?}: {e}"),
            }
        }
    }
    out.sort_by_key(|r| r.timestamp);
    Ok(out)
}

/// Read every `correlation-YYYY-MM-DD.jsonl` file. Returns one record per line.
fn read_all_correlations(dir: &str) -> anyhow::Result<Vec<CorrelationRecord>> {
    let mut out = Vec::new();
    let path = PathBuf::from(dir);
    if !path.exists() { return Ok(out); }
    for entry in fs::read_dir(&path)? {
        let entry = entry?;
        let p = entry.path();
        let Some(name) = p.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with("correlation-") || !name.ends_with(".jsonl") { continue; }
        let f = fs::File::open(&p)?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() { continue; }
            match serde_json::from_str::<CorrelationRecord>(&line) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("warning: {p:?}: {e}"),
            }
        }
    }
    Ok(out)
}

fn dollars(units_usdc: i64) -> f64 { units_usdc as f64 / 1_000_000.0 }

#[derive(Debug, Default, Clone, Copy)]
struct CaptureStats {
    correlated:           u64,
    lost:                 u64,
    uncontested:          u64,
    skipped:              u64,
    total_competitor_tip: u64,
    bundles_per_lost:     u64, // sum of competing_bundles across "lost" rows
}

impl CaptureStats {
    fn capture_rate(&self) -> Option<f64> {
        let resolved = self.lost + self.uncontested;
        if resolved == 0 { None }
        else { Some(self.uncontested as f64 / resolved as f64) }
    }
    fn avg_competitor_tip_lamports(&self) -> Option<u64> {
        if self.lost == 0 { None } else { Some(self.total_competitor_tip / self.lost) }
    }
}

fn aggregate_capture(correlations: &[CorrelationRecord]) -> CaptureStats {
    let mut s = CaptureStats::default();
    for c in correlations {
        s.correlated += 1;
        match c.outcome.as_str() {
            "lost" => {
                s.lost += 1;
                s.total_competitor_tip += c.competitor_tip_lamports;
                s.bundles_per_lost += c.competing_bundles as u64;
            }
            "uncontested" => s.uncontested += 1,
            _ => s.skipped += 1,
        }
    }
    s
}

fn build_report(records: &[OpportunityRecord], correlations: &[CorrelationRecord]) -> String {
    let total = records.len();
    let first = records.first().map(|r| r.timestamp);
    let last  = records.last().map(|r| r.timestamp);

    let observed_days = match (first, last) {
        (Some(a), Some(b)) => {
            let span = (b - a).num_seconds().max(1) as f64 / 86400.0;
            span.max(1.0 / 24.0) // at least 1 hour to avoid divide-by-zero
        }
        _ => 0.0,
    };

    let count_at = |threshold_usdc: i64| records.iter().filter(|r| r.net_profit_usdc >= threshold_usdc).count();
    let n_10c = count_at(100_000);
    let n_1d  = count_at(1_000_000);
    let n_5d  = count_at(5_000_000);
    let n_20d = count_at(20_000_000);

    let avg_net = if total == 0 { 0.0 }
        else { dollars(records.iter().map(|r| r.net_profit_usdc).sum::<i64>()) / total as f64 };

    // Time-of-day buckets (UTC hour).
    let mut hour_counts: BTreeMap<u32, usize> = (0..24).map(|h| (h, 0)).collect();
    for r in records { *hour_counts.entry(r.timestamp.hour()).or_default() += 1; }

    // DEX-pair distribution: derived from `route` string (already pretty-printed).
    let mut pair_counts: BTreeMap<String, usize> = BTreeMap::new();
    for r in records { *pair_counts.entry(r.route.clone()).or_default() += 1; }

    // Streaming uptime estimate. We detect gaps by comparing consecutive
    // record timestamps; any gap > 5 minutes counts as inferred downtime.
    let downtime = inferred_downtime(records);
    let total_span = match (first, last) {
        (Some(a), Some(b)) => (b - a).max(ChronoDuration::seconds(1)),
        _ => ChronoDuration::seconds(0),
    };
    let uptime_pct = if total_span.num_seconds() > 0 {
        let up = (total_span - downtime).num_seconds().max(0) as f64;
        100.0 * up / total_span.num_seconds() as f64
    } else { 100.0 };

    // Estimated monthly net = (sum_net / observed_days) * 30
    let total_net = dollars(records.iter().map(|r| r.net_profit_usdc).sum::<i64>());
    let monthly_net = if observed_days > 0.0 { total_net * 30.0 / observed_days } else { 0.0 };
    let opps_per_day_1d = if observed_days > 0.0 { n_1d as f64 / observed_days } else { 0.0 };
    let opps_per_day_5d = if observed_days > 0.0 { n_5d as f64 / observed_days } else { 0.0 };

    // Decision verdict per INSTRUCTIONS.md Phase −1 thresholds.
    let largest = dollars(records.iter().map(|r| r.net_profit_usdc).max().unwrap_or(0));
    let verdict = decide(opps_per_day_1d, opps_per_day_5d, largest, uptime_pct, monthly_net);

    let mut s = String::new();
    s.push_str("# Scout Report — Phase −1 Decision Gate\n\n");
    s.push_str(&format!(
        "_Generated {now}. Window: {window}._\n\n",
        now = Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
        window = format_window(first, last),
    ));

    s.push_str("## TL;DR — Verdict\n\n");
    s.push_str(&format!("**{}**\n\n", verdict.headline));
    s.push_str(&format!("{}\n\n", verdict.detail));

    s.push_str("## 1. Totals\n\n");
    s.push_str(&format!("- Opportunities logged: **{total}**\n"));
    s.push_str(&format!("- Observed window: **{observed_days:.2} days**\n"));
    s.push_str(&format!("- Sum net profit (theoretical): **${total_net:.2}**\n\n"));

    s.push_str("## 2. Net-profit distribution\n\n");
    s.push_str("| Threshold | Count | Per day |\n|---|---|---|\n");
    let pd = |n: usize| if observed_days > 0.0 { n as f64 / observed_days } else { 0.0 };
    s.push_str(&format!("| ≥ $0.10 | {n_10c} | {:.2} |\n", pd(n_10c)));
    s.push_str(&format!("| ≥ $1    | {n_1d}  | {:.2} |\n", pd(n_1d)));
    s.push_str(&format!("| ≥ $5    | {n_5d}  | {:.2} |\n", pd(n_5d)));
    s.push_str(&format!("| ≥ $20   | {n_20d} | {:.2} |\n\n", pd(n_20d)));

    s.push_str("## 3. Averages\n\n");
    s.push_str(&format!("- Avg net per opportunity: **${avg_net:.4}**\n"));
    s.push_str(&format!("- Largest single opportunity: **${largest:.2}**\n\n"));

    s.push_str("## 4. Time-of-day distribution (UTC hour)\n\n");
    s.push_str("| Hour | Count |\n|---|---|\n");
    for (h, c) in hour_counts { if c > 0 { s.push_str(&format!("| {h:02}:00 | {c} |\n")); } }
    s.push('\n');

    s.push_str("## 5. DEX-pair distribution (top 20 routes)\n\n");
    s.push_str("| Route | Count |\n|---|---|\n");
    let mut pairs_sorted: Vec<_> = pair_counts.into_iter().collect();
    pairs_sorted.sort_by_key(|(_, c)| std::cmp::Reverse(*c));
    for (route, count) in pairs_sorted.into_iter().take(20) {
        s.push_str(&format!("| `{route}` | {count} |\n"));
    }
    s.push('\n');

    s.push_str("## 6. Capture-rate analysis\n\n");
    let cap = aggregate_capture(correlations);
    if cap.correlated == 0 {
        s.push_str("_No correlation records yet. The correlator runs in the background and \
                    waits ~30 s after each opportunity to fetch the slot's block. After a \
                    fresh 7-day run this section auto-populates._\n\n");
    } else {
        s.push_str("Each logged opportunity is correlated against the on-chain block at its \
                    slot. We scan every transaction in that block for SystemProgram::Transfer \
                    instructions to one of the 8 Jito tip accounts. If a bundle that touches \
                    one of OUR pools also tipped Jito → a competing searcher won that race.\n\n");
        s.push_str(&format!(
            "- Correlation records produced: **{}**\n\
             - Resolved (lost + uncontested): **{}**\n\
             - Skipped (block fetch failed / slot was missed by validator): **{}**\n\n",
            cap.correlated, cap.lost + cap.uncontested, cap.skipped,
        ));
        s.push_str("| Outcome | Count | Share of resolved |\n|---|---|---|\n");
        let resolved = (cap.lost + cap.uncontested).max(1);
        s.push_str(&format!(
            "| Lost to a competing Jito bundle | {} | {:.1}% |\n",
            cap.lost, 100.0 * cap.lost as f64 / resolved as f64,
        ));
        s.push_str(&format!(
            "| Uncontested (no competing bundle) | {} | {:.1}% |\n\n",
            cap.uncontested, 100.0 * cap.uncontested as f64 / resolved as f64,
        ));
        if let Some(tip) = cap.avg_competitor_tip_lamports() {
            s.push_str(&format!(
                "- Average competitor tip: **{} lamports** (≈ ${:.4})\n",
                tip, tip as f64 / 1_000_000_000.0 * 100.0, // rough $/SOL=$100 placeholder
            ));
            s.push_str(&format!(
                "- Average competing bundles per lost opportunity: **{:.2}**\n\n",
                cap.bundles_per_lost as f64 / cap.lost.max(1) as f64,
            ));
        }
        let rate = cap.capture_rate().unwrap_or(0.0);
        s.push_str(&format!(
            "### 🎯 Estimated capture rate: **{:.1}%**\n\n\
             This is the *ceiling* — the fraction of opportunities that **no other searcher** \
             would have taken from us. Real production capture would also depend on our \
             tipping strategy, which the scout cannot simulate.\n\n",
            rate * 100.0,
        ));
    }

    s.push_str("## 7. Streaming uptime (estimate)\n\n");
    s.push_str(&format!(
        "- Observed span: **{} h**\n\
         - Inferred downtime (gaps > 5 min): **{} min**\n\
         - **Estimated uptime: {:.2}%**\n\n",
        total_span.num_hours(),
        downtime.num_minutes(),
        uptime_pct,
    ));
    s.push_str("_Note: this estimate looks at gaps between **logged opportunities**, not WSS \
                connection events. A pool with no profitable trade for 10 min looks identical \
                to a 10-min disconnect. Treat as a soft floor._\n\n");

    s.push_str("## 8. Estimated monthly net profit\n\n");
    s.push_str(&format!("- Extrapolated from {observed_days:.2} days of data:\n"));
    s.push_str(&format!("  - Total net observed (gross of capture rate): **${total_net:.2}**\n"));
    s.push_str(&format!("  - **Gross monthly estimate: ${monthly_net:.2}**\n"));
    if let Some(rate) = cap.capture_rate() {
        let adjusted = monthly_net * rate;
        s.push_str(&format!(
            "  - **Capture-rate adjusted (× {:.1}%): ${:.2}/month**\n",
            rate * 100.0, adjusted
        ));
    } else {
        s.push_str("  - Capture-rate adjustment: **TBD** — Section 6 needs more correlation data.\n");
    }
    s.push('\n');

    s.push_str("## Decision thresholds (per INSTRUCTIONS.md)\n\n");
    s.push_str("| Metric | Build the bot | Reconsider | This run |\n|---|---|---|---|\n");
    s.push_str(&format!("| Opps/day ≥ $1 | ≥ 10 | < 5 | {opps_per_day_1d:.2} |\n"));
    s.push_str(&format!("| Opps/day ≥ $5 | ≥ 2  | 0   | {opps_per_day_5d:.2} |\n"));
    s.push_str(&format!("| Largest single | ≥ $20 | < $5 | ${largest:.2} |\n"));
    s.push_str(&format!("| Uptime | ≥ 99% | < 95% | {uptime_pct:.2}% |\n"));
    s.push_str(&format!("| Est monthly net | ≥ $1,500 | < $500 | ${monthly_net:.2} |\n\n"));

    s.push_str("## Methodology notes\n\n");
    s.push_str("- **Raydium AMM v4** quotes use real on-chain reserves fetched via vault account subscriptions.\n");
    s.push_str("- **Orca Whirlpool** quotes use a spot-price approximation (`sqrt_price^2 × (1−fee)`). Exact for small in-tick swaps; overestimates for large swaps that would cross ticks. Acceptable for opportunity *detection*.\n");
    s.push_str("- **Raydium CLMM** quotes use the same spot-price approximation as Whirlpool. Per-pool fee tier is fetched from the AmmConfig account at bootstrap.\n");
    s.push_str("- **Meteora DLMM** quotes use the active-bin price (`(1+bin_step/10000)^active_id`) with the base-fee formula from `StaticParameters`. Walking adjacent bins is deferred to Phase 2.\n");
    s.push_str("- **Jito-bundle correlation**: ~30 s after each opportunity, the scout fetches the on-chain block at that slot via `getBlock`, identifies all transactions that include a SystemProgram::Transfer to one of the 8 Jito tip accounts, and checks whether any of those tipping txs touched our pool addresses. Result is the fraction of opportunities **no faster searcher** would have raced us for.\n\n");

    s.push_str("---\n_End of report. Operator: review and decide._\n");
    s
}

fn format_window(first: Option<DateTime<Utc>>, last: Option<DateTime<Utc>>) -> String {
    match (first, last) {
        (Some(a), Some(b)) => format!(
            "{} → {}",
            a.format("%Y-%m-%d %H:%M UTC"),
            b.format("%Y-%m-%d %H:%M UTC"),
        ),
        _ => "no records".into(),
    }
}

fn inferred_downtime(records: &[OpportunityRecord]) -> ChronoDuration {
    let mut total = ChronoDuration::seconds(0);
    let threshold = ChronoDuration::minutes(5);
    for w in records.windows(2) {
        let gap = w[1].timestamp - w[0].timestamp;
        if gap > threshold {
            total += gap;
        }
    }
    total
}

struct Verdict {
    headline: &'static str,
    detail:   String,
}

fn decide(per_day_1d: f64, per_day_5d: f64, largest_usd: f64, uptime_pct: f64, monthly_net: f64) -> Verdict {
    let go = per_day_1d >= 10.0 && per_day_5d >= 2.0
          && largest_usd >= 20.0 && uptime_pct >= 99.0 && monthly_net >= 1500.0;
    let stop = per_day_1d < 5.0 || per_day_5d < 1.0
            || largest_usd < 5.0 || uptime_pct < 95.0 || monthly_net < 500.0;
    if go {
        Verdict {
            headline: "✅ GO — proceed to Phase 0 (build the full bot).",
            detail: format!(
                "All five Phase −1 decision criteria cleared (≥10 opps/day ≥$1, ≥2 opps/day ≥$5, \
                 largest ≥$20, uptime ≥99%, est monthly ≥$1,500). Observed values: \
                 {per_day_1d:.2}, {per_day_5d:.2}, ${largest_usd:.2}, {uptime_pct:.2}%, ${monthly_net:.2}/mo."
            ),
        }
    } else if stop {
        Verdict {
            headline: "❌ RECONSIDER — at least one threshold fell into the 'stop' band.",
            detail: format!(
                "Per-day ≥$1: {per_day_1d:.2}, per-day ≥$5: {per_day_5d:.2}, largest: \
                 ${largest_usd:.2}, uptime: {uptime_pct:.2}%, est monthly: ${monthly_net:.2}. \
                 Recommended next steps before deciding: (a) extend the run window, \
                 (b) implement capture-rate (Section 6), (c) check whether \
                 Raydium-CLMM/Meteora-DLMM coverage would change the picture."
            ),
        }
    } else {
        Verdict {
            headline: "🟨 INCONCLUSIVE — between go and reconsider.",
            detail: format!(
                "Per-day ≥$1: {per_day_1d:.2}, per-day ≥$5: {per_day_5d:.2}, largest: \
                 ${largest_usd:.2}, uptime: {uptime_pct:.2}%, est monthly: ${monthly_net:.2}. \
                 Suggest extending the observation window by another 7 days and reviewing \
                 whether to add Meteora-DLMM coverage."
            ),
        }
    }
}
