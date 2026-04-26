use anyhow::{Context, Result};
use futures::StreamExt;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Channel, ClientTlsConfig};
use tracing::{debug, error, info, warn};
use yellowstone_grpc_proto::geyser::{
    geyser_client::GeyserClient,
    subscribe_update::UpdateOneof,
    SubscribeRequest, SubscribeUpdate,
};

use crate::config::Config;

/// Callback invoked on every account update from the stream.
/// Receives (account_pubkey_bytes, account_data_bytes, slot).
pub type AccountUpdateCallback =
    Arc<dyn Fn([u8; 32], Vec<u8>, u64) + Send + Sync + 'static>;

pub struct GrpcStreamer {
    config: Arc<Config>,
    active: Arc<AtomicBool>,
    control_tx: Option<mpsc::Sender<SubscribeRequest>>,
}

impl GrpcStreamer {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            active: Arc::new(AtomicBool::new(false)),
            control_tx: None,
        }
    }

    /// Connect to the Yellowstone gRPC endpoint and begin streaming account updates.
    /// `initial_request` defines the initial subscription filter.
    /// `callback` is invoked for every account update received.
    pub async fn start(
        &mut self,
        initial_request: SubscribeRequest,
        callback: AccountUpdateCallback,
    ) -> Result<()> {
        if self.active.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            anyhow::bail!("Streamer is already active. Use update_subscription() to change filters.");
        }

        let channel = self.build_channel().await?;
        let mut client = self.build_grpc_client(channel);

        // Bidirectional stream: we send SubscribeRequests, server sends SubscribeUpdates
        let (ctrl_tx, ctrl_rx) = mpsc::channel::<SubscribeRequest>(8);
        self.control_tx = Some(ctrl_tx.clone());

        // Send the initial subscription before opening the stream
        ctrl_tx.send(initial_request).await.context("Failed to send initial request")?;

        let request_stream = ReceiverStream::new(ctrl_rx);
        // Attach x-token auth header if configured
        let mut grpc_request = tonic::Request::new(request_stream);
        if let Some(token) = &self.config.grpc_token {
            let val: tonic::metadata::MetadataValue<tonic::metadata::Ascii> =
                token.parse().context("Invalid GRPC_TOKEN value")?;
            grpc_request.metadata_mut().insert("x-token", val);
        }
        let response = client.subscribe(grpc_request).await
            .context("Failed to open gRPC subscribe stream")?;
        let mut inbound = response.into_inner();

        let active = Arc::clone(&self.active);

        tokio::spawn(async move {
            info!("gRPC stream started");

            loop {
                tokio::select! {
                    msg = inbound.next() => {
                        match msg {
                            Some(Ok(update)) => {
                                Self::handle_update(update, &callback);
                            }
                            Some(Err(status)) => {
                                error!("Stream error: {status}");
                                break;
                            }
                            None => {
                                warn!("Stream closed by server");
                                break;
                            }
                        }
                    }
                    // Yield to allow other tasks to run; re-check loop condition
                    _ = tokio::time::sleep(Duration::from_secs(30)) => {
                        if !active.load(Ordering::Relaxed) {
                            info!("Streamer stopped");
                            break;
                        }
                        debug!("Stream heartbeat OK");
                    }
                }
            }

            active.store(false, Ordering::Relaxed);
        });

        Ok(())
    }

    /// Send a new SubscribeRequest to change the active subscription filters.
    #[allow(dead_code)]
    pub async fn update_subscription(&self, request: SubscribeRequest) -> Result<()> {
        let tx = self.control_tx.as_ref().context("Streamer not started")?;
        tx.send(request).await.context("Failed to send subscription update")?;
        Ok(())
    }

    pub fn stop(&mut self) {
        self.active.store(false, Ordering::Relaxed);
        self.control_tx = None;
    }

    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    fn handle_update(update: SubscribeUpdate, callback: &AccountUpdateCallback) {
        match update.update_oneof {
            Some(UpdateOneof::Account(account_update)) => {
                if let Some(info) = account_update.account {
                    let slot = account_update.slot;
                    let Ok(pubkey_arr): Result<[u8; 32], _> =
                        info.pubkey.as_slice().try_into()
                    else {
                        return;
                    };
                    callback(pubkey_arr, info.data, slot);
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                // Keepalive — no action needed
            }
            _ => {}
        }
    }

    async fn build_channel(&self) -> Result<Channel> {
        let cfg = &self.config;
        let endpoint = Channel::from_shared(cfg.grpc_endpoint.clone())
            .context("Invalid gRPC endpoint")?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .context("TLS config error")?
            .connect_timeout(Duration::from_secs(cfg.grpc_connect_timeout_secs()))
            .timeout(Duration::from_secs(cfg.grpc_request_timeout_secs()))
            .tcp_keepalive(Some(Duration::from_secs(10)));

        endpoint.connect().await.context("Failed to connect to gRPC endpoint")
    }

    fn build_grpc_client(&self, channel: Channel) -> GeyserClient<Channel> {
        let client = GeyserClient::new(channel)
            .max_decoding_message_size(self.config.grpc_max_message_size());

        client
    }
}
