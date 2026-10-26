//! Whirlpool program-emitted `Traded` event parser.
//!
//! When a Whirlpool swap completes, the on-chain program emits a `Traded`
//! Anchor event (NOT named "Swap" — that name is used by Raydium CLMM).
//! We can decode it from a transaction's program-log lines (delivered via
//! Yellowstone tx-stream ~50-150 ms before the corresponding account-write
//! update). That gives the scout a head-start on routes touching this pool
//! — without waiting for the cached pool state to be refreshed from RPC /
//! Geyser.
//!
//! ## Event format
//!
//! Anchor encodes events into the program's log stream as
//!
//! ```text
//! Program data: <base64>
//! ```
//!
//! Decoded, that's an 8-byte discriminator + Borsh-serialized fields.
//! The discriminator is `sha256("event:Traded")[..8]`.
//!
//! Source: <https://github.com/orca-so/whirlpools/blob/main/programs/whirlpool/src/events.rs>
//!
//! ## Fields parsed
//!
//! Only what the predictive cache needs — pool pubkey + post-swap
//! `sqrt_price`. The remaining fields (in/out amounts, transfer fees, lp
//! fee, protocol fee) are decodable but not used downstream.

use base64::Engine;
use solana_program::pubkey::Pubkey;

/// `sha256("event:Traded")[..8]`. Anchor discriminator for the Whirlpool
/// `Traded` event. NOTE: an earlier version used the wrong constant
/// (Raydium CLMM's `SwapEvent` discriminator, `[64,198,205,232,38,8,113,
/// 226]`) which silently failed to match any log line in production.
pub const TRADED_EVENT_DISCRIMINATOR: [u8; 8] = [225, 202, 73, 175, 147, 43, 160, 150];

#[derive(Debug, Clone)]
pub struct WhirlpoolTradedEvent {
    pub whirlpool:       Pubkey,
    pub post_sqrt_price: u128,
}

impl WhirlpoolTradedEvent {
    /// Try to parse a single log-message line. Returns `None` for any log
    /// that isn't a Whirlpool `Traded` event (most lines won't be).
    pub fn try_parse_from_log(line: &str) -> Option<Self> {
        let b64 = line.strip_prefix("Program data: ")?;
        let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
        if bytes.len() < 8 + 32 + 1 + 16 + 16 { return None; }
        if bytes[..8] != TRADED_EVENT_DISCRIMINATOR { return None; }

        // Borsh layout (after the 8-byte disc):
        //   whirlpool:           Pubkey (32)
        //   a_to_b:              bool   (1)
        //   pre_sqrt_price:      u128   (16)
        //   post_sqrt_price:     u128   (16)
        //   ... remaining fields not parsed
        let whirlpool_bytes: [u8; 32] = bytes[8..40].try_into().ok()?;
        // Skip a_to_b (1 byte) and pre_sqrt_price (16 bytes) → offset 8+32+1+16 = 57.
        let post_sqrt_bytes: [u8; 16] = bytes[57..73].try_into().ok()?;
        Some(Self {
            whirlpool:       Pubkey::new_from_array(whirlpool_bytes),
            post_sqrt_price: u128::from_le_bytes(post_sqrt_bytes),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_program_data_line() {
        assert!(WhirlpoolTradedEvent::try_parse_from_log("Program log: Instruction: Swap").is_none());
    }

    #[test]
    fn rejects_wrong_discriminator() {
        // 8-byte wrong disc + 50 bytes of zeros — base64-encode and feed.
        let mut payload = vec![0u8; 73];
        payload[..8].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&payload);
        let line = format!("Program data: {b64}");
        assert!(WhirlpoolTradedEvent::try_parse_from_log(&line).is_none());
    }

    #[test]
    fn rejects_raydium_clmm_swap_event_discriminator() {
        // Defends against the v59 regression: SwapEvent (CLMM) discriminator
        // must NOT parse as a Whirlpool Traded event.
        let raydium_clmm_disc: [u8; 8] = [64, 198, 205, 232, 38, 8, 113, 226];
        let mut payload = vec![0u8; 73];
        payload[..8].copy_from_slice(&raydium_clmm_disc);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&payload);
        let line = format!("Program data: {b64}");
        assert!(WhirlpoolTradedEvent::try_parse_from_log(&line).is_none());
    }

    #[test]
    fn parses_valid_event() {
        // Build a synthetic event payload.
        let whirlpool = [9u8; 32];
        let post_sqrt: u128 = 18_446_744_073_709_551_616_u128 * 2; // 2 * 2^64
        let mut payload = Vec::with_capacity(73);
        payload.extend_from_slice(&TRADED_EVENT_DISCRIMINATOR);
        payload.extend_from_slice(&whirlpool);
        payload.push(1u8);                       // a_to_b
        payload.extend_from_slice(&[0u8; 16]);   // pre_sqrt_price (unused)
        payload.extend_from_slice(&post_sqrt.to_le_bytes()); // post_sqrt_price
        let b64 = base64::engine::general_purpose::STANDARD.encode(&payload);
        let line = format!("Program data: {b64}");
        let ev = WhirlpoolTradedEvent::try_parse_from_log(&line).expect("parse ok");
        assert_eq!(ev.whirlpool, Pubkey::new_from_array(whirlpool));
        assert_eq!(ev.post_sqrt_price, post_sqrt);
    }
}
