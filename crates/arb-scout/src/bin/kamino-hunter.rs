//! Event-driven Kamino Lend liquidation hunter.
//!
//! ## Why event-driven
//!
//! The timer-based 8-min scan cycle (legacy: see `--scan-once`) reacts to
//! liquidation opportunities at most every 8 minutes. By then a competitor
//! bot — sub-second-paced ones run by Marinade, Mango, etc — has already
//! claimed it. This module flips the model: we sit on a Yellowstone gRPC
//! account-subscription for every Pyth feed referenced by Kamino reserves,
//! and on every tick we recompute fresh LTV for obligations affected by
//! that price and fire if liquidatable. Reaction window: ~5-10 ms.
//!
//! ## Architecture
//!
//! ```text
//!   bootstrap (one-time, ~8 min)
//!     └─ reserve cache + obligation universe + indices
//!         └─ pyth_to_reserves: HashMap<Pyth, Vec<Reserve>>
//!         └─ reserve_to_obls:  HashMap<Reserve, Vec<usize>>
//!
//!   tasks (long-running):
//!     A. pyth_subscriber  (LaserStream gRPC, accounts=pyth_feeds)
//!          → mpsc<AccountUpdate>
//!     B. tick_processor   (consume A, parse Pyth, recompute LTV)
//!          → mpsc<FireRequest>
//!     C. fire_worker × N  (refetch obligation, build+sim, TPU send)
//!     D. background_refresh (every 30 min: rebuild obligation cache)
//! ```
//!
//! ## Env
//!
//! Required: HELIUS_API_KEY, SOLANA_KEYPAIR_JSON, LASERSTREAM_GRPC_URL
//! Optional: EXECUTOR_PYTH_ALT, KAMINO_AUTOFIRE (default false),
//!           KAMINO_HUNT_MIN_DEBT_USD (50),
//!           KAMINO_JITO_TIP_LAMPORTS (500_000),
//!           KAMINO_MAX_FLASH_USD (5000),
//!           KAMINO_REFRESH_INTERVAL_SECS (1800),
//!           KAMINO_FIRE_WORKERS (2),
//!           NATIVE_KAMINO_HEALTH_MARGIN_BPS (100),
//!           NATIVE_SUBMISSION_PATH (tpu|helius — default tpu),
//!           NATIVE_TPU_FANOUT (4)
//!
//! ## Build
//!
//! Frankfurt: `cargo build --release --bin kamino-hunter --features laserstream`
//! Without the feature, the subscriber path is stubbed and the binary
//! falls back to the legacy timer scan (--scan-once).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use solana_program::pubkey::Pubkey;
use tokio::sync::{mpsc, RwLock};

use arb_scout::kamino_liq::{
    fetch_obligations_with_debt, fetch_reserve_cache,
    find_liquidatable_obligations_at_slot, fresh_market_price_sf,
    LiquidationIndex, ParsedObligation, ReserveCache, ReserveSummary,
    parse_obligation, parse_reserve, KAMINO_RESERVE_SIZE,
};
use arb_scout::kamino_scope::fresh_market_price_sf_from_scope;
use arb_scout::wallet::Wallet;
use arb_flashloan::{
    kamino_build_refresh_reserve_ix, kamino_build_refresh_obligation_ix,
    kamino_build_liquidate_ix, kamino_build_liquidate_v2_ix,
    kamino_derive_lending_market_authority,
    KaminoLiquidateContext, KaminoObligationFarmsAccounts,
    KAMINO_FARMS_PROGRAM,
    build_marginfi_flash_loan, derive_liquidity_vault, derive_liquidity_vault_authority,
    marginfi_bank_for_mint, MarginFiFlashContext, MARGINFI_MAIN_GROUP,
};
use arb_scout::jupiter_swap::JupiterSwapClient;
use arb_scout::raw_tx::{
    create_ata_idempotent_ix, derive_ata, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix, system_transfer_ix,
};
use arb_scout::v0_tx::{build_v0_message_multi, sign_v0_tx_b64};
use arb_dex_obric::pyth::parse_pyth_price_any;

const JITO_TIP_ACCOUNT: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";
const SF_SCALE_F64: f64 = (1u128 << 60) as f64;
const SPL_TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

// ─── Shared hunter state ───────────────────────────────────────

struct HunterState {
    cache:       RwLock<ReserveCache>,
    obligations: RwLock<Vec<ParsedObligation>>,
    index:       RwLock<LiquidationIndex>,
    /// Overrides keyed by reserve pubkey — fresh market_price_sf computed
    /// from a Pyth tick that hasn't yet been folded into the reserve's
    /// own cached price. tick_processor writes here; fire_worker reads.
    price_overrides: RwLock<HashMap<Pubkey, u128>>,
    /// Recently-fired obligation pubkeys (with timestamp) — used to
    /// suppress duplicate fires from rapid back-to-back ticks on the
    /// same feed. Pruned to last 60s on every insert.
    fired_recently:  RwLock<HashMap<Pubkey, std::time::Instant>>,
    /// Mints we've observed Jupiter return `TOKEN_NOT_TRADABLE` for
    /// (HTTP 400 response). Cached for the bot's lifetime so we don't
    /// keep hitting Jupiter with the same dead mint. Jupiter's portal
    /// counts every one of these as a 4xx error against our quota.
    untradable_mints: RwLock<HashSet<Pubkey>>,
}

struct FireRequest {
    obligation: Pubkey,
    /// Reason captured at queue time for logs (e.g. which tick triggered).
    reason:     String,
}

// ─── Refetch helper (pre-sim freshness gate) ───────────────────

async fn refetch_obligation(rpc: &str, address: &Pubkey)
    -> anyhow::Result<Option<ParsedObligation>>
{
    // Retry on 429 with capped backoff. Workers race against Helius's
    // rate-limit window when many ticks fire at once; without the retry
    // we drop fresh candidates (observed live 2026-05-16: 5+ candidates
    // lost per minute to "HTTP 429" in worker logs).
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[address.to_string(), {"encoding":"base64","commitment":"processed"}],
    });
    let client = reqwest::Client::new();
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..4u32 {
        match client.post(rpc).json(&body).send().await {
            Ok(resp) => {
                if resp.status().as_u16() == 429 && attempt < 3 {
                    tokio::time::sleep(Duration::from_millis(150u64 << attempt)).await;
                    continue;
                }
                let resp = resp.error_for_status()?;
                let r: serde_json::Value = resp.json().await?;
                let Some(data_b64) = r.pointer("/result/value/data/0").and_then(|v| v.as_str()) else {
                    return Ok(None);
                };
                let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
                return Ok(parse_obligation(*address, &bytes));
            }
            Err(e) if attempt < 3 => {
                last_err = Some(e.into());
                tokio::time::sleep(Duration::from_millis(150u64 << attempt)).await;
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("refetch_obligation: retries exhausted")))
}

// ─── Bootstrap ─────────────────────────────────────────────────

async fn bootstrap(rpc: &str) -> anyhow::Result<HunterState> {
    println!("→ bootstrap: loading reserve cache");
    let cache = fetch_reserve_cache(rpc).await?;
    let lending_markets: Vec<Pubkey> = cache.by_lending_market.keys().copied().collect();
    println!("  ✓ {} reserves, {} lending markets",
        cache.by_address.len(), lending_markets.len());

    println!("→ bootstrap: fetching obligation pubkeys across markets");
    let pks = arb_scout::kamino_liq::fetch_obligation_pubkeys_by_market(rpc, &lending_markets).await?;
    println!("  ✓ {} obligation pubkeys (has_debt=1 filter)", pks.len());

    println!("→ bootstrap: fetching obligation accounts in chunks (this is the slow leg)");
    let obs = fetch_obligations_with_debt(rpc, &pks).await?;
    println!("  ✓ {} obligations with debt parsed", obs.len());

    let index = LiquidationIndex::build(&cache, &obs);
    println!("  ✓ index: {} pyth_feeds → reserves, {} reserves → obligations",
        index.pyth_to_reserves.len(), index.reserve_to_obls.len());

    Ok(HunterState {
        cache:           RwLock::new(cache),
        obligations:     RwLock::new(obs),
        index:           RwLock::new(index),
        price_overrides: RwLock::new(HashMap::new()),
        fired_recently:  RwLock::new(HashMap::new()),
        untradable_mints: RwLock::new(HashSet::new()),
    })
}

// ─── Subscriber (LaserStream) ─────────────────────────────
//
// Single subscription covering BOTH Pyth feeds AND reserve accounts.
// Pyth gives us "price moved" (predictive); reserves give us "market_price_sf
// just refreshed" (reactive) — covers Scope/Switchboard reserves we have no
// parser for. The tick_processor dispatches by `acc.data.len()`.

#[cfg(feature = "laserstream")]
async fn account_subscriber_loop(
    grpc_url: String,
    api_key:  String,
    accounts: Vec<Pubkey>,
    tx:       mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    use arb_stream::wss::SessionEnd;
    loop {
        println!("→ laserstream: connecting ({} accounts: pyth + reserves)", accounts.len());
        let end = arb_stream::laserstream::run_one_session(
            &grpc_url, &api_key, &accounts, &tx, None,
        ).await;
        match end {
            SessionEnd::Closed   => eprintln!("laserstream closed cleanly — reconnecting"),
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
    eprintln!("laserstream feature disabled — event-driven mode requires --features laserstream");
}

// ─── Tick processor ────────────────────────────────────────────

async fn tick_processor(
    state:    Arc<HunterState>,
    mut rx:   mpsc::Receiver<arb_stream::wss::AccountUpdate>,
    fire_tx:  mpsc::Sender<FireRequest>,
) {
    let margin_bps: u32 = std::env::var("NATIVE_KAMINO_HEALTH_MARGIN_BPS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(100);
    let min_debt_usd: f64 = std::env::var("KAMINO_HUNT_MIN_DEBT_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(50.0);

    let mut tick_count: u64 = 0;
    let mut pyth_ticks: u64 = 0;
    let mut reserve_ticks: u64 = 0;
    let mut scope_ticks: u64 = 0;
    let mut parse_fail: u64 = 0;
    let mut not_trading: u64 = 0;
    let mut hits_total: u64 = 0;
    let mut last_heartbeat = std::time::Instant::now();
    while let Some(acc) = rx.recv().await {
        tick_count += 1;
        if last_heartbeat.elapsed() > Duration::from_secs(60) {
            println!("  💓 ticks={tick_count} pyth={pyth_ticks} scope={scope_ticks} reserve={reserve_ticks} parse_fail={parse_fail} not_trading={not_trading} hits={hits_total}");
            last_heartbeat = std::time::Instant::now();
        }

        // Dispatch:
        //   1. KAMINO_RESERVE_SIZE (8624 B)  → reserve refresh path
        //   2. owner == Scope program       → Scope predictive path
        //   3. otherwise                     → Pyth price account path
        let (affected_obls, reason_kind) = if acc.data.len() == KAMINO_RESERVE_SIZE {
            // ─── Reserve path: refresh_reserve just landed ─────────
            reserve_ticks += 1;
            let Some(new_r) = parse_reserve(acc.account, &acc.data) else {
                parse_fail += 1; continue;
            };
            // Write the fresh ReserveSummary into the cache so build_and_sim
            // (and subsequent ticks) see the new market_price_sf.
            {
                let mut c = state.cache.write().await;
                c.by_address.insert(acc.account, new_r);
            }
            // Drop any stale Pyth-predicted override for this reserve — the
            // cached value is now authoritative.
            state.price_overrides.write().await.remove(&acc.account);

            let idx = state.index.read().await;
            let mut set: HashSet<usize> = HashSet::new();
            if let Some(obi) = idx.reserve_to_obls.get(&acc.account) {
                for i in obi { set.insert(*i); }
            }
            (set, "reserve")
        } else if acc.owner == arb_scout::kamino_scope::KAMINO_SCOPE_PROGRAM {
            // ─── Scope path: aggregator account just updated ──────
            // For every reserve that scope_price_feed == this account, walk
            // its scope_chain → look up the fresh price → compute new
            // market_price_sf → write as override. Then re-evaluate the
            // union of affected obligations.
            scope_ticks += 1;

            let idx = state.index.read().await;
            let Some(reserve_pks) = idx.scope_to_reserves.get(&acc.account) else {
                continue;
            };
            let reserve_pks = reserve_pks.clone();
            drop(idx);

            let cache = state.cache.read().await;
            let idx = state.index.read().await;
            let mut overrides_local: HashMap<Pubkey, u128> = HashMap::new();
            let mut set: HashSet<usize> = HashSet::new();
            for rpk in &reserve_pks {
                let Some(r) = cache.by_address.get(rpk) else { continue; };
                let Some(sf) = fresh_market_price_sf_from_scope(&acc.data, &r.scope_chain) else {
                    continue;
                };
                // Sanity gate: reject scope-derived prices that diverge
                // more than 50% from the reserve's cached price. Real
                // prices rarely move that fast between observations; a
                // large diff is almost always a parser issue (uninitialised
                // entry, wrong index, composite-chain mismatch — saw
                // 9HB4 mint where Scope chain[108]=$1.19 vs cache $4.52,
                // 73% diff). The reserve subscription path still catches
                // real moves via refresh_reserve.
                let cached = r.market_price_sf;
                if cached > 0 {
                    // 1.5× = 50% wider, 1/1.5 = 33% narrower.
                    let hi = cached.saturating_mul(3) / 2;
                    let lo = cached * 2 / 3;
                    if sf > hi || sf < lo {
                        continue;
                    }
                }
                overrides_local.insert(*rpk, sf);
                if let Some(ob_idxs) = idx.reserve_to_obls.get(rpk) {
                    for i in ob_idxs { set.insert(*i); }
                }
            }
            drop(idx); drop(cache);
            {
                let mut po = state.price_overrides.write().await;
                for (k, v) in overrides_local.iter() { po.insert(*k, *v); }
            }
            (set, "scope_tick")
        } else {
            // ─── Pyth path ─────────────────────────────────────────
            let Some(price) = parse_pyth_price_any(&acc.data) else {
                parse_fail += 1;
                if parse_fail < 5 || parse_fail % 100 == 0 {
                    eprintln!("  parse_fail #{parse_fail}: account={} bytes={} owner={}",
                              acc.account, acc.data.len(), acc.owner);
                }
                continue;
            };
            if !price.trading { not_trading += 1; continue; }
            pyth_ticks += 1;

            let idx = state.index.read().await;
            let Some(reserve_pks) = idx.pyth_to_reserves.get(&acc.account) else {
                continue;
            };
            let reserve_pks = reserve_pks.clone();
            drop(idx);

            let cache = state.cache.read().await;
            let mut overrides_local: HashMap<Pubkey, u128> = HashMap::new();
            let mut set: HashSet<usize> = HashSet::new();
            let idx = state.index.read().await;
            for rpk in &reserve_pks {
                let Some(r) = cache.by_address.get(rpk) else { continue; };
                let Some(sf) = fresh_market_price_sf(price.price, price.exponent, r.mint_decimals) else {
                    continue;
                };
                overrides_local.insert(*rpk, sf);
                if let Some(ob_idxs) = idx.reserve_to_obls.get(rpk) {
                    for i in ob_idxs { set.insert(*i); }
                }
            }
            drop(idx); drop(cache);
            // Merge overrides into shared state.
            {
                let mut po = state.price_overrides.write().await;
                for (k, v) in overrides_local.iter() { po.insert(*k, *v); }
            }
            (set, "pyth_tick")
        };

        if affected_obls.is_empty() { continue; }

        // Re-evaluate each affected obligation under the fresh price/cache.
        let cache = state.cache.read().await;
        let obs = state.obligations.read().await;
        let overrides_combined = state.price_overrides.read().await.clone();
        let mut hits: Vec<(Pubkey, f64)> = Vec::new();
        for &i in &affected_obls {
            let Some(ob) = obs.get(i) else { continue; };
            let liq = ob.is_liquidatable_fresh_via_cached_ratios(
                &cache, &overrides_combined, margin_bps);
            if liq != Some(true) { continue; }
            let debt_usd =
                (ob.borrow_factor_adjusted_debt_value_sf as f64) / SF_SCALE_F64;
            if debt_usd < min_debt_usd { continue; }
            hits.push((ob.address, debt_usd));
        }
        drop(obs); drop(cache);

        if hits.is_empty() { continue; }
        hits_total += hits.len() as u64;
        hits.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let now = std::time::Instant::now();
        // Suppress fires for any obligation we tried in the last 30s — that
        // tx either landed (no-op now) or reverted (won't land for the same
        // reason on another tick this fast).
        let mut fired = state.fired_recently.write().await;
        fired.retain(|_, t| now.duration_since(*t) < Duration::from_secs(60));
        for (ob_pk, debt) in hits.iter().take(5) {
            if let Some(last) = fired.get(ob_pk) {
                if now.duration_since(*last) < Duration::from_secs(30) {
                    continue;
                }
            }
            fired.insert(*ob_pk, now);
            let reason = format!(
                "{}={} (debt=${:.2})", reason_kind, acc.account, debt
            );
            // Drop oldest if queue is full — backpressure shouldn't block
            // the price stream. fire_worker is the right place to fall
            // behind, not tick processing.
            if let Err(_) = fire_tx.try_send(FireRequest {
                obligation: *ob_pk, reason,
            }) {
                eprintln!("  fire queue full — dropping {ob_pk}");
            }
        }
    }
    eprintln!("tick_processor: subscriber channel closed");
}

// ─── Fire worker ───────────────────────────────────────────────

async fn fire_worker(
    worker_id:   usize,
    state:       Arc<HunterState>,
    rpc:         String,
    wallet:      Arc<Wallet>,
    pyth_alt:    Option<Pubkey>,
    fire_rx:     Arc<tokio::sync::Mutex<mpsc::Receiver<FireRequest>>>,
    tpu_sender:  Option<Arc<arb_scout::tpu_send::TpuSender>>,
    autofire:    bool,
    max_flash_usd: f64,
    jito_tip:    u64,
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
        // 1. Refetch the obligation right before sim — see canonical reason
        //    in legacy --scan-once path.
        let ob = match refetch_obligation(&rpc, &req.obligation).await {
            Ok(Some(o)) => o,
            Ok(None)    => { eprintln!("w#{worker_id} {} refetch:parse-fail", req.obligation); continue; }
            Err(e)      => { eprintln!("w#{worker_id} {} refetch:rpc {}", req.obligation, e); continue; }
        };
        // 2. Build + sim under read lock on cache.
        let cache = state.cache.read().await;
        let outcome = build_and_sim(
            &rpc, &wallet, pyth_alt, &cache, &ob,
            max_flash_usd.min(0.5 *
                (ob.borrow_factor_adjusted_debt_value_sf as f64) / SF_SCALE_F64),
            jito_tip, autofire, tpu_sender.as_deref(),
            Some(&state.untradable_mints),
        ).await;
        drop(cache);
        let dt = t0.elapsed();
        match outcome {
            Ok(out) => {
                let sig_part = out.send_sig.map(|s| format!(" 🚀 sig={s}")).unwrap_or_default();
                let err_part = out.sim_err.map(|e| format!(" sim:{e}")).unwrap_or_default();
                println!("w#{worker_id} {} dt={:.0}ms reason={}{}{}",
                    req.obligation, dt.as_millis(), req.reason, sig_part, err_part);
            }
            Err(e) => eprintln!("w#{worker_id} {} build_err: {e}", req.obligation),
        }
    }
}

// ─── Background refresh ────────────────────────────────────────

async fn background_refresh(state: Arc<HunterState>, rpc: String, interval_secs: u64) {
    loop {
        tokio::time::sleep(Duration::from_secs(interval_secs)).await;
        println!("→ background refresh: rebuilding obligation universe");
        let cache_snapshot = state.cache.read().await.by_lending_market.keys().copied().collect::<Vec<_>>();
        let pks = match arb_scout::kamino_liq::fetch_obligation_pubkeys_by_market(&rpc, &cache_snapshot).await {
            Ok(p) => p,
            Err(e) => { eprintln!("refresh: pubkey fetch failed: {e}"); continue; }
        };
        let obs = match fetch_obligations_with_debt(&rpc, &pks).await {
            Ok(o) => o,
            Err(e) => { eprintln!("refresh: accounts fetch failed: {e}"); continue; }
        };
        let cache = state.cache.read().await;
        let new_index = LiquidationIndex::build(&cache, &obs);
        drop(cache);
        *state.obligations.write().await = obs;
        *state.index.write().await = new_index;
        // Clear stale price overrides so the next tick reseeds them.
        state.price_overrides.write().await.clear();
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

    let args: Vec<String> = std::env::args().collect();
    let mut scan_once = false;
    let mut loop_secs: Option<u64> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--scan-once" => { scan_once = true; i += 1; }
            "--loop" => {
                loop_secs = Some(args.get(i + 1).ok_or_else(|| anyhow!("--loop N"))?.parse()?);
                i += 2;
            }
            x => bail!("unknown arg: {x}"),
        }
    }

    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY missing")?;
    let rpc = arb_scout::rpc::helius_rpc_url(&api_key);
    let wallet = {
        let s = std::env::var("SOLANA_KEYPAIR_JSON").context("SOLANA_KEYPAIR_JSON missing")?;
        Arc::new(Wallet::from_json_array(&s).map_err(|e| anyhow!("wallet: {e}"))?)
    };
    let pyth_alt: Option<Pubkey> = std::env::var("EXECUTOR_PYTH_ALT").ok().and_then(|s| s.parse().ok());
    let autofire = std::env::var("KAMINO_AUTOFIRE").as_deref() == Ok("true");
    let min_debt_usd: f64 = std::env::var("KAMINO_HUNT_MIN_DEBT_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(50.0);
    let jito_tip: u64 = std::env::var("KAMINO_JITO_TIP_LAMPORTS").ok().and_then(|s| s.parse().ok()).unwrap_or(500_000);
    let max_flash_usd: f64 = std::env::var("KAMINO_MAX_FLASH_USD").ok().and_then(|s| s.parse().ok()).unwrap_or(5000.0);
    let refresh_secs: u64 = std::env::var("KAMINO_REFRESH_INTERVAL_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(1800);
    let fire_workers: usize = std::env::var("KAMINO_FIRE_WORKERS").ok().and_then(|s| s.parse().ok()).unwrap_or(2);

    println!("══════════════════════════════════════════════════════════════════");
    println!("  Kamino hunter  (autofire={autofire}, min_debt=${min_debt_usd}, max_flash=${max_flash_usd})");
    println!("══════════════════════════════════════════════════════════════════");

    if scan_once {
        // Legacy one-shot scan — kept for spot-checks / Track-A validation.
        return legacy_scan_once(&rpc, &wallet, pyth_alt, min_debt_usd, max_flash_usd, jito_tip, autofire, loop_secs).await;
    }

    // ── Event-driven mode ─────────────────────────────────────
    let state = Arc::new(bootstrap(&rpc).await?);
    let pyth_feeds = state.index.read().await.pyth_feeds();
    let scope_feeds = state.index.read().await.scope_feeds();
    let reserve_addrs = state.index.read().await.reserve_addresses();
    // Combined subscription list:
    //   • Pyth feeds   → predictive ticks for the 6 Pyth-priced reserves
    //   • Scope feeds  → predictive ticks for the 200+ Scope-priced reserves
    //                    (one Scope account fans out to many reserves)
    //   • Reserves     → reactive fallback for anything missed
    let mut sub_accounts: Vec<Pubkey> = pyth_feeds.clone();
    sub_accounts.extend(scope_feeds.iter().copied());
    sub_accounts.extend(reserve_addrs.iter().copied());
    sub_accounts.sort();
    sub_accounts.dedup();
    println!("  → subscribing to {} accounts ({} pyth + {} scope + {} reserves)",
        sub_accounts.len(), pyth_feeds.len(), scope_feeds.len(), reserve_addrs.len());

    let grpc_url = std::env::var("LASERSTREAM_GRPC_URL")
        .context("LASERSTREAM_GRPC_URL required for event-driven mode")?;

    // TPU sender — preferred path. Falls back to Helius send if init fails
    // (network not reachable, etc).
    let submission_path = std::env::var("NATIVE_SUBMISSION_PATH").unwrap_or_else(|_| "tpu".to_string());
    let tpu_sender = if submission_path == "tpu" {
        match arb_scout::tpu_send::TpuSender::new(rpc.clone()).await {
            Ok(s) => {
                let s = Arc::new(s);
                s.spawn_refresher();
                println!("  ✓ TpuSender initialised (path=tpu)");
                Some(s)
            }
            Err(e) => {
                eprintln!("  TpuSender init failed: {e} — falling back to Helius sendTransaction");
                None
            }
        }
    } else { None };

    let (acct_tx, acct_rx) = mpsc::channel::<arb_stream::wss::AccountUpdate>(2048);
    let (fire_tx, fire_rx) = mpsc::channel::<FireRequest>(128);
    let fire_rx = Arc::new(tokio::sync::Mutex::new(fire_rx));

    // E. Shred fast-paths (skip if SHRED_ALERT_DISABLE=true).
    //
    // With `--features shredstream` (Phase D) the bot subscribes directly
    // to Jito's ShredStream Proxy gRPC and bypasses the Python tee. The
    // file-bridge path remains as a build-without-shredstream fallback.
    let shred_disabled = std::env::var("SHRED_ALERT_DISABLE").ok().as_deref() == Some("true");
    if !shred_disabled {
        #[cfg(feature = "shredstream")]
        {
            let endpoint = std::env::var("SHREDSTREAM_GRPC_URL")
                .unwrap_or_else(|_| "http://localhost:9000".to_string());
            arb_scout::shred_alert::spawn_native_shred_refetch(
                endpoint.clone(),
                rpc.clone(),
                pyth_feeds.clone(),
                acct_tx.clone(),
                "pyth",
            );
            arb_scout::shred_alert::spawn_native_shred_refetch(
                endpoint,
                rpc.clone(),
                scope_feeds.clone(),
                acct_tx.clone(),
                "kamino",
            );
            println!("  ✓ NATIVE shred fast-paths spawned (Rust gRPC: pyth + kamino)");
        }
        #[cfg(not(feature = "shredstream"))]
        {
            tokio::spawn(arb_scout::shred_alert::poll_alert_then_refetch(
                "/tmp/shred-pyth-alert".to_string(),
                rpc.clone(),
                pyth_feeds.clone(),
                acct_tx.clone(),
                "pyth".to_string(),
            ));
            tokio::spawn(arb_scout::shred_alert::poll_alert_then_refetch(
                "/tmp/shred-kamino-alert".to_string(),
                rpc.clone(),
                scope_feeds.clone(),
                acct_tx.clone(),
                "kamino-scope".to_string(),
            ));
            println!("  ✓ shred file-bridge spawned (Python tee fallback)");
        }
    }

    // A. Combined account subscriber (Pyth + Reserves)
    tokio::spawn(account_subscriber_loop(grpc_url, api_key.clone(), sub_accounts, acct_tx));
    // B. Tick processor
    tokio::spawn(tick_processor(state.clone(), acct_rx, fire_tx));
    // C. Fire workers
    for w in 0..fire_workers {
        let s = state.clone();
        let rpc_c = rpc.clone();
        let wal = wallet.clone();
        let frx = fire_rx.clone();
        let tpu = tpu_sender.clone();
        tokio::spawn(async move {
            fire_worker(w, s, rpc_c, wal, pyth_alt, frx, tpu, autofire, max_flash_usd, jito_tip).await;
        });
    }
    // D. Background refresh
    tokio::spawn(background_refresh(state.clone(), rpc.clone(), refresh_secs));

    println!("✓ Event-driven hunter live. Press Ctrl-C to exit.");
    // Sleep forever — tasks own their own loops.
    futures::future::pending::<()>().await;
    Ok(())
}

// ─── Legacy one-shot scan (preserved for spot checks) ──────────

async fn legacy_scan_once(
    rpc:           &str,
    wallet:        &Wallet,
    pyth_alt:      Option<Pubkey>,
    min_debt_usd:  f64,
    max_flash_usd: f64,
    jito_tip:      u64,
    autofire:      bool,
    loop_secs:     Option<u64>,
) -> anyhow::Result<()> {
    let top_n: usize = std::env::var("KAMINO_HUNT_TOP_N").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    loop {
        let t0 = std::time::Instant::now();
        match scan_and_hunt(rpc, wallet, pyth_alt, top_n, min_debt_usd, max_flash_usd, jito_tip, autofire).await {
            Ok(fired) => if fired { println!("🎯 FIRED — stopping loop"); break; }
            Err(e) => eprintln!("scan failed: {e}"),
        }
        match loop_secs {
            None => break,
            Some(s) => {
                let elapsed = t0.elapsed();
                let sleep_for = Duration::from_secs(s).saturating_sub(elapsed);
                println!("\n--- cycle took {:.1}s, sleeping {:.0}s ---", elapsed.as_secs_f32(), sleep_for.as_secs_f32());
                tokio::time::sleep(sleep_for).await;
            }
        }
    }
    Ok(())
}

async fn scan_and_hunt(
    rpc:           &str,
    wallet:        &Wallet,
    pyth_alt:      Option<Pubkey>,
    top_n:         usize,
    min_debt_usd:  f64,
    max_flash_usd: f64,
    jito_tip:      u64,
    autofire:      bool,
) -> anyhow::Result<bool> {
    println!("\n→ loading caches");
    let cache = fetch_reserve_cache(rpc).await?;
    let lending_markets: Vec<solana_program::pubkey::Pubkey> =
        cache.by_lending_market.keys().copied().collect();
    println!("  → fetching obligation pubkeys across {} lending markets", lending_markets.len());
    let pks = arb_scout::kamino_liq::fetch_obligation_pubkeys_by_market(rpc, &lending_markets).await?;
    println!("  → total obligation pubkeys: {}", pks.len());
    let obs = fetch_obligations_with_debt(rpc, &pks).await?;
    let current_slot = {
        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getSlot"});
        let resp: serde_json::Value = reqwest::Client::new()
            .post(rpc).json(&body).send().await?.json().await?;
        resp.pointer("/result").and_then(|v| v.as_u64()).unwrap_or(0)
    };
    let candidates = find_liquidatable_obligations_at_slot(
        &obs, min_debt_usd, &cache, current_slot,
    );
    println!("✓ {} reserves, {} obligations w/ debt, {} cached-liquidatable (slot={})",
        cache.by_address.len(), obs.len(), candidates.len(), current_slot);
    if candidates.is_empty() { return Ok(false); }

    for (i, o_stale) in candidates.iter().take(top_n).enumerate() {
        let o = match refetch_obligation(rpc, &o_stale.address).await {
            Ok(Some(fresh)) => fresh,
            Ok(None)        => { println!("  #{i:>2}  ❌ refetch parse failed"); continue; }
            Err(e)          => { println!("  #{i:>2}  ❌ refetch RPC: {e}"); continue; }
        };
        let margin_bps: u32 = std::env::var("NATIVE_KAMINO_HEALTH_MARGIN_BPS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(100);
        if !o.is_liquidatable_per_cache_with_margin_bps(margin_bps) {
            let stale_ratio = o_stale.cached_ltv_ratio().unwrap_or(0.0);
            let fresh_ratio = o.cached_ltv_ratio().unwrap_or(0.0);
            println!("  #{i:>2}  skip:fresh_healthy stale_ltv={stale_ratio:.3} fresh_ltv={fresh_ratio:.3}");
            continue;
        }
        let ratio = o.cached_ltv_ratio().unwrap_or(0.0);
        let debt_usd = (o.borrow_factor_adjusted_debt_value_sf as f64) / SF_SCALE_F64;
        let cap = max_flash_usd.min(debt_usd * 0.5);
        if cap < 1.0 {
            println!("  #{i:>2}  ltv={ratio:.3}  debt=${debt_usd:.2}  skip:tiny");
            continue;
        }
        match build_and_sim(rpc, wallet, pyth_alt, &cache, &o, cap, jito_tip, autofire, None, None).await {
            Ok(outcome) => {
                let send_part = outcome.send_sig.as_ref().map(|s| format!(" 🚀 sig={s}")).unwrap_or_default();
                let err_part = outcome.sim_err.as_ref().map(|e| format!(" sim:{e}")).unwrap_or_default();
                println!("  #{i:>2}  ltv={ratio:.3}  debt=${debt_usd:.2}  cap=${cap:.2}  tx={}B{send_part}{err_part}",
                    outcome.tx_size.map(|s| s.to_string()).unwrap_or_else(|| "n/a".into()));
                if outcome.send_sig.is_some() { return Ok(true); }
            }
            Err(e) => println!("  #{i:>2}  ❌ build error: {e}"),
        }
    }
    Ok(false)
}

// ─── Build + sim + send (shared by event-driven and legacy) ─────

#[derive(Debug, Default)]
struct Outcome {
    tx_size:  Option<usize>,
    sim_err:  Option<String>,
    send_sig: Option<String>,
}

/// Parse Jupiter's "The token <pubkey> is not tradable" error to extract
/// the offending mint pubkey for caching.
fn extract_untradable_mint(err: &str) -> Option<Pubkey> {
    let prefix = "The token ";
    let suffix = " is not tradable";
    let start = err.find(prefix)? + prefix.len();
    let end = err[start..].find(suffix)? + start;
    err[start..end].parse().ok()
}

async fn build_and_sim(
    rpc:           &str,
    wallet:        &Wallet,
    pyth_alt:      Option<Pubkey>,
    cache:         &ReserveCache,
    o:             &ParsedObligation,
    repay_usd:     f64,
    jito_tip:      u64,
    autofire:      bool,
    tpu_sender:    Option<&arb_scout::tpu_send::TpuSender>,
    untradable:    Option<&RwLock<HashSet<Pubkey>>>,
) -> anyhow::Result<Outcome> {
    let payer = wallet.pubkey();
    let tip_acct: Pubkey = JITO_TIP_ACCOUNT.parse().unwrap();
    let spl_token: Pubkey = SPL_TOKEN_PROGRAM.parse().unwrap();
    let mut out = Outcome::default();

    let Some(largest_borrow) = o.borrows.iter().max_by(|a, b| a.borrowed_amount_sf.cmp(&b.borrowed_amount_sf)) else {
        out.sim_err = Some("no borrow".into()); return Ok(out);
    };
    let Some(largest_deposit) = o.deposits.iter().max_by(|a, b| a.deposited_amount.cmp(&b.deposited_amount)) else {
        out.sim_err = Some("no deposit".into()); return Ok(out);
    };
    let repay_reserve  = cache.by_address.get(&largest_borrow.borrow_reserve)
        .ok_or_else(|| anyhow!("repay reserve not in cache"))?;
    let withdraw_reserve = cache.by_address.get(&largest_deposit.deposit_reserve)
        .ok_or_else(|| anyhow!("withdraw reserve not in cache"))?;

    let needs_v2 = repay_reserve.has_farm_collateral() || repay_reserve.has_farm_debt()
                || withdraw_reserve.has_farm_collateral() || withdraw_reserve.has_farm_debt();

    // ── Bank lookup. If MarginFi doesn't have a bank for this repay mint
    //    (we currently whitelist USDC/USDT/WBTC/wSOL) we skip — the
    //    obligation will be hit only on the rare opportunities in our 4
    //    bank universe.
    let repay_mint = repay_reserve.liquidity_mint;
    let collateral_mint = withdraw_reserve.liquidity_mint;
    let Some(bank_info) = marginfi_bank_for_mint(&repay_mint) else {
        out.sim_err = Some(format!("skip — no MarginFi bank for repay_mint {repay_mint}"));
        return Ok(out);
    };
    let mfi_acct: Pubkey = std::env::var("MARGINFI_ACCOUNT")
        .map_err(|_| anyhow!("MARGINFI_ACCOUNT env missing"))?
        .parse().map_err(|e| anyhow!("MARGINFI_ACCOUNT parse: {e}"))?;

    if repay_reserve.market_price_sf == 0 {
        out.sim_err = Some("repay-reserve cached market_price_sf is 0 — refresh needed".into());
        return Ok(out);
    }
    // Repay amount: repay_usd USD → smallest-units of repay_mint.
    //   market_price_sf is USD per WHOLE token × 2^60 (verified on-chain).
    //   whole_tokens = repay_usd × 2^60 / market_price_sf
    //   smallest_units = whole_tokens × 10^decimals
    let dec_pow_f = 10f64.powi(repay_reserve.mint_decimals as i32);
    let raw_f = repay_usd * SF_SCALE_F64 * dec_pow_f / (repay_reserve.market_price_sf as f64);
    let flash_amount = raw_f as u64;
    if flash_amount == 0 {
        out.sim_err = Some(format!(
            "computed flash_amount=0 (usd={repay_usd}, sf={})",
            repay_reserve.market_price_sf
        ));
        return Ok(out);
    }

    let repay_ata = derive_ata(&payer, &repay_mint);
    let withdraw_ata = derive_ata(&payer, &collateral_mint);
    let c_token_ata = derive_ata(&payer, &withdraw_reserve.collateral_mint);

    // ── Jupiter swap quote (only when collateral_mint != repay_mint).
    //    We estimate the seized-collateral amount conservatively at
    //    repay_value × (1 + LIQ_BONUS_BPS / 10_000) ÷ collateral_price,
    //    then quote ExactIn from collateral_mint → repay_mint for that
    //    amount. Set slippage_bps from env; min_out is whatever Jupiter
    //    returns. On-chain sim is the final safety filter.
    let liq_bonus_bps: u64 = std::env::var("KAMINO_LIQUIDATION_BONUS_BPS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(500);  // conservative 5%
    let need_swap = collateral_mint != repay_mint;
    let mut jupiter_setup: Vec<solana_program::instruction::Instruction> = Vec::new();
    let mut jupiter_swap_ix: Option<solana_program::instruction::Instruction> = None;
    let mut jupiter_cleanup: Option<solana_program::instruction::Instruction> = None;
    let mut jupiter_alt_pks: Vec<Pubkey> = Vec::new();
    if need_swap {
        if withdraw_reserve.market_price_sf == 0 {
            out.sim_err = Some("withdraw-reserve cached market_price_sf is 0".into());
            return Ok(out);
        }
        // seized_usd = repay_usd × (1 + bonus)
        // seized_smallest = seized_usd × 2^60 × 10^coll_decimals / withdraw_market_price_sf
        let seized_usd = repay_usd * (1.0 + liq_bonus_bps as f64 / 10_000.0);
        let coll_dec_pow = 10f64.powi(withdraw_reserve.mint_decimals as i32);
        let seized_raw_f = seized_usd * SF_SCALE_F64 * coll_dec_pow
            / (withdraw_reserve.market_price_sf as f64);
        // Haircut 10% for sim safety — actual on-chain seized varies by reserve config.
        let swap_input_amount = (seized_raw_f * 0.90) as u64;
        if swap_input_amount == 0 {
            out.sim_err = Some("computed swap_input_amount=0".into());
            return Ok(out);
        }
        // Cap jupiter accounts low so the tx fits with flash + kamino +
        // refresh_reserves. Live observation: maxAccounts=12 with
        // restrict=true blocked 46% of fires with "No routes found" for
        // exotic LSTs (jupSOL, bSOL, USD1 etc) — those don't route well
        // through the curated intermediate set. Letting Jupiter pick
        // intermediates freely + a moderately larger maxAccounts gets
        // routes for them; tx-size pressure is now handled by ALT
        // compression (alt-extend-kamino-flash deployment).
        std::env::set_var("EXECUTOR_MAX_ACCOUNTS",
            &std::env::var("KAMINO_JUPITER_MAX_ACCOUNTS").unwrap_or_else(|_| "20".to_string()));
        // Read JUPITER_RESTRICT_INTERMEDIATES from operator env — don't
        // force it. Default false so exotic LST routes resolve.
        if std::env::var("JUPITER_RESTRICT_INTERMEDIATES").is_err() {
            std::env::set_var("JUPITER_RESTRICT_INTERMEDIATES", "false");
        }
        let jup = JupiterSwapClient::new();
        let slippage_bps: u32 = std::env::var("KAMINO_JUPITER_SLIPPAGE_BPS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(100);
        // Skip if EITHER mint is in the untradable cache — Jupiter charges
        // us a 4xx on the API portal for these (~70+ /min observed for
        // GYiUmJ8, 9HB4, BTRR3) so we MUST avoid resending.
        if let Some(set) = untradable {
            let s = set.read().await;
            if s.contains(&collateral_mint) || s.contains(&repay_mint) {
                out.sim_err = Some(format!(
                    "skip — mint cached as TOKEN_NOT_TRADABLE (collateral={collateral_mint} repay={repay_mint})"
                ));
                return Ok(out);
            }
        }

        let quote = match jup.quote(&collateral_mint, &repay_mint, swap_input_amount, slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                let msg = e.to_string();
                // Parse "The token <pubkey> is not tradable" and remember it.
                if msg.contains("not tradable") || msg.contains("TOKEN_NOT_TRADABLE") {
                    if let Some(bad) = extract_untradable_mint(&msg) {
                        if let Some(set) = untradable {
                            set.write().await.insert(bad);
                        }
                    }
                }
                out.sim_err = Some(format!("jupiter quote: {e}"));
                return Ok(out);
            }
        };
        let jup_ixs = match jup.swap_instructions(&quote, &payer, false).await {
            Ok(i) => i,
            Err(e) => { out.sim_err = Some(format!("jupiter swap-ixs: {e}")); return Ok(out); }
        };
        jupiter_setup = jup_ixs.setup_ixs;
        jupiter_swap_ix = Some(jup_ixs.swap_ix);
        jupiter_cleanup = jup_ixs.cleanup_ix;
        jupiter_alt_pks = jup_ixs.address_lookup_tables;
    }

    // ── Build refresh ixs (Kamino mid-section).
    let mut middle_ixs = Vec::with_capacity(20);
    let push_refresh = |all_ixs: &mut Vec<solana_program::instruction::Instruction>,
                        r: &ReserveSummary| {
        let pyth     = (r.pyth_price            != Pubkey::default()).then_some(r.pyth_price);
        let sb_price = (r.switchboard_price_agg != Pubkey::default()).then_some(r.switchboard_price_agg);
        let sb_twap  = (r.switchboard_twap_agg  != Pubkey::default()).then_some(r.switchboard_twap_agg);
        let scope    = (r.scope_price_feed      != Pubkey::default()).then_some(r.scope_price_feed);
        all_ixs.push(kamino_build_refresh_reserve_ix(
            r.address, r.lending_market, pyth, sb_price, sb_twap, scope,
        ));
    };
    push_refresh(&mut middle_ixs, repay_reserve);
    if withdraw_reserve.address != repay_reserve.address {
        push_refresh(&mut middle_ixs, withdraw_reserve);
    }
    let mut already = vec![repay_reserve.address, withdraw_reserve.address];
    for ref_pk in o.referenced_reserves() {
        if already.contains(&ref_pk) { continue; }
        if let Some(rs) = cache.by_address.get(&ref_pk) {
            push_refresh(&mut middle_ixs, rs);
            already.push(ref_pk);
        }
    }

    let deposit_reserves: Vec<Pubkey> = o.deposits.iter().map(|d| d.deposit_reserve).collect();
    let borrow_reserves:  Vec<Pubkey> = o.borrows.iter().map(|b| b.borrow_reserve).collect();
    middle_ixs.push(kamino_build_refresh_obligation_ix(
        o.lending_market, o.address, &deposit_reserves, &borrow_reserves, &[],
    ));

    let liq_ctx = KaminoLiquidateContext {
        obligation: o.address,
        lending_market: o.lending_market,
        lending_market_authority: kamino_derive_lending_market_authority(&o.lending_market),
        repay_reserve: repay_reserve.address,
        repay_reserve_liquidity_mint: repay_reserve.liquidity_mint,
        repay_reserve_liquidity_supply: repay_reserve.liquidity_supply_vault,
        withdraw_reserve: withdraw_reserve.address,
        withdraw_reserve_liquidity_mint: withdraw_reserve.liquidity_mint,
        withdraw_reserve_collateral_mint: withdraw_reserve.collateral_mint,
        withdraw_reserve_collateral_supply: withdraw_reserve.collateral_supply_vault,
        withdraw_reserve_liquidity_supply: withdraw_reserve.liquidity_supply_vault,
        withdraw_reserve_liquidity_fee_receiver: withdraw_reserve.liquidity_fee_vault,
        user_source_liquidity: repay_ata,
        user_destination_collateral: c_token_ata,
        user_destination_liquidity: withdraw_ata,
        collateral_token_program: spl_token,
        repay_liquidity_token_program: spl_token,
        withdraw_liquidity_token_program: spl_token,
    };
    let mut all_ref: Vec<Pubkey> = Vec::new();
    for d in &o.deposits { all_ref.push(d.deposit_reserve); }
    for b in &o.borrows  { all_ref.push(b.borrow_reserve); }
    if needs_v2 {
        let none_farm = KaminoObligationFarmsAccounts::default();
        middle_ixs.push(kamino_build_liquidate_v2_ix(
            &liq_ctx, payer, flash_amount, 0, 0,
            none_farm, none_farm, KAMINO_FARMS_PROGRAM, &all_ref,
        ));
    } else {
        middle_ixs.push(kamino_build_liquidate_ix(
            &liq_ctx, payer, flash_amount, 0, 0, &all_ref,
        ));
    }

    // Append Jupiter swap after kamino liquidate.
    middle_ixs.extend(jupiter_setup);
    if let Some(swap) = jupiter_swap_ix.clone() { middle_ixs.push(swap); }
    if let Some(c)  = jupiter_cleanup.clone() { middle_ixs.push(c); }

    // ── Assemble flash envelope.
    //
    // Layout:
    //   prefix (6) + pre_flash (2: start, borrow) + middle (M) + post_flash (2: repay, end)
    //   end_index = total_ixs - 1 = 6 + 2 + M + 2 - 1 = M + 9
    let middle_count = middle_ixs.len();
    let total_count  = 6 + 2 + middle_count + 2;
    let end_index    = (total_count - 1) as u64;

    let mfi_ctx = MarginFiFlashContext {
        group: MARGINFI_MAIN_GROUP,
        bank:  bank_info.bank,
        liquidity_vault:           derive_liquidity_vault(&bank_info.bank),
        liquidity_vault_authority: derive_liquidity_vault_authority(&bank_info.bank),
        usdc_pyth_oracle:          bank_info.pyth_oracle,
        marginfi_account:          mfi_acct,
    };
    let (pre_flash, post_flash) = build_marginfi_flash_loan(
        &mfi_ctx, payer, repay_ata, flash_amount, end_index,
    );

    let mut all_ixs: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(total_count);
    // Prefix
    all_ixs.push(set_compute_unit_limit_ix(1_400_000));
    all_ixs.push(set_compute_unit_price_ix(2_000));
    all_ixs.push(system_transfer_ix(payer, tip_acct, jito_tip));
    all_ixs.push(create_ata_idempotent_ix(payer, payer, repay_mint));
    all_ixs.push(create_ata_idempotent_ix(payer, payer, withdraw_reserve.collateral_mint));
    all_ixs.push(create_ata_idempotent_ix(payer, payer, collateral_mint));
    // Flash + middle + flash-close
    all_ixs.extend(pre_flash);
    all_ixs.extend(middle_ixs);
    all_ixs.extend(post_flash);
    debug_assert_eq!(all_ixs.len(), total_count, "tx layout count mismatch");

    // ── ALTs: pyth + jupiter ALTs (Jupiter swap_instructions returns them
    //    so the tx fits in 1232 bytes with all those routed-pool accounts).
    let mut alts: Vec<(Pubkey, Vec<Pubkey>)> = Vec::new();
    if let Some(alt_pk) = pyth_alt {
        if let Ok(addrs) = arb_scout::alt::fetch_alt_addresses(rpc, &alt_pk).await {
            alts.push((alt_pk, addrs));
        }
    }
    // Second ALT dedicated to kamino-flash (LST banks + hot reserves);
    // exists because EXECUTOR_PYTH_ALT hit Solana's 256-address cap.
    if let Ok(alt2_str) = std::env::var("EXECUTOR_KAMINO_ALT2") {
        if let Ok(alt2_pk) = alt2_str.parse::<Pubkey>() {
            if let Ok(addrs) = arb_scout::alt::fetch_alt_addresses(rpc, &alt2_pk).await {
                alts.push((alt2_pk, addrs));
            }
        }
    }
    for jup_alt_pk in &jupiter_alt_pks {
        if let Ok(addrs) = arb_scout::alt::fetch_alt_addresses(rpc, jup_alt_pk).await {
            alts.push((*jup_alt_pk, addrs));
        }
    }

    let blockhash = fetch_latest_blockhash(rpc).await.map_err(|e| anyhow!("blockhash: {e}"))?;
    let msg = build_v0_message_multi(&payer, &all_ixs, &blockhash, &alts, &[]);
    let signed_b64 = sign_v0_tx_b64(&msg, wallet.bytes()).map_err(|e| anyhow!("sign: {e}"))?;
    let signed_bytes = base64::engine::general_purpose::STANDARD.decode(&signed_b64).unwrap();
    out.tx_size = Some(signed_bytes.len());

    let sim = rpc_simulate(rpc, &signed_b64).await?;
    if let Some(e) = &sim.err {
        let tail: Vec<&str> = sim.logs.iter().rev().take(8).rev().map(|s| s.as_str()).collect();
        let oracle_info = format!("repay_oracle={:?} withdraw_oracle={:?}",
            repay_reserve.active_oracle(), withdraw_reserve.active_oracle());
        out.sim_err = Some(format!("{e} | {oracle_info} | logs_tail: [{}]", tail.join(" | ")));
        return Ok(out);
    }

    if autofire {
        // Prefer TPU when available; fall back to Helius sendTransaction.
        if let Some(tpu) = tpu_sender {
            match arb_scout::tpu_send::fetch_current_slot(rpc).await {
                Ok(slot) => {
                    match tpu.send_to_next_leaders(&signed_bytes, slot, 4).await {
                        Ok(_) => {
                            out.send_sig = arb_scout::tpu_send::extract_signature_base58(&signed_bytes);
                        }
                        Err(e) => {
                            eprintln!("tpu send failed ({e}) — falling back to Helius");
                            out.send_sig = Some(rpc_send(rpc, &signed_b64).await?);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("getSlot for tpu failed ({e}) — Helius fallback");
                    out.send_sig = Some(rpc_send(rpc, &signed_b64).await?);
                }
            }
        } else {
            out.send_sig = Some(rpc_send(rpc, &signed_b64).await?);
        }
    }
    Ok(out)
}

struct InlineSim { err: Option<String>, logs: Vec<String> }

async fn rpc_simulate(rpc: &str, signed_b64: &str) -> anyhow::Result<InlineSim> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
        "params":[signed_b64, {"encoding":"base64","sigVerify":false,"replaceRecentBlockhash":true,"commitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.error_for_status()?.json().await?;
    if let Some(err) = r.get("error") { bail!("simulate RPC error: {err}"); }
    let val = r.pointer("/result/value").ok_or_else(|| anyhow!("simulate: missing result/value"))?;
    let err = val.get("err").and_then(|e| if e.is_null() { None } else { Some(format!("{e}")) });
    let logs = val.get("logs").and_then(|l| l.as_array()).cloned().unwrap_or_default()
        .into_iter().filter_map(|l| l.as_str().map(String::from)).collect();
    Ok(InlineSim { err, logs })
}

async fn rpc_send(rpc: &str, signed_b64: &str) -> anyhow::Result<String> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {"encoding":"base64","skipPreflight":false,"maxRetries":5,"preflightCommitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.error_for_status()?.json().await?;
    if let Some(err) = r.get("error") { bail!("send RPC error: {err}"); }
    Ok(r.pointer("/result").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("send: missing sig"))?.to_string())
}
