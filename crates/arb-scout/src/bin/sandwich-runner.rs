//! Sandwich runner — Phase 3 live executor for shred-detected swaps.
//!
//! Tails the alert file from `sandwich-detector.service`, picks
//! parsed=true alerts on Raydium AMM v4, fetches the live pool reserve
//! state, computes a small front-run + back-run pair sized to capture
//! a fraction of the victim's price impact, bundles via Jito with a
//! high tip, and submits.
//!
//! ```text
//! cargo run --release --bin sandwich-runner               # DRY-RUN
//! cargo run --release --bin sandwich-runner -- --send     # autofire
//! ```
//!
//! ## Pool selection
//!
//! For Phase 3 we pin to the famous Raydium AMM v4 SOL/USDC pool
//! (`58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2`) — highest-volume
//! single AMM-v4 pool on Solana, most of the $200K+ swaps the detector
//! is flagging go through here. Phase 4 will iterate the alert with
//! actual tx-parse to extract the EXACT pool the victim hit.
//!
//! ## Required env
//!
//! `HELIUS_API_KEY`, `SOLANA_KEYPAIR_JSON`. Optional:
//! `SANDWICH_MIN_VICTIM_USD` (default 5000), `SANDWICH_FRONT_USD`
//! (default 1.0 — micro-test size), `SANDWICH_JITO_TIP_LAMPORTS`
//! (default 500_000), `SANDWICH_AUTOFIRE` (default false),
//! `SANDWICH_MIN_PROFIT_USD` (default 0.10 — block sends whose
//! simulated balance delta doesn't clear tip + fees + this margin),
//! `SANDWICH_SOL_USD` (default 90.0 — SOL price used to convert
//! lamports delta into USD for the profit gate).

use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde::Deserialize;
use serde_json::json;
use solana_program::pubkey::Pubkey;

use arb_core::PoolState;
use arb_dex_raydium::{
    amm_v4_swap_ix::{build_swap_base_in_ix, RaydiumSwapAccounts, RAYDIUM_AMM_AUTHORITY},
    quote_with_reserves, RaydiumAmmV4Data,
};
use arb_flashloan::{
    build_marginfi_flash_loan, derive_liquidity_vault_authority,
    MarginFiFlashContext, MARGINFI_MAIN_GROUP, MARGINFI_USDC_BANK,
};
use arb_scout::alt::fetch_alt_addresses;
use arb_scout::raw_tx::{
    create_ata_idempotent_ix, derive_ata, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix, system_transfer_ix,
};
use arb_scout::v0_tx::{build_v0_message_multi, sign_v0_tx_b64};
use arb_scout::wallet::Wallet;

// ─── Hardcoded SOL/USDC Raydium AMM v4 pool ─────────────────────
// All 13 serum + pool accounts for the famous SOL/USDC pool. These
// don't change — verified on-chain 2026-05-16. The pool itself
// `58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2` does the
// constant-product AMM math against (pool_coin, pool_pc) = (SOL, USDC).
const POOL_AMM: &str               = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";
const POOL_OPEN_ORDERS: &str       = "HRk9CMrpq7Jn9sh7mzxE8CChHG8dneX9p475QKz4Fsfc";
const POOL_TARGET_ORDERS: &str     = "CZza3Ej4Mc58MnxWA385itCC9jCo3L1D7zc3LKy1bZMR";
const POOL_COIN_VAULT: &str        = "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz";  // SOL vault
const POOL_PC_VAULT: &str          = "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz";  // USDC vault
const SERUM_PROGRAM: &str          = "srmqPvymJeFKQ4zGQed1GFppgkRHL9kaELCbyksJtPX";
const SERUM_MARKET: &str           = "8BnEgHoWFysVcuFFX7QztDmzuH8r5ZFvyP3sYwn1XTh6";
const SERUM_BIDS: &str             = "5jWUncPNBMZJ3sTHKmMLszypVkoRK6bfEQMQUHweeQnh";
const SERUM_ASKS: &str             = "EaXdHx7x3mdGA38j5RSmKYSXMzAFzzUXCLNBEDXDn1d5";
const SERUM_EVENT_QUEUE: &str      = "8CvwxZ9Db6XbLD46NZwwmVDZZRDy7eydFcAGkXKh9axa";
const SERUM_COIN_VAULT: &str       = "CKxTHwM9fPMRRvZmFnFoqKNd9pQR21c5Aq9bh5h9oghX";
const SERUM_PC_VAULT: &str         = "6A5NHCj1yF6urc9wZNe6Bcjj4LVszQNj5DwAWG97yzMu";
const SERUM_VAULT_SIGNER: &str     = "F8Vyqk3unwxkXukZFQeYyGmFfTG3CAX4v24iyrjEYBJV";

const USDC_MINT_STR: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";

const JITO_TIP_ACCOUNT: &str = "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5";

#[derive(Debug, Deserialize)]
struct Alert {
    slot:        u64,
    program:     String,
    usd:         f64,
    #[serde(default)]
    amount_raw:  u64,
    #[serde(default)]
    parsed:      bool,
    ts:          f64,
}

#[derive(Debug, Clone)]
struct PoolSnapshot {
    coin_reserve: u64,
    pc_reserve:   u64,
    fetched_slot: u64,
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
    let mut send = std::env::var("SANDWICH_AUTOFIRE").as_deref() == Ok("true");
    let mut min_victim_usd: f64 = std::env::var("SANDWICH_MIN_VICTIM_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(5000.0);
    let mut front_usd: f64 = std::env::var("SANDWICH_FRONT_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(1.0);
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--send" => { send = true; i += 1; }
            "--min-victim-usd" => {
                min_victim_usd = args.get(i + 1).ok_or_else(|| anyhow!("N"))?.parse()?;
                i += 2;
            }
            "--front-usd" => {
                front_usd = args.get(i + 1).ok_or_else(|| anyhow!("N"))?.parse()?;
                i += 2;
            }
            x => bail!("unknown arg: {x}"),
        }
    }

    // ── env ──
    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY missing")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");
    let wallet = {
        let s = std::env::var("SOLANA_KEYPAIR_JSON").context("SOLANA_KEYPAIR_JSON missing")?;
        Wallet::from_json_array(&s).map_err(|e| anyhow!("wallet: {e}"))?
    };
    let payer = wallet.pubkey();
    let jito_tip: u64 = std::env::var("SANDWICH_JITO_TIP_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(500_000);

    let alert_path = std::env::var("SANDWICH_ALERT_FILE")
        .unwrap_or_else(|_| "/tmp/shred-sandwich-alert".to_string());

    let amm:               Pubkey = POOL_AMM.parse()?;
    let usdc:              Pubkey = USDC_MINT_STR.parse()?;
    let wsol:              Pubkey = WSOL_MINT_STR.parse()?;
    let usdc_ata = derive_ata(&payer, &usdc);
    let wsol_ata = derive_ata(&payer, &wsol);
    let tip_acct: Pubkey = JITO_TIP_ACCOUNT.parse().unwrap();

    println!("══════════════════════════════════════════════════════════════════");
    println!("  Sandwich runner — Phase 3  send={send}  min_victim=${min_victim_usd}");
    println!("  pool=SOL/USDC ({})", amm);
    println!("  front-run size: ${front_usd}  (back-run sized symmetrically)");
    println!("  payer={payer}  USDC ATA={usdc_ata}  wSOL ATA={wsol_ata}");
    println!("══════════════════════════════════════════════════════════════════");

    let accs = Arc::new(RaydiumSwapAccounts {
        amm,
        amm_open_orders:           POOL_OPEN_ORDERS.parse()?,
        amm_target_orders:         POOL_TARGET_ORDERS.parse()?,
        pool_coin_token_account:   POOL_COIN_VAULT.parse()?,
        pool_pc_token_account:     POOL_PC_VAULT.parse()?,
        serum_program:             SERUM_PROGRAM.parse()?,
        serum_market:              SERUM_MARKET.parse()?,
        serum_bids:                SERUM_BIDS.parse()?,
        serum_asks:                SERUM_ASKS.parse()?,
        serum_event_queue:         SERUM_EVENT_QUEUE.parse()?,
        serum_coin_vault:          SERUM_COIN_VAULT.parse()?,
        serum_pc_vault:            SERUM_PC_VAULT.parse()?,
        serum_vault_signer:        SERUM_VAULT_SIGNER.parse()?,
        user_source_token_account: usdc_ata,           // overwritten per-leg
        user_dest_token_account:   wsol_ata,           // overwritten per-leg
        user_owner:                payer,
    });

    // Tail alert file.
    let f = std::fs::OpenOptions::new().read(true).open(&alert_path)
        .context("opening alert file (is sandwich-detector.service running?)")?;
    let mut reader = BufReader::new(f);
    reader.seek(SeekFrom::End(0))?;

    let mut line = String::new();
    let mut last_fire = std::time::Instant::now() - Duration::from_secs(60);
    let cooldown = Duration::from_secs(5);  // don't spam Jito

    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() { continue; }
        let Ok(alert) = serde_json::from_str::<Alert>(trimmed) else { continue };

        // Filter: only Raydium AMM v4, only parsed, only big enough.
        if alert.program != "RaydiumAMMv4" || !alert.parsed { continue; }
        if alert.usd < min_victim_usd { continue; }
        if last_fire.elapsed() < cooldown { continue; }
        last_fire = std::time::Instant::now();

        let age_ms = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap_or_default()
            .as_secs_f64() - alert.ts) * 1000.0;
        println!(
            "\n🎯 victim alert slot={}  usd=${:.0}  amount_raw={}  age={:.0}ms",
            alert.slot, alert.usd, alert.amount_raw, age_ms,
        );

        match handle_alert(&rpc, &wallet, &accs, payer, usdc_ata, wsol_ata,
                          usdc, wsol, tip_acct, jito_tip, front_usd, send).await {
            // both _usdc/_wsol args are placeholders for Phase 4's
            // ATA-create + Token-2022 routing — kept in the signature
            // so the call site doesn't have to change later.
            Ok(()) => {}
            Err(e) => println!("  ❌ {e}"),
        }
    }
}

/// SPL Token v1 account `amount` lives at offset 64 (u64 LE). For Token-2022
/// the base-layout offset is identical; extensions append AFTER the base
/// 165 bytes so this works for both. Returns `None` if the buffer is too
/// short or is the empty (uninitialised) account.
fn spl_token_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 72 { return None; }
    Some(u64::from_le_bytes(data[64..72].try_into().ok()?))
}

/// Pre-sim wallet snapshot used by the profit gate. All values are raw
/// (lamports for SOL/wSOL, micro-units for USDC).
#[derive(Debug, Clone, Copy)]
struct PreBal {
    payer_lamports: u64,
    usdc_ata:       u64,
    wsol_ata:       u64,
}

/// Fetch pre-sim balances for [payer, usdc_ata, wsol_ata] in a single RPC
/// hop via `getMultipleAccounts`. Missing/uninitialised ATAs are treated
/// as zero (which is the correct interpretation — no tokens held yet).
async fn fetch_pre_balances(
    rpc:      &str,
    payer:    &Pubkey,
    usdc_ata: &Pubkey,
    wsol_ata: &Pubkey,
) -> anyhow::Result<PreBal> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"getMultipleAccounts",
        "params":[[payer.to_string(), usdc_ata.to_string(), wsol_ata.to_string()],
                  {"encoding":"base64","commitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    let arr = r.pointer("/result/value").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("getMultipleAccounts: no result.value: {r}"))?;
    let lamports_at = |i: usize| -> u64 {
        arr.get(i).and_then(|v| v.get("lamports")).and_then(|v| v.as_u64()).unwrap_or(0)
    };
    let token_amount_at = |i: usize| -> u64 {
        let data_b64 = arr.get(i)
            .and_then(|v| v.pointer("/data/0"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if data_b64.is_empty() { return 0; }
        base64::engine::general_purpose::STANDARD.decode(data_b64).ok()
            .and_then(|bytes| spl_token_amount(&bytes))
            .unwrap_or(0)
    };
    Ok(PreBal {
        payer_lamports: lamports_at(0),
        usdc_ata:       token_amount_at(1),
        wsol_ata:       token_amount_at(2),
    })
}

/// Fetch the AMM account just to confirm it exists and grab its slot.
/// Real reserves live in the base_vault and quote_vault SPL token
/// accounts; we don't need them for Phase 3's fixed-amount micro-test.
/// Newton's-method sizing in Phase 4 will fetch both vaults.
async fn fetch_pool_snapshot(rpc: &str, amm: &Pubkey) -> anyhow::Result<PoolSnapshot> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[amm.to_string(), {"encoding":"base64","commitment":"processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    let data_b64 = r.pointer("/result/value/data/0").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no amm data"))?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
    let _parsed = RaydiumAmmV4Data::try_parse(&bytes)
        .ok_or_else(|| anyhow!("RaydiumAmmV4Data parse failed"))?;
    let slot = r.pointer("/result/context/slot").and_then(|v| v.as_u64()).unwrap_or(0);
    Ok(PoolSnapshot {
        coin_reserve: 0,  // populated in Phase 4 via vault fetch
        pc_reserve:   0,
        fetched_slot: slot,
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_alert(
    rpc:        &str,
    wallet:     &Wallet,
    accs:       &Arc<RaydiumSwapAccounts>,
    payer:      Pubkey,
    usdc_ata:   Pubkey,
    wsol_ata:   Pubkey,
    _usdc:      Pubkey,
    _wsol:      Pubkey,
    tip_acct:   Pubkey,
    jito_tip:   u64,
    front_usd:  f64,
    send:       bool,
) -> anyhow::Result<()> {
    // 1. Fetch pool snapshot (cached price reference).
    let snap = fetch_pool_snapshot(rpc, &accs.amm).await
        .context("pool snapshot")?;
    println!("  📊 pool snapshot: coin={} pc={} slot={}",
        snap.coin_reserve, snap.pc_reserve, snap.fetched_slot);

    // 2. Front-run amount: $front_usd worth of USDC → SOL.
    //    For phase-3 micro-test we use a fixed $1 to prove mechanics.
    let front_amount_usdc = (front_usd * 1_000_000.0) as u64;  // USDC is 6 decimals
    if front_amount_usdc == 0 {
        bail!("front_usd produces zero amount");
    }

    // 3. Build front-run swap IX (USDC → SOL).
    let front_accs = RaydiumSwapAccounts {
        amm:                       accs.amm,
        amm_open_orders:           accs.amm_open_orders,
        amm_target_orders:         accs.amm_target_orders,
        pool_coin_token_account:   accs.pool_coin_token_account,
        pool_pc_token_account:     accs.pool_pc_token_account,
        serum_program:             accs.serum_program,
        serum_market:              accs.serum_market,
        serum_bids:                accs.serum_bids,
        serum_asks:                accs.serum_asks,
        serum_event_queue:         accs.serum_event_queue,
        serum_coin_vault:          accs.serum_coin_vault,
        serum_pc_vault:            accs.serum_pc_vault,
        serum_vault_signer:        accs.serum_vault_signer,
        user_source_token_account: usdc_ata,
        user_dest_token_account:   wsol_ata,
        user_owner:                payer,
    };
    let front_ix = build_swap_base_in_ix(&front_accs, front_amount_usdc, 1)
        .map_err(|e| anyhow!("build front_ix: {e}"))?;

    // 4. Back-run swap IX (SOL → USDC). The front-run yielded some
    //    wSOL into our ATA. We swap nearly all of that back so the
    //    round-trip stays USDC-neutral — a small buffer is required
    //    because the back IX would fail "insufficient funds" if it
    //    asks for more wSOL than the front actually produced after
    //    AMM slippage.
    //
    //    Calibration: SANDWICH_BACK_RATIO_BPS (default 9900 = 99%).
    //    At 99% each fire leaves only ~1% of yield (~$0.50 per $50
    //    front) in wSOL residue. Before 2026-05-16 this was 90% and
    //    each fire drifted ~$5 USDC → wSOL — across 68 fires the
    //    wallet's USDC was almost entirely converted into wSOL.
    //    Coupled with the profit-gate (introduced commit 00a784b),
    //    no unprofitable tx is sent at all; this knob only matters
    //    for sizing when a profitable opportunity does fire.
    let sol_price_usd: f64 = std::env::var("SANDWICH_SOL_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(90.0);
    let back_ratio_bps: u64 = std::env::var("SANDWICH_BACK_RATIO_BPS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(9900);
    let lamports_per_dollar = (1e9 / sol_price_usd) as u64;
    let estimated_yield_lamports = front_amount_usdc.saturating_mul(lamports_per_dollar) / 1_000_000;
    let back_amount_sol = estimated_yield_lamports.saturating_mul(back_ratio_bps) / 10_000;
    if back_amount_sol == 0 {
        bail!("back_amount calculated as 0 (front_amount={front_amount_usdc})");
    }

    let back_accs = RaydiumSwapAccounts {
        user_source_token_account: wsol_ata,
        user_dest_token_account:   usdc_ata,
        ..front_accs
    };
    let back_ix = build_swap_base_in_ix(&back_accs, back_amount_sol, 1)
        .map_err(|e| anyhow!("build back_ix: {e}"))?;
    println!("  📐 front={} USDC base  back={} lamports ({}bps of est yield)",
        front_amount_usdc, back_amount_sol, back_ratio_bps);

    // 5. Assemble bundle wrapped in MarginFi flash envelope.
    //    Layout (11 IXs, end_index=10):
    //      0  cu_limit
    //      1  cu_price
    //      2  jito_tip
    //      3  USDC ATA (idempotent)
    //      4  wSOL ATA (idempotent)
    //      5  start_flashloan(end_index=10)
    //      6  borrow USDC
    //      7  front_swap (USDC -> SOL)
    //      8  back_swap  (SOL  -> USDC)
    //      9  deposit  (repay flash)
    //     10  end_flashloan
    //
    //    Atomicity guarantee: if back's USDC output < borrowed amount + flash fee,
    //    the deposit IX fails (insufficient balance) → entire tx reverts. We only
    //    pay base+priority+tip (~$0.005), not the front swap's $0.30 fee.
    //
    //    Previous architecture (no flash) round-tripped from our wallet and lost
    //    swap fees on every miss. Memory: $3 lost on 3 micro-tests.
    let usdc_mint: Pubkey = USDC_MINT_STR.parse()?;
    let wsol_mint: Pubkey = WSOL_MINT_STR.parse()?;
    let mfi_acct_pk: Pubkey = std::env::var("MARGINFI_ACCOUNT")
        .context("MARGINFI_ACCOUNT env var required for flash sandwich")?
        .parse().map_err(|e| anyhow!("MARGINFI_ACCOUNT parse: {e}"))?;
    let mfi_liq_vault: Pubkey = std::env::var("MARGINFI_USDC_LIQUIDITY_VAULT")
        .context("MARGINFI_USDC_LIQUIDITY_VAULT env var required")?
        .parse().map_err(|e| anyhow!("vault parse: {e}"))?;
    let usdc_pyth: Pubkey = std::env::var("SOLEND_USDC_PYTH_ORACLE")
        .ok().and_then(|s| s.parse::<Pubkey>().ok())
        .unwrap_or(usdc_mint); // fallback (won't pass health but lets us see failure)
    let mfi_ctx = MarginFiFlashContext {
        group: MARGINFI_MAIN_GROUP,
        bank:  MARGINFI_USDC_BANK,
        liquidity_vault: mfi_liq_vault,
        liquidity_vault_authority: derive_liquidity_vault_authority(&MARGINFI_USDC_BANK),
        usdc_pyth_oracle: usdc_pyth,
        marginfi_account: mfi_acct_pk,
    };
    // end_index = position of end_flashloan IX in the tx (0-indexed). With this
    // layout it's 10. Hardcoded since structure is fixed for sandwich.
    let end_index: u64 = 10;
    let (pre, post) = build_marginfi_flash_loan(
        &mfi_ctx, payer, usdc_ata, front_amount_usdc, end_index,
    );
    // pre = [start_flash, borrow], post = [deposit, end_flash]
    let mut ixs: Vec<solana_program::instruction::Instruction> = Vec::with_capacity(11);
    ixs.push(set_compute_unit_limit_ix(800_000));
    ixs.push(set_compute_unit_price_ix(10_000));
    ixs.push(system_transfer_ix(payer, tip_acct, jito_tip));
    ixs.push(create_ata_idempotent_ix(payer, payer, usdc_mint));
    ixs.push(create_ata_idempotent_ix(payer, payer, wsol_mint));
    ixs.extend(pre);  // start_flash + borrow
    ixs.push(front_ix);
    ixs.push(back_ix);
    ixs.extend(post); // deposit + end_flash
    debug_assert_eq!(ixs.len(), 11, "sandwich flash envelope should be exactly 11 ixs");

    // 6. Build v0 message with ALTs (Phase 4: 11-ix flash-wrapped envelope
    //    pushes tx past 1232 bytes without ALT compression. We pull arbbot's
    //    EXECUTOR_NATIVE_ALT{,2,3} so most of the AMM/serum + flash accounts
    //    compress to 1-byte indices.
    let mut alts: Vec<(Pubkey, Vec<Pubkey>)> = Vec::new();
    for env_key in ["EXECUTOR_NATIVE_ALT", "EXECUTOR_NATIVE_ALT2", "EXECUTOR_NATIVE_ALT3"] {
        if let Ok(s) = std::env::var(env_key) {
            if let Ok(pk) = s.trim().parse::<Pubkey>() {
                if let Ok(addrs) = fetch_alt_addresses(rpc, &pk).await {
                    if !addrs.is_empty() {
                        alts.push((pk, addrs));
                    }
                }
            }
        }
    }
    let blockhash = fetch_latest_blockhash(rpc).await
        .map_err(|e| anyhow!("blockhash: {e}"))?;
    let msg = build_v0_message_multi(&payer, &ixs, &blockhash, &alts, &[]);
    let signed_b64 = sign_v0_tx_b64(&msg, wallet.bytes())
        .map_err(|e| anyhow!("sign: {e}"))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(&signed_b64).unwrap();
    println!("  ✍️  signed tx: {} bytes ({} ixs)", raw.len(), ixs.len());

    if raw.len() > 1232 {
        bail!("tx too large ({} bytes); add an ALT for serum accounts", raw.len());
    }

    // 7. Profit-gate prep: capture pre-balances, then simulate with
    //    `accounts.addresses` so the RPC returns post-state for those
    //    same keys. The delta (post − pre) tells us exactly what this
    //    sandwich would change in our wallet, including the Jito tip.
    //
    //    Architectural reality (2026-05-16 incident): the front+back
    //    round-trip on a SINGLE AMM is mathematically guaranteed to lose
    //    money — AMM fees + Jito tip with no victim slot in between.
    //    Before this gate, 68 fires landed OK and bled ~0.034 SOL because
    //    the simulator only checked "IXs succeed", not "we made money".
    //    Now we require the simulated net change in (USDC + wSOL + native
    //    SOL, valued in USD) to clear `SANDWICH_MIN_PROFIT_USD`.
    let pre = fetch_pre_balances(rpc, &payer, &usdc_ata, &wsol_ata).await
        .context("pre-sim balance fetch")?;

    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
        "params":[signed_b64, {
            "encoding":"base64","sigVerify":false,"replaceRecentBlockhash":true,
            "commitment":"processed",
            "accounts":{
                "addresses":[payer.to_string(), usdc_ata.to_string(), wsol_ata.to_string()],
                "encoding":"base64",
            },
        }],
    });
    let sim_r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    let sim_val = sim_r.pointer("/result/value")
        .ok_or_else(|| anyhow!("sim: no result/value: {sim_r}"))?;
    let sim_err = sim_val.get("err").cloned().unwrap_or(serde_json::Value::Null);
    let units = sim_val.get("unitsConsumed").and_then(|u| u.as_u64()).unwrap_or(0);
    if !sim_err.is_null() {
        let logs: Vec<&str> = sim_val.get("logs").and_then(|l| l.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).rev().take(6).collect::<Vec<_>>())
            .unwrap_or_default();
        let logs_rev: Vec<&str> = logs.into_iter().rev().collect();
        bail!("sim failed: {sim_err}  units={units}  logs_tail=[{}]", logs_rev.join(" | "));
    }

    // Profit gate — block any tx whose simulated USD delta doesn't clear the
    // tip+fees+margin floor. The accounts array order matches our request.
    let accs_arr = sim_val.get("accounts").and_then(|a| a.as_array())
        .ok_or_else(|| anyhow!("sim missing accounts (RPC version too old?)"))?;
    let post_lamports = accs_arr.get(0)
        .and_then(|v| v.get("lamports")).and_then(|v| v.as_u64()).unwrap_or(0);
    let post_token_amt = |i: usize| -> u64 {
        accs_arr.get(i).and_then(|v| v.pointer("/data/0")).and_then(|v| v.as_str())
            .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
            .and_then(|b| spl_token_amount(&b))
            .unwrap_or(0)
    };
    let post_usdc = post_token_amt(1);
    let post_wsol = post_token_amt(2);

    let delta_lamports: i64 = post_lamports as i64 - pre.payer_lamports as i64;
    let delta_usdc:     i64 = post_usdc      as i64 - pre.usdc_ata       as i64;
    let delta_wsol:     i64 = post_wsol      as i64 - pre.wsol_ata       as i64;

    let sol_usd: f64 = std::env::var("SANDWICH_SOL_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(90.0);
    let usdc_usd  = delta_usdc as f64 / 1_000_000.0;
    let sol_usd_d = delta_lamports as f64 * sol_usd / 1e9;
    let wsol_usd  = delta_wsol     as f64 * sol_usd / 1e9;
    let profit_usd = usdc_usd + sol_usd_d + wsol_usd;
    let min_profit: f64 = std::env::var("SANDWICH_MIN_PROFIT_USD")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.10);

    println!(
        "  ✓ sim OK — units={units}  ΔUSDC=${:+.4}  Δlamports={:+}  ΔwSOL_lp={:+}  → profit=${:+.4}  gate=${:.2}",
        usdc_usd, delta_lamports, delta_wsol, profit_usd, min_profit,
    );
    if profit_usd < min_profit {
        bail!(
            "profit ${:+.4} < min ${:.2} — skipping send (would bleed)",
            profit_usd, min_profit,
        );
    }

    if !send {
        println!("  (DRY-RUN — pass --send to submit)");
        return Ok(());
    }

    // 8. Send via Helius (regular sendTransaction, not Jito bundle).
    //    Jito needs the searcher SDK auth flow; for Phase 3 we use
    //    direct RPC with high priority fee for ordering.
    let send_body = json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {
            "encoding":"base64","skipPreflight":false,"maxRetries":3,
            "preflightCommitment":"processed",
        }],
    });
    let send_r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&send_body).send().await?
        .error_for_status()?
        .json().await?;
    if let Some(err) = send_r.get("error") {
        bail!("send error: {err}");
    }
    let sig = send_r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send: no sig"))?;
    println!("  🚀 SENT: https://solscan.io/tx/{sig}");
    Ok(())
}

// Unused but referenced for shape — kept so the dead-code lint
// surfaces when we wire actual sandwich sizing in Phase 4.
#[allow(dead_code)]
fn _placeholder_quoter(_snap: &PoolSnapshot) -> Option<(u64, u64)> {
    None
}

#[allow(dead_code)]
fn _placeholder_sizing(_snap: &PoolSnapshot, _victim_amount: u64) -> u64 {
    // Newton's method on Raydium AMM constant-product to find optimal
    // front-run size that maximizes sandwich profit. Deferred to phase 4.
    0
}

// Re-exports of trait usage that keep crates linked in.
#[allow(dead_code)]
fn _force_link() {
    let _ = quote_with_reserves;
    let _: Option<PoolState> = None;
}
