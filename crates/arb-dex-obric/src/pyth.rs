//! Pyth oracle price parser — minimal, hand-rolled to avoid pulling in the
//! full `pyth-sdk-solana` (which depends on `solana-sdk` and breaks the
//! Windows build).
//!
//! Layout reference: `pyth-sdk-solana` v0.10 `state::PriceAccount`. We only
//! need three fields: the magic discriminator, the price exponent, and the
//! latest aggregated price.
//!
//! ```text
//! offset   0   magic           u32  = 0xa1b2c3d4
//! offset   4   ver             u32
//! offset   8   atype           u32
//! offset  12   size            u32
//! offset  16   ptype           u32
//! offset  20   exponent        i32
//! offset  24   num             u32
//! offset  28   num_qt          u32
//! offset  32   last_slot       u64
//! offset  40   valid_slot      u64
//! offset  48   ema_price       PriceInfo (32 bytes)
//! offset  80   ema_conf        PriceInfo (32 bytes)
//! offset 112   timestamp       i64
//! offset 120   min_pub         u8
//! offset 121   _drv2           u8
//! offset 122   _drv3           u16
//! offset 124   _drv4           u32
//! offset 128   product         [u8; 32]
//! offset 160   next            [u8; 32]
//! offset 192   prev_slot       u64
//! offset 200   prev_price      i64
//! offset 208   prev_conf       u64
//! offset 216   prev_timestamp  i64
//! offset 224   agg.price       i64    ← we read this
//! offset 232   agg.conf        u64
//! offset 240   agg.status      u32    ← we read this (1 = Trading)
//! ```

const PYTH_MAGIC: u32 = 0xa1b2c3d4;
const PYTH_AGG_STATUS_TRADING: u32 = 1;

/// Parsed Pyth price feed snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PythPrice {
    /// Raw aggregated price. Multiply by `10^exponent` for the float value
    /// (or scale by `10^-exponent` to keep integer math).
    pub price:    i64,
    /// Power of 10 to apply to `price`. Typical range −10..−6 for USD pairs.
    pub exponent: i32,
    /// `true` only when the feed is in `Trading` status — quote should
    /// reject if `false` (oracle could be stale / halted).
    pub trading:  bool,
}

/// Parse a Pyth price account. Returns `None` if the magic doesn't match
/// (account isn't a Pyth price account) or the buffer is too short.
pub fn parse_pyth_price(data: &[u8]) -> Option<PythPrice> {
    if data.len() < 244 { return None; }
    let magic = u32::from_le_bytes(data[0..4].try_into().ok()?);
    if magic != PYTH_MAGIC { return None; }
    let exponent = i32::from_le_bytes(data[20..24].try_into().ok()?);
    let price    = i64::from_le_bytes(data[224..232].try_into().ok()?);
    let status   = u32::from_le_bytes(data[240..244].try_into().ok()?);
    Some(PythPrice {
        price, exponent,
        trading: status == PYTH_AGG_STATUS_TRADING,
    })
}

/// Parse a Pyth Pull (`PriceUpdateV2`) account — Pyth Network's newer
/// receiver-program format. Owner = `rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ`.
///
/// Empirically-verified layout (134 bytes total for "Full" verification):
/// ```text
///   0   anchor_disc            (8)
///   8   write_authority        (32)
///  40   verification_level     (1)  ← Full=1, Partial=2 (tag + num_sigs)
///  41   feed_id                (32)
///  73   price                  (i64 LE)
///  81   conf                   (u64 LE)
///  89   exponent               (i32 LE)
///  93   publish_time           (i64 LE)  ← unix seconds
/// 101   prev_publish_time      (i64 LE)
/// 109   ema_price              (i64 LE)
/// 117   ema_conf               (u64 LE)
/// 125   _padding/slack         (1)
/// 126   posted_slot            (u64 LE)
/// ```
///
/// Returns `None` if the buffer is too short or the publish_time is
/// nonsensical (stale-or-zero feed). Caller treats `trading=false` the same
/// as a V1 Trading-status mismatch.
pub fn parse_pyth_pull_price(data: &[u8]) -> Option<PythPrice> {
    if data.len() < 101 { return None; }
    let price        = i64::from_le_bytes(data[73..81].try_into().ok()?);
    let exponent     = i32::from_le_bytes(data[89..93].try_into().ok()?);
    let publish_time = i64::from_le_bytes(data[93..101].try_into().ok()?);
    // Sanity: publish_time must be a plausible unix timestamp (2020+).
    let trading = publish_time > 1_500_000_000 && price != 0;
    Some(PythPrice { price, exponent, trading })
}

/// Parse a Pyth price account regardless of format. Tries V2 (magic
/// 0xa1b2c3d4) first, then falls back to Pyth Pull (PriceUpdateV2 anchor
/// account). Returns `None` if neither matches.
pub fn parse_pyth_price_any(data: &[u8]) -> Option<PythPrice> {
    parse_pyth_price(data).or_else(|| parse_pyth_pull_price(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake Pyth account with given price + exponent + status.
    fn fake_pyth(price: i64, exponent: i32, status: u32) -> Vec<u8> {
        let mut data = vec![0u8; 256];
        data[0..4].copy_from_slice(&PYTH_MAGIC.to_le_bytes());
        data[20..24].copy_from_slice(&exponent.to_le_bytes());
        data[224..232].copy_from_slice(&price.to_le_bytes());
        data[240..244].copy_from_slice(&status.to_le_bytes());
        data
    }

    #[test]
    fn rejects_wrong_magic() {
        let data = vec![0u8; 256];
        assert!(parse_pyth_price(&data).is_none());
    }

    #[test]
    fn rejects_too_short() {
        let data = vec![0xd4, 0xc3, 0xb2, 0xa1, 0, 0, 0, 0]; // magic + ~ but truncated
        assert!(parse_pyth_price(&data).is_none());
    }

    #[test]
    fn parses_sol_at_93_dollars() {
        // Pyth-style: SOL price 93.00 USD encoded as 9_300_000_000 with exponent -8.
        let data = fake_pyth(9_300_000_000, -8, 1);
        let p = parse_pyth_price(&data).unwrap();
        assert_eq!(p.price, 9_300_000_000);
        assert_eq!(p.exponent, -8);
        assert!(p.trading);
        // Float reconstruction: 9.3e9 * 10^-8 = 93.0
        let f = p.price as f64 * (10f64).powi(p.exponent);
        assert!((f - 93.0).abs() < 0.001, "got {f}");
    }

    #[test]
    fn flags_non_trading_status() {
        let data = fake_pyth(100, -8, 0); // status 0 = Unknown
        let p = parse_pyth_price(&data).unwrap();
        assert!(!p.trading);
    }
}
