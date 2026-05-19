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

/// Callback invoked when a confirmed transaction touching a tracked vault is received.
/// Receives (account_key_bytes_list, estimated_swap_lamports, slot).
/// The estimated swap size is best-effort from instruction data (bytes 1–8 as LE u64).
pub type TransactionCallback =
    Arc<dyn Fn(Vec<[u8; 32]>, u64, u64) + Send + Sync + 'static>;

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
    /// `tx_callback` is invoked for every confirmed transaction touching a tracked vault.
    pub async fn start(
        &mut self,
        initial_request: SubscribeRequest,
        callback: AccountUpdateCallback,
        tx_callback: Option<TransactionCallback>,
    ) -> Result<()> {
        if self.active.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            anyhow::bail!("Streamer is already active. Use update_subscription() to change filters.");
        }

        let active        = Arc::clone(&self.active);
        let config        = Arc::clone(&self.config);
        let initial_req   = initial_request;
        let tx_callback   = tx_callback;

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
                                    Self::handle_update(update, &callback, tx_callback.as_ref());
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
                            // Force reconnect — 30s of silence means the connection is
                            // stale. Staying connected with frozen pool state causes the
                            // graph to accumulate phantom profitable cycles that the
                            // evaluator correctly rejects, producing profitable=0.
                            warn!("Stream: no updates in 30s — forcing reconnect");
                            continue 'reconnect;
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

    fn handle_update(
        update: SubscribeUpdate,
        account_callback: &AccountUpdateCallback,
        tx_callback: Option<&TransactionCallback>,
    ) {
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
                    account_callback(pubkey_arr, info.data, slot);
                }
            }
            Some(UpdateOneof::Transaction(tx_update)) => {
                if let Some(cb) = tx_callback {
                    if let Some((keys, estimated, slot)) = Self::extract_whale_signal(&tx_update) {
                        cb(keys, estimated, slot);
                    }
                }
            }
            Some(UpdateOneof::Ping(_)) => {
                debug!("gRPC ping received");
            }
            _ => {}
        }
    }

    /// Extract a list of account pubkeys and a best-effort swap size estimate from a
    /// confirmed transaction update. Returns None if the transaction has no instructions.
    ///
    /// Swap size estimation: reads bytes 1–8 of the first instruction as a little-endian
    /// u64. This is correct for Raydium AMM V4 (SwapBaseIn: disc=9, then amount_in:u64)
    /// and close enough for Orca/CLMM (same offset layout). For DLMM the estimate may
    /// be wrong but only used for the threshold check, so false negatives are harmless.
    fn extract_whale_signal(
        tx_update: &yellowstone_grpc_proto::geyser::SubscribeUpdateTransaction,
    ) -> Option<(Vec<[u8; 32]>, u64, u64)> {
        let tx  = tx_update.transaction.as_ref()?.transaction.as_ref()?;
        let msg = tx.message.as_ref()?;

        let account_keys: Vec<[u8; 32]> = msg
            .account_keys
            .iter()
            .filter_map(|k| k.as_slice().try_into().ok())
            .collect();

        if account_keys.is_empty() {
            return None;
        }

        // Best-effort: bytes 1–8 of the first instruction as LE u64 (amount_in field).
        let estimated = msg
            .instructions
            .iter()
            .find_map(|ix| {
                let bytes: [u8; 8] = ix.data.get(1..9)?.try_into().ok()?;
                Some(u64::from_le_bytes(bytes))
            })
            .unwrap_or(0);

        Some((account_keys, estimated, tx_update.slot))
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
