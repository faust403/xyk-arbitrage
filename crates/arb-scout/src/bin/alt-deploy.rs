//! Deploy an Address Lookup Table on mainnet pre-loaded with every "hot"
//! pubkey our native arb path references in a single bundle.
//!
//! Run once locally (uses the wallet from `.env`):
//!
//! ```
//! cargo run --release --bin alt-deploy
//! ```
//!
//! Outputs the ALT pubkey to paste into Railway as `EXECUTOR_NATIVE_ALT`.
//!
//! ## Why this exists
//!
//! `arb-scout/src/native_executor.rs` builds v0 (versioned) transactions and
//! references `EXECUTOR_NATIVE_ALT` to compress account keys. With a flash
//! loan envelope (refresh + borrow + repay) wrapped around a 2-leg swap,
//! the legacy wire format runs ~1500 bytes — over Solana's 1232-byte
//! mainnet limit, hence Jito's `transaction #0 could not be decoded`. ALT
//! compression replaces each in-table 32-byte pubkey with a 1-byte index,
//! dropping ~31 bytes per match, comfortably under the limit.
//!
//! ## What goes into the ALT
//!
//! Anything referenced by EVERY native bundle (or by every bundle of a
//! specific shape). Specifically excluded: program ids and signers — those
//! must remain in `static_account_keys` per Solana's v0 rules.
//!
//!  * USDC, USDT, WSOL mints
//!  * 8 Jito tip accounts
//!  * The 8 Solend USDC reserve addresses (when env vars are set)
//!  * Sysvar Clock (refresh_reserve account #4)
//!  * 20 pool addresses (from scout-pools.toml)
//!  * 2 vaults per pool, fetched + parsed by DEX
//!  * Whirlpool oracle PDA / Raydium CLMM observation_key / Raydium CLMM amm_config
//!  * Obric x/y price feeds
//!  * Wallet ATAs for USDC, USDT, WSOL
//!
//! ## Cost
//!
//! ~0.0024 SOL in tx fees + ~0.01 SOL rent-exempt deposit on the ALT
//! account. The user's wallet has ample SOL for this (~0.113 SOL).

use std::collections::BTreeSet;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;
use solana_program::sysvar;

use arb_dex_obric::ObricV2Data;
use arb_dex_orca::{
    WhirlpoolData,
    derive_tick_array_pda as wp_derive_tick_array_pda,
    tick_array_start_index as wp_tick_array_start_index,
};
use arb_dex_raydium::{
    RaydiumAmmV4Data, RaydiumClmmData,
    derive_tick_array_pda as clmm_derive_tick_array_pda,
    tick_array_start_index as clmm_tick_array_start_index,
    TICK_ARRAY_SIZE as CLMM_TICK_ARRAY_SIZE,
};
use arb_dex_meteora::{
    bin_array_index_for, derive_bin_array_pda,
    derive_event_authority_pda as dlmm_event_authority,
    derive_oracle_pda as dlmm_oracle_pda,
    LbPairData, METEORA_DLMM_PROGRAM,
};
use arb_scout::config::ScoutConfig;
use arb_scout::known::{JITO_TIP_ACCOUNTS, SOL, USDC, USDT};
use arb_scout::raw_tx::{
    build_and_sign_legacy_tx_b64, derive_ata, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix,
};
use arb_scout::wallet::Wallet;
use arb_core::PoolKind;

const ADDRESS_LOOKUP_TABLE_PROGRAM: Pubkey =
    pubkey!("AddressLookupTab1e1111111111111111111111111");
const SYSTEM_PROGRAM: Pubkey =
    pubkey!("11111111111111111111111111111111");

/// Max addresses per `ExtendLookupTable` we send. Limit is set by the 1232-byte
/// tx ceiling: ~310 bytes overhead + 32 bytes per address = ~28 max.
/// 25 leaves headroom for compute-budget ix size.
const ADDRESSES_PER_EXTEND_TX: usize = 25;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    // ── 1. Load wallet, RPC, pools toml ────────────────────────────────────
    let wallet = Wallet::from_env().context("loading wallet")?
        .ok_or_else(|| anyhow!("no wallet — set SOLANA_KEYPAIR_JSON"))?;
    let payer = wallet.pubkey();
    println!("Wallet: {payer}");

    let rpc_url = std::env::var("HELIUS_RPC_URL").or_else(|_| {
        std::env::var("HELIUS_API_KEY").map(|k|
            format!("https://mainnet.helius-rpc.com/?api-key={k}"))
    }).context("HELIUS_RPC_URL or HELIUS_API_KEY")?;

    let cfg_path = std::env::var("SCOUT_CONFIG")
        .unwrap_or_else(|_| "config/scout-pools.toml".to_string());
    let scout_cfg = ScoutConfig::load(&cfg_path)
        .context(format!("loading {cfg_path}"))?;
    println!("Loaded {} pools from {cfg_path}", scout_cfg.pools.len());

    // ── 2. Build hot-address set ───────────────────────────────────────────
    let mut hot: BTreeSet<Pubkey> = BTreeSet::new();

    // Static well-known
    hot.insert(USDC);
    hot.insert(USDT);
    hot.insert(SOL);
    hot.insert(sysvar::clock::id());
    hot.insert(sysvar::rent::id());
    for tip in JITO_TIP_ACCOUNTS { hot.insert(tip); }

    // Solend USDC reserve addresses (skip silently if not set — flash-loan
    // path simply can't fire without them anyway)
    for k in [
        "SOLEND_LENDING_MARKET", "SOLEND_LENDING_MARKET_AUTHORITY",
        "SOLEND_USDC_RESERVE", "SOLEND_USDC_RESERVE_LIQUIDITY_SUPPLY",
        "SOLEND_USDC_FEE_RECEIVER", "SOLEND_USDC_HOST_FEE_RECEIVER",
        "SOLEND_USDC_PYTH_ORACLE", "SOLEND_USDC_SWITCHBOARD_ORACLE",
    ] {
        if let Ok(s) = std::env::var(k) {
            if let Ok(pk) = Pubkey::from_str(s.trim()) { hot.insert(pk); }
        }
    }

    // Wallet's ATAs for USDC / USDT / WSOL
    hot.insert(derive_ata(&payer, &USDC));
    hot.insert(derive_ata(&payer, &USDT));
    hot.insert(derive_ata(&payer, &SOL));

    // MarginFi v2 flash-loan addresses (group, bank, vaults). These are
    // added unconditionally — even if the operator runs Solend today, an
    // ALT row for an unused address is harmless (1 byte savings if used,
    // 0 cost otherwise). MarginFi origination_fee on USDC bank ≈ 0,
    // making these flash loans the path forward for tiny-spread arb.
    let marginfi_program = Pubkey::from_str("MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA")?;
    let mfi_group = Pubkey::from_str("4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG8")?;
    let mfi_bank = Pubkey::from_str("2s37akK2eyBbp8DZgCm7RtsaEz8eJP3Nxd4urLHQv7yB")?;
    let (mfi_liq_auth, _) = Pubkey::find_program_address(
        &[b"liquidity_vault_auth", mfi_bank.as_ref()],
        &marginfi_program,
    );
    let (mfi_liq_vault, _) = Pubkey::find_program_address(
        &[b"liquidity_vault", mfi_bank.as_ref()],
        &marginfi_program,
    );
    hot.insert(mfi_group);
    hot.insert(mfi_bank);
    hot.insert(mfi_liq_auth);
    hot.insert(mfi_liq_vault);
    if let Ok(s) = std::env::var("MARGINFI_ACCOUNT") {
        if let Ok(pk) = Pubkey::from_str(s.trim()) { hot.insert(pk); }
    }

    // ── MarginFi USDT bank (alternation path) ──────────────────────────
    let mfi_usdt_bank = Pubkey::from_str("HmpMfL8942u22htC4EMiWgLX931g3sacXFR6KjuLgKLV")?;
    let (mfi_usdt_liq_auth, _) = Pubkey::find_program_address(
        &[b"liquidity_vault_auth", mfi_usdt_bank.as_ref()],
        &marginfi_program,
    );
    let (mfi_usdt_liq_vault, _) = Pubkey::find_program_address(
        &[b"liquidity_vault", mfi_usdt_bank.as_ref()],
        &marginfi_program,
    );
    hot.insert(mfi_usdt_bank);
    hot.insert(mfi_usdt_liq_auth);
    hot.insert(mfi_usdt_liq_vault);

    // ── Jupiter intermediate addresses ─────────────────────────────────
    // When Jupiter routes USDC→X→USDC at $5K it touches many program ids,
    // pool addresses, and token mints. The previous ALT only carried our
    // own DEX programs; Jupiter's intermediate pools/programs were absent,
    // making each tx push past the 1232-byte limit → `transaction #0
    // could not be decoded`. This block adds the most-frequent
    // non-pool-specific addresses: DEX programs, well-known LSTs / memes
    // that Jupiter routes through, and the support programs Jupiter swaps
    // touch (Memo, Token-2022, ATA program).
    for addr in [
        // DEX programs Jupiter routes through (in addition to ours already covered)
        "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",   // Jupiter v6 aggregator program
        "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY",   // Phoenix (orderbook)
        "2wT8Yq49kHgDzXuPxZSaeLaH1qbmGXtEyPy64bL7aD3c",   // Lifinity V2
        "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB",   // Meteora DAMM
        "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK",   // Raydium CLMM (verify already)
        "CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C",   // Raydium CPMM
        "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo",    // Meteora DLMM
        "stkitrT1Uoy18Dk1fTrgPw8W6MVzoCfYoAFT4MLsmhq",    // Sanctum stake-pool router
        "SSwpkEEcbUqx4vtoEByFjSkhKdCT862DNVb52nZg1UZ",    // Saber stable-swap
        // Well-known LST mints (Jupiter loves routing through these)
        "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn",   // JitoSOL
        "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",    // mSOL
        "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1",    // bSOL
        "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v",    // jupSOL
        "5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm",   // INF
        // Top memecoins / governance Jupiter routes through
        "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",   // BONK
        "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",   // WIF
        "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",    // JUP
        "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL",    // JTO
        "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3",   // PYTH
        "85VBFQZC9TZkfaptBWjvUw7YbZjy52A6mjtPGjstQAmQ",   // W (Wormhole)
        "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4",   // JLP (Jupiter Perps LP)
        // System programs that Jupiter swap-instructions invoke
        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",    // Token-2022
        "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL",    // Associated Token Program
        "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",    // Memo Program v2
        "ComputeBudget111111111111111111111111111111",    // Compute Budget Program
    ] {
        if let Ok(pk) = Pubkey::from_str(addr) {
            hot.insert(pk);
        }
    }
    // Wallet ATAs for the new tokens so swap_instructions doesn't have to
    // include them as full 32-byte accounts.
    for mint_b58 in [
        "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn",   // JitoSOL
        "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",    // mSOL
        "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1",    // bSOL
        "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",   // BONK
        "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",   // WIF
        "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",    // JUP
        "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL",    // JTO
        "HZ1JovNiVvGrGNiiYvEozEVgZ58xaU3RKwX8eACQBCt3",   // PYTH
    ] {
        if let Ok(mint) = Pubkey::from_str(mint_b58) {
            hot.insert(derive_ata(&payer, &mint));
        }
    }

    // ── 3. Pool addresses + per-pool vaults/oracles ────────────────────────
    println!("Fetching {} pool accounts …", scout_cfg.pools.len());
    let pool_addrs: Vec<Pubkey> = scout_cfg.pools.iter().map(|p| p.address).collect();
    let pool_data = get_multiple_account_data(&rpc_url, &pool_addrs).await?;

    for (entry, data) in scout_cfg.pools.iter().zip(pool_data.iter()) {
        hot.insert(entry.address);
        let Some(raw) = data else {
            eprintln!("  ! pool {} not found on chain — skipping its vaults",
                      entry.address);
            continue;
        };
        match entry.kind {
            PoolKind::RaydiumAmmV4 => {
                if let Some(p) = RaydiumAmmV4Data::try_parse(raw) {
                    hot.insert(p.base_vault);
                    hot.insert(p.quote_vault);
                    hot.insert(p.market_id);
                }
            }
            PoolKind::RaydiumClmm => {
                if let Some(p) = RaydiumClmmData::try_parse(raw) {
                    hot.insert(p.token_vault_0);
                    hot.insert(p.token_vault_1);
                    hot.insert(p.amm_config);
                    hot.insert(p.observation_key);
                    // Tick-arrays around the current active tick. Each tick
                    // array spans `TICK_ARRAY_SIZE * tick_spacing` ticks.
                    // Adding ±2 covers price swings up to ~5*span which is
                    // enough for nearly all swap legs (typical leg moves
                    // 0.1-0.5 spans). Without these the executor's per-swap
                    // tick-array references blow the 1232-byte v0 budget.
                    let span = (CLMM_TICK_ARRAY_SIZE as i32) * p.tick_spacing as i32;
                    for di in [-2_i32, -1, 0, 1, 2] {
                        let ref_tick = p.tick_current.saturating_add(di.saturating_mul(span));
                        let start = clmm_tick_array_start_index(ref_tick, p.tick_spacing);
                        hot.insert(clmm_derive_tick_array_pda(&entry.address, start));
                    }
                }
            }
            PoolKind::OrcaWhirlpool => {
                if let Some(p) = WhirlpoolData::try_parse(raw) {
                    hot.insert(p.token_vault_a);
                    hot.insert(p.token_vault_b);
                    // Whirlpool oracle PDA — derived; deterministic per pool.
                    let oracle = arb_dex_orca::derive_oracle_pda(&entry.address);
                    hot.insert(oracle);
                    // Whirlpool tick-arrays around the current tick.
                    // TICK_ARRAY_SIZE for Whirlpool = 88. Same ±2 strategy.
                    let span = 88_i32 * p.tick_spacing as i32;
                    for di in [-2_i32, -1, 0, 1, 2] {
                        let ref_tick = p.tick_current_index.saturating_add(di.saturating_mul(span));
                        let start = wp_tick_array_start_index(ref_tick, p.tick_spacing);
                        hot.insert(wp_derive_tick_array_pda(&entry.address, start));
                    }
                }
            }
            PoolKind::ObricV2 => {
                if let Some(p) = ObricV2Data::try_parse(raw) {
                    hot.insert(p.reserve_x_vault);
                    hot.insert(p.reserve_y_vault);
                    hot.insert(p.x_price_feed);
                    hot.insert(p.y_price_feed);
                    hot.insert(p.protocol_fee_x);
                    hot.insert(p.protocol_fee_y);
                }
            }
            PoolKind::MeteoraDlmm => {
                if let Some(p) = LbPairData::try_parse(raw) {
                    hot.insert(p.reserve_x);
                    hot.insert(p.reserve_y);
                    hot.insert(p.token_x_mint);
                    hot.insert(p.token_y_mint);
                    // Per-pool oracle PDA — used by every DLMM swap.
                    hot.insert(dlmm_oracle_pda(&entry.address));
                    // Bin arrays straddling the active bin. Same window the
                    // executor will pull at run time (active ±1). Without
                    // these in the ALT the v0 tx still works but each one
                    // costs 31 bytes in `static_account_keys`.
                    let active_idx = bin_array_index_for(p.active_id);
                    for di in [-3_i32, -2, -1, 0, 1, 2, 3] {
                        hot.insert(derive_bin_array_pda(&entry.address, active_idx + di));
                    }
                }
            }
            _ => { /* Lifinity, Phoenix — not native-eligible yet, skip */ }
        }
    }
    // Add DLMM globals once if any DLMM pool is present in the config.
    if scout_cfg.pools.iter().any(|e| matches!(e.kind, PoolKind::MeteoraDlmm)) {
        hot.insert(METEORA_DLMM_PROGRAM);
        hot.insert(dlmm_event_authority());
    }

    // Drop the wallet pubkey itself (signer — must stay static, ALT entry wasted)
    hot.remove(&payer);

    // Dedupe against existing ALTs so we only deploy addresses NOT already
    // covered. Comma-separated list of existing ALT pubkeys via
    // ALT_DEDUPE_AGAINST. Without this, deploying a second/third ALT just
    // duplicates the first's contents (~89% overlap observed in practice).
    if let Ok(deds) = std::env::var("ALT_DEDUPE_AGAINST") {
        let alts: Vec<Pubkey> = deds.split(',')
            .filter_map(|s| Pubkey::from_str(s.trim()).ok())
            .collect();
        let mut existing: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
        for alt_pk in &alts {
            match arb_scout::alt::fetch_alt_addresses(&rpc_url, alt_pk).await {
                Ok(addrs) => {
                    println!("Dedupe vs {alt_pk}: {} addresses", addrs.len());
                    for a in addrs { existing.insert(a); }
                }
                Err(e) => eprintln!("  ! failed to fetch {alt_pk}: {e}"),
            }
        }
        let before = hot.len();
        hot.retain(|a| !existing.contains(a));
        println!("Deduped: {before} → {} ({} dropped)", hot.len(), before - hot.len());
    }

    let addresses: Vec<Pubkey> = hot.into_iter().collect();
    println!();
    println!("Built {} unique hot addresses:", addresses.len());
    for (i, a) in addresses.iter().enumerate() {
        println!("  [{:>3}] {a}", i);
    }
    println!();

    // ── 4. Compute ALT PDA + recent slot ───────────────────────────────────
    // Resume support: if `ALT_RESUME` env var holds an ALT pubkey we already
    // created in a prior partial run, skip CreateLookupTable and continue
    // extending. `ALT_SKIP_BATCHES` says how many leading batches to drop —
    // i.e. how many ExtendLookupTable txs landed last time. This lets us
    // recover from a mid-run Custom(1) failure without burning fresh rent
    // on a brand-new ALT.
    let resume_alt = std::env::var("ALT_RESUME").ok()
        .and_then(|s| Pubkey::from_str(s.trim()).ok());
    let skip_batches: usize = std::env::var("ALT_SKIP_BATCHES").ok()
        .and_then(|s| s.parse().ok()).unwrap_or(0);

    let alt_pubkey = if let Some(existing) = resume_alt {
        println!("Resuming existing ALT {existing} (skip {skip_batches} batches)");
        println!();
        existing
    } else {
        let recent_slot = get_slot(&rpc_url).await? - 1;  // -1: must be already-finalized
        let (alt_pubkey, bump_seed) = Pubkey::find_program_address(
            &[payer.as_ref(), &recent_slot.to_le_bytes()],
            &ADDRESS_LOOKUP_TABLE_PROGRAM,
        );
        println!("ALT pubkey will be: {alt_pubkey}");
        println!("  derived from authority={payer} recent_slot={recent_slot} bump={bump_seed}");
        println!();

        // ── 5. Send Create tx ──────────────────────────────────────────────
        println!("Sending CreateLookupTable …");
        let create_ix = build_create_lookup_table_ix(
            alt_pubkey, payer, recent_slot, bump_seed,
        );
        let bh = fetch_latest_blockhash(&rpc_url).await?;
        let create_ixs = vec![
            set_compute_unit_limit_ix(50_000),
            set_compute_unit_price_ix(10_000),
            create_ix,
        ];
        let create_b64 = build_and_sign_legacy_tx_b64(
            &payer, &create_ixs, &bh, wallet.bytes(),
        ).map_err(|e| anyhow!("sign create tx: {e:?}"))?;
        let create_sig = send_tx_and_confirm(&rpc_url, &create_b64).await?;
        println!("  → confirmed sig {}", create_sig);
        println!();
        alt_pubkey
    };

    // ── 6. Send Extend txs in batches ──────────────────────────────────────
    for (batch_idx, chunk) in addresses.chunks(ADDRESSES_PER_EXTEND_TX).enumerate() {
        if batch_idx < skip_batches {
            println!("Skipping batch {} (already landed)", batch_idx + 1);
            continue;
        }
        println!("Sending ExtendLookupTable batch {} ({} addresses) …",
                 batch_idx + 1, chunk.len());
        let extend_ix = build_extend_lookup_table_ix(alt_pubkey, payer, chunk);
        let mut last_err: Option<anyhow::Error> = None;
        let mut landed = false;
        for attempt in 1..=4u32 {
            let bh = fetch_latest_blockhash(&rpc_url).await?;
            let extend_ixs = vec![
                set_compute_unit_limit_ix(50_000),
                set_compute_unit_price_ix(10_000),
                extend_ix.clone(),
            ];
            let extend_b64 = build_and_sign_legacy_tx_b64(
                &payer, &extend_ixs, &bh, wallet.bytes(),
            ).map_err(|e| anyhow!("sign extend tx: {e:?}"))?;
            match send_tx_and_confirm(&rpc_url, &extend_b64).await {
                Ok(sig) => {
                    println!("  → confirmed sig {}", sig);
                    landed = true;
                    break;
                }
                Err(e) => {
                    eprintln!("  ! attempt {attempt} failed: {e}");
                    last_err = Some(e);
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
        if !landed {
            return Err(last_err.unwrap_or_else(|| anyhow!("extend batch never landed")));
        }
    }

    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("ALT deployed: {alt_pubkey}");
    println!("Add to Railway / .env:");
    println!();
    println!("EXECUTOR_NATIVE_ALT={alt_pubkey}");
    println!("══════════════════════════════════════════════════════════════════");
    Ok(())
}

// ─── ALT instruction encoders ────────────────────────────────────────────

/// `CreateLookupTable { recent_slot: u64, bump_seed: u8 }`
/// Bincode encoding: u32 LE variant index (0) + u64 LE recent_slot + u8 bump.
fn build_create_lookup_table_ix(
    alt: Pubkey, authority_and_payer: Pubkey, recent_slot: u64, bump: u8,
) -> Instruction {
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&0u32.to_le_bytes());  // variant 0
    data.extend_from_slice(&recent_slot.to_le_bytes());
    data.push(bump);
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt, false),
            AccountMeta::new_readonly(authority_and_payer, true),
            AccountMeta::new(authority_and_payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data,
    }
}

/// `ExtendLookupTable { new_addresses: Vec<Pubkey> }`
/// Bincode: u32 LE variant (2) + u64 LE vec length + N×32 bytes.
fn build_extend_lookup_table_ix(
    alt: Pubkey, authority_and_payer: Pubkey, addrs: &[Pubkey],
) -> Instruction {
    let mut data = Vec::with_capacity(12 + addrs.len() * 32);
    data.extend_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&(addrs.len() as u64).to_le_bytes());
    for a in addrs { data.extend_from_slice(a.as_ref()); }
    Instruction {
        program_id: ADDRESS_LOOKUP_TABLE_PROGRAM,
        accounts: vec![
            AccountMeta::new(alt, false),
            AccountMeta::new_readonly(authority_and_payer, true),
            AccountMeta::new(authority_and_payer, true),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
        ],
        data,
    }
}

// ─── RPC helpers ─────────────────────────────────────────────────────────

async fn get_slot(rpc: &str) -> anyhow::Result<u64> {
    let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"getSlot"});
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.json().await?;
    r.pointer("/result").and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("getSlot bad response: {r}"))
}

async fn get_multiple_account_data(
    rpc: &str, addrs: &[Pubkey],
) -> anyhow::Result<Vec<Option<Vec<u8>>>> {
    let pks: Vec<String> = addrs.iter().map(|p| p.to_string()).collect();
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getMultipleAccounts",
        "params": [pks, {"encoding": "base64", "commitment": "confirmed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.json().await?;
    let arr = r.pointer("/result/value").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("getMultipleAccounts bad response: {r}"))?;
    Ok(arr.iter().map(|a| {
        if a.is_null() { return None; }
        let b64 = a.pointer("/data/0").and_then(|v| v.as_str())?;
        base64::engine::general_purpose::STANDARD.decode(b64).ok()
    }).collect())
}

async fn send_tx(rpc: &str, b64: &str) -> anyhow::Result<String> {
    // skipPreflight=true: Helius sometimes returns BlockhashNotFound from
    // preflight simulation even when the blockhash IS valid (the simulating
    // node lags the fetching node). The actual cluster execution catches
    // any real error and getSignatureStatuses surfaces it to us.
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "sendTransaction",
        "params": [b64, {"encoding": "base64", "skipPreflight": true,
                         "preflightCommitment": "processed"}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc).json(&body).send().await?.json().await?;
    if let Some(err) = r.pointer("/error") {
        return Err(anyhow!("sendTransaction RPC error: {err}"));
    }
    r.pointer("/result").and_then(|v| v.as_str()).map(String::from)
        .ok_or_else(|| anyhow!("sendTransaction missing result: {r}"))
}

async fn confirm_signature(rpc: &str, sig: &str, timeout_secs: u64)
    -> anyhow::Result<()>
{
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    while std::time::Instant::now() < deadline {
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "getSignatureStatuses",
            "params": [[sig], {"searchTransactionHistory": false}],
        });
        let r: serde_json::Value = reqwest::Client::new()
            .post(rpc).json(&body).send().await?.json().await?;
        let st = r.pointer("/result/value/0");
        if let Some(s) = st {
            if !s.is_null() {
                if let Some(err) = s.pointer("/err") {
                    if !err.is_null() {
                        return Err(anyhow!("tx {sig} failed: {err}"));
                    }
                }
                let conf = s.pointer("/confirmationStatus").and_then(|v| v.as_str()).unwrap_or("");
                if conf == "confirmed" || conf == "finalized" { return Ok(()); }
            }
        }
        tokio::time::sleep(Duration::from_millis(2000)).await;
    }
    Err(anyhow!("tx {sig} not confirmed within {timeout_secs}s"))
}

async fn send_tx_and_confirm(rpc: &str, b64: &str) -> anyhow::Result<String> {
    let sig = send_tx(rpc, b64).await?;
    confirm_signature(rpc, &sig, 60).await?;
    Ok(sig)
}
