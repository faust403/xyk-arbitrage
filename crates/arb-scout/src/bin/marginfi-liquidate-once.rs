//! One-shot MarginFi liquidation executor — builds and (optionally)
//! sends a single flash-wrapped liquidate tx for a given underwater
//! account.
//!
//! ```text
//! cargo run --release --bin marginfi-liquidate-once -- <CANDIDATE_PUBKEY>
//! cargo run --release --bin marginfi-liquidate-once -- <PK> --send
//! cargo run --release --bin marginfi-liquidate-once -- <PK> --close-factor 0.25
//! ```
//!
//! ## Default behavior: DRY-RUN
//!
//! Without `--send`, the binary builds the v0 tx, signs it, then calls
//! `simulateTransaction` and prints the result. No bundle/tx is
//! submitted, no SOL is spent. Use this to validate the IX layout +
//! account ordering + observation set before flipping live.
//!
//! ## What it does (per candidate)
//!
//! 1. Load `BankCache` (gPA, dataSize=1864 — all 425 banks).
//! 2. Fetch oracle prices for every Pyth-observable bank via gMA.
//! 3. Fetch the specific candidate's MarginfiAccount via getAccountInfo.
//! 4. Compute health → pick best asset/liab banks (highest USD).
//! 5. Refuse if either oracle isn't Push-V2 observable (we exclude
//!    PythLegacy on purpose — Wormhole-CPI updates aren't atomically
//!    refreshable from shred top-level, same blocker that killed Solend
//!    atomic on 2026-05-14).
//! 6. Compute `flash_amount` (liab tokens to flash-borrow) and
//!    `asset_amount` (collateral tokens to seize). Defaults: 50%
//!    close-factor, 5% bonus assumption — both env-overridable.
//! 7. Try `pyth_shred_dump::build_atomic_pyth_ix` for the liab oracle.
//!    If unavailable, proceed without atomic prepend (sim will tell us
//!    if the price was already fresh enough).
//! 8. Jupiter `/quote` + `/swap-instructions` for `asset_mint → liab_mint`.
//! 9. `plan_liquidation_tx` from `marginfi_liq` assembles the 8-IX
//!    flash envelope. Multi-ALT v0 message via `EXECUTOR_PYTH_ALT` +
//!    addresses from the Jupiter response.
//! 10. Sign with our wallet, simulate, print logs. If `--send`,
//!     `sendTransaction` over Helius RPC and print the signature.

use anyhow::{anyhow, bail, Context};
use base64::Engine;
use serde_json::{json, Value};
use solana_program::pubkey::Pubkey;

use arb_scout::{
    alt::fetch_alt_addresses,
    jupiter_swap::JupiterSwapClient,
    marginfi_liq::{
        compute_health, fetch_bank_cache, fetch_bank_prices, parse_marginfi_account,
        plan_liquidation_tx, LiquidationPlanInputs,
    },
    pyth_shred_dump,
    raw_tx::fetch_latest_blockhash,
    v0_tx::{build_v0_message_multi, sign_v0_tx_b64},
    wallet::Wallet,
};

const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pivKeVQ8nfBwUKxukH4j",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    // ── args ──
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        bail!("usage: marginfi-liquidate-once <CANDIDATE_PUBKEY> [--send] [--close-factor N] [--bonus N]");
    }
    let candidate_pk: Pubkey = args[1].parse()
        .context("first arg must be the candidate (underwater MarginfiAccount) pubkey")?;

    let mut send = false;
    let mut close_factor: f64 = std::env::var("MARGINFI_CLOSE_FACTOR")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.5);
    let mut bonus: f64 = std::env::var("MARGINFI_LIQ_BONUS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(0.05);
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "--send" => { send = true; i += 1; }
            "--close-factor" => {
                close_factor = args.get(i + 1).ok_or_else(|| anyhow!("--close-factor needs N"))?.parse()?;
                i += 2;
            }
            "--bonus" => {
                bonus = args.get(i + 1).ok_or_else(|| anyhow!("--bonus needs N"))?.parse()?;
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
        Wallet::from_json_array(&s).map_err(|e| anyhow!("wallet load: {e}"))?
    };
    let payer = wallet.pubkey();

    let liquidator_account: Pubkey = std::env::var("MARGINFI_ACCOUNT")
        .context("MARGINFI_ACCOUNT missing (our wallet's initialized marginfi_account)")?
        .parse().context("MARGINFI_ACCOUNT not a pubkey")?;

    let pyth_alt: Option<Pubkey> = std::env::var("EXECUTOR_PYTH_ALT").ok()
        .and_then(|s| s.parse().ok());

    let jito_tip_lamports: u64 = std::env::var("MARGINFI_JITO_TIP_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(500_000);

    println!("══════════════════════════════════════════════════════════════════");
    println!("  MarginFi liquidate-once  (mode: {})", if send { "SEND" } else { "DRY-RUN" });
    println!("══════════════════════════════════════════════════════════════════");
    println!("payer:       {payer}");
    println!("liquidator:  {liquidator_account}");
    println!("target:      {candidate_pk}");
    println!("close_factor:{close_factor}  bonus:{bonus}  tip:{jito_tip_lamports}");
    println!();

    // ── 1. bank cache ──
    println!("→ loading bank cache");
    let cache = fetch_bank_cache(&rpc).await?;
    println!("✓ banks: {} total, {} Pyth-observable",
        cache.by_bank.len(),
        cache.by_bank.values().filter(|b| b.oracle_setup.is_pyth_observable()).count());

    // ── 2. prices ──
    println!("→ fetching oracle prices");
    let prices = fetch_bank_prices(&rpc, &cache).await?;
    println!("✓ prices: {} banks priced", prices.len());

    // ── 3. fetch target ──
    println!("→ fetching candidate account");
    let bytes = rpc_get_account_data(&rpc, &candidate_pk).await?;
    let parsed = parse_marginfi_account(candidate_pk, &bytes)
        .ok_or_else(|| anyhow!("could not parse MarginfiAccount at {candidate_pk}"))?;
    println!("✓ candidate parsed; {} active balances",
        parsed.active_balances().count());

    // ── 4. compute health ──
    let candidate = compute_health(&parsed, &cache, &prices);
    println!("\n=== candidate health ===");
    println!("  health_usd:  ${:.2}", candidate.health_usd);
    println!("  total_asset: ${:.2}", candidate.total_asset_usd);
    println!("  total_liab:  ${:.2}", candidate.total_liab_usd);
    println!("  best_asset_bank: {:?}", candidate.best_asset_bank);
    println!("  best_liab_bank:  {:?}", candidate.best_liab_bank);

    if candidate.health_usd >= 0.0 {
        bail!("not underwater (health >= 0) — no liquidation");
    }
    let asset_bank_pk = candidate.best_asset_bank
        .ok_or_else(|| anyhow!("no best asset bank — collateral oracle not Push-V2 observable"))?;
    let liab_bank_pk = candidate.best_liab_bank
        .ok_or_else(|| anyhow!("no best liab bank — debt oracle not Push-V2 observable"))?;
    let asset_bank = cache.by_bank.get(&asset_bank_pk).unwrap().clone();
    let liab_bank  = cache.by_bank.get(&liab_bank_pk).unwrap().clone();
    let asset_price = *prices.get(&asset_bank_pk)
        .ok_or_else(|| anyhow!("no price for asset bank"))?;
    let liab_price  = *prices.get(&liab_bank_pk)
        .ok_or_else(|| anyhow!("no price for liab bank"))?;

    println!("\n=== pick ===");
    println!("  asset_bank ({}): mint={} price=${:.4} weight_maint={}",
        asset_bank.address, asset_bank.mint, asset_price, asset_bank.asset_weight_maint);
    println!("  liab_bank  ({}): mint={} price=${:.4} weight_maint={}",
        liab_bank.address, liab_bank.mint, liab_price, liab_bank.liability_weight_maint);

    // ── 5. amounts ──
    let liab_usd_to_repay = candidate.total_liab_usd * close_factor;
    let liab_tokens = liab_usd_to_repay / liab_price;
    let flash_amount = (liab_tokens * 10f64.powi(liab_bank.mint_decimals as i32)) as u64;

    let asset_usd_to_seize = liab_usd_to_repay * (1.0 + bonus);
    let asset_tokens = asset_usd_to_seize / asset_price;
    let asset_amount = (asset_tokens * 10f64.powi(asset_bank.mint_decimals as i32)) as u64;

    println!("\n=== amounts ===");
    println!("  flash_amount: {flash_amount}  ({:.6} {} = ${:.2})",
        liab_tokens, liab_bank.mint, liab_usd_to_repay);
    println!("  asset_amount: {asset_amount}  ({:.6} {} = ${:.2})",
        asset_tokens, asset_bank.mint, asset_usd_to_seize);

    // ── 6. Pyth prepend (best-effort) ──
    let pyth_prepend = pyth_shred_dump::build_atomic_pyth_ix(&liab_bank.primary_oracle(), &payer);
    println!("\n=== pyth prepend ===");
    match &pyth_prepend {
        Some(ix) => println!("  ✓ atomic Pyth IX from shred dump ({} bytes, {} accounts)",
            ix.data.len(), ix.accounts.len()),
        None => println!("  · no fresh dump for {} — proceeding without atomic prepend",
            liab_bank.primary_oracle()),
    }

    // ── 7. Jupiter swap (asset → liab) ──
    println!("\n=== jupiter quote (asset → liab) ===");
    let jup = JupiterSwapClient::new();
    let slippage_bps = 100;  // 1%
    let quote = jup.quote(&asset_bank.mint, &liab_bank.mint, asset_amount, slippage_bps).await
        .map_err(|e| anyhow!("jupiter quote: {e}"))?;
    println!("  ✓ quote: {} {} → {} {} (slippage {}bps)",
        asset_amount, asset_bank.mint, quote.out_amount, liab_bank.mint, slippage_bps);

    let jup_ixs = jup.swap_instructions(&quote, &payer, false).await
        .map_err(|e| anyhow!("jupiter swap-instructions: {e}"))?;
    let mut jupiter_swap_ixs: Vec<solana_program::instruction::Instruction> = Vec::new();
    jupiter_swap_ixs.extend(jup_ixs.setup_ixs.iter().cloned());
    jupiter_swap_ixs.push(jup_ixs.swap_ix.clone());
    if let Some(cleanup) = &jup_ixs.cleanup_ix {
        jupiter_swap_ixs.push(cleanup.clone());
    }
    println!("  ✓ jupiter ixs: setup={} swap=1 cleanup={} ALTs={}",
        jup_ixs.setup_ixs.len(),
        if jup_ixs.cleanup_ix.is_some() { 1 } else { 0 },
        jup_ixs.address_lookup_tables.len());

    // ── 8. observation pairs ──
    // For the liquidatee, walk every active balance IN STORAGE ORDER.
    // MarginFi's get_health_components iterates balances[].filter(active)
    // and for each, reads the next remaining_account as bank_pk, then
    // matches against balance.bank_pk via `check_eq!` — mismatch = 6008
    // InvalidBankAccount. So we MUST include EVERY active balance,
    // including the asset_bank and liab_bank (yes, they appear in both
    // the named-accounts slots AND in observations — the program walks
    // them in the observation slice during the post-liquidate health
    // snapshot).
    //
    // Also refuse early if ANY observation bank isn't Push-V2 — that
    // would revert with code 6049 SwitchboardStalePrice mid-tx.
    let liquidator_obs: Vec<(Pubkey, Pubkey)> = Vec::new();
    let mut liquidatee_obs: Vec<(Pubkey, Pubkey)> = Vec::new();
    let mut blockers: Vec<String> = Vec::new();
    for bal in parsed.active_balances() {
        let Some(bs) = cache.by_bank.get(&bal.bank_pk) else {
            blockers.push(format!("bank {} not in cache", bal.bank_pk));
            continue;
        };
        if !bs.oracle_setup.is_pyth_observable() {
            blockers.push(format!(
                "bank {} uses non-Push-V2 oracle ({:?}); MarginFi would revert with SwitchboardStalePrice (6049)",
                bs.address, bs.oracle_setup,
            ));
            continue;
        }
        liquidatee_obs.push((bal.bank_pk, bs.primary_oracle()));
    }
    println!("\n=== observations ===");
    println!("  liquidator: {} pairs", liquidator_obs.len());
    println!("  liquidatee: {} pairs", liquidatee_obs.len());
    if !blockers.is_empty() {
        println!("\n=== BLOCKERS — pick a different candidate ===");
        for b in &blockers { println!("  - {b}"); }
        bail!("candidate has {} non-Push-V2 oracle exposure(s); refusing to build tx", blockers.len());
    }

    // ── 9. plan tx ──
    let tip_account = JITO_TIP_ACCOUNTS[0].parse::<Pubkey>().unwrap();
    let inputs = LiquidationPlanInputs {
        liquidator_authority: payer,
        liquidator_account,
        liquidatee_account: candidate_pk,
        asset_bank: &asset_bank,
        liab_bank: &liab_bank,
        asset_amount,
        flash_amount,
        liquidator_observations: liquidator_obs,
        liquidatee_observations: liquidatee_obs,
        pyth_prepend,
        jupiter_swap_ixs,
        compute_unit_limit: 1_400_000,
        priority_micro_lamports: 2_000,
        jito_tip: Some((tip_account, jito_tip_lamports)),
    };
    let ixs = plan_liquidation_tx(&inputs);
    println!("\n=== tx plan ===");
    println!("  total ixs: {}", ixs.len());

    // ── 10. ALTs ──
    let mut alts: Vec<(Pubkey, Vec<Pubkey>)> = Vec::new();
    if let Some(alt_pk) = pyth_alt {
        let addrs = fetch_alt_addresses(&rpc, &alt_pk).await
            .map_err(|e| anyhow!("fetch pyth ALT: {e}"))?;
        println!("  pyth ALT: {alt_pk} ({} addrs)", addrs.len());
        alts.push((alt_pk, addrs));
    }
    for jup_alt_pk in &jup_ixs.address_lookup_tables {
        let addrs = fetch_alt_addresses(&rpc, jup_alt_pk).await
            .map_err(|e| anyhow!("fetch jupiter ALT {jup_alt_pk}: {e}"))?;
        println!("  jupiter ALT: {jup_alt_pk} ({} addrs)", addrs.len());
        alts.push((*jup_alt_pk, addrs));
    }

    // ── 11. build v0 message + sign ──
    let blockhash = fetch_latest_blockhash(&rpc).await
        .map_err(|e| anyhow!("blockhash: {e}"))?;
    let msg = build_v0_message_multi(&payer, &ixs, &blockhash, &alts, &[]);
    println!("\n=== v0 message ===");
    println!("  static_keys: {}", msg.static_account_keys.len());
    println!("  ALT lookups: {}", msg.address_table_lookups.len());

    let signed_b64 = sign_v0_tx_b64(&msg, wallet.bytes())
        .map_err(|e| anyhow!("sign: {e}"))?;
    let signed_bytes = base64::engine::general_purpose::STANDARD.decode(&signed_b64).unwrap();
    println!("  signed_tx_size: {} bytes (limit 1232)", signed_bytes.len());

    // ── 12. simulate ──
    println!("\n=== simulate ===");
    let sim = rpc_simulate(&rpc, &signed_b64).await?;
    match &sim.err {
        None => println!("  ✓ SIMULATION SUCCEEDED — units_consumed={}", sim.units_consumed),
        Some(e) => println!("  ✗ simulation err: {e}"),
    }
    println!("  --- logs (last 30) ---");
    for line in sim.logs.iter().rev().take(30).rev() {
        println!("    {line}");
    }

    // ── 13. send (only if --send AND sim passed) ──
    if send {
        if sim.err.is_some() {
            bail!("simulation failed; refusing to --send (override by patching after fixing the err)");
        }
        println!("\n=== sending ===");
        let sig = rpc_send(&rpc, &signed_b64).await?;
        println!("  🚀 sent: {sig}");
        println!("  https://solscan.io/tx/{sig}");
    } else {
        println!("\n(DRY-RUN — pass --send to submit)");
    }

    Ok(())
}

async fn rpc_get_account_data(rpc: &str, pk: &Pubkey) -> anyhow::Result<Vec<u8>> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[pk.to_string(), {"encoding":"base64","commitment":"confirmed"}],
    });
    let r: Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    let data_b64 = r.pointer("/result/value/data/0").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("no account data at {pk}: {r}"))?;
    Ok(base64::engine::general_purpose::STANDARD.decode(data_b64)?)
}

struct SimOutcome {
    err: Option<String>,
    logs: Vec<String>,
    units_consumed: u64,
}

async fn rpc_simulate(rpc: &str, signed_b64: &str) -> anyhow::Result<SimOutcome> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
        "params":[signed_b64, {
            "encoding":"base64", "sigVerify": false, "replaceRecentBlockhash": true,
            "commitment":"processed",
        }],
    });
    let r: Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    if let Some(err) = r.get("error") {
        bail!("simulate RPC error: {err}");
    }
    let val = r.pointer("/result/value")
        .ok_or_else(|| anyhow!("simulate: missing result.value: {r}"))?;
    let err = val.get("err").and_then(|e| if e.is_null() { None } else { Some(format!("{e}")) });
    let logs = val.get("logs").and_then(|l| l.as_array()).cloned().unwrap_or_default()
        .into_iter().filter_map(|l| l.as_str().map(String::from)).collect();
    let units_consumed = val.get("unitsConsumed").and_then(|u| u.as_u64()).unwrap_or(0);
    Ok(SimOutcome { err, logs, units_consumed })
}

async fn rpc_send(rpc: &str, signed_b64: &str) -> anyhow::Result<String> {
    let body = json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {
            "encoding":"base64", "skipPreflight": false, "maxRetries": 5,
            "preflightCommitment":"processed",
        }],
    });
    let r: Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    if let Some(err) = r.get("error") {
        bail!("send RPC error: {err}");
    }
    let sig = r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send: missing result.sig: {r}"))?;
    Ok(sig.to_string())
}
