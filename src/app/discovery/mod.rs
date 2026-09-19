pub mod config;
mod raydium_amm_v4;

use crate::app::yellowstone::ACCOUNTS_LABEL;
use crate::app::yellowstone::YellowstoneApp;
use crate::logger::LoggerTitle;
use crate::logger::error;
use crate::logger::info;
use crate::logger::warn;
use anyhow::Result;
use raydium_amm_v4::RaydiumAMMv4Discovery;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;
use valuable::Valuable;
use yellowstone_grpc_proto::geyser::SubscribeUpdateAccount;
use yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction;
use yellowstone_grpc_proto::tonic::async_trait;

pub const QUEUE_CAPACITY: usize = 100_000;
pub const RESUBSCRIBE_INTERVAL: Duration = Duration::from_secs(10);

pub struct DiscoveryApp {
    yellowstone: Arc<Mutex<YellowstoneApp>>,
    transaction_discovery: Vec<Box<dyn ProgramTransactionDiscovery>>,
    account_discovery: Vec<Box<dyn ProgramAccountDiscovery>>,
    sender: Sender<DiscoveryUpdate>,
}

pub enum DiscoveryUpdate {
    Transaction(SubscribeUpdateTransaction),
    Account(SubscribeUpdateAccount),
}

impl DiscoveryApp {
    pub fn new(yellowstone: Arc<Mutex<YellowstoneApp>>) -> Result<Arc<Self>> {
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let app = Arc::new(Self {
            yellowstone,
            transaction_discovery: vec![Box::new(RaydiumAMMv4Discovery::new()?)],
            account_discovery: vec![Box::new(RaydiumAMMv4Discovery::new()?)],
            sender,
        });
        tokio::spawn(app.clone().run(receiver));
        Ok(app)
    }

    pub fn push_update(&self, update: DiscoveryUpdate) {
        match self.sender.try_send(update) {
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

    async fn run(self: Arc<Self>, mut receiver: Receiver<DiscoveryUpdate>) {
        let mut resubscribed = Instant::now();
        let mut pending = HashSet::new();
        while let Some(update) = receiver.recv().await {
            match update {
                DiscoveryUpdate::Transaction(transaction) => {
                    for discovery in &self.transaction_discovery {
                        match discovery.handle(transaction.clone()).await {
                            Ok(pools) => pending.extend(pools),
                            Err(e) => error(LoggerTitle::DiscoveryHandleError, Some(e.to_string())),
                        }
                    }
                }
                DiscoveryUpdate::Account(account) => {
                    for discovery in &self.account_discovery {
                        match discovery.handle(&account) {
                            Ok(accounts) => pending.extend(accounts),
                            Err(e) => error(LoggerTitle::DiscoveryHandleError, Some(e.to_string())),
                        }
                    }
                }
            }
            /* New pools arrive with almost every transaction at startup, so resubscribing
            per discovery would flood the sink with multi-megabyte requests. Batching on a
            minimum gap keeps it to one request per RESUBSCRIBE_INTERVAL at most */
            if !pending.is_empty() && resubscribed.elapsed() >= RESUBSCRIBE_INTERVAL {
                match self.resubscribe(&pending).await {
                    Ok(log) => {
                        pending.clear();
                        resubscribed = Instant::now();
                        info(LoggerTitle::YellowstoneResubscribed, log);
                    }
                    Err(e) => error(
                        LoggerTitle::YellowstoneResubscribeError,
                        Some(e.to_string()),
                    ),
                }
            }
        }
    }

    async fn resubscribe(&self, pending: &HashSet<String>) -> Result<YellowstoneResubscribedLog> {
        let mut yellowstone = self.yellowstone.lock().await;
        let mut request = yellowstone.get_subscription_request();
        let filter = request
            .accounts
            .entry(ACCOUNTS_LABEL.to_string())
            .or_default();
        /* The subscription request is the only full copy of the subscribed pools. The set
        holds just the pools seen since the previous resubscribe, and a pool swapped twice
        in ten seconds is in both, so dedupe against the request before appending */
        let subscribed = filter
            .account
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        let discovered = pending
            .iter()
            .filter(|pool| !subscribed.contains(pool.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        filter.account.extend(discovered);
        let pools = filter.account.len() as u64;
        let size = yellowstone.set_subscription_request(request).await?;
        Ok(YellowstoneResubscribedLog {
            pools,
            request_size: to_human_size(size),
        })
    }
}

fn to_human_size(bytes: usize) -> String {
    let kilobytes = bytes as f64 / 1024.0;
    if kilobytes < 1024.0 {
        format!("{kilobytes:.1} KB")
    } else {
        format!("{:.2} MB", kilobytes / 1024.0)
    }
}

#[derive(Valuable)]
struct YellowstoneResubscribedLog {
    pools: u64,
    request_size: String,
}

#[async_trait]
pub trait ProgramTransactionDiscovery: Send + Sync {
    async fn handle(&self, update: SubscribeUpdateTransaction) -> Result<Vec<String>>;
}

pub trait ProgramAccountDiscovery: Send + Sync {
    fn handle(&self, update: &SubscribeUpdateAccount) -> Result<Vec<String>>;
}
