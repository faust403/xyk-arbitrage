use crate::app::discovery::ProgramAccountDiscovery;
use crate::app::discovery::ProgramTransactionDiscovery;
use anyhow::Context;
use anyhow::Result;
use solana_pubkey::Pubkey;
use std::mem::offset_of;
use yellowstone_grpc_proto::geyser::SubscribeUpdateAccount;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

pub struct RaydiumAMMv4Discovery {}

impl RaydiumAMMv4Discovery {
    pub fn new() -> Result<Self> {
        Ok(Self {})
    }
}

#[async_trait]
impl ProgramTransactionDiscovery for RaydiumAMMv4Discovery {
    async fn handle(&self, update: SubscribeUpdateTransaction) -> Result<Vec<String>> {
        let mut pools = Vec::new();
        let Some(info) = &update.transaction else {
            return Ok(pools);
        };
        let Some(message) = info.transaction.as_ref().and_then(|t| t.message.as_ref()) else {
            return Ok(pools);
        };
        let Some(meta) = &info.meta else {
            return Ok(pools);
        };
        /* Instruction account indexes address the static keys first, then the
        lookup-table keys in the order the runtime loaded them: writable, then readonly */
        let keys = message
            .account_keys
            .iter()
            .chain(&meta.loaded_writable_addresses)
            .chain(&meta.loaded_readonly_addresses)
            .collect::<Vec<_>>();
        let outer = message
            .instructions
            .iter()
            .map(|ix| (ix.program_id_index, &ix.accounts, &ix.data));
        let inner = meta
            .inner_instructions
            .iter()
            .flat_map(|set| &set.instructions)
            .map(|ix| (ix.program_id_index, &ix.accounts, &ix.data));
        for (program, accounts, data) in outer.chain(inner) {
            if keys.get(program as usize).map(|key| key.as_slice()) != Some(PROGRAM_ID.as_ref()) {
                continue;
            }
            let Some(position) = pool_account_position(data) else {
                continue;
            };
            let Some(key) = accounts.get(position).and_then(|i| keys.get(*i as usize)) else {
                continue;
            };
            let Ok(pool) = Pubkey::try_from(key.as_slice()).map(|pool| pool.to_string()) else {
                continue;
            };
            if !pools.contains(&pool) {
                pools.push(pool);
            }
        }
        Ok(pools)
    }
}

impl ProgramAccountDiscovery for RaydiumAMMv4Discovery {
    fn handle(&self, update: &SubscribeUpdateAccount) -> Result<Vec<String>> {
        let Some(info) = &update.account else {
            return Ok(vec![]);
        };
        /* Vault token accounts we subscribe to arrive on this same path; the owner and the
        exact length are what the program's own loader checks before trusting the bytes */
        if info.owner.as_slice() != PROGRAM_ID.as_ref() || info.data.len() != size_of::<AmmInfo>() {
            return Ok(vec![]);
        }
        Ok(vec![
            pubkey_at(&info.data, offset_of!(AmmInfo, coin_vault))?,
            pubkey_at(&info.data, offset_of!(AmmInfo, pc_vault))?,
        ])
    }
}

fn pubkey_at(data: &[u8], offset: usize) -> Result<String> {
    let bytes = data
        .get(offset..offset + size_of::<Pubkey>())
        .context("Pubkey offset out of range")?;
    Ok(Pubkey::try_from(bytes)?.to_string())
}

fn pool_account_position(data: &[u8]) -> Option<usize> {
    /* Initialize2 = 1, SetParams = 6, Deposit = 3, Withdraw = 4, WithdrawPnl = 7,
    SwapBaseIn = 9, SwapBaseOut = 11, SwapBaseInV2 = 16, SwapBaseOutV2 = 17 */
    match data.first()? {
        1 => Some(4),
        6 => Some(0),
        3 | 4 | 7 | 9 | 11 | 16 | 17 => Some(1),
        _ => None,
    }
}

/* Mirror of raydium-amm's state.rs so the vault offsets come from the compiler, not a table */
#[repr(C, packed)]
struct AmmInfo {
    status: u64,
    nonce: u64,
    order_num: u64,
    depth: u64,
    coin_decimals: u64,
    pc_decimals: u64,
    state: u64,
    reset_flag: u64,
    min_size: u64,
    vol_max_cut_ratio: u64,
    amount_wave: u64,
    coin_lot_size: u64,
    pc_lot_size: u64,
    min_price_multiplier: u64,
    max_price_multiplier: u64,
    sys_decimal_value: u64,
    fees: Fees,
    state_data: StateData,
    coin_vault: Pubkey,
    pc_vault: Pubkey,
    coin_vault_mint: Pubkey,
    pc_vault_mint: Pubkey,
    lp_mint: Pubkey,
    open_orders: Pubkey,
    market: Pubkey,
    market_program: Pubkey,
    target_orders: Pubkey,
    padding1: [u64; 8],
    amm_owner: Pubkey,
    lp_amount: u64,
    client_order_id: u64,
    recent_epoch: u64,
    padding2: u64,
}

#[repr(C, packed)]
struct Fees {
    min_separate_numerator: u64,
    min_separate_denominator: u64,
    trade_fee_numerator: u64,
    trade_fee_denominator: u64,
    pnl_numerator: u64,
    pnl_denominator: u64,
    swap_fee_numerator: u64,
    swap_fee_denominator: u64,
}

#[repr(C, packed)]
struct StateData {
    need_take_pnl_coin: u64,
    need_take_pnl_pc: u64,
    total_pnl_pc: u64,
    total_pnl_coin: u64,
    pool_open_time: u64,
    padding: [u64; 2],
    orderbook_to_init_time: u64,
    swap_coin_in_amount: u128,
    swap_pc_out_amount: u128,
    swap_acc_pc_fee: u64,
    swap_pc_in_amount: u128,
    swap_coin_out_amount: u128,
    swap_acc_coin_fee: u64,
}
