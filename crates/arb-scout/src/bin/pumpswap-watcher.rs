//! PumpSwap graduation watcher.
//!
//! Subscribes via Helius LaserStream to every account owned by the
//! PumpSwap AMM program with `data_size == 301` and `memcmp[0..8] ==
//! pool_discriminator`. The combination matches a freshly-written Pool
//! account → typically the migration tx that runs when a pump.fun
//! bonding-curve token "graduates" to PumpSwap.
//!
//! Hot path on graduation:
//!   1. Parse pool (base_mint, quote_mint, vaults).
//!   2. Lazy-query the chain for OTHER pools holding the same base_mint
//!      on Raydium AMM v4, Orca Whirlpool, Meteora DLMM.
//!   3. If another pool exists → log `ARB CANDIDATE` with spread estimate.
//!
//! Phase-1 scope (this binary): detect + log. No tx fire yet — the
//! Jupiter+MarginFi-flash leg goes in Phase 2.
//!
//! ## Layout (verified live 2026-05-17 from pool FRtvYqasUXFvxwg8pKW5reV1GoGomv4Yidc3g1FanYTC)
//!
//! ```text
//! offset  size  field
//! 0       8     anchor discriminator   = f19a6d0411b16dbc
//! 8       1     pool_bump (u8)
//! 9       2     index    (u16 LE)
//! 11      32    creator               (Pubkey)
//! 43      32    base_mint             (Pubkey)  ← graduated token
//! 75      32    quote_mint            (Pubkey)  ← typically SOL (So111…)
//! 107     32    lp_mint               (Pubkey)
//! 139     32    pool_base_token_account  (Pubkey)
//! 171     32    pool_quote_token_account (Pubkey)
//! 203     8     lp_supply (u64 LE)
//! 211     32    coin_creator (Pubkey)
//! ```
//!
//! ## Env
//!
//! Required: HELIUS_API_KEY, LASERSTREAM_GRPC_URL.
//! Optional: PUMPSWAP_MIN_QUOTE_LAMPORTS (default 1e9 = 1 SOL, skip tiny
//!   pools that aren't worth arbing), PUMPSWAP_LOG_EVERY (default 1 =
//!   log every detection).

use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

/// Process-local dead-letter cache: mints whose first sim FAILED. We
/// skip them for the rest of the process lifetime — the failure modes
/// we've seen (Token-2022 ATA program mismatch, etc.) don't self-heal
/// from one block to the next. Restart of the watcher resets the set,
/// which is the right behaviour when we deploy a fix.
static FAILED_MINTS: StdMutex<Option<HashSet<Pubkey>>> = StdMutex::new(None);

fn mint_is_dead_lettered(mint: &Pubkey) -> bool {
    let g = FAILED_MINTS.lock().ok();
    g.and_then(|guard| guard.as_ref().map(|s| s.contains(mint))).unwrap_or(false)
}

fn dead_letter_mint(mint: Pubkey) {
    if let Ok(mut guard) = FAILED_MINTS.lock() {
        guard.get_or_insert_with(HashSet::new).insert(mint);
    }
}

/// Process-local cache: mint → token_program owner. Mints don't change
/// their owner program after creation, so a one-shot RPC per mint is
/// sufficient for the bot's lifetime.
static MINT_OWNERS: StdMutex<Option<std::collections::HashMap<Pubkey, Pubkey>>> = StdMutex::new(None);

async fn mint_token_program(rpc_url: &str, mint: &Pubkey) -> Option<Pubkey> {
    if let Ok(guard) = MINT_OWNERS.lock() {
        if let Some(map) = guard.as_ref() {
            if let Some(p) = map.get(mint) { return Some(*p); }
        }
    }
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
        "params":[mint.to_string(),{"encoding":"base64","commitment":"processed"}],
    });
    let r: serde_json::Value = match reqwest::Client::new()
        .post(rpc_url).json(&body).send().await {
        Ok(resp) => match resp.error_for_status() {
            Ok(resp) => match resp.json().await { Ok(v) => v, Err(_) => return None },
            Err(_) => return None,
        },
        Err(_) => return None,
    };
    let owner_str = r.pointer("/result/value/owner").and_then(|v| v.as_str())?;
    let owner: Pubkey = owner_str.parse().ok()?;
    if let Ok(mut guard) = MINT_OWNERS.lock() {
        guard.get_or_insert_with(std::collections::HashMap::new).insert(*mint, owner);
    }
    Some(owner)
}

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;
use tokio::sync::mpsc;

use arb_scout::alt::fetch_alt_addresses;
use arb_scout::raw_tx::{
    create_ata_idempotent_ix, derive_ata, fetch_latest_blockhash,
    set_compute_unit_limit_ix, set_compute_unit_price_ix, system_transfer_ix,
};
use arb_scout::tpu_send::{fetch_current_slot, TpuSender};
use arb_scout::v0_tx::{build_v0_message_multi, sign_v0_tx_b64};
use arb_scout::wallet::Wallet;

// Token program ids — for Token-2022 detection. Pump.fun emits both
// legacy (TokenkegQ...) and Token-2022 (TokenzQd...) mints since 2024;
// Jupiter's swap-instructions setup_ix returns the wrong token_program
// account for Token-2022 mints. V2.5 dead-lettered them; V3 rewrites
// the ATA-create's account[5] to point at the correct program.
const TOKEN_LEGACY_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022_PROGRAM:   Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
const ATA_PROGRAM:          Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// V3: Walk `ixs` in-place. For every Associated-Token-Account program
/// instruction (Create / CreateIdempotent) where the mint at account[3]
/// is `target_mint`, replace account[5] (`token_program`) with
/// `correct_token_program` if it doesn't already match.
///
/// Rationale: Jupiter's `/swap-instructions` returns ATA-create ixs
/// hardcoded to the LEGACY SPL Token program even when the mint is
/// Token-2022. The ATA program then CPIs to the legacy SPL Token
/// program, which doesn't recognise the mint → IX reverts with
/// Custom:6024 IncorrectProgramId. Rewriting the account is a 1-byte
/// surgical fix and unblocks ~30-50% of pump.fun graduations.
///
/// Returns the number of accounts rewritten (mostly for telemetry).
fn rewrite_ata_token_program(
    ixs:                   &mut [Instruction],
    target_mint:           &Pubkey,
    correct_token_program: &Pubkey,
) -> usize {
    let mut rewritten = 0;
    for ix in ixs.iter_mut() {
        if ix.program_id != ATA_PROGRAM { continue; }
        // ATA program ix data: 0 byte = Create (legacy), 1 byte = CreateIdempotent.
        // Both have the same account layout: [payer, ata, owner, mint, system, token_program].
        let disc = ix.data.first().copied();
        if !matches!(disc, Some(0) | Some(1)) { continue; }
        if ix.accounts.len() < 6 { continue; }
        if ix.accounts[3].pubkey != *target_mint { continue; }
        if ix.accounts[5].pubkey == *correct_token_program { continue; }
        ix.accounts[5].pubkey = *correct_token_program;
        rewritten += 1;
    }
    rewritten
}

const PUMPSWAP_PROGRAM: Pubkey =
    pubkey!("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
const POOL_DISCRIMINATOR: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];
const POOL_DATA_SIZE: u64 = 301;
const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
const JITO_TIP_ACCOUNT: Pubkey =
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5");

// Programs to cross-reference for "same token on another DEX" arb candidates.
const RAYDIUM_AMM_V4:    Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
const ORCA_WHIRLPOOL:    Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
const METEORA_DLMM:      Pubkey = pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
const RAYDIUM_CLMM:      Pubkey = pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

#[derive(Debug, Clone, Copy)]
pub struct PumpSwapPool {
    pub address:           Pubkey,
    pub pool_bump:         u8,
    pub index:             u16,
    pub creator:           Pubkey,
    pub base_mint:         Pubkey,
    pub quote_mint:        Pubkey,
    pub lp_mint:           Pubkey,
    pub pool_base_vault:   Pubkey,
    pub pool_quote_vault:  Pubkey,
    pub lp_supply:         u64,
    pub coin_creator:      Pubkey,
}

/// Returns Some when bytes match the Pool layout — discriminator + size.
/// Returns None for any other account that happens to live under the
/// program (LpMint Authority PDA, Fee PDA, etc.). Caller should silently
/// drop None.
pub fn parse_pumpswap_pool(address: Pubkey, data: &[u8]) -> Option<PumpSwapPool> {
    if data.len() != POOL_DATA_SIZE as usize { return None; }
    if data[0..8] != POOL_DISCRIMINATOR { return None; }
    let pk = |o: usize| -> Pubkey {
        let arr: [u8; 32] = data[o..o + 32].try_into().ok().unwrap_or([0u8; 32]);
        Pubkey::new_from_array(arr)
    };
    let u16le = |o: usize| -> u16 { u16::from_le_bytes(data[o..o + 2].try_into().ok().unwrap_or([0; 2])) };
    let u64le = |o: usize| -> u64 { u64::from_le_bytes(data[o..o + 8].try_into().ok().unwrap_or([0; 8])) };
    Some(PumpSwapPool {
        address,
        pool_bump:        data[8],
        index:            u16le(9),
        creator:          pk(11),
        base_mint:        pk(43),
        quote_mint:       pk(75),
        lp_mint:          pk(107),
        pool_base_vault:  pk(139),
        pool_quote_vault: pk(171),
        lp_supply:        u64le(203),
        coin_creator:     pk(211),
    })
}

#[derive(Debug, Default, Clone)]
struct Stats {
    pool_updates_seen:   u64,
    new_pools_detected:  u64,
    skipped_low_quote:   u64,
    skipped_stale:       u64,  // pool's oldest sig > PUMPSWAP_FRESH_MAX_AGE_SECS
    skipped_no_sol_side: u64,  // neither side of pool is SOL
    arb_candidates:      u64,
}

#[cfg(feature = "laserstream")]
async fn subscriber_loop(
    grpc_url: String,
    api_key:  String,
    tx:       mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    use arb_stream::wss::SessionEnd;
    loop {
        println!(
            "→ laserstream: connecting (owner={}, dataSize={}, memcmp@0={:02x?})",
            PUMPSWAP_PROGRAM, POOL_DATA_SIZE, POOL_DISCRIMINATOR,
        );
        let end = arb_stream::laserstream::run_one_session_by_owner(
            &grpc_url,
            &api_key,
            &PUMPSWAP_PROGRAM,
            Some(POOL_DATA_SIZE),
            Some((0, POOL_DISCRIMINATOR.to_vec())),
            &tx,
        ).await;
        match end {
            SessionEnd::Closed     => eprintln!("laserstream closed cleanly — reconnecting"),
            SessionEnd::Errored(e) => eprintln!("laserstream errored: {e:?} — reconnect in 2s"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

#[cfg(not(feature = "laserstream"))]
async fn subscriber_loop(
    _grpc_url: String,
    _api_key:  String,
    _tx:       mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    eprintln!("laserstream feature disabled — pumpswap-watcher requires --features laserstream");
}

/// Poll `/tmp/shred-pumpswap-create-alert`. When mtime advances, do a
/// targeted `getProgramAccounts(PumpSwap, dataSize=301, memcmp@0=disc)`
/// to find every current pool. Push any address we haven't seen before
/// as a synthetic `AccountUpdate`, which lands ~50-100 ms ahead of the
/// equivalent LaserStream event.
async fn shred_pumpswap_poller(
    rpc_url: String,
    tx: mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    use base64::Engine;
    use std::time::{Duration, SystemTime};
    let path = "/tmp/shred-pumpswap-create-alert";
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_mtime = SystemTime::UNIX_EPOCH;
    let mut alerts: u64 = 0;
    let mut new_pools: u64 = 0;
    let mut seen: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    let start = std::time::Instant::now();
    // First-alert warmup: getProgramAccounts returns ALL existing pools.
    // Pushing those as "new" would flood downstream with thousands of
    // stale pools. So the first response only populates `seen`; only the
    // GENUINELY new pools (creates that arrive after warmup) get pushed.
    let mut warmup_done = false;

    loop {
        tick.tick().await;
        let meta = match tokio::fs::metadata(path).await {
            Ok(m) => m, Err(_) => continue,
        };
        let mtime = match meta.modified() { Ok(t) => t, Err(_) => continue };
        if mtime <= last_mtime { continue; }
        last_mtime = mtime;
        let content = tokio::fs::read_to_string(path).await.unwrap_or_default();
        let slot: u64 = content.split_whitespace().next()
            .and_then(|s| s.parse().ok()).unwrap_or(0);
        if slot == 0 { continue; }
        alerts += 1;

        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
            "params":[PUMPSWAP_PROGRAM.to_string(), {
                "encoding":"base64",
                "commitment":"processed",
                "filters":[
                    {"dataSize": POOL_DATA_SIZE},
                    {"memcmp": {"offset": 0, "bytes": bs58::encode(POOL_DISCRIMINATOR).into_string()}}
                ],
            }],
        });
        let resp = match client.post(&rpc_url).json(&body).send().await {
            Ok(r) => r, Err(_) => continue,
        };
        let json: serde_json::Value = match resp.json().await { Ok(j) => j, Err(_) => continue };
        let vals = match json.pointer("/result").and_then(|v| v.as_array()) {
            Some(v) => v, None => continue,
        };
        for val in vals {
            let pk_str = val.pointer("/pubkey").and_then(|v| v.as_str()).unwrap_or("");
            let pk: Pubkey = match pk_str.parse() { Ok(p) => p, Err(_) => continue };
            if !seen.insert(pk) { continue; }
            if !warmup_done { continue; }  // first batch is warmup, don't push
            let data_b64 = val.pointer("/account/data").and_then(|v| v.as_array())
                .and_then(|a| a.first()).and_then(|s| s.as_str()).unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b, Err(_) => continue,
            };
            let upd = arb_stream::wss::AccountUpdate {
                account: pk, slot, data: bytes, owner: PUMPSWAP_PROGRAM,
            };
            if tx.send(upd).await.is_err() { return; }
            new_pools += 1;
        }
        if !warmup_done {
            eprintln!("  📡 shred-pumpswap warmup done: {} pools registered, only NEW pools push after this", seen.len());
            warmup_done = true;
        }
        if alerts % 5 == 0 {
            let el = start.elapsed().as_secs_f64();
            eprintln!("  📡 shred-pumpswap stats: {alerts} alerts ({:.2}/s), {new_pools} new pools pushed", alerts as f64 / el);
        }
    }
}

/// Native Rust shred subscriber for PumpSwap CreatePool events. Same
/// outcome as `shred_pumpswap_poller` but driven by an in-process gRPC
/// stream rather than a Python tee + tmpfile.
#[cfg(feature = "shredstream")]
async fn shred_pumpswap_native(
    endpoint: String,
    rpc_url: String,
    tx: mpsc::Sender<arb_stream::wss::AccountUpdate>,
) {
    use base64::Engine;
    use std::time::Duration;
    let mut shred_rx = arb_stream::shred::spawn_shred_subscriber(
        endpoint,
        arb_stream::shred::default_filters(),
        512,
    );
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    let mut seen: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    let mut alerts: u64 = 0;
    let mut new_pools: u64 = 0;
    let mut warmup_done = false;
    let start = std::time::Instant::now();
    eprintln!("  📡 native-shred[pumpswap-create]: subscribed via Rust client");

    while let Some(ev) = shred_rx.recv().await {
        if ev.label != "pumpswap-create" { continue; }
        alerts += 1;
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
            "params":[PUMPSWAP_PROGRAM.to_string(), {
                "encoding":"base64",
                "commitment":"processed",
                "filters":[
                    {"dataSize": POOL_DATA_SIZE},
                    {"memcmp": {"offset": 0, "bytes": bs58::encode(POOL_DISCRIMINATOR).into_string()}}
                ],
            }],
        });
        let resp = match client.post(&rpc_url).json(&body).send().await {
            Ok(r) => r, Err(_) => continue,
        };
        let json: serde_json::Value = match resp.json().await { Ok(j) => j, Err(_) => continue };
        let vals = match json.pointer("/result").and_then(|v| v.as_array()) {
            Some(v) => v, None => continue,
        };
        for val in vals {
            let pk_str = val.pointer("/pubkey").and_then(|v| v.as_str()).unwrap_or("");
            let pk: Pubkey = match pk_str.parse() { Ok(p) => p, Err(_) => continue };
            if !seen.insert(pk) { continue; }
            let data_b64 = val.pointer("/account/data").and_then(|v| v.as_array())
                .and_then(|a| a.first()).and_then(|s| s.as_str()).unwrap_or("");
            let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
                Ok(b) => b, Err(_) => continue,
            };
            let upd = arb_stream::wss::AccountUpdate {
                account: pk, slot: ev.slot, data: bytes, owner: PUMPSWAP_PROGRAM,
            };
            if tx.send(upd).await.is_err() { return; }
            new_pools += 1;
        }
        if alerts % 5 == 0 {
            let el = start.elapsed().as_secs_f64();
            eprintln!("  📡 native-shred[pumpswap]: {alerts} alerts ({:.2}/s), {new_pools} pools pushed", alerts as f64 / el);
        }
    }
}

/// Return the unix-second age of the OLDEST signature for `address`. Used
/// as a proxy for "did this account just get created?" — PumpSwap rewrites
/// pool accounts on every swap, so "first observation" on LaserStream
/// catches pools that have existed for hours/days as well as fresh
/// graduations. Filtering on `oldest sig < N seconds` separates the
/// graduations (interesting) from the routine swap activity (noise).
///
/// Helius's `getSignaturesForAddress` returns signatures newest-first,
/// so we walk to the END of the available history (up to 1000 sigs) to
/// find the OLDEST timestamp. For freshly graduated pools this is cheap
/// — the pool only has 1-5 sigs in its lifetime so the first page covers
/// it. For long-lived pools the function still returns within one
/// round-trip but the timestamp is days old.
async fn oldest_sig_age_seconds(rpc_url: &str, address: &Pubkey) -> anyhow::Result<i64> {
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getSignaturesForAddress",
        "params":[address.to_string(), {"limit": 1000}],
    });
    let r: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await?
        .error_for_status()?
        .json().await?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("getSignaturesForAddress no result: {r}"))?;
    // Walk from the end backward looking for any non-null blockTime.
    let oldest_ts: i64 = arr.iter().rev()
        .filter_map(|s| s.get("blockTime").and_then(|v| v.as_i64()))
        .next()
        .unwrap_or(0);
    if oldest_ts == 0 { return Ok(i64::MAX); }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
    Ok(now.saturating_sub(oldest_ts))
}

/// Per-program (memcmp_offset, label) pairs for "find pool by base_mint".
/// Each AMM stores its base/quote mints at a fixed offset; we probe BOTH
/// directions per program so a target token can be on either side.
fn cross_dex_search_specs() -> &'static [(Pubkey, &'static str, u64, u64)] {
    // (program, name, base_mint_offset, quote_mint_offset)
    //
    // Offsets from public IDLs:
    //   Raydium AMM v4  — base_mint @ 400, quote_mint @ 432
    //   Orca Whirlpool  — token_mint_a @ 101, token_mint_b @ 133
    //   Raydium CLMM    — token_mint_0 @ 73,  token_mint_1 @ 105
    //   Meteora DLMM    — token_x_mint @ 88,  token_y_mint @ 120
    &[
        (RAYDIUM_AMM_V4, "Raydium AMM v4", 400, 432),
        (ORCA_WHIRLPOOL, "Orca Whirlpool", 101, 133),
        (RAYDIUM_CLMM,   "Raydium CLMM",    73, 105),
        (METEORA_DLMM,   "Meteora DLMM",    88, 120),
    ]
}

/// Lazy probe: query the chain for at least one OTHER pool holding
/// `target_mint` on a supported AMM. Tries each (program, offset) pair
/// — the offset is "where does this AMM store one of its two pool
/// mints". Returns on FIRST hit so we minimise RPC traffic.
async fn first_cross_dex_pool(
    rpc_url:     &str,
    target_mint: &Pubkey,
) -> anyhow::Result<Option<(Pubkey, &'static str, Pubkey)>> {
    let client = reqwest::Client::new();
    for (program, name, off_a, off_b) in cross_dex_search_specs() {
        // Try both "target is base_mint" and "target is quote_mint" — we
        // don't know which side the new pool put it on.
        for off in [*off_a, *off_b] {
            let body = serde_json::json!({
                "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
                "params":[program.to_string(), {
                    "encoding": "base64",
                    "commitment": "processed",
                    "dataSlice": {"offset": 0, "length": 0},
                    "filters": [
                        {"memcmp": {"offset": off, "bytes": target_mint.to_string()}}
                    ],
                }],
            });
            let r: serde_json::Value = match client.post(rpc_url).json(&body).send().await {
                Ok(resp) => match resp.error_for_status() {
                    Ok(resp) => match resp.json().await {
                        Ok(v) => v,
                        Err(_) => continue,
                    },
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            let Some(arr) = r.pointer("/result").and_then(|v| v.as_array()) else { continue };
            if let Some(first) = arr.first() {
                if let Some(pk_str) = first.get("pubkey").and_then(|v| v.as_str()) {
                    if let Ok(pk) = pk_str.parse::<Pubkey>() {
                        return Ok(Some((*program, name, pk)));
                    }
                }
            }
        }
    }
    Ok(None)
}

// ─── Jupiter helpers (parameterised dex filter — bypasses
//     JupiterSwapClient which only reads env vars) ─────────────────

#[derive(Debug, Clone)]
struct JupQuote {
    in_mint:     Pubkey,
    out_mint:    Pubkey,
    in_amount:   u64,
    out_amount:  u64,
    /// Raw JSON Jupiter returned — required by `/swap-instructions` as
    /// the `quoteResponse` body field.
    raw:         serde_json::Value,
}

async fn jup_quote(
    base:          &str,           // e.g. https://api.jup.ag/swap/v1
    in_mint:       &Pubkey,
    out_mint:      &Pubkey,
    in_amount:     u64,
    slippage_bps:  u32,
    dexes:         Option<&str>,   // include-only filter (Jupiter `dexes`)
    exclude_dexes: Option<&str>,   // exclude filter (Jupiter `excludeDexes`)
    max_accounts:  Option<u32>,
) -> anyhow::Result<JupQuote> {
    let mut url = format!(
        "{base}/quote?inputMint={in_mint}&outputMint={out_mint}\
         &amount={in_amount}&slippageBps={slippage_bps}\
         &restrictIntermediateTokens=false&onlyDirectRoutes=true&swapMode=ExactIn",
    );
    if let Some(d) = dexes {
        let encoded = d.replace(' ', "%20").replace(',', "%2C");
        url.push_str(&format!("&dexes={encoded}"));
    }
    if let Some(d) = exclude_dexes {
        let encoded = d.replace(' ', "%20").replace(',', "%2C");
        url.push_str(&format!("&excludeDexes={encoded}"));
    }
    if let Some(n) = max_accounts {
        url.push_str(&format!("&maxAccounts={n}"));
    }
    let api_key = std::env::var("JUPITER_API_KEY").ok();
    let mut req = reqwest::Client::new().get(&url);
    if let Some(k) = api_key.as_deref() {
        req = req.header("x-api-key", k);
    }
    let raw: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
    if let Some(err) = raw.get("error") {
        return Err(anyhow!("jupiter quote error: {err}"));
    }
    let out_amount: u64 = raw.get("outAmount")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("jupiter quote: missing outAmount in {raw}"))?;
    Ok(JupQuote {
        in_mint:    *in_mint,
        out_mint:   *out_mint,
        in_amount,
        out_amount,
        raw,
    })
}

#[derive(Debug, Clone)]
struct JupSwapIxs {
    setup_ixs:       Vec<Instruction>,
    swap_ix:         Instruction,
    cleanup_ix:      Option<Instruction>,
    address_lookup_tables: Vec<Pubkey>,
}

async fn jup_swap_ixs(
    base:           &str,
    quote:          &JupQuote,
    payer:          &Pubkey,
    wrap_unwrap_sol: bool,
) -> anyhow::Result<JupSwapIxs> {
    let body = serde_json::json!({
        "quoteResponse":      quote.raw,
        "userPublicKey":      payer.to_string(),
        "wrapAndUnwrapSol":   wrap_unwrap_sol,
        // Token-2022 mints (many pump.fun graduations) revert in legacy-mode
        // ATA setup with "incorrect program id" — the setup_ix passes the
        // wrong token_program. v0 mode + non-shared accounts gives Jupiter
        // the room to pick the correct program per mint.
        "asLegacyTransaction": false,
        "useSharedAccounts":  false,
    });
    let url = format!("{base}/swap-instructions");
    let api_key = std::env::var("JUPITER_API_KEY").ok();
    let mut req = reqwest::Client::new().post(&url).json(&body);
    if let Some(k) = api_key.as_deref() {
        req = req.header("x-api-key", k);
    }
    let resp: serde_json::Value = req.send().await?.error_for_status()?.json().await?;
    if let Some(err) = resp.get("error") {
        return Err(anyhow!("jupiter swap-ixs error: {err}"));
    }
    let parse_ix = |v: &serde_json::Value| -> anyhow::Result<Instruction> {
        let program_id: Pubkey = v.get("programId").and_then(|x| x.as_str())
            .ok_or_else(|| anyhow!("ix missing programId"))?
            .parse()?;
        let accounts_arr = v.get("accounts").and_then(|a| a.as_array())
            .ok_or_else(|| anyhow!("ix missing accounts"))?;
        let mut accounts: Vec<AccountMeta> = Vec::with_capacity(accounts_arr.len());
        for a in accounts_arr {
            let pk: Pubkey = a.get("pubkey").and_then(|p| p.as_str())
                .ok_or_else(|| anyhow!("account missing pubkey"))?.parse()?;
            accounts.push(AccountMeta {
                pubkey: pk,
                is_signer:   a.get("isSigner").and_then(|s| s.as_bool()).unwrap_or(false),
                is_writable: a.get("isWritable").and_then(|s| s.as_bool()).unwrap_or(false),
            });
        }
        let data_str = v.get("data").and_then(|d| d.as_str())
            .ok_or_else(|| anyhow!("ix missing data"))?;
        let data = base64::engine::general_purpose::STANDARD.decode(data_str)
            .map_err(|e| anyhow!("ix data b64: {e}"))?;
        Ok(Instruction { program_id, accounts, data })
    };
    let setup_ixs: Vec<Instruction> = resp.get("setupInstructions")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(parse_ix).collect::<anyhow::Result<Vec<_>>>())
        .transpose()?.unwrap_or_default();
    let swap_ix = resp.get("swapInstruction")
        .ok_or_else(|| anyhow!("missing swapInstruction"))
        .and_then(parse_ix)?;
    let cleanup_ix = resp.get("cleanupInstruction")
        .filter(|v| !v.is_null())
        .map(parse_ix).transpose()?;
    let address_lookup_tables: Vec<Pubkey> = resp.get("addressLookupTableAddresses")
        .and_then(|v| v.as_array()).cloned().unwrap_or_default()
        .into_iter()
        .filter_map(|v| v.as_str().and_then(|s| s.parse::<Pubkey>().ok()))
        .collect();
    Ok(JupSwapIxs { setup_ixs, swap_ix, cleanup_ix, address_lookup_tables })
}

// ─── Arb attempt — SOL → token via PumpSwap → SOL via other DEX ──

#[derive(Debug, Clone, Copy)]
struct ArbResultLog {
    sent_sig:        Option<[u8; 0]>,  // placeholder for type consistency
    skip_reason:     u8,               // 0=sent, 1=no_route, 2=neg_net, 3=sim_fail
}

#[allow(clippy::too_many_arguments)]
async fn attempt_pumpswap_arb(
    rpc_url:             String,
    jup_base:            String,
    wallet:              Arc<Wallet>,
    target_mint:         Pubkey,
    pool_addr:           Pubkey,
    probe_lamports:      u64,
    min_profit_lamports: i64,
    jito_tip_lamports:   u64,
    autofire:            bool,
    tpu_sender:          Option<Arc<TpuSender>>,
    tpu_fanout:          usize,
) -> anyhow::Result<()> {
    let payer = wallet.pubkey();
    let sol = WSOL_MINT;

    // Dead-letter cache: skip mints whose first sim already failed.
    // Token-2022 ATA program mismatch and similar Jupiter-vs-mint issues
    // don't recover, so re-attempting just wastes Jupiter quote budget.
    if mint_is_dead_lettered(&target_mint) {
        return Ok(());
    }

    // V3: detect mint.owner once and cache. We'll pass it to
    // rewrite_ata_token_program() AFTER fetching Jupiter swap-ixs so
    // any ATA-create instruction gets the correct token_program account
    // before we sign. Default to legacy SPL Token if probe fails — most
    // mints are legacy and the rewriter is a no-op when the program
    // already matches.
    let target_token_program: Pubkey =
        mint_token_program(&rpc_url, &target_mint).await
            .unwrap_or(TOKEN_LEGACY_PROGRAM);

    // Slippage tolerance for Jupiter quotes. Default 500bps (5%) — pump.fun
    // graduations are volatile and 200bps cost us our first live fire
    // (Custom:6001 SlippageToleranceExceeded between sim and on-chain
    // landing). Tunable via PUMPSWAP_SLIPPAGE_BPS env.
    let slippage_bps: u32 = std::env::var("PUMPSWAP_SLIPPAGE_BPS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(500);

    // Try BOTH directions, pick the profitable one (if any). The arb is
    // PumpSwap-vs-other-DEX in either direction; we don't know upfront
    // which side has the better price.
    //
    //   FORWARD = buy on PumpSwap, sell elsewhere
    //     leg-A: SOL → token via Pump.fun Amm only
    //     leg-B: token → SOL via everything EXCEPT Pump.fun
    //
    //   REVERSE = buy elsewhere, sell on PumpSwap
    //     leg-A: SOL → token via everything EXCEPT Pump.fun
    //     leg-B: token → SOL via Pump.fun Amm only
    //
    // Concurrent issue of all 4 quotes — Jupiter Launch plan handles
    // it easily, and concurrency keeps the total latency to ~1 RTT.
    let (fa_a, fa_b, re_a, re_b) = tokio::join!(
        jup_quote(&jup_base, &sol, &target_mint, probe_lamports, slippage_bps,
                  Some("Pump.fun Amm"), None,                  Some(40)),
        // leg-B (forward) needs the token amount from leg-A — we can't
        // compute it yet, so we issue a placeholder probe and re-quote
        // after fa_a returns. For simplicity, do forward + reverse
        // SEQUENTIALLY but each direction's two legs concurrently.
        async { Ok::<JupQuote, anyhow::Error>(JupQuote {
            in_mint: target_mint, out_mint: sol, in_amount: 0, out_amount: 0,
            raw: serde_json::Value::Null,
        }) },
        jup_quote(&jup_base, &sol, &target_mint, probe_lamports, slippage_bps,
                  None,                   Some("Pump.fun Amm"), Some(40)),
        async { Ok::<JupQuote, anyhow::Error>(JupQuote {
            in_mint: target_mint, out_mint: sol, in_amount: 0, out_amount: 0,
            raw: serde_json::Value::Null,
        }) },
    );
    let _ = (fa_b, re_b);  // placeholders consumed by re-quote below

    // Re-quote each direction's leg B with the actual token amount from
    // its leg A.
    let (forward, reverse) = tokio::join!(
        async {
            let qa = fa_a.map_err(|e| anyhow!("FW leg-A: {e}"))?;
            if qa.out_amount == 0 { return Err(anyhow!("FW leg-A: out=0")); }
            let qb = jup_quote(&jup_base, &target_mint, &sol, qa.out_amount, slippage_bps,
                               None, Some("Pump.fun Amm"), Some(40))
                .await.map_err(|e| anyhow!("FW leg-B: {e}"))?;
            Ok::<(JupQuote, JupQuote), anyhow::Error>((qa, qb))
        },
        async {
            let qa = re_a.map_err(|e| anyhow!("RV leg-A: {e}"))?;
            if qa.out_amount == 0 { return Err(anyhow!("RV leg-A: out=0")); }
            let qb = jup_quote(&jup_base, &target_mint, &sol, qa.out_amount, slippage_bps,
                               Some("Pump.fun Amm"), None, Some(40))
                .await.map_err(|e| anyhow!("RV leg-B: {e}"))?;
            Ok::<(JupQuote, JupQuote), anyhow::Error>((qa, qb))
        },
    );

    // Pick the more profitable direction (if any).
    let fw_net = forward.as_ref().ok().map(|(_, qb)| qb.out_amount as i64 - probe_lamports as i64);
    let rv_net = reverse.as_ref().ok().map(|(_, qb)| qb.out_amount as i64 - probe_lamports as i64);
    let (qa, qb, direction_label, net) = match (forward, reverse) {
        (Ok((qa_f, qb_f)), Ok((qa_r, qb_r))) => {
            let net_f = qb_f.out_amount as i64 - probe_lamports as i64;
            let net_r = qb_r.out_amount as i64 - probe_lamports as i64;
            if net_f >= net_r { (qa_f, qb_f, "FWD(buy-Pump)",  net_f) }
            else              { (qa_r, qb_r, "REV(sell-Pump)", net_r) }
        }
        (Ok((qa_f, qb_f)), Err(_)) => {
            let n = qb_f.out_amount as i64 - probe_lamports as i64;
            (qa_f, qb_f, "FWD(buy-Pump)", n)
        }
        (Err(_), Ok((qa_r, qb_r))) => {
            let n = qb_r.out_amount as i64 - probe_lamports as i64;
            (qa_r, qb_r, "REV(sell-Pump)", n)
        }
        (Err(e1), Err(e2)) => {
            let s1 = e1.to_string(); let s2 = e2.to_string();
            // Silent on the common "no routes / not tradable" cases.
            let benign = |s: &str| s.contains("No routes found")
                || s.contains("not be found") || s.contains("not tradable")
                || s.contains("NO_ROUTES_FOUND");
            if !benign(&s1) || !benign(&s2) {
                eprintln!("  both directions failed for {target_mint}: FW={s1}  RV={s2}");
            }
            return Ok(());
        }
    };

    let net_minus_tip: i64 = net - jito_tip_lamports as i64;
    println!(
        "  💱 arb math {target_mint} [{direction_label}]: probe={} → token={} → back={} → net={:+} lam (after tip {:+})  | other-dir nets: FW={:?} RV={:?}",
        probe_lamports, qa.out_amount, qb.out_amount, net, net_minus_tip, fw_net, rv_net,
    );
    if net_minus_tip < min_profit_lamports {
        return Ok(());
    }

    // We're net-positive on paper — build the swap. Two Jupiter
    // /swap-instructions calls in parallel.
    let (sa_res, sb_res) = tokio::join!(
        jup_swap_ixs(&jup_base, &qa, &payer, true),
        jup_swap_ixs(&jup_base, &qb, &payer, true),
    );
    let mut sa = sa_res.context("leg-A swap-ixs")?;
    let mut sb = sb_res.context("leg-B swap-ixs")?;

    // V3: Token-2022 ATA setup_ix rewriter. Jupiter's setup_ixs hardcode
    // the LEGACY SPL Token program for ATA creates even when the mint is
    // Token-2022. Walk every setup_ix (and cleanup) and patch account[5]
    // to the correct token_program for `target_mint`. wSOL is always
    // legacy SPL Token so its ATA creates need no change — only the
    // target_mint creates get rewritten.
    let n_a = rewrite_ata_token_program(&mut sa.setup_ixs, &target_mint, &target_token_program);
    let n_b = rewrite_ata_token_program(&mut sb.setup_ixs, &target_mint, &target_token_program);
    if let Some(c) = sa.cleanup_ix.as_mut() {
        let mut v = vec![c.clone()];
        rewrite_ata_token_program(&mut v, &target_mint, &target_token_program);
        *c = v.into_iter().next().unwrap();
    }
    if let Some(c) = sb.cleanup_ix.as_mut() {
        let mut v = vec![c.clone()];
        rewrite_ata_token_program(&mut v, &target_mint, &target_token_program);
        *c = v.into_iter().next().unwrap();
    }
    if n_a + n_b > 0 {
        println!(
            "  🔧 V3 rewrote {} ATA-create token_program → {} for {target_mint}",
            n_a + n_b, target_token_program,
        );
    }

    // Assemble: cu_limit, cu_price, jito_tip, then both legs.
    // No flash loan in V2 MVP — wallet's native SOL covers the probe.
    let mut ixs: Vec<Instruction> = Vec::with_capacity(20);
    ixs.push(set_compute_unit_limit_ix(1_400_000));
    ixs.push(set_compute_unit_price_ix(2_000));
    ixs.push(system_transfer_ix(payer, JITO_TIP_ACCOUNT, jito_tip_lamports));
    ixs.extend(sa.setup_ixs.iter().cloned());
    ixs.push(sa.swap_ix.clone());
    if let Some(c) = sa.cleanup_ix.as_ref() { ixs.push(c.clone()); }
    ixs.extend(sb.setup_ixs.iter().cloned());
    ixs.push(sb.swap_ix.clone());
    if let Some(c) = sb.cleanup_ix.as_ref() { ixs.push(c.clone()); }

    // Collect ALTs from both Jupiter legs — they ship their own.
    let mut alts: Vec<(Pubkey, Vec<Pubkey>)> = Vec::new();
    let mut seen_alt: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    for alt_pk in sa.address_lookup_tables.iter().chain(sb.address_lookup_tables.iter()) {
        if !seen_alt.insert(*alt_pk) { continue; }
        if let Ok(addrs) = fetch_alt_addresses(&rpc_url, alt_pk).await {
            if !addrs.is_empty() { alts.push((*alt_pk, addrs)); }
        }
    }

    // Sign v0.
    let blockhash = fetch_latest_blockhash(&rpc_url).await
        .map_err(|e| anyhow!("blockhash: {e}"))?;
    let msg = build_v0_message_multi(&payer, &ixs, &blockhash, &alts, &[]);
    let signed_b64 = sign_v0_tx_b64(&msg, wallet.bytes())
        .map_err(|e| anyhow!("sign: {e}"))?;
    let signed_bytes = base64::engine::general_purpose::STANDARD.decode(&signed_b64).unwrap();
    println!("  ✍️  signed tx: {} bytes ({} ixs, {} alts)",
        signed_bytes.len(), ixs.len(), alts.len());
    if signed_bytes.len() > 1232 {
        eprintln!("  ⚠️  tx {} > 1232 bytes — would be rejected; skipping send", signed_bytes.len());
        return Ok(());
    }

    // Sim.
    let sim_body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"simulateTransaction",
        "params":[signed_b64, {
            "encoding":"base64","sigVerify":false,
            "replaceRecentBlockhash":true,"commitment":"processed",
        }],
    });
    let sim_r: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&sim_body).send().await?
        .error_for_status()?.json().await?;
    let sim_err = sim_r.pointer("/result/value/err");
    if sim_err.is_some() && !sim_err.unwrap().is_null() {
        let logs: Vec<String> = sim_r.pointer("/result/value/logs")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().rev().take(4).filter_map(|x| x.as_str()).map(String::from).collect())
            .unwrap_or_default();
        eprintln!("  🛑 sim FAILED for {target_mint}: err={:?}  logs={:?}", sim_err, logs);
        // Persistent sim failure → dead-letter the mint. Common causes:
        // Token-2022 ATA program mismatch, withhold fee on transfer,
        // mint freeze authority blocks the swap, etc. None of these
        // self-heal on retry; better to stop wasting Jupiter quote
        // budget on a known-bad mint and let other candidates through.
        dead_letter_mint(target_mint);
        return Ok(());
    }
    println!("  ✓ sim OK for {target_mint}");

    if !autofire {
        println!("  (DRY-RUN — set PUMPSWAP_AUTOFIRE=true to send)");
        return Ok(());
    }

    // Extract sig from signed bytes for solscan logging (the tx wire
    // format starts with `[compact-u16 sig_count][sig × 64 bytes]`).
    // With one signer this is `[1, sig_64_bytes, …]`.
    let sig_b58 = if signed_bytes.len() >= 65 && signed_bytes[0] == 1 {
        bs58::encode(&signed_bytes[1..65]).into_string()
    } else {
        "<unparseable>".to_string()
    };

    // Send path: TPU direct (5ms to leader) when configured, else fallback
    // to Helius sendTransaction (~300ms). The 300ms cost the first 3 live
    // fires to slippage races (Custom:6001) — by the time the tx landed
    // the pool moved past our slippage tolerance. TPU direct lands in the
    // same slot when the leader window is right.
    if let Some(sender) = tpu_sender.as_ref() {
        let slot = match fetch_current_slot(&rpc_url).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("  ❌ TPU slot fetch failed: {e}");
                return Ok(());
            }
        };
        match sender.send_to_next_leaders(&signed_bytes, slot, tpu_fanout).await {
            Ok(n) => println!(
                "  🚀 SENT (TPU×{n})  pool={pool_addr}  target={target_mint}  sig=https://solscan.io/tx/{sig_b58}"
            ),
            Err(e) => eprintln!("  ❌ TPU send failed: {e}"),
        }
        return Ok(());
    }

    // Fallback: Helius sendTransaction.
    let send_body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"sendTransaction",
        "params":[signed_b64, {
            "encoding":"base64","skipPreflight":false,"maxRetries":3,
            "preflightCommitment":"processed",
        }],
    });
    let send_r: serde_json::Value = reqwest::Client::new()
        .post(&rpc_url).json(&send_body).send().await?
        .error_for_status()?.json().await?;
    if let Some(err) = send_r.get("error") {
        eprintln!("  ❌ send error for {target_mint}: {err}");
        return Ok(());
    }
    let sig = send_r.pointer("/result").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("send: missing sig"))?;
    println!("  🚀 SENT (helius)  pool={pool_addr}  target={target_mint}  sig=https://solscan.io/tx/{sig}");
    let _ = ArbResultLog { sent_sig: None, skip_reason: 0 };
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    let api_key = std::env::var("HELIUS_API_KEY").context("HELIUS_API_KEY missing")?;
    let rpc_url = arb_scout::rpc::helius_rpc_url(&api_key);
    let grpc_url = std::env::var("LASERSTREAM_GRPC_URL")
        .context("LASERSTREAM_GRPC_URL required")?;
    let min_quote_lamports: u64 = std::env::var("PUMPSWAP_MIN_QUOTE_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(1_000_000_000);  // 1 SOL
    let log_every: u64 = std::env::var("PUMPSWAP_LOG_EVERY")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    // Fresh-graduation filter: a pool we just saw on LaserStream is
    // "interesting" only if its oldest signature is within this many
    // seconds. PumpSwap rewrites the account on every swap so first
    // observation alone catches existing pools too — the sig-age probe
    // separates them.
    let fresh_max_age_secs: i64 = std::env::var("PUMPSWAP_FRESH_MAX_AGE_SECS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(600);  // 10 min

    // ── V2 arb knobs ─────────────────────────────────────────
    let wallet = Arc::new({
        let s = std::env::var("SOLANA_KEYPAIR_JSON").context("SOLANA_KEYPAIR_JSON missing")?;
        Wallet::from_json_array(&s).map_err(|e| anyhow!("wallet: {e}"))?
    });
    let jup_base = std::env::var("JUPITER_ENDPOINT_BASE")
        .unwrap_or_else(|_| "https://api.jup.ag/swap/v1".to_string());
    // Probe size for the arb attempt. Starts conservative — wallet has
    // limited native SOL and V2 doesn't use flash. Default 0.02 SOL
    // (~$1.70) — small enough to test mechanics, large enough to clear
    // tip+fee floor if the spread is real.
    let probe_lamports: u64 = std::env::var("PUMPSWAP_PROBE_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(20_000_000);
    // Net profit floor AFTER tip. Default 0.005 SOL (~$0.43) so a
    // landed tx clears infra cost (tip + priority + fees ≈ 0.001 SOL).
    let min_profit_lamports: i64 = std::env::var("PUMPSWAP_MIN_PROFIT_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(5_000_000);
    let jito_tip_lamports: u64 = std::env::var("PUMPSWAP_JITO_TIP_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let autofire = std::env::var("PUMPSWAP_AUTOFIRE").as_deref() == Ok("true");
    // Adaptive probe sizing. The 1.5 SOL "probe" set via the legacy
    // PUMPSWAP_PROBE_LAMPORTS env is treated as a CEILING; the actual
    // per-attempt probe is min(ceiling, vault_sol × probe_pct_bps/10000).
    // Rationale: a fixed probe blows up to 5-10% slippage on shallow
    // pump.fun graduations (TVL $5-50k) and kills the spread; on deep
    // pools (TVL $500k+) the same probe is too small to amplify the
    // arb. Per-pool adaptive sizing matches probe to depth — same %
    // slippage regardless of pool size. Env knobs:
    //   PUMPSWAP_MAX_PROBE_LAMPORTS  (alias for PROBE_LAMPORTS, ceiling)
    //   PUMPSWAP_PROBE_PCT_VAULT_BPS (default 500 = 5% of SOL vault)
    //   PUMPSWAP_MIN_PROBE_LAMPORTS  (default 20_000_000 = 0.02 SOL,
    //                                  below this the attempt is skipped)
    let probe_pct_vault_bps: u64 = std::env::var("PUMPSWAP_PROBE_PCT_VAULT_BPS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(500);
    let min_probe_lamports: u64 = std::env::var("PUMPSWAP_MIN_PROBE_LAMPORTS")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(20_000_000);
    // TPU direct send — bypasses Helius sendTransaction's ~300ms latency
    // that was costing fires to slippage races. Set PUMPSWAP_USE_TPU=false
    // to fall back to Helius.
    let use_tpu = std::env::var("PUMPSWAP_USE_TPU").as_deref() != Ok("false");
    let tpu_fanout: usize = std::env::var("PUMPSWAP_TPU_FANOUT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(4);
    let tpu_sender: Option<Arc<TpuSender>> = if use_tpu {
        match TpuSender::new(rpc_url.clone()).await {
            Ok(s) => {
                let s = Arc::new(s);
                s.spawn_refresher();
                println!("  ✓ TpuSender initialised (fanout={tpu_fanout})");
                Some(s)
            }
            Err(e) => {
                eprintln!("  TpuSender init failed: {e} — falling back to Helius sendTransaction");
                None
            }
        }
    } else { None };

    println!("══════════════════════════════════════════════════════════════════");
    println!("  PumpSwap graduation watcher + arber (V2)");
    println!("  program:        {}", PUMPSWAP_PROGRAM);
    println!("  wallet:         {}", wallet.pubkey());
    println!("  min quote:      {min_quote_lamports} lamports (= {} SOL)",
             min_quote_lamports as f64 / 1e9);
    println!("  fresh max age:  {fresh_max_age_secs}s");
    println!("  jupiter base:   {jup_base}");
    println!("  arb probe:      {probe_lamports} lamports (= {} SOL)", probe_lamports as f64 / 1e9);
    println!("  min net profit: {min_profit_lamports} lamports after tip (= {} SOL)",
             min_profit_lamports as f64 / 1e9);
    println!("  jito tip:       {jito_tip_lamports} lamports");
    println!("  autofire:       {autofire}");
    println!("══════════════════════════════════════════════════════════════════");

    let (tx, mut rx) = mpsc::channel::<arb_stream::wss::AccountUpdate>(1024);
    tokio::spawn(subscriber_loop(grpc_url, api_key.clone(), tx.clone()));

    // Shred fast-path: catches PumpSwap pool creates ~50-100ms before
    // LaserStream account updates. Two impls:
    //   - `--features shredstream` (Phase D): native Rust gRPC subscriber
    //   - default: poll the alert file written by `unified_shred_tee.py`
    let shred_disabled = std::env::var("SHRED_ALERT_DISABLE").ok().as_deref() == Some("true");
    if !shred_disabled {
        #[cfg(feature = "shredstream")]
        {
            let endpoint = std::env::var("SHREDSTREAM_GRPC_URL")
                .unwrap_or_else(|_| "http://localhost:9000".to_string());
            let rpc_url_for_shred = rpc_url.clone();
            let shred_tx = tx.clone();
            tokio::spawn(async move {
                shred_pumpswap_native(endpoint, rpc_url_for_shred, shred_tx).await;
            });
            println!("  ✓ NATIVE shred-pumpswap-create fast-path spawned (Rust gRPC)");
        }
        #[cfg(not(feature = "shredstream"))]
        {
            let rpc_url_for_shred = rpc_url.clone();
            let shred_tx = tx.clone();
            tokio::spawn(async move {
                shred_pumpswap_poller(rpc_url_for_shred, shred_tx).await;
            });
            println!("  ✓ shred-pumpswap-create file-bridge spawned (Python tee)");
        }
    }

    let stats = Arc::new(tokio::sync::RwLock::new(Stats::default()));
    // Heartbeat task.
    {
        let stats = stats.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let s = stats.read().await.clone();
                println!(
                    "  💓 pool_updates={} new={} stale={} low_quote={} no_sol_side={} arb_candidates={}",
                    s.pool_updates_seen, s.new_pools_detected,
                    s.skipped_stale, s.skipped_low_quote, s.skipped_no_sol_side,
                    s.arb_candidates,
                );
            }
        });
    }

    // Recently-seen pool addresses → suppress duplicate logs on repeated
    // updates (PumpSwap rewrites the pool account on every swap with new
    // reserves, but the layout / mints don't change — only the LP supply
    // and vault balances). For arb detection we only care about the
    // FIRST observation per pool address.
    let seen: Arc<tokio::sync::RwLock<std::collections::HashSet<Pubkey>>> =
        Arc::new(tokio::sync::RwLock::new(std::collections::HashSet::new()));

    println!("✓ subscriber spawned. Waiting for PumpSwap pool updates…");
    while let Some(upd) = rx.recv().await {
        {
            let mut s = stats.write().await;
            s.pool_updates_seen += 1;
        }
        let Some(pool) = parse_pumpswap_pool(upd.account, &upd.data) else { continue; };

        // First-time observation? PumpSwap rewrites the pool account on
        // every swap, but our interest is "did we just see this address
        // for the FIRST time" — that's the graduation moment (or close
        // enough to it for arb detection purposes).
        let first_observe = {
            let mut s = seen.write().await;
            s.insert(pool.address)
        };
        if !first_observe { continue; }

        {
            let mut s = stats.write().await;
            s.new_pools_detected += 1;
        }

        // Echo the new pool. base/quote mints are the most actionable
        // bits — the user wants to arb the base_mint vs the same mint
        // on Raydium/Orca.
        let n = stats.read().await.new_pools_detected;
        if log_every == 0 || n % log_every == 0 {
            println!(
                "🆕 pool={}  base={}  quote={}  bump={}  idx={}",
                pool.address, pool.base_mint, pool.quote_mint,
                pool.pool_bump, pool.index,
            );
        }

        // Identify the NON-SOL side (the "target" token we'd arb on
        // another DEX) and the SOL side (where we measure quote depth).
        // PumpSwap doesn't fix base/quote order — some pools have base=SOL
        // others quote=SOL. Phase-1 only handles SOL-paired pools; if
        // neither side is SOL we skip.
        let (target_mint, sol_vault) = if pool.quote_mint == WSOL_MINT {
            (pool.base_mint,  pool.pool_quote_vault)
        } else if pool.base_mint == WSOL_MINT {
            (pool.quote_mint, pool.pool_base_vault)
        } else {
            let mut s = stats.write().await;
            s.skipped_no_sol_side += 1;
            continue;
        };

        // Spawn the freshness + cross-DEX probe so the subscriber loop
        // keeps draining. RPC budget: 2-7 calls per probe (1 sig probe +
        // 1 vault balance + up to 8 cross-DEX getProgramAccounts that
        // short-circuit on first hit).
        let pool_addr = pool.address;
        let sol_vault = sol_vault;
        let rpc = rpc_url.clone();
        let stats_inner = stats.clone();
        let wallet_inner = wallet.clone();
        let jup_inner = jup_base.clone();
        let tpu_inner = tpu_sender.clone();
        tokio::spawn(async move {
            // 1. Freshness gate. If the pool's oldest sig is too old,
            //    this LaserStream "first observation" was triggered by
            //    routine swap traffic on a long-lived pool, not a
            //    graduation — skip.
            let age = match oldest_sig_age_seconds(&rpc, &pool_addr).await {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("  age probe failed for pool={pool_addr}: {e}");
                    return;
                }
            };
            if age > fresh_max_age_secs {
                let mut s = stats_inner.write().await;
                s.skipped_stale += 1;
                return;
            }

            // 2. SOL-side vault balance — gates trivially-small pools.
            let body = serde_json::json!({
                "jsonrpc":"2.0","id":1,"method":"getTokenAccountBalance",
                "params":[sol_vault.to_string()],
            });
            let qv_amount: u64 = match reqwest::Client::new().post(&rpc).json(&body).send().await {
                Ok(r) => match r.error_for_status() {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(v) => v.pointer("/result/value/amount")
                            .and_then(|x| x.as_str())
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(0),
                        Err(_) => return,
                    },
                    Err(_) => return,
                },
                Err(_) => return,
            };
            if qv_amount < min_quote_lamports {
                let mut s = stats_inner.write().await;
                s.skipped_low_quote += 1;
                return;
            }

            // 2b. Adaptive probe sizing. Match probe to pool depth so the
            //     per-attempt slippage is bounded regardless of TVL.
            //       probe = min(MAX_PROBE, sol_vault × pct_bps / 10000)
            //     If the adaptive probe falls below MIN_PROBE we skip
            //     entirely — at probe < 0.02 SOL the tip + fee floor
            //     eats any reasonable spread.
            let adaptive_probe: u64 = std::cmp::min(
                probe_lamports,
                (qv_amount as u128 * probe_pct_vault_bps as u128 / 10_000u128) as u64,
            );
            if adaptive_probe < min_probe_lamports {
                let mut s = stats_inner.write().await;
                s.skipped_low_quote += 1;
                return;
            }

            // 3. Skip the offset-based cross-DEX getProgramAccounts probe
            //    — earlier deploy showed it was matching at wrong offsets
            //    (Raydium AMM v4 base_mint is NOT at 400 — that's the
            //    open-orders pubkey field). Result was 100% false-positive
            //    "ARB CANDIDATES" pointing to pools that don't actually
            //    hold the target token, so Jupiter couldn't route to them.
            //
            //    Jupiter IS the canonical "does this token trade anywhere"
            //    oracle. We let the two-leg Jupiter quote (leg A via
            //    Pump.fun Amm only, leg B via everything EXCEPT Pump.fun)
            //    serve as both the existence check and the spread probe —
            //    one call covers both. Cost: one extra Jupiter quote per
            //    fresh pool that doesn't have a twin yet (~$0 on Launch
            //    plan, well under the 100/sec budget).
            {
                let mut s = stats_inner.write().await;
                s.arb_candidates += 1;
                drop(s);
            }
            println!(
                "🎯 attempting arb  age={age}s  sol_depth={:.4}  target={}  pool={}",
                qv_amount as f64 / 1e9, target_mint, pool_addr,
            );
            if let Err(e) = attempt_pumpswap_arb(
                rpc.clone(), jup_inner.clone(), wallet_inner.clone(),
                target_mint, pool_addr,
                adaptive_probe, min_profit_lamports,
                jito_tip_lamports, autofire,
                tpu_inner.clone(), tpu_fanout,
            ).await {
                eprintln!("  arb attempt error for {target_mint}: {e}");
            }
        });
    }
    Err(anyhow!("subscriber channel closed — exiting"))
}
