//! Address Lookup Table cache.
//!
//! On startup we fetch the ALT account from the chain (via `getAccountInfo`),
//! parse out the list of addresses it stores, and keep them in memory. The
//! v0-tx builder consumes this list to decide which accounts can be
//! lookup-resolved (32-byte pubkey → 1-byte index in the wire format).
//!
//! ## Account layout (from `solana-sdk/program/src/address_lookup_table/state.rs`)
//!
//! ```text
//! [u32 LE: discriminator]                       offset 0..4
//! [u64 LE: deactivation_slot]                   offset 4..12
//! [u64 LE: last_extended_slot]                  offset 12..20
//! [u8: last_extended_slot_start_index]          offset 20..21
//! [u8: option discriminator for authority]      offset 21..22
//! [32 bytes: authority pubkey] (if option=1)    offset 22..54
//! [2 bytes: padding to 56-byte header]          offset 54..56
//! [N × 32 bytes: addresses]                     offset 56..
//! ```
//!
//! Total = `56 + 32 × N`. We only need the addresses, so we skip the header.

use arb_core::{ArbError, ArbResult};
use base64::Engine;
use solana_program::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const ALT_HEADER_SIZE: usize = 56;
const PUBKEY_SIZE: usize = 32;

/// Discriminator value the lookup-table program writes into the first 4 bytes
/// of an *initialised* ALT account. Used as a sanity check during parse.
const ACTIVATED_ALT_DISCRIMINATOR: u32 = 1;

/// Process-local cache for ALT address lists. ALTs are written-once /
/// extended-rarely; we deployed all three currently in use today and
/// don't expect them to change. The 5-minute TTL is a paranoid window
/// in case we DO re-deploy mid-session; anything sooner just refetches.
///
/// Why this matters: marginfi-hunter does ~3 `fetch_alt_addresses` calls
/// per liquidation attempt and at peak fires ~3 attempts/sec → 9 RPC
/// hits/sec on Helius just for ALT reads. Live observation 2026-05-16:
/// ALT fetches accounted for ~60% of all 429s coming back from the
/// `getAccountInfo` endpoint. Caching wipes them out.
static ALT_CACHE: Mutex<Option<HashMap<Pubkey, (Instant, Vec<Pubkey>)>>> = Mutex::new(None);
const ALT_CACHE_TTL: Duration = Duration::from_secs(300);

/// Parse the raw `data` of an ALT account into its address list.
pub fn parse_alt_account(data: &[u8]) -> ArbResult<Vec<Pubkey>> {
    if data.len() < ALT_HEADER_SIZE {
        return Err(ArbError::AccountParse {
            dex: "address-lookup-table",
            reason: format!(
                "data too short: {} bytes, need ≥{ALT_HEADER_SIZE} for header",
                data.len()
            ),
        });
    }
    let discriminator = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if discriminator != ACTIVATED_ALT_DISCRIMINATOR {
        return Err(ArbError::AccountParse {
            dex: "address-lookup-table",
            reason: format!(
                "wrong discriminator {discriminator} (expected {ACTIVATED_ALT_DISCRIMINATOR}); \
                 account is not an activated ALT"
            ),
        });
    }
    let body = &data[ALT_HEADER_SIZE..];
    if body.len() % PUBKEY_SIZE != 0 {
        return Err(ArbError::AccountParse {
            dex: "address-lookup-table",
            reason: format!(
                "addresses section length {} not a multiple of 32",
                body.len()
            ),
        });
    }
    Ok(body.chunks_exact(PUBKEY_SIZE)
        .map(|chunk| Pubkey::new_from_array(chunk.try_into().unwrap()))
        .collect())
}

/// Fetch the ALT account from the RPC, decode the address list. Used at
/// scout startup; refreshing during a run is rare (ALTs are stable once
/// extended) but possible by calling this again.
///
/// Process-local cached with [`ALT_CACHE_TTL`] — see the cache comment
/// for why. Pass `ALT_NO_CACHE=1` to bypass for debugging.
pub async fn fetch_alt_addresses(
    rpc_url:      &str,
    alt_pubkey:   &Pubkey,
) -> ArbResult<Vec<Pubkey>> {
    let bypass = std::env::var("ALT_NO_CACHE").as_deref() == Ok("1");
    if !bypass {
        if let Ok(mut guard) = ALT_CACHE.lock() {
            let cache = guard.get_or_insert_with(HashMap::new);
            if let Some((t, addrs)) = cache.get(alt_pubkey) {
                if t.elapsed() < ALT_CACHE_TTL {
                    return Ok(addrs.clone());
                }
            }
        }
    }

    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getAccountInfo",
        "params": [
            alt_pubkey.to_string(),
            {"encoding": "base64", "commitment": "confirmed"},
        ],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await
        .map_err(|e| ArbError::Rpc(format!("getAccountInfo for ALT {alt_pubkey}: {e}")))?
        .json().await
        .map_err(|e| ArbError::Rpc(format!("getAccountInfo parse: {e}")))?;

    let data_b64 = resp.pointer("/result/value/data/0")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ArbError::Rpc(format!(
            "ALT {alt_pubkey} not found or missing data field; response: {resp}"
        )))?;
    let raw = base64::engine::general_purpose::STANDARD.decode(data_b64)
        .map_err(|e| ArbError::Rpc(format!("ALT data base64 decode: {e}")))?;

    let addrs = parse_alt_account(&raw)?;

    if !bypass {
        if let Ok(mut guard) = ALT_CACHE.lock() {
            let cache = guard.get_or_insert_with(HashMap::new);
            cache.insert(*alt_pubkey, (Instant::now(), addrs.clone()));
        }
    }
    Ok(addrs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_short_data() {
        let r = parse_alt_account(&[0u8; 30]);
        assert!(matches!(r, Err(ArbError::AccountParse { .. })));
    }

    #[test]
    fn parse_rejects_wrong_discriminator() {
        let mut data = vec![0u8; ALT_HEADER_SIZE];
        data[0] = 99;  // wrong discriminator
        let r = parse_alt_account(&data);
        assert!(matches!(r, Err(ArbError::AccountParse { .. })));
    }

    #[test]
    fn parse_empty_alt_returns_empty_vec() {
        let mut data = vec![0u8; ALT_HEADER_SIZE];
        data[0] = 1;  // activated discriminator
        let r = parse_alt_account(&data).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn parse_returns_addresses_in_storage_order() {
        let pk = |b: u8| Pubkey::new_from_array([b; 32]);
        let addrs = [pk(1), pk(2), pk(3)];

        let mut data = vec![0u8; ALT_HEADER_SIZE + addrs.len() * PUBKEY_SIZE];
        data[0] = 1;  // discriminator
        for (i, a) in addrs.iter().enumerate() {
            let off = ALT_HEADER_SIZE + i * PUBKEY_SIZE;
            data[off..off + PUBKEY_SIZE].copy_from_slice(a.as_ref());
        }
        let parsed = parse_alt_account(&data).unwrap();
        assert_eq!(parsed, addrs.to_vec());
    }

    #[test]
    fn parse_rejects_partial_pubkey_at_end() {
        let mut data = vec![0u8; ALT_HEADER_SIZE + 50];  // 50 not divisible by 32
        data[0] = 1;
        let r = parse_alt_account(&data);
        assert!(matches!(r, Err(ArbError::AccountParse { .. })));
    }
}
