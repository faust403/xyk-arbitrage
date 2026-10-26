//! Meteora DLMM (Dynamic Liquidity Market Maker) quoter.
//!
//! ## Account layout — `LbPair` (904 bytes total)
//!
//! Verified against [`MeteoraAg/dlmm-sdk/idls/dlmm.json`] (program version
//! 0.10.1). The IDL has `serialization: "bytemuck"` and `repr: {kind: "c"}`,
//! so the on-wire layout is exactly the C struct order with no padding.
//!
//! Discriminator: `[33, 11, 49, 98, 181, 101, 177, 13]`.
//!
//! Field-by-field offsets (relative to start of `data`, including the 8-byte
//! Anchor discriminator):
//!
//! | Field                              | Offset | Size |
//! |------------------------------------|--------|------|
//! | (discriminator)                    |   0    |  8   |
//! | parameters (StaticParameters)      |   8    | 32   |
//! | v_parameters (VariableParameters)  |  40    | 32   |
//! | bump_seed [u8;1]                   |  72    |  1   |
//! | bin_step_seed [u8;2]               |  73    |  2   |
//! | pair_type u8                       |  75    |  1   |
//! | active_id i32                      |  76    |  4   |
//! | bin_step u16                       |  80    |  2   |
//! | status u8                          |  82    |  1   |
//! | (smaller fields)                   |  83-87 |      |
//! | token_x_mint pubkey                |  88    | 32   |
//! | token_y_mint pubkey                | 120    | 32   |
//! | reserve_x pubkey                   | 152    | 32   |
//! | reserve_y pubkey                   | 184    | 32   |
//! | (rest)                             |        |      |
//! | TOTAL                              | 904    |      |
//!
//! Inside `parameters` (32 bytes), we only care about:
//! | Field                  | Offset (rel to params) | Type |
//! |------------------------|------------------------|------|
//! | base_factor            | 0                      | u16  |
//! | base_fee_power_factor  | 26                     | u8   |
//!
//! ## Quote math — v1 active-bin spot approximation
//!
//! DLMM organises liquidity into discrete bins, each with a fixed price.
//! Per Meteora's docs:
//!
//! ```text
//! price_at_bin_id(i) = (1 + bin_step/10000)^i        // raw_Y per raw_X
//! base_fee_rate     = base_factor * bin_step * 10 * 10^base_fee_power_factor
//!                                                    // numerator over 10^9
//! ```
//!
//! Walking bins requires fetching the `BinArray` PDAs that hold each bin's
//! reserves — substantial extra subscription / RPC work that's deferred to
//! Phase 2. Like Whirlpool and Raydium CLMM, the v1 quoter reports the
//! infinite-depth spot price at the active bin, which is exact for small
//! swaps and overestimates for large swaps that would cross bins. Acceptable
//! for opportunity *detection*; not for production execution.
//!
//! [`MeteoraAg/dlmm-sdk/idls/dlmm.json`]: https://github.com/MeteoraAg/dlmm-sdk

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::pubkey::Pubkey;

use crate::{known_decimals, METEORA_DLMM_PROGRAM};

/// SPL Token program (classic, not Token-2022). DLMM v0.10.x pools all use
/// the classic Token program; if a future pool migrates to Token-2022 the
/// caller should pass `token_x_program` / `token_y_program` directly.
pub const SPL_TOKEN_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// SPL Memo (v2). The DLMM `swap` instruction historically didn't require a
/// memo account, but several IDL revisions list it as a no-op; we don't
/// include it here to stay within the v0.10.x layout.
#[allow(dead_code)]
pub const SPL_MEMO_PROGRAM: Pubkey =
    solana_program::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// Anchor discriminator for `global:swap`. Computed via
/// sha256("global:swap")[..8] = `f8 c6 9e 91 e1 75 87 c8`.
pub const DLMM_SWAP_DISCRIMINATOR: [u8; 8] =
    [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Each `BinArray` covers exactly 70 bins. To find which BinArray covers
/// a given bin id, divide using flooring division (i32::div_euclid).
pub const DLMM_BINS_PER_ARRAY: i32 = 70;

/// PDA-seed prefixes used by the DLMM program. Constants kept here to make
/// the seed strings testable and tracked alongside the layout.
pub const DLMM_BIN_ARRAY_SEED:        &[u8] = b"bin_array";
pub const DLMM_ORACLE_SEED:           &[u8] = b"oracle";
pub const DLMM_BITMAP_EXT_SEED:       &[u8] = b"bitmap";
pub const DLMM_EVENT_AUTHORITY_SEED:  &[u8] = b"__event_authority";

/// Compute the BinArray index that covers the given bin id. DLMM uses
/// flooring division so that negative bin ids round toward -∞ (matching
/// the on-chain seed derivation).
#[inline]
pub fn bin_array_index_for(bin_id: i32) -> i32 {
    bin_id.div_euclid(DLMM_BINS_PER_ARRAY)
}

/// Derive the BinArray PDA for `lb_pair` at array index `idx`.
/// Seeds: `["bin_array", lb_pair, idx.to_le_bytes()]` where `idx` is i64.
pub fn derive_bin_array_pda(lb_pair: &Pubkey, idx: i32) -> Pubkey {
    // DLMM stores bin_array index as i64 in the seed; widen here.
    let idx_i64 = idx as i64;
    let (pda, _bump) = Pubkey::find_program_address(
        &[
            DLMM_BIN_ARRAY_SEED,
            lb_pair.as_ref(),
            &idx_i64.to_le_bytes(),
        ],
        &METEORA_DLMM_PROGRAM,
    );
    pda
}

/// Derive the oracle PDA for an LbPair: `["oracle", lb_pair]`.
pub fn derive_oracle_pda(lb_pair: &Pubkey) -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(
        &[DLMM_ORACLE_SEED, lb_pair.as_ref()],
        &METEORA_DLMM_PROGRAM,
    );
    pda
}

/// Derive the BitmapExtension PDA: `["bitmap", lb_pair]`. Many pools never
/// instantiate this account (it's only created when bins span beyond the
/// default range); callers should treat it as optional and substitute the
/// program id when not present on chain.
pub fn derive_bitmap_extension_pda(lb_pair: &Pubkey) -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(
        &[DLMM_BITMAP_EXT_SEED, lb_pair.as_ref()],
        &METEORA_DLMM_PROGRAM,
    );
    pda
}

/// Derive the global event_authority PDA: `["__event_authority"]`. Single
/// PDA shared across the program; constant for the lifetime of DLMM.
pub fn derive_event_authority_pda() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(
        &[DLMM_EVENT_AUTHORITY_SEED],
        &METEORA_DLMM_PROGRAM,
    );
    pda
}

/// Account list for the `swap` instruction (v0.10.x layout). The IDL
/// orders accounts strictly; reordering or dropping any of them causes the
/// program to abort with `InstructionDidNotDeserialize` (Anchor 102) or
/// `AccountNotInitialized` (Anchor 3012). `bin_array_bitmap_extension`
/// and `host_fee_in` are optional in the IDL — when not present on chain,
/// pass `METEORA_DLMM_PROGRAM` as a benign placeholder (Anchor recognises
/// the program id as the `Option::None` sentinel).
#[derive(Debug, Clone)]
pub struct DlmmSwapAccounts {
    pub lb_pair:                     Pubkey,
    pub bin_array_bitmap_extension:  Pubkey, // pass DLMM_PROGRAM if not on chain
    pub reserve_x:                   Pubkey,
    pub reserve_y:                   Pubkey,
    pub user_token_in:               Pubkey,
    pub user_token_out:              Pubkey,
    pub token_x_mint:                Pubkey,
    pub token_y_mint:                Pubkey,
    pub oracle:                      Pubkey,
    pub host_fee_in:                 Pubkey, // pass DLMM_PROGRAM if not used
    pub user:                        Pubkey,
    pub token_x_program:             Pubkey, // SPL_TOKEN_PROGRAM for our pools
    pub token_y_program:             Pubkey,
    pub event_authority:             Pubkey,
    /// Bin arrays at the active range — usually 1 to 3, must include the
    /// active bin's array. All listed accounts must exist on chain (callers
    /// should skip non-existent BinArrays via getMultipleAccounts).
    pub bin_arrays:                  Vec<Pubkey>,
}

/// Build the DLMM `swap` instruction.
///
/// `amount_in` and `min_amount_out` are raw token base units. The
/// instruction data is `[disc(8) | amount_in(8) | min_out(8)]`, total 24 bytes.
pub fn build_dlmm_swap_ix(
    accs:           &DlmmSwapAccounts,
    amount_in:      u64,
    min_amount_out: u64,
) -> ArbResult<Instruction> {
    if accs.bin_arrays.is_empty() {
        return Err(ArbError::Invariant("dlmm: at least one bin_array required"));
    }
    if accs.bin_arrays.len() > 4 {
        return Err(ArbError::Invariant("dlmm: too many bin_arrays (max 4 to fit tx limit)"));
    }

    // Anchor account order — see DlmmSwapAccounts doc comment.
    let mut metas = Vec::with_capacity(15 + accs.bin_arrays.len());
    metas.push(AccountMeta::new(accs.lb_pair, false));
    metas.push(AccountMeta::new_readonly(accs.bin_array_bitmap_extension, false));
    metas.push(AccountMeta::new(accs.reserve_x, false));
    metas.push(AccountMeta::new(accs.reserve_y, false));
    metas.push(AccountMeta::new(accs.user_token_in, false));
    metas.push(AccountMeta::new(accs.user_token_out, false));
    metas.push(AccountMeta::new_readonly(accs.token_x_mint, false));
    metas.push(AccountMeta::new_readonly(accs.token_y_mint, false));
    metas.push(AccountMeta::new(accs.oracle, false));
    metas.push(AccountMeta::new_readonly(accs.host_fee_in, false));
    metas.push(AccountMeta::new_readonly(accs.user, true));            // signer
    metas.push(AccountMeta::new_readonly(accs.token_x_program, false));
    metas.push(AccountMeta::new_readonly(accs.token_y_program, false));
    metas.push(AccountMeta::new_readonly(accs.event_authority, false));
    metas.push(AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false)); // program id ref
    // remaining_accounts: each bin_array is writable (swap mutates bin
    // liquidity).
    for ba in &accs.bin_arrays {
        metas.push(AccountMeta::new(*ba, false));
    }

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&DLMM_SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    Ok(Instruction { program_id: METEORA_DLMM_PROGRAM, accounts: metas, data })
}

pub const METEORA_DLMM_DISCRIMINATOR: [u8; 8] = [33, 11, 49, 98, 181, 101, 177, 13];
pub const METEORA_DLMM_ACCOUNT_SIZE: usize = 904;

const M_OFF_PARAMETERS:   usize = 8;
const M_OFF_ACTIVE_ID:    usize = 76;
const M_OFF_BIN_STEP:     usize = 80;
const M_OFF_TOKEN_X_MINT: usize = 88;
const M_OFF_TOKEN_Y_MINT: usize = 120;
const M_OFF_RESERVE_X:    usize = 152;
const M_OFF_RESERVE_Y:    usize = 184;

// Inside parameters (which itself starts at M_OFF_PARAMETERS):
const M_PARAM_OFF_BASE_FACTOR:        usize = 0;
const M_PARAM_OFF_BASE_FEE_POWER:     usize = 26;

/// Fee denominator for DLMM base + variable fee. The IDL doc says the rate is
/// `base_factor * bin_step * 10 * 10^base_fee_power_factor`; dividing by 1e9
/// yields a fraction. Verified against typical SOL/USDC DLMM pools where
/// `base_factor=5000, bin_step=20, power=0 → 1_000_000 / 1e9 = 0.1% = 10 bp`.
pub const DLMM_FEE_DENOMINATOR: u64 = 1_000_000_000;

#[derive(Debug, Clone, Copy)]
pub struct LbPairData {
    pub base_factor:           u16,
    pub base_fee_power_factor: u8,
    pub active_id:             i32,
    pub bin_step:              u16,
    pub token_x_mint:          Pubkey,
    pub token_y_mint:          Pubkey,
    pub reserve_x:             Pubkey,
    pub reserve_y:             Pubkey,
}

impl LbPairData {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() != METEORA_DLMM_ACCOUNT_SIZE { return None; }
        if data[..8] != METEORA_DLMM_DISCRIMINATOR { return None; }

        let read_u16 = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        let read_i32 = |o: usize| {
            let mut b = [0u8; 4];
            b.copy_from_slice(&data[o..o + 4]);
            i32::from_le_bytes(b)
        };
        let read_pk = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o + 32]);
            Pubkey::new_from_array(b)
        };

        let params_off = M_OFF_PARAMETERS;
        Some(LbPairData {
            base_factor:           read_u16(params_off + M_PARAM_OFF_BASE_FACTOR),
            base_fee_power_factor: data[params_off + M_PARAM_OFF_BASE_FEE_POWER],
            active_id:             read_i32(M_OFF_ACTIVE_ID),
            bin_step:              read_u16(M_OFF_BIN_STEP),
            token_x_mint:          read_pk(M_OFF_TOKEN_X_MINT),
            token_y_mint:          read_pk(M_OFF_TOKEN_Y_MINT),
            reserve_x:             read_pk(M_OFF_RESERVE_X),
            reserve_y:             read_pk(M_OFF_RESERVE_Y),
        })
    }
}

/// Active-bin price in raw token units: raw_Y per raw_X.
pub fn active_bin_price_raw(bin_step: u16, active_id: i32) -> f64 {
    let q = 1.0 + (bin_step as f64) / 10_000.0;
    // f64::powi handles negative exponents; for the bin-id ranges we see in
    // practice (-440k..+440k) this stays well within f64 precision for any
    // realistic bin_step (≤ 200 = 2%).
    q.powi(active_id)
}

/// Base-fee fraction (e.g. 0.001 = 0.1%) per Meteora's formula.
pub fn base_fee_fraction(base_factor: u16, bin_step: u16, power_factor: u8) -> f64 {
    let pow = 10u64.saturating_pow(power_factor as u32);
    let rate = (base_factor as u64)
        .saturating_mul(bin_step as u64)
        .saturating_mul(10)
        .saturating_mul(pow);
    (rate as f64) / DLMM_FEE_DENOMINATOR as f64
}

/// Spot quote at the active bin, applying base fee.
///
/// `direction_x_to_y`: input is token_x if true, else input is token_y.
/// `decimals_x`/`decimals_y`: needed to convert raw price into a UI-aware quote.
#[allow(clippy::too_many_arguments)]
pub fn spot_quote(
    input:            u64,
    bin_step:         u16,
    active_id:        i32,
    base_factor:      u16,
    power_factor:     u8,
    direction_x_to_y: bool,
    decimals_x:       u8,
    decimals_y:       u8,
) -> ArbResult<u64> {
    if input == 0 { return Ok(0); }
    if bin_step == 0 {
        return Err(ArbError::Invariant("bin_step is zero"));
    }

    let price_raw = active_bin_price_raw(bin_step, active_id);
    if !price_raw.is_finite() || price_raw <= 0.0 {
        return Err(ArbError::Invariant("non-positive bin price"));
    }
    let fee = base_fee_fraction(base_factor, bin_step, power_factor);
    if fee >= 1.0 { return Err(ArbError::Invariant("fee >= 100%")); }
    let fee_factor = 1.0 - fee;

    let input_f = input as f64;
    let _ = (decimals_x, decimals_y); // raw price is already raw-per-raw, decimals only matter for human display
    let out_f = if direction_x_to_y {
        input_f * price_raw * fee_factor
    } else {
        input_f / price_raw * fee_factor
    };

    if !out_f.is_finite() || out_f < 0.0 { return Err(ArbError::QuoteOverflow); }
    if out_f > u64::MAX as f64 { return Err(ArbError::QuoteOverflow); }
    Ok(out_f as u64)
}

#[derive(Default)]
pub struct MeteoraDlmmQuoter;

impl MeteoraDlmmQuoter {
    pub fn new() -> Self { Self }
}

#[async_trait::async_trait]
impl DexQuoter for MeteoraDlmmQuoter {
    fn dex_id(&self) -> &'static str { PoolKind::MeteoraDlmm.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = LbPairData::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "meteora-dlmm",
                reason: "discriminator mismatch or wrong size".into(),
            })?;

        let direction_x_to_y = match input.mint.0 {
            x if x == parsed.token_x_mint => true,
            x if x == parsed.token_y_mint => false,
            other => return Err(ArbError::MintMismatch {
                expected: parsed.token_x_mint,
                actual:   other,
            }),
        };

        let dec_x = known_decimals(&parsed.token_x_mint).unwrap_or(input.decimals);
        let dec_y = known_decimals(&parsed.token_y_mint).unwrap_or(input.decimals);

        let out_amount = spot_quote(
            input.amount,
            parsed.bin_step,
            parsed.active_id,
            parsed.base_factor,
            parsed.base_fee_power_factor,
            direction_x_to_y,
            dec_x,
            dec_y,
        )?;

        let (out_mint, out_decimals) = if direction_x_to_y {
            (parsed.token_y_mint, dec_y)
        } else {
            (parsed.token_x_mint, dec_x)
        };
        Ok(TokenAmount::new(TokenMint(out_mint), out_amount, out_decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        // Caller doesn't have the bin_arrays / oracle / event_authority that
        // the on-chain layout requires. The native executor reaches in via
        // the lower-level `build_dlmm_swap_ix` after fetching bin arrays
        // from the chain. This trait method intentionally returns Invariant
        // so callers don't accidentally build an incomplete instruction.
        Err(ArbError::Invariant(
            "dlmm: use build_dlmm_swap_ix() — DexQuoter trait can't supply bin_arrays",
        ))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = LbPairData::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::MeteoraDlmm,
            TokenMint(parsed.token_x_mint),
            TokenMint(parsed.token_y_mint),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

pub fn is_meteora_dlmm(program_id: &Pubkey) -> bool {
    *program_id == METEORA_DLMM_PROGRAM
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn fake_lbpair(
        base_factor: u16, base_fee_power: u8, bin_step: u16, active_id: i32,
        mint_x: Pubkey, mint_y: Pubkey,
    ) -> Vec<u8> {
        let mut data = vec![0u8; METEORA_DLMM_ACCOUNT_SIZE];
        data[..8].copy_from_slice(&METEORA_DLMM_DISCRIMINATOR);
        // parameters.base_factor @ 8
        data[M_OFF_PARAMETERS..M_OFF_PARAMETERS + 2].copy_from_slice(&base_factor.to_le_bytes());
        data[M_OFF_PARAMETERS + M_PARAM_OFF_BASE_FEE_POWER] = base_fee_power;
        data[M_OFF_ACTIVE_ID..M_OFF_ACTIVE_ID + 4].copy_from_slice(&active_id.to_le_bytes());
        data[M_OFF_BIN_STEP..M_OFF_BIN_STEP + 2].copy_from_slice(&bin_step.to_le_bytes());
        data[M_OFF_TOKEN_X_MINT..M_OFF_TOKEN_X_MINT + 32].copy_from_slice(&mint_x.to_bytes());
        data[M_OFF_TOKEN_Y_MINT..M_OFF_TOKEN_Y_MINT + 32].copy_from_slice(&mint_y.to_bytes());
        data
    }

    #[test]
    fn parse_valid_lbpair() {
        let mx = Pubkey::new_from_array([1u8; 32]);
        let my = Pubkey::new_from_array([2u8; 32]);
        let data = fake_lbpair(5000, 0, 20, 8388608, mx, my);
        let p = LbPairData::try_parse(&data).expect("parse");
        assert_eq!(p.base_factor, 5000);
        assert_eq!(p.base_fee_power_factor, 0);
        assert_eq!(p.bin_step, 20);
        assert_eq!(p.active_id, 8388608);
        assert_eq!(p.token_x_mint, mx);
        assert_eq!(p.token_y_mint, my);
    }

    #[test]
    fn parse_rejects_wrong_size() {
        assert!(LbPairData::try_parse(&[0u8; 100]).is_none());
        assert!(LbPairData::try_parse(&[0u8; 903]).is_none());
        assert!(LbPairData::try_parse(&[0u8; 905]).is_none());
    }

    #[test]
    fn parse_rejects_wrong_discriminator() {
        let mut data = fake_lbpair(5000, 0, 20, 0, Pubkey::default(), Pubkey::default());
        data[0] = 99;
        assert!(LbPairData::try_parse(&data).is_none());
    }

    #[test]
    fn active_bin_price_at_zero_is_one() {
        let p = active_bin_price_raw(20, 0);
        assert!((p - 1.0).abs() < 1e-12);
    }

    #[test]
    fn active_bin_price_doubling_check() {
        // (1.0001)^6932 ≈ 2.0 (since ln(2) / ln(1.0001) ≈ 6931)
        let p = active_bin_price_raw(1, 6932);
        assert!((p - 2.0).abs() < 0.01, "got {p}");
    }

    #[test]
    fn base_fee_fraction_typical_pool() {
        // SOL/USDC pool: base_factor=5000, bin_step=20, power=0
        //   rate = 5000 * 20 * 10 * 1 = 1_000_000
        //   fee  = 1_000_000 / 1e9 = 0.001 = 0.1%
        let f = base_fee_fraction(5000, 20, 0);
        assert!((f - 0.001).abs() < 1e-9);
    }

    #[test]
    fn base_fee_fraction_high_power_factor() {
        // base_factor=10000, bin_step=100, power=1 → 10000*100*10*10 = 100_000_000 → 10% fee
        let f = base_fee_fraction(10_000, 100, 1);
        assert!((f - 0.1).abs() < 1e-9);
    }

    #[test]
    fn spot_quote_at_price_1_subtracts_only_fee() {
        // active_id=0 ⇒ price=1, base_factor=5000 bin_step=20 ⇒ 0.1% fee.
        // 1_000_000 in → 999_000 out
        let out = spot_quote(1_000_000, 20, 0, 5000, 0, true, 6, 6).unwrap();
        assert!((out as i64 - 999_000).abs() <= 1, "got {out}");
    }

    #[test]
    fn spot_quote_zero_input_zero_output() {
        let out = spot_quote(0, 20, 0, 5000, 0, true, 6, 6).unwrap();
        assert_eq!(out, 0);
    }

    #[test]
    fn spot_quote_zero_bin_step_returns_invariant() {
        let r = spot_quote(1, 0, 0, 5000, 0, true, 6, 6);
        assert!(matches!(r, Err(ArbError::Invariant(_))));
    }

    #[test]
    fn quoter_dispatches_correct_direction() {
        let mx = Pubkey::new_from_array([1u8; 32]);
        let my = Pubkey::new_from_array([2u8; 32]);
        let data = fake_lbpair(5000, 0, 20, 0, mx, my);
        let q = MeteoraDlmmQuoter::new();
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).expect("parse");

        let in_x = TokenAmount::new(TokenMint(mx), 1_000_000, 6);
        let out = q.quote(&pool, in_x).unwrap();
        assert_eq!(out.mint.0, my);

        let in_y = TokenAmount::new(TokenMint(my), 1_000_000, 6);
        let out = q.quote(&pool, in_y).unwrap();
        assert_eq!(out.mint.0, mx);
    }

    #[test]
    fn bin_array_index_for_positive() {
        assert_eq!(bin_array_index_for(0),   0);
        assert_eq!(bin_array_index_for(69),  0);
        assert_eq!(bin_array_index_for(70),  1);
        assert_eq!(bin_array_index_for(139), 1);
        assert_eq!(bin_array_index_for(140), 2);
    }

    #[test]
    fn bin_array_index_for_negative_floors() {
        // div_euclid floors toward -∞, matching Rust semantics for negative
        // bin ids in DLMM (verified against dlmm-sdk reference).
        assert_eq!(bin_array_index_for(-1),   -1);
        assert_eq!(bin_array_index_for(-69),  -1);
        assert_eq!(bin_array_index_for(-70),  -1);
        assert_eq!(bin_array_index_for(-71),  -2);
        assert_eq!(bin_array_index_for(-140), -2);
        assert_eq!(bin_array_index_for(-141), -3);
    }

    #[test]
    fn pdas_are_deterministic() {
        let lb = Pubkey::new_from_array([7u8; 32]);
        // Same inputs → same PDA.
        assert_eq!(derive_oracle_pda(&lb), derive_oracle_pda(&lb));
        assert_eq!(derive_event_authority_pda(), derive_event_authority_pda());
        assert_eq!(derive_bin_array_pda(&lb, 5), derive_bin_array_pda(&lb, 5));
        // Different lb_pair → different PDA.
        let lb2 = Pubkey::new_from_array([9u8; 32]);
        assert_ne!(derive_oracle_pda(&lb), derive_oracle_pda(&lb2));
        // Different idx → different PDA.
        assert_ne!(derive_bin_array_pda(&lb, 5), derive_bin_array_pda(&lb, 6));
    }

    #[test]
    fn build_swap_ix_data_layout() {
        let pk = Pubkey::new_unique();
        let accs = DlmmSwapAccounts {
            lb_pair: pk, bin_array_bitmap_extension: METEORA_DLMM_PROGRAM,
            reserve_x: pk, reserve_y: pk,
            user_token_in: pk, user_token_out: pk,
            token_x_mint: pk, token_y_mint: pk,
            oracle: pk, host_fee_in: METEORA_DLMM_PROGRAM,
            user: pk, token_x_program: SPL_TOKEN_PROGRAM, token_y_program: SPL_TOKEN_PROGRAM,
            event_authority: pk,
            bin_arrays: vec![pk, pk],
        };
        let ix = build_dlmm_swap_ix(&accs, 1_000_000_000, 999_000_000).unwrap();
        assert_eq!(ix.program_id, METEORA_DLMM_PROGRAM);
        // discriminator(8) + amount_in(8) + min_out(8) = 24 bytes
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &DLMM_SWAP_DISCRIMINATOR);
        // amount_in @ offset 8
        assert_eq!(&ix.data[8..16], &1_000_000_000u64.to_le_bytes());
        // min_out @ offset 16
        assert_eq!(&ix.data[16..24], &999_000_000u64.to_le_bytes());
        // 15 fixed accounts + 2 bin arrays = 17 metas
        assert_eq!(ix.accounts.len(), 17);
        // user is the only signer
        assert_eq!(ix.accounts.iter().filter(|m| m.is_signer).count(), 1);
    }

    #[test]
    fn build_swap_ix_rejects_zero_bin_arrays() {
        let pk = Pubkey::new_unique();
        let accs = DlmmSwapAccounts {
            lb_pair: pk, bin_array_bitmap_extension: METEORA_DLMM_PROGRAM,
            reserve_x: pk, reserve_y: pk,
            user_token_in: pk, user_token_out: pk,
            token_x_mint: pk, token_y_mint: pk,
            oracle: pk, host_fee_in: METEORA_DLMM_PROGRAM,
            user: pk, token_x_program: SPL_TOKEN_PROGRAM, token_y_program: SPL_TOKEN_PROGRAM,
            event_authority: pk,
            bin_arrays: vec![],
        };
        assert!(matches!(build_dlmm_swap_ix(&accs, 1, 1), Err(ArbError::Invariant(_))));
    }

    #[test]
    fn quoter_rejects_unknown_mint() {
        let mx = Pubkey::new_from_array([1u8; 32]);
        let my = Pubkey::new_from_array([2u8; 32]);
        let mc = Pubkey::new_from_array([3u8; 32]);
        let data = fake_lbpair(5000, 0, 20, 0, mx, my);
        let q = MeteoraDlmmQuoter::new();
        let pool = q.try_parse_account(&Pubkey::new_unique(), &data).expect("parse");
        let amt = TokenAmount::new(TokenMint(mc), 1, 6);
        assert!(matches!(q.quote(&pool, amt), Err(ArbError::MintMismatch { .. })));
    }
}
