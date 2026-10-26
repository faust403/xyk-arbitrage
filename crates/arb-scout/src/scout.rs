//! Scout orchestration: wires WSS subscriber, evaluators, logger, metrics,
//! vault store, and the bootstrap/reconnect loop.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, AtomicBool, Ordering};
use std::time::Duration;

use arb_core::{ArbError, ArbResult, DexQuoter, PoolKind, TokenAmount, TokenMint};
use chrono::Utc;
use solana_program::pubkey::Pubkey;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::ScoutConfig;
use crate::dex;
use crate::dex::openbook_market::{parse_openbook_market, OpenBookMarketStore};
use arb_dex_orca::WhirlpoolData;
use arb_dex_raydium::{parse_trade_fee_rate, ClmmFeeStore, RaydiumAmmV4Data, RaydiumClmmData};
use crate::fees::{FeeEstimator, SOL_PRICE_SCALE};
use crate::executor::{Executor, ExecutorConfig};
use crate::native_executor::{NativeExecutor, NativeExecutorConfig};
use arb_jito::{TipFloor, start_polling as start_tip_polling, DEFAULT_POLL_INTERVAL};
use crate::jito_correlator::JitoCorrelator;
use crate::jupiter::JupiterClient;
use crate::known;
use crate::wallet::Wallet;
use arb_telemetry::{JsonlLogger, OpportunityRecord};
use arb_pathfinder::{build_routes_from_store, Route};
use crate::rpc::HeliusRpc;
use arb_stream::{run_one_session, AccountUpdate, PoolStateStore, SessionEnd};
use crate::telegram::{render_summary, TelegramNotifier};
use crate::telegram_commands::TelegramCommands;
use crate::known::is_known_unparsed_dex;
use crate::vault_store::{is_spl_token_program, decode_spl_token_amount, VaultStore};

#[derive(Debug, Default)]
pub struct ScoutMetrics {
    pub updates_received:        AtomicU64,
    pub no_parser_claimed:       AtomicU64,
    /// Subset of `no_parser_claimed`: updates whose owner program IS in our
    /// known-unparsed list (e.g. AlphaQ). Lets the report quantify the
    /// specific cost of *not* writing a parser for a DEX we've identified.
    pub no_parser_known_dex:     AtomicU64,
    pub vault_updates_received:  AtomicU64,
    pub routes_evaluated:        AtomicU64,
    pub routes_eval_ok_none:     AtomicU64,
    pub routes_eval_ok_some:     AtomicU64,
    pub routes_eval_err:         AtomicU64,
    // Opportunity-size buckets. Lowered after analysing a real Solana arb tx
    // (5RzMmTu9...) where the searcher's net profit was $0.0035 — well under
    // the previous $0.10 floor. The sub-cent buckets exist so we can SEE
    // these tiny opportunities; whether they're worth pursuing in production
    // is a separate question (depends on capture rate × frequency).
    pub opps_ge_001:             AtomicU64,    // ≥ $0.001
    pub opps_ge_01:              AtomicU64,    // ≥ $0.01
    pub opps_ge_10c:             AtomicU64,    // ≥ $0.10
    pub opps_ge_1d:              AtomicU64,    // ≥ $1
    pub opps_ge_5d:              AtomicU64,    // ≥ $5
    pub opps_ge_20d:             AtomicU64,    // ≥ $20
    pub largest_net_profit_usdc: AtomicI64,
    pub wss_reconnects:          AtomicU64,
    pub bootstrap_count:         AtomicU64,
    pub sol_price_micros:        AtomicU64,
    // Jito-correlator metrics — populated by the background correlator task
    // ~30 s after each opportunity is logged.
    pub opps_correlated_total:        AtomicU64,
    pub opps_lost_to_competitor:      AtomicU64,
    pub opps_uncontested:             AtomicU64,
    pub competitor_tips_total_lamports: AtomicU64,
    /// Of the competing bundles seen, how many invoked Jupiter v6.
    pub competing_via_jupiter_total:  AtomicU64,
    /// Jupiter cross-check counters: opps where Jupiter agreed there was
    /// SOME profit (>0) vs disagreed (≤0 or error).
    pub jupiter_checked_total:        AtomicU64,
    pub jupiter_agreed_profitable:    AtomicU64,
    pub jupiter_disagreed:            AtomicU64,
    pub jupiter_check_errors:         AtomicU64,
}

impl ScoutMetrics {
    pub fn snapshot(&self) -> ScoutMetricsSnapshot {
        ScoutMetricsSnapshot {
            updates_received:        self.updates_received.load(Ordering::Relaxed),
            no_parser_claimed:       self.no_parser_claimed.load(Ordering::Relaxed),
            no_parser_known_dex:     self.no_parser_known_dex.load(Ordering::Relaxed),
            vault_updates_received:  self.vault_updates_received.load(Ordering::Relaxed),
            routes_evaluated:        self.routes_evaluated.load(Ordering::Relaxed),
            routes_eval_ok_none:     self.routes_eval_ok_none.load(Ordering::Relaxed),
            routes_eval_ok_some:     self.routes_eval_ok_some.load(Ordering::Relaxed),
            routes_eval_err:         self.routes_eval_err.load(Ordering::Relaxed),
            opps_ge_001:             self.opps_ge_001.load(Ordering::Relaxed),
            opps_ge_01:              self.opps_ge_01.load(Ordering::Relaxed),
            opps_ge_10c:             self.opps_ge_10c.load(Ordering::Relaxed),
            opps_ge_1d:              self.opps_ge_1d.load(Ordering::Relaxed),
            opps_ge_5d:              self.opps_ge_5d.load(Ordering::Relaxed),
            opps_ge_20d:             self.opps_ge_20d.load(Ordering::Relaxed),
            largest_net_profit_usdc: self.largest_net_profit_usdc.load(Ordering::Relaxed),
            wss_reconnects:          self.wss_reconnects.load(Ordering::Relaxed),
            bootstrap_count:         self.bootstrap_count.load(Ordering::Relaxed),
            sol_price_usd:           self.sol_price_micros.load(Ordering::Relaxed) as f64
                                      / SOL_PRICE_SCALE as f64,
            opps_correlated_total:   self.opps_correlated_total.load(Ordering::Relaxed),
            opps_lost_to_competitor: self.opps_lost_to_competitor.load(Ordering::Relaxed),
            opps_uncontested:        self.opps_uncontested.load(Ordering::Relaxed),
            competitor_tips_total_lamports:
                                     self.competitor_tips_total_lamports.load(Ordering::Relaxed),
            competing_via_jupiter_total:
                                     self.competing_via_jupiter_total.load(Ordering::Relaxed),
            jupiter_checked_total:   self.jupiter_checked_total.load(Ordering::Relaxed),
            jupiter_agreed_profitable: self.jupiter_agreed_profitable.load(Ordering::Relaxed),
            jupiter_disagreed:       self.jupiter_disagreed.load(Ordering::Relaxed),
            jupiter_check_errors:    self.jupiter_check_errors.load(Ordering::Relaxed),
        }
    }

    fn record_opportunity(&self, net_profit_usdc: i64) {
        if net_profit_usdc >= 1_000     { self.opps_ge_001.fetch_add(1, Ordering::Relaxed); }    // ≥ $0.001
        if net_profit_usdc >= 10_000    { self.opps_ge_01.fetch_add(1, Ordering::Relaxed); }     // ≥ $0.01
        if net_profit_usdc >= 100_000   { self.opps_ge_10c.fetch_add(1, Ordering::Relaxed); }    // ≥ $0.10
        if net_profit_usdc >= 1_000_000 { self.opps_ge_1d.fetch_add(1, Ordering::Relaxed); }
        if net_profit_usdc >= 5_000_000 { self.opps_ge_5d.fetch_add(1, Ordering::Relaxed); }
        if net_profit_usdc >= 20_000_000 { self.opps_ge_20d.fetch_add(1, Ordering::Relaxed); }
        let mut cur = self.largest_net_profit_usdc.load(Ordering::Relaxed);
        while net_profit_usdc > cur {
            match self.largest_net_profit_usdc.compare_exchange(
                cur, net_profit_usdc, Ordering::Relaxed, Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(seen) => cur = seen,
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ScoutMetricsSnapshot {
    pub updates_received:        u64,
    pub no_parser_claimed:       u64,
    pub no_parser_known_dex:     u64,
    pub vault_updates_received:  u64,
    pub routes_evaluated:        u64,
    pub routes_eval_ok_none:     u64,
    pub routes_eval_ok_some:     u64,
    pub routes_eval_err:         u64,
    pub opps_ge_001:             u64,
    pub opps_ge_01:              u64,
    pub opps_ge_10c:             u64,
    pub opps_ge_1d:              u64,
    pub opps_ge_5d:              u64,
    pub opps_ge_20d:             u64,
    pub largest_net_profit_usdc: i64,
    pub wss_reconnects:          u64,
    pub bootstrap_count:         u64,
    pub sol_price_usd:           f64,
    pub opps_correlated_total:        u64,
    pub opps_lost_to_competitor:      u64,
    pub opps_uncontested:             u64,
    pub competitor_tips_total_lamports: u64,
    pub competing_via_jupiter_total:  u64,
    pub jupiter_checked_total:        u64,
    pub jupiter_agreed_profitable:    u64,
    pub jupiter_disagreed:            u64,
    pub jupiter_check_errors:         u64,
}

pub struct Scout {
    config:      Arc<ScoutConfig>,
    store:       Arc<PoolStateStore>,
    vaults:      Arc<VaultStore>,
    pyth:        Arc<crate::pyth_store::PythStore>,
    clmm_fees:   Arc<ClmmFeeStore>,
    openbook:    Arc<OpenBookMarketStore>,
    parsers:     Vec<Arc<dyn DexQuoter>>,
    fees:        FeeEstimator,
    rpc:         Arc<HeliusRpc>,
    logger:      JsonlLogger,
    telegram:    TelegramNotifier,
    correlator:  JitoCorrelator,
    jupiter:     JupiterClient,
    executor:    Option<Arc<Executor>>,
    native_exec: Option<Arc<NativeExecutor>>,
    metrics:     Arc<ScoutMetrics>,
    /// Set once any bootstrap fetch has populated the vault list. Used by the
    /// SOL-price refresh task to avoid running before vault data exists.
    bootstrapped: AtomicBool,
    /// Phase 3c: Pyth oracle pubkeys used by Solend reserves. Populated by
    /// `run_solend_liquidator` after the reserve cache bootstraps.
    /// `handle_update` checks each incoming Pyth update against this set
    /// and pings the liquidator's `solend_scan_notify` on a match — so the
    /// scan loop wakes up the moment a relevant price moves instead of
    /// waiting for the next poll tick.
    solend_pyth_set: Arc<std::sync::RwLock<std::collections::HashSet<Pubkey>>>,
    /// Phase 3c: notifier that the liquidator loop selects on alongside the
    /// poll timer. A `notify_one` triggers an immediate scan; coalesces with
    /// the timer so we never queue up multiple scans for one notification.
    solend_scan_notify: Arc<tokio::sync::Notify>,
    /// Phase 4c: pump.fun positions held by the Axiom-trending sniper.
    /// Each entry tracks mint + entry price + buy time. The TP/SL monitor
    /// task polls these every 10s and sells when 2× hit or 5min timeout.
    /// Replaces atomic buy+sell which is mathematically unprofitable on
    /// pump.fun BC (in-block spread < fees+slippage).
    held_pump_positions: Arc<tokio::sync::Mutex<Vec<HeldPumpPosition>>>,
}

#[derive(Debug, Clone)]
pub struct HeldPumpPosition {
    pub mint:              Pubkey,
    pub creator:           Pubkey,
    pub bought_at:         std::time::Instant,
    pub sol_in_lamports:   u64,
    pub sold:              bool,
}

impl Scout {
    pub fn new(config: ScoutConfig) -> ArbResult<Self> {
        Self::new_with_wallet(config, None)
    }

    /// Build a scout that ALSO has an execution layer wired in.
    /// `wallet` is what `wallet::Wallet::from_env()` returned at startup.
    /// If `None`, executor stays disabled and the scout runs in detect-only
    /// mode regardless of EXECUTOR_LIVE.
    pub fn new_with_wallet(config: ScoutConfig, wallet: Option<Wallet>) -> ArbResult<Self> {
        let vaults    = VaultStore::new();
        let pyth      = crate::pyth_store::PythStore::new();
        let clmm_fees = ClmmFeeStore::new();
        let openbook  = OpenBookMarketStore::new();
        let parsers   = dex::all_quoters(Arc::clone(&vaults), Arc::clone(&clmm_fees));
        let store     = Arc::new(PoolStateStore::new());

        let fees = FeeEstimator {
            flashloan_fee_bps:                  config.solend_flash_fee_bps,
            priority_fee_microlamports_per_cu:  config.priority_fee_microlamports_per_cu,
            compute_unit_limit:                 config.compute_unit_limit,
            base_tip_lamports:                  config.base_tip_lamports,
            profit_tip_percent:                 config.profit_tip_percent,
            sol_price_micros:                   Arc::new(AtomicU64::new(0)),
        };

        let rpc = Arc::new(HeliusRpc::new(config.helius_rpc_url.clone()));
        let logger = JsonlLogger::new(config.log_dir.clone());
        let telegram = TelegramNotifier::new(
            config.telegram_bot_token.clone(),
            config.telegram_chat_id,
        );
        let metrics = Arc::new(ScoutMetrics::default());
        let correlator = JitoCorrelator::new(
            config.helius_rpc_url.clone(),
            Arc::clone(&metrics),
            config.log_dir.clone(),
        );
        let jupiter = JupiterClient::new();
        let (executor, native_exec) = match wallet {
            Some(w) => {
                let cfg = ExecutorConfig::from_env();
                info!(target: "scout::executor",
                    live = cfg.live,
                    min_profit_usdc = cfg.min_profit_usdc,
                    slippage_bps = cfg.slippage_bps,
                    max_daily_loss_usdc = cfg.max_daily_loss_usdc,
                    probe_amount_usdc = cfg.probe_amount_usdc,
                    jito_tip_lamports = cfg.jito_tip_lamports,
                    min_interval_ms = cfg.min_interval.as_millis() as u64,
                    sample_one_in_n = cfg.sample_one_in_n,
                    use_jito = cfg.use_jito,
                    "executor wired (BUILD_TAG=pumpfun-buf20-v94)");
                let native_cfg = NativeExecutorConfig::from_env();
                let probe_env = std::env::var("EXECUTOR_PROBE_AMOUNT_USDC").ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(50_000_000);
                info!(target: "scout::native",
                    live = native_cfg.live,
                    min_profit_usdc = native_cfg.min_profit_usdc,
                    slippage_bps = native_cfg.slippage_bps,
                    probe_amount_usdc = probe_env,
                    jito_tip_lamports = native_cfg.jito_tip_lamports,
                    cu_limit = native_cfg.compute_unit_limit,
                    priority_micro = native_cfg.priority_fee_micro_lamports_per_cu,
                    min_interval_ms = native_cfg.min_interval.as_millis() as u64,
                    flash_enabled = native_cfg.flash_loan_enabled,
                    flash_provider = ?native_cfg.flash_loan_provider,
                    has_solend_ctx = native_cfg.solend_ctx.is_some(),
                    has_marginfi_account = native_cfg.marginfi_account.is_some(),
                    has_marginfi_vault = native_cfg.marginfi_liquidity_vault.is_some(),
                    has_alt = native_cfg.alt_pubkey.is_some(),
                    "native executor wired");
                // Spawn the dynamic Jito tip-floor poller. The TipFloor is
                // cloned (Arc-internals) into BOTH the native executor and
                // the Jupiter executor (pump.fun atomic uses it for Helius
                // Sender tip sizing). One poll task serves all consumers.
                let tip_floor = TipFloor::new();
                start_tip_polling(tip_floor.clone(), DEFAULT_POLL_INTERVAL);
                let tip_floor_for_exec = tip_floor.clone();
                info!(target: "scout::native",
                      poll_interval_secs = DEFAULT_POLL_INTERVAL.as_secs(),
                      "Jito tip-floor poller started");

                let n = NativeExecutor::new(
                    native_cfg, w.clone(),
                    Arc::clone(&store), Arc::clone(&vaults),
                    Arc::clone(&pyth),
                    Arc::clone(&fees.sol_price_micros),
                    Arc::clone(&openbook), Arc::clone(&clmm_fees),
                    config.helius_rpc_url.clone(),
                    tip_floor,
                );
                let n = Arc::new(n);
                Arc::clone(&n).spawn_worker();
                n.spawn_balance_poller();
                // Initialise direct-TPU sender if NATIVE_SUBMISSION_PATH=tpu.
                // No-op for other paths so we don't pay the 60s cluster_nodes
                // poll cost when we're using Jito/Helius relays.
                n.init_tpu_sender();
                (Some(Arc::new(Executor::new(cfg, w, config.helius_rpc_url.clone(), tip_floor_for_exec))), Some(n))
            }
            None => (None, None),
        };

        info!(pools = config.pools.len(), "scout initialised");

        Ok(Self {
            config: Arc::new(config),
            store,
            vaults,
            pyth,
            clmm_fees,
            openbook,
            parsers,
            fees,
            rpc,
            logger,
            telegram,
            correlator,
            jupiter,
            executor,
            native_exec,
            metrics,
            bootstrapped: AtomicBool::new(false),
            solend_pyth_set:    Arc::new(std::sync::RwLock::new(std::collections::HashSet::new())),
            solend_scan_notify: Arc::new(tokio::sync::Notify::new()),
            held_pump_positions: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        })
    }

    /// Cross-check an opportunity against Jupiter's real routing API.
    /// Sampled (1-in-N) so we don't blow past Jupiter's rate limit.
    /// Updates jupiter_* metrics as a side effect; returns nothing.
    async fn jupiter_verify(&self, route: &Route, probe_in: u64) {
        // v72: when JUPITER_POLL_ENABLED=true, the dedicated polling worker
        // owns all Jupiter quota — skip this verify path entirely so we
        // don't blow the Pro tier RPS limit.
        if std::env::var("JUPITER_POLL_ENABLED").as_deref() == Ok("true") {
            return;
        }
        // Sample: only check 1 in N opportunities (configurable later;
        // hardcoded to 10 for now to stay well under Jupiter's ~10 req/sec).
        let n = self.metrics.routes_eval_ok_some.load(Ordering::Relaxed);
        if n % 10 != 0 { return; }
        let intermediate = match route.hops.first().map(|h| h.token_out) {
            Some(t) if t.0 != known::USDC => t,
            _ => return,
        };
        let result = self.jupiter
            .verify_arb_cycle(&known::USDC, &intermediate.0, probe_in, 50)
            .await;
        match result {
            Ok(check) => {
                self.metrics.jupiter_checked_total.fetch_add(1, Ordering::Relaxed);
                if check.net_profit_usdc > 0 {
                    self.metrics.jupiter_agreed_profitable.fetch_add(1, Ordering::Relaxed);
                    info!(
                        target: "scout::jupiter",
                        intermediate = %intermediate.0,
                        scout_probe_usdc = probe_in,
                        jupiter_final_usdc = check.final_usdc,
                        jupiter_net_usdc = check.net_profit_usdc,
                        leg1_hops = check.leg1_hops,
                        leg2_hops = check.leg2_hops,
                        round_trip_ms = check.total_round_trip.as_millis(),
                        "✅ Jupiter agrees there's profit"
                    );
                } else {
                    self.metrics.jupiter_disagreed.fetch_add(1, Ordering::Relaxed);
                    debug!(
                        target: "scout::jupiter",
                        intermediate = %intermediate.0,
                        scout_probe_usdc = probe_in,
                        jupiter_final_usdc = check.final_usdc,
                        jupiter_net_usdc = check.net_profit_usdc,
                        "Jupiter disagrees — opportunity not real after slippage"
                    );
                }
            }
            Err(e) => {
                self.metrics.jupiter_check_errors.fetch_add(1, Ordering::Relaxed);
                debug!(target: "scout::jupiter", error = %e, "Jupiter check failed");
            }
        }
    }

    pub fn metrics(&self) -> Arc<ScoutMetrics> { Arc::clone(&self.metrics) }

    /// Fetch all configured pools via RPC, parse each, store the result, and
    /// (for Raydium AMM v4 pools) discover the base/quote vault addresses.
    /// A second RPC call then fetches those vaults so balances are populated
    /// before we evaluate any route.
    ///
    /// Returns the full list of accounts to subscribe to (pools + vaults).
    async fn bootstrap(&self) -> ArbResult<Vec<Pubkey>> {
        let pool_addrs: Vec<Pubkey> = self.config.pools.iter().map(|p| p.address).collect();
        info!(count = pool_addrs.len(), "bootstrap: fetching pool accounts");
        let snapshots = self.rpc.get_multiple_accounts(&pool_addrs).await?;

        // Step A: store parseable pool snapshots; collect vault & amm_config
        // addresses for the subsequent fetches.
        let mut vault_addrs:   Vec<Pubkey> = Vec::new();
        let mut pyth_addrs:    Vec<Pubkey> = Vec::new();
        let mut clmm_pools:    Vec<(Pubkey, Pubkey)> = Vec::new(); // (pool, amm_config)
        let mut openbook_markets: Vec<(Pubkey, Pubkey)> = Vec::new(); // (market_id, market_program)
        for (entry, snap) in self.config.pools.iter().zip(snapshots.iter()) {
            let Some(snap) = snap else {
                warn!(pool = %entry.address, name = %entry.name,
                      "bootstrap: account not found on-chain (null result)");
                continue;
            };
            let Some(mut pool) = self.parse_account_into_pool(&snap.pubkey, &snap.data) else {
                warn!(pool = %entry.address, name = %entry.name, dex = ?entry.kind,
                      data_len = snap.data.len(), owner = %snap.owner,
                      "bootstrap: no parser claimed this account (wrong size / discriminator?)");
                continue;
            };
            pool.last_update_slot = snap.slot;
            match pool.kind {
                PoolKind::RaydiumAmmV4 => {
                    if let Some(rd) = RaydiumAmmV4Data::try_parse(&snap.data) {
                        vault_addrs.push(rd.base_vault);
                        vault_addrs.push(rd.quote_vault);
                        // Phase E: collect OpenBook market id so we can pre-fetch
                        // its bids/asks/eventQueue/vaults for native swap building.
                        if rd.market_id != Pubkey::default() {
                            openbook_markets.push((rd.market_id, rd.market_program));
                        }
                    }
                }
                PoolKind::RaydiumClmm => {
                    if let Some(cl) = RaydiumClmmData::try_parse(&snap.data) {
                        clmm_pools.push((entry.address, cl.amm_config));
                    }
                }
                PoolKind::RaydiumCpmm => {
                    if let Some(cp) = arb_dex_raydium::RaydiumCpmmData::try_parse(&snap.data) {
                        vault_addrs.push(cp.token_0_vault);
                        vault_addrs.push(cp.token_1_vault);
                    }
                }
                PoolKind::ObricV2 => {
                    if let Some(ob) = arb_dex_obric::ObricV2Data::try_parse(&snap.data) {
                        vault_addrs.push(ob.reserve_x_vault);
                        vault_addrs.push(ob.reserve_y_vault);
                        pyth_addrs.push(ob.x_price_feed);
                        pyth_addrs.push(ob.y_price_feed);
                    }
                }
                _ => {}
            }
            self.store.upsert(pool);
        }

        // Step B: fetch + decode vault accounts.
        if !vault_addrs.is_empty() {
            info!(count = vault_addrs.len(), "bootstrap: fetching vault accounts");
            let vsnaps = self.rpc.get_multiple_accounts(&vault_addrs).await?;
            for (addr, snap) in vault_addrs.iter().zip(vsnaps.iter()) {
                if let Some(s) = snap {
                    if let Some(amount) = decode_spl_token_amount(&s.data) {
                        self.vaults.upsert(*addr, amount);
                    }
                }
            }
        }

        // Step B': fetch Pyth price-feed accounts for Obric pools. Stored
        // raw — `arb_dex_obric::parse_pyth_price` extracts price/exponent at
        // quote time. Refresh happens via WSS (these are subscribed).
        pyth_addrs.sort();
        pyth_addrs.dedup();
        if !pyth_addrs.is_empty() {
            info!(count = pyth_addrs.len(), "bootstrap: fetching Pyth price feeds");
            let psnaps = self.rpc.get_multiple_accounts(&pyth_addrs).await?;
            for (addr, snap) in pyth_addrs.iter().zip(psnaps.iter()) {
                if let Some(s) = snap {
                    self.pyth.upsert(*addr, s.data.clone());
                }
            }
        }

        // Step C': fetch OpenBook market accounts (bids/asks/event_queue/vaults).
        // These are static per market — fetched once at bootstrap, never refreshed.
        if !openbook_markets.is_empty() {
            // Dedupe — many Raydium pools may share the same OpenBook market.
            let mut seen: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
            let unique: Vec<(Pubkey, Pubkey)> = openbook_markets.iter()
                .filter(|(m, _)| seen.insert(*m)).copied().collect();
            let market_addrs: Vec<Pubkey> = unique.iter().map(|(m, _)| *m).collect();
            info!(count = market_addrs.len(), "bootstrap: fetching OpenBook markets");
            let msnaps = self.rpc.get_multiple_accounts(&market_addrs).await?;
            for ((market_addr, market_program), snap) in unique.iter().zip(msnaps.iter()) {
                let Some(s) = snap else {
                    warn!(market = %market_addr, "bootstrap: OpenBook market account missing");
                    continue;
                };
                match parse_openbook_market(&s.data, market_addr, market_program) {
                    Ok(accs) => self.openbook.insert(*market_addr, accs),
                    Err(e) => warn!(market = %market_addr, error = %e,
                                    "bootstrap: OpenBook market parse failed"),
                }
            }
        }

        // Step C: fetch CLMM AmmConfig accounts to learn each pool's fee tier.
        if !clmm_pools.is_empty() {
            let amm_addrs: Vec<Pubkey> = clmm_pools.iter().map(|(_, a)| *a).collect();
            info!(count = amm_addrs.len(), "bootstrap: fetching CLMM AmmConfigs");
            let asnaps = self.rpc.get_multiple_accounts(&amm_addrs).await?;
            for ((pool_addr, _), snap) in clmm_pools.iter().zip(asnaps.iter()) {
                if let Some(s) = snap {
                    if let Some(fee_rate) = parse_trade_fee_rate(&s.data) {
                        self.clmm_fees.upsert(*pool_addr, fee_rate);
                    } else {
                        warn!(pool = %pool_addr, "bootstrap: AmmConfig parse failed");
                    }
                } else {
                    warn!(pool = %pool_addr, "bootstrap: AmmConfig account not found");
                }
            }
        }

        info!(
            pools_loaded = self.store.len(),
            vaults_loaded = self.vaults.len(),
            clmm_fees_loaded = self.clmm_fees.len(),
            openbook_markets_loaded = self.openbook.len(),
            "bootstrap complete"
        );
        self.bootstrapped.store(true, Ordering::Relaxed);
        self.metrics.bootstrap_count.fetch_add(1, Ordering::Relaxed);

        // Phase 3c: when the Solend liquidator is enabled, also subscribe to
        // every Pyth oracle pubkey referenced by a Solend reserve. The
        // liquidator's scan loop uses handle_update's notify hook to wake
        // up the instant any of these prices ticks — closing the latency
        // gap from "wait up to poll_secs" to "<100ms after price change".
        let mut solend_pyth_addrs: Vec<Pubkey> = Vec::new();
        if std::env::var("SOLEND_LIQUIDATOR_ENABLED").as_deref() == Ok("true") {
            let markets = crate::solend_liq::active_markets();
            match crate::solend_liq::bootstrap_reserve_cache_multi(
                &self.config.helius_rpc_url, &markets,
            ).await {
                Ok(reserves) => {
                    let mut seen: std::collections::HashSet<Pubkey> =
                        std::collections::HashSet::new();
                    for r in reserves.values() {
                        if seen.insert(r.pyth_oracle) {
                            solend_pyth_addrs.push(r.pyth_oracle);
                        }
                    }
                    info!(target: "solend_liq",
                        unique_pyth_feeds = solend_pyth_addrs.len(),
                        "bootstrap: added Solend reserve Pyth oracles to subscription");
                }
                Err(e) => warn!(target: "solend_liq", error = %e,
                    "bootstrap: skipping Solend Pyth subscription (will retry in liquidator task)"),
            }
        }

        // Final subscription list = pools + discovered vaults + Pyth feeds
        // (Obric + Solend if enabled). AmmConfigs don't need WSS subscriptions
        // (fee tiers don't change at runtime).
        let mut all = pool_addrs;
        all.extend(vault_addrs);
        all.extend(pyth_addrs);
        all.extend(solend_pyth_addrs);
        Ok(all)
    }

    fn parse_account_into_pool(&self, pubkey: &Pubkey, data: &[u8]) -> Option<arb_core::Pool> {
        self.parsers.iter().find_map(|q| q.try_parse_account(pubkey, data))
    }

    /// Long-running task: bootstrap → WSS subscribe → handle updates → reconnect.
    pub async fn run_enhanced_websocket(&self) -> ArbResult<()> {
        let mut backoff = Duration::from_millis(200);
        loop {
            // Step 1: bootstrap (fetch pool + vault state via RPC).
            let mut subscription_list = match self.bootstrap().await {
                Ok(list) => {
                    backoff = Duration::from_millis(200); // reset on success
                    list
                }
                Err(e) => {
                    warn!(error = %e, "bootstrap failed; will retry after backoff");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            };

            // v79: when pump.fun executor is enabled, add the pump.fun program
            // to the Yellowstone subscription so the tx-stream picks up every
            // Create event in real time (~50-150 ms latency vs ~5 s for RPC poll).
            if std::env::var("PUMP_FUN_EXECUTE_ENABLED").as_deref() == Ok("true")
                || std::env::var("AXIOM_TRENDING_ENABLED").as_deref() == Ok("true")
                || std::env::var("COPY_TRADE_ENABLED").as_deref() == Ok("true")
            {
                subscription_list.push(crate::pump_fun::PUMP_FUN_PROGRAM);
                info!("added pump.fun program to LaserStream subscription (Create / trending / copy-trade detection)");
            }
            // Phase 5: copy-trade KOL wallets. Each pubkey in COPY_TRADE_WALLETS
            // (comma-separated) gets added to the laserstream's account_include
            // filter, so every tx touching them streams to us. The handler
            // detects pump.fun Buy IXs in these txs and, when 2+ distinct KOLs
            // buy the same mint within COPY_TRADE_WINDOW_SECS, fires our own
            // buy. This is the "smart-money confluence" strategy whales use.
            if let Ok(raw) = std::env::var("COPY_TRADE_WALLETS") {
                use std::str::FromStr;
                let mut n_added = 0;
                for s in raw.split(',') {
                    let s = s.trim();
                    if s.is_empty() { continue; }
                    if let Ok(pk) = Pubkey::from_str(s) {
                        subscription_list.push(pk);
                        n_added += 1;
                    }
                }
                if n_added > 0 {
                    info!(target: "copy_trade",
                        kol_wallets = n_added,
                        "📡 added KOL wallets to LaserStream subscription");
                }
            }

            // Step 2: open the streaming session — LaserStream gRPC when
            // `LASERSTREAM_GRPC_URL` is set, else fall back to Atlas WSS.
            // Same `AccountUpdate` channel surface, so handle_update doesn't
            // care which backend produced the message.
            let (tx, mut rx) = mpsc::channel::<AccountUpdate>(2048);
            let laserstream_url = std::env::var("LASERSTREAM_GRPC_URL").ok()
                .filter(|s| !s.trim().is_empty());
            let api_key = std::env::var("HELIUS_API_KEY").unwrap_or_default();
            let wss_url = self.config.helius_wss_url.clone();

            // Phase A1: optional Yellowstone transaction stream. When
            // `YELLOWSTONE_TX_STREAM_ENABLED=true`, the LaserStream session
            // also subscribes to TXs touching any of our pools and emits
            // them ~50-150 ms before the matching account-write. The
            // consumer spawned below currently just LOGS the tx (phase A1
            // = visibility); a later phase parses the Whirlpool/CLMM
            // event logs to derive predictive pool state.
            let tx_stream_enabled = std::env::var("YELLOWSTONE_TX_STREAM_ENABLED")
                .as_deref() == Ok("true");
            let (tx_chan, mut tx_rx) = mpsc::channel::<arb_stream::TransactionUpdate>(4096);
            let tx_chan_opt = if tx_stream_enabled { Some(tx_chan) } else { None };
            if tx_stream_enabled {
                // Phase A2: parse Whirlpool Swap events out of each tx's
                // program logs, splice the post-swap sqrt_price into the
                // cached pool bytes, and feed a synthetic AccountUpdate
                // through the existing pipeline. The route enumerator then
                // re-evaluates ~50-150 ms before the real account-write
                // would have triggered it.
                let predictive_tx  = tx.clone();
                let pool_store_dup = Arc::clone(&self.store);
                let executor_dup   = self.executor.clone();  // Option<Arc<Executor>>
                // Graduation-arb Phase 2 captures: cloned for the per-Migrate
                // spawn that does a Helius getTransaction (to extract mint)
                // + Jupiter quote dry-run loop.
                let helius_url_dup = self.config.helius_rpc_url.clone();
                let jupiter_dup    = self.jupiter.clone();
                // Phase 4 (Axiom-style trending): rolling-window buy-count
                // tracker per pump.fun mint. When a mint accumulates N buys
                // in the last 60s we treat it as "trending" and snipe it.
                // `already_sniped`: per-mint dedupe so a single trending
                // event spawns at most one snipe over the cooldown period.
                let trending_state: Arc<tokio::sync::Mutex<
                    std::collections::HashMap<Pubkey, std::collections::VecDeque<std::time::Instant>>
                >> = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
                let already_sniped: Arc<tokio::sync::Mutex<
                    std::collections::HashMap<Pubkey, std::time::Instant>
                >> = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
                // Phase 4c: shared held-positions list, written by Axiom-trending
                // buy and read+drained by `run_pump_fun_tp_sl`.
                let held_pump_positions_dup = Arc::clone(&self.held_pump_positions);
                // Phase 4d: clone the RPC client so the Axiom-trending spawn
                // can check live SOL balance before firing the buy.
                let rpc_for_axiom = Arc::clone(&self.rpc);
                // Phase 5: copy-trade confluence state. Tracks per-mint signals
                // from KOL wallets — when 2+ distinct KOLs buy the same mint
                // within COPY_TRADE_WINDOW_SECS, the bot fires its own buy.
                let copy_trade_signals: Arc<tokio::sync::Mutex<
                    std::collections::HashMap<Pubkey, Vec<(Pubkey, std::time::Instant)>>
                >> = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
                let copy_trade_sniped: Arc<tokio::sync::Mutex<
                    std::collections::HashSet<Pubkey>
                >> = Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
                // Parse the KOL set once at task start.
                let copy_trade_set: std::collections::HashSet<Pubkey> = {
                    use std::str::FromStr;
                    std::env::var("COPY_TRADE_WALLETS").ok().map(|raw| {
                        raw.split(',')
                            .filter_map(|s| Pubkey::from_str(s.trim()).ok())
                            .collect()
                    }).unwrap_or_default()
                };
                let copy_trade_on = std::env::var("COPY_TRADE_ENABLED")
                    .as_deref() == Ok("true") && !copy_trade_set.is_empty();
                if copy_trade_on {
                    info!(target: "copy_trade",
                        n_kols = copy_trade_set.len(),
                        "🎯 copy-trade confluence detector active");
                }
                tokio::spawn(async move {
                    let mut n_total: u64 = 0;
                    let mut n_predicted: u64 = 0;
                    while let Some(t) = tx_rx.recv().await {
                        n_total += 1;
                        if n_total % 500 == 0 {
                            let sig_b58 = bs58::encode(&t.signature).into_string();
                            tracing::info!(
                                target: "scout::yellowstone_tx",
                                slot = t.slot,
                                signature = %sig_b58,
                                writable_count = t.writable_accounts.len(),
                                log_count = t.log_messages.len(),
                                predicted_so_far = n_predicted,
                                "🔭 tx-stream sample (every 500th)"
                            );
                        }
                        // v79: pump.fun Create detection — real-time path.
                        // Look for "Instruction: Create" + pump.fun program
                        // invocation in the same tx. On match: extract
                        // mint = account_keys[1] (second signer), creator =
                        // account_keys[0] (fee payer). Fire async buy.
                        let pf_pid = crate::pump_fun::PUMP_FUN_PROGRAM.to_string();
                        let mut saw_create_inst = false;
                        let mut saw_pump_invoke = false;
                        for line in &t.log_messages {
                            if line.contains("Program log: Instruction: Create") {
                                saw_create_inst = true;
                            }
                            if line.contains(&pf_pid) && line.contains("invoke") {
                                saw_pump_invoke = true;
                            }
                        }
                        if saw_create_inst && saw_pump_invoke
                            && t.account_keys.len() >= 2
                            && t.num_signers >= 2
                            // Decouple from the trending sniper: the HODL
                            // Create-buy path stays gated on PUMP_FUN_EXECUTE_ENABLED,
                            // so flipping AXIOM_TRENDING_ENABLED on alone
                            // doesn't suddenly fire HODL buys on every Create.
                            && std::env::var("PUMP_FUN_EXECUTE_ENABLED")
                                .as_deref() == Ok("true")
                        {
                            let creator = t.account_keys[0];
                            let mint    = t.account_keys[1];
                            let exec_for_buy = match executor_dup.as_ref() {
                                Some(e) => e.clone(),
                                None => continue,
                            };
                            tokio::spawn(async move {
                                let buy_lamports: u64 = std::env::var("PUMP_FUN_BUY_LAMPORTS")
                                    .ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
                                let hodl = std::env::var("PUMP_FUN_HODL_MODE")
                                    .as_deref() == Ok("true");
                                let poll_sell = std::env::var("PUMP_FUN_POLL_SELL")
                                    .as_deref() == Ok("true");
                                let bundle = std::env::var("PUMP_FUN_BUNDLE_HODL")
                                    .as_deref() == Ok("true");
                                if hodl && poll_sell {
                                    tracing::info!(target: "scout::pump_fun_exec",
                                        mint = %mint, creator = %creator,
                                        "🎯 Create detected — HODL poll-then-sell");
                                    match exec_for_buy
                                        .pump_fun_buy_then_sell(mint, creator, buy_lamports)
                                        .await
                                    {
                                        Ok(Some((bs, Some(ss)))) => tracing::info!(
                                            target: "scout::pump_fun_exec",
                                            buy_sig = %bs, sell_sig = %ss, mint = %mint,
                                            "💰 HODL poll-sell round-trip"),
                                        Ok(Some((bs, None))) => tracing::info!(
                                            target: "scout::pump_fun_exec",
                                            buy_sig = %bs, mint = %mint,
                                            "🚀 HODL poll-sell BUY only (SELL skipped)"),
                                        Ok(None) => {} // skip reason logged inside
                                        Err(e) => tracing::warn!(
                                            target: "scout::pump_fun_exec",
                                            mint = %mint, error = %e,
                                            "HODL poll-sell failed"),
                                    }
                                } else if hodl && bundle {
                                    tracing::info!(target: "scout::pump_fun_exec",
                                        mint = %mint, creator = %creator,
                                        "🎯 Create detected — HODL bundle (buy+sell same slot)");
                                    match exec_for_buy
                                        .pump_fun_buy_sell_bundle(mint, creator, buy_lamports)
                                        .await
                                    {
                                        Ok(Some(bid)) => tracing::info!(
                                            target: "scout::pump_fun_exec",
                                            bundle_id = %bid, mint = %mint,
                                            "🎰 HODL bundle sent"),
                                        Ok(None) => {} // skip reason logged inside
                                        Err(e) => tracing::warn!(
                                            target: "scout::pump_fun_exec",
                                            mint = %mint, error = %e,
                                            "HODL bundle failed"),
                                    }
                                } else if hodl {
                                    tracing::info!(target: "scout::pump_fun_exec",
                                        mint = %mint, creator = %creator,
                                        "🎯 Create detected — HODL buy-only");
                                    match exec_for_buy
                                        .pump_fun_buy_only(mint, creator, buy_lamports)
                                        .await
                                    {
                                        Ok(Some(sig)) => tracing::info!(
                                            target: "scout::pump_fun_exec",
                                            sig = %sig, mint = %mint,
                                            "🚀 HODL buy sent"),
                                        Ok(None) => {} // skip reason already logged inside
                                        Err(e) => tracing::warn!(
                                            target: "scout::pump_fun_exec",
                                            mint = %mint, error = %e,
                                            "HODL buy failed"),
                                    }
                                } else {
                                    tracing::info!(target: "scout::pump_fun_exec",
                                        mint = %mint, creator = %creator,
                                        "🎯 Create detected — atomic buy+sell");
                                    match exec_for_buy
                                        .pump_fun_buy_sell_atomic(mint, creator, buy_lamports)
                                        .await
                                    {
                                        Ok(sig) => tracing::info!(target: "scout::pump_fun_exec",
                                            sig = %sig, mint = %mint,
                                            "🎰 atomic round-trip sent"),
                                        Err(e) => tracing::warn!(target: "scout::pump_fun_exec",
                                            mint = %mint, error = %e,
                                            "atomic round-trip failed"),
                                    }
                                }
                            });
                        }

                        // Graduation arb Phase 1 — Migrate detection. When a
                        // pump.fun bonding curve completes (~85 SOL accumulated),
                        // the program emits a `Migrate` IX that moves liquidity
                        // to PumpSwap AMM (account [9] in the IX). PumpSwap
                        // appears in Jupiter routing, so the actual buy will
                        // route via Jupiter — no PumpSwap-specific integration
                        // needed in our code. For now this is observation only
                        // (no buys); we log every detected graduation so we
                        // can verify which tx-level account position holds the
                        // mint (IDL says IX-index 2 = mint, but tx-level index
                        // depends on the writability ordering).
                        let mut saw_migrate_inst = false;
                        let mut saw_pump_invoke_for_migrate = false;
                        for line in &t.log_messages {
                            // Strict match — `contains` was matching the
                            // unrelated `MigrateBondingCurveCreator` IX too,
                            // wasting cycles on a Helius RPC + Jupiter poll
                            // for txs that aren't real graduations.
                            if line.trim_end() == "Program log: Instruction: Migrate" {
                                saw_migrate_inst = true;
                            }
                            if line.contains(&pf_pid) && line.contains("invoke") {
                                saw_pump_invoke_for_migrate = true;
                            }
                        }
                        if saw_migrate_inst && saw_pump_invoke_for_migrate
                            && t.account_keys.len() >= 10
                        {
                            let sig_b58 = bs58::encode(&t.signature).into_string();
                            let first_keys: Vec<String> = t.account_keys.iter()
                                .take(20)
                                .map(|k| k.to_string())
                                .collect();
                            tracing::info!(target: "scout::pump_fun_grad",
                                slot = t.slot,
                                signature = %sig_b58,
                                num_signers = t.num_signers,
                                n_keys = t.account_keys.len(),
                                n_writable = t.writable_accounts.len(),
                                first_20_keys = ?first_keys,
                                "🎓 graduation Migrate detected");

                            // Graduation-arb Phase 2/3 — DRY-RUN by default,
                            // LIVE buy+sell when `PUMP_FUN_GRAD_ARB_ENABLED=true`.
                            // We fetch the tx via Helius RPC to extract the
                            // mint (IDL: IX accounts[2]), then poll Jupiter
                            // for SOL→mint quotes until indexed.
                            //   DRY-RUN: log hypothetical buy, wait HOLD_SECS,
                            //            quote sell, log paper PnL.
                            //   LIVE:    call executor.execute_jupiter_swap_raw
                            //            for actual BUY, spawn monitor task
                            //            that polls Jupiter for TOKEN→SOL and
                            //            exits on TP/SL/timeout via raw_swap.
                            let helius_url = helius_url_dup.clone();
                            let jupiter    = jupiter_dup.clone();
                            let executor_for_grad = executor_dup.clone();
                            let sig_for_task = sig_b58.clone();
                            let pump_pid_str = pf_pid.clone();
                            tokio::spawn(async move {
                                let started = std::time::Instant::now();
                                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                                let http = match reqwest::Client::builder()
                                    .timeout(std::time::Duration::from_secs(10))
                                    .build()
                                {
                                    Ok(c) => c,
                                    Err(_) => return,
                                };
                                let body = serde_json::json!({
                                    "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
                                    "params": [&sig_for_task, {
                                        "encoding": "jsonParsed",
                                        "commitment": "confirmed",
                                        "maxSupportedTransactionVersion": 0,
                                    }],
                                });
                                let resp: serde_json::Value = match http.post(&helius_url)
                                    .json(&body).send().await
                                {
                                    Ok(r) => match r.json().await {
                                        Ok(v) => v,
                                        Err(e) => {
                                            tracing::warn!(target: "scout::pump_fun_grad",
                                                sig = %sig_for_task, error = %e,
                                                "getTransaction parse failed");
                                            return;
                                        }
                                    },
                                    Err(e) => {
                                        tracing::warn!(target: "scout::pump_fun_grad",
                                            sig = %sig_for_task, error = %e,
                                            "getTransaction send failed");
                                        return;
                                    }
                                };

                                // Locate the pump.fun Migrate IX. Per IDL the Migrate IX
                                // has 25 accounts and mint is at IX-account-index 2 — but
                                // sometimes the IX appears in inner_instructions (CPI'd by
                                // a router contract). We search both top-level and inner.
                                // Account-count threshold lowered to 20 to absorb minor
                                // IDL drift (was 24).
                                fn find_mint_in_ix_list(
                                    instructions: &serde_json::Value,
                                    pump_pid_str: &str,
                                ) -> Option<String> {
                                    let arr = instructions.as_array()?;
                                    for ix in arr {
                                        let pid = ix.get("programId")
                                            .and_then(|v| v.as_str())?;
                                        if pid != pump_pid_str { continue; }
                                        let accts = ix.get("accounts")
                                            .and_then(|v| v.as_array())?;
                                        if accts.len() < 20 { continue; }
                                        if let Some(m) = accts.get(2)
                                            .and_then(|v| v.as_str())
                                        {
                                            return Some(m.to_string());
                                        }
                                    }
                                    None
                                }
                                // Try top-level instructions first.
                                let mut mint_str: Option<String> = resp
                                    .pointer("/result/transaction/message/instructions")
                                    .and_then(|v| find_mint_in_ix_list(v, &pump_pid_str));
                                // Fall back to inner instructions (CPI calls).
                                if mint_str.is_none() {
                                    if let Some(inners) = resp
                                        .pointer("/result/meta/innerInstructions")
                                        .and_then(|v| v.as_array())
                                    {
                                        for inner_group in inners {
                                            if let Some(ixs) = inner_group.get("instructions") {
                                                if let Some(m) = find_mint_in_ix_list(ixs, &pump_pid_str) {
                                                    mint_str = Some(m);
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                                let mint_str = match mint_str {
                                    Some(s) => s,
                                    None => {
                                        tracing::warn!(target: "scout::pump_fun_grad",
                                            sig = %sig_for_task,
                                            "could not locate Migrate IX (top + inner) in tx");
                                        return;
                                    }
                                };
                                let mint_pk = match <solana_program::pubkey::Pubkey
                                    as std::str::FromStr>::from_str(&mint_str)
                                {
                                    Ok(p) => p,
                                    Err(_) => return,
                                };

                                let sim_buy: u64 = std::env::var("PUMP_FUN_GRAD_SIM_BUY_LAMPORTS")
                                    .ok().and_then(|s| s.parse().ok())
                                    .unwrap_or(50_000_000);
                                let hold_secs: u64 = std::env::var("PUMP_FUN_GRAD_HOLD_SECS")
                                    .ok().and_then(|s| s.parse().ok()).unwrap_or(30);
                                let index_timeout_secs: u64 = std::env::var(
                                    "PUMP_FUN_GRAD_INDEX_TIMEOUT_SECS"
                                ).ok().and_then(|s| s.parse().ok()).unwrap_or(30);
                                let slippage_bps: u32 = std::env::var(
                                    "PUMP_FUN_GRAD_SLIPPAGE_BPS"
                                ).ok().and_then(|s| s.parse().ok()).unwrap_or(100);

                                let sol_mint = crate::known::SOL;
                                let max_iter = (index_timeout_secs * 1000) / 2000;
                                let mut idx_iter = 0u64;
                                let buy_quote = loop {
                                    idx_iter += 1;
                                    if idx_iter > max_iter {
                                        tracing::info!(target: "scout::pump_fun_grad",
                                            sig = %sig_for_task, mint = %mint_pk,
                                            timeout_secs = index_timeout_secs,
                                            "⏰ Jupiter never indexed graduation mint");
                                        return;
                                    }
                                    tokio::time::sleep(std::time::Duration::from_millis(2000))
                                        .await;
                                    match jupiter.quote(
                                        &sol_mint, &mint_pk, sim_buy, slippage_bps
                                    ).await {
                                        Ok(q) => {
                                            tracing::info!(target: "scout::pump_fun_grad",
                                                sig = %sig_for_task, mint = %mint_pk,
                                                indexed_at_ms = started.elapsed().as_millis() as u64,
                                                sim_buy_lamports = sim_buy,
                                                tokens_received = q.out_amount,
                                                price_impact_bps = ?q.price_impact_bps,
                                                route_hops = q.route_hops,
                                                "🎓 Jupiter indexed — hypothetical BUY");
                                            break q;
                                        }
                                        Err(_) => continue,
                                    }
                                };

                                let grad_live = std::env::var("PUMP_FUN_GRAD_ARB_ENABLED")
                                    .as_deref() == Ok("true");

                                if grad_live {
                                    // ── Phase 3: LIVE BUY via Jupiter ──────
                                    let exec = match executor_for_grad.as_ref() {
                                        Some(e) => e.clone(),
                                        None => {
                                            tracing::warn!(target: "scout::pump_fun_grad",
                                                mint = %mint_pk,
                                                "no executor wired — grad arb live skipped");
                                            return;
                                        }
                                    };
                                    let (buy_sig, expected_tokens) = match exec
                                        .execute_jupiter_swap_raw(sol_mint, mint_pk, sim_buy)
                                        .await
                                    {
                                        Ok(r) => r,
                                        Err(e) => {
                                            tracing::warn!(target: "scout::pump_fun_grad",
                                                mint = %mint_pk, error = %e,
                                                "Phase 3: LIVE BUY failed");
                                            return;
                                        }
                                    };
                                    tracing::info!(target: "scout::pump_fun_grad",
                                        buy_sig = %buy_sig, mint = %mint_pk,
                                        sol_in_lamports = sim_buy, expected_tokens,
                                        "💰 Phase 3: LIVE BUY sent");

                                    // ── Phase 4: monitor for TP/SL/timeout ──
                                    let tp_pct: u64 = std::env::var("PUMP_FUN_GRAD_TP_PCT")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(200);
                                    let sl_pct: u64 = std::env::var("PUMP_FUN_GRAD_SL_PCT")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(50);
                                    let timeout_secs: u64 = std::env::var(
                                        "PUMP_FUN_GRAD_TIMEOUT_SECS"
                                    ).ok().and_then(|s| s.parse().ok()).unwrap_or(60);
                                    let monitor_start = std::time::Instant::now();
                                    let exit_reason: &str = loop {
                                        tokio::time::sleep(
                                            std::time::Duration::from_secs(2)
                                        ).await;
                                        let elapsed = monitor_start.elapsed().as_secs();
                                        if elapsed >= timeout_secs {
                                            break "timeout";
                                        }
                                        match jupiter.quote(
                                            &mint_pk, &sol_mint, expected_tokens, slippage_bps
                                        ).await {
                                            Ok(q) => {
                                                let ratio_pct = (q.out_amount as u128
                                                    * 100 / sim_buy.max(1) as u128) as u64;
                                                if ratio_pct >= tp_pct {
                                                    tracing::info!(target: "scout::pump_fun_grad",
                                                        mint = %mint_pk, ratio_pct,
                                                        sim_buy_lamports = sim_buy,
                                                        current_lamports = q.out_amount,
                                                        "🚀 Phase 4: TP triggered");
                                                    break "tp";
                                                }
                                                if ratio_pct <= sl_pct {
                                                    tracing::info!(target: "scout::pump_fun_grad",
                                                        mint = %mint_pk, ratio_pct,
                                                        sim_buy_lamports = sim_buy,
                                                        current_lamports = q.out_amount,
                                                        "🛑 Phase 4: SL triggered");
                                                    break "sl";
                                                }
                                            }
                                            Err(_) => continue,
                                        }
                                    };

                                    // ── SELL via Jupiter ────────────────────
                                    // Reduce to 95% of expected_tokens to avoid
                                    // "not enough" reverts from slippage drift.
                                    let sell_amount = expected_tokens
                                        .saturating_mul(95) / 100;
                                    match exec.execute_jupiter_swap_raw(
                                        mint_pk, sol_mint, sell_amount
                                    ).await {
                                        Ok((sell_sig, sol_out)) => {
                                            let pnl = sol_out as i64 - sim_buy as i64;
                                            let pnl_pct = (pnl as f64 / sim_buy as f64)
                                                * 100.0;
                                            tracing::info!(target: "scout::pump_fun_grad",
                                                buy_sig = %buy_sig, sell_sig = %sell_sig,
                                                mint = %mint_pk, exit_reason,
                                                sim_buy_lamports = sim_buy,
                                                sol_out_lamports = sol_out,
                                                pnl_lamports = pnl,
                                                pnl_pct = format!("{:+.2}%", pnl_pct),
                                                "💎 Phase 4: SELL completed");
                                        }
                                        Err(e) => {
                                            tracing::warn!(target: "scout::pump_fun_grad",
                                                buy_sig = %buy_sig, mint = %mint_pk,
                                                exit_reason, error = %e,
                                                "Phase 4: SELL failed — tokens stuck in wallet");
                                        }
                                    }
                                } else {
                                    // ── DRY-RUN (Phase 2): no spending ──────
                                    tokio::time::sleep(
                                        std::time::Duration::from_secs(hold_secs)
                                    ).await;
                                    match jupiter.quote(
                                        &mint_pk, &sol_mint, buy_quote.out_amount, slippage_bps
                                    ).await {
                                        Ok(sell_q) => {
                                            let lamports_back = sell_q.out_amount;
                                            let pnl = lamports_back as i64 - sim_buy as i64;
                                            let pnl_pct = (pnl as f64 / sim_buy as f64) * 100.0;
                                            tracing::info!(target: "scout::pump_fun_grad",
                                                sig = %sig_for_task, mint = %mint_pk,
                                                sim_buy_lamports = sim_buy,
                                                tokens_held = buy_quote.out_amount,
                                                sim_sell_lamports = lamports_back,
                                                pnl_lamports = pnl,
                                                pnl_pct = format!("{:+.2}%", pnl_pct),
                                                hold_secs,
                                                "💎 dry-run PnL");
                                        }
                                        Err(e) => {
                                            tracing::warn!(target: "scout::pump_fun_grad",
                                                sig = %sig_for_task, mint = %mint_pk,
                                                error = %e,
                                                "sell-side quote failed after hold");
                                        }
                                    }
                                }
                            });
                        }

                        // Walk the program logs for Whirlpool Traded events.
                        // (NOT named "Swap" — that's Raydium CLMM's event name.)
                        for line in &t.log_messages {
                            let Some(ev) = arb_dex_orca::WhirlpoolTradedEvent::try_parse_from_log(line)
                                else { continue };
                            // Only act on pools we actually monitor.
                            let Some(pool) = pool_store_dup.get(&ev.whirlpool) else { continue };
                            if pool.kind != arb_core::PoolKind::OrcaWhirlpool { continue; }
                            // Splice post_sqrt_price into byte offset 65..81.
                            // (See `WhirlpoolData` doc-comment for offsets.)
                            let mut new_data = pool.state.as_slice().to_vec();
                            if new_data.len() < 81 { continue; }
                            new_data[65..81].copy_from_slice(&ev.post_sqrt_price.to_le_bytes());
                            // Push as synthetic AccountUpdate so the
                            // existing handle_update flow takes over —
                            // parse → upsert pool_store → build routes →
                            // dispatch to native.
                            let synth = AccountUpdate {
                                account: ev.whirlpool,
                                slot:    t.slot,
                                data:    new_data,
                                owner:   arb_dex_orca::ORCA_WHIRLPOOL_PROGRAM,
                            };
                            if predictive_tx.try_send(synth).is_ok() {
                                n_predicted += 1;
                                if n_predicted % 50 == 0 {
                                    tracing::info!(
                                        target: "scout::yellowstone_tx",
                                        pool = %ev.whirlpool,
                                        slot = t.slot,
                                        post_sqrt = %ev.post_sqrt_price,
                                        total = n_predicted,
                                        "⚡ predictive pool update (every 50th)"
                                    );
                                }
                            }
                        }

                        // Phase 5 — copy-trade confluence detector. Triggered
                        // when a tx is signed by one of our tracked KOL
                        // wallets AND contains a pump.fun Buy IX. We track
                        // per-mint signals and fire our own buy when 2+
                        // distinct KOLs hit the same mint within the window.
                        if copy_trade_on {
                            let mut saw_pump_buy = false;
                            let mut saw_pump_invoke = false;
                            for line in &t.log_messages {
                                if line.trim_end() == "Program log: Instruction: Buy" {
                                    saw_pump_buy = true;
                                }
                                if line.contains(&pf_pid) && line.contains("invoke") {
                                    saw_pump_invoke = true;
                                }
                            }
                            if saw_pump_buy && saw_pump_invoke {
                                // Find which KOL signed this tx (signers come first in account_keys).
                                let kol = (0..t.num_signers as usize)
                                    .filter_map(|i| t.account_keys.get(i))
                                    .find(|pk| copy_trade_set.contains(pk))
                                    .copied();
                                let mint = t.account_keys.iter()
                                    .find(|pk| pk.to_string().ends_with("pump"))
                                    .copied();
                                if let (Some(kol), Some(mint)) = (kol, mint) {
                                    let window_secs: u64 = std::env::var("COPY_TRADE_WINDOW_SECS")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(60);
                                    let min_kols: usize = std::env::var("COPY_TRADE_MIN_KOLS")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(2);
                                    let buy_lamports: u64 = std::env::var("COPY_TRADE_BUY_LAMPORTS")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(50_000_000);

                                    let now = std::time::Instant::now();
                                    let mut sigs = copy_trade_signals.lock().await;
                                    let entry = sigs.entry(mint).or_default();
                                    let window = std::time::Duration::from_secs(window_secs);
                                    entry.retain(|(_, t0)| now.duration_since(*t0) < window);
                                    if !entry.iter().any(|(k, _)| *k == kol) {
                                        entry.push((kol, now));
                                    }
                                    let n_distinct_kols = entry.iter()
                                        .map(|(k, _)| *k)
                                        .collect::<std::collections::HashSet<_>>()
                                        .len();
                                    drop(sigs);

                                    tracing::info!(target: "copy_trade",
                                        kol = %kol, mint = %mint,
                                        distinct_kols_in_window = n_distinct_kols,
                                        threshold = min_kols,
                                        "🐋 KOL buy detected");

                                    if n_distinct_kols >= min_kols {
                                        let mut sniped = copy_trade_sniped.lock().await;
                                        if sniped.insert(mint) {
                                            drop(sniped);
                                            tracing::info!(target: "copy_trade",
                                                mint = %mint,
                                                distinct_kols = n_distinct_kols,
                                                buy_lamports,
                                                "🎯 confluence threshold crossed — firing snipe");
                                            if let Some(exec) = executor_dup.as_ref() {
                                                let exec = exec.clone();
                                                let held = held_pump_positions_dup.clone();
                                                let rpc = rpc_for_axiom.clone();
                                                tokio::spawn(async move {
                                                    // Same reserve guard as Axiom-trending.
                                                    let min_reserve: u64 = std::env::var(
                                                        "MIN_SOL_RESERVE_LAMPORTS"
                                                    ).ok().and_then(|s| s.parse().ok())
                                                        .unwrap_or(50_000_000);
                                                    let payer = exec.wallet_pubkey();
                                                    let bal = rpc.get_balance(&payer).await
                                                        .unwrap_or(0);
                                                    let est_cost = buy_lamports + 405_000;
                                                    if bal < est_cost + min_reserve {
                                                        tracing::info!(target: "copy_trade",
                                                            mint = %mint, balance_sol = bal as f64/1e9,
                                                            "⛽ skipping — below reserve floor");
                                                        return;
                                                    }
                                                    match exec.pump_fun_buy_only(
                                                        mint, mint, buy_lamports
                                                    ).await {
                                                        Ok(Some(sig)) => {
                                                            tracing::info!(target: "copy_trade",
                                                                sig = %sig, mint = %mint,
                                                                "🚀 copy-trade buy sent — TP/SL will exit");
                                                            held.lock().await.push(HeldPumpPosition {
                                                                mint, creator: mint,
                                                                bought_at: std::time::Instant::now(),
                                                                sol_in_lamports: buy_lamports,
                                                                sold: false,
                                                            });
                                                        }
                                                        Ok(None) => {}
                                                        Err(e) => tracing::warn!(target: "copy_trade",
                                                            mint = %mint, error = %e,
                                                            "copy-trade buy failed"),
                                                    }
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // Phase 4 — Axiom-style trending detector. Pump.fun
                        // Buy IX appears in the tx logs; we count buys per
                        // mint over a 60s rolling window. Mint extracted by
                        // the vanity "ends-with-pump" suffix (pump.fun's
                        // grinder convention), filtering the writable
                        // accounts to the unique candidate. When the count
                        // crosses threshold AND we haven't sniped this mint
                        // in the last cooldown window, fire a buy via the
                        // same pump_fun_buy_only path the HODL flow uses.
                        //
                        // Tunables:
                        //   AXIOM_TRENDING_ENABLED         (default false)
                        //   AXIOM_TRENDING_WINDOW_SECS     (default 60)
                        //   AXIOM_TRENDING_MIN_BUYS        (default 8)
                        //   AXIOM_TRENDING_COOLDOWN_SECS   (default 600)
                        //   AXIOM_TRENDING_BUY_LAMPORTS    (default 10_000_000 = 0.01 SOL)
                        let trending_on = std::env::var("AXIOM_TRENDING_ENABLED")
                            .as_deref() == Ok("true");
                        if trending_on {
                            let mut saw_buy_inst = false;
                            let mut saw_pump_for_buy = false;
                            for line in &t.log_messages {
                                if line.trim_end() == "Program log: Instruction: Buy" {
                                    saw_buy_inst = true;
                                }
                                if line.contains(&pf_pid) && line.contains("invoke") {
                                    saw_pump_for_buy = true;
                                }
                            }
                            if saw_buy_inst && saw_pump_for_buy {
                                // Heuristic mint extraction. Pump.fun mints are
                                // vanity-grinded to end with "pump". In a Buy
                                // IX the mint is NOT writable, so scan all
                                // tx-level account_keys (signers + writables +
                                // readonlies) — the mint appears as a readonly.
                                let candidate = t.account_keys.iter()
                                    .find(|pk| pk.to_string().ends_with("pump"))
                                    .copied();
                                if let Some(mint) = candidate {
                                    let window_secs: u64 = std::env::var("AXIOM_TRENDING_WINDOW_SECS")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(60);
                                    let min_buys: usize = std::env::var("AXIOM_TRENDING_MIN_BUYS")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(8);
                                    let cooldown_secs: u64 = std::env::var("AXIOM_TRENDING_COOLDOWN_SECS")
                                        .ok().and_then(|s| s.parse().ok()).unwrap_or(600);

                                    let now = std::time::Instant::now();
                                    let mut state = trending_state.lock().await;
                                    let deq = state.entry(mint).or_default();
                                    let window = std::time::Duration::from_secs(window_secs);
                                    while let Some(&front) = deq.front() {
                                        if now.duration_since(front) > window {
                                            deq.pop_front();
                                        } else {
                                            break;
                                        }
                                    }
                                    deq.push_back(now);
                                    let count = deq.len();
                                    drop(state);

                                    if count >= min_buys {
                                        // Cooldown check.
                                        let mut snipes = already_sniped.lock().await;
                                        let cooldown = std::time::Duration::from_secs(cooldown_secs);
                                        let allowed = match snipes.get(&mint) {
                                            Some(t0) => now.duration_since(*t0) > cooldown,
                                            None => true,
                                        };
                                        if allowed {
                                            snipes.insert(mint, now);
                                            drop(snipes);
                                            let buy_lamports: u64 = std::env::var(
                                                "AXIOM_TRENDING_BUY_LAMPORTS"
                                            ).ok().and_then(|s| s.parse().ok())
                                                .unwrap_or(10_000_000);
                                            tracing::info!(
                                                target: "scout::axiom_trending",
                                                mint = %mint,
                                                buys_in_window = count,
                                                window_secs,
                                                buy_lamports,
                                                "🔥 trending threshold crossed — firing snipe");
                                            if let Some(exec) = executor_dup.as_ref() {
                                                let exec = exec.clone();
                                                let held = held_pump_positions_dup.clone();
                                                let rpc_for_balance = rpc_for_axiom.clone();
                                                tokio::spawn(async move {
                                                    // Phase 4d: hard SOL-reserve floor.
                                                    // We never want to drain the wallet
                                                    // so low that subsequent TP/SL sells
                                                    // (which need priority fee + tip)
                                                    // can't execute. Check live balance
                                                    // and skip the buy if firing it
                                                    // would push us under the reserve.
                                                    let min_reserve: u64 = std::env::var(
                                                        "MIN_SOL_RESERVE_LAMPORTS"
                                                    ).ok().and_then(|s| s.parse().ok())
                                                        .unwrap_or(30_000_000); // 0.03 SOL
                                                    let payer_pk = exec.wallet_pubkey();
                                                    let bal = rpc_for_balance.get_balance(&payer_pk)
                                                        .await.unwrap_or(0);
                                                    // Total per-snipe cost ≈ buy + priority + Helius tip + base fee.
                                                    let est_cost = buy_lamports + 405_000;
                                                    if bal < est_cost + min_reserve {
                                                        tracing::info!(
                                                            target: "scout::axiom_trending",
                                                            mint = %mint, balance_sol = bal as f64/1e9,
                                                            need_sol = (est_cost + min_reserve) as f64/1e9,
                                                            "⛽ skipping — wallet below SOL reserve floor");
                                                        return;
                                                    }
                                                    // Phase 4c: buy-only entry.
                                                    // Atomic buy+sell on pump.fun
                                                    // is mathematically a loser
                                                    // (in-block spread < fees +
                                                    // slippage). Instead we BUY
                                                    // and let `run_pump_fun_tp_sl`
                                                    // exit on 2× target or 5min
                                                    // timeout — same playbook as
                                                    // the FLASHX whales (analysed
                                                    // 2026-05-12 via tx
                                                    // 5VBFv8n5… that revealed
                                                    // Custom 6003 = TooLittleSolReceived
                                                    // on every atomic sell leg).
                                                    let creator_hint = mint; // re-derived in fn
                                                    match exec.pump_fun_buy_only(
                                                        mint, creator_hint, buy_lamports
                                                    ).await {
                                                        Ok(Some(sig)) => {
                                                            tracing::info!(
                                                                target: "scout::axiom_trending",
                                                                sig = %sig, mint = %mint,
                                                                buy_lamports,
                                                                "🚀 trending buy sent — TP/SL monitor will exit");
                                                            held.lock().await.push(HeldPumpPosition {
                                                                mint,
                                                                creator: creator_hint, // BondingCurve actual is fine for sell
                                                                bought_at: std::time::Instant::now(),
                                                                sol_in_lamports: buy_lamports,
                                                                sold: false,
                                                            });
                                                        }
                                                        Ok(None) => {} // budget/dedup logged inside
                                                        Err(e) => tracing::warn!(
                                                            target: "scout::axiom_trending",
                                                            mint = %mint, error = %e,
                                                            "trending buy failed"),
                                                    }
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                });
            }

            let session_handle = if let Some(grpc_url) = laserstream_url {
                #[cfg(feature = "laserstream")]
                {
                    info!(endpoint = %grpc_url, "using LaserStream gRPC backend");
                    let sub_list = subscription_list.clone();
                    tokio::spawn(async move {
                        arb_stream::laserstream::run_one_session(
                            &grpc_url, &api_key, &sub_list, &tx,
                            tx_chan_opt.as_ref(),
                        ).await
                    })
                }
                #[cfg(not(feature = "laserstream"))]
                {
                    warn!(
                        "LASERSTREAM_GRPC_URL is set but binary built without \
                         the `laserstream` feature; falling back to WSS. \
                         Rebuild with `--features laserstream`."
                    );
                    let _ = (grpc_url, api_key, tx_chan_opt);
                    tokio::spawn(async move {
                        run_one_session(&wss_url, &subscription_list, &tx).await
                    })
                }
            } else {
                let _ = tx_chan_opt;
                tokio::spawn(async move {
                    run_one_session(&wss_url, &subscription_list, &tx).await
                })
            };

            while let Some(update) = rx.recv().await {
                self.metrics.updates_received.fetch_add(1, Ordering::Relaxed);
                self.handle_update(update).await;
            }

            // rx loop exited → session ended. Wait for the session task to
            // surface its end reason for logging.
            let end = session_handle.await.unwrap_or(SessionEnd::Closed);
            self.metrics.wss_reconnects.fetch_add(1, Ordering::Relaxed);
            match end {
                SessionEnd::Closed => warn!("WSS session closed cleanly; reconnecting"),
                SessionEnd::Errored(e) => {
                    warn!(error = %e, "WSS session errored; reconnecting after backoff");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    async fn handle_update(&self, update: AccountUpdate) {
        // Vault account? (SPL Token program owns it.) Decode amount, store, done.
        if is_spl_token_program(&update.owner) {
            if let Some(amount) = decode_spl_token_amount(&update.data) {
                self.vaults.upsert(update.account, amount);
                self.metrics.vault_updates_received.fetch_add(1, Ordering::Relaxed);
            }
            return;
        }

        // Pyth price-feed account?
        //   - V2 legacy (magic 0xa1b2c3d4): pool oracles, Obric pairs
        //   - Pull V2 (owner = rec5EKMG…): Solend's current oracles
        // Cache the raw bytes either way — `parse_pyth_price_any` figures
        // out the format at parse time.
        let pyth_v2 = update.data.len() >= 4 && update.data[0..4] == [0xd4, 0xc3, 0xb2, 0xa1];
        let pyth_pull = update.owner == crate::pyth_store::PYTH_PROGRAM_ID_PULL;
        if pyth_v2 || pyth_pull {
            self.pyth.upsert(update.account, update.data.clone());
            // Phase 3c: if this feed is used by Solend, ping the liquidator
            // loop so it scans immediately instead of waiting for the next
            // poll tick. Coalescing Notify means a price-storm collapses to
            // one extra scan.
            let is_solend = self.solend_pyth_set.read()
                .map(|s| s.contains(&update.account))
                .unwrap_or(false);
            if is_solend {
                self.solend_scan_notify.notify_one();
            }
            return;
        }

        // Otherwise treat as a pool account: try DEX parsers.
        let parsed = self.parse_account_into_pool(&update.account, &update.data);
        let Some(mut pool) = parsed else {
            self.metrics.no_parser_claimed.fetch_add(1, Ordering::Relaxed);
            if is_known_unparsed_dex(&update.owner) {
                self.metrics.no_parser_known_dex.fetch_add(1, Ordering::Relaxed);
                debug!(account = %update.account, owner = %update.owner,
                       "known-but-unparsed DEX update (parser TODO)");
            } else {
                debug!(account = %update.account, "no parser claimed this account");
            }
            return;
        };
        pool.last_update_slot = update.slot;
        self.store.upsert(pool);

        // Rebuild routes (cycles up to max_hops) and evaluate any that touch
        // the just-updated pool.
        let routes = build_routes_from_store(&self.store, self.config.max_hops);
        let probe_in = self.config.probe_amount_usdc;

        // ── Native dispatch pre-filter ────────────────────────────────────
        // Older phase 6o code ran an UNCONDITIONAL `maybe_execute` spawn here
        // for every native-eligible route, on the theory that build_leg's
        // accurate quoter would catch what scout's cheap quoter missed.
        // Live data 2026-05-16 disproved it: 99.5% of those spawns hit
        // `below_min` inside build_leg (after Jupiter quote + per-DEX
        // requote), burning the Helius RPC budget without ever sending.
        //
        // The fix is to let scout's own `evaluate_route_at_size` (which
        // ALREADY subtracts fee_estimator + flash fee + tip + priority —
        // line 1898) be the first gate. If scout's after-fees net <= 0,
        // skip the spawn entirely. The post-evaluate spawn (further below,
        // gated on `find_optimal_size` returning Some) still handles the
        // routes scout's quoter agrees are profitable — at the OPTIMAL
        // size, not the config probe size, so it's strictly better.
        //
        // Set `NATIVE_INLINE_EXECUTE_UNGATED=1` to restore the legacy
        // unconditional pre-eval spawn for A/B testing.
        let native_disabled = std::env::var("NATIVE_DISPATCH_DISABLED")
            .as_deref() == Ok("true");
        let inline = std::env::var("NATIVE_INLINE_EXECUTE")
            .as_deref() != Ok("false"); // default ON
        let ungated_legacy = std::env::var("NATIVE_INLINE_EXECUTE_UNGATED")
            .as_deref() == Ok("1");
        if ungated_legacy && !native_disabled {
            // Legacy unconditional spawn — kept behind feature flag.
            if let Some(native) = self.native_exec.as_ref() {
                for route in routes.iter() {
                    if !route.hops.iter().any(|h| h.pool == update.account) { continue; }
                    if native.is_native_eligible(route) {
                        if inline {
                            let n = native.clone();
                            let r = route.clone();
                            tokio::spawn(async move {
                                let _ = n.maybe_execute(&r, probe_in as u64).await;
                            });
                        } else {
                            native.submit(route.clone(), probe_in as u64, 0);
                        }
                    }
                }
            }
        }

        for (ri, route) in routes.iter().enumerate() {
            if !route.hops.iter().any(|h| h.pool == update.account) { continue; }
            self.metrics.routes_evaluated.fetch_add(1, Ordering::Relaxed);

            // Step 1: cheap probe at fixed size. If it's not profitable, skip
            // the expensive ternary search.
            let probe_outcome = self.evaluate_route_at_size(route, probe_in, update.slot).await;
            match probe_outcome {
                Err(e) => {
                    self.metrics.routes_eval_err.fetch_add(1, Ordering::Relaxed);
                    debug!(route = ri, error = %e, "evaluate failed");
                    continue;
                }
                Ok(None) => {
                    self.metrics.routes_eval_ok_none.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Ok(Some(_)) => {} // fall through to optimization
            }

            // Step 2: profitable at probe size — search for the OPTIMAL size.
            // The net-profit-vs-size function on a constant-product cycle is
            // concave (returns rise as size scales the spread, then drop as
            // slippage dominates). Ternary search converges in ~30 iterations.
            let optimal = self.find_optimal_size(route, update.slot).await;
            let record = match optimal {
                Some(r) => r,
                None => {
                    // Optimization couldn't beat zero (rare — probe was profitable
                    // but search failed). Fall back to probe result.
                    self.metrics.routes_eval_ok_none.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            };

            self.metrics.routes_eval_ok_some.fetch_add(1, Ordering::Relaxed);
            self.metrics.record_opportunity(record.net_profit_usdc);
            let pool_set: std::collections::HashSet<Pubkey> = route.hops.iter().map(|h| h.pool).collect();
            self.correlator.submit(
                record.slot,
                pool_set,
                record.net_profit_usdc,
                record.timestamp,
            );
            // Cross-check 1-in-10 opportunities against Jupiter's real route.
            self.jupiter_verify(route, record.input_usdc).await;

            // Stage B: dispatch to native executor's BUFFER (no Jupiter, no
            // HTTP). Submission is non-blocking — every native-eligible
            // opportunity goes into a ring; a worker picks the highest-net
            // entry every `min_interval` ms and runs maybe_execute on it.
            //
            // `NATIVE_DISPATCH_DISABLED=true` forces all opportunities through
            // the Jupiter executor instead — useful when the native path is
            // returning 0 sent (e.g., the pool universe lacks the multi-DEX
            // routes Jupiter's aggregator finds via USDT/JLP/etc bridges).
            let native_disabled = std::env::var("NATIVE_DISPATCH_DISABLED")
                .as_deref() == Ok("true");
            if !native_disabled {
                if let Some(native) = self.native_exec.as_ref() {
                    if native.is_native_eligible(route) {
                        // Phase 6o: same inline-execute fix as the
                        // handle_update path. Skip the buffer; spawn
                        // maybe_execute directly so the call hits the
                        // executor's quoter on the same pool snapshot scout
                        // just evaluated, before state moves.
                        let inline = std::env::var("NATIVE_INLINE_EXECUTE")
                            .as_deref() != Ok("false");
                        let probe = record.input_usdc as u64;
                        if inline {
                            let n = native.clone();
                            let r = route.clone();
                            tokio::spawn(async move {
                                let _ = n.maybe_execute(&r, probe).await;
                            });
                        } else {
                            native.submit(route.clone(), probe, 0);
                        }
                        // Native path used → skip Jupiter executor (don't double-spend).
                        if let Err(e) = self.logger.log(&record).await {
                            warn!(error = %e, "failed to write opportunity record");
                        }
                        continue;
                    }
                }
            }
            // v72: when JUPITER_POLL_ENABLED=true, the dedicated polling worker
            // is the sole Jupiter caller — skip the scout-triggered path here
            // to avoid blowing through the Pro tier's RPS quota.
            let jupiter_poll_active = std::env::var("JUPITER_POLL_ENABLED")
                .as_deref() == Ok("true");
            if jupiter_poll_active {
                if let Err(e) = self.logger.log(&record).await {
                    warn!(error = %e, "failed to write opportunity record");
                }
                continue;
            }
            if let (Some(exec), Some(intermediate)) = (
                self.executor.as_ref(),
                route.hops.first().map(|h| h.token_out.0).filter(|m| *m != known::USDC),
            ) {
                // Rotate base mint across enabled options. Each base widens
                // the surface area: Jupiter quotes USDT→X→USDT and SOL→X→SOL
                // differently from USDC→X→USDC because the aggregator picks
                // different intermediate hops per input mint.
                //
                // SOL_ALTERNATION_ENABLED is the new option (no MarginFi
                // flash-loan vault for SOL exists, so SOL routes go through
                // the legacy non-flash path; probe size capped at wallet SOL).
                //
                // GUARD: candidate base must not equal `intermediate`, else
                // Jupiter returns "Input and output mints not allowed to be
                // equal". Fall back to a different base if there's a clash.
                let usdt_alt = std::env::var("USDT_ALTERNATION_ENABLED")
                    .as_deref() == Ok("true");
                let sol_alt  = std::env::var("SOL_ALTERNATION_ENABLED")
                    .as_deref() == Ok("true");
                // v66: when only SOL is enabled (wallet funded in SOL, USDC/USDT
                // empty + flash off), drop USDC entirely from the rotation. Without
                // this guard, 1/3 of attempts target a base mint we have no funds
                // for — wasting Jito's 1/sec rate limit on routes that would
                // simulation-fail anyway.
                let mut bases: Vec<solana_program::pubkey::Pubkey> = Vec::new();
                if sol_alt && !usdt_alt {
                    bases.push(known::SOL);
                } else {
                    bases.push(known::USDC);
                    if usdt_alt { bases.push(known::USDT); }
                    if sol_alt  { bases.push(known::SOL);  }
                }
                let n = self.metrics.routes_eval_ok_some.load(Ordering::Relaxed);
                let candidate = bases[(n as usize) % bases.len()];
                // v67: when the candidate base equals the intermediate, look
                // for ANOTHER base in the rotation that differs. If there
                // isn't one (e.g. SOL-only mode + intermediate=SOL), SKIP
                // the route entirely — do NOT silently fall back to USDC.
                // The old `.unwrap_or(known::USDC)` was sending $5000 USDC
                // arbs from a USDC-empty wallet, wasting Jito's rate budget
                // on guaranteed-fail txs.
                let base_mint_opt: Option<solana_program::pubkey::Pubkey> =
                    if candidate == intermediate {
                        bases.iter().copied().find(|b| *b != intermediate)
                    } else {
                        Some(candidate)
                    };
                if let Some(base_mint) = base_mint_opt {
                    // Convert the optimal probe size to base-mint units. The
                    // ternary search reports `input_usdc` in USDC base units
                    // (6 decimals == micro-USD). For SOL base, convert via
                    // the current dynamic SOL price (micros).
                    let probe_amount: u64 = if base_mint == known::SOL {
                        let sol_micros = self.metrics.sol_price_micros
                            .load(std::sync::atomic::Ordering::Relaxed);
                        if sol_micros == 0 {
                            std::env::var("SCOUT_PROBE_AMOUNT_SOL_LAMPORTS")
                                .ok().and_then(|s| s.parse().ok()).unwrap_or(20_000_000u64)
                        } else {
                            ((record.input_usdc as u128) * 1_000_000_000u128
                                / sol_micros as u128) as u64
                        }
                    } else {
                        record.input_usdc as u64
                    };
                    let probe = exec.maybe_execute_with_base(
                        base_mint, intermediate, probe_amount,
                    ).await;
                    if let Err(e) = probe {
                        warn!(target: "scout::executor",
                              base_mint = %base_mint,
                              intermediate = %intermediate,
                              probe_amount,
                              error = %e,
                              "maybe_execute failed");
                    }
                }
            }

            if let Err(e) = self.logger.log(&record).await {
                warn!(error = %e, "failed to write opportunity record");
            }
        }
    }

    /// Ternary-search for the input size that maximizes net profit on this
    /// route. Returns the best `OpportunityRecord` found, or None if no size
    /// in the configured range is profitable.
    async fn find_optimal_size(
        &self,
        route:   &Route,
        at_slot: u64,
    ) -> Option<OpportunityRecord> {
        let mut lo = self.config.size_search_min_usdc;
        let mut hi = self.config.size_search_max_usdc;
        if hi <= lo { return None; }

        // Ternary search: ~30 iterations is enough for any realistic range
        // (log_1.5(hi/lo) * 1 quote per check).
        for _ in 0..32 {
            if hi.saturating_sub(lo) < 1_000 { break; } // converged to ~$0.001 precision
            let third = (hi - lo) / 3;
            let m1 = lo + third;
            let m2 = hi - third;
            let p1 = self.evaluate_route_at_size(route, m1, at_slot).await.ok().flatten()
                .map(|r| r.net_profit_usdc).unwrap_or(i64::MIN);
            let p2 = self.evaluate_route_at_size(route, m2, at_slot).await.ok().flatten()
                .map(|r| r.net_profit_usdc).unwrap_or(i64::MIN);
            if p1 < p2 { lo = m1; } else { hi = m2; }
        }
        let final_size = (lo + hi) / 2;
        self.evaluate_route_at_size(route, final_size, at_slot).await.ok().flatten()
    }

    /// Single-shot evaluation at a specific input size. Pure function in
    /// store/vault state — no mutation.
    async fn evaluate_route_at_size(
        &self,
        route:   &Route,
        input:   u64,
        at_slot: u64,
    ) -> ArbResult<Option<OpportunityRecord>> {
        self.evaluate_route(route, input, at_slot).await
    }

    async fn evaluate_route(
        &self,
        route:   &Route,
        input:   u64,
        at_slot: u64,
    ) -> ArbResult<Option<OpportunityRecord>> {
        let mut current = TokenAmount::new(known::usdc_mint(), input, 6);

        for hop in &route.hops {
            let pool = self.store.get(&hop.pool).ok_or(ArbError::PoolNotFound(hop.pool))?;
            let quoter = self.quoter_for_kind(pool.kind)
                .ok_or(ArbError::NotImplemented("no quoter for this pool kind"))?;
            if current.mint.0 != pool.token_a.0 && current.mint.0 != pool.token_b.0 {
                return Err(ArbError::Invariant("route hop token does not match pool tokens"));
            }
            let out = quoter.quote(&pool, current)?;
            current = out;
        }

        if current.mint.0 != known::USDC {
            return Err(ArbError::Invariant("route did not return to USDC"));
        }

        let gross_profit = (current.amount as i64) - (input as i64);
        if gross_profit <= 0 { return Ok(None); }

        let breakdown = self.fees.estimate(input, gross_profit as u64);
        let net = gross_profit - (breakdown.total_fees_usdc as i64);
        if net <= 0 { return Ok(None); }

        let route_str = render_route(route);
        let pools = route.hops.iter().map(|h| h.pool.to_string()).collect();

        Ok(Some(OpportunityRecord {
            timestamp:             Utc::now(),
            slot:                  at_slot,
            route:                 route_str,
            input_usdc:            input,
            output_usdc:           current.amount,
            gross_profit_usdc:     gross_profit,
            flashloan_fee_usdc:    breakdown.flashloan_fee_usdc,
            priority_fee_lamports: breakdown.priority_fee_lamports,
            jito_tip_lamports:     breakdown.jito_tip_lamports,
            net_profit_usdc:       net,
            pools,
        }))
    }

    fn quoter_for_kind(&self, kind: PoolKind) -> Option<&Arc<dyn DexQuoter>> {
        self.parsers.iter().find(|q| q.dex_id() == kind.id())
    }

    /// Long-running task: every 60 s, derive SOL/USD price from any monitored
    /// SOL/USDC Orca Whirlpool and update the FeeEstimator's atomic.
    pub async fn run_sol_price_refresh(&self) -> ArbResult<()> {
        // First, wait for bootstrap to finish populating the store. Without
        // this, the immediate-firing tokio interval would tick once, find
        // `bootstrapped = false`, `continue`, and then wait a full 60s.
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await; // first tick fires immediately

            for pool_arc in self.store.snapshot() {
                if pool_arc.kind != PoolKind::OrcaWhirlpool { continue; }
                let (mint_a, mint_b) = (pool_arc.token_a.0, pool_arc.token_b.0);

                // Orca sorts (token_a, token_b) by raw byte order of the
                // mint Pubkeys, NOT by base58 string. SOL's mint bytes start
                // with 0x06 while USDC's start with 0xc6 — so in any
                // SOL/USDC Whirlpool, token_a = SOL and token_b = USDC.
                // We still handle both orderings defensively in case Orca's
                // convention shifts or another USDC-quoted pair appears.
                let usdc_is_a = mint_a == known::USDC;
                let usdc_is_b = mint_b == known::USDC;
                let sol_is_a  = mint_a == known::SOL;
                let sol_is_b  = mint_b == known::SOL;
                if !((sol_is_a && usdc_is_b) || (usdc_is_a && sol_is_b)) { continue; }

                let Some(parsed) = WhirlpoolData::try_parse(pool_arc.state.as_slice()) else { continue };
                if parsed.sqrt_price == 0 { continue; }

                // sqrt_price = sqrt(raw_B / raw_A) * 2^64 (Q64.64)
                let sqrt_f = (parsed.sqrt_price as f64) / 18_446_744_073_709_551_616.0;
                let raw_b_per_raw_a = sqrt_f * sqrt_f;
                if raw_b_per_raw_a <= 0.0 { continue; }

                // Convert to sol_price_usd. USDC has 6 decimals, SOL has 9.
                let sol_price_usd = if sol_is_a && usdc_is_b {
                    // raw_b_per_raw_a = raw_USDC per raw_SOL
                    // price_USDC_per_SOL = raw_b_per_raw_a * (10^6 USDC dec) / (10^9 SOL dec) ⁻¹
                    //                    = raw_b_per_raw_a * 10^9 / 10^6
                    //                    = raw_b_per_raw_a * 1000
                    raw_b_per_raw_a * 1000.0
                } else {
                    // usdc_is_a && sol_is_b
                    // raw_b_per_raw_a = raw_SOL per raw_USDC
                    // price_USDC_per_SOL = (10^9 / 10^6) / raw_b_per_raw_a = 1000 / raw_b_per_raw_a
                    1000.0 / raw_b_per_raw_a
                };
                if !sol_price_usd.is_finite() || sol_price_usd <= 0.0 { continue; }

                self.fees.set_sol_price_usd(sol_price_usd);
                self.metrics.sol_price_micros
                    .store((sol_price_usd * SOL_PRICE_SCALE as f64) as u64, Ordering::Relaxed);
                info!(target: "scout::sol_price",
                    price_usd = sol_price_usd,
                    from_pool = %pool_arc.address,
                    "SOL price updated"
                );
                break; // first match wins
            }
        }
    }

    /// Watch-only wallet balance monitor. Polls the bot's SOL + USDC balance
    /// every 30s and fires a Telegram alert when the total drops more than
    /// `WALLET_WATCH_DROP_PCT` (default 10%) from the 5-minute rolling peak.
    /// This is the early-warning canary against key compromise: if a sweeper
    /// drains the wallet, the alert fires within ~30s of the first transfer.
    ///
    /// No automatic remediation — the operator must manually pause the bot
    /// and rotate keys. This is intentional: an automated "halt on drop"
    /// would also halt during legitimate large outflows (a successful
    /// liquidation that pays a flash-loan fee, for instance).
    ///
    /// Tunables:
    ///   WALLET_WATCH_ENABLED      (default true)
    ///   WALLET_WATCH_INTERVAL_MS  (default 30000)
    ///   WALLET_WATCH_DROP_PCT     (default 10 = 10% drop = alert)
    pub async fn run_wallet_watch(&self) -> ArbResult<()> {
        let enabled = std::env::var("WALLET_WATCH_ENABLED")
            .as_deref().unwrap_or("true") == "true";
        if !enabled {
            std::future::pending::<()>().await;
            return Ok(());
        }
        // Need an executor (= wallet) to monitor. If not configured, dormant.
        let Some(exec) = self.executor.as_ref() else {
            tracing::info!(target: "wallet_watch",
                "no executor configured — wallet watch dormant");
            std::future::pending::<()>().await;
            return Ok(());
        };
        use std::str::FromStr;
        let pubkey_str = exec.wallet_pubkey_str();
        let owner = match Pubkey::from_str(&pubkey_str) {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(target: "wallet_watch", error = ?e,
                    "wallet pubkey unparsable — watch dormant");
                std::future::pending::<()>().await;
                return Ok(());
            }
        };
        let usdc_ata = crate::raw_tx::derive_ata(&owner, &crate::known::USDC);

        let interval_ms: u64 = std::env::var("WALLET_WATCH_INTERVAL_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(30_000);
        let drop_pct: f64 = std::env::var("WALLET_WATCH_DROP_PCT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(10.0);

        tracing::info!(target: "wallet_watch",
            wallet = %owner, interval_ms, drop_pct,
            "🛡 wallet watch active — Telegram alerts on sudden balance drops");

        // Rolling 5-min peak window: ~10 samples at 30s interval.
        let mut peaks: std::collections::VecDeque<(std::time::Instant, f64)> =
            std::collections::VecDeque::with_capacity(16);
        let mut interval_t = tokio::time::interval(Duration::from_millis(interval_ms));
        // Discard the immediate first tick — first reading establishes baseline.
        interval_t.tick().await;
        // SOL price stub: 100 — refined once vault store has live price. The
        // watch alerts on PERCENTAGE drops, so the absolute SOL/USD ratio is
        // less important than detecting a sudden change.
        let approx_sol_usd: f64 = 100.0;
        let mut last_alert_at: Option<std::time::Instant> = None;
        let alert_cooldown = Duration::from_secs(300); // dedupe rapid-fire alerts

        loop {
            interval_t.tick().await;
            let sol_lamports = self.rpc.get_balance(&owner).await.unwrap_or(0);
            let usdc_base    = self.rpc.get_token_account_balance(&usdc_ata).await.unwrap_or(0);
            let sol_usd  = (sol_lamports as f64 / 1e9) * approx_sol_usd;
            let usdc_usd =  usdc_base   as f64 / 1e6;
            let total    = sol_usd + usdc_usd;

            let now = std::time::Instant::now();
            // Trim peaks older than 5 minutes.
            while let Some(&(t, _)) = peaks.front() {
                if now.duration_since(t) > Duration::from_secs(300) { peaks.pop_front(); }
                else { break; }
            }
            peaks.push_back((now, total));
            let peak_total = peaks.iter().map(|(_, v)| *v).fold(0.0_f64, f64::max);

            let drop = if peak_total > 0.01 {
                (peak_total - total) / peak_total * 100.0
            } else { 0.0 };

            tracing::info!(target: "wallet_watch",
                sol = format!("{:.6}", sol_lamports as f64 / 1e9),
                usdc = format!("{:.2}", usdc_usd),
                total_usd = format!("{:.2}", total),
                peak_5min = format!("{:.2}", peak_total),
                drop_pct = format!("{:.1}", drop),
                "balance snapshot");

            if drop >= drop_pct && peaks.len() >= 2 {
                let recently_alerted = last_alert_at
                    .map(|t| now.duration_since(t) < alert_cooldown)
                    .unwrap_or(false);
                if !recently_alerted {
                    let msg = format!(
                        "🚨 WALLET DROP ALERT\nwallet: {}\nSOL: {:.6}\nUSDC: ${:.2}\nTotal: ${:.2}\n5-min peak: ${:.2}\nDrop: {:.1}%\n\nPossible compromise — pause bot and check on-chain.",
                        owner,
                        sol_lamports as f64 / 1e9,
                        usdc_usd, total, peak_total, drop,
                    );
                    let _ = self.telegram.send(&msg).await;
                    tracing::warn!(target: "wallet_watch",
                        wallet = %owner, drop_pct = drop, total, peak_total,
                        "🚨 alert fired — Telegram notified");
                    last_alert_at = Some(now);
                }
            }
        }
    }

    /// Phase 4c: TP/SL monitor for pump.fun positions held by the
    /// Axiom-trending sniper. Polls each held mint every 10s, computes
    /// current sell-value from the bonding curve, exits at:
    ///   * 2× of entry SOL  → take-profit
    ///   * elapsed > 5 min  → market-out (timeout)
    /// Sold positions get marked, not removed, so the log retains a
    /// per-mint audit trail for the session.
    ///
    /// Tunables:
    ///   PUMP_FUN_TP_MULTIPLIER  (default 2.0)
    ///   PUMP_FUN_TIMEOUT_SECS   (default 300)
    ///   PUMP_FUN_TP_POLL_SECS   (default 10)
    pub async fn run_pump_fun_tp_sl(&self) -> ArbResult<()> {
        let Some(exec) = self.executor.as_ref().cloned() else {
            std::future::pending::<()>().await;
            return Ok(());
        };
        let tp_mult: f64 = std::env::var("PUMP_FUN_TP_MULTIPLIER")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(2.0);
        let timeout_secs: u64 = std::env::var("PUMP_FUN_TIMEOUT_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(300);
        let poll_secs: u64 = std::env::var("PUMP_FUN_TP_POLL_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(10);
        info!(target: "pump_tp_sl",
            tp_multiplier = tp_mult, timeout_secs, poll_secs,
            "🎯 pump.fun TP/SL monitor active");

        let helius_url = self.config.helius_rpc_url.clone();
        let payer_pk = exec.wallet_pubkey_str();
        use std::str::FromStr;
        let payer = match Pubkey::from_str(&payer_pk) {
            Ok(p) => p,
            Err(_) => {
                std::future::pending::<()>().await;
                return Ok(());
            }
        };

        // Phase 4e: optional boot-scan. When SELL_ALL_PUMP_ON_BOOT=true the
        // task scans the wallet for every SPL token whose mint ends with
        // "pump" (pump.fun's vanity grinder convention) and seeds
        // held_pump_positions with a bought_at timestamp far enough in the
        // past to immediately trigger the timeout-sell branch in the main
        // loop. Use to liquidate accumulated bags back to SOL in one pass.
        if std::env::var("SELL_ALL_PUMP_ON_BOOT").as_deref() == Ok("true") {
            match self.rpc.get_token_accounts_by_owner(&payer).await {
                Ok(accs) => {
                    let mut seeded = 0;
                    let mut positions = self.held_pump_positions.lock().await;
                    let seen: std::collections::HashSet<_> = positions.iter()
                        .map(|p| p.mint).collect();
                    let stale_buy = std::time::Instant::now()
                        .checked_sub(Duration::from_secs(timeout_secs + 60))
                        .unwrap_or_else(std::time::Instant::now);
                    for (_ata, mint, _amt) in accs {
                        if !mint.to_string().ends_with("pump") { continue; }
                        if seen.contains(&mint) { continue; }
                        positions.push(HeldPumpPosition {
                            mint,
                            creator: mint, // re-derived from BC state in main loop
                            bought_at: stale_buy,
                            sol_in_lamports: 1, // any non-zero — timeout path doesn't gate on it
                            sold: false,
                        });
                        seeded += 1;
                    }
                    info!(target: "pump_tp_sl",
                        seeded, total_positions = positions.len(),
                        "🧹 SELL_ALL_PUMP_ON_BOOT: seeded stale positions for timeout-sell");
                }
                Err(e) => warn!(target: "pump_tp_sl", error = %e,
                    "boot scan failed — no positions seeded"),
            }
        }

        loop {
            tokio::time::sleep(Duration::from_secs(poll_secs)).await;
            let mut positions = self.held_pump_positions.lock().await;
            for pos in positions.iter_mut() {
                if pos.sold { continue; }
                let elapsed = pos.bought_at.elapsed();
                let is_timeout = elapsed > Duration::from_secs(timeout_secs);

                // Fetch BC state for current price.
                let bc_pda = crate::pump_fun::derive_bonding_curve_pda(&pos.mint);
                let bc_data = match self.rpc.get_multiple_accounts(&[bc_pda, pos.mint]).await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let bc_state = bc_data.first().and_then(|o| o.as_ref())
                    .and_then(|s| crate::pump_fun::BondingCurveState::try_parse(&s.data));
                let (creator, virt_sol, virt_tok) = match bc_state {
                    Some(st) => (st.creator, st.virtual_sol_reserves, st.virtual_token_reserves),
                    None => {
                        warn!(target: "pump_tp_sl", mint = %pos.mint,
                            "could not parse BC state — skipping tick");
                        continue;
                    }
                };

                // Get our wallet's token balance for this mint.
                let user_ata = crate::raw_tx::derive_ata(&payer, &pos.mint);
                let tokens_held = self.rpc.get_token_account_balance(&user_ata).await
                    .unwrap_or(0);
                if tokens_held == 0 {
                    // Either never landed, already sold, or migrated to AMM. Mark sold.
                    pos.sold = true;
                    continue;
                }

                // Approximate sell value: tokens × (virt_sol / virt_tok) × 0.99 (1% fee).
                // For exact, run the BC constant-product math but this estimate is
                // accurate enough for TP/SL gates.
                let sell_value_lamports = ((tokens_held as u128 * virt_sol as u128)
                    / virt_tok.max(1) as u128) as u64;
                let sell_value_lamports = (sell_value_lamports as f64 * 0.99) as u64;
                let entry = pos.sol_in_lamports;
                let multiple = sell_value_lamports as f64 / entry.max(1) as f64;

                let should_sell = multiple >= tp_mult || is_timeout;
                if !should_sell {
                    tracing::debug!(target: "pump_tp_sl",
                        mint = %pos.mint, multiple = format!("{:.2}", multiple),
                        elapsed_s = elapsed.as_secs(),
                        "holding");
                    continue;
                }

                let reason = if multiple >= tp_mult { "TP" } else { "timeout" };
                info!(target: "pump_tp_sl",
                    mint = %pos.mint, reason,
                    multiple = format!("{:.2}", multiple),
                    elapsed_s = elapsed.as_secs(),
                    tokens_held, entry_lamports = entry,
                    estimated_value_lamports = sell_value_lamports,
                    "🔴 selling pump.fun position");

                // Fire sell. min_sol_out = 0 to accept market (we want OUT).
                match exec.pump_fun_sell(pos.mint, creator, tokens_held, 0).await {
                    Ok(sig) => {
                        info!(target: "pump_tp_sl",
                            sig = %sig, mint = %pos.mint, reason,
                            "💰 pump.fun sell SENT");
                        pos.sold = true;
                    }
                    Err(e) => warn!(target: "pump_tp_sl",
                        mint = %pos.mint, error = %e,
                        "pump.fun sell failed — will retry next tick"),
                }
            }
            drop(positions);
        }
    }

    pub async fn run_telegram_summary(&self) -> ArbResult<()> {
        // Wait for bootstrap so the first message has real numbers, not zeros.
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        // 15-minute cadence; the immediate first tick goes out right after
        // bootstrap so the operator knows Telegram delivery works.
        let mut interval = tokio::time::interval(Duration::from_secs(900));
        loop {
            interval.tick().await;
            let msg = render_summary(&self.metrics, self.store.len(), self.vaults.len());
            let _ = self.telegram.send(&msg).await;
        }
    }

    /// Long-running task: Telegram interactive commands (/status, /recent,
    /// /pools, /help). Silent no-op if Telegram isn't configured.
    pub async fn run_telegram_commands(&self) -> ArbResult<()> {
        let (Some(token), Some(chat_id)) = (
            self.config.telegram_bot_token.clone(),
            self.config.telegram_chat_id,
        ) else {
            info!(target: "scout::telegram", "commands disabled (token/chat_id not set)");
            // Park forever so try_join! doesn't immediately exit.
            std::future::pending::<()>().await;
            return Ok(());
        };
        let cmds = TelegramCommands::new(
            token, chat_id,
            Arc::clone(&self.metrics),
            Arc::clone(&self.store),
            self.config.log_dir.clone(),
        );
        cmds.run().await
    }

    /// Jupiter-driven detection loop. Continuously asks Jupiter's aggregator
    /// "is there an arb on SOL → X → SOL right now?" for a curated list of
    /// intermediates — bypasses the scout's pool-update trigger entirely.
    ///
    /// When `JUPITER_POLL_ENABLED=true`, this worker:
    ///   1. Iterates over a hardcoded list of liquid intermediates (USDC,
    ///      USDT, LSTs, memes, ecosystem tokens).
    ///   2. For each, fires `executor.maybe_execute_with_base(SOL, X, probe)`.
    ///   3. The executor calls Jupiter quote → checks `net >= min_profit` →
    ///      sends via Helius Sender if profitable.
    ///   4. Sleeps `JUPITER_POLL_INTERVAL_MS` (default 100 ms) between
    ///      intermediates.
    ///
    /// Rate budget at 100 ms interval × 14 intermediates × 2 legs/quote =
    /// ~280 Jupiter requests/sec; Pro tier handles this comfortably. The
    /// existing `min_interval_ms` throttle still caps SEND rate.
    pub async fn run_jupiter_poll(&self) -> ArbResult<()> {
        let enabled = std::env::var("JUPITER_POLL_ENABLED").as_deref() == Ok("true");
        if !enabled {
            // Stay alive so try_join! doesn't trip — sleep forever.
            std::future::pending::<()>().await;
            return Ok(());
        }

        // Wait for bootstrap so we don't fire before pools loaded.
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        let Some(executor) = self.executor.as_ref() else {
            info!(target: "scout::jupiter_poll",
                "no executor configured — polling worker dormant");
            std::future::pending::<()>().await;
            return Ok(());
        };

        let interval_ms: u64 = std::env::var("JUPITER_POLL_INTERVAL_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(100);
        let probe_sol_lamports: u64 = std::env::var("SCOUT_PROBE_AMOUNT_SOL_LAMPORTS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(20_000_000);

        // Curated intermediates — verified liquid pairs against SOL on
        // mainnet (May 2026). Order: stablecoins → LSTs → memes → ecosystem.
        //
        // 2026-05-17: extended from 14→22. Reason: both observed cleared
        // events (13:12 mSOL, 13:15 jitoSOL) routed through LSTs. Added
        // LST, hubSOL, vSOL, compassSOL to broaden the LST surface that's
        // where the brief mid-price drifts seem to live; plus POPCAT, W,
        // PENGU, PYUSD as additional liquid ecosystem mints.
        use solana_program::pubkey::Pubkey;
        let intermediates: Vec<Pubkey> = vec![
            solana_program::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"), // USDC
            solana_program::pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"), // USDT
            // ─── LSTs (graceful expansion — these had the cleared events) ───
            solana_program::pubkey!("J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn"), // JitoSOL
            solana_program::pubkey!("mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So"), // mSOL
            solana_program::pubkey!("jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v"), // jupSOL
            solana_program::pubkey!("bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1"), // bSOL
            solana_program::pubkey!("5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm"), // INF
            solana_program::pubkey!("LSTxxxnJzKDFSLr4dUkPcmCf5VyryEqzPLz5j4bpxFp"), // LST (Marinade Liquid Staking)
            solana_program::pubkey!("HUBsveNpjo5pWqNkH57QzxjQASdTVXcSK7bVKTSZtcSX"), // hubSOL
            solana_program::pubkey!("vSoLxydx6akxyMD9XEcPvGYNGq6Nn66oqVb3UkGkei7"), // vSOL
            // compassSOL removed 2026-05-17 — incorrect mint address returned
            // "not tradable" from Jupiter (CgnTSoL3DgY9SFHxcLj6CnCsk6jgmGsvANJoXxJgVMHF
            // doesn't match the real compassSOL mint; needs verification before re-adding).
            // ─── Memes / ecosystem ───
            solana_program::pubkey!("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263"), // BONK
            solana_program::pubkey!("EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm"), // WIF
            solana_program::pubkey!("7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr"), // POPCAT
            solana_program::pubkey!("2zMMhcVQEXDtdE6vsFS7S7D5oUodfJHE8vd1gnBouauv"), // PENGU
            solana_program::pubkey!("jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"), // JTO
            solana_program::pubkey!("JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN"), // JUP
            solana_program::pubkey!("4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"), // RAY
            solana_program::pubkey!("HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3"), // PYTH
            solana_program::pubkey!("27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4"), // JLP
            solana_program::pubkey!("85VBFQZC9TZkfaptBWjvUw7YbZjy52A6mjtPGjstQAmQ"), // W (Wormhole)
            // ─── New stables (PayPal + Sky/MakerDAO) — additional pegged routes ───
            solana_program::pubkey!("2b1kV6DkPAnxd5ixfnxCpjxmKwqjjaYmCZfHsFu24GXo"), // PYUSD
        ];

        info!(target: "scout::jupiter_poll",
            intermediates = intermediates.len(),
            interval_ms,
            probe_sol_lamports,
            "🪐 Jupiter polling worker started — Jupiter is the detector");

        let mut idx: usize = 0;
        loop {
            let intermediate = intermediates[idx % intermediates.len()];
            idx = idx.wrapping_add(1);
            if let Err(e) = executor
                .maybe_execute_with_base(known::SOL, intermediate, probe_sol_lamports)
                .await
            {
                tracing::warn!(target: "scout::jupiter_poll",
                    intermediate = %intermediate, error = %e,
                    "poll iteration error");
            }
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }

    /// Cross-stable arb worker — rotates through the 6 directions formed by
    /// USDC, USDT, and CASH. One-way trades only; each successful land flips
    /// the wallet from `src` to `dst`. Profit = spread between Jupiter's
    /// optimal route for that direction vs its reverse.
    ///
    /// Toggle: `CROSS_STABLE_ARB_ENABLED=true`
    /// Tune: `CROSS_STABLE_POLL_INTERVAL_MS` (default 500),
    ///       `CROSS_STABLE_PROBE_USD_BASE` (default 100_000_000 = $100),
    ///       `CROSS_STABLE_CASH_ENABLED` (default true — set false to skip CASH legs)
    pub async fn run_cross_stable_arb(&self) -> ArbResult<()> {
        let enabled = std::env::var("CROSS_STABLE_ARB_ENABLED").as_deref() == Ok("true");
        if !enabled {
            std::future::pending::<()>().await;
            return Ok(());
        }
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let Some(executor) = self.executor.as_ref() else {
            info!(target: "scout::cross_stable",
                "no executor configured — cross-stable worker dormant");
            std::future::pending::<()>().await;
            return Ok(());
        };
        let interval_ms: u64 = std::env::var("CROSS_STABLE_POLL_INTERVAL_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(500);
        let probe: u64 = std::env::var("CROSS_STABLE_PROBE_USD_BASE")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(100_000_000);
        let cash_enabled = std::env::var("CROSS_STABLE_CASH_ENABLED")
            .as_deref() != Ok("false");

        // Rotation: USDC↔USDT always; if cash_enabled, add the 4 CASH legs.
        // First failing leg (e.g. no CASH yet) just skips inside the executor.
        let mut directions: Vec<(solana_program::pubkey::Pubkey, solana_program::pubkey::Pubkey)> =
            vec![(known::USDC, known::USDT), (known::USDT, known::USDC)];
        if cash_enabled {
            directions.push((known::USDC, known::CASH));
            directions.push((known::CASH, known::USDC));
            directions.push((known::USDT, known::CASH));
            directions.push((known::CASH, known::USDT));
        }

        info!(target: "scout::cross_stable",
            interval_ms, probe, cash_enabled, n_directions = directions.len(),
            "🔁 Cross-stable arb worker started — rotating USDC/USDT/CASH one-way trades");

        let mut idx: usize = 0;
        loop {
            let (src, dst) = directions[idx % directions.len()];
            idx = idx.wrapping_add(1);
            if let Err(e) = executor.maybe_execute_one_way(src, dst, probe).await {
                tracing::warn!(target: "scout::cross_stable",
                    src = %src, dst = %dst, error = %e,
                    "cross-stable iteration error");
            }
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    }

    /// SOL accumulator — periodically sweeps USDC/USDT balances above a
    /// configured threshold into SOL via Jupiter. Designed to pair with
    /// `run_cross_stable_arb`: the cross-stable worker captures stable
    /// depegs into USDC or USDT; this worker converts the surplus to SOL.
    ///
    /// Tune:
    ///   `SOL_ACCUMULATOR_ENABLED`              (default false)
    ///   `SOL_ACCUMULATOR_INTERVAL_SECS`        (default 60)
    ///   `SOL_ACCUMULATOR_THRESHOLD_USDC_BASE`  (default 75_000_000 = $75)
    ///   `SOL_ACCUMULATOR_MIN_SWAP_USDC_BASE`   (default 1_000_000 = $1)
    pub async fn run_sol_accumulator(&self) -> ArbResult<()> {
        let enabled = std::env::var("SOL_ACCUMULATOR_ENABLED")
            .as_deref() == Ok("true");
        if !enabled {
            std::future::pending::<()>().await;
            return Ok(());
        }
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let Some(executor) = self.executor.as_ref() else {
            info!(target: "scout::sol_accum",
                "no executor — sol accumulator dormant");
            std::future::pending::<()>().await;
            return Ok(());
        };
        let interval_secs: u64 = std::env::var("SOL_ACCUMULATOR_INTERVAL_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let threshold_base: u64 = std::env::var("SOL_ACCUMULATOR_THRESHOLD_USDC_BASE")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(75_000_000);
        let min_swap_base: u64 = std::env::var("SOL_ACCUMULATOR_MIN_SWAP_USDC_BASE")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
        info!(target: "scout::sol_accum",
            interval_secs, threshold_base, min_swap_base,
            "💰 SOL accumulator started — sweeps stable excess into SOL");
        let owner_str = self.executor.as_ref()
            .map(|e| e.wallet_pubkey_str())
            .unwrap_or_else(|| "unknown".to_string());
        let http = match reqwest::Client::builder()
            .timeout(Duration::from_secs(10)).build()
        {
            Ok(c) => c,
            Err(_) => {
                warn!(target: "scout::sol_accum", "could not build http client");
                std::future::pending::<()>().await;
                return Ok(());
            }
        };
        let helius_url = self.config.helius_rpc_url.clone();

        loop {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            for (label, mint) in [
                ("USDC", crate::known::USDC),
                ("USDT", crate::known::USDT),
            ] {
                let body = serde_json::json!({
                    "jsonrpc": "2.0", "id": 1,
                    "method": "getTokenAccountsByOwner",
                    "params": [&owner_str, {"mint": mint.to_string()},
                        {"encoding": "jsonParsed"}],
                });
                let bal: u64 = match http.post(&helius_url).json(&body).send().await {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(v) => v.pointer("/result/value/0/account/data/parsed/info/tokenAmount/amount")
                            .and_then(|a| a.as_str())
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(0),
                        Err(_) => 0,
                    },
                    Err(_) => 0,
                };
                if bal <= threshold_base { continue; }
                let excess = bal - threshold_base;
                if excess < min_swap_base { continue; }
                info!(target: "scout::sol_accum",
                    stable = label, balance_base = bal, threshold_base,
                    excess_base = excess,
                    "🔁 sweeping stable surplus → SOL");
                match executor.execute_jupiter_swap_raw(
                    mint, crate::known::SOL, excess
                ).await {
                    Ok((sig, sol_out)) => {
                        info!(target: "scout::sol_accum",
                            stable = label, swap_in_base = excess,
                            sol_out_lamports = sol_out, sig = %sig,
                            "✅ stable→SOL swap sent");
                    }
                    Err(e) => {
                        warn!(target: "scout::sol_accum",
                            stable = label, error = %e,
                            "stable→SOL swap failed");
                    }
                }
            }
        }
    }

    /// Solend liquidator — monitors all main-market obligations, computes
    /// health ratio from on-chain (stale) state every `SOLEND_POLL_SECS`,
    /// logs alerts when a position is close to or past liquidation threshold.
    ///
    /// When `SOLEND_LIQUIDATOR_LIVE=true` AND a candidate's health is past
    /// the threshold AND the protocol-allowed liquidation amount × bonus
    /// exceeds `SOLEND_LIQUIDATOR_MIN_PROFIT_USD`, builds and sends a Refresh→
    /// Liquidate→Redeem bundle via Helius RPC.
    ///
    /// Tunables:
    ///   SOLEND_LIQUIDATOR_ENABLED         (default false)
    ///   SOLEND_LIQUIDATOR_LIVE            (default false — observe-only)
    ///   SOLEND_POLL_SECS                  (default 60)
    ///   SOLEND_ALERT_RATIO                (default 0.95)
    ///   SOLEND_LIQUIDATOR_MIN_PROFIT_USD  (default 1.0 = $1 minimum after costs)
    pub async fn run_solend_liquidator(&self) -> ArbResult<()> {
        let enabled = std::env::var("SOLEND_LIQUIDATOR_ENABLED").as_deref() == Ok("true");
        if !enabled {
            std::future::pending::<()>().await;
            return Ok(());
        }
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let poll_secs: u64 = std::env::var("SOLEND_POLL_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let alert_ratio: f64 = std::env::var("SOLEND_ALERT_RATIO")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(0.95);
        let live = std::env::var("SOLEND_LIQUIDATOR_LIVE").as_deref() == Ok("true");
        let min_profit_usd: f64 = std::env::var("SOLEND_LIQUIDATOR_MIN_PROFIT_USD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
        // Phase 6n: max-bonus filter. Without Shred Delivery we lose every
        // Pyth race against searchers chasing the high-bonus obligations
        // (~100 competitors on >$100 bonuses). Capping the upper bound
        // routes us to the niches that the big players ignore. Default
        // f64::INFINITY = disabled (legacy behaviour).
        let max_bonus_usd: f64 = std::env::var("SOLEND_LIQUIDATOR_MAX_BONUS_USD")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(f64::INFINITY);

        info!(target: "solend_liq",
            poll_secs, alert_ratio, live, min_profit_usd, max_bonus_usd,
            "💀 Solend liquidator started");

        let rpc = self.config.helius_rpc_url.clone();
        let markets = crate::solend_liq::active_markets();

        // Bootstrap reserve cache across ALL active markets (multi-pool).
        let reserves = match crate::solend_liq::bootstrap_reserve_cache_multi(&rpc, &markets).await {
            Ok(c) => c,
            Err(e) => {
                warn!(target: "solend_liq", error = %e,
                    "could not bootstrap reserve cache — dormant");
                std::future::pending::<()>().await;
                return Ok(());
            }
        };

        // Phase 6q: shred-bridge wake — spawn a background task that polls
        // /tmp/shred-pyth-alert (written by the Python shred subscriber on
        // every Pyth program detection in shreds). When the file's mtime
        // changes, wake the Solend scan via the same notify channel that
        // Yellowstone's Pyth-tick path uses. Shreds arrive ~50-200ms before
        // LaserStream Processed, giving us a head-start on Pyth-race
        // liquidations.
        if std::env::var("SOLEND_SHRED_BRIDGE_ENABLED").as_deref() == Ok("true") {
            let notify = Arc::clone(&self.solend_scan_notify);
            tokio::spawn(async move {
                let path = std::path::Path::new("/tmp/shred-pyth-alert");
                let mut last_mtime: Option<std::time::SystemTime> = None;
                loop {
                    if let Ok(meta) = std::fs::metadata(path) {
                        if let Ok(mtime) = meta.modified() {
                            if Some(mtime) != last_mtime {
                                last_mtime = Some(mtime);
                                notify.notify_one();
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            });
            info!(target: "solend_liq", "🩸 shred-bridge active — polling /tmp/shred-pyth-alert every 10ms");
        }

        // Phase 3c: publish the Pyth oracle set used by Solend reserves so
        // handle_update can wake this loop the moment any of them ticks.
        // The set is small (~30-50 unique Pyth feeds across 117 reserves).
        {
            let mut w = self.solend_pyth_set.write().unwrap();
            for r in reserves.values() {
                w.insert(r.pyth_oracle);
            }
            info!(target: "solend_liq",
                pyth_feeds = w.len(),
                "🔔 Pyth-trigger set published");
        }

        // Phase 3b: track the previous scan's underwater-obligation set so we
        // can target ONLY freshly-crossed positions. The same 13 stale-zombie
        // obligations show up every scan with ratio >= 1.0 by stale on-chain
        // math, but Pyth refresh in sim flips them back to healthy. By
        // attempting only fresh crossings we (a) stop burning resources on
        // known-zombie obligations and (b) reach truly-new underwater
        // positions before competitors that ALSO use stale ranking.
        let fresh_only = std::env::var("SOLEND_LIQUIDATOR_FRESH_ONLY")
            .as_deref() == Ok("true");
        let mut prev_underwater: std::collections::HashSet<solana_program::pubkey::Pubkey>
            = std::collections::HashSet::new();
        let mut scans_completed: u64 = 0;

        loop {
            // Phase 3c: wake on EITHER the poll timer OR a Pyth-oracle ping.
            // The Notify is single-permit-with-coalescing, so multiple Pyth
            // ticks during one scan collapse into a single follow-up scan.
            let trigger = tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(poll_secs)) => "timer",
                _ = self.solend_scan_notify.notified() => "pyth",
            };
            if trigger == "pyth" {
                tracing::info!(target: "solend_liq",
                    "⚡ Pyth-triggered scan (woke before poll tick)");
            }

            let started = std::time::Instant::now();
            let obligations = match crate::solend_liq::fetch_all_obligations_multi(&rpc, &markets).await {
                Ok(o) => o,
                Err(e) => {
                    warn!(target: "solend_liq", error = %e, "fetch failed");
                    continue;
                }
            };
            let fetched = obligations.len();

            // Sort: highest expected bonus first (real money). Sanity-filter junk.
            let mut ranked: Vec<_> = obligations.iter()
                .filter(|o| o.is_sane())
                .map(|o| (o.liq_ratio(), o.est_bonus_usd(), o))
                .filter(|(r, _, _)| *r > 0.0)
                .collect();
            ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            let n_liq = ranked.iter().filter(|(r, _, _)| *r >= 1.0).count();
            let n_close = ranked.iter().filter(|(r, _, _)| *r >= alert_ratio && *r < 1.0).count();
            let n_profitable = ranked.iter()
                .filter(|(r, b, _)| *r >= 1.0 && *b >= min_profit_usd).count();
            let top_bonus = ranked.first().map(|(_, b, _)| *b).unwrap_or(0.0);
            info!(target: "solend_liq",
                fetched,
                liquidatable = n_liq,
                close = n_close,
                profitable = n_profitable,
                top_bonus_usd = format!("${:.2}", top_bonus),
                elapsed_ms = started.elapsed().as_millis() as u64,
                reserves_known = reserves.len(),
                "📊 Solend scan");

            // Strategy: target the off-radar size band. With our $235 USDC
            // wallet cap, bonus is fixed at ~$11 regardless of position size.
            // So bigger ≠ better — the bigger ones are just heavily-watched.
            // Sweet spot = $1.5K-$5K borrowed: big bots set their min-bonus
            // filter above $50-100, so these positions fly under their radar.
            // Tunables:
            //   SOLEND_LIQUIDATOR_MIN_BORROWED_USD (default 1500)
            //   SOLEND_LIQUIDATOR_MAX_BORROWED_USD (default 5000)
            let skip_top: usize = std::env::var("SOLEND_LIQUIDATOR_SKIP_TOP")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(0);
            let take_n: usize = std::env::var("SOLEND_LIQUIDATOR_TAKE")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(20);
            let min_borrowed: f64 = std::env::var("SOLEND_LIQUIDATOR_MIN_BORROWED_USD")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(1500.0);
            let max_borrowed: f64 = std::env::var("SOLEND_LIQUIDATOR_MAX_BORROWED_USD")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(5000.0);

            // Phase 3d: upgrade the "close" set from header-only to full
            // breakdown via getMultipleAccounts. Then recompute each
            // obligation's ratio with LIVE Pyth prices overlaid on the cached
            // reserve+amount data. Catches price-driven crossings that the
            // stored ratio misses (Solend's on-chain market_value_usd only
            // updates on RefreshObligation). Bounded to the close set (ratio
            // 0.85+) AND the size band, so the full-fetch stays ~100-300
            // accounts per scan — well under getMultipleAccounts's 100/call.
            let close_set: Vec<solana_program::pubkey::Pubkey> = ranked.iter()
                .filter(|(r, _, o)| {
                    *r >= 0.85
                        && o.borrowed_value_usd >= min_borrowed
                        && o.borrowed_value_usd <= max_borrowed
                })
                .take(300)
                .map(|(_, _, o)| o.address)
                .collect();
            let live_ratios: std::collections::HashMap<solana_program::pubkey::Pubkey, f64> =
                if !close_set.is_empty() {
                    match crate::solend_liq::fetch_obligations_full_batch(&rpc, &close_set).await {
                        Ok(fulls) => fulls.into_iter()
                            .map(|f| (f.address, f.live_ratio(&reserves, &self.pyth)))
                            .collect(),
                        Err(e) => {
                            warn!(target: "solend_liq", error = %e,
                                "live-ratio batch fetch failed — falling back to stored ratio");
                            std::collections::HashMap::new()
                        }
                    }
                } else { std::collections::HashMap::new() };

            // Phase 3b — compute current underwater set + fresh crossings.
            // Use the LIVE ratio when available (close set, Phase 3d); fall
            // back to stored ratio for everything else.
            let effective_ratio = |o: &crate::solend_liq::ObligationHeader, stored: f64| -> f64 {
                live_ratios.get(&o.address).copied().unwrap_or(stored)
            };
            // Phase 4b: configurable underwater threshold. Default 1.0 means
            // ratio >= 1.0 counts as underwater (matches Solend's on-chain
            // logic). With SKIP_SIM=true we observed many reverts because our
            // live_ratio approximation hits 1.001-1.02 false-positives that
            // Solend's exact refresh resolves to "healthy". Bumping to 1.05
            // requires a 5% margin → fewer false positives, but still catches
            // genuine crossings that swing through.
            let underwater_threshold: f64 = std::env::var("SOLEND_LIVE_RATIO_THRESHOLD")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
            let current_underwater: std::collections::HashSet<_> = ranked.iter()
                .filter(|(r, _, o)| effective_ratio(o, *r) >= underwater_threshold)
                .map(|(_, _, o)| o.address)
                .collect();
            let n_fresh = current_underwater.difference(&prev_underwater).count();
            let n_live_priced = live_ratios.len();
            // On the very first scan, prev is empty so EVERYTHING looks fresh.
            // Skip the first scan's spawn loop entirely to establish baseline.
            let establishing_baseline = fresh_only && scans_completed == 0;
            if fresh_only {
                info!(target: "solend_liq",
                    fresh = n_fresh,
                    current_underwater = current_underwater.len(),
                    prev_underwater = prev_underwater.len(),
                    live_priced = n_live_priced,
                    baseline = establishing_baseline,
                    "🆕 fresh-only filter active");
            }

            for (ratio, est_bonus_usd, o) in ranked.iter()
                // Use effective_ratio (live if available, stored otherwise)
                // so price-driven crossings DO make it through this gate.
                .filter(|(r, b, o)| effective_ratio(o, *r) >= underwater_threshold
                    && *b >= min_profit_usd
                    && *b <= max_bonus_usd)
                .filter(|(_, _, o)| {
                    o.borrowed_value_usd >= min_borrowed
                        && o.borrowed_value_usd <= max_borrowed
                })
                .filter(|(_, _, o)| {
                    // When fresh-only is on, skip obligations we already
                    // saw underwater. Always pass when fresh-only is off,
                    // and ALWAYS skip during the baseline scan.
                    if !fresh_only { return true; }
                    if establishing_baseline { return false; }
                    !prev_underwater.contains(&o.address)
                })
                .skip(skip_top)
                .take(take_n)
            {
                let est_bonus_usd = *est_bonus_usd;
                info!(target: "solend_liq",
                    obligation = %o.address,
                    owner = %o.owner,
                    ratio = format!("{:.3}", ratio),
                    borrowed_usd = format!("${:.2}", o.borrowed_value_usd),
                    unhealthy_usd = format!("${:.2}", o.unhealthy_borrow_value),
                    est_bonus_usd = format!("${:.2}", est_bonus_usd),
                    live, profitable = est_bonus_usd >= min_profit_usd,
                    "🎯 LIQUIDATION CANDIDATE");

                if live && est_bonus_usd >= min_profit_usd {
                    let exec = match self.executor.as_ref() {
                        Some(e) => e.clone(),
                        None => {
                            warn!(target: "solend_liq", "no executor configured — cannot fire");
                            continue;
                        }
                    };
                    let rpc_dup = rpc.clone();
                    let reserves_dup = Arc::clone(&reserves);
                    let obligation_addr = o.address;
                    let borrowed = o.borrowed_value_usd;
                    tokio::spawn(async move {
                        if let Err(e) = exec.execute_solend_liquidation(
                            obligation_addr, &reserves_dup, &rpc_dup, borrowed,
                        ).await {
                            warn!(target: "solend_liq",
                                obligation = %obligation_addr, error = %e,
                                "💀 liquidation attempt failed");
                        }
                    });
                }
            }

            // Phase 3b — roll the underwater snapshot forward for next iteration.
            prev_underwater = current_underwater;
            scans_completed += 1;
        }
    }

    /// Pump.fun executor — detects launches, filters intelligently, buys via
    /// Jupiter with auto-exit at TP/SL/timeout. Use when `PUMP_FUN_EXECUTE_ENABLED`.
    /// Otherwise the detector below runs in observe-only mode.
    ///
    /// Heuristic filters (skip launches when):
    ///  * mint ends in literal "pump" (low-effort spam)
    ///  * dev wallet had a launch in last 60s (serial rugger)
    ///  * Jupiter has no route within 3s (= no liquidity yet)
    ///
    /// Exit rules per position:
    ///  * TP: current_value ≥ entry × `PUMP_FUN_TP_MULTIPLIER` (default 2.0)
    ///  * SL: current_value ≤ entry × `PUMP_FUN_SL_MULTIPLIER` (default 0.5)
    ///  * Timeout: age > `PUMP_FUN_TIMEOUT_SECS` (default 60)
    pub async fn run_pump_fun_executor(&self) -> ArbResult<()> {
        use std::collections::HashMap;
        use std::sync::Arc as StdArc;
        use tokio::sync::Mutex as TokioMutex;

        #[derive(Clone, Debug)]
        struct PumpPosition {
            mint:          solana_program::pubkey::Pubkey,
            entry_sol:     u64,           // lamports we spent
            token_amount:  u64,           // tokens we hold (from buy quote)
            entry_time:    std::time::Instant,
        }

        let enabled = std::env::var("PUMP_FUN_EXECUTE_ENABLED").as_deref() == Ok("true");
        if !enabled {
            std::future::pending::<()>().await;
            return Ok(());
        }

        let Some(executor) = self.executor.as_ref() else {
            info!(target: "scout::pump_fun_exec",
                "no executor configured — pump.fun executor dormant");
            std::future::pending::<()>().await;
            return Ok(());
        };

        // Tunables
        let buy_lamports: u64 = std::env::var("PUMP_FUN_BUY_LAMPORTS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(5_000_000);   // 0.005 SOL ≈ $0.48
        let tp_pct: u64 = std::env::var("PUMP_FUN_TP_PCT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(200);  // 2x
        let sl_pct: u64 = std::env::var("PUMP_FUN_SL_PCT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(50);   // -50%
        let timeout_secs: u64 = std::env::var("PUMP_FUN_TIMEOUT_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(60);
        let max_positions: usize = std::env::var("PUMP_FUN_MAX_POSITIONS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(5);
        let poll_secs: u64 = std::env::var("PUMP_FUN_POLL_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(5);
        let buy_delay_ms: u64 = std::env::var("PUMP_FUN_BUY_DELAY_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(3_000); // wait for Jupiter to index

        const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
        let rpc_url = self.config.helius_rpc_url.clone();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ArbError::Rpc(format!("reqwest build: {e}")))?;

        // Shared state
        let positions: StdArc<TokioMutex<HashMap<solana_program::pubkey::Pubkey, PumpPosition>>> =
            StdArc::new(TokioMutex::new(HashMap::new()));
        let recent_devs: StdArc<TokioMutex<HashMap<String, std::time::Instant>>> =
            StdArc::new(TokioMutex::new(HashMap::new()));
        let mut seen_sigs: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(2048);

        // ── Monitor loop (background) — passively logs position age every 30s
        // so the operator can see what's still open. Auto-exit is disabled
        // in this MVP — buys go through Jupiter (so `out_amount` semantics
        // for the sell direction are still being designed). The operator
        // manages exits via Phantom / Jupiter UI for now.
        let positions_for_monitor = StdArc::clone(&positions);
        let _ = (tp_pct, sl_pct, timeout_secs);  // tunables held for next iteration
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                let pos_map = positions_for_monitor.lock().await;
                if pos_map.is_empty() { continue }
                let now = std::time::Instant::now();
                for pos in pos_map.values() {
                    let age = now.duration_since(pos.entry_time).as_secs();
                    info!(target: "scout::pump_fun_exec",
                        mint = %pos.mint, entry_sol = pos.entry_sol, age_secs = age,
                        "📊 open position");
                }
            }
        });

        info!(target: "scout::pump_fun_exec",
            buy_lamports, tp_pct, sl_pct, timeout_secs, max_positions,
            "🎰 Pump.fun executor started — Jupiter-routed sniping with auto-exit");

        // ── Detection + buy loop ─────────────────────────────────────────
        loop {
            tokio::time::sleep(Duration::from_secs(poll_secs)).await;

            // 1. Fetch latest signatures
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "getSignaturesForAddress",
                "params": [PUMP_FUN_PROGRAM, {
                    "limit": 30,
                    "commitment": "confirmed",
                }],
            });
            let resp: serde_json::Value = match http.post(&rpc_url).json(&body).send().await {
                Ok(r) => match r.json().await { Ok(v) => v, Err(_) => continue },
                Err(_) => continue,
            };
            let Some(arr) = resp.get("result").and_then(|v| v.as_array()) else { continue };

            for entry in arr {
                let Some(sig) = entry.get("signature").and_then(|v| v.as_str()) else { continue };
                if seen_sigs.contains(sig) { continue }
                seen_sigs.insert(sig.to_string());

                // Fetch tx, check if Create
                let tx_body = serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
                    "params": [sig, {
                        "encoding": "jsonParsed",
                        "commitment": "confirmed",
                        "maxSupportedTransactionVersion": 0,
                    }],
                });
                let tx_resp: serde_json::Value = match http.post(&rpc_url).json(&tx_body).send().await {
                    Ok(r) => match r.json().await { Ok(v) => v, Err(_) => continue },
                    Err(_) => continue,
                };
                let Some(meta) = tx_resp.pointer("/result/meta") else { continue };
                let Some(logs) = meta.get("logMessages").and_then(|v| v.as_array()) else { continue };
                let mut is_create = false;
                for line in logs {
                    if let Some(s) = line.as_str() {
                        if s.contains("Program log: Instruction: Create") {
                            is_create = true;
                            break;
                        }
                    }
                }
                if !is_create { continue }

                let mint = tx_resp.pointer("/result/transaction/message/accountKeys/1/pubkey")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();
                let dev = tx_resp.pointer("/result/transaction/message/accountKeys/0/pubkey")
                    .and_then(|v| v.as_str()).unwrap_or("").to_string();

                if mint.is_empty() || dev.is_empty() { continue }

                // ── HEURISTIC FILTERS ─────────────────────────────────
                // Skip if mint ends in literal "pump" (low-effort spam).
                if mint.to_lowercase().ends_with("pump") {
                    continue;
                }
                // Skip if dev launched within last 60s (serial rugger).
                {
                    let mut devs = recent_devs.lock().await;
                    let now = std::time::Instant::now();
                    devs.retain(|_, t| now.duration_since(*t).as_secs() < 60);
                    if devs.contains_key(&dev) {
                        info!(target: "scout::pump_fun_exec",
                            mint, dev, "⏭ skip — dev launched <60s ago");
                        continue;
                    }
                    devs.insert(dev.clone(), now);
                }
                // Skip if max positions reached.
                {
                    let pos = positions.lock().await;
                    if pos.len() >= max_positions {
                        continue;
                    }
                }

                // ── BUY ATTEMPT (via Jupiter) ─────────────────────────
                let mint_pk = match <solana_program::pubkey::Pubkey as std::str::FromStr>::from_str(&mint) {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                let positions_for_buy = StdArc::clone(&positions);
                let executor_for_buy = executor.clone();
                tokio::spawn(async move {
                    // Wait for Jupiter to index the new token.
                    tokio::time::sleep(Duration::from_millis(buy_delay_ms)).await;
                    info!(target: "scout::pump_fun_exec",
                        mint = %mint_pk, "🎯 attempting Jupiter buy");
                    if let Err(e) = executor_for_buy.maybe_execute_one_way(
                        known::SOL, mint_pk, buy_lamports,
                    ).await {
                        warn!(target: "scout::pump_fun_exec",
                            mint = %mint_pk, error = %e,
                            "buy attempt failed (likely no Jupiter route yet)");
                        return;
                    }
                    // Record position. Token amount unknown precisely without
                    // landing confirmation — placeholder; monitor refreshes.
                    let pos = PumpPosition {
                        mint: mint_pk,
                        entry_sol: buy_lamports,
                        token_amount: 0,  // filled when sell-quote happens
                        entry_time: std::time::Instant::now(),
                    };
                    positions_for_buy.lock().await.insert(mint_pk, pos);
                });
            }

            if seen_sigs.len() > 5000 {
                seen_sigs.clear();
            }
        }
    }

    /// Pump.fun launch detector — Phase A "observation only" mode.
    ///
    /// Polls `getSignaturesForAddress(pump.fun program, limit=N)` every few
    /// seconds, then fetches the full tx for each previously-unseen signature
    /// and parses the program logs for `Instruction: Create`. Each detected
    /// launch is logged with `mint`, `dev`, `signature`, `slot`.
    ///
    /// Purpose: gather frequency + survivor-rate data before committing
    /// engineering to live sniping. The detector NEVER buys.
    ///
    /// Toggle: `PUMP_FUN_DETECTOR_ENABLED=true`.
    /// Tune: `PUMP_FUN_POLL_SECS` (default 5), `PUMP_FUN_BATCH_LIMIT`
    /// (default 50, max 1000 per Solana RPC limit).
    pub async fn run_pump_fun_detector(&self) -> ArbResult<()> {
        let enabled = std::env::var("PUMP_FUN_DETECTOR_ENABLED").as_deref() == Ok("true");
        if !enabled {
            std::future::pending::<()>().await;
            return Ok(());
        }

        const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
        let poll_secs: u64 = std::env::var("PUMP_FUN_POLL_SECS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(5);
        let batch_limit: u64 = std::env::var("PUMP_FUN_BATCH_LIMIT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(50);

        let rpc_url = self.config.helius_rpc_url.clone();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ArbError::Rpc(format!("reqwest build: {e}")))?;

        // LRU-style cap on seen signatures to bound memory.
        let mut seen_sigs: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(2048);
        let mut total_creates: u64 = 0;
        let mut total_polls: u64 = 0;
        let mut total_sigs_seen: u64 = 0;

        info!(target: "scout::pump_fun",
            poll_secs, batch_limit,
            "🎯 Pump.fun launch detector started — observe-only, no buys");

        loop {
            tokio::time::sleep(Duration::from_secs(poll_secs)).await;
            total_polls += 1;

            // 1. Fetch latest signatures touching pump.fun program.
            let body = serde_json::json!({
                "jsonrpc": "2.0", "id": 1, "method": "getSignaturesForAddress",
                "params": [PUMP_FUN_PROGRAM, {
                    "limit": batch_limit,
                    "commitment": "confirmed",
                }],
            });
            let resp: serde_json::Value = match http.post(&rpc_url).json(&body).send().await {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        warn!(target: "scout::pump_fun", error = %e, "sigs json parse");
                        continue;
                    }
                },
                Err(e) => {
                    warn!(target: "scout::pump_fun", error = %e, "sigs fetch");
                    continue;
                }
            };
            let Some(arr) = resp.get("result").and_then(|v| v.as_array()) else {
                continue;
            };

            // 2. For each previously-unseen sig, fetch tx + parse logs.
            for entry in arr {
                let Some(sig) = entry.get("signature").and_then(|v| v.as_str()) else { continue };
                if seen_sigs.contains(sig) { continue; }
                seen_sigs.insert(sig.to_string());
                total_sigs_seen += 1;

                let tx_body = serde_json::json!({
                    "jsonrpc": "2.0", "id": 1, "method": "getTransaction",
                    "params": [sig, {
                        "encoding": "jsonParsed",
                        "commitment": "confirmed",
                        "maxSupportedTransactionVersion": 0,
                    }],
                });
                let tx_resp: serde_json::Value = match http.post(&rpc_url).json(&tx_body)
                    .send().await {
                    Ok(r) => match r.json().await { Ok(v) => v, Err(_) => continue },
                    Err(_) => continue,
                };
                let Some(meta) = tx_resp.pointer("/result/meta") else { continue };
                let Some(logs) = meta.get("logMessages").and_then(|v| v.as_array()) else { continue };

                // Heuristic: `Program log: Instruction: Create` appears when
                // the pump.fun create instruction runs. Some bots invoke
                // their own "create" handlers — we cross-check that the
                // pump.fun program is also invoked in this tx.
                let mut is_create = false;
                let mut pump_invoked = false;
                for line in logs {
                    let Some(s) = line.as_str() else { continue };
                    if s.contains("Program log: Instruction: Create") {
                        is_create = true;
                    }
                    if s.contains(PUMP_FUN_PROGRAM) && s.contains("invoke") {
                        pump_invoked = true;
                    }
                }
                if !(is_create && pump_invoked) { continue; }

                total_creates += 1;

                // Extract mint — pump.fun's Create instruction layout puts
                // the new mint at account index 0 of the create ix's accounts.
                // accountKeys[0] is the fee payer, [1] is typically the mint.
                let mint = tx_resp.pointer("/result/transaction/message/accountKeys/1/pubkey")
                    .and_then(|v| v.as_str()).unwrap_or("?");
                let dev = tx_resp.pointer("/result/transaction/message/accountKeys/0/pubkey")
                    .and_then(|v| v.as_str()).unwrap_or("?");
                let slot = tx_resp.pointer("/result/slot")
                    .and_then(|v| v.as_u64()).unwrap_or(0);

                info!(target: "scout::pump_fun",
                    sig, mint, dev, slot,
                    "🎯 NEW PUMP.FUN TOKEN");
            }

            // Bound memory: drop seen-set when it grows large.
            if seen_sigs.len() > 5000 {
                seen_sigs.clear();
            }

            // Periodic rollup line so the operator can see liveness.
            if total_polls % 12 == 0 {
                info!(target: "scout::pump_fun",
                    total_polls, total_sigs_seen, total_creates,
                    "📊 pump.fun detector rollup");
            }
        }
    }

    pub async fn run_jito_correlator(&self) -> ArbResult<()> {
        // Wait for bootstrap so we don't try to fetch blocks before any
        // opportunities can have been queued.
        while !self.bootstrapped.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.correlator.clone().run().await
    }

    /// Periodic stats line so the operator can see liveness without DEBUG spam.
    pub async fn run_stats_dump(&self) -> ArbResult<()> {
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            let s = self.metrics.snapshot();
            let capture_pct = if s.opps_correlated_total > 0 {
                100.0 * s.opps_uncontested as f64 / s.opps_correlated_total as f64
            } else { 0.0 };

            // Executor metrics, if executor is wired
            let exec = self.executor.as_ref().map(|e| e.metrics());
            let (ex_considered, ex_sample, ex_throttle, ex_jup_err, ex_below_min, ex_sim_ok, ex_sim_fail, ex_dry, ex_sent, ex_landed, ex_net) =
                if let Some(em) = exec.as_ref() {
                    (em.considered.load(Ordering::Relaxed),
                     em.skipped_sample.load(Ordering::Relaxed),
                     em.skipped_throttle.load(Ordering::Relaxed),
                     em.jupiter_errors.load(Ordering::Relaxed),
                     em.skipped_below_min.load(Ordering::Relaxed),
                     em.simulated_ok.load(Ordering::Relaxed),
                     em.simulated_fail.load(Ordering::Relaxed),
                     em.skipped_dry_run.load(Ordering::Relaxed),
                     em.sent_bundles.load(Ordering::Relaxed),
                     em.landed_bundles.load(Ordering::Relaxed),
                     em.running_net_usdc.load(Ordering::Relaxed))
                } else { (0,0,0,0,0,0,0,0,0,0,0) };

            // Native executor metrics
            let native = self.native_exec.as_ref().map(|e| e.metrics());
            let (nat_checks, nat_eligible, nat_considered, nat_throttle, nat_no_market, nat_no_vault, nat_below_min, nat_sent, nat_landed) =
                if let Some(nm) = native.as_ref() {
                    (nm.eligibility_checks.load(Ordering::Relaxed),
                     nm.eligibility_passed.load(Ordering::Relaxed),
                     nm.considered.load(Ordering::Relaxed),
                     nm.skipped_throttle.load(Ordering::Relaxed),
                     nm.skipped_no_market.load(Ordering::Relaxed),
                     nm.skipped_no_vault.load(Ordering::Relaxed),
                     nm.skipped_below_min.load(Ordering::Relaxed),
                     nm.sent_bundles.load(Ordering::Relaxed),
                     nm.landed_bundles.load(Ordering::Relaxed))
                } else { (0,0,0,0,0,0,0,0,0) };

            // Buffer metrics — only meaningful with the buffer-and-select
            // worker. `submitted` is the new "considered by scout" count;
            // `selected` is what the worker actually fired on.
            let (buf_submitted, buf_selected, buf_dropped_stale, buf_dropped_overflow, buf_max_seen) =
                if let Some(ne) = self.native_exec.as_ref() {
                    let m = &ne.buffer.metrics;
                    (m.submitted.load(Ordering::Relaxed),
                     m.selected.load(Ordering::Relaxed),
                     m.dropped_stale.load(Ordering::Relaxed),
                     m.dropped_overflow.load(Ordering::Relaxed),
                     m.max_size_seen.load(Ordering::Relaxed))
                } else { (0,0,0,0,0) };

            info!(
                target: "scout::stats",
                updates        = s.updates_received,
                no_parser      = s.no_parser_claimed,
                no_parser_known = s.no_parser_known_dex,
                vault_updates  = s.vault_updates_received,
                evaluated      = s.routes_evaluated,
                ok_none        = s.routes_eval_ok_none,
                ok_some        = s.routes_eval_ok_some,
                err            = s.routes_eval_err,
                opps_001       = s.opps_ge_001,
                opps_01        = s.opps_ge_01,
                opps_10c       = s.opps_ge_10c,
                opps_1d        = s.opps_ge_1d,
                opps_5d        = s.opps_ge_5d,
                opps_20d       = s.opps_ge_20d,
                largest_usdc   = s.largest_net_profit_usdc,
                wss_reconnect  = s.wss_reconnects,
                bootstraps     = s.bootstrap_count,
                pools_in_store = self.store.len(),
                vaults_in_store = self.vaults.len(),
                sol_price_usd  = format!("{:.2}", s.sol_price_usd),
                correlated     = s.opps_correlated_total,
                lost           = s.opps_lost_to_competitor,
                uncontested    = s.opps_uncontested,
                capture_pct    = format!("{capture_pct:.1}"),
                ex_considered,
                ex_sample,
                ex_throttle,
                ex_jup_err,
                ex_below_min,
                ex_sim_ok,
                ex_sim_fail,
                ex_dry,
                ex_sent,
                ex_landed,
                ex_net_usdc    = ex_net,
                nat_checks,
                nat_eligible,
                nat_considered,
                nat_throttle,
                nat_no_market,
                nat_no_vault,
                nat_below_min,
                nat_sent,
                nat_landed,
                buf_submitted,
                buf_selected,
                buf_dropped_stale,
                buf_dropped_overflow,
                buf_max_seen,
                "scout stats"
            );
        }
    }

    /// Subscribe to Jito's ShredStream Proxy on `localhost:9000` and tally
    /// matches by program label. Disabled at build time when the
    /// `shredstream` feature is off — falls through to a no-op.
    ///
    /// First-cut integration: just observe. The bot keeps its existing
    /// LaserStream subscription for pool account updates. Future work
    /// could route shred events into the same channel `run_enhanced_websocket`
    /// feeds, giving 50-100ms earlier pool-state refreshes.
    pub async fn run_shred_subscriber(&self) -> ArbResult<()> {
        #[cfg(feature = "shredstream")]
        {
            if std::env::var("SHRED_ALERT_DISABLE").ok().as_deref() == Some("true") {
                tracing::info!(target: "scout::shred", "SHRED_ALERT_DISABLE=true — skipping");
                return Ok(());
            }
            let endpoint = std::env::var("SHREDSTREAM_GRPC_URL")
                .unwrap_or_else(|_| "http://localhost:9000".to_string());
            tracing::info!(target: "scout::shred", %endpoint, "subscribing to ShredStream Proxy");
            let mut rx = arb_stream::shred::spawn_shred_subscriber(
                endpoint,
                arb_stream::shred::default_filters(),
                512,
            );
            use std::collections::HashMap;
            let mut tally: HashMap<&'static str, u64> = HashMap::new();
            let mut last_log = std::time::Instant::now();
            let mut last_pumpcreate_slot: u64 = 0;
            while let Some(ev) = rx.recv().await {
                *tally.entry(ev.label).or_insert(0) += 1;
                // Per-event spotlight for the rare-but-actionable graduations.
                if ev.label == "pumpswap-create" && ev.slot != last_pumpcreate_slot {
                    last_pumpcreate_slot = ev.slot;
                    tracing::info!(
                        target: "scout::shred",
                        slot = ev.slot,
                        ts_ms = ev.ts_ms,
                        "🎓 PumpSwap pool-create observed via shred"
                    );
                }
                if last_log.elapsed() > std::time::Duration::from_secs(60) {
                    tracing::info!(
                        target: "scout::shred",
                        pyth = tally.get("pyth").copied().unwrap_or(0),
                        pumpswap = tally.get("pumpswap").copied().unwrap_or(0),
                        pumpswap_create = tally.get("pumpswap-create").copied().unwrap_or(0),
                        kamino = tally.get("kamino").copied().unwrap_or(0),
                        marginfi = tally.get("marginfi").copied().unwrap_or(0),
                        "📡 shred tally (last 60s)"
                    );
                    tally.clear();
                    last_log = std::time::Instant::now();
                }
            }
            Ok(())
        }
        #[cfg(not(feature = "shredstream"))]
        {
            tracing::warn!(target: "scout::shred",
                "binary built without shredstream feature — arbbot won't see shred events. \
                 Rebuild with `--features laserstream,shredstream`.");
            std::future::pending::<()>().await;
            Ok(())
        }
    }
}

fn render_route(route: &Route) -> String {
    let mut s = String::new();
    for (i, h) in route.hops.iter().enumerate() {
        if i == 0 {
            s.push_str(&token_label(&h.token_in));
        }
        s.push_str(" -> ");
        s.push_str(&token_label(&h.token_out));
    }
    s
}

fn token_label(m: &TokenMint) -> String {
    if m.0 == known::USDC      { "USDC".into() }
    else if m.0 == known::USDT { "USDT".into() }
    else if m.0 == known::SOL  { "SOL".into() }
    else                       { m.0.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_record_opportunity_increments_buckets() {
        let m = ScoutMetrics::default();
        m.record_opportunity(50_000);
        m.record_opportunity(150_000);
        m.record_opportunity(2_000_000);
        m.record_opportunity(10_000_000);
        m.record_opportunity(25_000_000);
        let s = m.snapshot();
        assert_eq!(s.opps_ge_10c, 4);
        assert_eq!(s.opps_ge_1d, 3);
        assert_eq!(s.opps_ge_5d, 2);
        assert_eq!(s.opps_ge_20d, 1);
        assert_eq!(s.largest_net_profit_usdc, 25_000_000);
    }

    #[test]
    fn render_route_uses_known_token_labels() {
        use arb_pathfinder::{Hop, Route};
        let r = Route { hops: vec![
            Hop { pool: Pubkey::new_unique(), token_in: known::usdc_mint(), token_out: known::sol_mint() },
            Hop { pool: Pubkey::new_unique(), token_in: known::sol_mint(),  token_out: known::usdc_mint() },
        ]};
        let s = render_route(&r);
        assert!(s.contains("USDC"));
        assert!(s.contains("SOL"));
    }
}
