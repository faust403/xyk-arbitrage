//! Build, serialize, and sign Solana legacy transactions WITHOUT `solana-sdk`.
//!
//! Phase D of the native-execution stack. The pieces:
//!
//! 1. **Instruction helpers**: ComputeBudget (priority fee), SystemProgram::Transfer
//!    (Jito tip), ATA derivation + create-idempotent.
//! 2. **`build_legacy_message`**: takes a payer + a slice of `Instruction`s + a
//!    recent blockhash, returns a `LegacyMessage` with deduplicated/sorted
//!    accounts and compiled instruction indices.
//! 3. **`serialize_message`**: emits the wire-format bytes (header + compact-u16
//!    arrays + Pubkeys + blockhash + compiled instructions).
//! 4. **`sign_legacy_tx_b64`**: ed25519-signs the serialized message and
//!    prepends the `compact-u16(1) + 64-byte sig` to produce a base64
//!    transaction ready for `sendTransaction` or a Jito bundle.
//! 5. **`fetch_latest_blockhash`**: tiny async RPC helper.
//!
//! ## Why legacy and not v0?
//!
//! v0 transactions add address-lookup-table support — useful for huge
//! txs that would otherwise exceed 1232 bytes. Our two-Raydium-swaps + tip
//! fits comfortably under that limit, so legacy keeps the encoding simpler
//! and skips the LUT-fetching dance entirely.
//!
//! ## Account-key ordering rule (do not "improve" without re-reading)
//!
//! Solana's runtime requires account keys in this exact order:
//! ```text
//! [payer, ...other writable signers, ...readonly signers,
//!         ...writable non-signers,  ...readonly non-signers]
//! ```
//! and the message header announces the boundaries. Each compiled instruction
//! references accounts by *index into this array*. Get this wrong and the
//! runtime rejects the tx with `MissingSignatureForFee` or worse,
//! silently mutates the wrong account.

use std::collections::HashMap;

use arb_core::{ArbError, ArbResult};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use crate::known::SYSTEM_PROGRAM;
use arb_dex_raydium::TOKEN_PROGRAM;

// ─── Program IDs ──────────────────────────────────────────────────────────

pub const COMPUTE_BUDGET_PROGRAM: Pubkey =
    solana_program::pubkey!("ComputeBudget111111111111111111111111111111");

pub const ATA_PROGRAM: Pubkey =
    solana_program::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// Token-2022 program — pump.fun mints (and many newer tokens) use this
/// instead of the standard SPL Token program. ATAs for Token-2022 mints
/// use this program as the seed (not the standard TOKEN_PROGRAM).
pub const TOKEN_2022_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

// ─── Instruction helpers ──────────────────────────────────────────────────

/// ComputeBudget::SetComputeUnitLimit(units). Discriminator = 2.
pub fn set_compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM, accounts: vec![], data }
}

/// ComputeBudget::SetComputeUnitPrice(micro_lamports_per_cu). Discriminator = 3.
/// Adds priority fee, paid to the validator (NOT the same as Jito tip).
pub fn set_compute_unit_price_ix(micro_lamports: u64) -> Instruction {
    let mut data = vec![3u8];
    data.extend_from_slice(&micro_lamports.to_le_bytes());
    Instruction { program_id: COMPUTE_BUDGET_PROGRAM, accounts: vec![], data }
}

/// SystemProgram::Transfer. Used for Jito tip transfers (and anything else).
/// Instruction layout: u32(2) + u64(lamports) = 12 bytes.
pub fn system_transfer_ix(from: Pubkey, to: Pubkey, lamports: u64) -> Instruction {
    let mut data = Vec::with_capacity(12);
    data.extend_from_slice(&2u32.to_le_bytes()); // Transfer discriminator
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: SYSTEM_PROGRAM,
        accounts: vec![
            AccountMeta::new(from, true),
            AccountMeta::new(to,   false),
        ],
        data,
    }
}

/// Derive the canonical Associated Token Account for `(owner, mint)`.
/// PDA: seeds = [owner, TOKEN_PROGRAM, mint], program = ATA_PROGRAM.
pub fn derive_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    ).0
}

/// Token-2022 variant. Same ATA program but the SEED uses Token-2022
/// instead of the standard SPL Token program, producing a different PDA.
pub fn derive_ata_2022(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_2022_PROGRAM.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    ).0
}

/// AssociatedTokenAccount::CreateIdempotent for a Token-2022 mint. The
/// account list is the same as the SPL-Token variant but slot 5 is the
/// Token-2022 program ID. The ATA address is derived with Token-2022 seed.
pub fn create_ata_idempotent_ix_2022(payer: Pubkey, owner: Pubkey, mint: Pubkey) -> Instruction {
    let ata = derive_ata_2022(&owner, &mint);
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(payer,                       true),
            AccountMeta::new(ata,                         false),
            AccountMeta::new_readonly(owner,              false),
            AccountMeta::new_readonly(mint,               false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM,     false),
            AccountMeta::new_readonly(TOKEN_2022_PROGRAM, false),
        ],
        data: vec![1u8],
    }
}

/// AssociatedTokenAccount::CreateIdempotent (discriminator = 1). Unlike
/// `Create` (= 0), this is a no-op if the ATA already exists — perfect for
/// bundles where we don't know upfront whether the destination ATA was
/// initialised. Costs ~2000 CUs when no-op, ~30000 CUs when it actually
/// creates.
pub fn create_ata_idempotent_ix(payer: Pubkey, owner: Pubkey, mint: Pubkey) -> Instruction {
    let ata = derive_ata(&owner, &mint);
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(payer,                   true),
            AccountMeta::new(ata,                     false),
            AccountMeta::new_readonly(owner,          false),
            AccountMeta::new_readonly(*mint_ref(&mint), false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM,  false),
        ],
        data: vec![1u8],
    }
}

// Tiny helper to take Pubkey by ref without importing extras.
fn mint_ref(p: &Pubkey) -> &Pubkey { p }

// ─── Compact-u16 encoder ──────────────────────────────────────────────────

/// Solana's variable-width length prefix used for arrays in tx wire format.
/// 1-3 bytes; each byte carries 7 bits + a continuation flag (0x80).
pub fn encode_compact_u16(n: u16) -> Vec<u8> {
    let n = n as u32;
    if n < 0x80 {
        vec![n as u8]
    } else if n < 0x4000 {
        vec![(n & 0x7f) as u8 | 0x80, ((n >> 7) & 0x7f) as u8]
    } else {
        vec![
            (n & 0x7f) as u8 | 0x80,
            ((n >> 7) & 0x7f) as u8 | 0x80,
            ((n >> 14) & 0x7f) as u8,
        ]
    }
}

// ─── Message types & serialization ───────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CompiledInstruction {
    pub program_id_index: u8,
    pub account_indices:  Vec<u8>,
    pub data:             Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct LegacyMessage {
    pub num_required_signatures:       u8,
    pub num_readonly_signed_accounts:  u8,
    pub num_readonly_unsigned_accounts:u8,
    pub account_keys:                  Vec<Pubkey>,
    pub recent_blockhash:              [u8; 32],
    pub instructions:                  Vec<CompiledInstruction>,
}

#[derive(Clone, Copy, Default)]
struct AccMeta { is_signer: bool, is_writable: bool }

/// Build a `LegacyMessage` from a payer + instructions + blockhash.
/// Performs account deduplication, signer/writable union across instructions,
/// and the canonical sort required by the Solana runtime.
pub fn build_legacy_message(
    payer:            &Pubkey,
    instructions:     &[Instruction],
    recent_blockhash: &[u8; 32],
) -> LegacyMessage {
    // Step 1: union account metadata across all instructions.
    let mut all: HashMap<Pubkey, AccMeta> = HashMap::new();
    all.insert(*payer, AccMeta { is_signer: true, is_writable: true });

    for ix in instructions {
        for am in &ix.accounts {
            let entry = all.entry(am.pubkey).or_default();
            if am.is_signer   { entry.is_signer   = true; }
            if am.is_writable { entry.is_writable = true; }
        }
        // Program IDs are referenced by index too, but they're always
        // readonly + non-signer. or_insert keeps existing entries unchanged.
        all.entry(ix.program_id).or_insert(AccMeta::default());
    }

    // Step 2: split into 4 buckets (excluding payer, who is forced to slot 0).
    let mut writable_signers:    Vec<Pubkey> = vec![];
    let mut readonly_signers:    Vec<Pubkey> = vec![];
    let mut writable_nonsigners: Vec<Pubkey> = vec![];
    let mut readonly_nonsigners: Vec<Pubkey> = vec![];
    for (pk, m) in all.iter() {
        if pk == payer { continue; }
        match (m.is_signer, m.is_writable) {
            (true,  true)  => writable_signers.push(*pk),
            (true,  false) => readonly_signers.push(*pk),
            (false, true)  => writable_nonsigners.push(*pk),
            (false, false) => readonly_nonsigners.push(*pk),
        }
    }
    // Sort each bucket — gives deterministic output and avoids the small
    // chance that HashMap iteration order changes a tx hash mid-flight.
    writable_signers.sort();
    readonly_signers.sort();
    writable_nonsigners.sort();
    readonly_nonsigners.sort();

    // Step 3: lay out account_keys in the runtime-required order.
    let mut account_keys = Vec::with_capacity(1 + all.len() - 1);
    account_keys.push(*payer);
    account_keys.extend(writable_signers.iter().copied());
    account_keys.extend(readonly_signers.iter().copied());
    account_keys.extend(writable_nonsigners.iter().copied());
    account_keys.extend(readonly_nonsigners.iter().copied());

    let num_required_signatures        = (1 + writable_signers.len() + readonly_signers.len()) as u8;
    let num_readonly_signed_accounts   = readonly_signers.len() as u8;
    let num_readonly_unsigned_accounts = readonly_nonsigners.len() as u8;

    // Step 4: compile each instruction by replacing pubkeys with their indices.
    let key_index: HashMap<Pubkey, u8> = account_keys.iter().enumerate()
        .map(|(i, k)| (*k, i as u8))
        .collect();
    let compiled = instructions.iter().map(|ix| {
        CompiledInstruction {
            program_id_index: *key_index.get(&ix.program_id)
                .expect("program id was inserted into all"),
            account_indices: ix.accounts.iter()
                .map(|am| *key_index.get(&am.pubkey).expect("account was inserted into all"))
                .collect(),
            data: ix.data.clone(),
        }
    }).collect();

    LegacyMessage {
        num_required_signatures,
        num_readonly_signed_accounts,
        num_readonly_unsigned_accounts,
        account_keys,
        recent_blockhash:    *recent_blockhash,
        instructions:        compiled,
    }
}

/// Serialize the message to the bytes the runtime hashes + signs.
pub fn serialize_message(msg: &LegacyMessage) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        3 + 3 + msg.account_keys.len() * 32 + 32 + 3 +
        msg.instructions.iter().map(|i| 4 + i.account_indices.len() + i.data.len()).sum::<usize>()
    );
    // Header
    buf.push(msg.num_required_signatures);
    buf.push(msg.num_readonly_signed_accounts);
    buf.push(msg.num_readonly_unsigned_accounts);
    // Account keys
    buf.extend(encode_compact_u16(msg.account_keys.len() as u16));
    for k in &msg.account_keys { buf.extend_from_slice(k.as_ref()); }
    // Recent blockhash
    buf.extend_from_slice(&msg.recent_blockhash);
    // Instructions
    buf.extend(encode_compact_u16(msg.instructions.len() as u16));
    for ix in &msg.instructions {
        buf.push(ix.program_id_index);
        buf.extend(encode_compact_u16(ix.account_indices.len() as u16));
        buf.extend_from_slice(&ix.account_indices);
        buf.extend(encode_compact_u16(ix.data.len() as u16));
        buf.extend_from_slice(&ix.data);
    }
    buf
}

/// Sign the serialized message and produce the full base64 tx wire bytes.
/// Only handles the single-signer case (our wallet pays + signs everything),
/// which covers our entire arb path.
pub fn sign_legacy_tx_b64(
    msg:           &LegacyMessage,
    keypair_bytes: &[u8; 64],
) -> ArbResult<String> {
    if msg.num_required_signatures != 1 {
        return Err(ArbError::Invariant(
            "sign_legacy_tx_b64: only single-signer txs supported (our wallet)"
        ));
    }
    let msg_bytes = serialize_message(msg);

    let secret_seed: [u8; 32] = keypair_bytes[0..32].try_into()
        .expect("slice of 32 from 64");
    let signing_key = SigningKey::from_bytes(&secret_seed);
    // Defence-in-depth: confirm the derived pubkey matches what the keypair
    // claims. If they disagree the keypair file is corrupt.
    if signing_key.verifying_key().to_bytes() != keypair_bytes[32..64] {
        return Err(ArbError::Invariant(
            "sign_legacy_tx_b64: keypair public-key derivation mismatch"
        ));
    }
    // Sanity: payer (account_keys[0]) must equal our verifying key.
    if msg.account_keys[0].to_bytes() != keypair_bytes[32..64] {
        return Err(ArbError::Invariant(
            "sign_legacy_tx_b64: payer (account_keys[0]) doesn't match keypair pubkey"
        ));
    }

    let signature = signing_key.sign(&msg_bytes);
    let mut tx = Vec::with_capacity(1 + 64 + msg_bytes.len());
    tx.extend(encode_compact_u16(1));        // 1 signature
    tx.extend_from_slice(&signature.to_bytes());
    tx.extend_from_slice(&msg_bytes);
    Ok(base64::engine::general_purpose::STANDARD.encode(tx))
}

/// One-call convenience: build, serialize, and sign.
pub fn build_and_sign_legacy_tx_b64(
    payer:            &Pubkey,
    instructions:     &[Instruction],
    recent_blockhash: &[u8; 32],
    keypair_bytes:    &[u8; 64],
) -> ArbResult<String> {
    let msg = build_legacy_message(payer, instructions, recent_blockhash);
    sign_legacy_tx_b64(&msg, keypair_bytes)
}

// ─── Blockhash fetch ──────────────────────────────────────────────────────

/// Process-local blockhash cache. Solana accepts a blockhash for ~150
/// slots (~75 s); we cap at 10 s here so we always submit a fresh-ish
/// one but spare Helius the ~3 calls/sec marginfi-hunter would otherwise
/// generate at peak. Set `BLOCKHASH_NO_CACHE=1` to bypass.
static BLOCKHASH_CACHE: std::sync::Mutex<Option<([u8; 32], std::time::Instant)>> =
    std::sync::Mutex::new(None);
const BLOCKHASH_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(10);

/// Fetch the latest blockhash from a Solana RPC. Confirmed commitment is
/// fresh enough — using `processed` risks landing on a fork.
///
/// Process-cached for [`BLOCKHASH_CACHE_TTL`]. Per-rpc-url caching is
/// not needed because every binary on this host uses the same Helius
/// endpoint; if that changes, add a HashMap<rpc_url, ...>.
pub async fn fetch_latest_blockhash(rpc_url: &str) -> ArbResult<[u8; 32]> {
    let bypass = std::env::var("BLOCKHASH_NO_CACHE").as_deref() == Ok("1");
    if !bypass {
        if let Ok(guard) = BLOCKHASH_CACHE.lock() {
            if let Some((bh, t)) = *guard {
                if t.elapsed() < BLOCKHASH_CACHE_TTL {
                    return Ok(bh);
                }
            }
        }
    }
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "getLatestBlockhash",
        "params": [{"commitment": "confirmed"}],
    });
    let resp: serde_json::Value = reqwest::Client::new()
        .post(rpc_url).json(&body).send().await
        .map_err(|e| ArbError::Rpc(format!("getLatestBlockhash: {e}")))?
        .json().await
        .map_err(|e| ArbError::Rpc(format!("getLatestBlockhash parse: {e}")))?;
    let bh_b58 = resp.pointer("/result/value/blockhash")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ArbError::Rpc(format!("missing blockhash in response: {resp}")))?;
    let decoded = bs58::decode(bh_b58).into_vec()
        .map_err(|e| ArbError::Rpc(format!("blockhash bs58 decode: {e}")))?;
    if decoded.len() != 32 {
        return Err(ArbError::Rpc(format!("blockhash wrong length: {} bytes", decoded.len())));
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&decoded);
    if !bypass {
        if let Ok(mut guard) = BLOCKHASH_CACHE.lock() {
            *guard = Some((buf, std::time::Instant::now()));
        }
    }
    Ok(buf)
}

// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }

    #[test]
    fn compact_u16_round_trip_small() {
        for v in [0u16, 1, 64, 127] {
            let bytes = encode_compact_u16(v);
            assert_eq!(bytes.len(), 1, "v={v} should be 1 byte");
            // Decode using the existing decoder in signing.rs
            let (decoded, n) = crate::signing::decode_compact_u16(&bytes).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(n, bytes.len());
        }
    }

    #[test]
    fn compact_u16_round_trip_medium() {
        for v in [128u16, 200, 16383] {
            let bytes = encode_compact_u16(v);
            assert_eq!(bytes.len(), 2, "v={v} should be 2 bytes");
            let (decoded, n) = crate::signing::decode_compact_u16(&bytes).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(n, bytes.len());
        }
    }

    #[test]
    fn compact_u16_round_trip_large() {
        for v in [16384u16, 30000, 65535] {
            let bytes = encode_compact_u16(v);
            assert_eq!(bytes.len(), 3, "v={v} should be 3 bytes");
            let (decoded, n) = crate::signing::decode_compact_u16(&bytes).unwrap();
            assert_eq!(decoded, v);
            assert_eq!(n, bytes.len());
        }
    }

    #[test]
    fn system_transfer_ix_data_format() {
        let ix = system_transfer_ix(pk(1), pk(2), 5_000);
        assert_eq!(ix.data.len(), 12);
        assert_eq!(&ix.data[0..4], &2u32.to_le_bytes());     // discriminator
        assert_eq!(&ix.data[4..12], &5_000u64.to_le_bytes()); // amount
        assert_eq!(ix.accounts.len(), 2);
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[0].is_writable);
        assert!(!ix.accounts[1].is_signer);
        assert!(ix.accounts[1].is_writable);
    }

    #[test]
    fn compute_budget_ix_format() {
        let limit = set_compute_unit_limit_ix(250_000);
        assert_eq!(limit.data, &[2u8, 0x90, 0xd0, 0x03, 0x00]);
        let price = set_compute_unit_price_ix(1_000);
        assert_eq!(price.data[0], 3u8);
        assert_eq!(&price.data[1..], &1_000u64.to_le_bytes());
    }

    #[test]
    fn derive_ata_is_deterministic() {
        let a1 = derive_ata(&pk(7), &pk(8));
        let a2 = derive_ata(&pk(7), &pk(8));
        assert_eq!(a1, a2);
        // Different inputs → different outputs
        let a3 = derive_ata(&pk(7), &pk(9));
        assert_ne!(a1, a3);
    }

    #[test]
    fn build_message_payer_is_first_writable_signer() {
        let payer = pk(100);
        let other = pk(50);
        let ix = system_transfer_ix(payer, other, 1);
        let msg = build_legacy_message(&payer, &[ix], &[0u8; 32]);
        assert_eq!(msg.account_keys[0], payer);
        assert_eq!(msg.num_required_signatures, 1);
        // `other` is writable non-signer, so should appear before any program ids
        let other_idx = msg.account_keys.iter().position(|k| *k == other).unwrap();
        let sys_idx = msg.account_keys.iter().position(|k| *k == SYSTEM_PROGRAM).unwrap();
        assert!(other_idx < sys_idx, "writable non-signer comes before readonly");
    }

    #[test]
    fn build_message_dedupes_accounts_used_in_multiple_ixs() {
        let payer = pk(100);
        let recipient = pk(50);
        let ix1 = system_transfer_ix(payer, recipient, 100);
        let ix2 = system_transfer_ix(payer, recipient, 200);
        let msg = build_legacy_message(&payer, &[ix1, ix2], &[0u8; 32]);
        // payer appears once, recipient once, SYSTEM_PROGRAM once = 3 total.
        assert_eq!(msg.account_keys.len(), 3);
        // Both compiled instructions reference the SAME indices (since the
        // accounts are deduped).
        assert_eq!(msg.instructions[0].account_indices, msg.instructions[1].account_indices);
    }

    #[test]
    fn build_message_header_counts_match_layout() {
        // Single transfer: payer is sole writable signer, recipient is
        // writable non-signer, system is readonly non-signer.
        let payer = pk(1);
        let to    = pk(2);
        let ix = system_transfer_ix(payer, to, 1);
        let msg = build_legacy_message(&payer, &[ix], &[0u8; 32]);
        assert_eq!(msg.num_required_signatures,        1);
        assert_eq!(msg.num_readonly_signed_accounts,   0);
        assert_eq!(msg.num_readonly_unsigned_accounts, 1); // SYSTEM_PROGRAM
    }

    #[test]
    fn serialize_message_is_byte_stable() {
        let payer = pk(1);
        let ix = system_transfer_ix(payer, pk(2), 100);
        let msg = build_legacy_message(&payer, &[ix], &[7u8; 32]);
        let s1 = serialize_message(&msg);
        let s2 = serialize_message(&msg);
        assert_eq!(s1, s2, "serialization must be deterministic");
        // Sanity: header is the first 3 bytes
        assert_eq!(s1[0], msg.num_required_signatures);
        assert_eq!(s1[1], msg.num_readonly_signed_accounts);
        assert_eq!(s1[2], msg.num_readonly_unsigned_accounts);
    }

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
    fn sign_and_serialize_produces_valid_b64() {
        let kp = test_keypair();
        let payer = Pubkey::new_from_array(kp[32..64].try_into().unwrap());
        let ix = system_transfer_ix(payer, pk(50), 100);
        let b64 = build_and_sign_legacy_tx_b64(
            &payer, &[ix], &[0u8; 32], &kp,
        ).expect("sign");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&b64).expect("decode");
        // Must start with compact-u16(1) = 0x01
        assert_eq!(bytes[0], 0x01);
        // Followed by 64-byte non-zero signature
        assert!(bytes[1..65].iter().any(|b| *b != 0));
    }

    #[test]
    fn sign_rejects_payer_mismatch() {
        let kp = test_keypair();
        let bogus_payer = pk(123); // not the real public key
        let ix = system_transfer_ix(bogus_payer, pk(50), 100);
        let r = build_and_sign_legacy_tx_b64(
            &bogus_payer, &[ix], &[0u8; 32], &kp,
        );
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn create_ata_idempotent_has_correct_accounts() {
        let payer = pk(1);
        let owner = pk(2);
        let mint  = pk(3);
        let ix = create_ata_idempotent_ix(payer, owner, mint);
        assert_eq!(ix.program_id, ATA_PROGRAM);
        assert_eq!(ix.data, vec![1u8]);
        assert_eq!(ix.accounts.len(), 6);
        // First account: payer (signer + writable)
        assert!(ix.accounts[0].is_signer);
        assert!(ix.accounts[0].is_writable);
        // ATA itself is at index 1, writable
        assert!(!ix.accounts[1].is_signer);
        assert!(ix.accounts[1].is_writable);
        assert_eq!(ix.accounts[1].pubkey, derive_ata(&owner, &mint));
    }
}
