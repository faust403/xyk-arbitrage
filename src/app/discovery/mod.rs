pub mod config;
mod raydium_amm_v4;

use crate::app::yellowstone::YellowstoneApp;
use crate::logger::LoggerTitle;
use crate::logger::error;
use crate::logger::warn;
use anyhow::Result;
use dashmap::DashSet;
use raydium_amm_v4::RaydiumAMMv4Discovery;
use solana_client::nonblocking::rpc_client::RpcClient;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

pub const QUEUE_CAPACITY: usize = 100_000;

pub struct DiscoveryApp {
    yellowstone: Arc<Mutex<YellowstoneApp>>,
    transaction_discovery: Vec<Box<dyn ProgramTransactionDiscovery>>,
    raydium_pools: Arc<DashSet<String>>,
    sender: Sender<SubscribeUpdateTransaction>,
}

impl DiscoveryApp {
    pub fn new(
        rpc: Arc<RpcClient>,
        yellowstone: Arc<Mutex<YellowstoneApp>>,
        raydium_pools: Arc<DashSet<String>>,
    ) -> Result<Arc<Self>> {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let app = Arc::new(Self {
            raydium_pools,
            yellowstone,
            transaction_discovery: vec![Box::new(RaydiumAMMv4Discovery::new(rpc.clone())?)],
            sender,
        });
        tokio::spawn(app.clone().run(receiver));
        Ok(app)
    }

    pub fn push_update(&self, transaction: SubscribeUpdateTransaction) {
        match self.sender.try_send(transaction) {
            Ok(()) => (),
            /* This is fine for us to sometimes miss the new pools.
            Maybe if we see them once in a week, then they aren't worth it? */
            Err(TrySendError::Full(_)) => warn(LoggerTitle::DiscoveryQueueFull, None::<String>),
            /* Normally should no happen */
            Err(TrySendError::Closed(_)) => {
                error(LoggerTitle::DiscoveryQueueClosed, None::<String>)
            }
        }
    }

    async fn run(self: Arc<Self>, mut receiver: Receiver<SubscribeUpdateTransaction>) {
        while let Some(transaction) = receiver.recv().await {
            for discovery in &self.transaction_discovery {
                match discovery.handle(transaction.clone()).await {
                    Ok(pools) => {
                        for pool in pools {
                            self.raydium_pools.insert(pool);
                        }
                    }
                    Err(e) => error(LoggerTitle::DiscoveryHandleError, Some(e.to_string())),
                }
            }
        }
    }
}

#[async_trait]
pub trait ProgramTransactionDiscovery: Send + Sync {
    async fn handle(&self, update: SubscribeUpdateTransaction) -> Result<Vec<String>>;
}
