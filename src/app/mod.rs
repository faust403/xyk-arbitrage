pub mod config;
mod discovery;
pub mod yellowstone;

use crate::app::discovery::DiscoveryApp;
use crate::logger::LoggerTitle;
use crate::logger::error;
use crate::logger::warn;
use anyhow::Result;
use config::Config;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::CommitmentConfig;
use std::ops::Deref;
use std::sync::Arc;
use tokio::sync::Mutex;
use yellowstone::StreamEnded;
use yellowstone::YellowstoneApp;
use yellowstone_grpc_proto::geyser::SubscribeUpdate;
use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;

pub struct App {
    /* Only used to rebuild the yellowstone app */
    config: Config,
    /* Only responsible for safe delivery of the subscription updates */
    yellowstone: Arc<Mutex<YellowstoneApp>>,
    /* This app takes the transaction update and parses it to discover a new pool.
    After discovery, it rebuilds the SubscribeRequest and sends it again into the sink */
    discovery: Arc<DiscoveryApp>,
}

impl App {
    pub async fn new(config: Config) -> Result<Self> {
        let yellowstone = Arc::new(Mutex::new(
            YellowstoneApp::new(&config.yellowstone, config.discovery.programs.clone()).await?,
        ));
        let rpc = Arc::new(RpcClient::new_with_commitment(
            config.discovery.rpc.clone(),
            CommitmentConfig::confirmed(),
        ));
        Ok(Self {
            yellowstone: yellowstone.clone(),
            discovery: DiscoveryApp::new(rpc, yellowstone)?,
            config,
        })
    }

    pub async fn run(&mut self) {
        loop {
            let result = self.yellowstone.lock().await.next().await;
            match result {
                Ok(update) => self.handle_update(update),
                Err(ended) => {
                    match ended {
                        /* No update arrived for STREAM_IDLE_TIMEOUT */
                        StreamEnded::Idle => {
                            error(LoggerTitle::YellowstoneEndpointSilence, None::<String>)
                        }
                        /* The server closed the stream */
                        StreamEnded::Closed => {
                            error(LoggerTitle::YellowstoneStreamClosed, None::<String>)
                        }
                        /* The stream yielded a transport error */
                        StreamEnded::Failed(status) => error(
                            LoggerTitle::YellowstoneStreamError,
                            Some(status.to_string()),
                        ),
                    }
                    break;
                }
            }
        }
    }

    fn handle_update(&self, update: SubscribeUpdate) {
        match update.update_oneof {
            /* We receive ping every ~10s, this does not need to be handled */
            Some(UpdateOneof::Ping(_)) => (),
            Some(UpdateOneof::Pong(_)) => (),
            Some(UpdateOneof::Transaction(transaction)) => self.discovery.push_update(transaction),
            Some(UpdateOneof::Account(_)) => (),
            /* The GeyserStream's AutoReconnect reads the slot from BlockMeta before
            it comes to us from .next() method. The reconnecting logic is hidden */
            Some(UpdateOneof::BlockMeta(_)) => (),
            _ => warn(LoggerTitle::YellowstoneUnknownUpdate, None::<String>),
        }
    }
}
