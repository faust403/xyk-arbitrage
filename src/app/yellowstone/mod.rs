pub mod config;

use anyhow::{Result, anyhow};
use config::YellowstoneConfig;
use futures::SinkExt;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;
use yellowstone_grpc_client::GeyserStream;
use yellowstone_grpc_client::ReconnectConfig;
use yellowstone_grpc_client::{GeyserGrpcClient, SubscribeRequestSink};
use yellowstone_grpc_proto::geyser::SubscribeRequestFilterBlocksMeta;
use yellowstone_grpc_proto::geyser::SubscribeUpdate;
use yellowstone_grpc_proto::geyser::{SubscribeRequest, SubscribeRequestFilterTransactions};
use yellowstone_grpc_proto::prost::Message;
use yellowstone_grpc_proto::tonic::Status;
use yellowstone_grpc_proto::tonic::codegen::tokio_stream::StreamExt;
use yellowstone_grpc_proto::tonic::transport::ClientTlsConfig;

/* Uniblock limit on gRPC request size is 4MB  */
pub const MAX_SUBSCRIPTION_REQUEST_SIZE: usize = 4 * 1024 * 1024;
pub const BACKOFF: Duration = Duration::from_secs(5);
pub const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
pub const HTTP2_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
pub const HTTP2_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5);
pub const PROGRAMS_LABEL: &str = "programs";
pub const ACCOUNTS_LABEL: &str = "accounts";
pub const BLOCKS_META_LABEL: &str = "blocks-meta";

pub struct YellowstoneApp {
    stream: GeyserStream,
    sink: SubscribeRequestSink,
    request: SubscribeRequest,
}

pub enum StreamEnded {
    Idle,
    Closed,
    Failed(Status),
}

impl YellowstoneApp {
    pub async fn new(config: &YellowstoneConfig, programs: Vec<String>) -> Result<Self> {
        let mut client = GeyserGrpcClient::build_from_shared(config.grpc.clone())?
            .x_token(config.x_token.clone())?
            .tls_config(ClientTlsConfig::new().with_native_roots())?
            /* how often to ping */
            .http2_keep_alive_interval(HTTP2_KEEP_ALIVE_INTERVAL)
            /* how long to wait for the answer before declaring death */
            .keep_alive_timeout(HTTP2_KEEP_ALIVE_TIMEOUT)
            /* ping even when no data is flowing */
            .keep_alive_while_idle(true)
            .set_reconnect_config(ReconnectConfig::default())
            .connect()
            .await?;
        let request = SubscribeRequest {
            /* We track the txs on programs to discover new pools and legs.
            These updates are all going to the dispatcher to update .accounts */
            transactions: HashMap::from([(
                PROGRAMS_LABEL.to_string(),
                SubscribeRequestFilterTransactions {
                    vote: Some(false),
                    failed: Some(false),
                    signature: None,
                    account_include: programs,
                    account_exclude: vec![],
                    account_required: vec![],
                    cuckoo_account_include: None,
                    token_accounts: None,
                },
            )]),
            /* Helps to keep track of the latest slot */
            blocks_meta: HashMap::from([(
                BLOCKS_META_LABEL.to_string(),
                SubscribeRequestFilterBlocksMeta {},
            )]),
            commitment: Some(config.commitment_from_str() as i32),
            ..Default::default()
        };
        if request.encode_to_vec().len() >= MAX_SUBSCRIPTION_REQUEST_SIZE {
            return Err(anyhow!("Subscription request is too large"));
        }
        let (sink, stream) = client.subscribe_with_request(Some(request.clone())).await?;
        Ok(Self {
            sink,
            stream,
            request,
        })
    }

    pub async fn get_subscription_request(&self) -> SubscribeRequest {
        self.request.clone()
    }

    pub async fn set_subscription_request(&mut self, request: SubscribeRequest) -> Result<()> {
        if request.encode_to_vec().len() >= MAX_SUBSCRIPTION_REQUEST_SIZE {
            return Err(anyhow!("Subscription request is too large"));
        }
        self.sink.send(request).await?;
        Ok(())
    }

    pub async fn next(&mut self) -> Result<SubscribeUpdate, StreamEnded> {
        match timeout(STREAM_IDLE_TIMEOUT, self.stream.next()).await {
            Ok(Some(Ok(update))) => Ok(update),
            Ok(Some(Err(status))) => Err(StreamEnded::Failed(status)),
            Ok(None) => Err(StreamEnded::Closed),
            Err(_) => Err(StreamEnded::Idle),
        }
    }
}
