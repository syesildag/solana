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

        let active        = Arc::clone(&self.active);
        let config        = Arc::clone(&self.config);
        let initial_req   = initial_request;

        tokio::spawn(async move {
            // Reconnect loop with exponential backoff (1s → 2s → 4s … capped at 30s).
            let mut backoff = Duration::from_secs(1);

            'reconnect: loop {
                if !active.load(Ordering::Relaxed) { break; }

                // ── Connect ───────────────────────────────────────────────────
                let channel = match Self::build_channel_from_config(&config).await {
                    Ok(ch) => ch,
                    Err(e) => {
                        error!("gRPC connect failed: {e} — retrying in {}s", backoff.as_secs());
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        continue 'reconnect;
                    }
                };
                let mut client = Self::build_grpc_client_from_config(channel, &config);

                let (ctrl_tx2, ctrl_rx2) = mpsc::channel::<SubscribeRequest>(8);
                if ctrl_tx2.send(initial_req.clone()).await.is_err() { break; }
                let request_stream = ReceiverStream::new(ctrl_rx2);
                let mut grpc_request = tonic::Request::new(request_stream);
                if let Some(token) = &config.grpc_token {
                    match token.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
                        Ok(val) => { grpc_request.metadata_mut().insert("x-token", val); }
                        Err(e)  => { error!("Invalid GRPC_TOKEN: {e}"); break; }
                    }
                }
                let mut inbound = match client.subscribe(grpc_request).await {
                    Ok(r) => r.into_inner(),
                    Err(e) => {
                        error!("gRPC subscribe failed: {e} — retrying in {}s", backoff.as_secs());
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(Duration::from_secs(30));
                        continue 'reconnect;
                    }
                };

                info!("gRPC stream started");
                backoff = Duration::from_secs(1); // reset on successful connect
                let mut update_count: u64 = 0;
                let mut last_report = std::time::Instant::now();

                // ── Receive loop ──────────────────────────────────────────────
                loop {
                    tokio::select! {
                        msg = inbound.next() => {
                            match msg {
                                Some(Ok(update)) => {
                                    update_count += 1;
                                    Self::handle_update(update, &callback);
                                    let elapsed = last_report.elapsed();
                                    if elapsed.as_secs() >= 10 {
                                        info!(
                                            "Stream alive: {} updates in the last {:.0}s ({:.1}/s)",
                                            update_count,
                                            elapsed.as_secs_f64(),
                                            update_count as f64 / elapsed.as_secs_f64()
                                        );
                                        update_count = 0;
                                        last_report = std::time::Instant::now();
                                    }
                                }
                                Some(Err(status)) => {
                                    error!("Stream error: {status} — reconnecting in {}s", backoff.as_secs());
                                    tokio::time::sleep(backoff).await;
                                    backoff = (backoff * 2).min(Duration::from_secs(30));
                                    continue 'reconnect;
                                }
                                None => {
                                    warn!("Stream closed by server — reconnecting in {}s", backoff.as_secs());
                                    tokio::time::sleep(backoff).await;
                                    backoff = (backoff * 2).min(Duration::from_secs(30));
                                    continue 'reconnect;
                                }
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_secs(30)) => {
                            if !active.load(Ordering::Relaxed) { break 'reconnect; }
                            info!("Stream heartbeat — no updates in 30s (check subscription filters)");
                        }
                    }

                    if !active.load(Ordering::Relaxed) { break 'reconnect; }
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
                        warn!("Received account update with invalid pubkey length");
                        return;
                    };
                    debug!(
                        "Account update: pubkey={} data_len={} slot={}",
                        solana_sdk::pubkey::Pubkey::from(pubkey_arr),
                        info.data.len(),
                        slot
                    );
                    callback(pubkey_arr, info.data, slot);
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                debug!("gRPC ping received");
            }
            _ => {}
        }
    }

    async fn build_channel_from_config(config: &Config) -> Result<Channel> {
        let endpoint = Channel::from_shared(config.grpc_endpoint.clone())
            .context("Invalid gRPC endpoint")?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .context("TLS config error")?
            .connect_timeout(Duration::from_secs(config.grpc_connect_timeout_secs()))
            .timeout(Duration::from_secs(config.grpc_request_timeout_secs()))
            .tcp_keepalive(Some(Duration::from_secs(10)));
        endpoint.connect().await.context("Failed to connect to gRPC endpoint")
    }

    fn build_grpc_client_from_config(channel: Channel, config: &Config) -> GeyserClient<Channel> {
        GeyserClient::new(channel)
            .max_decoding_message_size(config.grpc_max_message_size())
    }
}
