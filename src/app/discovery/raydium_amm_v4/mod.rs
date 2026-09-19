use crate::app::discovery::ProgramTransactionDiscovery;
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_pubkey::Pubkey;
use std::sync::Arc;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

pub const PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");

pub struct RaydiumAMMv4Discovery {
    rpc: Arc<RpcClient>,
}

impl RaydiumAMMv4Discovery {
    pub fn new(rpc: Arc<RpcClient>) -> Result<Self> {
        Ok(Self { rpc })
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
