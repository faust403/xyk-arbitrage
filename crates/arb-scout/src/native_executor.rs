//! Native arb executor — bypasses Jupiter entirely.
//!
//! When the scout finds an opportunity in a 2-hop cycle USDC → X → USDC
//! AND both pools are Raydium AMM v4, we build the swap instructions
//! directly against Raydium's program, pack them into a single Solana tx
//! with priority fee + Jito tip, sign with our wallet, and ship to Jito.
//!
//! ## Why this is faster than the Jupiter path
//!
//! Jupiter path (current):
//! - quote leg 1 (~30ms)  ← Jupiter HTTP
//! - quote leg 2 (~30ms)  ← Jupiter HTTP
//! - swap leg 1 (~100ms)  ← Jupiter HTTP
//! - swap leg 2 (~100ms)  ← Jupiter HTTP (parallelised → 100ms total)
//! - sign + send (~150ms)
//! - **total: ~280ms** before bytes hit Jito
//!
//! Native path:
//! - quote leg 1 + 2 locally (~0.01ms each — pure constant-product math)
//! - blockhash fetch (~30ms — once per tx)
//! - build msg + sign locally (~5ms)
//! - send to Jito (~50ms)
//! - **total: ~85ms**
//!
//! Cuts ~200ms off the staleness window — the largest single dent we can make
//! without colocated infra. Also eliminates the "Jupiter routes through pools
//! we don't see the inefficiency in" structural problem.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arb_core::{ArbResult, PoolKind};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::dex::openbook_market::OpenBookMarketStore;
use arb_dex_obric::{
    build_swap_x_to_y_ix as build_obric_xy_ix,
    build_swap_y_to_x_ix as build_obric_yx_ix,
    parse_pyth_price, quote_via_pyth,
    ObricSwapAccounts, ObricV2Data, OBRIC_FEE_DENOMINATOR,
};
use arb_dex_meteora::{
    build_dlmm_swap_ix, bin_array_index_for, derive_bin_array_pda,
    derive_event_authority_pda as dlmm_event_authority,
    derive_oracle_pda as dlmm_oracle_pda,
    DlmmSwapAccounts, LbPairData, METEORA_DLMM_PROGRAM,
    SPL_TOKEN_PROGRAM as DLMM_TOKEN_PROGRAM,
};
use crate::rpc::HeliusRpc;
use arb_flashloan::{
    build_usdc_flash_loan_envelope, FlashLoanEnvelope, ReserveState, SolendUsdcContext,
    build_marginfi_flash_loan, MarginFiFlashContext,
    MARGINFI_MAIN_GROUP, MARGINFI_USDC_BANK,
    derive_liquidity_vault_authority,
};
use arb_dex_orca::{swap_quote as whirlpool_swap_quote, WhirlpoolData};
use arb_dex_raydium::{
    build_clmm_swap_ix, build_swap_base_in_ix, quote_with_reserves,
    derive_tick_array_pda as clmm_tick_array_pda,
    tick_array_start_index as clmm_tick_array_start,
    ClmmFeeStore, RaydiumAmmV4Data, RaydiumClmmData, RaydiumClmmSwapAccounts,
    RaydiumSwapAccounts,
};
use arb_dex_orca::{
    build_swap_ix as build_whirlpool_swap_ix, derive_oracle_pda, derive_tick_array_pda,
    tick_array_start_index, WhirlpoolSwapAccounts,
};
use arb_jito::{JitoBundleClient, TipFloor};
use crate::alt::fetch_alt_addresses;
use crate::known::{is_jito_tip_account, HELIUS_SENDER_MIN_TIP_LAMPORTS, HELIUS_TIP_ACCOUNTS, JITO_TIP_ACCOUNTS, USDC};
use crate::raw_tx::{
    create_ata_idempotent_ix, derive_ata, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix, system_transfer_ix,
};
use base64::Engine as _;
use crate::v0_tx::build_and_sign_v0_tx_b64_multi;
use crate::tpu_send::{TpuSender, fetch_current_slot, extract_signature_base58};
use arb_pathfinder::Route;
use arb_stream::PoolStateStore;
use tokio::sync::OnceCell;
use crate::vault_store::VaultStore;
use crate::pyth_store::PythStore;

fn short_mint(pk: &Pubkey) -> String {
    let s = pk.to_string();
    if s.len() > 8 { format!("{}…", &s[..6]) } else { s }
}
use crate::wallet::Wallet;
use crate::native_buffer::OpportunityBuffer;

#[derive(Debug, Clone)]
pub struct NativeExecutorConfig {
    pub live:                  bool,
    pub min_profit_usdc:       u64,
    pub slippage_bps:          u32,
    pub max_daily_loss_usdc:   u64,
    pub jito_tip_lamports:     u64,
    pub priority_fee_micro_lamports_per_cu: u64,
    pub compute_unit_limit:    u32,
    pub min_interval:          Duration,
    /// Wrap each native bundle in a Solend flash-loan envelope. Lets the
    /// trade size scale beyond the wallet's USDC balance — required when
    /// `EXECUTOR_PROBE_AMOUNT_USDC > wallet_balance`. Read from env
    /// `FLASH_LOAN_ENABLED`. Default false.
    pub flash_loan_enabled:    bool,
    /// Pre-built Solend USDC context. `Some` only when flash loans are
    /// enabled AND every required `SOLEND_*` env var is populated.
    pub solend_ctx:            Option<SolendUsdcContext>,
    /// Solend reserve state used for the fee math. Default fee = 9 bps from
    /// `SOLEND_FLASH_LOAN_FEE_BPS`.
    pub solend_reserve_state:  ReserveState,
    /// Address Lookup Table to compress the v0 transaction. When `Some`, the
    /// addresses stored at this on-chain ALT are pulled out of the static
    /// account-keys list and referenced by 1-byte indices, dropping ~31 bytes
    /// per match. Required for flash-loan-wrapped 2-leg arbs which would
    /// otherwise exceed the 1232-byte mainnet tx limit. Read from env
    /// `EXECUTOR_NATIVE_ALT`. The address list is fetched lazily on first
    /// execute attempt and cached for the process lifetime.
    pub alt_pubkey:            Option<Pubkey>,
    /// Optional SECOND ALT, for routes that don't fit under just one ALT.
    /// Solana v0 supports up to 4 ALTs per tx. Read from `EXECUTOR_NATIVE_ALT2`.
    pub alt_pubkey2:           Option<Pubkey>,
    /// Optional THIRD ALT. `EXECUTOR_NATIVE_ALT3`.
    pub alt_pubkey3:           Option<Pubkey>,
    /// Optional FOURTH ALT (Solana v0 max). `EXECUTOR_NATIVE_ALT4`.
    pub alt_pubkey4:           Option<Pubkey>,
    /// Per-wallet MarginFi account PDA. Created once via `bin/marginfi-init`
    /// and supplied via `MARGINFI_ACCOUNT` env. `Some` plus
    /// `flash_loan_provider == "marginfi"` activates the MarginFi
    /// envelope (start_flashloan + borrow + …swaps… + deposit + end).
    pub marginfi_account:      Option<Pubkey>,
    /// USDC bank's liquidity_vault on MarginFi. Defaults to the well-known
    /// pubkey for the main group; override via `MARGINFI_USDC_LIQUIDITY_VAULT`
    /// if MarginFi migrates.
    pub marginfi_liquidity_vault: Option<Pubkey>,
    /// Flash-loan provider switch. Reads `FLASH_LOAN_PROVIDER` env:
    /// `"solend"` (default, 9 bps) or `"marginfi"` (≈0 bps).
    pub flash_loan_provider:   FlashLoanProvider,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashLoanProvider {
    Solend,
    MarginFi,
}

impl FlashLoanProvider {
    pub fn from_env() -> Self {
        match std::env::var("FLASH_LOAN_PROVIDER").as_deref() {
            Ok("marginfi") | Ok("MarginFi") => FlashLoanProvider::MarginFi,
            _ => FlashLoanProvider::Solend,
        }
    }
}

impl NativeExecutorConfig {
    /// Inherit most settings from the existing `EXECUTOR_*` env vars so the
    /// operator only manages one set of knobs.
    pub fn from_env() -> Self {
        let env_u64 = |k: &str, d: u64| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let env_u32 = |k: &str, d: u32| std::env::var(k).ok().and_then(|s| s.parse().ok()).unwrap_or(d);
        let env_pk  = |k: &str| -> Option<solana_program::pubkey::Pubkey> {
            std::env::var(k).ok()
                .and_then(|s| {
                    use std::str::FromStr;
                    solana_program::pubkey::Pubkey::from_str(s.trim()).ok()
                })
        };

        let flash_loan_enabled = std::env::var("FLASH_LOAN_ENABLED").as_deref() == Ok("true");

        // SolendUsdcContext only populated if every Solend address parses.
        // Missing/invalid → flash loans silently disabled at runtime + WARN
        // logged once on first execute attempt.
        let solend_ctx = if flash_loan_enabled {
            let lending_market           = env_pk("SOLEND_LENDING_MARKET");
            let lending_market_authority = env_pk("SOLEND_LENDING_MARKET_AUTHORITY");
            let reserve                  = env_pk("SOLEND_USDC_RESERVE");
            let reserve_liquidity_supply = env_pk("SOLEND_USDC_RESERVE_LIQUIDITY_SUPPLY");
            let fee_receiver             = env_pk("SOLEND_USDC_FEE_RECEIVER");
            let host_fee_receiver        = env_pk("SOLEND_USDC_HOST_FEE_RECEIVER")
                .or(fee_receiver);
            let pyth_oracle              = env_pk("SOLEND_USDC_PYTH_ORACLE");
            let switchboard_oracle       = env_pk("SOLEND_USDC_SWITCHBOARD_ORACLE")
                .or(pyth_oracle); // Solend reads only one oracle in practice; reuse if missing.

            match (lending_market, lending_market_authority, reserve,
                   reserve_liquidity_supply, fee_receiver, host_fee_receiver,
                   pyth_oracle, switchboard_oracle) {
                (Some(lm), Some(lma), Some(r), Some(rls), Some(fr), Some(hfr), Some(po), Some(so)) => {
                    Some(SolendUsdcContext {
                        lending_market:           lm,
                        lending_market_authority: lma,
                        reserve:                  r,
                        reserve_liquidity_supply: rls,
                        fee_receiver:             fr,
                        host_fee_receiver:        hfr,
                        pyth_oracle:              po,
                        switchboard_oracle:       so,
                    })
                }
                _ => {
                    tracing::warn!(target: "scout::native",
                        "FLASH_LOAN_ENABLED=true but one or more SOLEND_* env vars are missing/invalid; \
                         flash loan path will silently fall back to wallet-capital execution");
                    None
                }
            }
        } else {
            None
        };

        let solend_fee_bps = env_u64("SOLEND_FLASH_LOAN_FEE_BPS", 9);

        Self {
            live: std::env::var("EXECUTOR_LIVE").as_deref() == Ok("true"),
            min_profit_usdc:     env_u64("EXECUTOR_MIN_PROFIT_USDC",     500_000),
            slippage_bps:        env_u32("EXECUTOR_SLIPPAGE_BPS",        80),
            max_daily_loss_usdc: env_u64("EXECUTOR_MAX_DAILY_LOSS_USDC", 5_000_000),
            jito_tip_lamports:   env_u64("EXECUTOR_JITO_TIP_LAMPORTS",   100_000),
            priority_fee_micro_lamports_per_cu:
                                 env_u64("EXECUTOR_PRIORITY_MICRO_LAMPORTS", 10_000),
            compute_unit_limit:  env_u32("EXECUTOR_CU_LIMIT",            400_000),
            min_interval:        Duration::from_millis(
                                     env_u64("EXECUTOR_MIN_INTERVAL_MS", 1100)),
            flash_loan_enabled,
            solend_ctx,
            solend_reserve_state: ReserveState::from_fee_bps(solend_fee_bps),
            alt_pubkey:  env_pk("EXECUTOR_NATIVE_ALT"),
            alt_pubkey2: env_pk("EXECUTOR_NATIVE_ALT2"),
            alt_pubkey3: env_pk("EXECUTOR_NATIVE_ALT3"),
            alt_pubkey4: env_pk("EXECUTOR_NATIVE_ALT4"),
            marginfi_account: env_pk("MARGINFI_ACCOUNT"),
            marginfi_liquidity_vault: env_pk("MARGINFI_USDC_LIQUIDITY_VAULT"),
            flash_loan_provider: FlashLoanProvider::from_env(),
        }
    }
}

#[derive(Debug, Default)]
pub struct NativeMetrics {
    /// Routes the dispatcher checked for native eligibility (any 2-hop route).
    pub eligibility_checks:AtomicU64,
    /// Routes that PASSED the native-eligibility check (both pools Raydium AMM v4).
    pub eligibility_passed:AtomicU64,
    pub considered:        AtomicU64,
    pub skipped_throttle:  AtomicU64,
    pub skipped_not_native:AtomicU64,    // route not all Raydium AMM v4
    pub skipped_no_market: AtomicU64,    // OpenBook market not in store
    pub skipped_no_vault:  AtomicU64,    // vault balance unknown
    pub skipped_no_pool:   AtomicU64,    // pool state missing
    pub skipped_below_min: AtomicU64,
    pub skipped_dry_run:   AtomicU64,
    pub skipped_daily_cap: AtomicU64,
    pub build_errors:      AtomicU64,
    pub send_errors:       AtomicU64,
    pub sent_bundles:      AtomicU64,
    pub landed_bundles:    AtomicU64,
    pub running_net_usdc:  AtomicI64,
}

pub struct NativeExecutor {
    cfg:           NativeExecutorConfig,
    wallet:        Wallet,
    jito:          JitoBundleClient,
    pool_store:    Arc<PoolStateStore>,
    vault_store:   Arc<VaultStore>,
    pyth_store:    Arc<PythStore>,
    /// Live SOL/USD spot in micros (USD × 1_000_000), shared with the
    /// Jupiter executor's fee estimator. Drives the Obric Pyth-quote
    /// fallback when a pool's `*_price_feed` doesn't cache (SOL/USDC pools
    /// often store only the SOL/USD feed; the USDC side is null-sentinel).
    sol_price_micros: Arc<AtomicU64>,
    openbook:      Arc<OpenBookMarketStore>,
    clmm_fees:     Arc<ClmmFeeStore>,
    rpc_url:       String,
    /// Live Jito tip percentiles polled every ~15s. Drives the dynamic
    /// tip-sizing path; if `None`, falls back to `cfg.jito_tip_lamports`.
    /// Cloning is cheap (Arc-internals).
    tip_floor:     TipFloor,
    metrics:       Arc<NativeMetrics>,
    in_flight:     Mutex<()>,
    last_attempt:  Mutex<Option<Instant>>,
    /// Lazy-initialised ALT address list. `OnceCell::get_or_init` fetches the
    /// table on the first execute call; subsequent calls reuse the cached
    /// `Vec<Pubkey>`. Holds an empty vec on fetch failure so we don't retry
    /// forever and so v0 building falls through to a no-lookup tx.
    alt_addresses:  OnceCell<Vec<Pubkey>>,
    alt_addresses2: OnceCell<Vec<Pubkey>>,
    alt_addresses3: OnceCell<Vec<Pubkey>>,
    alt_addresses4: OnceCell<Vec<Pubkey>>,
    /// Opportunity ring used by the buffer-and-select scheduler. The scout
    /// dispatcher calls `submit()` on every native-eligible opportunity;
    /// the worker spawned in `spawn_worker` ticks every `min_interval` ms,
    /// picks the highest-predicted-net entry, and runs `maybe_execute` on it.
    pub buffer:    Arc<OpportunityBuffer>,
    /// Caps the number of concurrent maybe_execute tasks. Without this,
    /// the inline+fast-mode spawn-per-route pattern can pile up thousands
    /// of tokio tasks per second and OOM the box (saw this 2026-05-14:
    /// Frankfurt t3.medium fell over with ~50MB/s allocations). Default
    /// `NATIVE_MAX_CONCURRENT=16` permits in-flight; try_acquire returns
    /// instantly so over-cap calls just drop instead of queueing.
    concurrent:    Arc<tokio::sync::Semaphore>,
    /// Live wallet SOL balance in lamports, refreshed by a background
    /// poller (spawn_balance_poller). The executor reads this when sizing
    /// the Jito tip transfer: if the dynamic-computed tip exceeds our
    /// balance minus a reserve for base+priority+rent, we either clamp
    /// the tip to what we can afford or skip the send entirely. Without
    /// this gate, a tip that exceeds balance fails on-chain with
    /// SystemError::ResultWithNegativeLamports (custom 0x1) — the
    /// "insufficient liquidity in System Program" symptom we hit when the
    /// wallet drained from 1.077 → 0.000947 SOL on 2026-05-12. Zero means
    /// "not polled yet" — the gate falls through to the on-chain check.
    wallet_balance_lamports: Arc<AtomicU64>,
    /// Lazily-initialised direct-TPU sender. Populated by
    /// `init_tpu_sender` if `NATIVE_SUBMISSION_PATH=tpu`. Bypasses
    /// Helius/Jito relays for ~50ms latency win on outbound.
    tpu_sender: Arc<tokio::sync::OnceCell<Arc<TpuSender>>>,
}

impl NativeExecutor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg:         NativeExecutorConfig,
        wallet:      Wallet,
        pool_store:  Arc<PoolStateStore>,
        vault_store: Arc<VaultStore>,
        pyth_store:  Arc<PythStore>,
        sol_price_micros: Arc<AtomicU64>,
        openbook:    Arc<OpenBookMarketStore>,
        clmm_fees:   Arc<ClmmFeeStore>,
        rpc_url:     String,
        tip_floor:   TipFloor,
    ) -> Self {
        // Buffer max_age policy — the buffer's purpose is to give the worker
        // a SHORT window of freshly-arrived opportunities to pick the best
        // from. With executor-decides dispatch v38+ the buffer fills at
        // 700+/s (every native-eligible route on every update), so an over-
        // generous max_age means picking from STALE pool snapshots — the
        // very thing arb breaks on.
        //
        // Default = 1× min_interval (was 1.5× before v40). Operator can
        // tighten further via `NATIVE_BUFFER_MAX_AGE_MS` (e.g. 30 = pick
        // only entries less than 30ms old; smaller window, fresher state).
        let max_age_override_ms = std::env::var("NATIVE_BUFFER_MAX_AGE_MS")
            .ok().and_then(|s| s.parse::<u64>().ok());
        let buffer_max_age = match max_age_override_ms {
            Some(ms) => Duration::from_millis(ms),
            None     => cfg.min_interval,
        };
        // Capacity tunable via `NATIVE_BUFFER_CAPACITY` (default 256). Larger
        // capacity reduces overflow drops when the firehose outruns the
        // worker, but does not reduce stale drops (those depend on max_age).
        let buffer_capacity: usize = std::env::var("NATIVE_BUFFER_CAPACITY")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(256);
        let buffer = Arc::new(OpportunityBuffer::new(buffer_max_age, buffer_capacity));
        Self {
            cfg, wallet,
            jito:         JitoBundleClient::default(),
            pool_store, vault_store, pyth_store,
            sol_price_micros,
            openbook, clmm_fees, rpc_url,
            tip_floor,
            metrics:      Arc::new(NativeMetrics::default()),
            in_flight:    Mutex::new(()),
            last_attempt: Mutex::new(None),
            alt_addresses:  OnceCell::new(),
            alt_addresses2: OnceCell::new(),
            alt_addresses3: OnceCell::new(),
            alt_addresses4: OnceCell::new(),
            buffer,
            concurrent: Arc::new(tokio::sync::Semaphore::new(
                std::env::var("NATIVE_MAX_CONCURRENT")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(16)
            )),
            wallet_balance_lamports: Arc::new(AtomicU64::new(0)),
            tpu_sender: Arc::new(tokio::sync::OnceCell::new()),
        }
    }

    /// Initialise the direct-TPU sender + spawn its cache refresher.
    /// Call once from scout.rs after `Arc::new(executor)`. No-op when
    /// `NATIVE_SUBMISSION_PATH` is anything other than `tpu` so we don't
    /// pay the 60s/getClusterNodes RPC cost when the path isn't used.
    /// Synchronous wrapper that spawns the actual async setup in a
    /// background task — fits the existing scout.rs sync init flow.
    pub fn init_tpu_sender(self: &Arc<Self>) {
        if std::env::var("NATIVE_SUBMISSION_PATH").as_deref() != Ok("tpu") {
            return;
        }
        let rpc_url = self.rpc_url.clone();
        let cell = Arc::clone(&self.tpu_sender);
        tokio::spawn(async move {
            match TpuSender::new(rpc_url).await {
                Ok(sender) => {
                    let arc = Arc::new(sender);
                    arc.spawn_refresher();
                    if cell.set(arc).is_err() {
                        warn!(target: "scout::native", "TpuSender already initialised");
                    }
                    info!(target: "scout::native",
                          "TpuSender initialised — direct UDP send to leader TPUs");
                }
                Err(e) => {
                    warn!(target: "scout::native", error = %e,
                          "TpuSender::new failed — direct TPU send will be unavailable");
                }
            }
        });
    }

    /// Spawn a background task that polls the payer's SOL balance every
    /// 5 s and stores it on the executor for the tip-vs-balance gate.
    /// Must be called once after wrapping the executor in `Arc::new(...)`.
    /// First poll fires immediately so the gate is hot by the time the
    /// scout's first opportunity arrives.
    pub fn spawn_balance_poller(self: &Arc<Self>) {
        let bal = self.wallet_balance_lamports.clone();
        let rpc_url = self.rpc_url.clone();
        let payer = self.wallet.pubkey();
        tokio::spawn(async move {
            let rpc = crate::rpc::HeliusRpc::new(rpc_url);
            loop {
                match rpc.get_balance(&payer).await {
                    Ok(b) => {
                        let prev = bal.swap(b, Ordering::Relaxed);
                        if prev == 0 || prev.abs_diff(b) > 50_000_000 {
                            info!(target: "scout::native",
                                  balance_lamports = b,
                                  prev_lamports = prev,
                                  "wallet balance poller update");
                        }
                    }
                    Err(e) => {
                        warn!(target: "scout::native", error = %e,
                              "wallet balance poll failed — keeping prev value");
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    /// Submit an opportunity to the worker queue. The dispatcher should call
    /// this for every native-eligible 2-hop route — we no longer per-call
    /// throttle. The buffer holds entries until the worker's next tick
    /// (every `min_interval` ms), at which point the highest-predicted-net
    /// entry is fired and the rest are discarded.
    ///
    /// `predicted_net_usdc` should be the scout's net-profit estimate; the
    /// worker uses it as the selection key.
    pub fn submit(&self, route: Route, probe_usdc: u64, predicted_net_usdc: i64) {
        self.buffer.submit(route, probe_usdc, predicted_net_usdc);
    }

    /// Spawn the buffer worker task. Call once, after `Arc::new(executor)`,
    /// passing the Arc so the task can hold a strong reference for the
    /// process lifetime.
    ///
    /// The worker:
    /// 1. Sleeps for `min_interval`.
    /// 2. Calls `buffer.pick_best()`. If `None`, loops.
    /// 3. Calls `self.maybe_execute(route, probe)`. Errors are logged but
    ///    don't kill the worker — next tick continues.
    pub fn spawn_worker(self: Arc<Self>) {
        let interval = self.cfg.min_interval;
        info!(target: "scout::native",
              interval_ms = interval.as_millis() as u64,
              max_age_ms  = self.buffer.max_age.as_millis() as u64,
              max_capacity = self.buffer.max_capacity,
              "native buffer worker started");
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let opp = match self.buffer.pick_best() {
                    Some(o) => o,
                    None => continue,
                };
                if let Err(e) = self.maybe_execute(&opp.route, opp.probe_usdc).await {
                    warn!(target: "scout::native",
                          predicted_net_usdc = opp.predicted_net_usdc,
                          probe_usdc = opp.probe_usdc,
                          error = %e,
                          "buffer worker: maybe_execute failed");
                }
            }
        });
    }

    /// Resolve the ALT addresses for v0 compression. Lazy-fetched from the
    /// chain on first call; cached for the process lifetime. Returns an
    /// empty slice when no `EXECUTOR_NATIVE_ALT` is configured OR when the
    /// fetch fails — in either case the v0 builder falls through to a
    /// no-lookup layout (same wire size as legacy + 1 version byte).
    async fn resolve_alt(&self) -> &[Pubkey] {
        let alt_pk = match self.cfg.alt_pubkey {
            Some(pk) => pk,
            None => return &[],
        };
        let cached = self.alt_addresses.get_or_init(|| async {
            match fetch_alt_addresses(&self.rpc_url, &alt_pk).await {
                Ok(addrs) => {
                    info!(target: "scout::native",
                          alt = %alt_pk,
                          addresses = addrs.len(),
                          "ALT loaded — v0 tx will compress these addresses");
                    addrs
                }
                Err(e) => {
                    warn!(target: "scout::native",
                          alt = %alt_pk, error = %e,
                          "failed to fetch ALT — v0 tx will fall back to no-lookup layout");
                    Vec::new()
                }
            }
        }).await;
        cached.as_slice()
    }

    /// Resolve up to 4 ALTs (Solana v0 max per tx). Each is lazily fetched
    /// + cached. Returns the list in the form expected by
    /// `build_and_sign_v0_tx_b64_multi`.
    async fn resolve_all_alts(&self) -> Vec<(Pubkey, Vec<Pubkey>)> {
        let mut out: Vec<(Pubkey, Vec<Pubkey>)> = Vec::with_capacity(4);
        let entries = [
            (self.cfg.alt_pubkey,  &self.alt_addresses,  "ALT1"),
            (self.cfg.alt_pubkey2, &self.alt_addresses2, "ALT2"),
            (self.cfg.alt_pubkey3, &self.alt_addresses3, "ALT3"),
            (self.cfg.alt_pubkey4, &self.alt_addresses4, "ALT4"),
        ];
        for (maybe_pk, cell, name) in entries {
            if let Some(pk) = maybe_pk {
                let addrs = cell.get_or_init(|| async {
                    match fetch_alt_addresses(&self.rpc_url, &pk).await {
                        Ok(a) => {
                            info!(target: "scout::native", alt = %pk, addresses = a.len(),
                                  "{name} loaded");
                            a
                        }
                        Err(e) => {
                            warn!(target: "scout::native", alt = %pk, error = %e,
                                  "failed to fetch {name}");
                            Vec::new()
                        }
                    }
                }).await;
                if !addrs.is_empty() { out.push((pk, addrs.clone())); }
            }
        }
        out
    }

    pub fn metrics(&self) -> Arc<NativeMetrics> { Arc::clone(&self.metrics) }

    /// Returns true iff every hop is a DEX we can natively execute:
    /// Raydium AMM v4, Orca Whirlpool, or Raydium CLMM.
    pub fn is_native_eligible(&self, route: &Route) -> bool {
        let n = self.metrics.eligibility_checks.fetch_add(1, Ordering::Relaxed);
        // 2-hop (USDC→A→USDC) and 3-hop (USDC→A→B→USDC) cycles. v41 keeps
        // 3-hop on by default so the enhanced per-leg `🔬 below_min sample`
        // log can surface which middle hop is bleeding value (the bug
        // hypothesis: quoter direction or mint-decimal scaling on the
        // middle non-USDC→non-USDC swap). 4+ hops overflow the 1232-byte
        // v0 tx budget even with ALT compression.
        let max_hops: usize = std::env::var("NATIVE_MAX_HOPS").ok()
            .and_then(|s| s.parse().ok()).unwrap_or(3);
        if route.hops.len() < 2 || route.hops.len() > max_hops {
            if n < 10 {
                tracing::info!(target: "scout::native",
                    n_hops = route.hops.len(),
                    max_hops,
                    "diag: route rejected — hop count out of bounds");
            }
            return false;
        }
        // All four kinds eligible by default; env knobs can disable each.
        // Comments preserved on disabled-kind caveats:
        //   * Raydium AMM v4 + another DEX may overflow 1232-byte v0 limit.
        //   * Obric V2's quoter is constant-product, inflated 10-100x; live
        //     swaps revert when leg N's output falls short of min_amount_out.
        let amm_v4_disabled = std::env::var("NATIVE_DISABLE_RAYDIUM_AMM_V4")
            .as_deref() == Ok("true");
        let obric_disabled = std::env::var("NATIVE_DISABLE_OBRIC")
            .as_deref() == Ok("true");
        let dlmm_disabled = std::env::var("NATIVE_DISABLE_METEORA_DLMM")
            .as_deref() == Ok("true");
        let kind_ok = |k: Option<PoolKind>| match k {
            Some(PoolKind::OrcaWhirlpool)
            | Some(PoolKind::RaydiumClmm) => true,
            Some(PoolKind::RaydiumAmmV4) => !amm_v4_disabled,
            Some(PoolKind::ObricV2)      => !obric_disabled,
            Some(PoolKind::MeteoraDlmm)  => !dlmm_disabled,
            _ => false,
        };
        // Every hop must pass kind_ok. Snapshot the kinds for the diag log
        // before walking so the rejection message lists all of them.
        let kinds: Vec<Option<PoolKind>> = route.hops.iter()
            .map(|h| self.pool_store.get(&h.pool).map(|p| p.kind))
            .collect();
        if !kinds.iter().copied().all(kind_ok) {
            if n < 30 {
                let kind_strs: Vec<String> = kinds.iter()
                    .map(|k| format!("{k:?}"))
                    .collect();
                tracing::info!(target: "scout::native",
                    n_hops = route.hops.len(),
                    kinds = ?kind_strs,
                    "diag: route rejected — kind not native-eligible");
            }
            return false;
        }
        self.metrics.eligibility_passed.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Build a single leg's quote + swap instruction for whichever DEX the
    /// pool belongs to. Returns `(quoted_out, min_out, swap_instruction)`.
    /// The caller decides what `amount_in` to pass: for leg 1 it's the user's
    /// probe USDC; for leg 2 it's leg 1's *floored* output (so we know our
    /// SPL token balance will cover the swap).
    async fn build_leg(
        &self,
        pool_addr:    Pubkey,
        input_mint:   Pubkey,
        amount_in:    u64,
        payer:        &Pubkey,
        user_src_ata: Pubkey,
        user_dst_ata: Pubkey,
    ) -> ArbResult<(u64, u64, Instruction)> {
        let pool = self.pool_store.get(&pool_addr)
            .ok_or(arb_core::ArbError::Invariant("native: pool missing from store"))?;
        match pool.kind {
            PoolKind::RaydiumAmmV4 => {
                let parsed = RaydiumAmmV4Data::try_parse(pool.state.as_slice())
                    .ok_or(arb_core::ArbError::Invariant("native: raydium pool parse failed"))?;
                let market = self.openbook.get(&parsed.market_id)
                    .ok_or(arb_core::ArbError::Invariant("native: openbook market not in store"))?;
                let raw_quoted = self.local_quote(&parsed, input_mint, amount_in)?;
                // Haircut to absorb pool-state movement between quote-time
                // and chain-execution time. State moves in tens of ms even
                // for AmmV4. Env: NATIVE_AMM_QUOTE_HAIRCUT_BPS (default 50).
                let haircut: u64 = std::env::var("NATIVE_AMM_QUOTE_HAIRCUT_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(50);
                let quoted = (raw_quoted as u128 * (10_000 - haircut as u128) / 10_000) as u64;
                let min_out = apply_slippage(quoted, self.cfg.slippage_bps);
                let accs = swap_accounts(
                    pool_addr, parsed.market_id,
                    &parsed, &market, payer, user_src_ata, user_dst_ata,
                );
                let ix = build_swap_base_in_ix(&accs, amount_in, min_out)?;
                Ok((quoted, min_out, ix))
            }
            PoolKind::OrcaWhirlpool => {
                let parsed = WhirlpoolData::try_parse(pool.state.as_slice())
                    .ok_or(arb_core::ArbError::Invariant("native: whirlpool parse failed"))?;
                let a_to_b = input_mint == parsed.token_mint_a;
                let raw_quoted = whirlpool_swap_quote(
                    amount_in, parsed.sqrt_price, parsed.liquidity,
                    parsed.fee_rate, a_to_b, parsed.tick_spacing,
                ).map_err(|e| match e {
                    arb_core::ArbError::InsufficientLiquidity { amount, .. } =>
                        arb_core::ArbError::InsufficientLiquidity { pool: pool_addr, amount },
                    other => other,
                })?;
                // Haircut to absorb tick movement between quote and execution.
                // Env: NATIVE_CLMM_QUOTE_HAIRCUT_BPS (default 50, shared with CLMM).
                let haircut: u64 = std::env::var("NATIVE_CLMM_QUOTE_HAIRCUT_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(50);
                let quoted = (raw_quoted as u128 * (10_000 - haircut as u128) / 10_000) as u64;
                let min_out = apply_slippage(quoted, self.cfg.slippage_bps);
                // user_token_a is the wallet ATA for token_mint_a regardless
                // of swap direction; map src/dst based on direction.
                let (user_token_a, user_token_b) = if a_to_b {
                    (user_src_ata, user_dst_ata)
                } else {
                    (user_dst_ata, user_src_ata)
                };
                // Tick-array sequence: Whirlpool's swap walks tick_array_0
                // → _1 → _2 in the direction of the swap. Each array covers
                // 88 × tick_spacing ticks. The on-chain program rejects
                // (`TickArraySequenceInvalidIndex` 6038) if _1's start is
                // not exactly one span away from _0 in the swap direction,
                // and _2 likewise from _1. Direction:
                //   * a→b ⇒ price decreasing ⇒ ticks DECREASING
                //   * b→a ⇒ price increasing ⇒ ticks INCREASING
                let span = 88_i32 * parsed.tick_spacing as i32;
                let start_0 = tick_array_start_index(parsed.tick_current_index, parsed.tick_spacing);
                let (start_1, start_2) = if a_to_b {
                    (start_0 - span, start_0 - 2 * span)
                } else {
                    (start_0 + span, start_0 + 2 * span)
                };
                let ta_0 = derive_tick_array_pda(&pool_addr, start_0);
                let ta_1 = derive_tick_array_pda(&pool_addr, start_1);
                let ta_2 = derive_tick_array_pda(&pool_addr, start_2);
                let oracle = derive_oracle_pda(&pool_addr);
                let accs = WhirlpoolSwapAccounts {
                    whirlpool:     pool_addr,
                    token_vault_a: parsed.token_vault_a,
                    token_vault_b: parsed.token_vault_b,
                    user_token_a, user_token_b,
                    tick_array_0: ta_0,
                    tick_array_1: ta_1,
                    tick_array_2: ta_2,
                    oracle,
                    user_owner: *payer,
                };
                let ix = build_whirlpool_swap_ix(&accs, amount_in, min_out, a_to_b)?;
                Ok((quoted, min_out, ix))
            }
            PoolKind::RaydiumClmm => {
                let parsed = RaydiumClmmData::try_parse(pool.state.as_slice())
                    .ok_or(arb_core::ArbError::Invariant("native: clmm parse failed"))?;
                let fee = self.clmm_fees.get(&pool_addr)
                    .ok_or(arb_core::ArbError::Invariant("native: clmm fee not in store"))?;
                let zero_for_one = input_mint == parsed.token_mint_0;
                let raw_quoted = arb_dex_raydium::swap_quote(
                    amount_in, parsed.sqrt_price_x64, parsed.liquidity,
                    fee, zero_for_one, parsed.tick_spacing,
                ).map_err(|e| match e {
                    arb_core::ArbError::InsufficientLiquidity { amount, .. } =>
                        arb_core::ArbError::InsufficientLiquidity { pool: pool_addr, amount },
                    other => other,
                })?;
                // Haircut to absorb tick movement between quote and execution.
                let haircut: u64 = std::env::var("NATIVE_CLMM_QUOTE_HAIRCUT_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(50);
                let quoted = (raw_quoted as u128 * (10_000 - haircut as u128) / 10_000) as u64;
                let min_out = apply_slippage(quoted, self.cfg.slippage_bps);
                // CLMM uses input/output naming in the instruction itself, so
                // we pass the user ATA + vault that match the swap direction.
                let (input_vault, output_vault) = if zero_for_one {
                    (parsed.token_vault_0, parsed.token_vault_1)
                } else {
                    (parsed.token_vault_1, parsed.token_vault_0)
                };
                // user_src/user_dst ATAs are already resolved by caller.
                let start_0 = clmm_tick_array_start(parsed.tick_current, parsed.tick_spacing);
                let tick_array = clmm_tick_array_pda(&pool_addr, start_0);
                // Pass 2 adjacent tick_arrays so the swap can cross at least
                // one boundary without reverting with NotEnoughTickArrayAccount
                // (Custom 6027). Direction: zero_for_one=true → price ↓ →
                // next is at LOWER tick; zero_for_one=false → price ↑ →
                // next is at HIGHER tick.
                let span = (arb_dex_raydium::TICK_ARRAY_SIZE as i32)
                    .saturating_mul(parsed.tick_spacing as i32);
                let (next_start, after_start) = if zero_for_one {
                    (start_0.saturating_sub(span), start_0.saturating_sub(span.saturating_mul(2)))
                } else {
                    (start_0.saturating_add(span), start_0.saturating_add(span.saturating_mul(2)))
                };
                let accs = RaydiumClmmSwapAccounts {
                    payer:                *payer,
                    amm_config:           parsed.amm_config,
                    pool_state:           pool_addr,
                    input_token_account:  user_src_ata,
                    output_token_account: user_dst_ata,
                    input_vault, output_vault,
                    observation_state:    parsed.observation_key,
                    tick_array,
                    tick_array_next:      Some(clmm_tick_array_pda(&pool_addr, next_start)),
                    tick_array_after:     Some(clmm_tick_array_pda(&pool_addr, after_start)),
                };
                let ix = build_clmm_swap_ix(&accs, amount_in, min_out, zero_for_one)?;
                Ok((quoted, min_out, ix))
            }
            PoolKind::ObricV2 => {
                let parsed = ObricV2Data::try_parse(pool.state.as_slice())
                    .ok_or(arb_core::ArbError::Invariant("native: obric pool parse failed"))?;
                let _reserve_x = self.vault_store.get(&parsed.reserve_x_vault)
                    .ok_or(arb_core::ArbError::Invariant("obric: reserve_x_vault unknown"))?;
                let _reserve_y = self.vault_store.get(&parsed.reserve_y_vault)
                    .ok_or(arb_core::ArbError::Invariant("obric: reserve_y_vault unknown"))?;

                let x_to_y = input_mint == parsed.mint_x;

                // Pyth-based quote: Obric is a Pyth-anchored PMM, so its
                // output ≈ Pyth price × (1 − fee). For SOL/USDC pools Obric
                // commonly publishes only the SOL feed and treats USDC as
                // a dollar-pegged constant ($1). When a side's Pyth feed
                // isn't cached, fall back to USDC=$1 if the corresponding
                // mint is USDC, otherwise reject.
                let load_pyth = |feed_pk: &Pubkey, mint: &Pubkey|
                    -> ArbResult<arb_dex_obric::PythPrice>
                {
                    if let Some(data) = self.pyth_store.get(feed_pk) {
                        if let Some(p) = parse_pyth_price(&data) {
                            return Ok(p);
                        }
                    }
                    if *mint == USDC || *mint == crate::known::USDT {
                        // Dollar-pegged: synthetic Pyth at $1 with exp=-8.
                        return Ok(arb_dex_obric::PythPrice {
                            price: 100_000_000, exponent: -8, trading: true,
                        });
                    }
                    if *mint == crate::known::SOL {
                        // SOL: use the live spot price the scout already
                        // tracks via Whirlpool. sol_price_micros = USD × 1e6
                        // (e.g., $93.21 → 93_210_000). Map to Pyth scale
                        // exp=-6 so price field is the integer micros.
                        let micros = self.sol_price_micros.load(Ordering::Relaxed);
                        if micros > 0 {
                            return Ok(arb_dex_obric::PythPrice {
                                price: micros as i64, exponent: -6, trading: true,
                            });
                        }
                    }
                    Err(arb_core::ArbError::Invariant(
                        "obric: price feed not cached and mint isn't a known pegged token"))
                };
                let x_pyth = load_pyth(&parsed.x_price_feed, &parsed.mint_x)?;
                let y_pyth = load_pyth(&parsed.y_price_feed, &parsed.mint_y)?;
                let x_dec = decimals_for_mint(&parsed.mint_x)
                    .ok_or(arb_core::ArbError::Invariant("obric: unknown x mint decimals"))?;
                let y_dec = decimals_for_mint(&parsed.mint_y)
                    .ok_or(arb_core::ArbError::Invariant("obric: unknown y mint decimals"))?;

                let raw_quoted = quote_via_pyth(
                    amount_in, x_to_y, x_pyth, y_pyth, x_dec, y_dec,
                    parsed.fee_millionth,
                ).ok_or(arb_core::ArbError::Invariant("obric: pyth quote failed"))?;
                // Obric V2's quote_via_pyth approximates on-chain output via
                // pure Pyth × (1 − fee). The actual PMM curve (with internal
                // concentration math + protocol fee accounting) consistently
                // delivers slightly less. Live evidence 2026-05-15 19:42-19:46:
                // 1008 of 1116 reverts (90%) were Obric AnchorError::Rejected
                // (6028) at swap_ixs.rs:331 = the min_amount_out gate, even
                // with 200bps slip.
                //
                // FIX: haircut the quote at the SOURCE so it propagates to
                // both the profit-calc (route's net is computed conservatively)
                // AND the next leg's amount_in (we don't over-promise). The
                // executor's min_out gate then computes 200bps off the
                // HAIRCUT value — which the chain actually delivers.
                //
                // Env override: NATIVE_OBRIC_QUOTE_HAIRCUT_BPS (default 300 = 3%).
                let obric_haircut_bps: u32 = std::env::var("NATIVE_OBRIC_QUOTE_HAIRCUT_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(300);
                let quoted = (raw_quoted as u128 * (10_000 - obric_haircut_bps as u128) / 10_000) as u64;
                let obric_slippage_bps: u32 = std::env::var("NATIVE_OBRIC_SLIPPAGE_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(200);
                let effective_slip = obric_slippage_bps.max(self.cfg.slippage_bps);
                let min_out = apply_slippage(quoted, effective_slip);

                // Map user ATAs by swap direction. user_src always holds
                // input_mint; user_dst holds the other side. So the X/Y
                // assignment depends on whether we're swapping x→y or y→x.
                let (user_token_account_x, user_token_account_y) = if x_to_y {
                    (user_src_ata, user_dst_ata)
                } else {
                    (user_dst_ata, user_src_ata)
                };

                let accs = ObricSwapAccounts {
                    trading_pair: pool_addr,
                    mint_x:                  parsed.mint_x,
                    mint_y:                  parsed.mint_y,
                    reserve_x:               parsed.reserve_x_vault,
                    reserve_y:               parsed.reserve_y_vault,
                    user_token_account_x,
                    user_token_account_y,
                    protocol_fee_x:          parsed.protocol_fee_x,
                    protocol_fee_y:          parsed.protocol_fee_y,
                    slot2_account:           parsed.slot2_account,
                    x_price_feed:            parsed.x_price_feed,
                    y_price_feed:            parsed.y_price_feed,
                    user:                    *payer,
                };
                let ix = if x_to_y {
                    build_obric_xy_ix(&accs, amount_in, min_out)?
                } else {
                    build_obric_yx_ix(&accs, amount_in, min_out)?
                };
                Ok((quoted, min_out, ix))
            }
            PoolKind::MeteoraDlmm => {
                // 1) Parse the on-chain LbPair account → reserves, mints,
                //    active_id, fee parameters. `parsed` is a Copy struct
                //    so we can drop the dashmap guard immediately after.
                let parsed = LbPairData::try_parse(pool.state.as_slice())
                    .ok_or(arb_core::ArbError::Invariant("native: dlmm pool parse failed"))?;
                drop(pool); // release the pool_store guard before any .await

                // 2) Local spot quote — same approximation used by the scout.
                //    Use the bare `spot_quote` function so we don't need the
                //    Pool reference (which we just dropped to keep `Send`
                //    futures across the upcoming await).
                let dec_x = arb_dex_meteora::known_decimals(&parsed.token_x_mint)
                    .unwrap_or(6);
                let dec_y = arb_dex_meteora::known_decimals(&parsed.token_y_mint)
                    .unwrap_or(6);
                let direction_x_to_y = input_mint == parsed.token_x_mint;
                let raw_quoted = arb_dex_meteora::spot_quote(
                    amount_in,
                    parsed.bin_step,
                    parsed.active_id,
                    parsed.base_factor,
                    parsed.base_fee_power_factor,
                    direction_x_to_y,
                    dec_x, dec_y,
                )?;
                // DLMM uses bin-based liquidity; `spot_quote` returns the
                // optimistic per-bin output without modeling bin-crossing
                // costs. Live evidence 2026-05-15 21:00: 100% of 50 reverts
                // were AnchorError::ExceededAmountSlippageTolerance (Custom
                // 6003) from program LBUZKhRxPF... = DLMM (swap.rs:219).
                // Apply haircut to match what the on-chain bin walk actually
                // delivers. Env: NATIVE_DLMM_QUOTE_HAIRCUT_BPS (default 150).
                let haircut: u64 = std::env::var("NATIVE_DLMM_QUOTE_HAIRCUT_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(150);
                let quoted = (raw_quoted as u128 * (10_000 - haircut as u128) / 10_000) as u64;
                let min_out = apply_slippage(quoted, self.cfg.slippage_bps);

                // 3) user_src already holds input_mint; user_dst the output.
                let user_token_in  = user_src_ata;
                let user_token_out = user_dst_ata;

                // 4) Derive the bin_array PDAs around the active bin. A
                //    swap that crosses bins may consume liquidity in
                //    arrays adjacent to the active one; fetch ±1 to cover
                //    typical-size swaps. Each array holds 70 bins, so 3
                //    arrays = 210 bins of liquidity in scope.
                let active_idx = bin_array_index_for(parsed.active_id);
                let candidate_indices = [active_idx - 1, active_idx, active_idx + 1];
                let candidate_pdas: Vec<Pubkey> = candidate_indices.iter()
                    .map(|i| derive_bin_array_pda(&pool_addr, *i))
                    .collect();

                // 5) Fetch all 3 in one batch RPC call. Filter to those
                //    that exist on chain (non-existent BinArray accounts
                //    cause `AccountNotInitialized` aborts, so we drop them).
                let helius = HeliusRpc::new(self.rpc_url.clone());
                let snaps = helius.get_multiple_accounts(&candidate_pdas).await?;
                let bin_arrays: Vec<Pubkey> = candidate_pdas.iter().zip(snaps.iter())
                    .filter_map(|(pda, snap)| snap.as_ref().map(|_| *pda))
                    .collect();
                if bin_arrays.is_empty() {
                    return Err(arb_core::ArbError::Invariant(
                        "dlmm: no bin_arrays found around active bin — pool may be empty"));
                }

                // 6) Derive shared PDAs (oracle, event_authority).
                //    bitmap_extension is optional — we pass the program id
                //    as the Anchor None-sentinel rather than fetching it.
                let oracle = dlmm_oracle_pda(&pool_addr);
                let event_authority = dlmm_event_authority();

                // 7) Build the instruction.
                let accs = DlmmSwapAccounts {
                    lb_pair:                    pool_addr,
                    bin_array_bitmap_extension: METEORA_DLMM_PROGRAM, // Option::None sentinel
                    reserve_x:                  parsed.reserve_x,
                    reserve_y:                  parsed.reserve_y,
                    user_token_in,
                    user_token_out,
                    token_x_mint:               parsed.token_x_mint,
                    token_y_mint:               parsed.token_y_mint,
                    oracle,
                    host_fee_in:                METEORA_DLMM_PROGRAM, // no host fee
                    user:                       *payer,
                    token_x_program:            DLMM_TOKEN_PROGRAM,
                    token_y_program:            DLMM_TOKEN_PROGRAM,
                    event_authority,
                    bin_arrays,
                };
                let ix = build_dlmm_swap_ix(&accs, amount_in, min_out)?;
                Ok((quoted, min_out, ix))
            }
            _ => Err(arb_core::ArbError::Invariant("native: unsupported pool kind")),
        }
    }

    /// Try to execute `route` natively. Caller is responsible for ensuring
    /// `is_native_eligible(route) == true`.
    pub async fn maybe_execute(&self, route: &Route, probe_usdc: u64) -> ArbResult<()> {
        // Phase 6p: concurrency cap. If 16 (default) tasks are already
        // running maybe_execute, drop this attempt immediately so we don't
        // pile up spawn-per-route tasks. This was the OOM root cause on
        // Frankfurt 2026-05-14.
        let _permit = match self.concurrent.try_acquire() {
            Ok(p) => p,
            Err(_) => {
                self.metrics.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        };
        // CAP the trade size at `EXECUTOR_PROBE_AMOUNT_USDC`. When flash loan
        // is going to be active (2-hop with `flash_loan_enabled`) the cap is
        // the borrow size — MarginFi can lend any amount. When flash will be
        // INACTIVE (3-hop, or flash disabled, or env not configured), the cap
        // must respect the wallet's USDC balance — otherwise leg-1 fails on
        // SPL-Token Custom(1) "insufficient funds".
        //
        // `NATIVE_NO_FLASH_PROBE_USDC` (default $100) is the no-flash cap.
        // Operator should set this to ≤ wallet's USDC balance.
        let cap_env = std::env::var("EXECUTOR_PROBE_AMOUNT_USDC").ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(500_000_000);
        let no_flash_cap = std::env::var("NATIVE_NO_FLASH_PROBE_USDC").ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(100_000_000);
        let n_hops = route.hops.len();
        let will_flash = self.cfg.flash_loan_enabled && n_hops == 2;
        let effective_cap = if will_flash { cap_env } else { cap_env.min(no_flash_cap) };
        let probe_usdc = probe_usdc.min(effective_cap);

        // v56: smart per-route probe sizing. v51 capped only the FIRST hop;
        // v56 walks ALL legs and computes a per-leg max-safe input, converted
        // back to USDC equivalent via the leg's spot price. Take the MIN
        // across all legs — the route's shallowest bottleneck determines
        // the probe.
        //
        // Math: cumulative_rate = "tokens of current mint per 1 USDC entered
        // at leg 0". After each leg we multiply by that leg's spot price.
        // Each leg's max_safe is in its INPUT token; divide by cumulative
        // rate to get USDC equivalent.
        //
        // Deep pools (RaydiumAmmV4 / Obric / DLMM) return u64::MAX from the
        // helper ⇒ no constraint from that leg.
        let dyn_max_ticks: i32 = std::env::var("NATIVE_DYNAMIC_PROBE_MAX_TICKS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(2);
        let smart_probe: u64 = {
            let q64 = 18_446_744_073_709_551_616.0_f64;
            let mut cap_usdc: u64 = u64::MAX;
            let mut cumulative_rate: f64 = 1.0;
            let mut current_mint = crate::known::USDC;
            for hop in route.hops.iter() {
                let Some(pool) = self.pool_store.get(&hop.pool) else { break };
                let (leg_max_in, leg_spot): (u64, f64) = match pool.kind {
                    PoolKind::OrcaWhirlpool => {
                        if let Some(p) = WhirlpoolData::try_parse(pool.state.as_slice()) {
                            let a_to_b = current_mint == p.token_mint_a;
                            let max_in = arb_dex_orca::max_safe_input(
                                p.sqrt_price, p.liquidity, a_to_b,
                                p.tick_spacing, dyn_max_ticks,
                            );
                            let raw_price = (p.sqrt_price as f64 / q64).powi(2);
                            let spot = if a_to_b { raw_price } else if raw_price > 0.0 { 1.0 / raw_price } else { 1.0 };
                            (max_in, spot)
                        } else { (u64::MAX, 1.0) }
                    }
                    PoolKind::RaydiumClmm => {
                        if let Some(p) = arb_dex_raydium::RaydiumClmmData::try_parse(pool.state.as_slice()) {
                            let z0_to_1 = current_mint == p.token_mint_0;
                            let max_in = arb_dex_raydium::clmm::max_safe_input(
                                p.sqrt_price_x64, p.liquidity, z0_to_1,
                                p.tick_spacing, dyn_max_ticks,
                            );
                            let raw_price = (p.sqrt_price_x64 as f64 / q64).powi(2);
                            let spot = if z0_to_1 { raw_price } else if raw_price > 0.0 { 1.0 / raw_price } else { 1.0 };
                            (max_in, spot)
                        } else { (u64::MAX, 1.0) }
                    }
                    _ => (u64::MAX, 1.0),
                };
                if leg_max_in < u64::MAX && cumulative_rate > 0.0 {
                    let usdc_eq_f = leg_max_in as f64 / cumulative_rate;
                    if usdc_eq_f.is_finite() && usdc_eq_f > 0.0 {
                        let usdc_eq = if usdc_eq_f >= u64::MAX as f64 {
                            u64::MAX
                        } else {
                            usdc_eq_f as u64
                        };
                        cap_usdc = cap_usdc.min(usdc_eq);
                    }
                }
                cumulative_rate *= leg_spot;
                current_mint = hop.token_out.0;
            }
            cap_usdc
        };
        // Floor at $1 so the smart cap never zeroes out the route. Operator
        // can lower with `NATIVE_DYNAMIC_PROBE_MIN_USDC`.
        let dyn_min: u64 = std::env::var("NATIVE_DYNAMIC_PROBE_MIN_USDC")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000);
        let probe_usdc = probe_usdc.min(smart_probe.max(dyn_min));

        self.metrics.considered.fetch_add(1, Ordering::Relaxed);

        // Phase 6p: `NATIVE_FAST_MODE=true` (default ON) collapses the early
        // gates so that detection → build_leg → send happens as one tight
        // sequence. Operator preference — every extra mutex/atomic compare
        // shaves microseconds that compound across 100s of routes per slot.
        // The gates skipped here are defence-in-depth; the real safety is
        // `min_amount_out` enforced on-chain by the swap instructions
        // themselves. Falls back to legacy gating when false.
        let fast_mode = std::env::var("NATIVE_FAST_MODE")
            .as_deref() != Ok("false");

        if !fast_mode {
            // Throttle gate (legacy): mutex on last_attempt.
            {
                let mut last = self.last_attempt.lock().await;
                let now = Instant::now();
                if let Some(t) = *last {
                    if now.duration_since(t) < Duration::from_millis(50) {
                        self.metrics.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                        return Ok(());
                    }
                }
                *last = Some(now);
            }

            // Daily-loss cap (legacy).
            if self.metrics.running_net_usdc.load(Ordering::Relaxed)
                  < -(self.cfg.max_daily_loss_usdc as i64) {
                self.metrics.skipped_daily_cap.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }

            // Native eligibility (defence in depth).
            if !self.is_native_eligible(route) {
                self.metrics.skipped_not_native.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
        }

        let payer = self.wallet.pubkey();
        let usdc_ata = derive_ata(&payer, &USDC);

        // Resolve a wallet ATA for every distinct mint in the route. Used
        // to map each leg's input/output side to the correct ATA. For a
        // 2-hop route this yields {USDC, interim_mint}; for a 3-hop it
        // yields {USDC, interim_a, interim_b}.
        let mut mint_atas: std::collections::HashMap<Pubkey, Pubkey> =
            std::collections::HashMap::with_capacity(route.hops.len() + 1);
        mint_atas.insert(USDC, usdc_ata);
        for hop in &route.hops {
            mint_atas.entry(hop.token_in.0).or_insert_with(|| derive_ata(&payer, &hop.token_in.0));
            mint_atas.entry(hop.token_out.0).or_insert_with(|| derive_ata(&payer, &hop.token_out.0));
        }
        let interim_mints: Vec<Pubkey> = route.hops.iter()
            .map(|h| h.token_out.0)
            .filter(|m| *m != USDC)
            .collect::<std::collections::BTreeSet<Pubkey>>()
            .into_iter().collect();

        // Walk the route, building each leg's swap-ix.
        //
        // Two distinct amount streams:
        //   * `quote_amount` — the OPTIMISTIC carry. Each leg's quoter sees
        //     the previous leg's `quoted` output. Used for the net-profit
        //     calculation: if our quoters are accurate, this is the expected
        //     on-chain net. Using min_out here would over-discount by the
        //     slippage haircut on EVERY leg, hiding real profit.
        //   * `ix_amount`    — the PESSIMISTIC carry. Each leg's IX uses the
        //     previous leg's `min_out` as `amount_in`, so we never try to
        //     swap more SPL than leg N-1 actually delivered. The IX-level
        //     `min_out` is enforced on-chain regardless of our quote.
        //
        // We call build_leg TWICE per hop: once for the optimistic quote (to
        // compute net), once for the actual IX (with pessimistic input). The
        // double call is cheap — both branches just hash math, no I/O — and
        // keeps the quote stream from compounding slippage haircuts.
        let n_hops = route.hops.len();
        let mut leg_ixs: Vec<Instruction> = Vec::with_capacity(n_hops);
        let mut leg_quoteds: Vec<u64> = Vec::with_capacity(n_hops);
        let mut leg_min_outs: Vec<u64> = Vec::with_capacity(n_hops);
        let mut quote_amount = probe_usdc;
        let mut ix_amount    = probe_usdc;
        let mut current_in_mint = USDC;
        for (i, hop) in route.hops.iter().enumerate() {
            let in_ata = *mint_atas.get(&current_in_mint)
                .ok_or(arb_core::ArbError::Invariant("native: missing input ATA"))?;
            let out_ata = *mint_atas.get(&hop.token_out.0)
                .ok_or(arb_core::ArbError::Invariant("native: missing output ATA"))?;
            // Optimistic quote at the carry's true expected size.
            let (quoted, _qmin, _qix) = self.build_leg(
                hop.pool, current_in_mint, quote_amount, &payer, in_ata, out_ata,
            ).await.map_err(|e| {
                if matches!(&e, arb_core::ArbError::Invariant(s)
                            if s.contains("openbook")) {
                    self.metrics.skipped_no_market.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.metrics.build_errors.fetch_add(1, Ordering::Relaxed);
                }
                e
            })?;
            // Pessimistic IX with min_out enforcement on the chain side.
            let (_iquoted, min_out, ix) = self.build_leg(
                hop.pool, current_in_mint, ix_amount, &payer, in_ata, out_ata,
            ).await.map_err(|e| {
                if matches!(&e, arb_core::ArbError::Invariant(s)
                            if s.contains("openbook")) {
                    self.metrics.skipped_no_market.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.metrics.build_errors.fetch_add(1, Ordering::Relaxed);
                }
                e
            })?;
            tracing::debug!(target: "scout::native",
                hop = i, pool = %hop.pool,
                in_mint = %current_in_mint, out_mint = %hop.token_out.0,
                quote_in = quote_amount, quoted,
                ix_in    = ix_amount,    min_out,
                "built leg");
            leg_ixs.push(ix);
            leg_quoteds.push(quoted);
            leg_min_outs.push(min_out);

            // 3-pool deviation guard: cross-check the leg's quote against
            // up to 2 other pools holding the same (in, out) pair. Compute
            // the median across the (up to 3) quotes; if our pool's quote
            // deviates from the median by more than PRICE_ORACLE_MAX_BPS
            // (default 100bps = 1%), the chosen pool is likely stale or
            // mid-shift and we skip the route. Set `PRICE_ORACLE_GUARD=true`
            // to enforce the skip; the deviation is always logged (with the
            // signed direction) regardless of the gate.
            let oracle_other_pools: Vec<Pubkey> = self.pool_store.snapshot()
                .iter()
                .filter(|p| {
                    p.address != hop.pool
                        && ((p.token_a.0 == current_in_mint && p.token_b.0 == hop.token_out.0)
                            || (p.token_a.0 == hop.token_out.0 && p.token_b.0 == current_in_mint))
                })
                .map(|p| p.address)
                .take(2)
                .collect();
            if !oracle_other_pools.is_empty() {
                let mut quotes: Vec<u64> = vec![quoted];
                for other in &oracle_other_pools {
                    if let Ok((q, _, _)) = self.build_leg(
                        *other, current_in_mint, quote_amount, &payer, in_ata, out_ata,
                    ).await {
                        if q > 0 { quotes.push(q); }
                    }
                }
                quotes.sort();
                let median = quotes[quotes.len() / 2];
                // Signed deviation in basis points: positive = our pool is
                // RICHER (gives more output), negative = LEANER. Operator
                // sees direction so they can tell over- vs under-pricing.
                let dev_bps: i64 = if median == 0 {
                    0
                } else {
                    ((quoted as i128 - median as i128) * 10_000 / median as i128) as i64
                };
                let max_bps: i64 = std::env::var("PRICE_ORACLE_MAX_BPS")
                    .ok().and_then(|s| s.parse().ok()).unwrap_or(100);
                let guard_on = std::env::var("PRICE_ORACLE_GUARD")
                    .as_deref() == Ok("true");
                info!(target: "scout::native",
                      hop = i, pool = %hop.pool,
                      own_quote = quoted, median, n_pools = quotes.len(),
                      dev_bps, direction = if dev_bps >= 0 { "over" } else { "under" },
                      max_bps, guard_on,
                      "🔭 price oracle median");
                if guard_on && dev_bps.abs() > max_bps {
                    info!(target: "scout::native",
                          hop = i, pool = %hop.pool,
                          dev_bps, max_bps,
                          "🚫 price oracle: deviation > threshold — skipping route");
                    return Ok(());
                }
            }

            current_in_mint = hop.token_out.0;
            quote_amount = quoted;     // optimistic carry → next leg's quote
            ix_amount    = min_out;    // pessimistic carry → next leg's IX
        }
        // Convenience aliases used downstream for log fields. For 3-hop the
        // "leg2" log fields actually refer to the LAST leg back to USDC.
        let leg1_quoted = leg_quoteds[0];
        let leg1_min_out = leg_min_outs[0];
        let leg2_quoted  = *leg_quoteds.last().unwrap();
        let leg2_min_out = *leg_min_outs.last().unwrap();
        let net = leg2_quoted as i64 - probe_usdc as i64;

        // Build the flash-loan envelope. Two providers supported:
        //   - Solend: 9bps fee, simple [refresh, borrow] / [repay] split.
        //   - MarginFi: ≈0bps, 4-instruction layout [start, borrow] / [deposit, end].
        // The provider switch is set by the FLASH_LOAN_PROVIDER env var.
        let mut pre_swap_ixs: Vec<Instruction> = Vec::new();
        let mut post_swap_ixs: Vec<Instruction> = Vec::new();
        let mut flash_fee: u64 = 0;
        let mut flash_active = false;

        // Flash gate relaxed 2026-05-16 per operator request: previous
        // `n_hops == 2` cap was based on a v0 budget calc that pre-dated
        // the extended Pyth ALT (80 addrs) + 4-5 Jupiter ALTs we now
        // routinely attach. With those, 3- and even 4-hop flash routes
        // often fit. 5+ may still overflow — the executor's v0
        // serializer will return a tx-too-large error which we already
        // catch & skip. Letting the path through here means MUCH more
        // of the route space gets evaluated for real profit potential
        // instead of being pre-filtered to dust-only $200 probes.
        //
        // Tunable via NATIVE_FLASH_MAX_HOPS env var (default 5).
        let flash_max_hops: usize = std::env::var("NATIVE_FLASH_MAX_HOPS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(5);
        if self.cfg.flash_loan_enabled && n_hops <= flash_max_hops {
            match self.cfg.flash_loan_provider {
                FlashLoanProvider::Solend => {
                    if let Some(ctx) = self.cfg.solend_ctx.as_ref() {
                        let env = build_usdc_flash_loan_envelope(
                            ctx, &self.cfg.solend_reserve_state,
                            payer, usdc_ata, probe_usdc, 6,
                        );
                        flash_fee = env.fee_lamports;
                        pre_swap_ixs.push(env.refresh_ix);
                        pre_swap_ixs.push(env.borrow_ix);
                        post_swap_ixs.push(env.repay_ix);
                        flash_active = true;
                    }
                }
                FlashLoanProvider::MarginFi => {
                    if let (Some(mfi_acct), Some(liq_vault)) = (
                        self.cfg.marginfi_account,
                        self.cfg.marginfi_liquidity_vault,
                    ) {
                        // Layout when MarginFi is active (dynamic n-hop):
                        //   [0]=budget_limit, [1]=budget_price, [2]=tip,
                        //   [3]              = USDC_ata
                        //   [4 .. 4+K]       = interim atas (K = interim_mints.len(), DEDUPED)
                        //   [4+K]            = start_flashloan
                        //   [5+K]            = borrow
                        //   [6+K .. 6+K+n]   = n_hops swap legs
                        //   [6+K+n]          = deposit
                        //   [7+K+n]          = end_flashloan  ← end_index
                        //
                        // BUGFIX 2026-05-15 (round 2): was `2*n_hops+6` which
                        // assumed |interim| = n_hops-1 (true only when every
                        // intermediate mint is unique). Routes with repeated
                        // mints (e.g. USDC→X→USDC→Y→USDC) dedupe interim_mints
                        // via BTreeSet, shifting end_flash earlier by the dupe
                        // count. start_flashloan then pointed past end_flash
                        // → load_instruction_at_checked returned InvalidArgument
                        // → 100% revert for routes with dupes (observed 410/501
                        // of recent reverts at IX 7 = start_flashloan).
                        let k = interim_mints.len();
                        let end_index: u64 = (7 + k + n_hops) as u64;
                        // We need a USDC Pyth oracle for end_flashloan's
                        // health check. Use the Solend Pyth oracle if
                        // available, else fall back to USDC mint (won't
                        // pass health check but lets us see the failure
                        // mode in pre-flight sim).
                        let pyth = self.cfg.solend_ctx
                            .as_ref()
                            .map(|c| c.pyth_oracle)
                            .unwrap_or(USDC);
                        let mfi_ctx = MarginFiFlashContext {
                            group: MARGINFI_MAIN_GROUP,
                            bank:  MARGINFI_USDC_BANK,
                            liquidity_vault: liq_vault,
                            liquidity_vault_authority:
                                derive_liquidity_vault_authority(&MARGINFI_USDC_BANK),
                            usdc_pyth_oracle: pyth,
                            marginfi_account: mfi_acct,
                        };
                        let (pre, post) = build_marginfi_flash_loan(
                            &mfi_ctx, payer, usdc_ata, probe_usdc, end_index,
                        );
                        flash_fee = 0; // MarginFi USDC bank's origination_fee is ~0
                        pre_swap_ixs = pre;
                        post_swap_ixs = post;
                        flash_active = true;
                    }
                }
            }
        }
        let net_after_fee = net - flash_fee as i64;

        if net_after_fee < self.cfg.min_profit_usdc as i64 {
            self.metrics.skipped_below_min.fetch_add(1, Ordering::Relaxed);
            // Sampled diagnostic — log EVERY leg so we can find which hop
            // is bleeding the value. For 3-hop this surfaces the middle hop
            // that v39's `leg1/leg_last` log hid.
            let n = self.metrics.skipped_below_min.load(Ordering::Relaxed);
            // v54: full visibility mode — log every below_min instead of
            // sampling. Operator wanted a status row per opportunity to see
            // the real-time spread landscape. With ~1k below_min / 30s this
            // is verbose but the bot is the only consumer of the log
            // stream; Railway's log retention handles the volume fine.
            // Set `NATIVE_BELOW_MIN_LOG_EVERY=10` to revert to sampling.
            let sample_every: u64 = std::env::var("NATIVE_BELOW_MIN_LOG_EVERY")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(1);
            if sample_every > 0 && n % sample_every == 0 {
                let legs_str: Vec<String> = route.hops.iter().enumerate().map(|(i, h)| {
                    let kind = self.pool_store.get(&h.pool).map(|p| format!("{:?}", p.kind))
                        .unwrap_or_else(|| "?".to_string());
                    format!(
                        "[leg{}: {} {}→{} q={} min={}]",
                        i,
                        kind,
                        short_mint(&h.token_in.0),
                        short_mint(&h.token_out.0),
                        leg_quoteds.get(i).copied().unwrap_or(0),
                        leg_min_outs.get(i).copied().unwrap_or(0),
                    )
                }).collect();
                info!(target: "scout::native",
                      n_hops,
                      probe_usdc,
                      net_after_fee_usdc = net_after_fee,
                      flash_active,
                      legs = %legs_str.join(" "),
                      "🔬 below_min sample — native quoter says route is net-negative");
            }
            return Ok(());
        }

        info!(target: "scout::native",
              pool_a = %route.hops[0].pool,
              pool_b = %route.hops[1].pool,
              probe_usdc, leg1_quoted, leg1_min_out, leg2_quoted, leg2_min_out,
              gross_net_usdc = net,
              flash_fee_usdc = flash_fee,
              net_after_fee_usdc = net_after_fee,
              flash_loan = flash_active,
              flash_provider = ?self.cfg.flash_loan_provider,
              "📈 native cleared min-profit — building tx");

        // Submission path: jito (default) or helius_sender. Helius Sender
        // bypasses Jito's per-region 1/sec + plane-wide ~5/sec rate limits
        // by routing the tx through Helius's stake-weighted private path
        // straight to leader validators. Trade-off: not a bundle (no
        // "all-or-nothing across multiple txs" guarantee) — but a single
        // flash-loan-wrapped arb tx is already atomic on-chain, so we
        // don't need bundle semantics for native arb.
        let submission_path = std::env::var("NATIVE_SUBMISSION_PATH")
            .unwrap_or_else(|_| "jito".to_string());
        let use_helius_sender = submission_path == "helius_sender";
        let use_tpu = submission_path == "tpu";
        // Pick a tip account (pseudo-random by considered counter).
        let tip_idx = self.metrics.considered.load(Ordering::Relaxed) as usize;
        let tip_account = if use_helius_sender {
            HELIUS_TIP_ACCOUNTS[tip_idx % HELIUS_TIP_ACCOUNTS.len()]
        } else {
            JITO_TIP_ACCOUNTS[tip_idx % JITO_TIP_ACCOUNTS.len()]
        };

        // Dynamic tip sizing.
        // ─────────────────────
        // Convert USDC profit → lamports via live SOL price, then ask the
        // tip-floor poller for a tip that:
        //   • beats the median landed tip (p50) so we land contested slots
        //   • is capped at 50% of the profit (never overpay)
        //   • respects the operator's static ceiling `cfg.jito_tip_lamports`
        // If the profit is too small to outbid p50, we skip the bundle —
        // sending it would cost more in tip than we earn.
        let net_after_fee_u: u64 = net_after_fee.max(0) as u64;
        let sol_price_micros = self.sol_price_micros.load(Ordering::Relaxed);
        // Convert USDC base units (6 dec) → lamports (9 dec) via SOL price:
        //   profit_usdc = net_after_fee_u / 1e6   USD
        //   profit_sol  = profit_usdc / sol_usd   SOL
        //   profit_lam  = profit_sol * 1e9        lamports
        // Combined: profit_lam = net_after_fee_u * 1e9 / (sol_price_micros)
        // (sol_price_micros = USD × 1e6, so 1e9 / sol_micros gives lamports per micro-USD.)
        // BUGFIX 2026-05-12: the multiplier was 1_000 (1e3) instead of
        // 1_000_000_000 (1e9), losing 6 orders of magnitude. With $0.27
        // profit on a real arb, the old math returned profit_lamports=2
        // → dynamic_tip=0 → Jito silently rejected the bundle (no
        // write-locked tip account). The unit derivation in the comment
        // above is correct; the code just had a typo.
        let profit_lamports: u64 = if sol_price_micros == 0 {
            0
        } else {
            ((net_after_fee_u as u128).saturating_mul(1_000_000_000)
                / sol_price_micros as u128) as u64
        };
        let tip_pct: u32 = std::env::var("EXECUTOR_TIP_PROFIT_PCT")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(70);
        // Default floor at p75 (upper quartile) not p50 — at our volume
        // (0/8 landed pre-fix) mid-pack tips lose every contested auction.
        // Operator can dial to 25/50/75/95/99 via EXECUTOR_TIP_FLOOR_PCTILE.
        let floor_pctile: u32 = std::env::var("EXECUTOR_TIP_FLOOR_PCTILE")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(75);
        let mut floor_lamports = self.tip_floor.percentile_lamports(floor_pctile);
        // Helius Sender has its own 200_000-lamport minimum — anything
        // below is silently dropped upstream. Promote the floor when
        // we're submitting via Sender.
        if use_helius_sender && floor_lamports < HELIUS_SENDER_MIN_TIP_LAMPORTS {
            floor_lamports = HELIUS_SENDER_MIN_TIP_LAMPORTS;
        }
        let static_ceiling = self.cfg.jito_tip_lamports;
        let dynamic_tip_lamports = match self.tip_floor
            .dynamic_tip_with_floor(profit_lamports, tip_pct, floor_lamports)
        {
            Ok(t) => t.min(static_ceiling.max(1_000)),
            Err(()) => {
                // Profit insufficient to outbid floor → skip cheaply (no Jito send).
                self.metrics.skipped_below_min.fetch_add(1, Ordering::Relaxed);
                info!(target: "scout::native",
                      net_after_fee_usdc = net_after_fee,
                      profit_lamports,
                      floor_pctile,
                      floor_lamports,
                      "📉 native skipping — profit < tip floor");
                return Ok(());
            }
        };

        // ── Tip-vs-balance gate ───────────────────────────────────────
        // Prevent SystemError::ResultWithNegativeLamports (custom 0x1)
        // on the tip transfer. Reserve covers:
        //   base fee (5_000 per sig)
        // + priority fee (cu_price × cu_limit / 1e6)
        // + ATA rent ~2M per fresh ATA × up to ~5 hops
        // Default 25_000_000 (=0.025 SOL) is generous; operator may tighten
        // via EXECUTOR_TIP_RESERVE_LAMPORTS.
        let tip_reserve: u64 = std::env::var("EXECUTOR_TIP_RESERVE_LAMPORTS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(25_000_000);
        let balance = self.wallet_balance_lamports.load(Ordering::Relaxed);
        let dynamic_tip_lamports = if balance == 0 {
            // Poller hasn't reported yet (first ~5s of process life).
            // Fall through; chain will reject if we're underfunded.
            dynamic_tip_lamports
        } else {
            let tip_budget = balance.saturating_sub(tip_reserve);
            if tip_budget < floor_lamports {
                // Can't even afford the floor + reserve → skip; chain would
                // either reject the tip transfer or land at p50<floor and
                // get auction-dropped anyway.
                self.metrics.skipped_below_min.fetch_add(1, Ordering::Relaxed);
                warn!(target: "scout::native",
                      balance_lamports = balance,
                      tip_reserve,
                      tip_budget,
                      floor_lamports,
                      "🛑 wallet too low for tip + reserve — skipping send");
                return Ok(());
            }
            if dynamic_tip_lamports > tip_budget {
                warn!(target: "scout::native",
                      balance_lamports = balance,
                      dynamic_tip_lamports,
                      tip_budget,
                      "⚠️ tip clamped to wallet balance — SOL is low");
                tip_budget
            } else {
                dynamic_tip_lamports
            }
        };

        info!(target: "scout::native",
              net_after_fee_usdc = net_after_fee,
              profit_lamports,
              dynamic_tip_lamports,
              tip_floor_p50 = self.tip_floor.p50_lamports(),
              tip_floor_p75 = self.tip_floor.p75_lamports(),
              tip_floor_p95 = self.tip_floor.p95_lamports(),
              floor_pctile,
              floor_lamports,
              static_ceiling,
              tip_pct,
              balance_lamports = balance,
              "💰 dynamic tip computed");

        // Compose the full instruction list.
        let mut ixs: Vec<Instruction> = Vec::with_capacity(12 + n_hops);
        ixs.push(set_compute_unit_limit_ix(self.cfg.compute_unit_limit));
        ixs.push(set_compute_unit_price_ix(self.cfg.priority_fee_micro_lamports_per_cu));
        ixs.push(system_transfer_ix(payer, tip_account, dynamic_tip_lamports));
        // Idempotent-ATA for USDC + every distinct interim mint in the route.
        ixs.push(create_ata_idempotent_ix(payer, payer, USDC));
        for m in &interim_mints {
            ixs.push(create_ata_idempotent_ix(payer, payer, *m));
        }
        ixs.extend(pre_swap_ixs);
        // Push every swap-leg in route order. For 2-hop this is identical
        // to the previous hardcoded `leg1_ix; leg2_ix` sequence.
        for ix in leg_ixs.into_iter() {
            ixs.push(ix);
        }
        ixs.extend(post_swap_ixs);

        // Fetch a fresh blockhash JUST before signing (max freshness).
        let blockhash = fetch_latest_blockhash(&self.rpc_url).await
            .map_err(|e| { self.metrics.build_errors.fetch_add(1, Ordering::Relaxed); e })?;

        // v0 tx with optional ALT compression. With flash-loan-wrapped 2-leg
        // arbs the legacy wire format runs over 1232 bytes; ALT replaces each
        // matched 32-byte pubkey with a 1-byte index. When no ALT is set the
        // builder still emits v0 (1 extra version byte) which Jito accepts.
        // Multi-ALT: resolve both EXECUTOR_NATIVE_ALT + EXECUTOR_NATIVE_ALT2
        // so 5-hop routes that don't fit under a single 256-cap ALT can
        // borrow extra compressible slots from the second.
        let alts = self.resolve_all_alts().await;
        // Force tip accounts into static_account_keys: Jito's auction filter
        // checks for "writable tip account" only on static keys, so an
        // ALT-resolved tip causes "Bundles must write lock at least one tip
        // account" rejection. Helius Sender doesn't have this rule, but
        // keeping the same force_static list is harmless (the Helius tip
        // accounts aren't in JITO_TIP_ACCOUNTS so they aren't pinned).
        let signed_b64 = build_and_sign_v0_tx_b64_multi(
            &payer, &ixs, &blockhash, &alts,
            &JITO_TIP_ACCOUNTS, self.wallet.bytes(),
        ).map_err(|e| { self.metrics.build_errors.fetch_add(1, Ordering::Relaxed); e })?;

        // Pre-flight simulation. The Helius RPC executes the v0 tx against
        // a recent bank snapshot and returns either a success (with logs)
        // or the on-chain error that would have caused a revert. Hits
        // Helius (NOT Jito), so it doesn't consume the 1/sec Block Engine
        // budget. Catches:
        //   * assert_min_profit failures (quoter overshoot vs reality)
        //   * SlippageToleranceExceeded (min_out can't be met)
        //   * insufficient ATA balance (vault state moved)
        //   * any program revert
        // Skipping the Jito send when sim fails saves the tip lamports
        // (~50_000 = $0.005 each) AND the auction-budget slot.
        //
        // BUT: when the quote is intentionally aggressive (zero-buffer,
        // tight slippage), the sim will reject almost every send — it's
        // looking at a stale snapshot, while in the block the price
        // actually moves (in our favour or against). Operators who want
        // "send and pray, eat only the tip if it reverts" can disable
        // pre-flight via `NATIVE_SKIP_PREFLIGHT_SIM=true`.
        let skip_sim = std::env::var("NATIVE_SKIP_PREFLIGHT_SIM")
            .ok()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);
        if !skip_sim {
            if let Err(sim_err) = simulate_tx_via_rpc(&self.rpc_url, &signed_b64).await {
                self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                warn!(target: "scout::native",
                      pool_a = %route.hops[0].pool,
                      pool_b = %route.hops[1].pool,
                      expected_net_usdc = net,
                      error = %sim_err,
                      "🛑 NATIVE pre-flight sim FAILED — would revert on-chain; \
                       skipping Jito send (no tip charged)");
                return Ok(());
            }
        }

        // Sanity: tip account is a real Jito tip account (defence in depth).
        debug_assert!(is_jito_tip_account(&tip_account));

        // From here on serialize bundle send.
        // Phase 6p: in fast_mode skip the in_flight mutex — concurrent
        // send_bundle calls are fine (Jito accepts independent tx
        // signatures). Only legacy mode locks.
        let _guard = if fast_mode {
            None
        } else {
            Some(self.in_flight.lock().await)
        };

        // Dry-run safety gate.
        if !self.cfg.live {
            self.metrics.skipped_dry_run.fetch_add(1, Ordering::Relaxed);
            info!(target: "scout::native",
                  net_usdc = net,
                  "🟡 NATIVE DRY-RUN — would have sent tx (set EXECUTOR_LIVE=true)");
            return Ok(());
        }

        if use_tpu {
            // Direct UDP to the next ~4 leader validators' TPU ports. Bypasses
            // Helius / Jito relays. Latency ~3-5ms (AWS Frankfurt → Solana FR
            // validators) vs ~50-150ms via relay. No tip account needed — the
            // priority fee in the tx (set_compute_unit_price) handles ordering.
            //
            // We send to multiple consecutive leaders (DEFAULT_FANOUT=4) so a
            // slot turnover during our ~3-5ms flight time doesn't miss the
            // window.
            let tpu = match self.tpu_sender.get() {
                Some(s) => Arc::clone(s),
                None => {
                    self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "scout::native",
                          "NATIVE_SUBMISSION_PATH=tpu but TpuSender not initialised (call init_tpu_sender)");
                    return Ok(());
                }
            };
            // Decode base64 -> raw bytes (UDP payload).
            let raw_tx = match base64::engine::general_purpose::STANDARD.decode(&signed_b64) {
                Ok(b) => b,
                Err(e) => {
                    self.metrics.build_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "scout::native", error = %e, "tpu: b64 decode failed");
                    return Ok(());
                }
            };
            let sig = match extract_signature_base58(&raw_tx) {
                Some(s) => s,
                None => "?".to_string(),
            };
            let current_slot = match fetch_current_slot(&self.rpc_url).await {
                Ok(s) => s,
                Err(e) => {
                    self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "scout::native", error = %e, "tpu: getSlot failed");
                    return Ok(());
                }
            };
            let fanout: usize = std::env::var("NATIVE_TPU_FANOUT")
                .ok().and_then(|s| s.parse().ok())
                .unwrap_or(crate::tpu_send::DEFAULT_FANOUT);
            match tpu.send_to_next_leaders(&raw_tx, current_slot, fanout).await {
                Ok(n) => {
                    self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
                    info!(target: "scout::native",
                          sig = %sig,
                          expected_net_usdc = net,
                          leaders = n,
                          slot = current_slot,
                          "🚀 NATIVE TPU SENT");
                }
                Err(e) => {
                    self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "scout::native", error = %e, "tpu send failed");
                    return Ok(());
                }
            }
            // Confirm via getSignatureStatuses after a brief delay.
            let rpc_url = self.rpc_url.clone();
            let metrics = Arc::clone(&self.metrics);
            let sig_clone = sig.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(4_000)).await;
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
                    "params":[[sig_clone], {"searchTransactionHistory": false}],
                });
                let resp: Option<serde_json::Value> = match reqwest::Client::new()
                    .post(&rpc_url).json(&body).send().await
                {
                    Ok(r) => r.json().await.ok(),
                    Err(_) => None,
                };
                match resp.as_ref().and_then(|v| v.pointer("/result/value/0")).cloned() {
                    Some(v) if !v.is_null() => {
                        let err = v.pointer("/err").and_then(|e| if e.is_null() { None } else { Some(e.to_string()) });
                        if err.is_none() {
                            metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                            metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                            info!(target: "scout::native", sig = %sig_clone, "🎯 NATIVE TPU LANDED");
                        } else {
                            info!(target: "scout::native", sig = %sig_clone,
                                  err = ?err.unwrap_or_default(),
                                  "💀 NATIVE TPU REVERTED");
                        }
                    }
                    _ => info!(target: "scout::native", sig = %sig_clone,
                               "💀 NATIVE TPU DROPPED — sig not seen after 4s"),
                }
            });
            return Ok(());
        }

        if use_helius_sender {
            // Single-tx submission via Helius Sender — stake-weighted private
            // path that bypasses Jito's plane-wide rate limit. Returns the
            // tx signature immediately; landing is confirmed via getSignatureStatuses
            // on the regular RPC.
            let sender_url = match std::env::var("HELIUS_SENDER_URL").ok()
                .filter(|s| !s.trim().is_empty())
            {
                Some(u) => u,
                None => {
                    warn!(target: "scout::native",
                          "HELIUS_SENDER_URL not set — falling back is not implemented; skipping send");
                    self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            };
            let api_key = match std::env::var("HELIUS_API_KEY").ok()
                .filter(|s| !s.trim().is_empty())
            {
                Some(k) => k,
                None => {
                    warn!(target: "scout::native",
                          "HELIUS_API_KEY not set — Sender path requires it");
                    self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            };
            let sig = match self.jito.send_via_helius_sender(&sender_url, &api_key, &signed_b64).await {
                Ok(s) => s,
                Err(e) => {
                    self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(target: "scout::native", error = %e, "native helius-sender send failed");
                    return Ok(());
                }
            };
            self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
            info!(target: "scout::native",
                  sig = %sig,
                  expected_net_usdc = net,
                  "🚀 NATIVE HELIUS SENT");

            // Confirm via getSignatureStatuses after a brief delay (Helius
            // Sender lands or fails within ~1-2 slots = ~800ms).
            let rpc_url = self.rpc_url.clone();
            let metrics = Arc::clone(&self.metrics);
            let sig_clone = sig.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(3_000)).await;
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id":1,"method":"getSignatureStatuses",
                    "params":[[sig_clone], {"searchTransactionHistory": false}],
                });
                let resp: Option<serde_json::Value> = match reqwest::Client::new()
                    .post(&rpc_url).json(&body).send().await
                {
                    Ok(r) => r.json().await.ok(),
                    Err(_) => None,
                };
                match resp.as_ref()
                    .and_then(|v| v.pointer("/result/value/0"))
                    .cloned()
                {
                    Some(v) if !v.is_null() => {
                        let err = v.pointer("/err").and_then(|e| if e.is_null() { None } else { Some(e.to_string()) });
                        if err.is_none() {
                            metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                            metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                            info!(target: "scout::native",
                                  sig = %sig_clone,
                                  "🎯 NATIVE HELIUS LANDED");
                        } else {
                            info!(target: "scout::native",
                                  sig = %sig_clone,
                                  err = ?err.unwrap_or_default(),
                                  "💀 NATIVE HELIUS REVERTED");
                        }
                    }
                    _ => {
                        info!(target: "scout::native", sig = %sig_clone,
                              "💀 NATIVE HELIUS DROPPED — sig not seen after 3s");
                    }
                }
            });
            return Ok(());
        }

        // Send to Jito as a single-tx bundle.
        let bundle = vec![signed_b64];
        let bundle_id = match self.jito.send_bundle(&bundle).await {
            Ok(id) => id,
            Err(e) => {
                self.metrics.send_errors.fetch_add(1, Ordering::Relaxed);
                warn!(target: "scout::native", error = %e, "native sendBundle failed");
                return Ok(());
            }
        };
        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::native",
              bundle_id = %bundle_id,
              expected_net_usdc = net,
              "🚀 NATIVE BUNDLE SENT");

        // SINGLE delayed status check, 12s after send. The earlier 6×2s
        // poll-loop here triggered Jito's 120s back-off because each
        // `getBundleStatuses` call shares the 1/sec budget with
        // `sendBundle`. One poll/bundle ≈ 0.08 req/sec, on top of the
        // ~0.2 req/sec send rate — comfortably under the limit.
        let jito = self.jito.clone();
        let metrics = Arc::clone(&self.metrics);
        let bid = bundle_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(12_000)).await;
            match jito.bundle_status(&bid).await {
                Ok(Some(slot)) => {
                    metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                    metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                    info!(target: "scout::native",
                          bundle_id = %bid, slot,
                          "🎯 NATIVE BUNDLE LANDED");
                }
                Ok(None) => {
                    info!(target: "scout::native", bundle_id = %bid,
                          "💀 NATIVE BUNDLE DROPPED — no slot after 12s");
                }
                Err(e) => warn!(target: "scout::native",
                                bundle_id = %bid, error = %e,
                                "native bundle_status poll failed"),
            }
        });
        Ok(())
    }

    fn local_quote(
        &self,
        pool:       &RaydiumAmmV4Data,
        input_mint: Pubkey,
        amount_in:  u64,
    ) -> ArbResult<u64> {
        let base_reserve = self.vault_store.get(&pool.base_vault)
            .ok_or(arb_core::ArbError::Invariant("base_vault unknown"))?;
        let quote_reserve = self.vault_store.get(&pool.quote_vault)
            .ok_or(arb_core::ArbError::Invariant("quote_vault unknown"))?;
        let (r_in, r_out) = if input_mint == pool.base_mint {
            (base_reserve, quote_reserve)
        } else {
            (quote_reserve, base_reserve)
        };
        quote_with_reserves(amount_in, r_in, r_out,
                            pool.swap_fee_numerator, pool.swap_fee_denominator)
    }
}

/// Simulate a base64 v0 transaction against the RPC. Returns `Ok(())` if
/// the simulation completes without an error (= the tx would land
/// successfully on-chain). Returns `Err` containing the program error +
/// truncated execution logs when the simulation reverts — those logs are
/// the most useful diagnostic ("AnchorError: ProfitBelowThreshold",
/// "SlippageToleranceExceeded", "InsufficientFunds", etc.).
///
/// Uses `replaceRecentBlockhash: true` so the simulator runs against the
/// CURRENT bank state, regardless of how stale our signed tx's blockhash
/// has become — the only thing we're testing is whether the instructions
/// would succeed, not whether the blockhash is fresh enough to land.
async fn simulate_tx_via_rpc(rpc_url: &str, signed_b64: &str) -> ArbResult<()> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "simulateTransaction",
        "params": [signed_b64, {
            "encoding": "base64",
            "commitment": "processed",
            "replaceRecentBlockhash": true,
            "sigVerify": false,
        }],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await
        .map_err(|e| arb_core::ArbError::Rpc(format!("simulate http: {e}")))?
        .json().await
        .map_err(|e| arb_core::ArbError::Rpc(format!("simulate json: {e}")))?;
    if let Some(err) = resp.pointer("/error") {
        return Err(arb_core::ArbError::Rpc(format!("simulate rpc error: {err}")));
    }
    let value = resp.pointer("/result/value")
        .ok_or_else(|| arb_core::ArbError::Rpc(format!("simulate missing value: {resp}")))?;
    let sim_err = value.get("err");
    if let Some(e) = sim_err {
        if !e.is_null() {
            // Pull the last few log lines — they almost always contain the
            // Anchor / SPL error message that triggered the revert.
            let logs = value.get("logs")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    let n = arr.len();
                    let start = n.saturating_sub(6);
                    arr[start..].iter()
                        .filter_map(|v| v.as_str()).collect::<Vec<_>>().join(" | ")
                })
                .unwrap_or_default();
            return Err(arb_core::ArbError::Rpc(
                format!("on-chain err={e} | tail-logs: {logs}")));
        }
    }
    Ok(())
}

/// Mint decimals for our known SOL/USDC/USDT cycle. Returns `None` for an
/// unknown mint — the Obric quoter rejects in that case.
fn decimals_for_mint(mint: &Pubkey) -> Option<u8> {
    crate::known::known_decimals(mint)
}

/// Apply slippage tolerance: `out * (10000 - bps) / 10000`. Used to compute
/// the `min_amount_out` floor we pass to Raydium's swap_base_in.
fn apply_slippage(out: u64, bps: u32) -> u64 {
    let bps = bps as u64;
    (out as u128 * (10_000u128 - bps as u128) / 10_000u128) as u64
}

/// Fill a `RaydiumSwapAccounts` from the pool's address + parsed state +
/// its OpenBook market accounts + the user's source/dest ATAs.
#[allow(clippy::too_many_arguments)]
fn swap_accounts(
    amm_addr:      Pubkey,
    market_addr:   Pubkey,
    pool:          &RaydiumAmmV4Data,
    market:        &crate::dex::openbook_market::OpenBookMarketAccounts,
    user_owner:    &Pubkey,
    user_src:      Pubkey,
    user_dst:      Pubkey,
) -> RaydiumSwapAccounts {
    RaydiumSwapAccounts {
        amm:                       amm_addr,
        amm_open_orders:           pool.open_orders,
        amm_target_orders:         pool.target_orders,
        pool_coin_token_account:   pool.base_vault,
        pool_pc_token_account:     pool.quote_vault,
        serum_program:             pool.market_program,
        serum_market:              market_addr,
        serum_bids:                market.bids,
        serum_asks:                market.asks,
        serum_event_queue:         market.event_queue,
        serum_coin_vault:          market.coin_vault,
        serum_pc_vault:            market.pc_vault,
        serum_vault_signer:        market.vault_signer,
        user_source_token_account: user_src,
        user_dest_token_account:   user_dst,
        user_owner:                *user_owner,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_slippage_floors_correctly() {
        assert_eq!(apply_slippage(10_000, 0),    10_000);
        assert_eq!(apply_slippage(10_000, 100),  9_900);   // 1%
        assert_eq!(apply_slippage(10_000, 80),   9_920);   // 80bps default
        assert_eq!(apply_slippage(10_000, 10_000), 0);     // 100% slippage
    }

    #[test]
    fn apply_slippage_no_overflow_on_max_u64() {
        // ensure the u128 intermediate prevents overflow
        let _ = apply_slippage(u64::MAX / 2, 80);
    }
}
