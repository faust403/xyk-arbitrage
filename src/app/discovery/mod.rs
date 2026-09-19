pub mod config;
mod raydium_amm_v4;

use crate::app::yellowstone::YellowstoneApp;
use anyhow::Result;
use raydium_amm_v4::RaydiumAMMv4Discovery;
use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::{Arc, RwLock};
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

pub struct DiscoveryApp {
    yellowstone: Arc<RwLock<YellowstoneApp>>,
    transaction_discovery: Vec<Box<dyn ProgramTransactionDiscovery>>,
}

impl DiscoveryApp {
    pub fn new(rpc: Arc<RpcClient>, yellowstone: Arc<RwLock<YellowstoneApp>>) -> Result<Self> {
        Ok(Self {
            yellowstone,
            transaction_discovery: vec![Box::new(RaydiumAMMv4Discovery::new(rpc.clone())?)],
        })
    }

    pub async fn handle_update(&self, transaction: SubscribeUpdateTransaction) {
        for td in &self.transaction_discovery {
            let result = td.handle(transaction.clone()).await;
            match result {
                Ok(_) => (),
                Err(_) => (),
            }
        }
    }
}

#[async_trait]
pub trait ProgramTransactionDiscovery: Send + Sync {
    async fn handle(&self, update: SubscribeUpdateTransaction) -> Result<Vec<String>>;
}
