//! Build, serialize, and sign Solana **v0** (versioned) transactions WITHOUT
//! `solana-sdk`, with single-ALT (Address Lookup Table) compression.
//!
//! Sister module to `raw_tx.rs` which handles legacy transactions. The two
//! share the instruction helpers, compact-u16 encoder, and ed25519 signer;
//! only the message structure differs.
//!
//! ## Why we need this
//!
//! Flash-loan-wrapped arb cycles exceed Solana's 1232-byte legacy-tx limit
//! (~1700-2000 bytes raw). ALT compression replaces a 32-byte pubkey with a
//! 1-byte index for any account that's been pre-published in an on-chain
//! lookup table — typically the SPL Token program, system program, USDC mint,
//! Raydium AMM authority PDA, the 8 Jito tip accounts, and similar always-
//! reused addresses. With ~15 such accounts in an ALT we drop ~430 bytes
//! per tx, comfortably under the limit.
//!
//! ## Wire format reference
//!
//! ```text
//! v0 transaction:
//!   [compact-u16: signatures count] [signatures: 64 bytes each]
//!   [version byte: 0x80]
//!   [num_required_signatures]
//!   [num_readonly_signed_accounts]
//!   [num_readonly_unsigned_accounts]
//!   [compact-u16: static account_keys count] [static keys: 32 bytes each]
//!   [recent_blockhash: 32 bytes]
//!   [compact-u16: instructions count] [instructions...]
//!   [compact-u16: address_table_lookups count]
//!     [for each lookup:
//!       [account_key: 32 bytes — the ALT's address]
//!       [compact-u16: writable_indexes count] [writable_indexes: u8 each]
//!       [compact-u16: readonly_indexes count] [readonly_indexes: u8 each]
//!     ]
//! ```
//!
//! Compiled instruction account indices in v0 reference the synthetic list
//! `[static_keys ++ alt_writable_resolved ++ alt_readonly_resolved]`.
//!
//! ## Constraints (do not "improve" without re-reading)
//!
//! * **Signers must be static.** ALTs don't store signer flags. Any account
//!   that appears as a signer in any instruction goes into `static_keys`.
//! * **Program ids must be static.** The runtime dispatches programs by
//!   index into static keys only.
//! * **Payer is `static_keys[0]`.** Solana convention.
//! * **Single signer.** This module only supports the single-payer-signs-
//!   everything case (matches our entire arb path).

use std::collections::HashMap;

use arb_core::{ArbError, ArbResult};
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::raw_tx::{encode_compact_u16, CompiledInstruction};

/// One ALT participation in a v0 message: which writable and readonly
/// addresses we're pulling out of the ALT, by their byte indices in the
/// ALT's stored address list.
#[derive(Debug, Clone)]
pub struct AddressTableLookup {
    pub account_key:      Pubkey,
    pub writable_indexes: Vec<u8>,
    pub readonly_indexes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct V0Message {
    pub num_required_signatures:        u8,
    pub num_readonly_signed_accounts:   u8,
    pub num_readonly_unsigned_accounts: u8,
    pub static_account_keys:            Vec<Pubkey>,
    pub recent_blockhash:               [u8; 32],
    pub instructions:                   Vec<CompiledInstruction>,
    pub address_table_lookups:          Vec<AddressTableLookup>,
}

#[derive(Clone, Copy, Default)]
struct AccMeta { is_signer: bool, is_writable: bool }

/// Build a `V0Message` from a payer + instructions + blockhash + a single ALT
/// description (`alt_pubkey` + the address list it stores).
///
/// Accounts that appear in the ALT are pulled out into the lookup section.
/// Everything else (signers, program ids, accounts not in the ALT) stays in
/// `static_account_keys`. Pass `alt_addresses = &[]` to fall through to a
/// pure-static layout (equivalent to legacy but with the v0 wrapper).
///
/// `force_static` is a list of pubkeys that MUST stay in `static_account_keys`
/// even when present in the ALT. Required for Jito tip accounts: Jito's
/// auction filter only inspects static keys when checking for the
/// "Bundles must write lock at least one tip account" requirement; an
/// ALT-resolved tip is invisible to it and causes auction rejection.
pub fn build_v0_message(
    payer:            &Pubkey,
    instructions:     &[Instruction],
    recent_blockhash: &[u8; 32],
    alt_pubkey:       &Pubkey,
    alt_addresses:    &[Pubkey],
    force_static:     &[Pubkey],
) -> V0Message {
    // Step 1: union account metadata across all instructions (same as legacy).
    let mut all: HashMap<Pubkey, AccMeta> = HashMap::new();
    all.insert(*payer, AccMeta { is_signer: true, is_writable: true });
    let mut program_ids: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();

    for ix in instructions {
        for am in &ix.accounts {
            let entry = all.entry(am.pubkey).or_default();
            if am.is_signer   { entry.is_signer   = true; }
            if am.is_writable { entry.is_writable = true; }
        }
        all.entry(ix.program_id).or_insert(AccMeta::default());
        program_ids.insert(ix.program_id);
    }

    // Step 2: for each non-payer, non-signer, non-program-id account, decide
    // whether it's resolvable from the ALT.
    let alt_index: HashMap<Pubkey, u8> = alt_addresses.iter().enumerate()
        .map(|(i, k)| (*k, i as u8))
        .collect();
    let force_static_set: std::collections::HashSet<Pubkey> = force_static.iter().copied().collect();

    let mut static_writable_signers:    Vec<Pubkey> = vec![];
    let mut static_readonly_signers:    Vec<Pubkey> = vec![];
    let mut static_writable_nonsigners: Vec<Pubkey> = vec![];
    let mut static_readonly_nonsigners: Vec<Pubkey> = vec![];

    let mut alt_writable_resolved: Vec<Pubkey> = vec![];
    let mut alt_readonly_resolved: Vec<Pubkey> = vec![];
    let mut alt_writable_indexes:  Vec<u8>     = vec![];
    let mut alt_readonly_indexes:  Vec<u8>     = vec![];

    for (pk, m) in all.iter() {
        if pk == payer                     { continue; }     // forced static slot 0
        if m.is_signer                     {
            // Signers always static — ALT can't resolve signer flags.
            if m.is_writable { static_writable_signers.push(*pk); }
            else              { static_readonly_signers.push(*pk); }
            continue;
        }
        if program_ids.contains(pk)        {
            // Program ids always static — runtime dispatches by static index.
            static_readonly_nonsigners.push(*pk);
            continue;
        }
        if force_static_set.contains(pk)   {
            // Caller-pinned static. Jito tip accounts use this so the
            // auction filter sees them in static_account_keys.
            if m.is_writable { static_writable_nonsigners.push(*pk); }
            else              { static_readonly_nonsigners.push(*pk); }
            continue;
        }
        // Eligible for ALT resolution. Check membership.
        if let Some(&idx) = alt_index.get(pk) {
            if m.is_writable {
                alt_writable_resolved.push(*pk);
                alt_writable_indexes.push(idx);
            } else {
                alt_readonly_resolved.push(*pk);
                alt_readonly_indexes.push(idx);
            }
        } else {
            // Not in ALT → stays static.
            if m.is_writable { static_writable_nonsigners.push(*pk); }
            else              { static_readonly_nonsigners.push(*pk); }
        }
    }

    // Sort each static bucket for deterministic output (same reason as legacy).
    static_writable_signers.sort();
    static_readonly_signers.sort();
    static_writable_nonsigners.sort();
    static_readonly_nonsigners.sort();
    // ALT-resolved lists keep their pubkeys+indexes paired; sort by index so
    // identical inputs produce identical wire bytes.
    let mut alt_w: Vec<(u8, Pubkey)> = alt_writable_indexes.into_iter()
        .zip(alt_writable_resolved.into_iter()).collect();
    alt_w.sort_by_key(|(i, _)| *i);
    let mut alt_r: Vec<(u8, Pubkey)> = alt_readonly_indexes.into_iter()
        .zip(alt_readonly_resolved.into_iter()).collect();
    alt_r.sort_by_key(|(i, _)| *i);

    // Step 3: assemble static_account_keys in runtime-required order.
    let mut static_keys = Vec::with_capacity(
        1 + static_writable_signers.len() + static_readonly_signers.len()
        + static_writable_nonsigners.len() + static_readonly_nonsigners.len()
    );
    static_keys.push(*payer);
    static_keys.extend(static_writable_signers.iter().copied());
    static_keys.extend(static_readonly_signers.iter().copied());
    static_keys.extend(static_writable_nonsigners.iter().copied());
    static_keys.extend(static_readonly_nonsigners.iter().copied());

    let num_required_signatures        = (1 + static_writable_signers.len()
                                            + static_readonly_signers.len()) as u8;
    let num_readonly_signed_accounts   = static_readonly_signers.len() as u8;
    let num_readonly_unsigned_accounts = static_readonly_nonsigners.len() as u8;

    // Step 4: compile instructions with v0 indexing.
    // Index space: [static_keys (0..K)] [alt_w_resolved (K..K+W)] [alt_r_resolved (K+W..K+W+R)]
    let k = static_keys.len();
    let mut key_index: HashMap<Pubkey, u8> = static_keys.iter().enumerate()
        .map(|(i, k)| (*k, i as u8))
        .collect();
    for (offset, (_, pk)) in alt_w.iter().enumerate() {
        key_index.insert(*pk, (k + offset) as u8);
    }
    for (offset, (_, pk)) in alt_r.iter().enumerate() {
        key_index.insert(*pk, (k + alt_w.len() + offset) as u8);
    }

    let compiled = instructions.iter().map(|ix| {
        CompiledInstruction {
            program_id_index: *key_index.get(&ix.program_id)
                .expect("program id was inserted into static keys"),
            account_indices: ix.accounts.iter()
                .map(|am| *key_index.get(&am.pubkey)
                    .expect("account was inserted into static or alt"))
                .collect(),
            data: ix.data.clone(),
        }
    }).collect();

    // Build the lookup section. Skip if no ALT resolutions actually happened.
    let address_table_lookups = if alt_w.is_empty() && alt_r.is_empty() {
        vec![]
    } else {
        vec![AddressTableLookup {
            account_key:      *alt_pubkey,
            writable_indexes: alt_w.iter().map(|(i, _)| *i).collect(),
            readonly_indexes: alt_r.iter().map(|(i, _)| *i).collect(),
        }]
    };

    V0Message {
        num_required_signatures,
        num_readonly_signed_accounts,
        num_readonly_unsigned_accounts,
        static_account_keys: static_keys,
        recent_blockhash:    *recent_blockhash,
        instructions:        compiled,
        address_table_lookups,
    }
}

/// Multi-ALT variant. Each entry in `alts` is `(alt_pubkey, alt_addresses)` —
/// the ALT account address and the list of pubkeys it stores on-chain.
/// Accounts are resolved against EACH ALT in order; the first ALT containing
/// a given pubkey wins. Useful when stitching txs that span our own ALT and
/// e.g. Jupiter's swap ALTs.
pub fn build_v0_message_multi(
    payer:            &Pubkey,
    instructions:     &[Instruction],
    recent_blockhash: &[u8; 32],
    alts:             &[(Pubkey, Vec<Pubkey>)],
    force_static:     &[Pubkey],
) -> V0Message {
    // Step 1: union account metadata.
    let mut all: HashMap<Pubkey, AccMeta> = HashMap::new();
    all.insert(*payer, AccMeta { is_signer: true, is_writable: true });
    let mut program_ids: std::collections::HashSet<Pubkey> = std::collections::HashSet::new();
    for ix in instructions {
        for am in &ix.accounts {
            let entry = all.entry(am.pubkey).or_default();
            if am.is_signer   { entry.is_signer   = true; }
            if am.is_writable { entry.is_writable = true; }
        }
        all.entry(ix.program_id).or_insert(AccMeta::default());
        program_ids.insert(ix.program_id);
    }

    // Step 2: build "which ALT contains this pubkey" lookup (first wins).
    // alt_membership: pubkey -> (alt_index, index_in_alt)
    let mut alt_membership: HashMap<Pubkey, (usize, u8)> = HashMap::new();
    for (alt_idx, (_alt_pk, addrs)) in alts.iter().enumerate() {
        for (i, pk) in addrs.iter().enumerate() {
            alt_membership.entry(*pk).or_insert((alt_idx, i as u8));
        }
    }
    let force_static_set: std::collections::HashSet<Pubkey> = force_static.iter().copied().collect();

    let mut static_writable_signers:    Vec<Pubkey> = vec![];
    let mut static_readonly_signers:    Vec<Pubkey> = vec![];
    let mut static_writable_nonsigners: Vec<Pubkey> = vec![];
    let mut static_readonly_nonsigners: Vec<Pubkey> = vec![];

    // Per-ALT writable/readonly buckets: [(alt_idx, pk, idx_in_alt)]
    let mut alt_w_per: Vec<Vec<(Pubkey, u8)>> = vec![vec![]; alts.len()];
    let mut alt_r_per: Vec<Vec<(Pubkey, u8)>> = vec![vec![]; alts.len()];

    for (pk, m) in all.iter() {
        if pk == payer { continue; }
        if m.is_signer {
            if m.is_writable { static_writable_signers.push(*pk); }
            else              { static_readonly_signers.push(*pk); }
            continue;
        }
        if program_ids.contains(pk) {
            static_readonly_nonsigners.push(*pk);
            continue;
        }
        if force_static_set.contains(pk) {
            if m.is_writable { static_writable_nonsigners.push(*pk); }
            else              { static_readonly_nonsigners.push(*pk); }
            continue;
        }
        if let Some(&(alt_idx, in_alt_idx)) = alt_membership.get(pk) {
            if m.is_writable {
                alt_w_per[alt_idx].push((*pk, in_alt_idx));
            } else {
                alt_r_per[alt_idx].push((*pk, in_alt_idx));
            }
        } else {
            if m.is_writable { static_writable_nonsigners.push(*pk); }
            else              { static_readonly_nonsigners.push(*pk); }
        }
    }

    static_writable_signers.sort();
    static_readonly_signers.sort();
    static_writable_nonsigners.sort();
    static_readonly_nonsigners.sort();
    for v in alt_w_per.iter_mut() { v.sort_by_key(|(_, i)| *i); }
    for v in alt_r_per.iter_mut() { v.sort_by_key(|(_, i)| *i); }

    // Assemble static keys.
    let mut static_keys = Vec::new();
    static_keys.push(*payer);
    static_keys.extend(static_writable_signers.iter().copied());
    static_keys.extend(static_readonly_signers.iter().copied());
    static_keys.extend(static_writable_nonsigners.iter().copied());
    static_keys.extend(static_readonly_nonsigners.iter().copied());

    let num_required_signatures        = (1 + static_writable_signers.len()
                                            + static_readonly_signers.len()) as u8;
    let num_readonly_signed_accounts   = static_readonly_signers.len() as u8;
    let num_readonly_unsigned_accounts = static_readonly_nonsigners.len() as u8;

    // Build key_index. Layout in runtime account list:
    //   [static_keys] [alt0_w] [alt0_r] [alt1_w] [alt1_r] ... [altN_w] [altN_r]
    //
    // ⚠ This matches the Solana runtime ordering: all writable-from-ALT come
    // before all readonly-from-ALT (concat across ALTs), but Solana actually
    // expects [alt0_w][alt1_w]...[altN_w][alt0_r][alt1_r]...[altN_r] —
    // writables from ALL ALTs first, THEN readonlies from ALL ALTs.
    let k = static_keys.len();
    let mut key_index: HashMap<Pubkey, u8> = static_keys.iter().enumerate()
        .map(|(i, k)| (*k, i as u8))
        .collect();
    let mut offset = k;
    for v in alt_w_per.iter() {
        for (pk, _) in v.iter() {
            key_index.insert(*pk, offset as u8);
            offset += 1;
        }
    }
    for v in alt_r_per.iter() {
        for (pk, _) in v.iter() {
            key_index.insert(*pk, offset as u8);
            offset += 1;
        }
    }

    let compiled = instructions.iter().map(|ix| {
        CompiledInstruction {
            program_id_index: *key_index.get(&ix.program_id)
                .expect("program id was inserted into static keys"),
            account_indices: ix.accounts.iter()
                .map(|am| *key_index.get(&am.pubkey)
                    .expect("account was inserted into static or alt"))
                .collect(),
            data: ix.data.clone(),
        }
    }).collect();

    // Build address_table_lookups: one entry per ALT that has at least one resolved.
    let mut address_table_lookups = Vec::new();
    for (alt_idx, (alt_pubkey, _)) in alts.iter().enumerate() {
        let w_idxs: Vec<u8> = alt_w_per[alt_idx].iter().map(|(_, i)| *i).collect();
        let r_idxs: Vec<u8> = alt_r_per[alt_idx].iter().map(|(_, i)| *i).collect();
        if !w_idxs.is_empty() || !r_idxs.is_empty() {
            address_table_lookups.push(AddressTableLookup {
                account_key:      *alt_pubkey,
                writable_indexes: w_idxs,
                readonly_indexes: r_idxs,
            });
        }
    }

    V0Message {
        num_required_signatures,
        num_readonly_signed_accounts,
        num_readonly_unsigned_accounts,
        static_account_keys: static_keys,
        recent_blockhash:    *recent_blockhash,
        instructions:        compiled,
        address_table_lookups,
    }
}

/// One-call convenience: build, serialize, sign — v0 with MULTIPLE ALTs.
pub fn build_and_sign_v0_tx_b64_multi(
    payer:            &Pubkey,
    instructions:     &[Instruction],
    recent_blockhash: &[u8; 32],
    alts:             &[(Pubkey, Vec<Pubkey>)],
    force_static:     &[Pubkey],
    keypair_bytes:    &[u8; 64],
) -> ArbResult<String> {
    let msg = build_v0_message_multi(
        payer, instructions, recent_blockhash, alts, force_static,
    );
    sign_v0_tx_b64(&msg, keypair_bytes)
}

/// Serialize the v0 message to wire bytes (the bytes we sign + send).
pub fn serialize_v0_message(msg: &V0Message) -> Vec<u8> {
    let mut buf = Vec::with_capacity(
        1 + 3                                                        // version + header
        + 3 + msg.static_account_keys.len() * 32                     // static keys
        + 32                                                         // blockhash
        + 3 + msg.instructions.iter().map(|i|
            4 + i.account_indices.len() + i.data.len()).sum::<usize>()
        + 3 + msg.address_table_lookups.iter().map(|l|
            32 + 3 + l.writable_indexes.len() + 3 + l.readonly_indexes.len())
            .sum::<usize>()
    );
    // Version prefix: bit 7 set (= v0). Bits 0-6 = version number (0).
    buf.push(0x80u8);
    // Header (3 bytes — same shape as legacy, bracketed by the version byte).
    buf.push(msg.num_required_signatures);
    buf.push(msg.num_readonly_signed_accounts);
    buf.push(msg.num_readonly_unsigned_accounts);
    // Static account keys.
    buf.extend(encode_compact_u16(msg.static_account_keys.len() as u16));
    for k in &msg.static_account_keys { buf.extend_from_slice(k.as_ref()); }
    // Recent blockhash.
    buf.extend_from_slice(&msg.recent_blockhash);
    // Instructions.
    buf.extend(encode_compact_u16(msg.instructions.len() as u16));
    for ix in &msg.instructions {
        buf.push(ix.program_id_index);
        buf.extend(encode_compact_u16(ix.account_indices.len() as u16));
        buf.extend_from_slice(&ix.account_indices);
        buf.extend(encode_compact_u16(ix.data.len() as u16));
        buf.extend_from_slice(&ix.data);
    }
    // Address table lookups.
    buf.extend(encode_compact_u16(msg.address_table_lookups.len() as u16));
    for lookup in &msg.address_table_lookups {
        buf.extend_from_slice(lookup.account_key.as_ref());
        buf.extend(encode_compact_u16(lookup.writable_indexes.len() as u16));
        buf.extend_from_slice(&lookup.writable_indexes);
        buf.extend(encode_compact_u16(lookup.readonly_indexes.len() as u16));
        buf.extend_from_slice(&lookup.readonly_indexes);
    }
    buf
}

/// Sign a v0 message and produce the full wire-format base64 transaction.
/// Single-signer only — same constraint as the legacy signer.
pub fn sign_v0_tx_b64(
    msg:           &V0Message,
    keypair_bytes: &[u8; 64],
) -> ArbResult<String> {
    if msg.num_required_signatures != 1 {
        return Err(ArbError::Invariant(
            "sign_v0_tx_b64: only single-signer txs supported (our wallet)"
        ));
    }
    let msg_bytes = serialize_v0_message(msg);

    let secret_seed: [u8; 32] = keypair_bytes[0..32].try_into()
        .expect("slice of 32 from 64");
    let signing_key = SigningKey::from_bytes(&secret_seed);
    if signing_key.verifying_key().to_bytes() != keypair_bytes[32..64] {
        return Err(ArbError::Invariant(
            "sign_v0_tx_b64: keypair public-key derivation mismatch"
        ));
    }
    if msg.static_account_keys[0].to_bytes() != keypair_bytes[32..64] {
        return Err(ArbError::Invariant(
            "sign_v0_tx_b64: payer (static_account_keys[0]) doesn't match keypair pubkey"
        ));
    }

    let signature = signing_key.sign(&msg_bytes);
    let mut tx = Vec::with_capacity(1 + 64 + msg_bytes.len());
    tx.extend(encode_compact_u16(1));        // 1 signature
    tx.extend_from_slice(&signature.to_bytes());
    tx.extend_from_slice(&msg_bytes);
    Ok(base64::engine::general_purpose::STANDARD.encode(tx))
}

/// One-call convenience: build, serialize, sign — v0 with single ALT.
pub fn build_and_sign_v0_tx_b64(
    payer:            &Pubkey,
    instructions:     &[Instruction],
    recent_blockhash: &[u8; 32],
    alt_pubkey:       &Pubkey,
    alt_addresses:    &[Pubkey],
    force_static:     &[Pubkey],
    keypair_bytes:    &[u8; 64],
) -> ArbResult<String> {
    let msg = build_v0_message(
        payer, instructions, recent_blockhash,
        alt_pubkey, alt_addresses, force_static,
    );
    sign_v0_tx_b64(&msg, keypair_bytes)
}

// ──────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw_tx::{set_compute_unit_limit_ix, system_transfer_ix};

    fn pk(b: u8) -> Pubkey { Pubkey::new_from_array([b; 32]) }

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
    fn v0_with_empty_alt_falls_through_to_static_only() {
        let payer = pk(1);
        let to    = pk(2);
        let ix = system_transfer_ix(payer, to, 100);
        let msg = build_v0_message(&payer, &[ix], &[7u8; 32], &pk(99), &[], &[]);
        // Static layout matches legacy's: payer, recipient, system program.
        assert_eq!(msg.static_account_keys[0], payer);
        assert!(msg.static_account_keys.contains(&to));
        // No ALT lookups when nothing was in the table.
        assert!(msg.address_table_lookups.is_empty());
    }

    #[test]
    fn v0_serialization_starts_with_version_byte() {
        let payer = pk(1);
        let ix = system_transfer_ix(payer, pk(2), 100);
        let msg = build_v0_message(&payer, &[ix], &[0u8; 32], &pk(99), &[], &[]);
        let bytes = serialize_v0_message(&msg);
        assert_eq!(bytes[0], 0x80, "v0 marker missing");
        // Header bytes immediately follow.
        assert_eq!(bytes[1], msg.num_required_signatures);
        assert_eq!(bytes[2], msg.num_readonly_signed_accounts);
        assert_eq!(bytes[3], msg.num_readonly_unsigned_accounts);
    }

    #[test]
    fn v0_pulls_alt_addresses_out_of_static_keys() {
        let payer = pk(1);
        let recipient = pk(50);
        // Suppose `recipient` and SYSTEM_PROGRAM are both in the ALT.
        let alt_addr = pk(99);
        let alt_addrs = vec![recipient, crate::known::SYSTEM_PROGRAM];

        let ix = system_transfer_ix(payer, recipient, 100);
        let msg = build_v0_message(&payer, &[ix], &[0u8; 32], &alt_addr, &alt_addrs, &[]);

        // Recipient is writable + non-signer → should resolve via ALT, not be static.
        // SYSTEM_PROGRAM is a program id → MUST stay static.
        assert!(!msg.static_account_keys.contains(&recipient),
                "recipient should be ALT-resolved, not static");
        assert!(msg.static_account_keys.contains(&crate::known::SYSTEM_PROGRAM),
                "system program must stay static (program ids can't be in ALT lookups)");

        // Lookup section should reflect the ALT pull.
        assert_eq!(msg.address_table_lookups.len(), 1);
        let lookup = &msg.address_table_lookups[0];
        assert_eq!(lookup.account_key, alt_addr);
        assert_eq!(lookup.writable_indexes, vec![0u8]); // recipient was at ALT index 0
        assert_eq!(lookup.readonly_indexes, Vec::<u8>::new());
    }

    #[test]
    fn v0_account_indices_span_static_then_alt() {
        let payer = pk(1);
        let recipient = pk(50);
        let alt_addrs = vec![recipient];
        let ix = system_transfer_ix(payer, recipient, 100);
        let msg = build_v0_message(&payer, &[ix], &[0u8; 32], &pk(99), &alt_addrs, &[]);

        // recipient resolved at index = static_keys.len() (first ALT-writable).
        let recipient_idx = msg.static_account_keys.len() as u8;
        // Compiled instruction's recipient slot (account index 1 in the original
        // accounts list — `system_transfer_ix` puts `to` second) maps to that.
        assert_eq!(msg.instructions[0].account_indices[1], recipient_idx);
    }

    #[test]
    fn v0_signers_stay_static_even_if_in_alt() {
        // Edge case: someone puts the payer's own pubkey in an ALT. The
        // builder must NOT resolve the payer via ALT (signers must be static).
        let payer = pk(1);
        let alt_addrs = vec![payer]; // pathological but legal config

        let ix = set_compute_unit_limit_ix(100_000);
        let msg = build_v0_message(&payer, &[ix], &[0u8; 32], &pk(99), &alt_addrs, &[]);

        assert_eq!(msg.static_account_keys[0], payer);
        // The payer should NOT appear in any ALT lookup.
        for lookup in &msg.address_table_lookups {
            assert!(!lookup.writable_indexes.contains(&0u8)
                || lookup.account_key != pk(99),
                "payer must not be ALT-resolved");
        }
    }

    #[test]
    fn v0_serialize_is_deterministic() {
        let payer = pk(1);
        let ix = system_transfer_ix(payer, pk(2), 100);
        let alt_addrs = vec![pk(2)];
        let msg = build_v0_message(&payer, &[ix], &[7u8; 32], &pk(99), &alt_addrs, &[]);
        let s1 = serialize_v0_message(&msg);
        let s2 = serialize_v0_message(&msg);
        assert_eq!(s1, s2);
    }

    #[test]
    fn v0_sign_and_serialize_produces_valid_b64() {
        let kp = test_keypair();
        let payer = Pubkey::new_from_array(kp[32..64].try_into().unwrap());
        let ix = system_transfer_ix(payer, pk(50), 100);
        let b64 = build_and_sign_v0_tx_b64(
            &payer, &[ix], &[0u8; 32], &pk(99), &[], &[], &kp,
        ).expect("sign");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&b64).expect("decode");
        // sig count + signature + version byte
        assert_eq!(bytes[0], 0x01); // 1 signature
        // Find version marker after the 64-byte signature.
        assert_eq!(bytes[1 + 64], 0x80, "v0 marker should follow the signature");
    }

    #[test]
    fn v0_force_static_keeps_account_in_static_keys_even_when_in_alt() {
        // Regression for the Jito tip-account bug: with the tip in the ALT
        // and no force_static, the builder would resolve it via lookup, and
        // Jito's auction filter (which only inspects static_account_keys)
        // would reject the bundle with "Bundles must write lock at least one
        // tip account to be eligible for the auction."
        let payer = pk(1);
        let tip   = pk(50);
        let alt_addrs = vec![tip];
        let ix = system_transfer_ix(payer, tip, 100);

        // Without force_static, tip resolves via ALT.
        let msg = build_v0_message(&payer, &[ix.clone()], &[0u8; 32], &pk(99), &alt_addrs, &[]);
        assert!(!msg.static_account_keys.contains(&tip),
                "tip should be ALT-resolved when not forced static");

        // With force_static, tip stays in static_account_keys.
        let msg2 = build_v0_message(&payer, &[ix], &[0u8; 32], &pk(99), &alt_addrs, &[tip]);
        assert!(msg2.static_account_keys.contains(&tip),
                "tip MUST stay static when force_static contains it");
        assert!(msg2.address_table_lookups.is_empty(),
                "no ALT lookups should be emitted when the only ALT-eligible \
                 account was forced static");
    }

    #[test]
    fn v0_size_drops_when_alt_pulls_repeated_accounts() {
        // Build a tx that references the SAME 4 accounts twice — once as
        // direct accounts in 4 different (synthetic) instructions. Compare
        // wire size with vs. without ALT.
        let payer = pk(1);
        let common_a = pk(10);
        let common_b = pk(11);
        let common_c = pk(12);
        let common_d = pk(13);

        let make_ix = |target: Pubkey| -> Instruction {
            Instruction {
                program_id: pk(50),
                accounts: vec![
                    solana_program::instruction::AccountMeta::new_readonly(target, false),
                    solana_program::instruction::AccountMeta::new_readonly(common_a, false),
                    solana_program::instruction::AccountMeta::new_readonly(common_b, false),
                    solana_program::instruction::AccountMeta::new_readonly(common_c, false),
                    solana_program::instruction::AccountMeta::new_readonly(common_d, false),
                ],
                data: vec![1, 2, 3],
            }
        };
        let ixs = vec![make_ix(pk(20)), make_ix(pk(21)), make_ix(pk(22))];

        let msg_no_alt = build_v0_message(&payer, &ixs, &[0u8; 32], &pk(99), &[], &[]);
        let bytes_no_alt = serialize_v0_message(&msg_no_alt);

        let alt_addrs = vec![common_a, common_b, common_c, common_d];
        let msg_with_alt = build_v0_message(&payer, &ixs, &[0u8; 32], &pk(99), &alt_addrs, &[]);
        let bytes_with_alt = serialize_v0_message(&msg_with_alt);

        assert!(bytes_with_alt.len() < bytes_no_alt.len(),
                "ALT must shrink the wire size: no_alt={}, with_alt={}",
                bytes_no_alt.len(), bytes_with_alt.len());
        // 4 pulled accounts × ~31 bytes each saved (32 byte pubkey → 1 byte index)
        // minus the lookup overhead (32 byte ALT pubkey + ~6 byte indexes header).
        // Expect at least 60 bytes saved.
        assert!(bytes_no_alt.len() - bytes_with_alt.len() >= 60,
                "expected ≥60 bytes saved; got {}",
                bytes_no_alt.len() - bytes_with_alt.len());
    }
}
