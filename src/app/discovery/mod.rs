use crate::app::yellowstone::YellowstoneApp;
use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::{Arc, RwLock};
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

pub struct DiscoveryApp {
    yellowstone: Arc<RwLock<YellowstoneApp>>,
}

impl DiscoveryApp {
    pub fn new(yellowstone: Arc<RwLock<YellowstoneApp>>) -> Self {
        Self { yellowstone }
    }

    pub fn handle_update(&self, transaction: SubscribeUpdateTransaction) {}
}

#[async_trait]
pub trait ProgramTransactionDiscovery {
    fn new(rpc: Arc<RpcClient>) -> Result<()>;
    async fn handle(update: SubscribeUpdateTransaction) -> Result<()>;
}
