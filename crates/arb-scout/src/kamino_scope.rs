//! Kamino Scope (oracle aggregator) account parser.
//!
//! Scope is Kamino's in-house oracle aggregation program (program id
//! `HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ`). It maintains a single
//! large account (~28 KB) containing an array of ~512 `DatedPrice`
//! entries; each reserve points at this account via
//! `TokenInfo.scope_price_feed` plus a 4-element `scope_chain` of
//! `u16` indices.
//!
//! ## Why we need this
//!
//! Of the 462 Kamino reserves, only ~6 use Pyth feeds directly. The rest
//! (200+ relevant ones) price via Scope. Subscribing to the reserve
//! account is a reactive backstop — it only triggers AFTER someone calls
//! `refresh_reserve`. By subscribing to the Scope PRICES account itself
//! AND parsing it, we get a PREDICTIVE signal: as soon as Scope ticks
//! we can compute the new `market_price_sf` Kamino will compute on the
//! NEXT `refresh_reserve` call. That window (Scope publish → refresh)
//! can be 30 s to minutes on quiet reserves — exactly the window where
//! the bot would otherwise lose the race.
//!
//! ## On-chain layout (verified live 2026-05-16)
//!
//! ```text
//!   bytes 0..8         anchor discriminator
//!   bytes 8..N         array of DatedPrice (stride = 48 bytes)
//!
//! DatedPrice = 48 bytes:
//!   0..8     value:               u64 LE
//!   8..16    exp:                 u64 LE   ← we cap to u8 range (≤30)
//!   16..24   last_updated_slot:   u64 LE
//!   24..32   unix_timestamp:      u64 LE
//!   32..48   _reserved:           [u8; 16]
//! ```
//!
//! Total account data length observed: 28 712 bytes. (28 712 − 8) / 48 ≈
//! 598 entries; we cap iteration at the buffer length to stay safe even
//! if Scope ever grows the table.
//!
//! USD price = `value / 10^exp`. In our SF=2^60 USD-per-whole-token
//! scale (the same Kamino uses for `reserve.market_price_sf`),
//! `market_price_sf = value × 2^60 / 10^exp`.
//!
//! ## Composite (chained) prices
//!
//! `scope_chain: [u16; 4]` per reserve. Non-sentinel entries are
//! token IDs to look up in the same array. When multiple entries are
//! present, Kamino multiplies them (e.g. `kSOL_USD = kSOL_per_SOL ×
//! SOL_USD`). v1 of this parser handles single-entry chains directly;
//! multi-entry chains fall through to `r.market_price_sf` (the reserve's
//! own cached value), which is reactively-fresh via the reserve sub.

use solana_program::pubkey::Pubkey;

/// Scope program id (the aggregator program, NOT the Kamino lend program).
pub const KAMINO_SCOPE_PROGRAM: Pubkey =
    solana_program::pubkey!("HFn8GnPADiny6XqUoWE8uRPPxb29ikn4yTuPa9MF2fWJ");

const DATED_PRICE_STRIDE: usize = 48;
const PRICES_ARRAY_OFFSET: usize = 8;  // anchor disc

/// Sentinel value in `scope_chain` indicating "no further index".
pub const CHAIN_SENTINEL: u16 = u16::MAX;

#[derive(Debug, Clone, Copy)]
pub struct ScopePrice {
    pub value:             u64,
    pub exp:               u8,
    pub last_updated_slot: u64,
    pub unix_timestamp:    u64,
}

/// Parse a single `DatedPrice` at `prices[index]`. Returns None when the
/// entry is missing, malformed, or never initialised.
///
/// Sanity gates:
///  - `exp > 30`: Scope's real entries cap at ~18; anything larger means
///    we've mis-located the entry or hit padding bytes.
///  - `last_updated_slot == 0`: the slot is set on first publish; a zero
///    here means the index has never received a price. The value field
///    in this case can hold arbitrary metadata (we saw value=
///    1_778_902_201 exp=0 on cbBTC's chain[0]=175 — a unix timestamp,
///    not a price — which when interpreted as USD/whole-token inflates
///    cbBTC pricing by 22,000× and produces massive false-positive
///    liquidation signals).
pub fn parse_scope_price_at(data: &[u8], index: u16) -> Option<ScopePrice> {
    let off = PRICES_ARRAY_OFFSET + (index as usize) * DATED_PRICE_STRIDE;
    if off + DATED_PRICE_STRIDE > data.len() { return None; }
    let value = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
    let exp_u64 = u64::from_le_bytes(data[off + 8..off + 16].try_into().ok()?);
    if exp_u64 > 30 { return None; }
    let last_updated_slot =
        u64::from_le_bytes(data[off + 16..off + 24].try_into().ok()?);
    if last_updated_slot == 0 { return None; }
    let unix_timestamp =
        u64::from_le_bytes(data[off + 24..off + 32].try_into().ok()?);
    Some(ScopePrice {
        value,
        exp: exp_u64 as u8,
        last_updated_slot,
        unix_timestamp,
    })
}

/// Walk a `scope_chain` and return the FIRST non-sentinel index. v1
/// of the parser uses just this entry; multi-entry composite chains
/// (kSOL etc) fall back to the reserve's cached price.
pub fn primary_chain_index(chain: &[u16; 4]) -> Option<u16> {
    for &id in chain {
        if id != CHAIN_SENTINEL { return Some(id); }
    }
    None
}

/// Count the non-sentinel entries — used to decide whether to apply our
/// predictive override (chain length 1) or skip and let the reactive
/// reserve-sub path handle it (chain length >1).
pub fn chain_length(chain: &[u16; 4]) -> usize {
    chain.iter().take_while(|&&id| id != CHAIN_SENTINEL).count()
}

/// Convert a Scope `(value, exp)` price into Kamino's
/// `market_price_sf` format = USD per WHOLE token × 2^60.
///
///   usd_per_token = value / 10^exp
///   market_price_sf = usd_per_token × 2^60 = value × 2^60 / 10^exp
///
/// Verified against live USDC reserve 2026-05-16: Scope entry value=
/// 100_000_000 exp=8 → market_price_sf = 1e8 × 2^60 / 1e8 = 2^60 = 1.15e18,
/// which matches the reserve's stored `market_price_sf=1152694379070439427`
/// (modulo Scope's micro-deviation from $1.0).
pub fn scope_to_market_price_sf(value: u64, exp: u8) -> Option<u128> {
    let sf_scale: u128 = 1u128 << 60;
    let pow = 10u128.checked_pow(exp as u32)?;
    (value as u128).checked_mul(sf_scale).map(|v| v / pow)
}

/// Compute market_price_sf from a Scope chain — handles composite chains
/// (kSOL = SOL_USD × kSOL_per_SOL etc.) by multiplying all non-sentinel
/// price entries together.
///
/// Math: each entry contributes `value_i / 10^exp_i` (USD per WHOLE token).
///   total_usd_per_whole = product(value_i / 10^exp_i)
///                       = product(value_i) / 10^sum(exp_i)
///   market_price_sf     = total × 2^60 = product(value_i) × 2^60 / 10^sum(exp_i)
///
/// Overflow strategy: u128 holds ~3.4e38. For a 4-chain of `value_i ≤ 2^64`,
/// the raw product can be up to ~2^256 — too big. We multiply sequentially
/// and divide by 10^exp_i at each step to keep the running magnitude
/// bounded, then apply the final 2^60 scale at the end. The first
/// multiplication seeds the accumulator with `value_0 × 2^60`; subsequent
/// entries do `acc × value_i / 10^exp_i`.
///
/// Returns None when:
///   - chain is empty (all sentinels)
///   - any entry doesn't parse (exp implausible / offset OOB)
///   - intermediate arithmetic overflows
pub fn fresh_market_price_sf_from_scope(
    data:  &[u8],
    chain: &[u16; 4],
) -> Option<u128> {
    let sf_scale: u128 = 1u128 << 60;
    let mut acc: u128 = sf_scale;     // seed: 1.0 in SF=2^60
    let mut seen_any = false;
    for &id in chain {
        if id == CHAIN_SENTINEL { break; }
        let p = parse_scope_price_at(data, id)?;
        if p.value == 0 { return None; }
        seen_any = true;
        // acc = acc × value / 10^exp
        let pow = 10u128.checked_pow(p.exp as u32)?;
        acc = acc.checked_mul(p.value as u128)?;
        acc /= pow;
    }
    if !seen_any { return None; }
    Some(acc)
}

// ─── Tests ──────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_entry(value: u64, exp: u64, slot: u64, ts: u64) -> [u8; 48] {
        let mut out = [0u8; 48];
        out[0..8].copy_from_slice(&value.to_le_bytes());
        out[8..16].copy_from_slice(&exp.to_le_bytes());
        out[16..24].copy_from_slice(&slot.to_le_bytes());
        out[24..32].copy_from_slice(&ts.to_le_bytes());
        out
    }

    fn synth_prices(entries: &[[u8; 48]]) -> Vec<u8> {
        let mut buf = vec![0u8; 8];
        for e in entries { buf.extend_from_slice(e); }
        buf
    }

    #[test]
    fn parses_usdc_at_index_13() {
        // Pad first 13 entries with zero, then USDC at index 13.
        let mut entries = vec![[0u8; 48]; 13];
        entries.push(mk_entry(100_000_000, 8, 420_000_000, 1_700_000_000));
        let buf = synth_prices(&entries);
        let p = parse_scope_price_at(&buf, 13).unwrap();
        assert_eq!(p.value, 100_000_000);
        assert_eq!(p.exp, 8);
    }

    #[test]
    fn scope_to_sf_matches_kamino_usdc() {
        // value=1e8 exp=8 → $1.0 per token → 2^60 in SF.
        let sf = scope_to_market_price_sf(100_000_000, 8).unwrap();
        assert_eq!(sf, 1u128 << 60);
    }

    #[test]
    fn scope_to_sf_matches_kamino_sol() {
        // value=8_921_168_526 exp=8 → $89.21 per SOL.
        let sf = scope_to_market_price_sf(8_921_168_526, 8).unwrap();
        let usd = (sf as f64) / ((1u128 << 60) as f64);
        assert!((usd - 89.21).abs() < 0.01, "got ${usd}");
    }

    #[test]
    fn primary_chain_picks_first_non_sentinel() {
        assert_eq!(primary_chain_index(&[13, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL]), Some(13));
        assert_eq!(primary_chain_index(&[CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL]), None);
    }

    #[test]
    fn chain_length_counts_non_sentinels() {
        assert_eq!(chain_length(&[13, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL]), 1);
        assert_eq!(chain_length(&[1, 2, CHAIN_SENTINEL, CHAIN_SENTINEL]), 2);
        assert_eq!(chain_length(&[CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL]), 0);
    }

    #[test]
    fn rejects_implausible_exponent() {
        // exp=64 (way too big) — likely we mis-located the entry
        let entries = vec![mk_entry(123, 64, 0, 0)];
        let buf = synth_prices(&entries);
        assert!(parse_scope_price_at(&buf, 0).is_none());
    }

    #[test]
    fn end_to_end_fresh_market_price_sf() {
        let mut entries = vec![[0u8; 48]; 13];
        entries.push(mk_entry(100_000_000, 8, 420_000_000, 1_700_000_000));
        let buf = synth_prices(&entries);
        let chain = [13, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL];
        let sf = fresh_market_price_sf_from_scope(&buf, &chain).unwrap();
        assert_eq!(sf, 1u128 << 60);
    }

    #[test]
    fn composite_chain_multiplies_prices() {
        // Synthesise: index 0 = SOL @ $100 (value=10_000_000_000, exp=8)
        //             index 1 = kSOL = 1.05 SOL (value=1_050_000_000_000_000_000, exp=18)
        // Composite kSOL_USD = $100 × 1.05 = $105 per whole kSOL.
        // slot must be non-zero for the entry to be considered initialised.
        let entries = vec![
            mk_entry(10_000_000_000, 8, 420_000_000, 1_700_000_000),
            mk_entry(1_050_000_000_000_000_000, 18, 420_000_000, 1_700_000_000),
        ];
        let buf = synth_prices(&entries);
        let chain = [0, 1, CHAIN_SENTINEL, CHAIN_SENTINEL];
        let sf = fresh_market_price_sf_from_scope(&buf, &chain).unwrap();
        // Expect $105 × 2^60. Tolerate ±1 unit for integer-division rounding.
        let expected = 105u128 * (1u128 << 60);
        let diff = if sf > expected { sf - expected } else { expected - sf };
        assert!(diff < 10, "got {sf}, expected {expected} (diff {diff})");
    }

    #[test]
    fn single_chain_still_works_after_composite_refactor() {
        let mut entries = vec![[0u8; 48]; 3];
        entries.push(mk_entry(8_921_168_526, 8, 420_000_000, 1_700_000_000));  // index 3 = SOL @ $89.21
        let buf = synth_prices(&entries);
        let chain = [3, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL];
        let sf = fresh_market_price_sf_from_scope(&buf, &chain).unwrap();
        let usd = (sf as f64) / ((1u128 << 60) as f64);
        assert!((usd - 89.21).abs() < 0.01, "got ${usd}");
    }

    #[test]
    fn rejects_uninitialised_entry_slot_zero() {
        // Mimic cbBTC chain[0]=175 where Scope index has value=unix_ts,
        // exp=0, slot=0 (never published). Must NOT return a price.
        let entries = vec![mk_entry(1_778_902_201, 0, /* slot */ 0, 0)];
        let buf = synth_prices(&entries);
        assert!(parse_scope_price_at(&buf, 0).is_none(),
            "uninitialised entry (slot=0) must be rejected");
        let chain = [0, CHAIN_SENTINEL, CHAIN_SENTINEL, CHAIN_SENTINEL];
        assert!(fresh_market_price_sf_from_scope(&buf, &chain).is_none(),
            "composite/single chain must reject slot=0 entries");
    }

    #[test]
    fn empty_chain_returns_none() {
        let buf = synth_prices(&[mk_entry(100, 8, 0, 0)]);
        let chain = [CHAIN_SENTINEL; 4];
        assert!(fresh_market_price_sf_from_scope(&buf, &chain).is_none());
    }
}
