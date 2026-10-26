//! Reads /tmp/shred-pyth-latest.bin written by the Python atomic_pyth_liquidator
//! subscriber and produces a cloned Pyth update Instruction targeted at OUR
//! wallet as payer. Used by the Solend liquidator to prepend a Pyth oracle
//! update INSIDE the liquidation tx — so the on-chain Refresh Reserve runs
//! against the price we just submitted, defeating the 0x1d ObligationHealthy
//! revert that comes from racing Pyth crank.
//!
//! Binary format (matches scripts/atomic_pyth_liquidator.py write_pyth_dump):
//!   [4 magic = b"PYTH"] [1 version=2] [8 slot LE] [8 ts_ms LE]
//!   [32 program_id] [32 oracle]
//!   [4 ix_data_len LE] [N ix_data bytes]
//!   [1 n_accounts] [n * 32 account pubkeys]

use solana_program::pubkey::Pubkey;
use solana_program::instruction::{Instruction, AccountMeta};
use solana_program::system_program;
use std::time::{SystemTime, UNIX_EPOCH};

pub const DUMP_PATH: &str = "/tmp/shred-pyth-latest.bin";
pub const DUMP_DIR_PER_ORACLE: &str = "/tmp/shred-pyth-by-oracle";
pub const MAGIC: &[u8; 4] = b"PYTH";
pub const MAX_AGE_MS: u64 = 2000;  // dump must be < 2 seconds old to use

#[derive(Debug)]
pub struct PythDump {
    pub slot:        u64,
    pub ts_ms:       u64,
    pub program_id:  Pubkey,
    pub oracle:      Pubkey,
    pub ix_data:     Vec<u8>,
    pub accounts:    Vec<Pubkey>,
}

pub fn read_latest() -> Option<PythDump> {
    read_dump_file(DUMP_PATH)
}

/// Read the dump specifically for `oracle`. Used to avoid the race where the
/// 'latest' dump's oracle differs from the reserve we want to liquidate.
pub fn read_for_oracle(oracle: &Pubkey) -> Option<PythDump> {
    let path = format!("{}/{}.bin", DUMP_DIR_PER_ORACLE, oracle.to_string());
    read_dump_file(&path)
}

fn read_dump_file(path: &str) -> Option<PythDump> {
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < 4 + 1 + 8 + 8 + 32 + 32 + 4 + 1 { return None; }
    if &bytes[0..4] != MAGIC { return None; }
    let version = bytes[4];
    if version != 2 { return None; }
    let slot = u64::from_le_bytes(bytes[5..13].try_into().ok()?);
    let ts_ms = u64::from_le_bytes(bytes[13..21].try_into().ok()?);
    // Freshness check
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
    if now_ms.saturating_sub(ts_ms) > MAX_AGE_MS { return None; }
    let program_id = Pubkey::new_from_array(bytes[21..53].try_into().ok()?);
    let oracle = Pubkey::new_from_array(bytes[53..85].try_into().ok()?);
    let ix_len = u32::from_le_bytes(bytes[85..89].try_into().ok()?) as usize;
    if bytes.len() < 89 + ix_len + 1 { return None; }
    let ix_data = bytes[89..89+ix_len].to_vec();
    let n_acc = bytes[89+ix_len] as usize;
    if bytes.len() < 89 + ix_len + 1 + n_acc * 32 { return None; }
    let mut accounts = Vec::with_capacity(n_acc);
    for i in 0..n_acc {
        let off = 89 + ix_len + 1 + i * 32;
        let pk = Pubkey::new_from_array(bytes[off..off+32].try_into().ok()?);
        accounts.push(pk);
    }
    Some(PythDump { slot, ts_ms, program_id, oracle, ix_data, accounts })
}

/// If the freshest dump's oracle matches `target_oracle`, build a clone of the
/// Pyth update IX with OUR `payer` as account 0 (signer + writable). All other
/// accounts kept as-is; marked writable unless they are the system program.
pub fn build_atomic_pyth_ix(target_oracle: &Pubkey, payer: &Pubkey) -> Option<Instruction> {
    // Try per-oracle file first (no race), fall back to latest (legacy).
    let dump = read_for_oracle(target_oracle)
        .or_else(read_latest)?;
    if dump.oracle != *target_oracle { return None; }
    if dump.accounts.is_empty() { return None; }
    let sys = system_program::id();
    let mut metas: Vec<AccountMeta> = Vec::with_capacity(dump.accounts.len());
    // First account is the payer in the original tx — replace with ours.
    metas.push(AccountMeta::new(*payer, true));
    for a in dump.accounts.iter().skip(1) {
        let is_writable = *a != sys;
        if is_writable {
            metas.push(AccountMeta::new(*a, false));
        } else {
            metas.push(AccountMeta::new_readonly(*a, false));
        }
    }
    Some(Instruction {
        program_id: dump.program_id,
        accounts:   metas,
        data:       dump.ix_data,
    })
}
