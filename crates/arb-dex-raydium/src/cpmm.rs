//! Raydium CPMM (constant-product market maker, program
//! `CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C`).
//!
//! Newer than Raydium AMM v4. Same constant-product math (`x * y = k`) but
//! different on-chain layout and instruction interface. Critically, the
//! CPMM program does NOT use OpenBook markets — reserves live entirely in
//! the pool's two vault accounts.
//!
//! ## Why we need it (May 2026)
//!
//! GeckoTerminal lists pools as "raydium" without distinguishing CPMM from
//! AMM v4. We bootstrapped a JTO/SOL pool that turned out to be CPMM and
//! got `bootstrap: no parser claimed this account` because the AMM v4
//! parser rejected the data length / discriminator mismatch.
//!
//! ## Account layout (`PoolState`, 576 + 8 disc bytes = 584 total)
//!
//! Source: `raydium-cp-swap/programs/cp-swap/src/states/pool.rs`. Anchor
//! struct, no padding. Fields in declaration order:
//!
//! | Offset | Size | Field                  |
//! |--------|------|------------------------|
//! |   0    |   8  | account discriminator  |
//! |   8    |  32  | amm_config             |
//! |  40    |  32  | pool_creator           |
//! |  72    |  32  | token_0_vault          |
//! | 104    |  32  | token_1_vault          |
//! | 136    |  32  | lp_mint                |
//! | 168    |  32  | token_0_mint           |
//! | 200    |  32  | token_1_mint           |
//! | 232    |  32  | token_0_program        |
//! | 264    |  32  | token_1_program        |
//! | 296    |  32  | observation_key        |
//! | 328    |   1  | auth_bump              |
//! | 329    |   1  | status                 |
//! | 330    |   1  | lp_mint_decimals       |
//! | 331    |   1  | mint_0_decimals        |
//! | 332    |   1  | mint_1_decimals        |
//! | 333    |   8  | lp_supply              |
//! | 341    |   8  | protocol_fees_token_0  |
//! | 349    |   8  | protocol_fees_token_1  |
//! | 357    |   8  | fund_fees_token_0      |
//! | 365    |   8  | fund_fees_token_1      |
//! | 373    |   8  | open_time              |
//! | (rest is recent_epoch + creator-fee fields + padding — unused here)
//!
//! Total verified observed length on chain: **637 bytes** (matches
//! `data_len=637` we saw in the bootstrap warn log).

use std::sync::Arc;

use arb_core::{ArbError, ArbResult, DexQuoter, Pool, PoolKind, PoolState, TokenAmount, TokenMint};
use solana_program::instruction::Instruction;
use solana_program::pubkey::Pubkey;

use crate::program_ids::RAYDIUM_CPMM_PROGRAM;
use crate::VaultLookup;

/// Anchor discriminator: `sha256("account:PoolState")[..8]`.
pub const RAYDIUM_CPMM_POOL_DISCRIMINATOR: [u8; 8] =
    [0xf7, 0xed, 0xe3, 0xf5, 0xd7, 0xc3, 0xde, 0x46];

/// Minimum length of a valid CPMM PoolState account. The on-chain Anchor
/// account ends with a 28-u64 padding region for future expansion; the
/// total observed length is 637 bytes, but we accept anything ≥ this
/// minimum so future field additions don't break parsing.
pub const RAYDIUM_CPMM_ACCOUNT_MIN_SIZE: usize = 381;

/// Field offsets relative to the start of the account `data` (including
/// the 8-byte Anchor discriminator).
const O_AMM_CONFIG:        usize =   8;
const O_TOKEN_0_VAULT:     usize =  72;
const O_TOKEN_1_VAULT:     usize = 104;
const O_TOKEN_0_MINT:      usize = 168;
const O_TOKEN_1_MINT:      usize = 200;
const O_TOKEN_0_PROGRAM:   usize = 232;
const O_TOKEN_1_PROGRAM:   usize = 264;
const O_OBSERVATION_KEY:   usize = 296;
const O_MINT_0_DECIMALS:   usize = 331;
const O_MINT_1_DECIMALS:   usize = 332;

/// Parsed snapshot of a Raydium CPMM `PoolState`. Only the fields the
/// quoter and ix-builder need are extracted; the rest are skipped.
#[derive(Debug, Clone, Copy)]
pub struct RaydiumCpmmData {
    pub amm_config:       Pubkey,
    pub token_0_vault:    Pubkey,
    pub token_1_vault:    Pubkey,
    pub token_0_mint:     Pubkey,
    pub token_1_mint:     Pubkey,
    pub token_0_program:  Pubkey,
    pub token_1_program:  Pubkey,
    pub observation_key:  Pubkey,
    pub mint_0_decimals:  u8,
    pub mint_1_decimals:  u8,
}

impl RaydiumCpmmData {
    pub fn try_parse(data: &[u8]) -> Option<Self> {
        if data.len() < RAYDIUM_CPMM_ACCOUNT_MIN_SIZE { return None; }
        if data[..8] != RAYDIUM_CPMM_POOL_DISCRIMINATOR { return None; }

        let read_pk = |o: usize| {
            let mut b = [0u8; 32];
            b.copy_from_slice(&data[o..o + 32]);
            Pubkey::new_from_array(b)
        };

        Some(RaydiumCpmmData {
            amm_config:       read_pk(O_AMM_CONFIG),
            token_0_vault:    read_pk(O_TOKEN_0_VAULT),
            token_1_vault:    read_pk(O_TOKEN_1_VAULT),
            token_0_mint:     read_pk(O_TOKEN_0_MINT),
            token_1_mint:     read_pk(O_TOKEN_1_MINT),
            token_0_program:  read_pk(O_TOKEN_0_PROGRAM),
            token_1_program:  read_pk(O_TOKEN_1_PROGRAM),
            observation_key:  read_pk(O_OBSERVATION_KEY),
            mint_0_decimals:  data[O_MINT_0_DECIMALS],
            mint_1_decimals:  data[O_MINT_1_DECIMALS],
        })
    }
}

/// Constant-product quote at given vault reserves.
///
/// `r_in`, `r_out`: vault balances (token base units) for input/output sides.
/// `fee_bps`: pool trading fee in 1/10000 (e.g. 25 = 0.25%). CPMM fees are
/// loaded from the `amm_config` account; the caller supplies the resolved
/// numeric fee here.
pub fn cpmm_quote(amount_in: u64, r_in: u64, r_out: u64, fee_bps: u32) -> ArbResult<u64> {
    if amount_in == 0 { return Ok(0); }
    if r_in == 0 || r_out == 0 {
        return Err(ArbError::InsufficientLiquidity {
            pool: Pubkey::default(),
            amount: amount_in,
        });
    }
    // Apply fee on input.
    // amount_in_after_fee = amount_in * (10_000 - fee_bps) / 10_000
    let factor = (10_000u128).checked_sub(fee_bps as u128)
        .ok_or(ArbError::Invariant("cpmm: fee_bps > 10000"))?;
    let amount_in_post: u128 = (amount_in as u128) * factor / 10_000;

    // out = r_out * amount_in_post / (r_in + amount_in_post)
    let denom = (r_in as u128).checked_add(amount_in_post)
        .ok_or(ArbError::QuoteOverflow)?;
    let num   = (r_out as u128).checked_mul(amount_in_post)
        .ok_or(ArbError::QuoteOverflow)?;
    let out   = num / denom;
    if out > u64::MAX as u128 { return Err(ArbError::QuoteOverflow); }
    Ok(out as u64)
}

/// Anchor discriminator for `swap_base_input` (specify exact input amount).
pub const CPMM_SWAP_BASE_INPUT_DISC: [u8; 8] =
    [0x8f, 0xbe, 0x5a, 0xda, 0xc4, 0x1e, 0x33, 0xde];

/// Account layout for the `swap_base_input` instruction. Order matches the
/// CPMM program's IDL (`raydium-cp-swap/programs/cp-swap/src/instructions/swap_base_input.rs`).
#[derive(Debug, Clone)]
pub struct RaydiumCpmmSwapAccounts {
    pub payer:                Pubkey,        // signer
    pub authority:            Pubkey,        // CPMM auth PDA
    pub amm_config:           Pubkey,
    pub pool_state:           Pubkey,
    pub user_input_token:     Pubkey,        // user ATA (input side)
    pub user_output_token:    Pubkey,        // user ATA (output side)
    pub input_vault:          Pubkey,
    pub output_vault:         Pubkey,
    pub input_token_program:  Pubkey,
    pub output_token_program: Pubkey,
    pub input_token_mint:     Pubkey,
    pub output_token_mint:    Pubkey,
    pub observation_state:    Pubkey,
}

/// Derive the CPMM authority PDA. Seed: `b"vault_and_lp_mint_auth_seed"`.
pub fn derive_cpmm_authority() -> Pubkey {
    let (pda, _bump) = Pubkey::find_program_address(
        &[b"vault_and_lp_mint_auth_seed"],
        &RAYDIUM_CPMM_PROGRAM,
    );
    pda
}

/// Build the `swap_base_input` instruction.
///
/// `amount_in`/`min_amount_out`: raw token base units. Data layout is
/// `[disc(8) | amount_in(8) | min_amount_out(8)]`.
pub fn build_cpmm_swap_base_input_ix(
    accs:           &RaydiumCpmmSwapAccounts,
    amount_in:      u64,
    min_amount_out: u64,
) -> ArbResult<solana_program::instruction::Instruction> {
    use solana_program::instruction::{AccountMeta, Instruction};

    let metas = vec![
        AccountMeta::new(accs.payer, true),
        AccountMeta::new_readonly(accs.authority, false),
        AccountMeta::new_readonly(accs.amm_config, false),
        AccountMeta::new(accs.pool_state, false),
        AccountMeta::new(accs.user_input_token, false),
        AccountMeta::new(accs.user_output_token, false),
        AccountMeta::new(accs.input_vault, false),
        AccountMeta::new(accs.output_vault, false),
        AccountMeta::new_readonly(accs.input_token_program, false),
        AccountMeta::new_readonly(accs.output_token_program, false),
        AccountMeta::new_readonly(accs.input_token_mint, false),
        AccountMeta::new_readonly(accs.output_token_mint, false),
        AccountMeta::new(accs.observation_state, false),
    ];

    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&CPMM_SWAP_BASE_INPUT_DISC);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());

    Ok(Instruction { program_id: RAYDIUM_CPMM_PROGRAM, accounts: metas, data })
}

/// CPMM quoter that pulls vault balances from a shared `VaultLookup` and
/// runs constant-product math. Default fee = 25 bp (Raydium's standard CPMM
/// pool fee tier); production code should resolve the actual fee from the
/// pool's `amm_config` account, but for opportunity detection 25 bp is a
/// good approximation.
pub struct RaydiumCpmmQuoter {
    vaults:  Arc<dyn VaultLookup>,
    fee_bps: u32,
}

impl RaydiumCpmmQuoter {
    pub fn new(vaults: Arc<dyn VaultLookup>) -> Self {
        Self { vaults, fee_bps: 25 }
    }

    pub fn with_fee_bps(vaults: Arc<dyn VaultLookup>, fee_bps: u32) -> Self {
        Self { vaults, fee_bps }
    }
}

#[async_trait::async_trait]
impl DexQuoter for RaydiumCpmmQuoter {
    fn dex_id(&self) -> &'static str { PoolKind::RaydiumCpmm.id() }

    fn quote(&self, pool: &Pool, input: TokenAmount) -> ArbResult<TokenAmount> {
        let parsed = RaydiumCpmmData::try_parse(pool.state.as_slice())
            .ok_or_else(|| ArbError::AccountParse {
                dex: "raydium-cpmm",
                reason: "discriminator mismatch / size".into(),
            })?;

        let (in_vault, out_vault, out_mint, out_decimals) =
            if input.mint.0 == parsed.token_0_mint {
                (parsed.token_0_vault, parsed.token_1_vault, parsed.token_1_mint, parsed.mint_1_decimals)
            } else if input.mint.0 == parsed.token_1_mint {
                (parsed.token_1_vault, parsed.token_0_vault, parsed.token_0_mint, parsed.mint_0_decimals)
            } else {
                return Err(ArbError::MintMismatch {
                    expected: parsed.token_0_mint,
                    actual:   input.mint.0,
                });
            };

        let r_in  = self.vaults.get(&in_vault).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount })?;
        let r_out = self.vaults.get(&out_vault).ok_or(
            ArbError::InsufficientLiquidity { pool: pool.address, amount: input.amount })?;

        let out = cpmm_quote(input.amount, r_in, r_out, self.fee_bps)?;
        Ok(TokenAmount::new(TokenMint(out_mint), out, out_decimals))
    }

    fn build_swap_ix(
        &self, _pool: &Pool, _input: TokenAmount, _min_output: TokenAmount, _user: Pubkey,
    ) -> ArbResult<Instruction> {
        // Use `build_cpmm_swap_base_input_ix` directly — needs more accounts
        // (authority PDA, observation_state, etc.) than the trait surface.
        Err(ArbError::Invariant(
            "cpmm: use build_cpmm_swap_base_input_ix() — DexQuoter trait too thin",
        ))
    }

    fn try_parse_account(&self, pubkey: &Pubkey, data: &[u8]) -> Option<Pool> {
        let parsed = RaydiumCpmmData::try_parse(data)?;
        Some(Pool::new(
            *pubkey,
            PoolKind::RaydiumCpmm,
            TokenMint(parsed.token_0_mint),
            TokenMint(parsed.token_1_mint),
            PoolState::new(data.to_vec()),
            0,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_min_size_with_correct_discriminator() {
        let mut data = vec![0u8; 600];
        data[..8].copy_from_slice(&RAYDIUM_CPMM_POOL_DISCRIMINATOR);
        let mint0 = Pubkey::new_unique();
        let mint1 = Pubkey::new_unique();
        data[O_TOKEN_0_MINT..O_TOKEN_0_MINT + 32].copy_from_slice(&mint0.to_bytes());
        data[O_TOKEN_1_MINT..O_TOKEN_1_MINT + 32].copy_from_slice(&mint1.to_bytes());
        data[O_MINT_0_DECIMALS] = 9;
        data[O_MINT_1_DECIMALS] = 6;
        let p = RaydiumCpmmData::try_parse(&data).expect("parse");
        assert_eq!(p.token_0_mint, mint0);
        assert_eq!(p.token_1_mint, mint1);
        assert_eq!(p.mint_0_decimals, 9);
        assert_eq!(p.mint_1_decimals, 6);
    }

    #[test]
    fn rejects_short_data() {
        assert!(RaydiumCpmmData::try_parse(&[0u8; 100]).is_none());
    }

    #[test]
    fn rejects_wrong_discriminator() {
        let mut data = vec![0u8; 600];
        data[0] = 0xAA;
        assert!(RaydiumCpmmData::try_parse(&data).is_none());
    }

    #[test]
    fn cpmm_quote_zero_in_zero_out() {
        let q = cpmm_quote(0, 1_000_000, 1_000_000, 25).unwrap();
        assert_eq!(q, 0);
    }

    #[test]
    fn cpmm_quote_round_trip_invariant() {
        // 1M each side, 25bp fee, swap 1000 in.
        // out = 999_750 * 1_000_000 / (1_000_000 + 999_750)
        //     ≈ 499,937
        let q = cpmm_quote(1_000, 1_000_000, 1_000_000, 25).unwrap();
        // Must be positive, less than r_out, and consistent with x*y=k
        assert!(q > 0);
        assert!(q < 1_000_000);
        // Round-trip should NOT regenerate exactly 1000 (fee leaks).
        let back = cpmm_quote(q, 1_000_000 - q, 1_001_000, 25).unwrap();
        assert!(back < 1_000);
    }

    #[test]
    fn build_swap_ix_emits_correct_layout() {
        let accs = RaydiumCpmmSwapAccounts {
            payer: Pubkey::new_unique(),
            authority: derive_cpmm_authority(),
            amm_config: Pubkey::new_unique(),
            pool_state: Pubkey::new_unique(),
            user_input_token:  Pubkey::new_unique(),
            user_output_token: Pubkey::new_unique(),
            input_vault:  Pubkey::new_unique(),
            output_vault: Pubkey::new_unique(),
            input_token_program:  Pubkey::new_unique(),
            output_token_program: Pubkey::new_unique(),
            input_token_mint:  Pubkey::new_unique(),
            output_token_mint: Pubkey::new_unique(),
            observation_state: Pubkey::new_unique(),
        };
        let ix = build_cpmm_swap_base_input_ix(&accs, 1_000_000, 990_000).unwrap();
        assert_eq!(ix.program_id, RAYDIUM_CPMM_PROGRAM);
        assert_eq!(ix.data.len(), 24);
        assert_eq!(&ix.data[..8], &CPMM_SWAP_BASE_INPUT_DISC);
        assert_eq!(&ix.data[8..16], &1_000_000u64.to_le_bytes());
        assert_eq!(&ix.data[16..24], &990_000u64.to_le_bytes());
        // 13 fixed accounts in spec order.
        assert_eq!(ix.accounts.len(), 13);
        assert!(ix.accounts[0].is_signer);   // payer
    }

    #[test]
    fn authority_pda_is_deterministic() {
        let a = derive_cpmm_authority();
        let b = derive_cpmm_authority();
        assert_eq!(a, b);
    }
}
