//! One-shot helper: fetches a recent successful Obric V2 swap transaction
//! from mainnet, finds the program's instruction within it, and prints the
//! full ordered account list. Used to reverse-engineer the canonical
//! account layout when bitquery's IDL is incomplete.
//!
//! Run:
//! ```
//! cargo run --release --bin obric-trace
//! ```

use anyhow::{anyhow, Context};
use base64::Engine;
use bs58;
use arb_dex_obric::ObricV2Data;

const OBRIC_PROGRAM: &str = "obriQD1zbpyLz95G5n7nJe6a4DPjpFwa5XYPoNm113y";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();
    let api_key = std::env::var("HELIUS_API_KEY")
        .context("HELIUS_API_KEY not set in env or .env")?;
    let rpc = format!("https://mainnet.helius-rpc.com/?api-key={api_key}");
    let http = reqwest::Client::new();

    // Try to fetch the on-chain Anchor IDL first. Anchor programs that opt
    // in publish their IDL at PDA `["anchor:idl", program_id]`. The account
    // body is a 44-byte header (authority(32) + data_len(4) + reserved(8))
    // followed by zlib-deflated JSON. We'll bail out and use the inline
    // dump path if not found.
    let program_pk = solana_program::pubkey::Pubkey::from_str_const(OBRIC_PROGRAM);
    let (idl_pda, _bump) = solana_program::pubkey::Pubkey::find_program_address(
        &[], &program_pk,
    );
    let idl_pda_v2 = {
        let base = solana_program::pubkey::Pubkey::find_program_address(&[], &program_pk).0;
        solana_program::pubkey::Pubkey::create_with_seed(
            &base, "anchor:idl", &program_pk,
        ).ok()
    };
    println!("Anchor IDL PDA candidates:");
    println!("  v1 (base PDA, no seeds): {idl_pda}");
    if let Some(p) = idl_pda_v2 {
        println!("  v2 (createWithSeed):     {p}");
    }
    for (label, candidate) in [
        ("v1 base PDA", idl_pda),
        ("v2 seed",     idl_pda_v2.unwrap_or(idl_pda)),
    ] {
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
            "params":[candidate.to_string(),{"encoding":"base64","commitment":"confirmed"}],
        });
        let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
        if let Some(data_b64) = r.pointer("/result/value/data/0").and_then(|v| v.as_str()) {
            let raw = base64::engine::general_purpose::STANDARD.decode(data_b64)?;
            println!("  → {label}: account exists, {} bytes", raw.len());
            if raw.len() > 44 {
                let body = &raw[44..];
                use std::io::Read;
                let mut decoder = flate2::read::ZlibDecoder::new(body);
                let mut json_out = String::new();
                if decoder.read_to_string(&mut json_out).is_ok() && !json_out.is_empty() {
                    println!("════ ON-CHAIN IDL JSON ════");
                    let parsed: serde_json::Value = serde_json::from_str(&json_out)?;
                    if let Some(ixs) = parsed.get("instructions").and_then(|v| v.as_array()) {
                        for ix in ixs {
                            let name = ix.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            if name.to_lowercase().contains("swap") {
                                println!("---- {name} ----");
                                if let Some(accs) = ix.get("accounts").and_then(|v| v.as_array()) {
                                    for (i, a) in accs.iter().enumerate() {
                                        let an = a.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                        let w  = a.get("isMut").or_else(|| a.get("writable")).and_then(|v| v.as_bool()).unwrap_or(false);
                                        let s  = a.get("isSigner").or_else(|| a.get("signer")).and_then(|v| v.as_bool()).unwrap_or(false);
                                        let flags = match (w, s) {
                                            (true, true)  => "[W,S]",
                                            (true, false) => "[W,_]",
                                            (false,true)  => "[_,S]",
                                            (false,false) => "[_,_]",
                                        };
                                        println!("  [{i:>2}] {flags} {an}");
                                    }
                                }
                            }
                        }
                    }
                    return Ok(());
                }
            }
        } else {
            println!("  → {label}: not found");
        }
    }
    println!();
    println!("No on-chain IDL — falling back to ix trace.");

    println!("Fetching recent signatures for Obric V2 program …");
    let body = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"getSignaturesForAddress",
        "params":[OBRIC_PROGRAM,{"limit":50,"commitment":"confirmed"}],
    });
    let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
    let sigs = r.pointer("/result").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("getSignaturesForAddress: {r}"))?;

    let success_sigs: Vec<&str> = sigs.iter()
        .filter(|s| s.get("err").map(|e| e.is_null()).unwrap_or(false))
        .filter_map(|s| s.get("signature").and_then(|v| v.as_str()))
        .take(5)
        .collect();
    println!("Found {} recent successful txs. Probing each for swap_x_to_y …",
             success_sigs.len());

    for sig in success_sigs {
        let body = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"getTransaction",
            "params":[sig,{
                "encoding":"json",
                "commitment":"confirmed",
                "maxSupportedTransactionVersion":0,
            }],
        });
        let r: serde_json::Value = http.post(&rpc).json(&body).send().await?.json().await?;
        let result = match r.pointer("/result") {
            Some(v) if !v.is_null() => v,
            _ => continue,
        };

        // Account keys = static + ALT-loaded (writable then readonly).
        let static_keys: Vec<String> = result.pointer("/transaction/message/accountKeys")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let alt_writable: Vec<String> = result.pointer("/meta/loadedAddresses/writable")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let alt_readonly: Vec<String> = result.pointer("/meta/loadedAddresses/readonly")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut all_keys: Vec<String> = Vec::with_capacity(
            static_keys.len() + alt_writable.len() + alt_readonly.len(),
        );
        all_keys.extend(static_keys.iter().cloned());
        all_keys.extend(alt_writable.iter().cloned());
        all_keys.extend(alt_readonly.iter().cloned());

        let outer_ixs = result.pointer("/transaction/message/instructions")
            .and_then(|v| v.as_array());
        let inner_groups = result.pointer("/meta/innerInstructions")
            .and_then(|v| v.as_array());

        let try_match = |ix: &serde_json::Value, label: &str| -> Option<()> {
            let prog_idx = ix.get("programIdIndex")?.as_u64()? as usize;
            let prog_pk = all_keys.get(prog_idx)?;
            if prog_pk != OBRIC_PROGRAM { return None; }
            let data_b58 = ix.get("data")?.as_str()?;
            let data_bytes = bs58::decode(data_b58).into_vec().ok()?;
            if data_bytes.len() < 8 { return None; }
            let disc = &data_bytes[0..8];

            // Match against every plausible variant; if it's something
            // unexpected, print the raw discriminator so the operator can
            // identify it manually.
            let candidate_names = [
                "swap_x_to_y", "swap_y_to_x", "swap", "swapXToY", "swapYToX",
                "swap_v2", "swap_with_pyth", "swap_pyth", "swap_concentrated",
                "swap_pyth_v1", "swap_pyth_v2", "swap_with_amount",
                "swap_in_amount", "exchange", "exchange_x_to_y",
                "exchange_y_to_x", "execute_swap", "do_swap",
                "concentrated_swap", "trade", "trade_x_to_y",
                "swap_exact_in", "swap_exact_in_x_to_y",
                "obric_swap", "obric_swap_x_to_y",
                "swap_xy", "swap_yx", "swap_x", "swap_y",
                "swap_amount_in", "swap_with_oracle",
                "deposit", "withdraw", "createPair", "create_pair",
                "create_pair_v2", "create_pair_v3", "update_concentration",
                "update_version", "update_fee_params", "update_oracles",
                "withdraw_fees",
            ];
            let candidates: Vec<(&str, [u8; 8])> = candidate_names.iter()
                .map(|n| (*n, anchor_disc(&format!("global:{n}"))))
                .collect();
            let variant = candidates.iter()
                .find_map(|(n, d)| if disc == d.as_slice() { Some(*n) } else { None })
                .unwrap_or("UNKNOWN");
            // Only emit swap-shaped ixs (skip deposits/admin etc.).
            // Heuristic: data length ≥ 24 means it likely carries
            // (amount_in: u64) + (min_out: u64) after the discriminator =
            // typical swap shape. Deposit/withdraw differ.
            if data_bytes.len() < 24 { return None; }

            println!();
            println!("════════════════════════════════════════════════════════════════");
            println!("Found {label} Obric ix in tx {sig}");
            println!("Variant: {variant}");
            println!("Discriminator (hex): {}", hex(disc));
            println!("Data length: {} bytes  (= 8 disc + {} payload)",
                     data_bytes.len(), data_bytes.len() - 8);
            println!("RAW DATA (hex): {}", hex(&data_bytes));
            if data_bytes.len() >= 25 {
                let payload = &data_bytes[8..];
                println!("Payload first byte (u8): {}", payload[0]);
                if payload.len() >= 9 {
                    let u64a = u64::from_le_bytes(payload[1..9].try_into().unwrap());
                    println!("  bytes [1..9]   as u64 LE: {u64a}");
                }
                if payload.len() >= 17 {
                    let u64b = u64::from_le_bytes(payload[9..17].try_into().unwrap());
                    println!("  bytes [9..17]  as u64 LE: {u64b}");
                }
                println!("Alt: bytes[0..8] u64 LE = {}",
                         u64::from_le_bytes(payload[0..8].try_into().unwrap()));
                println!("     bytes[8..16] u64 LE = {}",
                         u64::from_le_bytes(payload[8..16].try_into().unwrap()));
                println!("     last byte = {}", payload[payload.len()-1]);
            }
            println!();
            println!("ACCOUNT LIST (in order, with mainnet pubkeys + role match):");
            // Fetch the trading_pair (slot 0) and parse it as ObricV2Data, then
            // tag each account slot with its role from the parser fields.
            let acc_indices = ix.get("accounts")?.as_array()?;
            let pool_idx = acc_indices.first()?.as_u64()? as usize;
            let pool_pk = all_keys.get(pool_idx)?;
            // Re-fetch pool data for parsing.
            // We're inside a closure with &http+&rpc captured — use blocking
            // join for simplicity.
            let pool_data = futures::executor::block_on(async {
                let body = serde_json::json!({
                    "jsonrpc":"2.0","id":1,"method":"getAccountInfo",
                    "params":[pool_pk,{"encoding":"base64","commitment":"confirmed"}],
                });
                let r: serde_json::Value = http.post(&rpc).json(&body).send().await.ok()?
                    .json().await.ok()?;
                let b64 = r.pointer("/result/value/data/0")?.as_str()?;
                base64::engine::general_purpose::STANDARD.decode(b64).ok()
            });
            let parsed = pool_data.as_deref().and_then(ObricV2Data::try_parse);
            let label_for = |pk: &str| -> &'static str {
                if let Some(p) = parsed.as_ref() {
                    if pk == p.mint_x.to_string()         { return "mint_x"; }
                    if pk == p.mint_y.to_string()         { return "mint_y"; }
                    if pk == p.reserve_x_vault.to_string(){ return "reserve_x_vault"; }
                    if pk == p.reserve_y_vault.to_string(){ return "reserve_y_vault"; }
                    if pk == p.protocol_fee_x.to_string() { return "protocol_fee_x"; }
                    if pk == p.protocol_fee_y.to_string() { return "protocol_fee_y"; }
                    if pk == p.x_price_feed.to_string()   { return "x_price_feed"; }
                    if pk == p.y_price_feed.to_string()   { return "y_price_feed"; }
                }
                if pk == "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" { return "token_program"; }
                if pk == pool_pk.as_str()                              { return "trading_pair"; }
                ""
            };
            // Brute-force locate each unknown slot's pubkey in the raw pool
            // bytes — tells us the offset, which we can then add to the
            // ObricV2Data parser.
            let raw_pool = pool_data.as_deref();
            let find_offset = |target_b58: &str| -> Option<usize> {
                let raw = raw_pool?;
                let bytes = bs58::decode(target_b58).into_vec().ok()?;
                if bytes.len() != 32 { return None; }
                raw.windows(32).position(|w| w == bytes.as_slice())
            };

            for (i, idx_v) in acc_indices.iter().enumerate() {
                let idx = idx_v.as_u64()? as usize;
                let pk = all_keys.get(idx).map(String::as_str).unwrap_or("?");
                let region = if idx < static_keys.len() { "static" }
                    else if idx < static_keys.len() + alt_writable.len() { "alt-W" }
                    else { "alt-R" };
                let role = label_for(pk);
                let offset_hint = if role.is_empty() {
                    match find_offset(pk) {
                        Some(off) => format!("found in pool data @ offset {off}"),
                        None => "(not in pool data)".to_string(),
                    }
                } else { String::new() };
                println!("  [{i:>2}] [{region}] {pk}  → {role}  {offset_hint}");
            }
            println!("════════════════════════════════════════════════════════════════");
            Some(())
        };

        if let Some(arr) = outer_ixs {
            for ix in arr {
                if try_match(ix, "outer").is_some() { return Ok(()); }
            }
        }
        if let Some(groups) = inner_groups {
            for g in groups {
                if let Some(inners) = g.get("instructions").and_then(|v| v.as_array()) {
                    for ix in inners {
                        if try_match(ix, "inner-CPI").is_some() { return Ok(()); }
                    }
                }
            }
        }
    }

    Err(anyhow!("scanned {} txs, no Obric swap_x_to_y / swap_y_to_x ix found — try a higher limit", 50))
}

fn anchor_disc(preimage: &str) -> [u8; 8] {
    let h = solana_program::hash::hash(preimage.as_bytes());
    let bytes = h.to_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&bytes[..8]);
    out
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}
