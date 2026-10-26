//! One-shot underwater-account scanner for MarginFi v2.
//!
//! ```text
//! cargo run --release --bin marginfi-discover                 # list all accounts with non-zero liability
//! cargo run --release --bin marginfi-discover -- --bank PK    # filter to a single liability bank
//! cargo run --release --bin marginfi-discover -- --top 50     # cap output
//! ```
//!
//! ## What it does
//!
//! 1. `getProgramAccounts` on `MARGINFI_V2_PROGRAM` filtered by
//!    - `dataSize: 2312` (= 8 disc + 2304 struct, verified 2026-05-15
//!      against marginfi-v2 main `type-crate/src/types/user_account.rs`)
//!    - `memcmp` on Anchor disc `account:MarginfiAccount`
//! 2. Parses each account using **literal byte offsets** from the
//!    `#[repr(C)]` layout — no Borsh, no Anchor crate, no surprises:
//!    ```text
//!    [0..8]    disc
//!    [8..40]   group
//!    [40..72]  authority
//!    [72..1800] balances[16] each 104 bytes
//!      per Balance:
//!        [0]    active   (u8)
//!        [1..33]  bank_pk (Pubkey)
//!        [33]    bank_asset_tag
//!        [34..36] tag
//!        [36..40] _pad0
//!        [40..56] asset_shares     (WrappedI80F48 = i128 LE, /2^48)
//!        [56..72] liability_shares (WrappedI80F48)
//!        [72..88] emissions_outstanding
//!        [88..96] last_update
//!    ```
//! 3. Sums asset/liab shares across active balances and emits a ranked
//!    list. Share counts are NOT dollar values — converting requires
//!    `bank.share_value` (fetched in step 2 of the pipeline). The ranking
//!    here is a CANDIDATE filter that the Pyth-paired liquidator then
//!    fetches detailed bank prices for before deciding to fire.
//!
//! Note on health: a fully-accurate health computation needs every
//! relevant bank's `asset_share_value`, `liability_share_value`,
//! `asset_weight_maint`, `liability_weight_maint`, and current oracle
//! price. That's a separate batch step. This binary's job is reducing
//! the universe from "every MarginFi account" (~thousands) to "few-dozen
//! candidates with real liabilities" so the next stage has a small set.

use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{anyhow, Context};
use base64::Engine;
use solana_program::pubkey::Pubkey;

const MARGINFI_V2_PROGRAM: &str = "MFv2hWf31Z9kbCa1snEPYctwafyhdvnV7FZnsebVacA";
const ACCOUNT_SIZE: usize = 2312;
const MAX_BALANCES: usize = 16;
const BALANCE_SIZE: usize = 104;
const BALANCES_OFFSET: usize = 72;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")))
        .compact()
        .init();

    // Tiny argparse — keep it dep-free.
    let args: Vec<String> = std::env::args().collect();
    let mut bank_filter: Option<Pubkey> = None;
    let mut top: usize = 30;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--bank" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow!("--bank needs a value"))?;
                bank_filter = Some(Pubkey::from_str(v.trim()).context("invalid --bank pubkey")?);
                i += 2;
            }
            "--top" => {
                top = args.get(i + 1).ok_or_else(|| anyhow!("--top needs a value"))?.parse()?;
                i += 2;
            }
            x => return Err(anyhow!("unknown arg: {x}")),
        }
    }

    let api_key = std::env::var("HELIUS_API_KEY")
        .context("HELIUS_API_KEY missing (set in .env or environment)")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");

    let disc = anchor_account_disc("MarginfiAccount");
    println!("MarginfiAccount Anchor disc: {}", hex(&disc));

    let memcmp_b58 = bs58::encode(disc).into_string();
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getProgramAccounts",
        "params":[MARGINFI_V2_PROGRAM, {
            "encoding": "base64",
            "commitment": "confirmed",
            "filters": [
                {"dataSize": ACCOUNT_SIZE},
                {"memcmp": {"offset": 0, "bytes": memcmp_b58}},
            ],
        }],
    });

    println!("→ fetching MarginfiAccount records (dataSize={ACCOUNT_SIZE}) …");
    let t = std::time::Instant::now();
    let r: serde_json::Value = reqwest::Client::new()
        .post(&rpc).json(&body)
        .send().await?
        .error_for_status()?
        .json().await?;
    let arr = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no result array: {r}"))?;
    println!("✓ {} accounts in {:.1}s", arr.len(), t.elapsed().as_secs_f32());

    // Parse + rank.
    let mut rows: Vec<Row> = Vec::with_capacity(arr.len());
    let mut bank_liab_totals: HashMap<Pubkey, f64> = HashMap::new();
    for entry in arr {
        let address = entry.get("pubkey").and_then(|v| v.as_str()).unwrap_or("");
        let data_b64 = entry.pointer("/account/data/0").and_then(|v| v.as_str()).unwrap_or("");
        let bytes = match base64::engine::general_purpose::STANDARD.decode(data_b64) {
            Ok(b) => b,
            Err(_) => continue,
        };
        if bytes.len() < ACCOUNT_SIZE { continue; }
        let Some(parsed) = parse_account(&bytes) else { continue };

        // Apply bank filter (if set): only keep accounts with a non-zero
        // liability on that bank.
        let matches_filter = match bank_filter {
            None => true,
            Some(b) => parsed.balances.iter().any(|bal|
                bal.active && bal.bank_pk == b && bal.liability_shares > 0.0),
        };
        if !matches_filter { continue; }

        let total_liab: f64 = parsed.balances.iter().map(|b| b.liability_shares).sum();
        let total_asset: f64 = parsed.balances.iter().map(|b| b.asset_shares).sum();
        if total_liab <= 0.0 { continue; }  // no debt = no liquidation target

        for b in &parsed.balances {
            if b.active && b.liability_shares > 0.0 {
                *bank_liab_totals.entry(b.bank_pk).or_insert(0.0) += b.liability_shares;
            }
        }

        rows.push(Row {
            address: address.to_string(),
            authority: parsed.authority,
            total_asset,
            total_liab,
            balances: parsed.balances,
        });
    }

    rows.sort_by(|a, b| b.total_liab.partial_cmp(&a.total_liab).unwrap_or(std::cmp::Ordering::Equal));
    println!("\n=== {} accounts with non-zero liability (top {}) ===",
             rows.len(), top.min(rows.len()));
    for (i, row) in rows.iter().take(top).enumerate() {
        println!(
            "  #{i:>3} liab={liab:>14.4}  asset={asset:>14.4}  ratio={ratio:>7.4}  acct={addr}  auth={auth}",
            i = i + 1,
            liab = row.total_liab,
            asset = row.total_asset,
            ratio = if row.total_asset > 0.0 { row.total_liab / row.total_asset } else { f64::INFINITY },
            addr = row.address,
            auth = row.authority,
        );
        for b in &row.balances {
            if !b.active { continue; }
            if b.asset_shares == 0.0 && b.liability_shares == 0.0 { continue; }
            let kind = if b.liability_shares > b.asset_shares { "L" } else { "A" };
            println!("       {kind} bank={bank} asset={a:>14.4} liab={l:>14.4}",
                     bank = b.bank_pk,
                     a = b.asset_shares,
                     l = b.liability_shares);
        }
    }

    // Bank histogram — useful for picking which liability banks we should
    // tag for atomic-Pyth coverage. Bigger numbers = more candidates.
    let mut bank_rows: Vec<(Pubkey, f64)> = bank_liab_totals.into_iter().collect();
    bank_rows.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    println!("\n=== Liability banks by total share volume ===");
    for (i, (bank, total)) in bank_rows.iter().take(15).enumerate() {
        println!("  #{:>3} bank={bank} liab_shares_total={total:.2}", i + 1);
    }

    println!("\nNext: pick a target bank (e.g. USDC bank) and rerun with --bank <PUBKEY>");
    println!("Then for each candidate fetch its banks' share_value + maintenance_weight to compute real $-denominated health.");
    Ok(())
}

#[derive(Debug, Clone)]
struct Balance {
    active:           bool,
    bank_pk:          Pubkey,
    asset_shares:     f64,
    liability_shares: f64,
}

#[derive(Debug)]
struct Parsed {
    #[allow(dead_code)]
    group:    Pubkey,
    authority: Pubkey,
    balances: [Balance; MAX_BALANCES],
}

#[derive(Debug)]
struct Row {
    address:    String,
    authority:  Pubkey,
    total_asset: f64,
    total_liab:  f64,
    balances:    [Balance; MAX_BALANCES],
}

fn parse_account(bytes: &[u8]) -> Option<Parsed> {
    let group     = read_pubkey(bytes, 8)?;
    let authority = read_pubkey(bytes, 40)?;
    let mut balances: [Balance; MAX_BALANCES] = std::array::from_fn(|_| Balance {
        active: false, bank_pk: Pubkey::default(),
        asset_shares: 0.0, liability_shares: 0.0,
    });
    for i in 0..MAX_BALANCES {
        let off = BALANCES_OFFSET + i * BALANCE_SIZE;
        if off + BALANCE_SIZE > bytes.len() { return None; }
        balances[i] = parse_balance(&bytes[off..off + BALANCE_SIZE])?;
    }
    Some(Parsed { group, authority, balances })
}

fn parse_balance(b: &[u8]) -> Option<Balance> {
    let active = b[0] == 1;
    let bank_pk = read_pubkey(b, 1)?;
    // asset_shares at offset 40, liability_shares at 56, both i128 LE / 2^48.
    let asset_raw = read_i128(b, 40)?;
    let liab_raw  = read_i128(b, 56)?;
    Some(Balance {
        active, bank_pk,
        asset_shares:     wrapped_i80f48_to_f64(asset_raw),
        liability_shares: wrapped_i80f48_to_f64(liab_raw),
    })
}

fn read_pubkey(bytes: &[u8], off: usize) -> Option<Pubkey> {
    if off + 32 > bytes.len() { return None; }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&bytes[off..off + 32]);
    Some(Pubkey::new_from_array(buf))
}

fn read_i128(bytes: &[u8], off: usize) -> Option<i128> {
    if off + 16 > bytes.len() { return None; }
    let mut buf = [0u8; 16];
    buf.copy_from_slice(&bytes[off..off + 16]);
    Some(i128::from_le_bytes(buf))
}

/// I80F48 = signed fixed-point with 48 fractional bits, stored as i128 LE.
fn wrapped_i80f48_to_f64(v: i128) -> f64 {
    (v as f64) / (1u128 << 48) as f64
}

fn anchor_account_disc(name: &str) -> [u8; 8] {
    let preimage = format!("account:{name}");
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}
