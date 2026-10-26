//! ed25519 signing of Solana versioned transactions, without `solana-sdk`.
//!
//! `solana-sdk` brings in `openssl-sys` which fails to build on Windows
//! without manual MSVC + OpenSSL setup. We avoid the whole stack by
//! signing transactions byte-level using `ed25519-dalek` + manual
//! handling of Solana's transaction wire format.
//!
//! ## Solana versioned transaction wire format
//!
//! A serialized `VersionedTransaction` is:
//! 1. **Signatures** — `compact-u16(N)` followed by `N × 64` raw signature bytes
//! 2. **Message** — everything after the signatures section
//!
//! The message itself starts with either a legacy header (`0x00` byte not set)
//! or a v0 header (`0x80` byte at position 0). Either way, **the entire
//! message section is what gets signed**.
//!
//! Jupiter's `/v6/swap` returns a transaction with the right number of
//! signature slots already allocated (filled with zeros). We just need to
//! sign the message and inject our signature at the slot matching our
//! wallet's position in the account-keys list (typically slot 0 — the
//! fee payer is always the first signer).

use arb_core::{ArbError, ArbResult};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};

/// Decode a Solana compact-u16 length prefix. Returns `(value, bytes_consumed)`.
/// Format: 1-3 bytes, each carrying 7 bits + a continuation flag (`0x80`).
pub fn decode_compact_u16(data: &[u8]) -> Option<(u16, usize)> {
    let mut value: u32 = 0;
    let mut shift = 0u32;
    for (i, &b) in data.iter().take(3).enumerate() {
        value |= ((b & 0x7f) as u32) << shift;
        if b & 0x80 == 0 {
            return Some((value as u16, i + 1));
        }
        shift += 7;
    }
    None // overflow / malformed
}

/// Sign a Jupiter-returned swap transaction with the operator's keypair.
/// The transaction must already have a signature slot reserved for us at
/// `signer_index` (Jupiter sets `signer_index=0` since we're the fee payer).
///
/// `keypair_bytes` is the full 64-byte Solana keypair (`bytes[0..32]` =
/// secret seed, `bytes[32..64]` = public key — this matches the format
/// `solana-keygen new` writes and what Wallet::from_env() loads).
///
/// Returns the signed transaction as a base64 string ready for
/// `sendTransaction` or inclusion in a Jito bundle.
pub fn sign_versioned_transaction_b64(
    swap_tx_b64:   &str,
    keypair_bytes: &[u8; 64],
    signer_index:  usize,
) -> ArbResult<String> {
    let raw = base64::engine::general_purpose::STANDARD.decode(swap_tx_b64)
        .map_err(|e| ArbError::Config(format!("swap tx not base64: {e}")))?;
    let signed = sign_versioned_transaction_bytes(&raw, keypair_bytes, signer_index)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(signed))
}

/// Same as above but operates on raw bytes (no base64).
pub fn sign_versioned_transaction_bytes(
    raw:           &[u8],
    keypair_bytes: &[u8; 64],
    signer_index:  usize,
) -> ArbResult<Vec<u8>> {
    if raw.len() < 1 {
        return Err(ArbError::Config("swap tx is empty".into()));
    }

    // Step 1: parse the signatures section.
    let (sig_count, sig_count_len) = decode_compact_u16(raw)
        .ok_or_else(|| ArbError::Config("swap tx: malformed signatures length prefix".into()))?;
    let sig_count = sig_count as usize;

    if signer_index >= sig_count {
        return Err(ArbError::Config(format!(
            "signer_index {signer_index} out of range; tx expects {sig_count} signatures"
        )));
    }
    let sigs_section_end = sig_count_len + sig_count * 64;
    if raw.len() < sigs_section_end {
        return Err(ArbError::Config(format!(
            "swap tx truncated: needs {sigs_section_end} bytes for sigs, got {}", raw.len()
        )));
    }

    // Step 2: sign the message (everything after the signatures section).
    let message_bytes = &raw[sigs_section_end..];
    let secret_seed: [u8; 32] = keypair_bytes[0..32].try_into()
        .expect("slice of 32 from 64");
    let signing_key = SigningKey::from_bytes(&secret_seed);
    let signature   = signing_key.sign(message_bytes);

    // Sanity: the public key derived from the secret should match bytes[32..64].
    // If not, the keypair is malformed (or someone passed wrong bytes).
    let derived_pk = signing_key.verifying_key().to_bytes();
    if derived_pk != keypair_bytes[32..64] {
        return Err(ArbError::Config(
            "keypair public-key derivation mismatch — corrupt keypair".into()
        ));
    }

    // Step 3: build the output by replacing our signature slot.
    let mut out = raw.to_vec();
    let sig_offset = sig_count_len + signer_index * 64;
    out[sig_offset..sig_offset + 64].copy_from_slice(&signature.to_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_compact_u16_one_byte() {
        // values 0..=127 take 1 byte
        assert_eq!(decode_compact_u16(&[0]),    Some((0, 1)));
        assert_eq!(decode_compact_u16(&[1]),    Some((1, 1)));
        assert_eq!(decode_compact_u16(&[127]),  Some((127, 1)));
    }

    #[test]
    fn decode_compact_u16_two_bytes() {
        // 128 = 0b10000000_00000001 → bytes [0x80, 0x01]
        assert_eq!(decode_compact_u16(&[0x80, 0x01]), Some((128, 2)));
        // 16383 = 7-bit max in two bytes
        assert_eq!(decode_compact_u16(&[0xff, 0x7f]), Some((16383, 2)));
    }

    #[test]
    fn decode_compact_u16_three_bytes() {
        // 16384 = 0b00000001_00000000_00000000 → [0x80,0x80,0x01]
        assert_eq!(decode_compact_u16(&[0x80, 0x80, 0x01]), Some((16384, 3)));
        // 65535 = u16::MAX
        assert_eq!(decode_compact_u16(&[0xff, 0xff, 0x03]), Some((65535, 3)));
    }

    #[test]
    fn decode_compact_u16_malformed() {
        // four high-bit-set bytes → overflow
        assert_eq!(decode_compact_u16(&[0xff, 0xff, 0xff, 0xff]), None);
        // empty → no result
        assert_eq!(decode_compact_u16(&[]), None);
    }

    /// Sample 64-byte keypair (test vector — never used in production).
    fn test_keypair() -> [u8; 64] {
        let mut kp = [0u8; 64];
        let json = "[208,175,150,242,88,34,108,88,177,16,168,75,115,181,199,242,\
                     120,4,78,75,19,227,13,215,184,108,226,53,111,149,179,84,\
                     137,121,79,1,160,223,124,241,202,203,220,237,50,242,57,158,\
                     226,207,203,188,43,28,70,110,214,234,251,15,249,157,62,80]";
        let v: Vec<u8> = serde_json::from_str::<Vec<u8>>(json).unwrap();
        kp.copy_from_slice(&v);
        kp
    }

    #[test]
    fn sign_known_transaction_replaces_slot_0() {
        // Build a minimal transaction: 1 signature slot (zero), then "message"
        // bytes. We sign and verify the slot now contains a non-zero signature.
        let mut tx = Vec::new();
        tx.push(0x01);                 // compact-u16: 1 signature
        tx.extend_from_slice(&[0u8; 64]);  // signature placeholder
        tx.extend_from_slice(b"hello world - this is the message bytes to sign");

        let kp = test_keypair();
        let signed = sign_versioned_transaction_bytes(&tx, &kp, 0).unwrap();
        // First byte still says "1 signature"
        assert_eq!(signed[0], 0x01);
        // Signature slot is no longer all zeros
        assert!(signed[1..65].iter().any(|&b| b != 0));
        // Message section is unchanged
        assert_eq!(&signed[65..], &tx[65..]);
    }

    #[test]
    fn sign_rejects_truncated_tx() {
        let mut tx = Vec::new();
        tx.push(0x01);
        tx.extend_from_slice(&[0u8; 30]);  // only 30 bytes, not 64
        let kp = test_keypair();
        let r = sign_versioned_transaction_bytes(&tx, &kp, 0);
        assert!(r.is_err());
    }

    #[test]
    fn sign_rejects_signer_index_out_of_range() {
        let mut tx = Vec::new();
        tx.push(0x01);                 // only 1 signature slot
        tx.extend_from_slice(&[0u8; 64]);
        tx.extend_from_slice(b"msg");
        let kp = test_keypair();
        let r = sign_versioned_transaction_bytes(&tx, &kp, 5);  // index 5 of 1-slot tx
        assert!(r.is_err());
    }

    #[test]
    fn b64_round_trip_signs() {
        let mut tx = Vec::new();
        tx.push(0x01);
        tx.extend_from_slice(&[0u8; 64]);
        tx.extend_from_slice(b"the message");
        let b64 = base64::engine::general_purpose::STANDARD.encode(&tx);
        let kp = test_keypair();
        let signed_b64 = sign_versioned_transaction_b64(&b64, &kp, 0).unwrap();
        let signed_bytes = base64::engine::general_purpose::STANDARD.decode(&signed_b64).unwrap();
        assert!(signed_bytes[1..65].iter().any(|&b| b != 0));
    }
}
