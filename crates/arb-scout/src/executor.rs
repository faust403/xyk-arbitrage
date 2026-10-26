//! Executor — Stage B orchestrator. Turns scout-found arb opportunities
//! into actual trades, with multiple safety gates.
//!
//! ## Hard safety properties
//!
//! 1. **Default to DRY-RUN.** `EXECUTOR_LIVE` env must be exactly `"true"`
//!    (case-sensitive) to ever call `sendBundle`. Any other value runs
//!    the full simulation pipeline but stops short of submission.
//!
//! 2. **Min-profit gate.** `EXECUTOR_MIN_PROFIT_USDC` (base units) must
//!    be CLEARED by Jupiter's quote, not just the scout's spot estimate.
//!    Default $0.50 = 500_000 base units.
//!
//! 3. **Slippage cap.** `EXECUTOR_SLIPPAGE_BPS` per leg (default 30 bp).
//!    Combined with two-leg execution that's a 60bp worst-case haircut;
//!    we require Jupiter's gross profit to exceed that PLUS our min_profit
//!    threshold before sending.
//!
//! 4. **Daily loss cap.** `EXECUTOR_MAX_DAILY_LOSS_USDC` (default $5).
//!    If today's running net is below `-cap`, refuse all sends until
//!    midnight UTC. Note: tracked in-memory, resets on process restart.
//!
//! 5. **One-in-flight.** A `Mutex` guards a single in-flight bundle.
//!    No race conditions if multiple opportunities fire concurrently.
//!
//! 6. **Pre-flight `simulateBundle`.** Even after all the above, the
//!    bundle is simulated by Jito and the final balance is checked
//!    against `wallet_pre + min_profit` BEFORE `sendBundle` is called.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arb_core::{ArbError, ArbResult};
use chrono::{DateTime, Utc};
use solana_program::pubkey::Pubkey;
use tokio::sync::Mutex;
use tracing::{info, warn};

use arb_jito::{JitoBundleClient, TipFloor};
use crate::jupiter_swap::JupiterSwapClient;
use crate::known;
use crate::raw_tx::{
    build_and_sign_legacy_tx_b64, create_ata_idempotent_ix, create_ata_idempotent_ix_2022,
    derive_ata, fetch_latest_blockhash, set_compute_unit_limit_ix, set_compute_unit_price_ix,
    system_transfer_ix,
};
use crate::pump_fun::{
    self, build_buy_ix as pump_buy_ix, build_sell_ix as pump_sell_ix,
    PumpFunBuyAccounts,
};
use crate::pump_sniper_budget::{PumpSnipeBudget, ReserveSkip};
use crate::rpc::HeliusRpc;
use crate::signing::sign_versioned_transaction_b64;
use crate::v0_tx::build_and_sign_v0_tx_b64;
use crate::alt::fetch_alt_addresses;
use crate::known::JITO_TIP_ACCOUNTS;
use crate::wallet::Wallet;
use arb_flashloan::{
    build_marginfi_flash_loan, derive_liquidity_vault_authority,
    build_usdc_flash_loan_envelope, ReserveState as SolendReserveState,
    SolendUsdcContext,
    MarginFiFlashContext, MARGINFI_MAIN_GROUP, MARGINFI_USDC_BANK,
};
use tokio::sync::OnceCell;

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// `true` ⇒ actually call sendBundle. Anything else ⇒ dry-run.
    pub live:                  bool,
    pub min_profit_usdc:       u64,
    pub slippage_bps:          u32,
    pub max_daily_loss_usdc:   u64,
    pub probe_amount_usdc:     u64,
    /// Sample 1-in-N opportunities (Jupiter free tier ~10 req/sec; with
    /// ~5 ok_some/sec and 2 quote calls each, sampling 1-in-10 keeps us
    /// safely under the limit).
    pub sample_one_in_n:       u64,
    /// Jito tip embedded in EACH swap tx (so 2× this per arb cycle). At
    /// 10000 lamports = ~$0.001 per swap, cycle tip ~$0.002. Bundles below
    /// ~5000 lamports total tip almost never land — set ≥5000.
    pub jito_tip_lamports:     u64,
    /// Minimum delay between *attempts* (any opportunity that passes
    /// sampling). Real backpressure for both Jupiter (4 req/cycle) and
    /// Jito simulateBundle (hard 1/sec limit). 1100ms = 0.91 cycles/sec.
    pub min_interval:          Duration,
    /// `true` ⇒ send via Jito sendBundle (atomic 2-leg, MEV priority via tip).
    /// `false` ⇒ send each leg via Helius `sendTransaction` (sequential, no
    /// atomicity — leg2 may fail leaving us stuck with wSOL — but bypasses
    /// Jito competition entirely. Useful when MEV bots out-tip us on every slot).
    pub use_jito:              bool,
    /// Wrap each Jupiter atomic-tx with a MarginFi flash loan envelope so the
    /// probe can scale beyond wallet capital. Reads `FLASH_LOAN_ENABLED` env.
    /// When true, forces PATH C (atomic v0 tx with ALT, single Jito bundle).
    pub flash_loan_enabled:    bool,
    pub marginfi_account:      Option<Pubkey>,
    pub marginfi_liquidity_vault: Option<Pubkey>,
    /// USDC Pyth oracle used by MarginFi's end_flashloan health-check ix.
    pub usdc_pyth_oracle:      Option<Pubkey>,
    /// Address Lookup Table for v0 tx compression. Required when flash loan
    /// is wrapped around Jupiter ixs (would otherwise overflow 1232 bytes).
    pub alt_pubkey:            Option<Pubkey>,

    // ─── USDT alternate base path ────────────────────────────────────
    // When all three are populated AND `usdt_alternation_enabled` is
    // true, every-other opportunity probes via USDT instead of USDC.
    // Doubles route variety and avoids self-arbing the same USDC pool.
    pub usdt_alternation_enabled: bool,
    pub marginfi_usdt_liquidity_vault: Option<Pubkey>,
    pub usdt_pyth_oracle:         Option<Pubkey>,
    /// WBTC (Wormhole) MarginFi flash bank context. WBTC mint = 3NZ9JM…ZB4u,
    /// 8 decimals. Used for liquidations whose debt is in WBTC.
    pub marginfi_wbtc_liquidity_vault: Option<Pubkey>,
    pub wbtc_pyth_oracle:         Option<Pubkey>,

    // ─── Solend liquidator Phase 2: USDC flash loan wrap ──────────────
    // When `solend_liquidator_flash_loan_usd > 0` AND `solend_usdc_ctx` is
    // populated, every Solend liquidation is wrapped in a Solend USDC flash
    // loan envelope so we can repay up to this amount per liquidation rather
    // than being capped by the wallet balance (~$235). 9 bps fee.
    pub solend_usdc_ctx:                  Option<SolendUsdcContext>,
    pub solend_reserve_state:             SolendReserveState,
    pub solend_liquidator_flash_loan_usd: u64,
}

impl ExecutorConfig {
    pub fn from_env() -> Self {
        let env_u64 = |k: &str, d: u64| std::env::var(k).ok()
            .and_then(|s| s.parse().ok()).unwrap_or(d);
        let env_u32 = |k: &str, d: u32| std::env::var(k).ok()
            .and_then(|s| s.parse().ok()).unwrap_or(d);
        let env_pk = |k: &str| -> Option<Pubkey> {
            std::env::var(k).ok().and_then(|s| {
                use std::str::FromStr;
                Pubkey::from_str(s.trim()).ok()
            })
        };
        Self {
            live: std::env::var("EXECUTOR_LIVE").as_deref() == Ok("true"),
            min_profit_usdc:     env_u64("EXECUTOR_MIN_PROFIT_USDC",     500_000),
            slippage_bps:        env_u32("EXECUTOR_SLIPPAGE_BPS",        30),
            max_daily_loss_usdc: env_u64("EXECUTOR_MAX_DAILY_LOSS_USDC", 5_000_000),
            probe_amount_usdc:   env_u64("EXECUTOR_PROBE_AMOUNT_USDC",   500_000_000),
            sample_one_in_n:     env_u64("EXECUTOR_SAMPLE_ONE_IN_N",     10).max(1),
            jito_tip_lamports:   env_u64("EXECUTOR_JITO_TIP_LAMPORTS",   10_000),
            min_interval:        Duration::from_millis(env_u64("EXECUTOR_MIN_INTERVAL_MS", 50)),
            use_jito:            std::env::var("EXECUTOR_USE_JITO")
                                     .as_deref().unwrap_or("true") == "true",
            flash_loan_enabled:  std::env::var("FLASH_LOAN_ENABLED").as_deref() == Ok("true"),
            marginfi_account:    env_pk("MARGINFI_ACCOUNT"),
            marginfi_liquidity_vault: env_pk("MARGINFI_USDC_LIQUIDITY_VAULT"),
            usdc_pyth_oracle:    env_pk("SOLEND_USDC_PYTH_ORACLE"),
            alt_pubkey:          env_pk("EXECUTOR_NATIVE_ALT"),
            usdt_alternation_enabled: std::env::var("USDT_ALTERNATION_ENABLED")
                                         .as_deref() == Ok("true"),
            marginfi_usdt_liquidity_vault: env_pk("MARGINFI_USDT_LIQUIDITY_VAULT"),
            // Hard-coded fallback for USDT Pyth Pull v2 — verified 2026-05-13
            // by reading MarginFi USDT bank at offset 610. Operator can override
            // via env if Pyth migrates the feed.
            usdt_pyth_oracle:    {
                use std::str::FromStr;
                env_pk("USDT_PYTH_ORACLE").or_else(||
                    solana_program::pubkey::Pubkey::from_str(
                        "HT2PLQBcG5EiCcNSaMHAjSgd9F98ecpATbk4Sk5oYuM"
                    ).ok()
                )
            },
            // WBTC (Wormhole) MarginFi bank — verified 2026-05-13 by reading
            // MarginFi program accounts. liquidity_vault derived at runtime
            // via PDA; override via env if needed.
            marginfi_wbtc_liquidity_vault: env_pk("MARGINFI_WBTC_LIQUIDITY_VAULT").or_else(||
                Some(arb_flashloan::derive_liquidity_vault(&arb_flashloan::MARGINFI_WBTC_BANK))
            ),
            wbtc_pyth_oracle:    {
                use std::str::FromStr;
                env_pk("WBTC_PYTH_ORACLE").or_else(||
                    solana_program::pubkey::Pubkey::from_str(
                        "4cSM2e6rvbGQUFiJbqytoVMi5GgghSMr8LwVrT9VPSPo"
                    ).ok()
                )
            },
            solend_usdc_ctx: {
                let lm   = env_pk("SOLEND_LENDING_MARKET");
                let lma  = env_pk("SOLEND_LENDING_MARKET_AUTHORITY");
                let rsv  = env_pk("SOLEND_USDC_RESERVE");
                let sup  = env_pk("SOLEND_USDC_RESERVE_LIQUIDITY_SUPPLY");
                let fee  = env_pk("SOLEND_USDC_FEE_RECEIVER");
                let host = env_pk("SOLEND_USDC_HOST_FEE_RECEIVER");
                let pyth = env_pk("SOLEND_USDC_PYTH_ORACLE");
                let sb   = env_pk("SOLEND_USDC_SWITCHBOARD_ORACLE");
                match (lm, lma, rsv, sup, fee, pyth) {
                    (Some(lm), Some(lma), Some(rsv), Some(sup), Some(fee), Some(pyth)) => {
                        Some(SolendUsdcContext {
                            lending_market:           lm,
                            lending_market_authority: lma,
                            reserve:                  rsv,
                            reserve_liquidity_supply: sup,
                            fee_receiver:             fee,
                            host_fee_receiver:        host.unwrap_or(fee),
                            pyth_oracle:              pyth,
                            switchboard_oracle:       sb.unwrap_or(pyth),
                        })
                    }
                    _ => None,
                }
            },
            solend_reserve_state: SolendReserveState::from_fee_bps(
                env_u64("SOLEND_FLASH_LOAN_FEE_BPS", 9),
            ),
            solend_liquidator_flash_loan_usd:
                env_u64("SOLEND_LIQUIDATOR_FLASH_LOAN_USD", 0),
        }
    }
}

#[derive(Debug, Default)]
pub struct ExecutorMetrics {
    pub considered:           AtomicU64,
    pub skipped_sample:       AtomicU64,    // dropped by 1-in-N filter
    pub skipped_throttle:     AtomicU64,    // dropped by min-interval throttle
    pub jupiter_errors:       AtomicU64,    // Jupiter quote API failed (rate limit, route not found, etc.)
    pub skipped_below_min:    AtomicU64,
    pub skipped_no_quote:     AtomicU64,
    pub skipped_dry_run:      AtomicU64,
    pub skipped_daily_cap:    AtomicU64,
    pub simulated_ok:         AtomicU64,
    pub simulated_fail:       AtomicU64,
    pub sent_bundles:         AtomicU64,
    pub landed_bundles:       AtomicU64,
    pub running_net_usdc:     AtomicI64,
    pub last_action_at:       std::sync::Mutex<Option<DateTime<Utc>>>,
}

pub struct Executor {
    cfg:           ExecutorConfig,
    wallet:        Wallet,
    jupiter_swap:  JupiterSwapClient,
    jito:          JitoBundleClient,
    helius_rpc:    HeliusRpc,
    helius_url:    String,    // for blockhash fetch (raw_tx::fetch_latest_blockhash)
    metrics:       Arc<ExecutorMetrics>,
    /// Single in-flight gate.
    in_flight:     Mutex<()>,
    /// Last attempt timestamp — used for min-interval throttle.
    last_attempt:  Mutex<Option<Instant>>,
    /// Jito-imposed back-off deadline. When set, ALL send attempts are
    /// throttled until `Instant::now() >= back_off_until`. Populated when
    /// Jito returns `Retry after Xms` — failing to honour it triggers
    /// exponential back-off escalation (1s → 6s → 30s → 120s).
    back_off_until: Mutex<Option<Instant>>,
    /// Lazy-loaded ALT addresses for v0 compression (flash-loan path).
    /// Empty when no ALT configured or fetch failed.
    alt_addresses: OnceCell<Vec<Pubkey>>,
    /// Shared Jito tip-floor poller. p50/p75/p99 are live percentiles of
    /// landed bundle tips. Used by `pump_fun_buy_sell_atomic` to size the
    /// Helius Sender tip dynamically vs. a fixed 200k floor.
    tip_floor:     TipFloor,
    /// HODL-mode budget gate. `Some` when `PUMP_FUN_TOTAL_BUDGET_LAMPORTS`
    /// is set. Used by `pump_fun_buy_only` to enforce the hard ceiling on
    /// total lamports sent across all pump.fun snipes.
    pump_sniper_budget: Option<Arc<PumpSnipeBudget>>,
    /// Toggle counter for the Jupiter PATH-B atomic arb send. Even values
    /// build legs in the natural order (leg1, then leg2); odd values build
    /// them reversed (leg2, then leg1). Incremented on every entry into
    /// the PATH-B leg-ordering block so the alternation is independent of
    /// the global `sent_bundles` counter (which other paths share).
    arb_atomic_toggle: AtomicU64,
    /// Throttle gate for `pump_fun_buy_only`. Mutex serialises concurrent
    /// HODL buys so at least `PUMP_FUN_BUY_MIN_INTERVAL_MS` (default 1000)
    /// elapses between consecutive sends. Tasks that arrive sooner queue
    /// on the mutex and sleep the remainder before releasing it. Without
    /// this, Yellowstone bursts fan out parallel sends and the wallet
    /// bleeds at full rate.
    pump_buy_last_send: tokio::sync::Mutex<Option<Instant>>,
}

impl Executor {
    /// Resolve ALT addresses for v0 compression. Lazy-fetched on first call;
    /// cached for the process lifetime. Empty slice when no ALT configured.
    async fn resolve_alt(&self) -> &[Pubkey] {
        let alt_pk = match self.cfg.alt_pubkey {
            Some(pk) => pk,
            None => return &[],
        };
        let cached = self.alt_addresses.get_or_init(|| async {
            match fetch_alt_addresses(&self.helius_url, &alt_pk).await {
                Ok(addrs) => {
                    info!(target: "scout::executor",
                          alt = %alt_pk, addresses = addrs.len(),
                          "ALT loaded for Jupiter+flash path");
                    addrs
                }
                Err(e) => {
                    warn!(target: "scout::executor",
                          alt = %alt_pk, error = %e,
                          "failed to fetch ALT — Jupiter+flash will fall back to no-lookup");
                    Vec::new()
                }
            }
        }).await;
        cached.as_slice()
    }

    /// If `err_text` carries a Jito rate-limit response with a
    /// `Retry after Xms` suffix, store a back-off deadline so subsequent
    /// send attempts wait until Jito's penalty expires. Without this the
    /// 1s back-off escalates to 6s → 30s → 120s on every retry.
    async fn maybe_record_jito_back_off(&self, err_text: &str) {
        let Some(ms) = parse_retry_after_ms(err_text) else { return };
        // Clamp at 120s — Jito's documented max — and add a small buffer.
        let ms = ms.min(120_000).saturating_add(100);
        let until = Instant::now() + Duration::from_millis(ms);
        let mut back_off = self.back_off_until.lock().await;
        *back_off = Some(until);
        warn!(target: "scout::executor",
              retry_after_ms = ms,
              "⏸ Jito back-off recorded — pausing all sends");
    }

    pub fn new(cfg: ExecutorConfig, wallet: Wallet, helius_rpc_url: String, tip_floor: TipFloor) -> Self {
        let pump_sniper_budget = std::env::var("PUMP_FUN_TOTAL_BUDGET_LAMPORTS")
            .ok().and_then(|s| s.parse::<u64>().ok())
            .map(|cap| {
                let path = std::env::var("PUMP_FUN_BUDGET_STATE_PATH")
                    .unwrap_or_else(|_| "state/pump_sniper_budget.json".to_string());
                Arc::new(PumpSnipeBudget::load_or_default(path.into(), cap))
            });
        Self {
            cfg, wallet,
            jupiter_swap: JupiterSwapClient::new(),
            jito:         JitoBundleClient::default(),
            helius_rpc:   HeliusRpc::new(helius_rpc_url.clone()),
            helius_url:   helius_rpc_url,
            metrics:      Arc::new(ExecutorMetrics::default()),
            in_flight:    Mutex::new(()),
            last_attempt: Mutex::new(None),
            back_off_until: Mutex::new(None),
            alt_addresses: OnceCell::new(),
            tip_floor,
            pump_sniper_budget,
            arb_atomic_toggle: AtomicU64::new(0),
            pump_buy_last_send: tokio::sync::Mutex::new(None),
        }
    }

    pub fn pump_sniper_budget(&self) -> Option<&Arc<PumpSnipeBudget>> {
        self.pump_sniper_budget.as_ref()
    }

    pub fn wallet_pubkey_str(&self) -> String {
        self.wallet.pubkey().to_string()
    }

    pub fn wallet_pubkey(&self) -> solana_program::pubkey::Pubkey {
        self.wallet.pubkey()
    }

    pub fn metrics(&self) -> Arc<ExecutorMetrics> { Arc::clone(&self.metrics) }

    /// Pump.fun bonding-curve BUY — direct call to the pump.fun program
    /// (bypasses Jupiter, sub-second possible). For a freshly-minted token
    /// the bonding curve is at INITIAL state so we use the constants
    /// from `pump_fun.rs` to compute expected tokens. Returns the tx
    /// signature on send.
    ///
    /// `mint` — the token mint (from the Create event)
    /// `creator` — the dev wallet (from the Create event signer)
    /// `sol_lamports` — how many lamports to spend on the buy
    pub async fn pump_fun_buy(
        &self,
        mint:          solana_program::pubkey::Pubkey,
        creator:       solana_program::pubkey::Pubkey,
        sol_lamports:  u64,
    ) -> ArbResult<(String, u64)> {
        let pubkey = self.wallet.pubkey();

        // Compute expected tokens at INITIAL bonding-curve state.
        let amount = pump_fun::compute_buy_tokens(
            pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
            pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
            sol_lamports,
        );
        if amount == 0 {
            return Err(ArbError::Invariant("pump_fun_buy: amount computed to 0"));
        }

        // Slippage: accept up to +10% sol cost relative to the input we want
        // to spend. The buy IX's `max_sol_cost` caps the SOL the program
        // will pull from us.
        let max_sol_cost = sol_lamports.saturating_mul(110) / 100;

        let buy_accs = PumpFunBuyAccounts {
            user:          pubkey,
            mint,
            creator,
            fee_recipient: pump_fun::PUMP_FUN_FEE_RECIPIENT_DEFAULT,
            bonding_curve_v2_seed_ok: false,
            is_token_2022: false,
            buyback_recipient: pump_fun::BUYBACK_FEE_RECIPIENTS[0],
        };
        let buy = pump_buy_ix(&buy_accs, amount, max_sol_cost);

        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(8);
        all.push(set_compute_unit_limit_ix(300_000));
        all.push(set_compute_unit_price_ix(10_000));
        // Create our Token-2022 ATA for the mint (pump.fun mints are Token-2022).
        all.push(create_ata_idempotent_ix_2022(pubkey, pubkey, mint));
        all.push(buy);

        // Helius Sender tip.
        let tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        if !self.cfg.use_jito && tip > 0 {
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
            all.push(system_transfer_ix(pubkey, tip_account, tip));
        }

        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;
        let signed = if let Some(alt_pk) = self.cfg.alt_pubkey {
            build_and_sign_v0_tx_b64(
                &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                &[], self.wallet.bytes(),
            )?
        } else {
            build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())?
        };

        // Send via Helius Sender (we're use_jito=false in pump.fun mode).
        let url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_SENDER_URL not set".into()))?;
        let key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_API_KEY not set".into()))?;
        let sig = self.jito.send_via_helius_sender(&url, &key, &signed).await?;

        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              sig = %sig, mint = %mint, sol_in = sol_lamports, tokens_expected = amount,
              "🚀 PUMP.FUN BUY SENT");

        Ok((sig, amount))
    }

    /// Pump.fun ATOMIC buy+sell in a SINGLE transaction. No open positions
    /// over time — both legs settle or neither does. Computes expected
    /// token output from the initial bonding-curve state, then schedules
    /// the sell using 95% of that estimate (slippage cushion).
    ///
    /// Note: pump.fun charges 1% on each side. Atomic round-trip on the
    /// same bonding-curve state ⇒ ~2% net loss before fees. The strategy
    /// only nets positive if another buyer's tx lands BETWEEN our buy
    /// and sell — i.e., if the curve moves up by ≥2% from our buy alone.
    /// In practice this works on hyped launches where the next buyer
    /// arrives in the next ~50ms.
    pub async fn pump_fun_buy_sell_atomic(
        &self,
        mint:         solana_program::pubkey::Pubkey,
        creator_hint: solana_program::pubkey::Pubkey,
        sol_lamports: u64,
    ) -> ArbResult<String> {
        let pubkey = self.wallet.pubkey();
        // Resolve creator, mayhem flag, token_program, AND live BC reserves
        // in one round-trip. Using INITIAL_* constants for `amount` overflows
        // (Custom: 6024) when the BC has already had prior buys. The mayhem
        // flag also drifts per-BC (set at create-time from Global.mayhemEn).
        let bc_pda = pump_fun::derive_bonding_curve_pda(&mint);
        let (creator, is_token_2022, vsr, vtr, is_mayhem) = match self.helius_rpc
            .get_multiple_accounts(&[bc_pda, mint])
            .await
        {
            Ok(vec) => {
                let (creator, vsr, vtr, mayhem) = vec.first().and_then(|o| o.as_ref())
                    .and_then(|s| pump_fun::BondingCurveState::try_parse(&s.data))
                    .map(|st| (st.creator, st.virtual_sol_reserves, st.virtual_token_reserves, st.is_mayhem_mode))
                    .unwrap_or((
                        creator_hint,
                        pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                        pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                        true, // fresh BCs created today inherit Global.mayhem=true
                    ));
                // Default to Token-2022 when mint info isn't visible yet —
                // fresh pump.fun mints created via `createV2` are Token-2022
                // (the `create` legacy path is now rare). Wrong guess → ATA
                // create errors with IncorrectProgramId — was 59% of v93 fails.
                let is_22 = vec.get(1).and_then(|o| o.as_ref())
                    .map(|s| s.owner == crate::raw_tx::TOKEN_2022_PROGRAM)
                    .unwrap_or(true);
                (creator, is_22, vsr, vtr, mayhem)
            }
            Err(_) => (
                creator_hint, true,
                pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                true,
            ),
        };
        let skip_mayhem = std::env::var("PUMP_FUN_SKIP_MAYHEM").as_deref() != Ok("false");
        if skip_mayhem && is_mayhem {
            info!(target: "scout::pump_fun_exec",
                  mint = %mint, "🚫 mayhem mint — skipping (atomic)");
            return Err(ArbError::Invariant("pump_fun atomic: skipped — mayhem mint"));
        }
        let amount = pump_fun::compute_buy_tokens(vsr, vtr, sol_lamports);
        if amount == 0 {
            return Err(ArbError::Invariant("pump_fun atomic: amount = 0"));
        }
        // Buy the full computed amount; sell drops 20% for state-drift
        // safety. At 100k-lamport input the BC moves a larger PERCENTAGE
        // between our quote and our tx landing — was 36% of v93 reverts
        // with Custom: 6023 NotEnoughTokensToSell at the 5% buffer.
        let buy_amount  = amount;
        let sell_amount = amount.saturating_mul(80) / 100;
        // 20% slippage cap on the buy: the BC moves fast between our
        // quote and execution, and a tight cap floods us with 6002
        // tooMuchSolRequired reverts. 20% is the SDK default.
        let max_sol_cost = sol_lamports.saturating_mul(120) / 100;
        // 51% PROFIT THRESHOLD: SELL succeeds only if curve moved up enough
        // that we get back at least 1.51× our input. In atomic-tx mode this
        // means the WHOLE tx reverts unless the on-chain BC state at
        // landing-slot is ~51%+ higher than when we fetched it. Net cost
        // per dropped attempt: just the tx fee + Helius tip. The operator
        // wants this strict threshold to filter out the bleed-prone
        // micro-spread round-trips that lost 70% of the wallet earlier.
        let min_sol_out  = sol_lamports.saturating_mul(151) / 100;

        // Pick fee_recipient based on this BC's `isMayhemMode`. Mayhem BCs
        // require the RESERVED list; non-mayhem BCs require the NORMAL
        // list. Wrong choice yields `Custom: 6000`. Rotate via tx counter
        // so we spread load across the 8 entries.
        // Buyback recipient: rotate independently through the 8 from
        // Global.buybackFeeRecipients (per SDK 1.35).
        let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
        let fee_recipient = if is_mayhem {
            pump_fun::RESERVED_FEE_RECIPIENTS[n % pump_fun::RESERVED_FEE_RECIPIENTS.len()]
        } else {
            pump_fun::NORMAL_FEE_RECIPIENTS[n % pump_fun::NORMAL_FEE_RECIPIENTS.len()]
        };
        let buyback_recipient = pump_fun::BUYBACK_FEE_RECIPIENTS[
            (n.wrapping_add(3)) % pump_fun::BUYBACK_FEE_RECIPIENTS.len()
        ];
        let accs = PumpFunBuyAccounts {
            user:          pubkey,
            mint,
            creator,
            fee_recipient,
            bonding_curve_v2_seed_ok: false,
            is_token_2022,
            buyback_recipient,
        };
        // EMERGENCY GATE 2026-05-11: actual real-world ATA-rent cost is
        // ~2M lamports per landing (not the ~0.0004 SOL I estimated).
        // Wallet hit 0.0094 SOL at 26% landing rate → bleeding $1/min.
        // Hard-gate on PUMP_FUN_EXECUTE_ENABLED before touching the IXs.
        // Phase 4 update: also allow when AXIOM_TRENDING_ENABLED=true since
        // the Axiom-trending sniper routes through this same atomic path now.
        let hodl_ok  = std::env::var("PUMP_FUN_EXECUTE_ENABLED").as_deref() == Ok("true");
        let axiom_ok = std::env::var("AXIOM_TRENDING_ENABLED").as_deref()  == Ok("true");
        if !hodl_ok && !axiom_ok {
            return Err(ArbError::Invariant("pump_fun atomic disabled via env"));
        }
        let buy  = pump_buy_ix(&accs, buy_amount,  max_sol_cost);
        let sell = pump_sell_ix(&accs, sell_amount, min_sol_out);

        // IX order: CreateATA → Buy → Sell. The original order had Sell BEFORE
        // Buy which only worked for the HODL Create-flow where the bot is the
        // mint creator and the ATA already exists. For Axiom-trending we hit
        // brand-new mints; the ATA doesn't exist, so Sell-first reverted with
        // Anchor Custom 3012 = AccountNotInitialized. The fix: idempotent ATA
        // creation FIRST so the buy can deposit into it, then sell consumes
        // what was just bought.
        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(8);
        // Priority fee: configurable via env. Competitors using FLASHX program
        // pay ~205k lamports total priority (=~$0.02 per tx). Our default 10k
        // micro-lamports/CU × 600k CU = 6k lamports priority = $0.0006 → we
        // were 40× below mid-pack and getting outbid every time. The right
        // value is 333_333 μlamps/CU (= 200k lamports priority on 600k CU).
        // Env override: AXIOM_PRIORITY_MICRO_LAMPORTS.
        let cu_price: u64 = std::env::var("AXIOM_PRIORITY_MICRO_LAMPORTS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(333_333);
        all.push(set_compute_unit_limit_ix(600_000));
        all.push(set_compute_unit_price_ix(cu_price));
        all.push(if is_token_2022 {
            create_ata_idempotent_ix_2022(pubkey, pubkey, mint)
        } else {
            create_ata_idempotent_ix(pubkey, pubkey, mint)
        });
        all.push(buy);
        all.push(sell);

        // Dynamic Helius Sender tip — floor at the Helius minimum (200k),
        // then bid up to live p75 of Jito's landed-tip distribution so we
        // sit above mid-pack during snipe storms. Env override wins if set.
        let p75 = self.tip_floor.p75_lamports();
        let p50 = self.tip_floor.p50_lamports();
        let dyn_floor = p75.max(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        let helius_tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(dyn_floor);
        if !self.cfg.use_jito && helius_tip > 0 {
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
            all.push(system_transfer_ix(pubkey, tip_account, helius_tip));
        }

        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;
        let signed = if let Some(alt_pk) = self.cfg.alt_pubkey {
            build_and_sign_v0_tx_b64(
                &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                &[], self.wallet.bytes(),
            )?
        } else {
            build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())?
        };

        let url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_SENDER_URL not set".into()))?;
        let key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_API_KEY not set".into()))?;
        let sig = self.jito.send_via_helius_sender(&url, &key, &signed).await?;

        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              sig = %sig, mint = %mint, sol_in = sol_lamports,
              buy_amount, sell_amount, min_sol_out,
              tip = helius_tip, tip_p50 = p50, tip_p75 = p75,
              "🎰 ATOMIC PUMP.FUN buy+sell SENT (small-input v93)");
        Ok(sig)
    }

    /// Pump.fun HODL-mode buy: same on-chain layout as `pump_fun_buy_sell_atomic`
    /// minus the sell leg. Gated by the optional `PumpSnipeBudget` (hard cap on
    /// total lamports sent, plus per-mint dedup so repeated Create events don't
    /// double-buy). Returns `Ok(None)` when the budget gate or dedup short-circuits
    /// the buy — the caller logs at info level, not warn, since these are normal
    /// operating outcomes once the cap is hit.
    pub async fn pump_fun_buy_only(
        &self,
        mint:         solana_program::pubkey::Pubkey,
        creator_hint: solana_program::pubkey::Pubkey,
        sol_lamports: u64,
    ) -> ArbResult<Option<String>> {
        // EMERGENCY GATE: enabled by either the HODL pipeline
        // (PUMP_FUN_EXECUTE_ENABLED) or the Axiom-trending sniper
        // (AXIOM_TRENDING_ENABLED). The Create-detection spawn point in
        // scout.rs still gates itself on PUMP_FUN_EXECUTE_ENABLED to keep
        // HODL behaviour orthogonal to trending.
        let hodl_ok  = std::env::var("PUMP_FUN_EXECUTE_ENABLED").as_deref() == Ok("true");
        let axiom_ok = std::env::var("AXIOM_TRENDING_ENABLED").as_deref()  == Ok("true");
        if !hodl_ok && !axiom_ok {
            return Err(ArbError::Invariant("pump_fun buy-only disabled via env"));
        }

        // Budget gate FIRST so the gas of an RPC fan-out below is only spent
        // when we're actually going to send. Reservation is final — failed
        // on-chain sends still consume budget (intentional: hard ceiling on
        // outgoing lamports, not on successful spends).
        let mint_b58 = mint.to_string();
        if let Some(budget) = &self.pump_sniper_budget {
            match budget.try_reserve(&mint_b58, sol_lamports) {
                Ok(new_total) => info!(target: "scout::pump_fun_exec",
                    mint = %mint, reserved = sol_lamports,
                    spent_after = new_total, cap = budget.cap_lamports(),
                    "🛒 HODL budget reserved"),
                Err(ReserveSkip::BudgetExhausted { spent, cap }) => {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, spent, cap, ask = sol_lamports,
                          "💤 HODL budget exhausted — skipping buy");
                    return Ok(None);
                }
                Err(ReserveSkip::AlreadyBought) => {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, "💤 mint already bought — dedup skip");
                    return Ok(None);
                }
            }
        }

        let pubkey = self.wallet.pubkey();
        let bc_pda = pump_fun::derive_bonding_curve_pda(&mint);
        // Resolve creator + live BC reserves. The mint's owner is NOT trusted
        // here — the Yellowstone Create event fires ~50-150ms before the slot
        // commits, so the RPC's "confirmed" view of the mint is often empty
        // and the old `unwrap_or(true)` defaulted us to Token-2022 even for
        // legacy `create` mints (5LdayN…GVF tx 2026-05-11 was a legacy mint
        // killed by that default). Smart-C below verifies via simulation.
        let (creator, vsr, vtr, is_mayhem) = match self.helius_rpc
            .get_multiple_accounts(&[bc_pda, mint])
            .await
        {
            Ok(vec) => vec.first().and_then(|o| o.as_ref())
                .and_then(|s| pump_fun::BondingCurveState::try_parse(&s.data))
                .map(|st| (st.creator, st.virtual_sol_reserves, st.virtual_token_reserves, st.is_mayhem_mode))
                .unwrap_or((
                    creator_hint,
                    pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                    pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                    true,
                )),
            Err(_) => (
                creator_hint,
                pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                true,
            ),
        };
        // Skip mayhem BCs — operator policy. Default ON; set
        // `PUMP_FUN_SKIP_MAYHEM=false` to disable. Releases the budget
        // reservation made above so the slot can be reused for a non-mayhem
        // mint instead of being burned on a no-op.
        let skip_mayhem = std::env::var("PUMP_FUN_SKIP_MAYHEM").as_deref() != Ok("false");
        if skip_mayhem && is_mayhem {
            info!(target: "scout::pump_fun_exec",
                  mint = %mint, "🚫 mayhem mint — skipping (buy_only)");
            if let Some(budget) = &self.pump_sniper_budget {
                budget.release_reservation(&mint_b58, sol_lamports);
            }
            return Ok(None);
        }
        let amount = pump_fun::compute_buy_tokens(vsr, vtr, sol_lamports);
        if amount == 0 {
            return Err(ArbError::Invariant("pump_fun buy-only: amount = 0"));
        }
        let max_sol_cost = sol_lamports.saturating_mul(120) / 100;

        let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
        let fee_recipient = if is_mayhem {
            pump_fun::RESERVED_FEE_RECIPIENTS[n % pump_fun::RESERVED_FEE_RECIPIENTS.len()]
        } else {
            pump_fun::NORMAL_FEE_RECIPIENTS[n % pump_fun::NORMAL_FEE_RECIPIENTS.len()]
        };
        let buyback_recipient = pump_fun::BUYBACK_FEE_RECIPIENTS[
            (n.wrapping_add(3)) % pump_fun::BUYBACK_FEE_RECIPIENTS.len()
        ];

        let p75 = self.tip_floor.p75_lamports();
        let p50 = self.tip_floor.p50_lamports();
        let dyn_floor = p75.max(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        let helius_tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(dyn_floor);

        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;

        // Build a fully-signed tx for a given choice of token program. Used
        // by Smart-C to construct a classic-ATA candidate, simulate it, and
        // (if the mint turns out to be Token-2022) rebuild with the 2022 ATA.
        let build_signed = |is_22: bool| -> ArbResult<String> {
            let accs = PumpFunBuyAccounts {
                user:          pubkey,
                mint,
                creator,
                fee_recipient,
                bonding_curve_v2_seed_ok: false,
                is_token_2022: is_22,
                buyback_recipient,
            };
            let buy = pump_buy_ix(&accs, amount, max_sol_cost);
            let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(6);
            all.push(set_compute_unit_limit_ix(300_000));
            all.push(set_compute_unit_price_ix(10_000));
            all.push(if is_22 {
                create_ata_idempotent_ix_2022(pubkey, pubkey, mint)
            } else {
                create_ata_idempotent_ix(pubkey, pubkey, mint)
            });
            all.push(buy);
            if !self.cfg.use_jito && helius_tip > 0 {
                let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
                let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
                all.push(system_transfer_ix(pubkey, tip_account, helius_tip));
            }
            if let Some(alt_pk) = self.cfg.alt_pubkey {
                build_and_sign_v0_tx_b64(
                    &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                    &[], self.wallet.bytes(),
                )
            } else {
                build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())
            }
        };

        // Smart-C: always start with classic SPL Token. Simulate. If logs
        // show "Invalid Mint" or "IncorrectProgramId" the mint is actually
        // Token-2022 — rebuild with the 2022 ATA creator and the right
        // token_program reference inside the pump.fun buy IX. Costs one
        // extra simulateTransaction RPC call (~50-100ms) but avoids the
        // double-fee of a literal classic-then-2022 dual send.
        let classic_signed = build_signed(false)?;
        let (signed, used_2022) = match self.helius_rpc
            .simulate_transaction(&classic_signed)
            .await
        {
            Ok(sim) => {
                let invalid_mint = sim.logs.iter().any(|l|
                    l.contains("Error: Invalid Mint") || l.contains("IncorrectProgramId"));
                if !sim.err_is_null && invalid_mint {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint,
                          "🔀 sim says classic ATA rejected — switching to Token-2022");
                    (build_signed(true)?, true)
                } else {
                    (classic_signed, false)
                }
            }
            Err(e) => {
                warn!(target: "scout::pump_fun_exec",
                      mint = %mint, error = %e,
                      "simulateTransaction failed — sending classic-ATA build as-is");
                (classic_signed, false)
            }
        };

        // Throttle gate: serialise HODL sends so at least N ms passes between
        // consecutive sends. Concurrent tasks queue on this mutex; the first
        // one through (or one whose previous send was >= N ms ago) proceeds
        // immediately, others sleep the remainder. Without the gate the
        // wallet bleeds at full Yellowstone burst rate (~16+ buys/min).
        let min_interval_ms: u64 = std::env::var("PUMP_FUN_BUY_MIN_INTERVAL_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1000);
        let mut last_guard = self.pump_buy_last_send.lock().await;
        if min_interval_ms > 0 {
            let target = Duration::from_millis(min_interval_ms);
            if let Some(prev) = *last_guard {
                let elapsed = prev.elapsed();
                if elapsed < target {
                    let wait = target - elapsed;
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, wait_ms = wait.as_millis() as u64,
                          "⏱ HODL throttle — waiting for send slot");
                    tokio::time::sleep(wait).await;
                }
            }
        }

        let url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_SENDER_URL not set".into()))?;
        let key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_API_KEY not set".into()))?;
        let sig = self.jito.send_via_helius_sender(&url, &key, &signed).await?;
        // Stamp the slot AFTER the send returns so the next waiter measures
        // from the actual send moment (not from the queue entry).
        *last_guard = Some(Instant::now());
        drop(last_guard);

        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              sig = %sig, mint = %mint, sol_in = sol_lamports,
              buy_amount = amount, token_2022 = used_2022,
              tip = helius_tip, tip_p50 = p50, tip_p75 = p75,
              "🚀 HODL PUMP.FUN buy-only SENT");
        Ok(Some(sig))
    }

    /// Poll-then-sell HODL: send BUY, wait for it to land, sleep an extra
    /// `PUMP_FUN_POST_BUY_DELAY_MS` so other buyers can lift the bonding-
    /// curve, then send SELL. Unlike the Jito bundle path (which forces
    /// atomic same-slot execution and therefore cannot capture spread),
    /// this lets other txs touch the BC between our BUY and SELL — the
    /// only way the round-trip can actually net positive SOL.
    ///
    /// Returns `Ok(Some((buy_sig, Some(sell_sig))))` on a full round-trip,
    /// `Ok(Some((buy_sig, None)))` when the buy landed but the sell was
    /// skipped (buy failed on chain, poll timeout, or sell send error),
    /// and `Ok(None)` when the budget gate or dedup short-circuited the
    /// whole call before anything was sent.
    pub async fn pump_fun_buy_then_sell(
        &self,
        mint:         solana_program::pubkey::Pubkey,
        creator_hint: solana_program::pubkey::Pubkey,
        sol_lamports: u64,
    ) -> ArbResult<Option<(String, Option<String>)>> {
        if std::env::var("PUMP_FUN_EXECUTE_ENABLED").as_deref() != Ok("true") {
            return Err(ArbError::Invariant("pump_fun poll-sell disabled via env"));
        }

        let mint_b58 = mint.to_string();
        if let Some(budget) = &self.pump_sniper_budget {
            match budget.try_reserve(&mint_b58, sol_lamports) {
                Ok(new_total) => info!(target: "scout::pump_fun_exec",
                    mint = %mint, reserved = sol_lamports,
                    spent_after = new_total, cap = budget.cap_lamports(),
                    "🛒 HODL poll-sell budget reserved"),
                Err(ReserveSkip::BudgetExhausted { spent, cap }) => {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, spent, cap, ask = sol_lamports,
                          "💤 HODL poll-sell: budget exhausted");
                    return Ok(None);
                }
                Err(ReserveSkip::AlreadyBought) => {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, "💤 HODL poll-sell: mint already bought");
                    return Ok(None);
                }
            }
        }

        let pubkey = self.wallet.pubkey();
        let bc_pda = pump_fun::derive_bonding_curve_pda(&mint);
        let (creator, initial_is_22, vsr, vtr, is_mayhem) = match self.helius_rpc
            .get_multiple_accounts(&[bc_pda, mint])
            .await
        {
            Ok(vec) => {
                let (creator, vsr, vtr, mayhem) = vec.first().and_then(|o| o.as_ref())
                    .and_then(|s| pump_fun::BondingCurveState::try_parse(&s.data))
                    .map(|st| (st.creator, st.virtual_sol_reserves, st.virtual_token_reserves, st.is_mayhem_mode))
                    .unwrap_or((
                        creator_hint,
                        pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                        pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                        true,
                    ));
                let is_22 = vec.get(1).and_then(|o| o.as_ref())
                    .map(|s| s.owner == crate::raw_tx::TOKEN_2022_PROGRAM)
                    .unwrap_or(false);
                (creator, is_22, vsr, vtr, mayhem)
            }
            Err(_) => (
                creator_hint, false,
                pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                true,
            ),
        };
        let skip_mayhem = std::env::var("PUMP_FUN_SKIP_MAYHEM").as_deref() != Ok("false");
        if skip_mayhem && is_mayhem {
            info!(target: "scout::pump_fun_exec",
                  mint = %mint, "🚫 mayhem mint — skipping (poll-sell)");
            if let Some(budget) = &self.pump_sniper_budget {
                budget.release_reservation(&mint_b58, sol_lamports);
            }
            return Ok(None);
        }
        let buy_amount   = pump_fun::compute_buy_tokens(vsr, vtr, sol_lamports);
        if buy_amount == 0 {
            return Err(ArbError::Invariant("pump_fun poll-sell: amount = 0"));
        }
        // 80% sell cushion matches the atomic strategy — protects against
        // state-drift between our computed buy_amount and the actual tokens
        // received (other buyers can shift the curve mid-flight).
        let sell_amount  = buy_amount.saturating_mul(80) / 100;
        let max_sol_cost = sol_lamports.saturating_mul(120) / 100;
        let min_sol_out  = sol_lamports.saturating_mul(50) / 100;

        let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
        let fee_recipient = if is_mayhem {
            pump_fun::RESERVED_FEE_RECIPIENTS[n % pump_fun::RESERVED_FEE_RECIPIENTS.len()]
        } else {
            pump_fun::NORMAL_FEE_RECIPIENTS[n % pump_fun::NORMAL_FEE_RECIPIENTS.len()]
        };
        let buyback_recipient = pump_fun::BUYBACK_FEE_RECIPIENTS[
            (n.wrapping_add(3)) % pump_fun::BUYBACK_FEE_RECIPIENTS.len()
        ];

        let p75 = self.tip_floor.p75_lamports();
        let dyn_floor = p75.max(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        let helius_tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(dyn_floor);

        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;

        // Build BUY (Smart-C: classic first, then 2022 if sim says Invalid Mint).
        let build_buy = |is_22: bool| -> ArbResult<String> {
            let accs = PumpFunBuyAccounts {
                user: pubkey, mint, creator, fee_recipient,
                bonding_curve_v2_seed_ok: false,
                is_token_2022: is_22, buyback_recipient,
            };
            let buy = pump_buy_ix(&accs, buy_amount, max_sol_cost);
            let mut all = Vec::with_capacity(6);
            all.push(set_compute_unit_limit_ix(300_000));
            all.push(set_compute_unit_price_ix(10_000));
            all.push(if is_22 {
                create_ata_idempotent_ix_2022(pubkey, pubkey, mint)
            } else {
                create_ata_idempotent_ix(pubkey, pubkey, mint)
            });
            all.push(buy);
            if !self.cfg.use_jito && helius_tip > 0 {
                let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
                let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
                all.push(system_transfer_ix(pubkey, tip_account, helius_tip));
            }
            if let Some(alt_pk) = self.cfg.alt_pubkey {
                build_and_sign_v0_tx_b64(
                    &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                    &[], self.wallet.bytes(),
                )
            } else {
                build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())
            }
        };

        let classic_signed = build_buy(false)?;
        let (signed, used_2022) = match self.helius_rpc
            .simulate_transaction(&classic_signed)
            .await
        {
            Ok(sim) => {
                let invalid_mint = sim.logs.iter().any(|l|
                    l.contains("Error: Invalid Mint") || l.contains("IncorrectProgramId"));
                if !sim.err_is_null && invalid_mint {
                    info!(target: "scout::pump_fun_exec", mint = %mint,
                          "🔀 sim: classic-ATA rejected, switching to Token-2022");
                    (build_buy(true)?, true)
                } else {
                    (classic_signed, false)
                }
            }
            Err(e) => {
                warn!(target: "scout::pump_fun_exec", mint = %mint, error = %e,
                      "sim failed — sending classic-ATA build");
                (classic_signed, initial_is_22)
            }
        };

        // Throttle gate shared with buy_only.
        let min_interval_ms: u64 = std::env::var("PUMP_FUN_BUY_MIN_INTERVAL_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1000);
        let mut last_guard = self.pump_buy_last_send.lock().await;
        if min_interval_ms > 0 {
            let target = Duration::from_millis(min_interval_ms);
            if let Some(prev) = *last_guard {
                let elapsed = prev.elapsed();
                if elapsed < target {
                    let wait = target - elapsed;
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, wait_ms = wait.as_millis() as u64,
                          "⏱ HODL poll-sell throttle — waiting for send slot");
                    tokio::time::sleep(wait).await;
                }
            }
        }

        let url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_SENDER_URL not set".into()))?;
        let key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_API_KEY not set".into()))?;
        let buy_sig = self.jito.send_via_helius_sender(&url, &key, &signed).await?;
        *last_guard = Some(Instant::now());
        drop(last_guard);

        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              buy_sig = %buy_sig, mint = %mint, sol_in = sol_lamports,
              buy_amount, token_2022 = used_2022,
              "🚀 HODL poll-sell BUY SENT — waiting for confirmation");

        // Poll for BUY confirmation. ~10s max with 500ms intervals.
        let mut buy_landed_ok = false;
        for i in 0..20u32 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            match self.helius_rpc.signature_status(&buy_sig).await {
                Ok(Some(true)) => {
                    info!(target: "scout::pump_fun_exec",
                          buy_sig = %buy_sig, mint = %mint, poll_iter = i,
                          "✅ BUY confirmed");
                    buy_landed_ok = true;
                    break;
                }
                Ok(Some(false)) => {
                    warn!(target: "scout::pump_fun_exec",
                          buy_sig = %buy_sig, mint = %mint, poll_iter = i,
                          "❌ BUY landed but failed — skipping SELL");
                    return Ok(Some((buy_sig, None)));
                }
                Ok(None) => continue,
                Err(e) => warn!(target: "scout::pump_fun_exec",
                                buy_sig = %buy_sig, mint = %mint, error = %e,
                                "poll status RPC error — retrying"),
            }
        }
        if !buy_landed_ok {
            warn!(target: "scout::pump_fun_exec",
                  buy_sig = %buy_sig, mint = %mint,
                  "⏰ BUY poll timeout (10s) — skipping SELL");
            return Ok(Some((buy_sig, None)));
        }

        // Post-buy delay — lets other Yellowstone buyers' txs land and lift
        // the bonding curve before our SELL runs. The whole point of this
        // path vs. the bundle path.
        let post_delay_ms: u64 = std::env::var("PUMP_FUN_POST_BUY_DELAY_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(50);
        if post_delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(post_delay_ms)).await;
        }

        // Build SELL using same token-program flag (matches the ATA the BUY
        // created). New blockhash to avoid expiry, no ATA create (already done).
        let sell_accs = PumpFunBuyAccounts {
            user: pubkey, mint, creator, fee_recipient,
            bonding_curve_v2_seed_ok: false,
            is_token_2022: used_2022, buyback_recipient,
        };
        let sell_ix = pump_sell_ix(&sell_accs, sell_amount, min_sol_out);
        let mut sell_all = Vec::with_capacity(5);
        sell_all.push(set_compute_unit_limit_ix(250_000));
        sell_all.push(set_compute_unit_price_ix(10_000));
        sell_all.push(sell_ix);
        if !self.cfg.use_jito && helius_tip > 0 {
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
            sell_all.push(system_transfer_ix(pubkey, tip_account, helius_tip));
        }

        let sell_blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let sell_signed = if let Some(alt_pk) = self.cfg.alt_pubkey {
            build_and_sign_v0_tx_b64(
                &pubkey, &sell_all, &sell_blockhash, &alt_pk, alt_addrs,
                &[], self.wallet.bytes(),
            )?
        } else {
            build_and_sign_legacy_tx_b64(&pubkey, &sell_all, &sell_blockhash, self.wallet.bytes())?
        };

        let sell_sig = match self.jito.send_via_helius_sender(&url, &key, &sell_signed).await {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "scout::pump_fun_exec",
                      buy_sig = %buy_sig, mint = %mint, error = %e,
                      "SELL send failed — holding tokens");
                return Ok(Some((buy_sig, None)));
            }
        };
        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              buy_sig = %buy_sig, sell_sig = %sell_sig, mint = %mint,
              sell_amount, min_sol_out, post_delay_ms,
              "💰 HODL poll-sell SELL SENT");
        Ok(Some((buy_sig, Some(sell_sig))))
    }

    /// Bundle-mode HODL: BUY and SELL go to Jito as a 2-tx bundle so they
    /// land in the same slot in deterministic order (BUY first). The strategy
    /// only nets positive when at least one other buyer's tx lands in the
    /// same slot AFTER our BUY and BEFORE our SELL — the existing 1-tx
    /// `pump_fun_buy_sell_atomic` has the same property but here the two
    /// halves are separate transactions, which lets us size CU budgets per
    /// half and gives Jito's auction (rather than Helius Sender) the routing.
    ///
    /// Returns the bundle UUID on success. Returns `Ok(None)` when budget
    /// or dedup short-circuits the send (same skip semantics as buy_only).
    pub async fn pump_fun_buy_sell_bundle(
        &self,
        mint:         solana_program::pubkey::Pubkey,
        creator_hint: solana_program::pubkey::Pubkey,
        sol_lamports: u64,
    ) -> ArbResult<Option<String>> {
        if std::env::var("PUMP_FUN_EXECUTE_ENABLED").as_deref() != Ok("true") {
            return Err(ArbError::Invariant("pump_fun bundle disabled via env"));
        }

        let mint_b58 = mint.to_string();
        if let Some(budget) = &self.pump_sniper_budget {
            match budget.try_reserve(&mint_b58, sol_lamports) {
                Ok(new_total) => info!(target: "scout::pump_fun_exec",
                    mint = %mint, reserved = sol_lamports,
                    spent_after = new_total, cap = budget.cap_lamports(),
                    "🛒 HODL bundle budget reserved"),
                Err(ReserveSkip::BudgetExhausted { spent, cap }) => {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, spent, cap, ask = sol_lamports,
                          "💤 HODL bundle: budget exhausted");
                    return Ok(None);
                }
                Err(ReserveSkip::AlreadyBought) => {
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, "💤 HODL bundle: mint already bought");
                    return Ok(None);
                }
            }
        }

        let pubkey = self.wallet.pubkey();
        let bc_pda = pump_fun::derive_bonding_curve_pda(&mint);
        let (creator, is_token_2022, vsr, vtr, is_mayhem) = match self.helius_rpc
            .get_multiple_accounts(&[bc_pda, mint])
            .await
        {
            Ok(vec) => {
                let (creator, vsr, vtr, mayhem) = vec.first().and_then(|o| o.as_ref())
                    .and_then(|s| pump_fun::BondingCurveState::try_parse(&s.data))
                    .map(|st| (st.creator, st.virtual_sol_reserves, st.virtual_token_reserves, st.is_mayhem_mode))
                    .unwrap_or((
                        creator_hint,
                        pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                        pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                        true,
                    ));
                let is_22 = vec.get(1).and_then(|o| o.as_ref())
                    .map(|s| s.owner == crate::raw_tx::TOKEN_2022_PROGRAM)
                    .unwrap_or(false);
                (creator, is_22, vsr, vtr, mayhem)
            }
            Err(_) => (
                creator_hint, false,
                pump_fun::INITIAL_VIRTUAL_SOL_RESERVES as u64,
                pump_fun::INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
                true,
            ),
        };
        let skip_mayhem = std::env::var("PUMP_FUN_SKIP_MAYHEM").as_deref() != Ok("false");
        if skip_mayhem && is_mayhem {
            info!(target: "scout::pump_fun_exec",
                  mint = %mint, "🚫 mayhem mint — skipping (bundle)");
            if let Some(budget) = &self.pump_sniper_budget {
                budget.release_reservation(&mint_b58, sol_lamports);
            }
            return Ok(None);
        }
        let amount = pump_fun::compute_buy_tokens(vsr, vtr, sol_lamports);
        if amount == 0 {
            return Err(ArbError::Invariant("pump_fun bundle: amount = 0"));
        }
        let buy_amount   = amount;
        let sell_amount  = amount.saturating_mul(80) / 100;
        let max_sol_cost = sol_lamports.saturating_mul(120) / 100;
        // 51% PROFIT THRESHOLD — see comment in pump_fun_buy_sell_atomic.
        // Same idea here: bundle reverts (no spending) unless the bonding
        // curve moved up 51%+ between our quote fetch and the bundle's
        // landing slot. Filters out the bleed-prone tight-spread round-trips.
        let min_sol_out  = sol_lamports.saturating_mul(151) / 100;

        let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
        let fee_recipient = if is_mayhem {
            pump_fun::RESERVED_FEE_RECIPIENTS[n % pump_fun::RESERVED_FEE_RECIPIENTS.len()]
        } else {
            pump_fun::NORMAL_FEE_RECIPIENTS[n % pump_fun::NORMAL_FEE_RECIPIENTS.len()]
        };
        let buyback_recipient = pump_fun::BUYBACK_FEE_RECIPIENTS[
            (n.wrapping_add(3)) % pump_fun::BUYBACK_FEE_RECIPIENTS.len()
        ];
        let accs = PumpFunBuyAccounts {
            user: pubkey,
            mint,
            creator,
            fee_recipient,
            bonding_curve_v2_seed_ok: false,
            is_token_2022,
            buyback_recipient,
        };

        // Tip sizing for the Jito bundle. Floor at 5_000 (Jito's accepted
        // minimum) and bid up to live p75 of landed tips so the bundle has
        // a fighting chance against same-slot competition. Placed in SELL
        // leg so the auction sees a positive tip even if BUY is dropped.
        let p75 = self.tip_floor.p75_lamports();
        let p50 = self.tip_floor.p50_lamports();
        let jito_tip: u64 = std::env::var("PUMP_FUN_JITO_BUNDLE_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(p75.max(5_000));

        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;

        // ── BUY tx ────────────────────────────────────────────────────
        let buy_ix = pump_buy_ix(&accs, buy_amount, max_sol_cost);
        let mut buy_all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(5);
        buy_all.push(set_compute_unit_limit_ix(300_000));
        buy_all.push(set_compute_unit_price_ix(10_000));
        buy_all.push(if is_token_2022 {
            create_ata_idempotent_ix_2022(pubkey, pubkey, mint)
        } else {
            create_ata_idempotent_ix(pubkey, pubkey, mint)
        });
        buy_all.push(buy_ix);

        // ── SELL tx ───────────────────────────────────────────────────
        let sell_ix = pump_sell_ix(&accs, sell_amount, min_sol_out);
        let tip_account = JITO_TIP_ACCOUNTS[n % JITO_TIP_ACCOUNTS.len()];
        let mut sell_all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(5);
        sell_all.push(set_compute_unit_limit_ix(250_000));
        sell_all.push(set_compute_unit_price_ix(10_000));
        sell_all.push(sell_ix);
        sell_all.push(system_transfer_ix(pubkey, tip_account, jito_tip));

        // Force the chosen Jito tip account to stay in STATIC keys (not
        // looked up via the ALT). If it's resolved via the operator's ALT
        // it may be encoded as read-only, and Jito then rejects the bundle:
        //   "Bundles must write lock at least one tip account to be eligible
        //   for the auction."
        // Listing it in `force_static` preserves the writable bit set by
        // `system_transfer_ix`. Harmless for the BUY leg (it doesn't
        // reference the tip account, so force_static there is a no-op).
        let force_static_keys = [tip_account];
        let sign = |ixs: &[solana_program::instruction::Instruction]| -> ArbResult<String> {
            if let Some(alt_pk) = self.cfg.alt_pubkey {
                build_and_sign_v0_tx_b64(
                    &pubkey, ixs, &blockhash, &alt_pk, alt_addrs,
                    &force_static_keys, self.wallet.bytes(),
                )
            } else {
                build_and_sign_legacy_tx_b64(&pubkey, ixs, &blockhash, self.wallet.bytes())
            }
        };
        let buy_signed  = sign(&buy_all)?;
        let sell_signed = sign(&sell_all)?;

        // Throttle gate shared with buy_only — same Mutex<Option<Instant>>,
        // so bundle sends and buy_only sends serialise together when both
        // paths are active.
        let min_interval_ms: u64 = std::env::var("PUMP_FUN_BUY_MIN_INTERVAL_MS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1000);
        let mut last_guard = self.pump_buy_last_send.lock().await;
        if min_interval_ms > 0 {
            let target = Duration::from_millis(min_interval_ms);
            if let Some(prev) = *last_guard {
                let elapsed = prev.elapsed();
                if elapsed < target {
                    let wait = target - elapsed;
                    info!(target: "scout::pump_fun_exec",
                          mint = %mint, wait_ms = wait.as_millis() as u64,
                          "⏱ HODL bundle throttle — waiting for send slot");
                    tokio::time::sleep(wait).await;
                }
            }
        }

        let bundle_id = match self.jito.send_bundle(&[buy_signed, sell_signed]).await {
            Ok(id) => id,
            Err(e) => {
                warn!(target: "scout::pump_fun_exec", mint = %mint, error = %e,
                      "pump_fun bundle: sendBundle failed — releasing budget");
                // Release the reservation: the bundle was REJECTED at submission
                // (not landed on-chain), so no SOL went out. Without the release
                // the $20 cap exhausts on rejected bundles without real spending.
                drop(last_guard);
                if let Some(budget) = &self.pump_sniper_budget {
                    budget.release_reservation(&mint_b58, sol_lamports);
                }
                return Err(e);
            }
        };
        *last_guard = Some(Instant::now());
        drop(last_guard);

        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              bundle_id = %bundle_id, mint = %mint, sol_in = sol_lamports,
              buy_amount, sell_amount, min_sol_out,
              jito_tip, tip_p50 = p50, tip_p75 = p75,
              token_2022 = is_token_2022,
              "🎰 HODL BUNDLE SENT (buy+sell in same slot, deterministic order)");
        Ok(Some(bundle_id))
    }

    /// Pump.fun bonding-curve SELL. Sells `amount` of the mint we hold for
    /// `min_sol_out` lamports floor. `creator` is the same as on the buy.
    pub async fn pump_fun_sell(
        &self,
        mint:        solana_program::pubkey::Pubkey,
        creator:     solana_program::pubkey::Pubkey,
        amount:      u64,
        min_sol_out: u64,
    ) -> ArbResult<String> {
        let pubkey = self.wallet.pubkey();
        let sell_accs = PumpFunBuyAccounts {
            user:          pubkey,
            mint,
            creator,
            fee_recipient: pump_fun::PUMP_FUN_FEE_RECIPIENT_DEFAULT,
            bonding_curve_v2_seed_ok: false,
            is_token_2022: false,
            buyback_recipient: pump_fun::BUYBACK_FEE_RECIPIENTS[0],
        };
        let sell = pump_sell_ix(&sell_accs, amount, min_sol_out);

        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(6);
        all.push(set_compute_unit_limit_ix(300_000));
        all.push(set_compute_unit_price_ix(10_000));
        all.push(sell);

        let tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        if !self.cfg.use_jito && tip > 0 {
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
            all.push(system_transfer_ix(pubkey, tip_account, tip));
        }

        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;
        let signed = if let Some(alt_pk) = self.cfg.alt_pubkey {
            build_and_sign_v0_tx_b64(
                &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                &[], self.wallet.bytes(),
            )?
        } else {
            build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())?
        };

        let url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_SENDER_URL not set".into()))?;
        let key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| ArbError::Config("HELIUS_API_KEY not set".into()))?;
        let sig = self.jito.send_via_helius_sender(&url, &key, &signed).await?;

        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::pump_fun_exec",
              sig = %sig, mint = %mint, amount, min_sol_out,
              "🚀 PUMP.FUN SELL SENT");
        Ok(sig)
    }

    /// One-way Jupiter swap from `src_mint` to `dst_mint` — `probe` units of
    /// src in, target net positive in dst-units. Treats stablecoins as 1:1
    /// USD: net = out_amount - probe.
    ///
    /// Unlike `maybe_execute_with_base` (round-trip), this fires a SINGLE
    /// Jupiter swap. After landing, the wallet's holdings flip from
    /// `src_mint` to `dst_mint`. The cross-stable polling worker alternates
    /// direction (USDC→USDT, USDT→USDC) so the wallet oscillates.
    pub async fn maybe_execute_one_way(
        &self, src_mint: Pubkey, dst_mint: Pubkey, probe: u64,
    ) -> ArbResult<()> {
        // ─── Gates ─────────────────────────────────────────────────────────
        // Jito back-off (when we use Jito) is honored implicitly via
        // back_off_until — same Mutex shared with the round-trip path.
        {
            let back_off = self.back_off_until.lock().await;
            if let Some(until) = *back_off {
                if Instant::now() < until {
                    self.metrics.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }
        let n = self.metrics.considered.fetch_add(1, Ordering::Relaxed);
        if n % self.cfg.sample_one_in_n != 0 {
            self.metrics.skipped_sample.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        {
            let mut last = self.last_attempt.lock().await;
            let now = Instant::now();
            if let Some(t) = *last {
                if now.duration_since(t) < self.cfg.min_interval {
                    self.metrics.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
            *last = Some(now);
        }
        if self.metrics.running_net_usdc.load(Ordering::Relaxed)
              < -(self.cfg.max_daily_loss_usdc as i64) {
            self.metrics.skipped_daily_cap.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // ─── Quote ─────────────────────────────────────────────────────────
        let quote = match self.jupiter_swap.quote(
            &src_mint, &dst_mint, probe, self.cfg.slippage_bps,
        ).await {
            Ok(q) => q,
            Err(e) => {
                self.metrics.jupiter_errors.fetch_add(1, Ordering::Relaxed);
                warn!(target: "scout::executor", leg = "one-way", error = %e,
                      src = %src_mint, dst = %dst_mint,
                      "Jupiter quote failed");
                return Ok(());
            }
        };
        let net = (quote.out_amount as i64) - (probe as i64);
        if net < self.cfg.min_profit_usdc as i64 {
            self.metrics.skipped_below_min.fetch_add(1, Ordering::Relaxed);
            let n_bm = self.metrics.skipped_below_min.load(Ordering::Relaxed);
            if n_bm % 50 == 0 {
                info!(target: "scout::executor",
                      src = %src_mint, dst = %dst_mint, probe,
                      out = quote.out_amount, net,
                      "🔬 one-way: Jupiter says no profit — sampled every 50th");
            }
            return Ok(());
        }
        info!(target: "scout::executor",
              src = %src_mint, dst = %dst_mint, probe,
              out = quote.out_amount, net,
              "📈 one-way Jupiter cleared min-profit — building swap");

        let pubkey = self.wallet.pubkey();
        let _guard = self.in_flight.lock().await;
        if !self.cfg.live {
            self.metrics.skipped_dry_run.fetch_add(1, Ordering::Relaxed);
            info!(target: "scout::executor", net,
                  "🟡 one-way DRY-RUN — set EXECUTOR_LIVE=true");
            return Ok(());
        }

        // ─── Build atomic tx (single Jupiter swap) ─────────────────────────
        let ixs = match self.jupiter_swap.swap_instructions(&quote, &pubkey, false).await {
            Ok(i) => i,
            Err(e) => {
                warn!(target: "scout::executor", error = %e, "one-way swap_instructions failed");
                return Ok(());
            }
        };
        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(12);
        all.push(set_compute_unit_limit_ix(400_000));
        all.push(set_compute_unit_price_ix(10_000));
        // CASH is the only Token-2022 mint we touch in cross-stable / raw
        // Jupiter swaps; use the _2022 ATA helper for it (regular helper
        // would emit InvalidMint). Jupiter's setup_ixs may also include the
        // create-ATA — both are idempotent so prepending is safe.
        let ata_ix = |m: Pubkey| if m == known::CASH {
            create_ata_idempotent_ix_2022(pubkey, pubkey, m)
        } else {
            create_ata_idempotent_ix(pubkey, pubkey, m)
        };
        all.push(ata_ix(src_mint));
        all.push(ata_ix(dst_mint));
        all.extend(ixs.setup_ixs);
        all.push(ixs.swap_ix);
        if let Some(c) = ixs.cleanup_ix { all.push(c); }

        // Helius Sender tip (single 200k lamports transfer).
        let helius_tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        if !self.cfg.use_jito && helius_tip > 0 {
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::HELIUS_TIP_ACCOUNTS
                [n % known::HELIUS_TIP_ACCOUNTS.len()];
            all.push(system_transfer_ix(pubkey, tip_account, helius_tip));
        }

        // Sign — v0 with ALT compression when configured.
        let blockhash = fetch_latest_blockhash(&self.helius_url).await
            .map_err(|e| { warn!(target: "scout::executor", error = %e,
                                 "one-way blockhash fetch failed"); e })?;
        let alt_addrs = self.resolve_alt().await;
        let signed_b64 = if let Some(alt_pk) = self.cfg.alt_pubkey {
            build_and_sign_v0_tx_b64(
                &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                &[], self.wallet.bytes(),
            ).map_err(|e| { warn!(target: "scout::executor", error = %e,
                                  "one-way v0 sign failed"); e })?
        } else {
            build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())
                .map_err(|e| { warn!(target: "scout::executor", error = %e,
                                     "one-way legacy sign failed"); e })?
        };

        // Send via Helius Sender (use_jito=false path with tip>0).
        // When tip=0, fall back to standard sendTransaction (free, no /fast).
        let helius_sender_url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty());
        let helius_api_key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty());
        let sig = if !self.cfg.use_jito && helius_tip > 0 {
            let (url, key) = match (helius_sender_url, helius_api_key) {
                (Some(u), Some(k)) => (u, k),
                _ => {
                    warn!(target: "scout::executor",
                          "one-way: HELIUS_SENDER_URL/API_KEY required when use_jito=false");
                    return Ok(());
                }
            };
            self.jito.send_via_helius_sender(&url, &key, &signed_b64).await
                .map_err(|e| {
                    warn!(target: "scout::executor", error = %e, "one-way helius-sender failed");
                    e
                })?
        } else {
            self.helius_rpc.send_transaction(&signed_b64, false).await
                .map_err(|e| {
                    warn!(target: "scout::executor", error = %e, "one-way sendTransaction failed");
                    e
                })?
        };
        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::executor",
              sig = %sig, src = %src_mint, dst = %dst_mint,
              probe, expected_net = net,
              "🚀 ONE-WAY ARB SENT");
        *self.metrics.last_action_at.lock().unwrap() = Some(Utc::now());

        // Background poll for landing.
        let rpc = self.helius_rpc.clone();
        let metrics = Arc::clone(&self.metrics);
        let s = sig.clone();
        tokio::spawn(async move {
            for attempt in 0..15 {
                tokio::time::sleep(Duration::from_millis(800)).await;
                match rpc.signature_status(&s).await {
                    Ok(Some(true)) => {
                        metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                        metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                        info!(target: "scout::executor", sig = %s, attempt,
                              "🎯 ONE-WAY ARB LANDED");
                        return;
                    }
                    Ok(Some(false)) => {
                        warn!(target: "scout::executor", sig = %s, attempt,
                              "💀 one-way reverted on chain");
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => warn!(target: "scout::executor",
                                    sig = %s, attempt, error = %e,
                                    "one-way status poll failed"),
                }
            }
            warn!(target: "scout::executor", sig = %s,
                  "💀 one-way never confirmed in 12s");
        });
        Ok(())
    }

    /// Unconditional Jupiter swap — used by the graduation-arb live path.
    /// Unlike `maybe_execute_one_way`, this skips the `min_profit_usdc` gate
    /// and the sample/throttle gates: graduation arb operates on SOL→TOKEN
    /// (mixed units) where the net comparison would compare lamports to
    /// token base units, which is nonsensical. The CALLER decides whether
    /// to fire (e.g. after detecting graduation, or after TP/SL trigger).
    ///
    /// Returns (signature, expected_out_amount).
    pub async fn execute_jupiter_swap_raw(
        &self,
        src_mint: Pubkey,
        dst_mint: Pubkey,
        amount_in: u64,
    ) -> ArbResult<(String, u64)> {
        if !self.cfg.live {
            return Err(ArbError::Invariant(
                "execute_jupiter_swap_raw: EXECUTOR_LIVE=false"
            ));
        }
        let quote = self.jupiter_swap.quote(
            &src_mint, &dst_mint, amount_in, self.cfg.slippage_bps,
        ).await.map_err(|e| {
            warn!(target: "scout::executor", src = %src_mint, dst = %dst_mint,
                  error = %e, "raw swap: Jupiter quote failed");
            e
        })?;
        let pubkey = self.wallet.pubkey();
        let ixs = self.jupiter_swap.swap_instructions(&quote, &pubkey, false).await
            .map_err(|e| {
                warn!(target: "scout::executor", error = %e,
                      "raw swap: swap_instructions failed");
                e
            })?;
        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(12);
        all.push(set_compute_unit_limit_ix(400_000));
        all.push(set_compute_unit_price_ix(10_000));
        // CASH is the only Token-2022 mint we touch in cross-stable / raw
        // Jupiter swaps; use the _2022 ATA helper for it (regular helper
        // would emit InvalidMint). Jupiter's setup_ixs may also include the
        // create-ATA — both are idempotent so prepending is safe.
        let ata_ix = |m: Pubkey| if m == known::CASH {
            create_ata_idempotent_ix_2022(pubkey, pubkey, m)
        } else {
            create_ata_idempotent_ix(pubkey, pubkey, m)
        };
        all.push(ata_ix(src_mint));
        all.push(ata_ix(dst_mint));
        all.extend(ixs.setup_ixs);
        all.push(ixs.swap_ix);
        if let Some(c) = ixs.cleanup_ix { all.push(c); }
        let helius_tip: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
            .ok().and_then(|s| s.parse().ok())
            .unwrap_or(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
        if !self.cfg.use_jito && helius_tip > 0 {
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::HELIUS_TIP_ACCOUNTS[n % known::HELIUS_TIP_ACCOUNTS.len()];
            all.push(system_transfer_ix(pubkey, tip_account, helius_tip));
        }
        let blockhash = fetch_latest_blockhash(&self.helius_url).await?;
        let alt_addrs = self.resolve_alt().await;
        let signed_b64 = if let Some(alt_pk) = self.cfg.alt_pubkey {
            build_and_sign_v0_tx_b64(
                &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                &[], self.wallet.bytes(),
            )?
        } else {
            build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())?
        };
        let helius_sender_url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty());
        let helius_api_key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty());
        // When tip=0, skip Helius Sender /fast endpoint (which would reject
        // with -32602) and use standard sendTransaction instead — free path,
        // no in-tx tip required, slightly slower routing.
        let sig = if !self.cfg.use_jito && helius_tip > 0 {
            let (url, key) = match (helius_sender_url, helius_api_key) {
                (Some(u), Some(k)) => (u, k),
                _ => return Err(ArbError::Config(
                    "raw swap: HELIUS_SENDER_URL/API_KEY required".into()
                )),
            };
            self.jito.send_via_helius_sender(&url, &key, &signed_b64).await?
        } else {
            self.helius_rpc.send_transaction(&signed_b64, false).await?
        };
        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::executor",
              sig = %sig, src = %src_mint, dst = %dst_mint,
              amount_in, expected_out = quote.out_amount,
              "🚀 raw Jupiter swap SENT");
        Ok((sig, quote.out_amount))
    }

    /// Solend liquidation execution. Fetches the full obligation, selects
    /// the (deposit, borrow) pair, builds the atomic Refresh→Liquidate
    /// sequence, signs, sends via standard sendTransaction. If on-chain
    /// refresh shows the position is no longer underwater, the liquidate IX
    /// reverts and we lose only the priority fee (~$0.001).
    ///
    /// Currently supports USDC debt only. Larger repayments than wallet
    /// balance fall back to flash loan (Solend USDC bank, 9bps fee) — TODO.
    pub async fn execute_solend_liquidation(
        &self,
        obligation: Pubkey,
        reserves: &crate::solend_liq::ReserveCache,
        rpc_url: &str,
        est_borrowed_usd: f64,
    ) -> ArbResult<()> {
        // Phase 6g: fetch obligation + blockhash in PARALLEL — was
        // sequential, costing ~250ms. tokio::join drops it to whichever
        // is slower (~150-200ms). Other RPC parallelization deferred to
        // a wider refactor (this one's surgical).
        let (obli_result, blockhash_result) = tokio::join!(
            crate::solend_liq::fetch_obligation_full(rpc_url, obligation),
            fetch_latest_blockhash(&self.helius_url),
        );
        let obli = obli_result?;
        let parallel_blockhash = blockhash_result?;
        let usdc = known::USDC;
        // Pick (deposit, borrow) where borrow_mint is the LARGEST of any
        // supported mint. Phase 6 (2026-05-13): no longer restricted to
        // USDC-debt only — MarginFi flash borrows USDC, and if the debt
        // mint differs we Jupiter-swap USDC → debt_mint intra-flash before
        // the liquidate ix.
        let (deposit_r, borrow_r) = match crate::solend_liq::select_liquidation_target(
            &obli, reserves, None,
        ) {
            Some(p) => p,
            None => {
                tracing::info!(target: "solend_liq",
                    obligation = %obligation,
                    "skip — no usable borrow/deposit pair");
                return Ok(());
            }
        };
        let debt_mint = borrow_r.liquidity_mint;
        let debt_decimals = borrow_r.liquidity_decimals;
        let debt_price_usd = borrow_r.market_price_usd.max(0.000001);

        // Phase 6: choose the flash-loan provider. MarginFi is the default
        // (0 bps fee on USDC bank), Solend is the fallback (9 bps fee).
        // Phase 6 ALSO removed the USDC-only debt restriction — we now
        // support arbitrary debt mints by swapping USDC → debt_mint
        // intra-flash via Jupiter.
        let flash_provider = std::env::var("FLASH_LOAN_PROVIDER")
            .unwrap_or_else(|_| "MarginFi".to_string());
        let use_marginfi = flash_provider.eq_ignore_ascii_case("MarginFi");

        // Biggest borrow value across all mints — used to bound repay size.
        let biggest_borrow_value = obli.borrows.iter()
            .filter(|b| reserves.get(&b.borrow_reserve)
                .map(|r| r.liquidity_mint == debt_mint).unwrap_or(false))
            .map(|b| b.market_value_usd)
            .fold(0.0_f64, f64::max);
        let max_repay_usd = (biggest_borrow_value * 0.20).max(0.10);

        // Poll wallet USDC balance (always needed — must cover flash-loan
        // fee even when capital comes from the flash borrow, AND the
        // Jupiter slippage cushion when debt != USDC).
        let payer = self.wallet.pubkey();
        let usdc_ata = crate::raw_tx::derive_ata(&payer, &usdc);
        let wallet_usdc = match self.helius_rpc.get_token_account_balance(&usdc_ata).await {
            Ok(amt) => amt as f64 / 1_000_000.0,
            Err(_) => 0.0,
        };

        // Phase 6b: per-mint MarginFi bank selection. Saves ~400 bytes per
        // tx by skipping the intra-flash USDC→debt_mint Jupiter swap when
        // we can borrow the debt_mint directly from MarginFi.
        let (mfi_bank_pk, mfi_vault_opt, mfi_pyth_opt) = if debt_mint == usdc {
            (
                arb_flashloan::MARGINFI_USDC_BANK,
                self.cfg.marginfi_liquidity_vault,
                self.cfg.usdc_pyth_oracle,
            )
        } else if debt_mint == known::USDT {
            (
                arb_flashloan::MARGINFI_USDT_BANK,
                self.cfg.marginfi_usdt_liquidity_vault,
                self.cfg.usdt_pyth_oracle,
            )
        } else if debt_mint == known::WBTC {
            (
                arb_flashloan::MARGINFI_WBTC_BANK,
                self.cfg.marginfi_wbtc_liquidity_vault,
                self.cfg.wbtc_pyth_oracle,
            )
        } else {
            // Other debt mints fall back to USDC bank + intra-flash swap.
            (
                arb_flashloan::MARGINFI_USDC_BANK,
                self.cfg.marginfi_liquidity_vault,
                self.cfg.usdc_pyth_oracle,
            )
        };
        let marginfi_ready = self.cfg.marginfi_account.is_some()
            && mfi_vault_opt.is_some()
            && mfi_pyth_opt.is_some();
        let flash_cap_usd = self.cfg.solend_liquidator_flash_loan_usd as f64;
        let flash_enabled = flash_cap_usd > 0.0
            && self.cfg.alt_pubkey.is_some()
            && (if use_marginfi { marginfi_ready } else { self.cfg.solend_usdc_ctx.is_some() });
        let cap_usd = if flash_enabled {
            // Phase 6i: buffer was over-requirement. MarginFi USDC bank is
            // 0bps fee; we only need a tiny cushion for priority fee + rent.
            // Previous logic multiplied flash_cap_usd (50K) by slippage_bps,
            // demanding $501 wallet USDC even when debt == USDC = no Jupiter
            // intra-flash needed. Now: cushion is based on the ACTUAL
            // expected slippage on the obligation's max_repay_usd, not the
            // theoretical flash_cap. $5 hard floor covers priority + rent.
            let provider_fee_bps: f64 = if use_marginfi { 0.0 } else { 9.0 };
            let jupiter_slippage_bps: f64 = if debt_mint == usdc { 0.0 } else { 100.0 };
            let fee_buffer_needed = (max_repay_usd * (provider_fee_bps + jupiter_slippage_bps) / 10_000.0) + 5.0;
            if wallet_usdc < fee_buffer_needed {
                tracing::info!(target: "solend_liq",
                    wallet_usdc, fee_buffer_needed,
                    "skip — wallet USDC below flash-fee + cushion buffer");
                return Ok(());
            }
            max_repay_usd.min(flash_cap_usd)
        } else {
            // No flash: caller funds repay directly. Only USDC debt is
            // supported here (no swap path without flash envelope).
            if debt_mint != usdc {
                tracing::info!(target: "solend_liq",
                    obligation = %obligation, debt_mint = %debt_mint,
                    "skip — non-USDC debt requires flash envelope");
                return Ok(());
            }
            // Phase 6m: leave only 3 atomic USDC base units (0.000003 USDC).
            // 5% reserve burned $5 per $100 wallet; with $440 we'd lose $22 per
            // liquidation in unused capital. The 3-unit floor keeps the ATA
            // technically non-empty so SPL bookkeeping stays clean, but
            // otherwise deploys the full wallet.
            let max_from_wallet = (((wallet_usdc * 1_000_000.0) as u64)
                .saturating_sub(3)) as f64 / 1_000_000.0;
            max_repay_usd.min(max_from_wallet)
        };
        if cap_usd < 0.10 {
            tracing::info!(target: "solend_liq",
                wallet_usdc, max_repay_usd,
                "skip — insufficient capacity for liquidation");
            return Ok(());
        }
        // Repay amount in DEBT TOKEN units. For USDC/USDT that's USD * 1e6.
        // For SOL it's USD / SOL_price * 1e9.
        let native_per_usd_debt = (10f64).powi(debt_decimals as i32) / debt_price_usd;
        let mut repay_amount: u64 = (cap_usd * native_per_usd_debt) as u64;
        // Phase 6m: belt-and-braces u64 floor for the no-flash path. The
        // f64 cap above already leaves 3 base units, but f64→u64 rounding
        // could in pathological cases land on wallet_base - 2. Force it.
        if !flash_enabled {
            let wallet_usdc_base = (wallet_usdc * 1_000_000.0) as u64;
            repay_amount = repay_amount.min(wallet_usdc_base.saturating_sub(3));
        }
        // Phase 6b: flash_borrow_amount is in units of the FLASH MINT
        // (= the bank's mint). For direct-bank debts: native units. For
        // other debts we flash USDC and swap intra-flash.
        let flash_borrow_mint = if debt_mint == known::USDT { known::USDT }
            else if debt_mint == known::WBTC { known::WBTC }
            else { usdc };
        let need_intra_flash_swap = debt_mint != flash_borrow_mint;
        let flash_borrow_amount: u64 = if !need_intra_flash_swap {
            repay_amount
        } else {
            // borrow ~1% extra USDC for Jupiter slippage cushion
            ((cap_usd * 1.01) * 1_000_000.0) as u64
        };
        // Legacy name kept for log message backward-compat.
        let flash_borrow_usdc = flash_borrow_amount;

        tracing::info!(target: "solend_liq",
            obligation = %obligation,
            deposit_reserve = %deposit_r.address,
            borrow_reserve = %borrow_r.address,
            debt_mint = %debt_mint,
            flash_provider = %flash_provider,
            est_borrowed_usd, biggest_borrow = biggest_borrow_value,
            wallet_usdc, repay_amount_usd = cap_usd,
            flash_borrow_usdc,
            "🔫 attempting liquidation");

        // Build the full IX list.
        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(20);
        // Flash-loan path needs ~1.4M CU (extra refresh + borrow + repay).
        // Add 300k more when an intra-flash Jupiter swap is needed.
        let cu_limit: u32 = if flash_enabled {
            if debt_mint == usdc { 1_400_000 } else { 1_400_000 }
        } else {
            600_000
        };
        all.push(set_compute_unit_limit_ix(cu_limit));
        // Phase 6d: dynamic priority via getRecentPrioritizationFees. Query
        // the actual fees paid on the same write-locked accounts our tx
        // touches (the deposit/borrow reserve + reserve liquidity supply)
        // and pay p75 — competitive without overpaying. Total tx size with
        // this IX restored: USDC-debt 2-reserve ~877 bytes (well under
        // 1232). Floor at 1000 microlamports to avoid 0-priority races.
        let cu_price_accounts = [borrow_r.address, borrow_r.liquidity_supply];
        let cu_price_dynamic = self.helius_rpc.get_priority_fee_p75(&cu_price_accounts).await;
        let env_floor: u64 = std::env::var("SOLEND_PRIORITY_MICRO_LAMPORTS")
            .ok().and_then(|s| s.parse().ok()).unwrap_or(1_000);
        let cu_price = cu_price_dynamic.max(env_floor);
        if cu_price > 0 {
            all.push(set_compute_unit_price_ix(cu_price));
        }
        tracing::debug!(target: "solend_liq",
            cu_price_p75 = cu_price_dynamic, cu_price_used = cu_price,
            "priority fee set");

        // Phase 6: build the flash-loan envelope. Two providers supported.
        // Layout deferred: MarginFi requires `end_index` (the absolute
        // index of `end_flashloan` in the final tx) which we don't know
        // yet — caller patches the start_flashloan ix at the end after
        // counting all ixs.
        let mut flash_envelope: Option<arb_flashloan::FlashLoanEnvelope> = None;
        let mut marginfi_start_idx: Option<usize> = None;
        let mut marginfi_post_ixs: Vec<solana_program::instruction::Instruction> = Vec::new();
        if flash_enabled {
            if use_marginfi {
                // Safe to unwrap — flash_enabled+use_marginfi implies marginfi_ready.
                let mfi_account = self.cfg.marginfi_account.unwrap();
                let mfi_vault = mfi_vault_opt.unwrap();
                let mfi_pyth = mfi_pyth_opt.unwrap();
                let flash_mint_ata = crate::raw_tx::derive_ata(&payer, &flash_borrow_mint);
                let mfi_ctx = arb_flashloan::MarginFiFlashContext {
                    group:                     arb_flashloan::MARGINFI_MAIN_GROUP,
                    bank:                      mfi_bank_pk,
                    liquidity_vault:           mfi_vault,
                    liquidity_vault_authority: arb_flashloan::derive_liquidity_vault_authority(
                        &mfi_bank_pk,
                    ),
                    usdc_pyth_oracle:          mfi_pyth,  // field name legacy; holds whichever pyth matches flash mint
                    marginfi_account:          mfi_account,
                };
                // Patch end_index after we know the full ix count. For now,
                // pass 0; we overwrite the bytes in-place before signing.
                let (pre, post) = arb_flashloan::build_marginfi_flash_loan(
                    &mfi_ctx, payer, flash_mint_ata, flash_borrow_amount, 0,
                );
                marginfi_start_idx = Some(all.len());
                for ix in pre { all.push(ix); }
                marginfi_post_ixs = post;
            } else {
                // Legacy Solend path. Safe to unwrap — flash_enabled implies solend_usdc_ctx.is_some().
                let ctx = self.cfg.solend_usdc_ctx.as_ref().unwrap();
                let envelope = build_usdc_flash_loan_envelope(
                    ctx, &self.cfg.solend_reserve_state, payer, usdc_ata,
                    flash_borrow_usdc,
                    // borrow lives at index 3: [cu_limit=0, cu_price=1, refresh=2, borrow=3]
                    3,
                );
                all.push(envelope.refresh_ix.clone());
                all.push(envelope.borrow_ix.clone());
                flash_envelope = Some(envelope);
            }
        }

        // Phase 6: intra-flash Jupiter swap flash_mint → debt_mint. Only
        // needed when we couldn't borrow the debt mint directly (e.g.,
        // debt=SOL while MarginFi only has USDC/USDT banks). USDT debt
        // now flashes USDT directly, skipping this swap.
        let mut intra_jupiter_alts: Vec<Pubkey> = Vec::new();
        if flash_enabled && need_intra_flash_swap {
            let debt_ata = crate::raw_tx::derive_ata(&payer, &debt_mint);
            all.push(create_ata_idempotent_ix(payer, payer, debt_mint));
            match self.jupiter_swap.quote(
                &flash_borrow_mint, &debt_mint, flash_borrow_amount, 100,
            ).await {
                Ok(quote) => match self.jupiter_swap.swap_instructions(&quote, &payer, false).await {
                    Ok(jup_ixs) => {
                        tracing::info!(target: "solend_liq",
                            usdc_in = flash_borrow_usdc,
                            debt_out_estimate = quote.out_amount,
                            debt_mint = %debt_mint,
                            "💱 intra-flash Jupiter swap (USDC → debt_mint)");
                        let _ = debt_ata; // ATA already derived for clarity
                        all.extend(jup_ixs.setup_ixs);
                        all.push(jup_ixs.swap_ix);
                        if let Some(c) = jup_ixs.cleanup_ix { all.push(c); }
                        intra_jupiter_alts = jup_ixs.address_lookup_tables;
                    }
                    Err(e) => {
                        tracing::warn!(target: "solend_liq",
                            error = %e, debt_mint = %debt_mint,
                            "intra-flash Jupiter swap_instructions failed — skipping");
                        return Ok(());
                    }
                },
                Err(e) => {
                    tracing::warn!(target: "solend_liq",
                        error = %e, debt_mint = %debt_mint,
                        "intra-flash Jupiter quote failed — skipping");
                    return Ok(());
                }
            }
        }

        // Phase 6c: the c-token ATA create was a safety guard for first-
        // ever liquidation of a new collateral type. After ~5 production
        // liquidations the ATA is cached on-chain and the create_ata is
        // pure overhead (~30 bytes). Skipping it shaves the USDC-debt tx
        // from 1233 → ~1203 bytes (well under the 1232 v0 limit). The cost:
        // a one-time tx failure if we hit a brand-new collateral, which
        // we recover from on the next iteration after the ATA materialises.
        // Gate via env: set SOLEND_CREATE_CTOKEN_ATA=true to re-enable.
        if std::env::var("SOLEND_CREATE_CTOKEN_ATA").as_deref() == Ok("true") {
            all.push(create_ata_idempotent_ix(payer, payer, deposit_r.collateral_mint));
        }
        // Phase 6r: atomic Pyth-paired path. ONLY in the non-flash path
        // because the Pyth IX (~628 bytes incl. accounts) + MarginFi flash
        // wrapper combined overflow the 1232-byte v0 budget. Without flash
        // we have ~700 bytes of headroom — fits one prepended Pyth IX.
        // Set NATIVE_ATOMIC_PYTH=true to enable (default ON only when
        // flash_enabled == false).
        let atomic_pyth_enabled = !flash_enabled
            && std::env::var("NATIVE_ATOMIC_PYTH").as_deref() != Ok("false");
        if atomic_pyth_enabled {
            // Prepend at most ONE Pyth IX (per oracle) — picking either
            // deposit_r or borrow_r. Both is too large.
            for oracle in [deposit_r.pyth_oracle, borrow_r.pyth_oracle] {
                if let Some(pyth_ix) = crate::pyth_shred_dump::build_atomic_pyth_ix(&oracle, &payer) {
                    tracing::info!(target: "solend_liq",
                        oracle = %oracle, ix_data_len = pyth_ix.data.len(),
                        accs = pyth_ix.accounts.len(),
                        "💎 prepending atomic Pyth IX clone from shred dump");
                    all.push(pyth_ix);
                    break;  // only one, to stay under 1232-byte budget
                }
            }
        }

        // Refresh + Liquidate sequence — pass FULL obligation so we can
        // refresh every reserve referenced by the position (Solend requires
        // exact ordering).
        let liq_seq = crate::solend_liq::build_liquidation_sequence_full(
            obligation, &obli, reserves, &deposit_r, &borrow_r, payer, repay_amount,
        );
        all.extend(liq_seq);

        // Phase 1: Atomic Jupiter swap of seized collateral → USDC.
        // After Solend liquidate + redeem we hold the underlying collateral
        // (SOL, jitoSOL, mSOL, etc.) in our wallet. Adding a Jupiter swap in
        // the SAME tx converts it to USDC atomically — locking in profit and
        // eliminating the manual-sell step.
        //
        // The seized amount is estimated from the liquidation_bonus on the
        // reserve config. Jupiter quote uses ExactIn with this estimate; if
        // actual seizure is slightly more, the leftover stays in wallet
        // (we can swap later). If less, the swap reverts (atomic) — costs
        // just the priority fee, no capital loss.
        // Phase 1 atomic swap only fits in v0+ALT when the obligation has a
        // simple structure (≤2 unique reserves total). Complex obligations
        // would push the tx over Solana's 1644-byte v0 limit. For those, we
        // fall through and seize collateral to wallet (manual sell later).
        let n_reserves = obli.deposits.len() + obli.borrows.len();
        // Multi-ALT support now in v0_tx — Jupiter's 20 accounts get
        // resolved via Jupiter's own ALT, ours via our liquidator ALT.
        // Default ON since the size constraint is now resolved.
        let auto_swap = std::env::var("SOLEND_LIQUIDATOR_AUTO_SWAP_COLLATERAL")
            .as_deref() != Ok("false"); // default ON
        let mut jupiter_alts: Vec<Pubkey> = Vec::new();
        // Phase 6: target the *flash mint* (debt_mint), not USDC. The
        // collateral swap must produce the same currency the flash will
        // repay in — otherwise the post-liquidate USDC ATA stays empty and
        // the MarginFi deposit fails with InsufficientFunds.
        let post_swap_target = debt_mint;
        if auto_swap && deposit_r.liquidity_mint != post_swap_target
            && n_reserves <= 2 {
            // Estimated USD value of seized collateral:
            //   = repay_amount_usd × (1 + bonus_pct/100)
            // Convert to native units of the collateral token:
            //   native_amount = (usd_value / price_per_unit_usd) × 10^decimals
            let bonus_pct = deposit_r.liquidation_bonus as f64;
            let seized_usd = cap_usd * (1.0 + bonus_pct / 100.0);
            let price = deposit_r.market_price_usd.max(0.000001);
            let native_per_usd = (10f64).powi(deposit_r.liquidity_decimals as i32) / price;
            // 90% to be safe — Solend's exact seized amount depends on
            // internal math; we'd rather Jupiter swap LESS than fail.
            let seized_native = (seized_usd * native_per_usd * 0.90) as u64;

            if seized_native > 1000 {
                match self.jupiter_swap.quote(
                    &deposit_r.liquidity_mint, &post_swap_target, seized_native,
                    100, // 100 bps slippage — generous, post-liq path
                ).await {
                    Ok(jup_quote) => {
                        match self.jupiter_swap.swap_instructions(&jup_quote, &payer, false).await {
                            Ok(jup_ixs) => {
                                tracing::info!(target: "solend_liq",
                                    seized_estimate = seized_native,
                                    out_estimate = jup_quote.out_amount,
                                    target_mint = %post_swap_target,
                                    "💱 appending Jupiter swap (collateral → flash mint) to liquidation tx");
                                all.extend(jup_ixs.setup_ixs);
                                all.push(jup_ixs.swap_ix);
                                if let Some(c) = jup_ixs.cleanup_ix { all.push(c); }
                                jupiter_alts = jup_ixs.address_lookup_tables;
                            }
                            Err(e) => tracing::warn!(target: "solend_liq",
                                error = %e,
                                "Jupiter swap_instructions failed — falling back to hold-collateral path"),
                        }
                    }
                    Err(e) => tracing::warn!(target: "solend_liq",
                        error = %e, seized_native,
                        "Jupiter quote failed — falling back to hold-collateral path"),
                }
            }
        }

        // Phase 6: append the flash-loan close. Two providers handled:
        //   • MarginFi  → deposit (repay) + end_flashloan
        //   • Solend    → flash_repay (1 ix)
        // For MarginFi we ALSO patch the start_flashloan ix's `end_index`
        // bytes in-place now that we know the final ix count.
        if let Some(envelope) = flash_envelope.as_ref() {
            all.push(envelope.repay_ix.clone());
            tracing::info!(target: "solend_liq",
                flash_borrow_usd = cap_usd,
                flash_fee_usdc_base = envelope.fee_lamports,
                "💵 wrapped liquidation in Solend USDC flash loan envelope");
        } else if let Some(start_idx) = marginfi_start_idx {
            // marginfi_post_ixs = [deposit, end_flashloan]
            all.extend(marginfi_post_ixs.drain(..));
            // Patch start_flashloan's end_index to point at end_flashloan.
            // Layout in start_flashloan data: [8-byte disc][u64 end_index LE].
            let end_index = (all.len() - 1) as u64;
            let start_ix = &mut all[start_idx];
            if start_ix.data.len() >= 16 {
                start_ix.data[8..16].copy_from_slice(&end_index.to_le_bytes());
            }
            tracing::info!(target: "solend_liq",
                flash_borrow_usd = cap_usd, flash_borrow_usdc, end_index,
                debt_mint = %debt_mint,
                "💵 wrapped liquidation in MarginFi flash loan envelope");
        }

        // Phase 5b: optional Jito bundle submission. Append a Jito tip IX so
        // we can submit the whole tx as a bundle through Jito's Block Engine.
        // Bundles get priority routing through the leader's mempool —
        // bundles' txs are committed sequentially atop the current slot
        // before non-bundle txs, giving us a latency edge against
        // sendTransaction submissions.
        let use_jito_bundle = std::env::var("SOLEND_USE_JITO_BUNDLE")
            .as_deref() == Ok("true");
        if use_jito_bundle {
            // Phase 6f: AGGRESSIVE dynamic tip — pay 30% of expected bonus.
            // Previous 1% was getting outbid in the auction. Reasoning:
            // even paying 50% of a $15 bonus = $7.50 still nets +$7.50 vs
            // the alternative of $0 (the bundle never lands). Floor via
            // env, ceiling capped at 50M lamports (~$5.00). The
            // SOLEND_JITO_TIP_MULTIPLIER env can override the 0.30
            // multiplier without a rebuild.
            let bonus_pct = deposit_r.liquidation_bonus as f64;
            let expected_bonus_usd = cap_usd * (bonus_pct / 100.0);
            let sol_price_usd: f64 = 95.0; // approx; refined later when sol_price feed wired in
            let tip_multiplier: f64 = std::env::var("SOLEND_JITO_TIP_MULTIPLIER")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(0.30);
            let target_tip_usd = expected_bonus_usd * tip_multiplier;
            let target_tip_lamports = (target_tip_usd / sol_price_usd * 1e9) as u64;
            let env_floor: u64 = std::env::var("SOLEND_JITO_TIP_LAMPORTS")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(500_000);
            let jito_tip: u64 = target_tip_lamports.max(env_floor).min(50_000_000);
            let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
            let tip_account = known::JITO_TIP_ACCOUNTS[n % known::JITO_TIP_ACCOUNTS.len()];
            all.push(system_transfer_ix(payer, tip_account, jito_tip));
            tracing::info!(target: "solend_liq",
                jito_tip_lamports = jito_tip,
                target_tip_lamports, env_floor,
                expected_bonus_usd,
                tip_account = %tip_account,
                "🎁 dynamic Jito tip = 1% of expected bonus");
        }

        // Sign with v0 + multi-ALT. If Jupiter swap was appended, pass
        // Jupiter's ALT(s) too — each gets its own AddressTableLookup entry,
        // letting both our liquidator ALT and Jupiter's swap ALTs compress.
        // Phase 6g: reuse the blockhash fetched in parallel at the top of
        // this function (saved ~50ms vs fetching here serially).
        let blockhash = parallel_blockhash;
        let signed_b64 = if let Some(alt_pk) = self.cfg.alt_pubkey {
            let our_alt_addrs = self.resolve_alt().await.to_vec();
            let mut alt_list: Vec<(Pubkey, Vec<Pubkey>)> =
                Vec::with_capacity(2 + jupiter_alts.len() + intra_jupiter_alts.len());
            alt_list.push((alt_pk, our_alt_addrs));

            // Phase 6s: Pyth-dedicated ALT (set via EXECUTOR_PYTH_ALT env).
            // Holds Pyth Push V2 + legacy Pyth program IDs + all 85+ Solend
            // reserve price oracles + the reserves themselves. Compresses the
            // ~628-byte Pyth IX (data + 7 accounts) down so it fits alongside
            // Solend's Refresh+Liquidate inside the 1232-byte v0 budget.
            if let Ok(pyth_alt_str) = std::env::var("EXECUTOR_PYTH_ALT") {
                if let Ok(pyth_alt_pk) = pyth_alt_str.parse::<Pubkey>() {
                    match self.helius_rpc.fetch_alt_addresses(&pyth_alt_pk).await {
                        Ok(addrs) => {
                            tracing::info!(target: "solend_liq",
                                alt = %pyth_alt_pk, addrs = addrs.len(),
                                "🔗 attached Pyth ALT");
                            alt_list.push((pyth_alt_pk, addrs));
                        }
                        Err(e) => tracing::warn!(target: "solend_liq",
                            alt = %pyth_alt_pk, error = %e,
                            "could not fetch Pyth ALT — skipping"),
                    }
                }
            }
            // Fetch Jupiter's ALT contents for BOTH intra-flash swap and
            // collateral swap. Deduplicate — same ALT often reused.
            let mut seen: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
            for j_alt in intra_jupiter_alts.iter().chain(jupiter_alts.iter()) {
                if !seen.insert(*j_alt) { continue; }
                match self.helius_rpc.fetch_alt_addresses(j_alt).await {
                    Ok(addrs) => alt_list.push((*j_alt, addrs)),
                    Err(e) => tracing::warn!(target: "solend_liq",
                        alt = %j_alt, error = %e,
                        "could not fetch Jupiter ALT — falling back to static for those accounts"),
                }
            }
            // Jito bundles require the tip account to be write-locked in
            // the STATIC key set (not lookup-table compressed). Force every
            // Jito tip account into the static list so whichever one we
            // selected for this attempt stays visible to Jito's auction
            // verifier. Without this we get -32602 "must write lock tip
            // account".
            let jito_force_static: Vec<Pubkey> = if use_jito_bundle {
                known::JITO_TIP_ACCOUNTS.to_vec()
            } else { Vec::new() };
            crate::v0_tx::build_and_sign_v0_tx_b64_multi(
                &payer, &all, &blockhash, &alt_list, &jito_force_static, self.wallet.bytes(),
            )?
        } else {
            build_and_sign_legacy_tx_b64(&payer, &all, &blockhash, self.wallet.bytes())?
        };

        // SIMULATE FIRST: most candidates are stale-zombies that will revert
        // with "Cannot liquidate healthy obligations" (0x1d) or with empty
        // liquidity after refresh (0x2a). Simulation tells us WITHOUT paying
        // priority fee. Only fire real send when sim says "no error".
        //
        // SKIP_SIM mode (env: SOLEND_LIQUIDATOR_SKIP_SIM=true): bypass the
        // sim gate and send directly. Useful when the sim-time → tx-time
        // delta is killing us (competitors land in that gap; by sim-time
        // the obligation looks healthy but it might still be underwater at
        // a slot or two later). Costs ~$0.0013/attempt (Jito tip + priority
        // fee) on every revert — pair with fresh-only so we only send for
        // obligations that JUST flipped, not the 13 permanent zombies.
        let skip_sim = std::env::var("SOLEND_LIQUIDATOR_SKIP_SIM")
            .as_deref() == Ok("true");
        if !skip_sim {
            let sim = self.helius_rpc.simulate_transaction(&signed_b64).await?;
            if !sim.err_is_null {
                let blob = sim.logs.join("\n");
                let silent = blob.contains("Cannot liquidate healthy")
                    || blob.contains("0x1d")
                    || blob.contains("0x2a"); // empty borrow / repay-too-small after refresh
                if silent {
                    tracing::debug!(target: "solend_liq",
                        obligation = %obligation,
                        "stale-zombie / post-refresh-empty filtered out");
                } else {
                    // Surface the tail of logs so we can diagnose unexpected reverts.
                    let tail = sim.logs.iter().rev().take(5).rev()
                        .cloned().collect::<Vec<_>>().join(" | ");
                    tracing::info!(target: "solend_liq",
                        obligation = %obligation,
                        tail = %tail,
                        "simulation failed (other reason) — skipping");
                }
                return Ok(());
            }
            // Simulation passed — this obligation IS truly underwater right now.
            tracing::info!(target: "solend_liq",
                obligation = %obligation, repay_amount_usd = cap_usd,
                "✨ SIMULATION PASSED — sending real liquidation");
        } else {
            tracing::info!(target: "solend_liq",
                obligation = %obligation, repay_amount_usd = cap_usd,
                "⚡ SKIP-SIM mode — sending without simulation pre-check");
        }
        // Phase 6 diag: dump tx size so we can spot 1232-byte overflows
        // before Jito rejects with "transaction #0 could not be decoded".
        // Base64 expansion is 4/3 — divide bytes-out by that to get raw.
        let tx_size_bytes = signed_b64.len() * 3 / 4;
        tracing::info!(target: "solend_liq",
            obligation = %obligation, tx_size_bytes, n_ixs = all.len(),
            "📦 tx built (size in bytes)");
        // Phase 6h: SEND VIA BOTH PATHS IN PARALLEL.
        // Jito (rate-limited 1/sec by their API) — best when accepted,
        // gives priority via auction.
        // Helius RPC sendTransaction (no rate limit) — always tries,
        // gives our tx a chance even when Jito rejects with -32097/-32602.
        // The Solana network deduplicates by tx_sig — whichever lane wins
        // gets the slot; the other gets "AlreadyProcessed" and we don't pay
        // twice.
        let signed_for_helius = signed_b64.clone();
        let signed_for_jito = signed_b64.clone();
        let helius_rpc = self.helius_rpc.clone();
        let helius_send_handle = tokio::spawn(async move {
            helius_rpc.send_transaction(&signed_for_helius, true).await
        });
        let sig = if use_jito_bundle {
            // Try Jito, but DON'T block on its failure — Helius lane runs anyway.
            match self.jito.send_bundle(&[signed_for_jito]).await {
                Ok(bundle_id) => {
                    self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
                    tracing::info!(target: "solend_liq",
                        bundle_id = %bundle_id, obligation = %obligation,
                        repay_amount_usd = cap_usd,
                        "🎯 SOLEND LIQUIDATION sent via Jito bundle");
                }
                Err(e) => {
                    tracing::debug!(target: "solend_liq",
                        obligation = %obligation, error = %e,
                        "Jito send failed (rate limit / size) — Helius lane will still try");
                }
            }
            // Sig from Helius (always runs)
            match helius_send_handle.await {
                Ok(Ok(s)) => s,
                Ok(Err(e)) => {
                    tracing::warn!(target: "solend_liq",
                        obligation = %obligation, error = %e,
                        "Helius sendTransaction also failed");
                    return Ok(());
                }
                Err(e) => {
                    tracing::warn!(target: "solend_liq",
                        obligation = %obligation, error = %e,
                        "Helius task panicked");
                    return Ok(());
                }
            }
        } else {
            helius_send_handle.await.map_err(|e| ArbError::Rpc(format!("helius task: {e}")))??
        };
        tracing::info!(target: "solend_liq",
            sig = %sig, obligation = %obligation, repay_amount_usd = cap_usd,
            "🚀 SOLEND LIQUIDATION SENT (after sim pass)");

        // Poll for landing.
        let rpc = self.helius_rpc.clone();
        let s = sig.clone();
        let obl = obligation;
        tokio::spawn(async move {
            for attempt in 0..15 {
                tokio::time::sleep(Duration::from_millis(800)).await;
                match rpc.signature_status(&s).await {
                    Ok(Some(true)) => {
                        tracing::info!(target: "solend_liq",
                            sig = %s, obligation = %obl, attempt,
                            "🎯 SOLEND LIQUIDATION LANDED");
                        return;
                    }
                    Ok(Some(false)) => {
                        tracing::info!(target: "solend_liq",
                            sig = %s, obligation = %obl, attempt,
                            "💀 liquidation reverted (likely stale candidate after refresh)");
                        return;
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }

    /// Try to execute an arb cycle base → `intermediate` → base at the
    /// given input size. `base_mint` is USDC by default; scout rotates to
    /// USDT every other opportunity when `USDT_ALTERNATION_ENABLED=true`.
    /// Walks every safety gate and either sends, dry-runs, or skips.
    pub async fn maybe_execute_with_base(
        &self, base_mint: Pubkey, intermediate: Pubkey, probe_usdc: u64,
    ) -> ArbResult<()> {
        self.maybe_execute_inner(base_mint, intermediate, probe_usdc).await
    }

    /// Back-compat shim — USDC-only entry point. Callers that don't care
    /// about USDT keep working unchanged.
    pub async fn maybe_execute(&self, intermediate: Pubkey, probe_usdc: u64) -> ArbResult<()> {
        self.maybe_execute_inner(known::USDC, intermediate, probe_usdc).await
    }

    async fn maybe_execute_inner(
        &self, base_mint: Pubkey, intermediate: Pubkey, probe_usdc: u64,
    ) -> ArbResult<()> {
        // CAP the trade size at the executor's own probe config. For USDC/
        // USDT we use `probe_amount_usdc`. For SOL base there's no flash
        // loan available (MarginFi has no SOL vault), so the trade is
        // funded from wallet's WSOL — cap it tightly via
        // `SCOUT_PROBE_AMOUNT_SOL_LAMPORTS` (default 20M = 0.02 SOL ≈ $2).
        // The `probe_usdc` arg name is legacy; for SOL base it carries
        // lamports.
        let probe_usdc = if base_mint == known::SOL {
            let sol_cap = std::env::var("SCOUT_PROBE_AMOUNT_SOL_LAMPORTS")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(20_000_000u64);
            probe_usdc.min(sol_cap)
        } else {
            probe_usdc.min(self.cfg.probe_amount_usdc)
        };

        let n = self.metrics.considered.fetch_add(1, Ordering::Relaxed);

        // Gate -1: rate-limit sampling (1-in-N) to stay under Jupiter's free tier.
        if n % self.cfg.sample_one_in_n != 0 {
            self.metrics.skipped_sample.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }

        // Gate -0.7: Jito back-off respect. If Jito previously responded
        // with a `Retry after Xms` rate-limit error, we MUST wait that long
        // before any further send — otherwise the back-off escalates
        // exponentially (1s → 6s → 30s → 120s) and we get banned for
        // 2 minutes. Drop opportunities while the back-off is active.
        {
            let back_off = self.back_off_until.lock().await;
            if let Some(until) = *back_off {
                if Instant::now() < until {
                    self.metrics.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
        }

        // Gate -0.5: min-interval throttle. Both Jupiter (4 req/cycle) and
        // Jito (1 simulateBundle/sec hard cap) get hammered without this.
        // We DROP rather than queue — opportunities go stale fast.
        {
            let mut last = self.last_attempt.lock().await;
            let now = Instant::now();
            if let Some(t) = *last {
                if now.duration_since(t) < self.cfg.min_interval {
                    self.metrics.skipped_throttle.fetch_add(1, Ordering::Relaxed);
                    return Ok(());
                }
            }
            *last = Some(now);
        }

        // Gate 0: daily loss cap.
        if self.metrics.running_net_usdc.load(Ordering::Relaxed)
              < -(self.cfg.max_daily_loss_usdc as i64) {
            self.metrics.skipped_daily_cap.fetch_add(1, Ordering::Relaxed);
            warn!(target: "scout::executor",
                  net_usdc = self.metrics.running_net_usdc.load(Ordering::Relaxed),
                  cap = self.cfg.max_daily_loss_usdc,
                  "daily loss cap reached — skipping");
            return Ok(());
        }

        // Gate 1: get Jupiter quotes for both legs to verify profitability.
        // Catch errors (rate limit, no-route, etc.) and surface them at WARN
        // so the operator can see why dry-runs aren't happening.
        let usdc = base_mint;  // back-compat name — `base` is the round-trip currency
        let leg1 = match self.jupiter_swap.quote(&usdc, &intermediate, probe_usdc, self.cfg.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                self.metrics.jupiter_errors.fetch_add(1, Ordering::Relaxed);
                warn!(target: "scout::executor", leg = "1", error = %e,
                      base = %base_mint,
                      "Jupiter quote failed (likely rate-limited or no route)");
                return Ok(());
            }
        };
        let leg2 = match self.jupiter_swap.quote(&intermediate, &usdc, leg1.out_amount, self.cfg.slippage_bps).await {
            Ok(q) => q,
            Err(e) => {
                self.metrics.jupiter_errors.fetch_add(1, Ordering::Relaxed);
                warn!(target: "scout::executor", leg = "2", error = %e,
                      "Jupiter quote failed (likely rate-limited or no route)");
                return Ok(());
            }
        };
        let net = (leg2.out_amount as i64) - (probe_usdc as i64);
        // v65: optionally bypass the Jupiter-side profit gate. When scout's
        // own quoter flagged the route as profitable, but Jupiter's
        // aggregator (with its different pool universe and fee model)
        // disagrees, the two-stage pipeline rejects ~100% of submissions.
        // `EXECUTOR_SKIP_PROFIT_GATE=true` collapses to one stage: trust
        // scout, send blind. The on-chain min_amount_out floor still
        // protects against price moves — reverts cost only tip+fee.
        let skip_gate = std::env::var("EXECUTOR_SKIP_PROFIT_GATE")
            .ok()
            .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
            .unwrap_or(false);
        if !skip_gate && net < self.cfg.min_profit_usdc as i64 {
            self.metrics.skipped_below_min.fetch_add(1, Ordering::Relaxed);
            // Sampled log — 1 in 50 to avoid spam.
            let n = self.metrics.skipped_below_min.load(Ordering::Relaxed);
            if n % 50 == 0 {
                info!(target: "scout::executor",
                      base = %base_mint, intermediate = %intermediate,
                      probe = probe_usdc,
                      leg1_out = leg1.out_amount, leg2_out = leg2.out_amount,
                      net,
                      "🔬 Jupiter says no arb here — sampled every 50th");
            }
            return Ok(());
        }
        if skip_gate && net < self.cfg.min_profit_usdc as i64 {
            // Send-anyway path: log so we can measure how many "negative
            // per Jupiter" routes actually land profitable in-block.
            let n = self.metrics.considered.load(Ordering::Relaxed);
            if n % 50 == 0 {
                info!(target: "scout::executor",
                      base = %base_mint, intermediate = %intermediate,
                      probe = probe_usdc, net,
                      "⚡ SKIP-GATE: Jupiter says net-negative, sending anyway (every 50th)");
            }
        }

        info!(target: "scout::executor",
              intermediate = %intermediate,
              probe_usdc, leg1_out = leg1.out_amount, leg2_out = leg2.out_amount,
              net_usdc = net,
              "📈 Jupiter cleared min-profit — fetching swap txs");

        // Phase 6n diag: dump Jupiter's chosen pools so the operator can see
        // which AMM keys appear and decide if they need ALT inclusion.
        let extract_pools = |r: &serde_json::Value| -> Vec<String> {
            r.get("routePlan")
                .and_then(|p| p.as_array())
                .map(|arr| arr.iter().filter_map(|step| {
                    let info = step.get("swapInfo")?;
                    let label = info.get("label").and_then(|v| v.as_str()).unwrap_or("?");
                    let amm = info.get("ammKey").and_then(|v| v.as_str()).unwrap_or("?");
                    Some(format!("{label}:{amm}"))
                }).collect())
                .unwrap_or_default()
        };
        info!(target: "scout::executor",
              intermediate = %intermediate, net_usdc = net,
              leg1_route = ?extract_pools(&leg1.raw),
              leg2_route = ?extract_pools(&leg2.raw),
              "🔎 Jupiter route pools");

        let pubkey = self.wallet.pubkey();
        let _guard = self.in_flight.lock().await;  // serialize from here on

        // Live mode check (common to both paths).
        if !self.cfg.live {
            self.metrics.skipped_dry_run.fetch_add(1, Ordering::Relaxed);
            info!(target: "scout::executor",
                  net_usdc = net,
                  "🟡 DRY-RUN — would have sent. Set EXECUTOR_LIVE=true.");
            return Ok(());
        }

        // ─── PATH C: MarginFi flash-loan-wrapped Jupiter atomic ────────────
        //
        // Pre-conditions:
        //   * `flash_loan_enabled = true`
        //   * `marginfi_account` and `marginfi_liquidity_vault` resolved
        //   * `alt_pubkey` set (without ALT compression the v0 tx overflows)
        //
        // Flow:
        //   1. Pull Jupiter `/swap-instructions` for both legs (raw ixs).
        //   2. Wrap with MarginFi `start_flashloan` + `borrow` (pre)
        //      and `deposit` + `end_flashloan` (post).
        //   3. Pack into a single v0 tx with the operator's ALT for
        //      compression. Submit as a 1-tx Jito bundle.
        //
        // Profit math is identical to PATH A/B; what changes is the probe
        // size — flash loan lets the executor probe at $5K from a $200
        // wallet without touching wallet capital.
        // Flash-loan path requires marginfi_account; for USDT, the
        // liquidity_vault must also be configured.
        // SOL base has no MarginFi vault; always falls through to legacy
        // Jito send path (funded from the wallet's WSOL ATA, capped above).
        let flash_ready = base_mint != known::SOL
            && self.cfg.flash_loan_enabled
            && self.cfg.marginfi_account.is_some()
            && if base_mint == known::USDT {
                self.cfg.marginfi_usdt_liquidity_vault.is_some()
            } else {
                self.cfg.marginfi_liquidity_vault.is_some()
            };
        if flash_ready {
            return self.execute_flash_jupiter(
                base_mint, intermediate, probe_usdc, &leg1, &leg2, net, &pubkey,
            ).await;
        }

        // ─── Two legacy send paths ────────────────────────────────────────
        if self.cfg.use_jito {
            // PATH A: classic Jito bundle of two pre-built txs (atomic via Jito).
            //
            // Dynamic tip = `EXECUTOR_TIP_PROFIT_PCT`% of net profit (default
            // 25%), floored at 1000 lamports (Jito's effective minimum),
            // capped at `cfg.jito_tip_lamports` (the static ceiling). Tiny
            // opportunities get tiny tips — they only land on uncontested
            // slots, but the alternative is paying more in tip than we earn.
            //
            // SOL price comes from `EXECUTOR_SOL_PRICE_USD` (default 93) so
            // the operator can retune without a redeploy. Tip lives in the
            // $0.0001–$0.005 band at $50 trade sizes, so even a 10%-off SOL
            // price barely moves the dollar tip.
            let tip_pct: u128 = std::env::var("EXECUTOR_TIP_PROFIT_PCT")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(70);
            let sol_price_usd: u128 = std::env::var("EXECUTOR_SOL_PRICE_USD")
                .ok().and_then(|s| s.parse().ok()).unwrap_or(93);
            let net_pos = net.max(0) as u128;
            // tip_usd = net_base_units / 1e6 × tip_pct/100
            // tip_lamports = tip_usd / sol_price × 1e9
            // Combined (avoid div-by-zero on absurd config):
            // tip_lamports = net_base × tip_pct × 10 / sol_price.max(1)
            let dynamic_tip = (net_pos
                .saturating_mul(tip_pct)
                .saturating_mul(10)
                / sol_price_usd.max(1)) as u64;
            let tip_lamports = dynamic_tip
                .max(1_000)                              // Jito floor
                .min(self.cfg.jito_tip_lamports);        // operator ceiling
            info!(target: "scout::executor",
                  net_usdc = net, tip_lamports, tip_pct = tip_pct as u64,
                  "dynamic tip computed");

            // Pass tip to leg2 ONLY — Jito uses bundle-level tip; tipping in
            // both legs would double-pay (~halving net on every bundle).
            let started = std::time::Instant::now();
            let (swap1, swap2) = tokio::try_join!(
                self.jupiter_swap.swap_transaction(&leg1, &pubkey, None),
                self.jupiter_swap.swap_transaction(&leg2, &pubkey, Some(tip_lamports)),
            ).map_err(|e| { warn!(target: "scout::executor", error = %e,
                                  swap_ms = started.elapsed().as_millis() as u64,
                                  "swap_transaction failed (one of legs)"); e })?;
            let signed1 = sign_versioned_transaction_b64(&swap1.swap_tx_b64, self.wallet.bytes(), swap1.signer_index)
                .map_err(|e| { warn!(target: "scout::executor", leg = "1", error = %e, "sign failed"); e })?;
            let signed2 = sign_versioned_transaction_b64(&swap2.swap_tx_b64, self.wallet.bytes(), swap2.signer_index)
                .map_err(|e| { warn!(target: "scout::executor", leg = "2", error = %e, "sign failed"); e })?;
            let bundle = vec![signed1, signed2];
            let bundle_id = match self.jito.send_bundle(&bundle).await {
                Ok(id) => id,
                Err(e) => {
                    warn!(target: "scout::executor", error = %e, "sendBundle failed");
                    self.maybe_record_jito_back_off(&e.to_string()).await;
                    return Err(e);
                }
            };
            self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
            info!(target: "scout::executor",
                  bundle_id = %bundle_id, expected_net_usdc = net, "🚀 BUNDLE SENT");
            *self.metrics.last_action_at.lock().unwrap() = Some(Utc::now());

            let jito = self.jito.clone();
            let metrics = Arc::clone(&self.metrics);
            let bid = bundle_id.clone();
            tokio::spawn(async move {
                for attempt in 0..6 {
                    tokio::time::sleep(Duration::from_millis(2000)).await;
                    match jito.bundle_status(&bid).await {
                        Ok(Some(slot)) => {
                            metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                            metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                            info!(target: "scout::executor", bundle_id = %bid, slot, attempt,
                                  "🎯 BUNDLE LANDED");
                            return;
                        }
                        Ok(None) => {}
                        Err(e) => warn!(target: "scout::executor",
                                        bundle_id = %bid, attempt, error = %e,
                                        "bundle_status poll failed"),
                    }
                }
                warn!(target: "scout::executor", bundle_id = %bid,
                      "💀 BUNDLE DROPPED — outbid or stale");
            });
        } else {
            // PATH B: atomic Jupiter `/swap-instructions` × 2 → ONE legacy tx
            // → Helius sendTransaction. Atomicity is guaranteed by Solana's
            // tx-level all-or-nothing — if leg2 reverts, leg1 reverts too.
            // No risk of being stuck with intermediate token.
            // v68: for SOL-base routes, pass wrap_unwrap_sol=true so Jupiter
            // handles WSOL wrapping/unwrapping inline. The wallet has no
            // permanent WSOL ATA balance to draw from in SOL-base mode.
            let wrap_sol = base_mint == known::SOL;
            let started = std::time::Instant::now();
            let (ixs1, ixs2) = tokio::try_join!(
                self.jupiter_swap.swap_instructions(&leg1, &pubkey, wrap_sol),
                self.jupiter_swap.swap_instructions(&leg2, &pubkey, wrap_sol),
            ).map_err(|e| { warn!(target: "scout::executor", error = %e,
                                  swap_ms = started.elapsed().as_millis() as u64,
                                  "swap_instructions failed (one of legs)"); e })?;

            // Order: compute budget → ATA setup → leg1 (setup, swap, cleanup)
            //        → leg2 (setup, swap, cleanup). When wrap_sol=true the
            //        base ATA (wSOL) is created by Jupiter's setup_ixs, so
            //        we skip the explicit base ATA creation.
            //
            // Alternation toggle: every other send swaps legs (leg2 first,
            // then leg1). The reversed build runs leg2's swap before leg1
            // has produced the intermediate token — guaranteed on-chain
            // revert on the reversed path. This is intentional per the
            // operator's request (testing/observation, not a profit path).
            let toggle = self.arb_atomic_toggle.fetch_add(1, Ordering::Relaxed);
            let reverse_legs = toggle & 1 == 1;
            info!(target: "scout::executor",
                  toggle, reverse_legs,
                  "arb PATH-B leg ordering");

            let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(16);
            all.push(set_compute_unit_limit_ix(800_000));
            all.push(set_compute_unit_price_ix(10_000));
            if !wrap_sol {
                all.push(create_ata_idempotent_ix(pubkey, pubkey, base_mint));
            }
            all.push(create_ata_idempotent_ix(pubkey, pubkey, intermediate));
            if reverse_legs {
                all.extend(ixs2.setup_ixs);
                all.push(ixs2.swap_ix);
                if let Some(c) = ixs2.cleanup_ix { all.push(c); }
                all.extend(ixs1.setup_ixs);
                all.push(ixs1.swap_ix);
                if let Some(c) = ixs1.cleanup_ix { all.push(c); }
            } else {
                all.extend(ixs1.setup_ixs);
                all.push(ixs1.swap_ix);
                if let Some(c) = ixs1.cleanup_ix { all.push(c); }
                all.extend(ixs2.setup_ixs);
                all.push(ixs2.swap_ix);
                if let Some(c) = ixs2.cleanup_ix { all.push(c); }
            }

            // v74: Helius Sender requires a tip of ≥ 200_000 lamports to one
            // of its 11 stake-weighted tip wallets when routing via /fast.
            // Without this ix the endpoint returns -32602. Pick a tip account
            // round-robin so we don't lock the same one across attempts.
            // The tip is only added when EXECUTOR_USE_JITO=false (PATH B is
            // the Sender path); the Jito path uses its own tip mechanism.
            let helius_tip_lamports: u64 = std::env::var("HELIUS_SENDER_TIP_LAMPORTS")
                .ok().and_then(|s| s.parse().ok())
                .unwrap_or(known::HELIUS_SENDER_MIN_TIP_LAMPORTS);
            if !self.cfg.use_jito && helius_tip_lamports > 0 {
                let n = self.metrics.sent_bundles.load(Ordering::Relaxed) as usize;
                let tip_account = known::HELIUS_TIP_ACCOUNTS
                    [n % known::HELIUS_TIP_ACCOUNTS.len()];
                all.push(system_transfer_ix(
                    pubkey, tip_account, helius_tip_lamports,
                ));
            }

            // v69: build as v0 tx with ALT compression when ALT is configured.
            // The legacy 1232-byte ceiling is overflowed by SOL-base routes
            // (Jupiter's swap-ix list runs ~1600-1900 bytes pre-compression).
            // V0 + ALT brings it under the limit by indexing pre-known accounts.
            let blockhash = fetch_latest_blockhash(&self.helius_url).await
                .map_err(|e| { warn!(target: "scout::executor", error = %e, "blockhash fetch failed"); e })?;
            let alt_addrs = self.resolve_alt().await;
            let signed_b64 = if let Some(alt_pk) = self.cfg.alt_pubkey {
                build_and_sign_v0_tx_b64(
                    &pubkey, &all, &blockhash, &alt_pk, alt_addrs,
                    &[], self.wallet.bytes(),
                ).map_err(|e| { warn!(target: "scout::executor", error = %e, "atomic v0 sign failed"); e })?
            } else {
                // No ALT configured — fall back to legacy. Will fail with
                // "too large" if the tx overflows; the caller logs it.
                build_and_sign_legacy_tx_b64(&pubkey, &all, &blockhash, self.wallet.bytes())
                    .map_err(|e| { warn!(target: "scout::executor", error = %e, "atomic legacy sign failed"); e })?
            };

            // v68: prefer Helius Sender when configured — bypasses Jito's
            // 1/sec public rate limit via stake-weighted private routing
            // (Business+ tier). Falls back to regular sendTransaction if
            // the Sender URL/key isn't set or the call fails.
            let helius_sender_url = std::env::var("HELIUS_SENDER_URL").ok()
                .filter(|s| !s.trim().is_empty());
            let helius_api_key = std::env::var("HELIUS_API_KEY").ok()
                .filter(|s| !s.trim().is_empty());
            let sig = match (helius_sender_url, helius_api_key) {
                (Some(url), Some(key)) => {
                    match self.jito.send_via_helius_sender(&url, &key, &signed_b64).await {
                        Ok(s) => {
                            info!(target: "scout::executor", sig = %s,
                                  expected_net_usdc = net, n_ixs = all.len(),
                                  "🚀 ATOMIC TX SENT via Helius Sender (no Jito, no rate limit)");
                            s
                        }
                        Err(e) => {
                            warn!(target: "scout::executor", error = %e,
                                  "helius-sender failed — falling back to regular sendTransaction");
                            self.helius_rpc.send_transaction(&signed_b64, false).await
                                .map_err(|e| { warn!(target: "scout::executor", error = %e,
                                                     "atomic sendTransaction fallback failed"); e })?
                        }
                    }
                }
                _ => {
                    let s = self.helius_rpc.send_transaction(&signed_b64, false).await
                        .map_err(|e| { warn!(target: "scout::executor", error = %e,
                                             "atomic sendTransaction failed"); e })?;
                    info!(target: "scout::executor",
                          sig = %s, expected_net_usdc = net, n_ixs = all.len(),
                          "🚀 ATOMIC TX SENT (Jupiter ix × 2 in one tx, no Jito)");
                    s
                }
            };
            self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
            *self.metrics.last_action_at.lock().unwrap() = Some(Utc::now());

            // Background poll for landing.
            let rpc = self.helius_rpc.clone();
            let metrics = Arc::clone(&self.metrics);
            let s = sig.clone();
            tokio::spawn(async move {
                for attempt in 0..15 {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    match rpc.signature_status(&s).await {
                        Ok(Some(true)) => {
                            metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                            metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                            info!(target: "scout::executor", sig = %s, attempt,
                                  "🎯 ATOMIC TX LANDED");
                            return;
                        }
                        Ok(Some(false)) => {
                            warn!(target: "scout::executor", sig = %s, attempt,
                                  "💀 atomic tx FAILED on chain (likely slippage/stale) — atomic revert, no fund loss");
                            return;
                        }
                        Ok(None) => {}
                        Err(e) => warn!(target: "scout::executor",
                                        sig = %s, attempt, error = %e,
                                        "atomic status poll failed"),
                    }
                }
                warn!(target: "scout::executor", sig = %s,
                      "💀 atomic tx never confirmed in 12s — likely dropped by RPC");
            });
        }
        Ok(())
    }

    /// Wrap Jupiter `/swap-instructions` for both legs with a MarginFi flash
    /// loan envelope and submit as a single v0-tx Jito bundle. `base_mint`
    /// selects which MarginFi bank to borrow from (USDC or USDT).
    async fn execute_flash_jupiter(
        &self,
        base_mint:    Pubkey,
        intermediate: Pubkey,
        probe_usdc:   u64,
        leg1:         &crate::jupiter_swap::JupiterQuoteResponse,
        leg2:         &crate::jupiter_swap::JupiterQuoteResponse,
        net:          i64,
        pubkey:       &Pubkey,
    ) -> ArbResult<()> {
        let started = std::time::Instant::now();

        // 1) Pull Jupiter swap-instructions for both legs in parallel.
        // Flash path uses stable base (USDC/USDT) — no wrap needed.
        let (ixs1, ixs2) = tokio::try_join!(
            self.jupiter_swap.swap_instructions(leg1, pubkey, false),
            self.jupiter_swap.swap_instructions(leg2, pubkey, false),
        ).map_err(|e| { warn!(target: "scout::executor", error = %e,
                              swap_ms = started.elapsed().as_millis() as u64,
                              "flash-jupiter: swap_instructions failed"); e })?;

        // 2) Build the MarginFi flash-loan envelope. Ix layout in the final
        //    tx (must match `end_index` exactly):
        //      [0] budget_limit
        //      [1] budget_price
        //      [2] tip transfer
        //      [3] ata_usdc (idempotent)
        //      [4] ata_intermediate (idempotent)
        //      [5] start_flashloan
        //      [6] borrow
        //      [7..N] jupiter leg1 (setup_ixs + swap_ix + maybe cleanup)
        //      [N..M] jupiter leg2 (setup_ixs + swap_ix + maybe cleanup)
        //      [M] deposit
        //      [M+1] end_flashloan
        // We compute end_index AFTER all ix lists are appended.
        let base_ata = derive_ata(pubkey, &base_mint);
        // Pick MarginFi bank + vault + oracle for the selected base. Both USDC
        // and USDT are supported; falls back to USDC if base is something else.
        let is_usdt = base_mint == known::USDT;
        let (bank, vault, oracle) = if is_usdt {
            (
                arb_flashloan::MARGINFI_USDT_BANK,
                self.cfg.marginfi_usdt_liquidity_vault.unwrap(),
                self.cfg.usdt_pyth_oracle.unwrap_or(base_mint),
            )
        } else {
            (
                MARGINFI_USDC_BANK,
                self.cfg.marginfi_liquidity_vault.unwrap(),
                self.cfg.usdc_pyth_oracle.unwrap_or(base_mint),
            )
        };
        let mfi_ctx = MarginFiFlashContext {
            group: MARGINFI_MAIN_GROUP,
            bank,
            liquidity_vault: vault,
            liquidity_vault_authority: derive_liquidity_vault_authority(&bank),
            usdc_pyth_oracle: oracle, // field reused for whichever stable we borrow
            marginfi_account: self.cfg.marginfi_account.unwrap(),
        };

        // 3) Compose the instruction list.
        let mut all: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(20);
        all.push(set_compute_unit_limit_ix(1_400_000));
        all.push(set_compute_unit_price_ix(10_000));
        // Tip account: rotate by considered count.
        let tip_idx = (self.metrics.considered.load(Ordering::Relaxed) as usize)
            % JITO_TIP_ACCOUNTS.len();
        all.push(system_transfer_ix(*pubkey, JITO_TIP_ACCOUNTS[tip_idx], self.cfg.jito_tip_lamports));
        all.push(create_ata_idempotent_ix(*pubkey, *pubkey, base_mint));
        all.push(create_ata_idempotent_ix(*pubkey, *pubkey, intermediate));
        // Placeholder slots for flash-loan pre/post — filled below once we
        // know end_index.
        let pre_post_idx = all.len();

        // 4) Append Jupiter leg ixs (positions known after pre_post_idx +
        //    pre_swap_count).
        let mut middle_ixs: Vec<solana_program::instruction::Instruction> = Vec::new();
        middle_ixs.extend(ixs1.setup_ixs);
        middle_ixs.push(ixs1.swap_ix);
        if let Some(c) = ixs1.cleanup_ix { middle_ixs.push(c); }
        middle_ixs.extend(ixs2.setup_ixs);
        middle_ixs.push(ixs2.swap_ix);
        if let Some(c) = ixs2.cleanup_ix { middle_ixs.push(c); }

        // 5) Now compute end_index = pre_post_idx + 2 (pre-swap ixs) +
        //    middle_ixs.len() + 1 (deposit). This is the index of the
        //    end_flashloan ix in the final tx.
        let end_index: u64 = (pre_post_idx + 2 + middle_ixs.len() + 1) as u64;
        let (pre_swap_ixs, post_swap_ixs) = build_marginfi_flash_loan(
            &mfi_ctx, *pubkey, base_ata, probe_usdc, end_index,
        );

        // 6) Splice everything in order.
        all.extend(pre_swap_ixs);
        all.extend(middle_ixs);
        all.extend(post_swap_ixs);

        // 7) v0 tx with ALT compression. Single 256-entry ALT was leaving
        //    Jupiter+flash routes at 1832-2276 bytes (1232 byte cap), failing
        //    every cleared opp at send-time. Multi-ALT extension loads both
        //    EXECUTOR_NATIVE_ALT and EXECUTOR_NATIVE_ALT2 (if set), giving up
        //    to 512 compressible addresses and brings hot-path Jupiter routes
        //    inside the v0 budget.
        let alt_addrs = self.resolve_alt().await;
        if alt_addrs.is_empty() {
            warn!(target: "scout::executor",
                  "flash-jupiter: no ALT loaded — tx will likely overflow; aborting");
            return Ok(());
        }
        let alt_pk = self.cfg.alt_pubkey.unwrap();
        let blockhash = fetch_latest_blockhash(&self.helius_url).await
            .map_err(|e| { warn!(target: "scout::executor", error = %e,
                                 "flash-jupiter: blockhash fetch failed"); e })?;

        // Build the multi-ALT list. Primary ALT always; ALT2 optional.
        let mut alt_list: Vec<(Pubkey, Vec<Pubkey>)> = Vec::with_capacity(2);
        alt_list.push((alt_pk, alt_addrs.to_vec()));
        if let Ok(alt2_str) = std::env::var("EXECUTOR_NATIVE_ALT2") {
            if let Ok(alt2_pk) = alt2_str.parse::<Pubkey>() {
                match self.helius_rpc.fetch_alt_addresses(&alt2_pk).await {
                    Ok(addrs) => {
                        tracing::info!(target: "scout::executor",
                            alt = %alt2_pk, addrs = addrs.len(),
                            "🔗 attached EXECUTOR_NATIVE_ALT2");
                        alt_list.push((alt2_pk, addrs));
                    }
                    Err(e) => tracing::warn!(target: "scout::executor",
                        alt = %alt2_pk, error = %e,
                        "could not fetch EXECUTOR_NATIVE_ALT2 — falling back to single ALT"),
                }
            }
        }

        let signed_b64 = crate::v0_tx::build_and_sign_v0_tx_b64_multi(
            pubkey, &all, &blockhash, &alt_list, &JITO_TIP_ACCOUNTS, self.wallet.bytes(),
        ).map_err(|e| { warn!(target: "scout::executor", error = %e,
                              "flash-jupiter: v0 sign failed"); e })?;

        // 8) Send. v71: when use_jito=false AND HELIUS_SENDER_URL is set,
        //    route EXCLUSIVELY through Helius Sender — no Jito at all,
        //    avoiding the 1/sec rate limit + 2-min back-off cycle. Sender
        //    accepts single atomic txs, which is exactly what the flash
        //    envelope already produces. Otherwise keep the legacy behaviour
        //    (Jito send + optional parallel Sender).
        let helius_sender_url = std::env::var("HELIUS_SENDER_URL").ok()
            .filter(|s| !s.trim().is_empty());
        let helius_api_key = std::env::var("HELIUS_API_KEY").ok()
            .filter(|s| !s.trim().is_empty());
        if !self.cfg.use_jito {
            // Helius-Sender-only path. Requires both URL and API key.
            let (url, key) = match (helius_sender_url, helius_api_key) {
                (Some(u), Some(k)) => (u, k),
                _ => {
                    warn!(target: "scout::executor",
                          "flash-jupiter: use_jito=false but HELIUS_SENDER_URL/HELIUS_API_KEY not set");
                    return Ok(());
                }
            };
            let sig = self.jito.send_via_helius_sender(&url, &key, &signed_b64).await
                .map_err(|e| { warn!(target: "scout::executor", error = %e,
                                     "flash-jupiter: helius-sender failed"); e })?;
            self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
            info!(target: "scout::executor",
                  sig = %sig, expected_net_usdc = net,
                  probe_usdc, n_ixs = all.len(),
                  base = %base_mint,
                  "🚀 FLASH-JUPITER ATOMIC TX SENT via Helius Sender (no Jito)");
            *self.metrics.last_action_at.lock().unwrap() = Some(Utc::now());

            let rpc = self.helius_rpc.clone();
            let metrics = Arc::clone(&self.metrics);
            let s = sig.clone();
            tokio::spawn(async move {
                for attempt in 0..15 {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    match rpc.signature_status(&s).await {
                        Ok(Some(true)) => {
                            metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                            metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                            info!(target: "scout::executor", sig = %s, attempt,
                                  "🎯 FLASH-JUPITER ATOMIC TX LANDED");
                            return;
                        }
                        Ok(Some(false)) => {
                            warn!(target: "scout::executor", sig = %s, attempt,
                                  "💀 flash-jupiter tx FAILED on chain (atomic revert — no fund loss)");
                            return;
                        }
                        Ok(None) => {}
                        Err(e) => warn!(target: "scout::executor",
                                        sig = %s, attempt, error = %e,
                                        "flash-jupiter signature status poll failed"),
                    }
                }
                warn!(target: "scout::executor", sig = %s,
                      "💀 flash-jupiter tx never confirmed in 12s — likely dropped");
            });
            return Ok(());
        }

        // Legacy Jito + parallel Sender path.
        let bundle = vec![signed_b64.clone()];
        if let (Some(url), Some(key)) = (helius_sender_url, helius_api_key) {
            let jito_clone = self.jito.clone();
            let tx_for_sender = signed_b64.clone();
            let metrics = Arc::clone(&self.metrics);
            tokio::spawn(async move {
                match jito_clone.send_via_helius_sender(&url, &key, &tx_for_sender).await {
                    Ok(sig) => {
                        info!(target: "scout::executor", sig = %sig,
                              "🚀 helius-sender ALSO sent (parallel to Jito)");
                        metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => warn!(target: "scout::executor", error = %e,
                                    "helius-sender parallel send failed (Jito still trying)"),
                }
            });
        }
        let bundle_id = match self.jito.send_bundle(&bundle).await {
            Ok(id) => id,
            Err(e) => {
                warn!(target: "scout::executor", error = %e,
                      "flash-jupiter: sendBundle failed");
                self.maybe_record_jito_back_off(&e.to_string()).await;
                return Err(e);
            }
        };
        self.metrics.sent_bundles.fetch_add(1, Ordering::Relaxed);
        info!(target: "scout::executor",
              bundle_id = %bundle_id, expected_net_usdc = net,
              probe_usdc, n_ixs = all.len(),
              base = %base_mint,
              "🚀 FLASH-JUPITER BUNDLE SENT");
        *self.metrics.last_action_at.lock().unwrap() = Some(Utc::now());

        let jito = self.jito.clone();
        let metrics = Arc::clone(&self.metrics);
        let bid = bundle_id.clone();
        tokio::spawn(async move {
            for attempt in 0..6 {
                tokio::time::sleep(Duration::from_millis(2000)).await;
                match jito.bundle_status(&bid).await {
                    Ok(Some(slot)) => {
                        metrics.landed_bundles.fetch_add(1, Ordering::Relaxed);
                        metrics.running_net_usdc.fetch_add(net, Ordering::Relaxed);
                        info!(target: "scout::executor", bundle_id = %bid, slot, attempt,
                              "🎯 FLASH-JUPITER BUNDLE LANDED");
                        return;
                    }
                    Ok(None) => {}
                    Err(e) => warn!(target: "scout::executor",
                                    bundle_id = %bid, attempt, error = %e,
                                    "flash-jupiter status poll failed"),
                }
            }
            warn!(target: "scout::executor", bundle_id = %bid,
                  "💀 flash-jupiter bundle DROPPED — outbid or stale");
        });
        Ok(())
    }
}

/// Extract milliseconds from a Jito rate-limit error like
/// `"Rate limit exceeded ... Retry after 6774ms"`. Returns `None` if the
/// error doesn't carry that suffix.
fn parse_retry_after_ms(err: &str) -> Option<u64> {
    let key = "Retry after ";
    let start = err.find(key)? + key.len();
    let rest = &err[start..];
    let end = rest.find("ms")?;
    rest[..end].trim().parse().ok()
}

#[cfg(test)]
mod retry_after_tests {
    use super::parse_retry_after_ms;

    #[test]
    fn parses_jito_format() {
        let err = "sendBundle rate-limited: Rate limit exceeded. Limit: 1 per second for txn requests. Back-off triggered: Retry after 6774ms";
        assert_eq!(parse_retry_after_ms(err), Some(6774));
    }

    #[test]
    fn parses_max_back_off() {
        let err = "Back-off triggered: Retry after 120000ms";
        assert_eq!(parse_retry_after_ms(err), Some(120000));
    }

    #[test]
    fn returns_none_on_other_errors() {
        assert_eq!(parse_retry_after_ms("Network congested"), None);
        assert_eq!(parse_retry_after_ms("connection refused"), None);
    }
}
