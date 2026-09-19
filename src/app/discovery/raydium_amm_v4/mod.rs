use crate::app::discovery::ProgramTransactionDiscovery;
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

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
    async fn handle(&self, update: SubscribeUpdateTransaction) -> Result<()> {
        Ok(())
    }
}
