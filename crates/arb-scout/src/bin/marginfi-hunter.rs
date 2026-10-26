//! Event-driven MarginFi liquidation hunter.
//!
//! ## Why event-driven
//!
//! The previous polling design (`--loop 90`) re-fetched ~512K MarginFi
//! account pubkeys + their state every 90s. Helius's 5/sec rate limit
//! turned that into a 64-min cycle in practice and silently produced
//! zero candidates while flooding the logs with `429 on chunk` retries.
//! Result: ~150-850 MarginFi liquidations/day on chain (measured live
//! 2026-05-16) and we caught **0**.
//!
//! This module mirrors the [`kamino-hunter`] architecture:
//!
//! ```text
//!   bootstrap (one-time, ~minutes)
//!     ├─ fetch_bank_cache      (~50 banks, single gPA)
//!     ├─ fetch_bank_prices     (one batched gMA per oracle list)
//!     ├─ fetch_pubkeys         (dataSize filter, one gPA — small payload)
//!     ├─ fetch_accounts_with_debt (chunked gMA, paced; ONLY ONCE)
//!     └─ build_index           (bank → Vec<account_idx>)
//!
//!   tasks (long-running):
//!     A. oracle_subscriber  (LaserStream gRPC, accounts=bank_oracles)
//!          → mpsc<AccountUpdate>
//!     B. tick_processor     (parse Pyth, update prices, recompute health)
//!          → mpsc<FireRequest>
//!     C. fire_worker × N    (refetch account, attempt_liquidation, log)
//!     D. background_refresh (every 30 min: rebuild account universe)
//! ```
//!
//! ## Env
//!
//! Required: HELIUS_API_KEY, SOLANA_KEYPAIR_JSON, MARGINFI_ACCOUNT,
//!           LASERSTREAM_GRPC_URL
//! Optional: EXECUTOR_PYTH_ALT,
//!           MARGINFI_AUTOFIRE (default false),
//!           MARGINFI_CLOSE_FACTOR (0.5),
//!           MARGINFI_LIQ_BONUS (0.05),
//!           MARGINFI_JITO_TIP_LAMPORTS (500_000),
//!           MARGINFI_HUNT_MIN_DEBT_USD (50),
//!           MARGINFI_FIRE_WORKERS (2),
//!           MARGINFI_REFRESH_INTERVAL_SECS (1800),
//!           MFI_CHUNK_THROTTLE_MS (250)  — bootstrap loader pacing
//!
//! ## Build
//!
//! Frankfurt: `cargo build --release --bin marginfi-hunter --features laserstream`
//! Without the `laserstream` feature the subscriber stub will refuse to
//! start (no fallback — the legacy polling path is gone).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::pubkey::Pubkey;
use tokio::sync::{mpsc, RwLock};

use arb_scout::marginfi_liq::{
    attempt_liquidation, compute_health, fetch_bank_cache, fetch_bank_prices,
    parse_marginfi_account, AttemptConfig, BankCache, LiquidationMode, ParsedAccount,
    MARGINFI_ACCOUNT_SIZE, MARGINFI_V2_PROGRAM_STR,
};
use arb_scout::wallet::Wallet;
use arb_dex_obric::pyth::parse_pyth_price_any;

const JITO_TIP_ACCOUNT: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";

// ─── Shared hunter state ───────────────────────────────────────

struct HunterState {
    cache:          RwLock<BankCache>,
    accounts:       RwLock<Vec<ParsedAccount>>,
    /// `bank_pk → indices into `accounts`` for every account that has an
    /// active asset OR liability balance in that bank. Built once at
    /// bootstrap and rebuilt by `background_refresh`.
    bank_to_accts:  RwLock<HashMap<Pubkey, Vec<usize>>>,
    /// `bank_pk → price_usd_per_token`. Seeded from `fetch_bank_prices`
    /// at bootstrap and updated incrementally on every Pyth tick.
    prices:         RwLock<HashMap<Pubkey, f64>>,
    /// Recently-fired account pubkeys (with timestamp) — used to
    /// suppress duplicate fires from rapid back-to-back ticks. Pruned
    /// to last 60s on every insert.
    fired_recently: RwLock<HashMap<Pubkey, std::time::Instant>>,
    /// Accounts whose last sim returned `pre_liquidation_health > 0`
    /// (program says healthy despite our naive math saying underwater).
    /// Suppresses re-queueing until the TTL expires. Live observation
    /// 2026-05-16: without this cache, 213 of 397 hits per 5-minute
    /// window were repeated sims on the same 4-6 false-positive accounts
    /// returning Custom:6068. With it, those accounts wait ~60-300 s
    /// for fresh sim, freeing worker time for real underwater hits.
    seen_healthy:   RwLock<HashMap<Pubkey, std::time::Instant>>,
    /// Accounts that recently hit `tx too large` during sim — same
    /// account, same bank composition, will fail again until the
    /// ALT is extended. Skip them for a long TTL (15 min) so we
    /// don't keep wasting Jupiter quote + build time. Refreshed on
    /// background_refresh in case the user deploys a new ALT.
    seen_too_large: RwLock<HashMap<Pubkey, std::time::Instant>>,
}

struct FireRequest {
    candidate: Pubkey,
    reason:    String,
}

// ─── Bootstrap ─────────────────────────────────────────────────

async fn bootstrap(rpc: &str) -> anyhow::Result<HunterState> {
    println!("→ bootstrap: loading bank cache");
    let cache = fetch_bank_cache(rpc).await?;
    let push_v2 = cache.by_bank.values()
        .filter(|b| b.oracle_setup.is_pyth_observable()).count();
    println!("  ✓ {} banks ({} Push-V2 observable, {} oracle feeds)",
        cache.by_bank.len(), push_v2, cache.by_oracle.len());

    println!("→ bootstrap: fetching bank prices");
    let prices = fetch_bank_prices(rpc, &cache).await?;
    println!("  ✓ {} bank prices seeded", prices.len());

    println!("→ bootstrap: fetching marginfi account pubkeys (dataSize-filtered)");
    let pubkeys = fetch_pubkeys(rpc).await?;
    println!("  ✓ {} marginfi accounts on-chain", pubkeys.len());

    println!("→ bootstrap: chunk-loading accounts with debt (this is the slow leg, ONCE)");
    let accounts = fetch_accounts_with_debt(rpc, &pubkeys).await?;
    println!("  ✓ {} accounts with non-zero debt", accounts.len());

    // bank → Vec<account_idx>: every active balance contributes one edge.
    let mut bank_to_accts: HashMap<Pubkey, Vec<usize>> = HashMap::new();
    for (i, acc) in accounts.iter().enumerate() {
        for bal in acc.active_balances() {
            bank_to_accts.entry(bal.bank_pk).or_default().push(i);
        }
    }
    println!("  ✓ index: {} banks → accounts edges", bank_to_accts.len());

    Ok(HunterState {
        cache:          RwLock::new(cache),
        accounts:       RwLock::new(accounts),
        bank_to_accts:  RwLock::new(bank_to_accts),
        prices:         RwLock::new(prices),
        fired_recently: RwLock::new(HashMap::new()),
        seen_healthy:   RwLock::new(HashMap::new()),
        seen_too_large: RwLock::new(HashMap::new()),
    })
}

// ─── Subscriber (LaserStream) ─────────────────────────────────

#[cfg(feature = "laserstream")]
async fn account_subscriber_loop(
    grpc_url: String,
    api_key:  String,
    accounts: Vec<Pubkey>,
    tx:       mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    use arb_stream::wss::SessionEnd;
    loop {
        println!("→ laserstream: connecting ({} bank oracles)", accounts.len());
        let end = arb_stream::laserstream::run_one_session(
            &grpc_url, &api_key, &accounts, &tx, None,
        ).await;
        match end {
            SessionEnd::Closed     => eprintln!("laserstream closed cleanly — reconnecting"),
            SessionEnd::Errored(e) => eprintln!("laserstream errored: {e:?} — reconnect in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(not(feature = "laserstream"))]
async fn account_subscriber_loop(
    _grpc_url: String,
    _api_key:  String,
    _accounts: Vec<Pubkey>,
    _tx:       mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    eprintln!("laserstream feature disabled — marginfi-hunter event-driven mode requires --features laserstream");
}

// ─── Tick processor ────────────────────────────────────────────

async fn tick_processor(
    state:    Arc<HunterState>,
    mut rx:   mpsc::Receiver<arb_stream::wss::AccountUpdate>,
    fire_tx:  mpsc::Sender<FireRequest>,
) {
    let min_debt_usd: f64 = std::env::var("MARGINFI_HUNT_MIN_DEBT_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(50.0);

    let mut tick_count: u64 = 0;
    let mut parse_fail: u64 = 0;
    let mut not_trading: u64 = 0;
    let mut hits_total: u64 = 0;
    let mut fires_queued: u64 = 0;
    let mut last_heartbeat = std::time::Instant::now();

    while let Some(acc) = rx.recv().await {
        tick_count += 1;
        if last_heartbeat.elapsed() > Duration::from_secs(60) {
            println!("  💓 ticks={tick_count} parse_fail={parse_fail} not_trading={not_trading} hits={hits_total} fires_queued={fires_queued}");
            last_heartbeat = std::time::Instant::now();
        }

        // Parse Pyth price from the account data. Banks use either Push-V2
        // (PythPushOracle program) or staked-collateral/legacy variants.
        // `parse_pyth_price_any` handles all of them; non-trading or
        // unparseable feeds are silently ignored.
        let Some(price) = parse_pyth_price_any(&acc.data) else {
            parse_fail += 1;
            if parse_fail < 5 || parse_fail % 200 == 0 {
                eprintln!("  parse_fail #{parse_fail}: oracle={} bytes={}",
                    acc.account, acc.data.len());
            }
            continue;
        };
        if !price.trading { not_trading += 1; continue; }
        let price_usd = price.price as f64 * 10f64.powi(price.exponent);

        // Which banks does this oracle feed?
        let cache = state.cache.read().await;
        let bank_pks: Vec<Pubkey> = cache.banks_for_oracle(&acc.account).to_vec();
        drop(cache);
        if bank_pks.is_empty() { continue; }

        // Update prices for those banks.
        {
            let mut p = state.prices.write().await;
            for bpk in &bank_pks { p.insert(*bpk, price_usd); }
        }

        // Affected accounts = union of bank_to_accts[bank] for each bank.
        let bta = state.bank_to_accts.read().await;
        let mut affected: HashSet<usize> = HashSet::new();
        for bpk in &bank_pks {
            if let Some(idxs) = bta.get(bpk) {
                for i in idxs { affected.insert(*i); }
            }
        }
        drop(bta);
        if affected.is_empty() { continue; }

        // Recompute health for each affected account under the fresh
        // price snapshot. `compute_health` already handles the
        // missing-bank / missing-price short-circuits.
        let healthy_ttl = Duration::from_secs(
            std::env::var("MARGINFI_HEALTHY_TTL_SECS").ok()
                .and_then(|s| s.parse().ok()).unwrap_or(60)
        );
        let too_large_ttl = Duration::from_secs(
            std::env::var("MARGINFI_TOO_LARGE_TTL_SECS").ok()
                .and_then(|s| s.parse().ok()).unwrap_or(900)
        );
        let now = std::time::Instant::now();
        let healthy_set = state.seen_healthy.read().await;
        let too_large_set = state.seen_too_large.read().await;
        let cache = state.cache.read().await;
        let prices = state.prices.read().await.clone();
        let accts = state.accounts.read().await;
        let mut hits: Vec<(Pubkey, f64, f64)> = Vec::new();  // (acct, health, liab_usd)
        let mut skipped_healthy = 0u32;
        let mut skipped_too_large = 0u32;
        for &i in &affected {
            let Some(acct) = accts.get(i) else { continue; };
            // Calibration cache: skip accounts the program just told us
            // were healthy, and accounts whose tx didn't fit the v0 budget.
            if let Some(t) = healthy_set.get(&acct.address) {
                if now.duration_since(*t) < healthy_ttl {
                    skipped_healthy += 1; continue;
                }
            }
            if let Some(t) = too_large_set.get(&acct.address) {
                if now.duration_since(*t) < too_large_ttl {
                    skipped_too_large += 1; continue;
                }
            }
            let cand = compute_health(acct, &cache, &prices);
            if cand.health_usd >= 0.0 { continue; }
            if cand.total_liab_usd < min_debt_usd { continue; }
            // All position banks must be Push-V2 observable, else the
            // liquidate ix reverts with `LendingAccountBalanceSlotsFull` /
            // `OracleSetupInvalid` (6049). Cheap pre-filter.
            let all_pyth = acct.active_balances().all(|b|
                cache.by_bank.get(&b.bank_pk)
                    .map(|bs| bs.oracle_setup.is_pyth_observable())
                    .unwrap_or(false));
            if !all_pyth { continue; }
            hits.push((acct.address, cand.health_usd, cand.total_liab_usd));
        }
        drop(accts); drop(cache);
        drop(healthy_set); drop(too_large_set);
        if (skipped_healthy + skipped_too_large) > 0 && tick_count % 50 == 0 {
            eprintln!("  cache hits: skipped {} healthy + {} too-large this tick",
                skipped_healthy, skipped_too_large);
        }

        if hits.is_empty() { continue; }
        hits_total += hits.len() as u64;
        // Worst-health first.
        hits.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        let mut fired = state.fired_recently.write().await;
        fired.retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));

        // Top 5 hits per tick — protect the fire queue from burst flooding
        // on a single big oracle move. Workers handle order in arrival
        // sequence so worst-first matters.
        for (acct_pk, health, liab) in hits.iter().take(5) {
            if let Some(last) = fired.get(acct_pk) {
                if now.duration_since(*last) < Duration::from_secs(30) {
                    continue;
                }
            }
            fired.insert(*acct_pk, now);
            let reason = format!(
                "oracle={} health=${health:.2} liab=${liab:.2}", acc.account
            );
            if let Err(_) = fire_tx.try_send(FireRequest {
                candidate: *acct_pk, reason,
            }) {
                eprintln!("  fire queue full — dropping {acct_pk}");
            } else {
                fires_queued += 1;
            }
        }
    }
    eprintln!("tick_processor: subscriber channel closed");
}

// ─── Fire worker ───────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn fire_worker(
    worker_id:   usize,
    state:       Arc<HunterState>,
    rpc:         String,
    wallet:      Arc<Wallet>,
    cfg:         Arc<AttemptConfig>,
    fire_rx:     Arc<tokio::sync::Mutex<mpsc::Receiver<FireRequest>>>,
) {
    loop {
        let req = {
            let mut rx = fire_rx.lock().await;
            match rx.recv().await {
                Some(r) => r,
                None => { eprintln!("worker#{worker_id}: queue closed, exit"); return; }
            }
        };
        let t0 = std::time::Instant::now();
        let cache = state.cache.read().await;
        let prices = state.prices.read().await.clone();
        // `attempt_liquidation` already re-fetches the candidate, runs the
        // full naive→sim→send pipeline, and parses program logs. The
        // tick_processor only pre-filters — the worker is the authoritative
        // gate. Cache+prices are passed by ref so the worker uses the
        // freshest snapshot at fire time.
        let r = match attempt_liquidation(&rpc, &wallet, req.candidate, &cache, &prices, &cfg).await {
            Ok(r) => r,
            Err(e) => {
                drop(cache);
                // Stash "too large" candidates so tick_processor skips them
                // until the ALT catches up. The error string is RPC-formatted
                // — match the literal Solana RPC message.
                let s = e.to_string();
                if s.contains("VersionedTransaction too large") {
                    state.seen_too_large.write().await.insert(req.candidate, std::time::Instant::now());
                }
                eprintln!("w#{worker_id} {} attempt_err: {e}", req.candidate);
                continue;
            }
        };
        drop(cache);
        let dt = t0.elapsed();
        // Cache updates — drives the calibration loop in tick_processor.
        let now = std::time::Instant::now();
        if let Some(h) = r.program_pre_liquidation_health {
            if h > 0.0 {
                state.seen_healthy.write().await.insert(req.candidate, now);
            }
            // h < 0 means the program agrees — let it through, no cache write
        }
        if let Some(sz) = r.tx_size_bytes {
            if sz > 1232 {
                state.seen_too_large.write().await.insert(req.candidate, now);
            }
        }
        let prog = r.program_pre_liquidation_health.map(|h| format!("${h:.2}")).unwrap_or_else(|| "n/a".to_string());
        let sig_part = r.send_signature.as_ref().map(|s| format!(" 🚀 sig={s}")).unwrap_or_default();
        let skip_part = r.skip_reason.as_ref().map(|s| format!(" skip:{s}")).unwrap_or_default();
        let sim_part = r.sim_err.as_ref().map(|s| format!(" sim_err:{s}")).unwrap_or_default();
        println!(
            "w#{worker_id} {} dt={:.0}ms naive=${:.2} prog={prog} sz={}B reason={}{}{}{}",
            req.candidate, dt.as_millis(), r.naive_health_usd,
            r.tx_size_bytes.map(|s| s.to_string()).unwrap_or_else(|| "?".to_string()),
            req.reason, sig_part, skip_part, sim_part,
        );
    }
}

// ─── Background refresh ────────────────────────────────────────

async fn background_refresh(state: Arc<HunterState>, rpc: String, interval_secs: u64) {
    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        println!("→ background refresh: rebuilding account universe + bank cache");
        // Re-fetch bank cache too — banks can be added/closed by MarginFi
        // ops, though rare. Cheap (~50 entries, one gPA).
        let new_cache = match fetch_bank_cache(&rpc).await {
            Ok(c) => c,
            Err(e) => { eprintln!("refresh: bank cache failed: {e}"); continue; }
        };
        let new_prices = match fetch_bank_prices(&rpc, &new_cache).await {
            Ok(p) => p,
            Err(e) => { eprintln!("refresh: bank prices failed: {e}"); continue; }
        };
        let pks = match fetch_pubkeys(&rpc).await {
            Ok(p) => p,
            Err(e) => { eprintln!("refresh: pubkey fetch failed: {e}"); continue; }
        };
        let accts = match fetch_accounts_with_debt(&rpc, &pks).await {
            Ok(a) => a,
            Err(e) => { eprintln!("refresh: account fetch failed: {e}"); continue; }
        };
        let mut bta: HashMap<Pubkey, Vec<usize>> = HashMap::new();
        for (i, acc) in accts.iter().enumerate() {
            for bal in acc.active_balances() {
                bta.entry(bal.bank_pk).or_default().push(i);
            }
        }
        *state.cache.write().await         = new_cache;
        *state.accounts.write().await      = accts;
        *state.bank_to_accts.write().await = bta;
        *state.prices.write().await        = new_prices;
        // Drop stale calibration caches so the new account universe gets
        // a fresh look. Otherwise a recently-resolved underwater account
        // could stay flagged "healthy" past a price move that re-uns it.
        state.seen_healthy.write().await.clear();
        state.seen_too_large.write().await.clear();
        println!("  ✓ refresh done");
    }
}

// ─── Main ──────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    // Args are ignored in event-driven mode — kept for backward compat
    // with the systemd unit (`--loop 90`).
    let args: Vec<String> = std::env::args().collect();
    for a in args.iter().skip(1) {
        eprintln!("(ignored arg: {a} — event-driven mode has no loop interval)");
    }

    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY missing")?;
    let rpc = arb_scout::rpc::helius_rpc_url(&api_key);
    let wallet = {
        let s = std::env::var("SOLANA_KEYPAIR_JSON").context("SOLANA_KEYPAIR_JSON missing")?;
        Arc::new(Wallet::from_json_array(&s).map_err(|e| anyhow!("wallet: {e}"))?)
    };
    let liquidator_account: Pubkey = std::env::var("MARGINFI_ACCOUNT")
        .context("MARGINFI_ACCOUNT missing")?.parse()?;
    let pyth_alt: Option<Pubkey> = std::env::var("EXECUTOR_PYTH_ALT").ok().and_then(|s| s.parse().ok());
    let autofire = std::env::var("MARGINFI_AUTOFIRE").as_deref() == Ok("true");
    let close_factor: f64 = std::env::var("MARGINFI_CLOSE_FACTOR").ok().and_then(|s| s.parse().ok()).unwrap_or(0.5);
    let bonus: f64 = std::env::var("MARGINFI_LIQ_BONUS").ok().and_then(|s| s.parse().ok()).unwrap_or(0.05);
    let jito_tip: u64 = std::env::var("MARGINFI_JITO_TIP_LAMPORTS").ok().and_then(|s| s.parse().ok()).unwrap_or(500_000);
    let fire_workers: usize = std::env::var("MARGINFI_FIRE_WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or(2);
    let refresh_secs: u64 = std::env::var("MARGINFI_REFRESH_INTERVAL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(1800);

    let mode = if autofire { LiquidationMode::SendIfTrulyUnderwater } else { LiquidationMode::DryRun };

    println!("══════════════════════════════════════════════════════════════════");
    println!("  MarginFi hunter (event-driven)  autofire={autofire} workers={fire_workers}");
    println!("══════════════════════════════════════════════════════════════════");

    let cfg = Arc::new(AttemptConfig {
        liquidator_account, pyth_alt,
        close_factor, bonus,
        jito_tip_lamports: jito_tip,
        jito_tip_account: JITO_TIP_ACCOUNT.parse().unwrap(),
        compute_unit_limit: 1_400_000,
        priority_micro_lamports: 2_000,
        // Was 32 — Jupiter routes returning ≥30 accounts blew the v0 tx
        // past 1232 B even WITH MarginFi-bank ALT compression. 16 keeps
        // Jupiter on direct 1-2-hop routes for the major mints (USDC,
        // USDT, wSOL) which is all that matters for liquidation legs.
        jupiter_max_accounts: 16,
        jupiter_slippage_bps: 100,
        mode,
    });

    let state = Arc::new(bootstrap(&rpc).await?);

    // Subscription = every PRIMARY oracle of every Push-V2-observable bank.
    // Banks with non-Push-V2 oracles are dropped here because we can't act
    // on liquidations involving them anyway (would revert 6049).
    let oracles: Vec<Pubkey> = {
        let c = state.cache.read().await;
        c.by_bank.values()
            .filter(|b| b.oracle_setup.is_pyth_observable())
            .map(|b| b.primary_oracle())
            .filter(|p| *p != Pubkey::default())
            .collect::<HashSet<_>>().into_iter().collect()
    };
    println!("  → subscribing to {} bank oracles", oracles.len());

    let grpc_url = std::env::var("LASERSTREAM_GRPC_URL")
        .context("LASERSTREAM_GRPC_URL required for event-driven mode")?;

    let (acct_tx, acct_rx) = mpsc::channel::<arb_stream::wss::AccountUpdate>(2048);
    let (fire_tx, fire_rx) = mpsc::channel::<FireRequest>(128);
    let fire_rx = Arc::new(tokio::sync::Mutex::new(fire_rx));

    // E. Shred-Pyth fast-path (skip if SHRED_ALERT_DISABLE=true).
    // With `--features shredstream`: native Rust gRPC client. Otherwise: file bridge.
    let shred_disabled = std::env::var("SHRED_ALERT_DISABLE").ok().as_deref() == Some("true");
    if !shred_disabled {
        let oracles_for_shred = oracles.clone();
        #[cfg(feature = "shredstream")]
        {
            let endpoint = std::env::var("SHREDSTREAM_GRPC_URL")
                .unwrap_or_else(|_| "http://localhost:9000".to_string());
            arb_scout::shred_alert::spawn_native_shred_refetch(
                endpoint,
                rpc.clone(),
                oracles_for_shred,
                acct_tx.clone(),
                "pyth",
            );
            println!("  ✓ NATIVE shred-pyth fast-path spawned (Rust gRPC, {} oracles)", oracles.len());
        }
        #[cfg(not(feature = "shredstream"))]
        {
            tokio::spawn(arb_scout::shred_alert::poll_alert_then_refetch(
                "/tmp/shred-pyth-alert".to_string(),
                rpc.clone(),
                oracles_for_shred,
                acct_tx.clone(),
                "pyth".to_string(),
            ));
            println!("  ✓ shred-pyth file-bridge spawned ({} oracles)", oracles.len());
        }
    }

    // A. Oracle subscriber
    tokio::spawn(account_subscriber_loop(grpc_url, api_key.clone(), oracles, acct_tx));
    // B. Tick processor
    tokio::spawn(tick_processor(state.clone(), acct_rx, fire_tx));
    // C. Fire workers
    for w in 0..fire_workers {
        let s = state.clone();
        let rpc_c = rpc.clone();
        let wal = wallet.clone();
        let frx = fire_rx.clone();
        let cfg_c = cfg.clone();
        tokio::spawn(async move {
            fire_worker(w, s, rpc_c, wal, cfg_c, frx).await;
        });
    }
    // D. Background refresh
    tokio::spawn(background_refresh(state.clone(), rpc.clone(), refresh_secs));

    println!("✓ Event-driven MarginFi hunter live. Press Ctrl-C to exit.");
    futures::future::pending::<()>().await;
    Ok(())
}

// ─── Bootstrap loaders (one-shot variants of the legacy polling fns) ──

async fn fetch_pubkeys(rpc: &str) -> anyhow::Result<Vec<Pubkey>> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
        "params":[MARGINFI_V2_PROGRAM_STR, {
            "encoding":"base64","commitment":"confirmed",
            "filters":[{"dataSize": MARGINFI_ACCOUNT_SIZE}],
            "dataSlice":{"offset":0,"length":0},
        }],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.error_for_status()?.json().await?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no result array"))?;
    Ok(arr.iter()
        .filter_map(|e| e.get("pubkey").and_then(|v| v.as_str()).and_then(|s| s.parse().ok()))
        .collect())
}

async fn fetch_accounts_with_debt(
    rpc:     &str,
    pubkeys: &[Pubkey],
) -> anyhow::Result<Vec<ParsedAccount>> {
    let client = reqwest::Client::new();
    let mut out = Vec::new();
    // Bootstrap throughput: chunks of 100, throttled to stay under Helius
    // Business plan's 5/sec getMultipleAccounts cap. With 250ms pacing
    // 512K pubkeys → ~22 minutes once at startup. Acceptable.
    let throttle_ms: u64 = std::env::var("MFI_CHUNK_THROTTLE_MS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(250);
    let total_chunks = pubkeys.chunks(100).count();
    let mut chunk_idx = 0usize;
    for chunk in pubkeys.chunks(100) {
        chunk_idx += 1;
        let keys: Vec<String> = chunk.iter().map(|p| p.to_string()).collect();
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getMultipleAccounts",
            "params":[keys, {"encoding":"base64","commitment":"confirmed",
                "dataSlice":{"offset":0,"length":1736}}],
        });
        let mut attempt = 0;
        let r: serde_json::Value = loop {
            match client.post(rpc).json(&body).send().await {
                Ok(resp) => {
                    if resp.status().as_u16() == 429 && attempt < 6 {
                        let wait_ms = 500u64 << attempt.min(5);
                        attempt += 1;
                        eprintln!("  429 on chunk {chunk_idx}/{total_chunks} — retry {attempt} in {wait_ms}ms");
                        tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                        continue;
                    }
                    break resp.error_for_status()?.json().await?;
                }
                Err(e) if attempt < 6 => {
                    let wait_ms = 500u64 << attempt.min(5);
                    attempt += 1;
                    eprintln!("  net err {chunk_idx}/{total_chunks}: {e} — retry {attempt} in {wait_ms}ms");
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        };
        let vals = r.pointer("/result/value").and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("no result/value"))?;
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
        if chunk_idx % 200 == 0 {
            println!("    bootstrap progress: {}/{} chunks loaded, {} accts-with-debt so far",
                chunk_idx, total_chunks, out.len());
        }
        if throttle_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(throttle_ms)).await;
        }
    }
    Ok(out)
}
