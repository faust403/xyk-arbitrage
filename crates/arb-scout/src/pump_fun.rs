//! Pump.fun bonding-curve buy/sell instruction builders.
//!
//! ## Why this exists
//!
//! Jupiter doesn't index pump.fun tokens until ~10-30 seconds after launch.
//! For true sniping (<200 ms after mint) we have to call the pump.fun
//! program directly. This module builds the buy/sell Anchor instructions
//! with the v2-upgrade account list (16 accounts for buy / 14 for sell)
//! verified against `pump-public-docs`, `chainstacklabs/pump-fun-bot`,
//! and on-chain successful txs as of May 2026.
//!
//! ## Critical gotchas
//!
//! * `fee_recipient` ROTATES among 8 addresses — pump.fun changes the
//!   default in the `Global` account. We hardcode the current primary
//!   (`62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV`) but operators may
//!   want to `fetch_global()` at startup and override.
//! * The `creator_vault` PDA derives from `bonding_curve.creator` — for
//!   a fresh mint that's the same as the dev wallet (the signer of the
//!   Create tx). Cache from the Create event.
//! * Missing any of `creator_vault`, `global_volume_accumulator`,
//!   `user_volume_accumulator`, `fee_config`, `fee_program`, OR the
//!   trailing `bonding_curve_v2` remaining-account → `Custom:6024`
//!   overflow → tx revert.
//!
//! ## Source
//!
//! * [pump-public-docs](https://github.com/pump-fun/pump-public-docs)
//! * [chainstacklabs/pump-fun-bot](https://github.com/chainstacklabs/pump-fun-bot)
//! * `sha256("global:buy")[..8]` and `sha256("global:sell")[..8]`.

use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey;
use solana_program::pubkey::Pubkey;

use crate::raw_tx::{derive_ata, derive_ata_2022, TOKEN_2022_PROGRAM};
use crate::known::SYSTEM_PROGRAM;
use arb_dex_raydium::TOKEN_PROGRAM as SPL_TOKEN_PROGRAM;

// ─── Program & well-known PDAs ─────────────────────────────────────────────

pub const PUMP_FUN_PROGRAM: Pubkey =
    pubkey!("6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P");

/// `Global` PDA — seeds = `["global"]`. Holds the `fee_recipient` and
/// initial bonding-curve constants. Cached const value verified by
/// `Pubkey::find_program_address(&[b"global"], &PUMP_FUN_PROGRAM)`.
pub const PUMP_FUN_GLOBAL: Pubkey =
    pubkey!("4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf");

/// Event-authority PDA — seeds = `["__event_authority"]`. Anchor uses
/// this for `emit_cpi!` log events.
pub const PUMP_FUN_EVENT_AUTHORITY: Pubkey =
    pubkey!("Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1");

/// Primary fee-recipient when `mayhemMode = false`. As of May 2026 the
/// global has `mayhemModeEnabled = true`, which means freshly-created
/// bonding curves inherit `isMayhemMode = true` and MUST use the
/// `RESERVED_FEE_RECIPIENTS` list instead — passing this here yields
/// `Custom: 6000 NotAuthorized`. Kept for non-mayhem coins.
pub const PUMP_FUN_FEE_RECIPIENT_DEFAULT: Pubkey =
    pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");

/// The 8 normal fee recipients from `Global.feeRecipient` +
/// `Global.feeRecipients[7]`. Used when a BC has `isMayhemMode = false`.
pub const NORMAL_FEE_RECIPIENTS: [Pubkey; 8] = [
    pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV"), // Global.feeRecipient
    pubkey!("7VtfL8fvgNfhz17qKRMjzQEXgbdpnHHHQRh54R9jP2RJ"), // [0]
    pubkey!("7hTckgnGnLQR6sdH7YkqFTAA7VwTfYFaZ6EhEsU3saCX"), // [1]
    pubkey!("9rPYyANsfQZw3DnDmKE3YCQF5E8oD89UXoHn9JFEhJUz"), // [2]
    pubkey!("AVmoTthdrX6tKt4nDjco2D775W2YK3sDhxPcMmzUAmTY"), // [3]
    pubkey!("CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM"), // [4]
    pubkey!("FWsW1xNtWscwNmKv6wVsU1iTzRN6wmmk3MjxRP5tT7hz"), // [5]
    pubkey!("G5UZAVbAf46s7cKWoyKu8kYTip9DGTpbLZ2qa9Aq69dP"), // [6]
];

/// The 8 reserved fee recipients from `Global.reservedFeeRecipient` +
/// `Global.reservedFeeRecipients[7]`. These are the *only* authorised
/// fee recipients when a bonding curve has `isMayhemMode = true`, which
/// is the default for any BC created while `Global.mayhemModeEnabled`
/// is on. Pick one (deterministic-rotation or random) per tx.
pub const RESERVED_FEE_RECIPIENTS: [Pubkey; 8] = [
    pubkey!("GesfTA3X2arioaHp8bbKdjG9vJtskViWACZoYvxp4twS"), // reservedFeeRecipient
    pubkey!("4budycTjhs9fD6xw62VBducVTNgMgJJ5BgtKq7mAZwn6"), // reservedFeeRecipients[0]
    pubkey!("8SBKzEQU4nLSzcwF4a74F2iaUDQyTfjGndn6qUWBnrpR"), // [1]
    pubkey!("4UQeTP1T39KZ9Sfxzo3WR5skgsaP6NZa87BAkuazLEKH"), // [2]
    pubkey!("8sNeir4QsLsJdYpc9RZacohhK1Y5FLU3nC5LXgYB4aa6"), // [3]
    pubkey!("Fh9HmeLNUMVCvejxCtCL2DbYaRyBFVJ5xrWkLnMH6fdk"), // [4]
    pubkey!("463MEnMeGyJekNZFQSTUABBEbLnvMTALbT6ZmsxAbAdq"), // [5]
    pubkey!("6AUH3WEHucYZyC61hqpqYUWVto5qA5hjHuNQ32GNnNxA"), // [6]
];

// Rent sysvar (needed in some legacy paths; included for compatibility).
pub const RENT_SYSVAR: Pubkey =
    pubkey!("SysvarRent111111111111111111111111111111111");

// ─── Anchor discriminators ─────────────────────────────────────────────────

/// `sha256("global:buy")[..8]`.
pub const BUY_DISCRIMINATOR: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];

/// `sha256("global:sell")[..8]`.
pub const SELL_DISCRIMINATOR: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

// ─── Initial bonding-curve constants (Global account defaults) ────────────

/// 30 SOL in lamports.
pub const INITIAL_VIRTUAL_SOL_RESERVES: u128 = 30_000_000_000;
/// 1.073e9 tokens * 1e6 = 1.073e15 base units.
pub const INITIAL_VIRTUAL_TOKEN_RESERVES: u128 = 1_073_000_000_000_000;
/// 793.1M tokens * 1e6 — what's actually for sale on the curve.
pub const INITIAL_REAL_TOKEN_RESERVES: u128 = 793_100_000_000_000;
/// 1% — pump.fun's protocol fee.
pub const FEE_BASIS_POINTS: u128 = 100;

// ─── PDA derivations ───────────────────────────────────────────────────────

/// Per-mint `bonding_curve` PDA — seeds = `["bonding-curve", mint]`.
pub fn derive_bonding_curve_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bonding-curve", mint.as_ref()],
        &PUMP_FUN_PROGRAM,
    ).0
}

/// Per-creator `creator_vault` PDA — seeds = `["creator-vault", creator]`.
/// The creator is the dev wallet that ran the Create tx (and signs the
/// initial buy that funds the bonding curve).
pub fn derive_creator_vault_pda(creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"creator-vault", creator.as_ref()],
        &PUMP_FUN_PROGRAM,
    ).0
}

/// `bonding_curve_v2` PDA — seeds = `["bonding-curve-v2", mint]` (one
/// single-string seed, not `["bonding-curve", mint, "v2"]`). Verified
/// against `@pump-fun/pump-sdk 1.35.0` source. Pump's buy IX expects
/// this PDA as the *first* of two remaining accounts; without it the
/// program errors out before reaching the buyback-recipient check.
pub fn derive_bonding_curve_v2_pda(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bonding-curve-v2", mint.as_ref()],
        &PUMP_FUN_PROGRAM,
    ).0
}

/// Global volume accumulator PDA — seeds = `["global_volume_accumulator"]`.
pub fn derive_global_volume_accumulator_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[b"global_volume_accumulator"],
        &PUMP_FUN_PROGRAM,
    ).0
}

/// Per-user volume accumulator — seeds = `["user_volume_accumulator", user]`.
pub fn derive_user_volume_accumulator_pda(user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"user_volume_accumulator", user.as_ref()],
        &PUMP_FUN_PROGRAM,
    ).0
}

/// `fee_config` PDA — seeds = `["fee_config", pump_program_id_bytes]`,
/// derived under the **Fee Program** (not pump itself). Owned by fee program.
/// On mainnet today this resolves to `8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt`.
pub fn derive_fee_config_pda() -> Pubkey {
    Pubkey::find_program_address(
        &[b"fee_config", PUMP_FUN_PROGRAM.as_ref()],
        &FEE_PROGRAM,
    ).0
}

/// Fee program — verified from official pump-public-docs IDL (2026-05-11).
/// Owns the `fee_config` PDA. Pump CPIs into this program on every buy/sell
/// to compute the dynamic fee tier (post-Sept-2025 cashback upgrade).
pub const FEE_PROGRAM: Pubkey =
    pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");

/// The 8 buyback fee recipients from Global.buybackFeeRecipients, read from
/// the on-chain `Global` PDA on 2026-05-11. When `Global.buybackBasisPoints`
/// is non-zero (currently 5000 = 50% of fees go to buyback), pump.fun's buy
/// and sell IXs error with `BuybackFeeRecipientMissing` (6062) unless these
/// are passed as remaining-accounts at slots 16+. Per the IDL the program
/// requires "exactly 8 or none".
pub const BUYBACK_FEE_RECIPIENTS: [Pubkey; 8] = [
    pubkey!("5YxQFdt3Tr9zJLvkFccqXVUwhdTWJQc1fFg2YPbxvxeD"),
    pubkey!("9M4giFFMxmFGXtc3feFzRai56WbBqehoSeRE5GK7gf7"),
    pubkey!("GXPFM2caqTtQYC2cJ5yJRi9VDkpsYZXzYdwYpGnLmtDL"),
    pubkey!("3BpXnfJaUTiwXnJNe7Ej1rcbzqTTQUvLShZaWazebsVR"),
    pubkey!("5cjcW9wExnJJiqgLjq7DEG75Pm6JBgE1hNv4B2vHXUW6"),
    pubkey!("EHAAiTxcdDwQ3U4bU6YcMsQGaekdzLS3B5SmYo46kJtL"),
    pubkey!("5eHhjP8JaYkz83CWwvGU2uMUXefd3AazWGx4gpcuEEYD"),
    pubkey!("A7hAgCzFw14fejgCp387JUJRMNyz4j89JKnhtKU8piqW"),
];

// ─── Bonding curve account layout ─────────────────────────────────────────

/// Subset of `BondingCurve` fields we care about, after the 8-byte
/// Anchor discriminator.
///
/// Layout (offset relative to data start, post-disc):
///   * 8 + 0  : virtual_token_reserves   u64
///   * 8 + 8  : virtual_sol_reserves     u64
///   * 8 + 16 : real_token_reserves      u64
///   * 8 + 24 : real_sol_reserves        u64
///   * 8 + 32 : token_total_supply       u64
///   * 8 + 40 : complete                 bool (1 byte)
///   * 8 + 41 : creator                  Pubkey (32 bytes)
#[derive(Debug, Clone, Copy)]
pub struct BondingCurveState {
    pub virtual_token_reserves: u64,
    pub virtual_sol_reserves:   u64, // = virtualQuoteReserves in v2 IDL
    pub real_token_reserves:    u64,
    pub real_sol_reserves:      u64,
    pub token_total_supply:     u64,
    pub complete:               bool,
    pub creator:                Pubkey,
    /// Set when this BC was created while Global.mayhemModeEnabled=true.
    /// When set, buy/sell MUST use `RESERVED_FEE_RECIPIENTS`, else 6000.
    pub is_mayhem_mode:         bool,
    pub is_cashback_coin:       bool,
}

impl BondingCurveState {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        // Layout post-Sept-2025 upgrade (per official IDL):
        //   8  disc
        //  +0 virtualTokenReserves u64
        //  +8 virtualQuoteReserves u64
        // +16 realTokenReserves    u64
        // +24 realQuoteReserves    u64
        // +32 tokenTotalSupply     u64
        // +40 complete             bool
        // +41 creator              Pubkey
        // +73 isMayhemMode         bool
        // +74 isCashbackCoin       bool
        // +75 quoteMint            Pubkey
        if data.len() < 8 + 75 { return None; }
        let read_u64 = |o: usize| {
            let mut b = [0u8; 8];
            b.copy_from_slice(&data[o..o + 8]);
            u64::from_le_bytes(b)
        };
        let creator_bytes: [u8; 32] = data[8 + 41..8 + 41 + 32].try_into().ok()?;
        Some(BondingCurveState {
            virtual_token_reserves: read_u64(8),
            virtual_sol_reserves:   read_u64(8 + 8),
            real_token_reserves:    read_u64(8 + 16),
            real_sol_reserves:      read_u64(8 + 24),
            token_total_supply:     read_u64(8 + 32),
            complete:               data[8 + 40] != 0,
            creator:                Pubkey::new_from_array(creator_bytes),
            is_mayhem_mode:         data.get(8 + 73).copied().unwrap_or(0) != 0,
            is_cashback_coin:       data.get(8 + 74).copied().unwrap_or(0) != 0,
        })
    }
}

// ─── Constant-product math ─────────────────────────────────────────────────

/// Tokens received for `sol_in_lamports` on a bonding curve with given
/// virtual reserves. Standard constant-product (k = sol × tokens) with
/// 1% protocol fee deducted from input. Returns 0 if math overflows.
pub fn compute_buy_tokens(
    virtual_sol_reserves:   u64,
    virtual_token_reserves: u64,
    sol_in_lamports:        u64,
) -> u64 {
    let sol_in = sol_in_lamports as u128;
    let vs     = virtual_sol_reserves as u128;
    let vt     = virtual_token_reserves as u128;
    // Take 1% fee off the input.
    let fee   = sol_in * FEE_BASIS_POINTS / 10_000;
    let net_in = sol_in.saturating_sub(fee);
    if net_in == 0 || vs == 0 || vt == 0 { return 0; }
    // k = vs × vt
    // new_vs = vs + net_in
    // new_vt = k / new_vs
    // tokens_out = vt - new_vt
    let k       = vs.saturating_mul(vt);
    let new_vs  = vs.saturating_add(net_in);
    let new_vt  = k / new_vs;
    let tokens_out = vt.saturating_sub(new_vt);
    tokens_out.min(u64::MAX as u128) as u64
}

/// SOL received for `tokens_in_base` on a bonding curve with given
/// reserves. Constant-product with 1% fee on output.
pub fn compute_sell_sol(
    virtual_sol_reserves:   u64,
    virtual_token_reserves: u64,
    tokens_in_base:         u64,
) -> u64 {
    let ti = tokens_in_base as u128;
    let vs = virtual_sol_reserves as u128;
    let vt = virtual_token_reserves as u128;
    if ti == 0 || vs == 0 || vt == 0 { return 0; }
    let k        = vs.saturating_mul(vt);
    let new_vt   = vt.saturating_add(ti);
    let new_vs   = k / new_vt;
    let sol_gross = vs.saturating_sub(new_vs);
    // Take 1% fee off output.
    let fee  = sol_gross * FEE_BASIS_POINTS / 10_000;
    let sol_net = sol_gross.saturating_sub(fee);
    sol_net.min(u64::MAX as u128) as u64
}

// ─── Instruction builders ──────────────────────────────────────────────────

/// All accounts a pump.fun `buy` instruction takes (post-v2 upgrade).
pub struct PumpFunBuyAccounts {
    pub user:                 Pubkey,   // signer + fee payer
    pub mint:                 Pubkey,
    pub creator:              Pubkey,   // dev wallet from Create event
    pub fee_recipient:        Pubkey,   // current Global.fee_recipient or reserved
    pub bonding_curve_v2_seed_ok: bool, // (deprecated, ignored)
    /// `true` if the mint's owner is Token-2022, `false` for classic SPL
    /// Token. Pump.fun mints can be either — passing the wrong one yields
    /// "Invalid Mint" inside the ATA create ix. ~75% of mints are SPL.
    pub is_token_2022:        bool,
    /// One buyback fee recipient (rotating from the 8 in `Global.buybackFeeRecipients`).
    /// Per SDK 1.35: buy/sell append exactly 2 remaining accounts —
    /// `bonding_curve_v2_pda(mint)` then this recipient.
    pub buyback_recipient:    Pubkey,
}

/// Build a pump.fun `buy` instruction per the official IDL (2026-05-11).
///
/// `amount` — tokens to receive (base units, 6 decimals).
/// `max_sol_cost` — slippage cap in lamports.
///
/// IDL spec: 16 accounts, data = disc(8) + amount(8) + max_sol_cost(8) +
/// trackVolume:OptionBool(1 byte, None = `0x00`).
pub fn build_buy_ix(
    accs:        &PumpFunBuyAccounts,
    amount:      u64,
    max_sol_cost: u64,
) -> Instruction {
    let token_program = if accs.is_token_2022 { TOKEN_2022_PROGRAM } else { SPL_TOKEN_PROGRAM };
    let bonding_curve            = derive_bonding_curve_pda(&accs.mint);
    let associated_bonding_curve = if accs.is_token_2022
        { derive_ata_2022(&bonding_curve, &accs.mint) }
        else { derive_ata(&bonding_curve, &accs.mint) };
    let associated_user          = if accs.is_token_2022
        { derive_ata_2022(&accs.user, &accs.mint) }
        else { derive_ata(&accs.user, &accs.mint) };
    let creator_vault            = derive_creator_vault_pda(&accs.creator);
    let global_volume_accum      = derive_global_volume_accumulator_pda();
    let user_volume_accum        = derive_user_volume_accumulator_pda(&accs.user);
    let fee_config               = derive_fee_config_pda();

    // 25 bytes: 8 disc + 8 amount + 8 max_sol_cost + 1 trackVolume.
    // `optionBool` per IDL is a single-field struct holding a bool, not
    // an Option<bool>. The official SDK 1.28 passes `{ 0: true }` = 0x01.
    // When buyback fees are globally enabled (which they currently are),
    // pump.fun errors with 6062 unless trackVolume.0 = true.
    let mut data = Vec::with_capacity(25);
    data.extend_from_slice(&BUY_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());
    data.push(1u8); // trackVolume.0 = true (mirror official SDK)

    let mut metas = vec![
        AccountMeta::new_readonly(PUMP_FUN_GLOBAL,           false),  // 0  global
        AccountMeta::new(accs.fee_recipient,                 false),  // 1  fee_recipient
        AccountMeta::new_readonly(accs.mint,                 false),  // 2  mint
        AccountMeta::new(bonding_curve,                      false),  // 3  bonding_curve
        AccountMeta::new(associated_bonding_curve,           false),  // 4  associated_bonding_curve
        AccountMeta::new(associated_user,                    false),  // 5  associated_user
        AccountMeta::new(accs.user,                          true),   // 6  user (signer)
        AccountMeta::new_readonly(SYSTEM_PROGRAM,            false),  // 7  system_program
        AccountMeta::new_readonly(token_program,             false),  // 8  token_program (Token-2022 OR SPL)
        AccountMeta::new(creator_vault,                      false),  // 9  creator_vault
        AccountMeta::new_readonly(PUMP_FUN_EVENT_AUTHORITY,  false),  // 10 event_authority
        AccountMeta::new_readonly(PUMP_FUN_PROGRAM,          false),  // 11 program
        AccountMeta::new_readonly(global_volume_accum,       false),  // 12 global_volume_accumulator
        AccountMeta::new(user_volume_accum,                  false),  // 13 user_volume_accumulator
        AccountMeta::new_readonly(fee_config,                false),  // 14 fee_config (under FEE_PROGRAM)
        AccountMeta::new_readonly(FEE_PROGRAM,               false),  // 15 fee_program
    ];
    // Remaining accounts per SDK 1.35: exactly 2.
    //   [16] bondingCurveV2Pda(mint)         — read-only
    //   [17] buyback_recipient (1 of 8)      — writable
    metas.push(AccountMeta::new_readonly(derive_bonding_curve_v2_pda(&accs.mint), false));
    metas.push(AccountMeta::new(accs.buyback_recipient, false));
    Instruction { program_id: PUMP_FUN_PROGRAM, accounts: metas, data }
}

/// Build a pump.fun `sell` instruction.
///
/// `amount` — tokens to sell (base units).
/// `min_sol_output` — slippage floor in lamports (0 = accept any).
pub fn build_sell_ix(
    accs:           &PumpFunBuyAccounts,
    amount:         u64,
    min_sol_output: u64,
) -> Instruction {
    let token_program = if accs.is_token_2022 { TOKEN_2022_PROGRAM } else { SPL_TOKEN_PROGRAM };
    let bonding_curve            = derive_bonding_curve_pda(&accs.mint);
    let associated_bonding_curve = if accs.is_token_2022
        { derive_ata_2022(&bonding_curve, &accs.mint) }
        else { derive_ata(&bonding_curve, &accs.mint) };
    let associated_user          = if accs.is_token_2022
        { derive_ata_2022(&accs.user, &accs.mint) }
        else { derive_ata(&accs.user, &accs.mint) };
    let creator_vault            = derive_creator_vault_pda(&accs.creator);
    let fee_config               = derive_fee_config_pda();

    // 24 bytes: 8 disc + 8 amount + 8 min_sol_output (no trackVolume).
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&SELL_DISCRIMINATOR);
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    // IDL order: creator_vault BEFORE token_program (opposite of buy).
    let mut metas = vec![
        AccountMeta::new_readonly(PUMP_FUN_GLOBAL,           false),  // 0  global
        AccountMeta::new(accs.fee_recipient,                 false),  // 1  fee_recipient
        AccountMeta::new_readonly(accs.mint,                 false),  // 2  mint
        AccountMeta::new(bonding_curve,                      false),  // 3  bonding_curve
        AccountMeta::new(associated_bonding_curve,           false),  // 4  associated_bonding_curve
        AccountMeta::new(associated_user,                    false),  // 5  associated_user
        AccountMeta::new(accs.user,                          true),   // 6  user (signer)
        AccountMeta::new_readonly(SYSTEM_PROGRAM,            false),  // 7  system_program
        AccountMeta::new(creator_vault,                      false),  // 8  creator_vault
        AccountMeta::new_readonly(token_program,             false),  // 9  token_program (Token-2022 OR SPL)
        AccountMeta::new_readonly(PUMP_FUN_EVENT_AUTHORITY,  false),  // 10 event_authority
        AccountMeta::new_readonly(PUMP_FUN_PROGRAM,          false),  // 11 program
        AccountMeta::new_readonly(fee_config,                false),  // 12 fee_config (under FEE_PROGRAM)
        AccountMeta::new_readonly(FEE_PROGRAM,               false),  // 13 fee_program
    ];
    // Remaining accounts per SDK 1.35 sell: same 2 as buy.
    metas.push(AccountMeta::new_readonly(derive_bonding_curve_v2_pda(&accs.mint), false));
    metas.push(AccountMeta::new(accs.buyback_recipient, false));
    Instruction { program_id: PUMP_FUN_PROGRAM, accounts: metas, data }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminators_have_correct_values() {
        assert_eq!(BUY_DISCRIMINATOR, [102, 6, 61, 18, 1, 218, 235, 234]);
        assert_eq!(SELL_DISCRIMINATOR, [51, 230, 133, 164, 1, 127, 131, 173]);
    }

    #[test]
    fn buy_math_against_initial_curve() {
        // Spend 1 SOL on a fresh curve: virtual reserves = 30 SOL / 1.073e15 t.
        // Expected ~3.46e13 tokens (≈ 34.6M tokens with 6 dec).
        let out = compute_buy_tokens(
            INITIAL_VIRTUAL_SOL_RESERVES as u64,
            INITIAL_VIRTUAL_TOKEN_RESERVES as u64,
            1_000_000_000,
        );
        // Sanity: should be > 30M and < 40M whole tokens.
        assert!(out > 30_000_000_000_000 && out < 40_000_000_000_000,
                "buy out wrong: {}", out);
    }

    #[test]
    fn sell_math_inverse_of_buy_approximately() {
        let vs = INITIAL_VIRTUAL_SOL_RESERVES as u64;
        let vt = INITIAL_VIRTUAL_TOKEN_RESERVES as u64;
        // Small buy (0.005 SOL = 0.0017% of pool) — slippage is tiny,
        // so round-trip loss is dominated by the 2× 1% fee = ~2%.
        let buy_in = 5_000_000;
        let tokens = compute_buy_tokens(vs, vt, buy_in);
        let sol_back = compute_sell_sol(vs, vt, tokens);
        // Out should be 0.97 - 0.99 of input on a tiny buy.
        let pct = sol_back as f64 / buy_in as f64;
        assert!(pct > 0.97 && pct < 0.99,
                "round-trip ratio out of band: {pct:.4} ({sol_back} / {buy_in})");
    }

    #[test]
    fn bonding_curve_state_parses() {
        // Full layout: 8 disc + 5×8 u64 + 1 complete + 32 creator
        // + 1 isMayhemMode + 1 isCashbackCoin = 83 bytes minimum.
        let mut data = vec![0u8; 83];
        data[0..8].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11]);
        data[8..16].copy_from_slice(&1_073_000_000_000_000u64.to_le_bytes());
        data[16..24].copy_from_slice(&30_000_000_000u64.to_le_bytes());
        data[8 + 73] = 1; // isMayhemMode = true
        let parsed = BondingCurveState::try_parse(&data).expect("parse ok");
        assert_eq!(parsed.virtual_token_reserves, 1_073_000_000_000_000);
        assert_eq!(parsed.virtual_sol_reserves,   30_000_000_000);
        assert!(parsed.is_mayhem_mode);
        assert!(!parsed.is_cashback_coin);
    }

    #[test]
    fn buy_ix_matches_official_idl_layout() {
        let accs = PumpFunBuyAccounts {
            user:                 Pubkey::new_unique(),
            mint:                 Pubkey::new_unique(),
            creator:              Pubkey::new_unique(),
            fee_recipient:        PUMP_FUN_FEE_RECIPIENT_DEFAULT,
            bonding_curve_v2_seed_ok: true,
            is_token_2022:        false,
            buyback_recipient:    BUYBACK_FEE_RECIPIENTS[0],
        };
        let ix = build_buy_ix(&accs, 1_000_000, 5_000_000);
        assert_eq!(ix.accounts.len(), 16 + 2, "16 base + 2 remaining (bcV2, buyback)");
        // 8 disc + 8 amount + 8 max_sol_cost + 1 OptionBool::None.
        assert_eq!(ix.data.len(), 25);
        assert_eq!(&ix.data[..8], &BUY_DISCRIMINATOR);
        assert_eq!(ix.data[24], 1, "trackVolume.0 = true (matches SDK)");
    }

    #[test]
    fn fee_config_matches_known_mainnet_address() {
        let addr = derive_fee_config_pda();
        assert_eq!(
            addr.to_string(),
            "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt",
            "fee_config PDA should match on-chain singleton"
        );
    }
}
