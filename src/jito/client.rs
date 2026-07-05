use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::{Arc, atomic::{AtomicU64, Ordering}};
use tracing::{debug, info, warn};

/// Outcome of a submitted Jito bundle, as reported by getBundleStatuses.
#[derive(Debug, PartialEq, Eq)]
pub enum BundleOutcome {
    /// Included in a block and all transactions succeeded.
    Landed,
    /// Not included in any block within the 20-second polling window.
    /// Usually means the tip was too low relative to competing bundles.
    Dropped,
    /// Included in a block but at least one transaction failed on-chain.
    /// Usually means market conditions changed between submission and landing.
    FailedOnChain,
}

/// What `submit_bundle` learned from the first Block Engine to accept:
/// which region won the parallel race and how long the accept took.
/// Feeds the latency instrumentation (arbitrage::latency); `bundle_id`
/// is what callers previously received as a bare `String`.
#[derive(Debug, Clone)]
pub struct SubmitReceipt {
    pub bundle_id: String,
    pub region: &'static str,
    pub accept_ms: u32,
}

use crate::jito::bundle::JitoBundle;

/// All five Jito regional Block Engines. Submitting to all in parallel maximises the
/// probability that the bundle reaches the current slot leader regardless of region.
/// Status queries only need one endpoint — Frankfurt is listed first (lowest latency
/// from Valbonne, France: ~20 ms vs ~85 ms for NY).
const REGIONS: &[(&str, &str)] = &[
    ("frankfurt", "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("amsterdam", "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("ny",        "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("slc",       "https://slc.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("tokyo",     "https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles"),
];

/// HTTP client for the Jito Block Engine.
pub struct JitoClient {
    http: Client,
    dry_run: bool,
}

impl JitoClient {
    pub fn new(dry_run: bool) -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("Failed to build HTTP client"),
            dry_run,
        }
    }

    /// Establish TCP+TLS connections to all Block Engine regions at startup so the first
    /// real bundle submission doesn't pay the ~150 ms DNS+TLS handshake tax.
    /// Fire-and-forget: errors are ignored — if a region is unreachable now the real
    /// submission will still try it (and fail fast, because the connection pool is shared).
    pub async fn warmup_connections(&self) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getInflightBundleStatuses",
            "params": [[]]
        });

        let futs = REGIONS.iter().map(|&(region, url)| {
            let http = self.http.clone();
            let body = body.clone();
            async move {
                match http.post(url).json(&body).send().await {
                    Ok(_)  => debug!(region, "BE warmup ok"),
                    Err(e) => debug!(region, "BE warmup skipped: {e}"),
                }
            }
        });

        futures::future::join_all(futs).await;
        info!("Jito Block Engine connections pre-warmed ({} regions)", REGIONS.len());
    }

    /// Keep TCP/TLS connections alive by pinging all regions every 20 s.
    /// AWS load balancers drop idle connections after ~60 s; without this the first
    /// real bundle submission after a quiet period pays a ~150 ms TLS renegotiation tax.
    pub fn spawn_keepalive(self: std::sync::Arc<Self>) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getInflightBundleStatuses",
            "params": [[]]
        });
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(20)).await;
                let futs = REGIONS.iter().map(|&(region, url)| {
                    let http = self.http.clone();
                    let body = body.clone();
                    async move {
                        if let Err(e) = http.post(url).json(&body).send().await {
                            debug!(region, "keepalive failed: {e}");
                        }
                    }
                });
                futures::future::join_all(futs).await;
            }
        });
    }

    /// Fetch the EMA 50th-percentile tip floor from Jito's stats endpoint (in lamports).
    /// Returns 0 on any error — callers treat 0 as "data unavailable".
    async fn fetch_tip_floor(&self) -> u64 {
        let resp = match self.http
            .get("https://bundles.jito.wtf/api/v1/bundles/tip_floor")
            .send().await
        {
            Ok(r)  => r,
            Err(e) => { debug!("tip floor fetch failed: {e}"); return 0; }
        };
        let json: Value = match resp.json().await {
            Ok(v)  => v,
            Err(e) => { debug!("tip floor parse failed: {e}"); return 0; }
        };
        let ema_sol = json[0]["ema_landed_tips_50th_percentile"].as_f64().unwrap_or(0.0);
        (ema_sol * 1e9) as u64
    }

    /// Spawn a background task that refreshes the EMA 50th-percentile tip floor every 30 s.
    /// The returned `Arc<AtomicU64>` holds the current value in lamports; 0 = not yet fetched.
    ///
    /// Use this as a congestion signal: a sudden spike (e.g. 5 K → 500 K lamports) indicates
    /// the network is under heavy load and even large-tip bundles face stiffer competition.
    pub fn spawn_tip_floor_cache(self: Arc<Self>) -> Arc<AtomicU64> {
        let cache = Arc::new(AtomicU64::new(0));
        let cache_t = Arc::clone(&cache);
        tokio::spawn(async move {
            loop {
                let lamports = self.fetch_tip_floor().await;
                if lamports > 0 {
                    cache_t.store(lamports, Ordering::Relaxed);
                    debug!(lamports, "tip floor updated");
                }
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        });
        cache
    }

    /// Submit a Jito bundle to all regional Block Engines in parallel.
    /// Returns the first bundle ID on success; fails only if every region rejects.
    pub async fn submit_bundle(&self, bundle: &JitoBundle) -> Result<SubmitReceipt> {
        let t0 = std::time::Instant::now();
        let encoded = bundle.encode().context("Failed to encode bundle")?;

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [encoded]
        });

        if self.dry_run {
            let swap_count = encoded.len().saturating_sub(1);
            info!(
                "[DRY RUN] Would submit bundle: {} swap tx(s) + 1 tip tx  (tx[0] prefix: {}…)",
                swap_count,
                &encoded[0][..20]
            );
            return Ok(SubmitReceipt {
                bundle_id: "dry-run-no-id".to_string(),
                region: "dry",
                accept_ms: 0,
            });
        }

        // Spawn all regions as independent tasks — they all submit regardless of who finishes first.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(&'static str, Result<String, (bool, String)>)>(REGIONS.len());

        for &(region, url) in REGIONS {
            let http  = self.http.clone();
            let body  = body.clone();
            let tx    = tx.clone();
            tokio::spawn(async move {
                let result: Result<String, (bool, String)> = async {
                    let resp = http.post(url).json(&body).send().await
                        .map_err(|e| (false, e.to_string()))?;
                    let text = resp.text().await.unwrap_or_default();
                    let json: Value = serde_json::from_str(&text)
                        .map_err(|e| (false, e.to_string()))?;
                    if let Some(err) = json.get("error") {
                        let rate_limited = json["error"]["code"].as_i64() == Some(-32097);
                        return Err((rate_limited, format!("{err}")));
                    }
                    Ok(json["result"].as_str().unwrap_or("unknown").to_string())
                }.await;
                let _ = tx.send((region, result)).await;
            });
        }
        drop(tx); // channel closes once all spawned tasks finish sending

        // Return as soon as the first region confirms; drain the rest in a background task.
        let mut first: Option<SubmitReceipt> = None;
        let mut n_ok = 0usize;
        let mut n_fail = 0usize;

        while let Some((region, result)) = rx.recv().await {
            match result {
                Ok(id) => {
                    n_ok += 1;
                    if first.is_none() {
                        first = Some(SubmitReceipt {
                            bundle_id: id.clone(),
                            region,
                            accept_ms: t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                        });
                        // Hand off remaining results to a background logger and return immediately.
                        tokio::spawn(async move {
                            let mut total_ok  = n_ok;
                            let mut total_fail = n_fail;
                            while let Some((r, res)) = rx.recv().await {
                                match res {
                                    Ok(_)            => total_ok  += 1,
                                    Err((true,  _))  => { total_fail += 1; }
                                    Err((false, m))  => { total_fail += 1; warn!(region=r, "BE error: {m}"); }
                                }
                            }
                            info!(total_ok, total_fail, total=REGIONS.len(), "All regions responded");
                        });
                        break;
                    }
                }
                Err((true,  msg)) => { n_fail += 1; warn!(region, "BE rejected (-32097): {msg}"); }
                Err((false, msg)) => { n_fail += 1; warn!(region,  "BE error: {msg}"); }
            }
        }

        match first {
            Some(receipt) => {
                info!(
                    bundle_id = %receipt.bundle_id,
                    region = receipt.region,
                    accept_ms = receipt.accept_ms,
                    "Bundle accepted by first region"
                );
                Ok(receipt)
            }
            None => anyhow::bail!("All {} Block Engine regions rejected the bundle", REGIONS.len()),
        }
    }

    /// Poll getBundleStatuses every 2 s until the bundle lands, fails on-chain, or 20 s elapse.
    /// Returns a [`BundleOutcome`] so callers can apply appropriate cooldown strategies:
    ///   - `Landed`        → no cooldown change needed
    ///   - `Dropped`       → tip too low; apply a long cooldown before retrying
    ///   - `FailedOnChain` → market moved; apply the normal short cooldown
    ///
    /// Early-drop detection: Jito indexes landed bundles within 1 slot (~400 ms). Three
    /// consecutive polls that return `value:[]` with an advancing slot counter means the
    /// bundle is definitively gone from the chain — no need to wait the full 20 s.
    pub async fn log_bundle_outcome(&self, bundle_id: &str) -> BundleOutcome {
        if bundle_id == "dry-run-no-id" {
            return BundleOutcome::Landed;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut empty_polls = 0usize;
        let mut last_slot   = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if std::time::Instant::now() >= deadline {
                warn!(%bundle_id, "Bundle outcome: DROPPED (no confirmation in 20s)");
                return BundleOutcome::Dropped;
            }
            let resp = match self.get_bundle_status(bundle_id).await {
                Ok(v)  => v,
                Err(e) => { warn!("Bundle status poll failed: {e}"); continue; }
            };
            let context_slot = resp["result"]["context"]["slot"].as_u64().unwrap_or(0);
            let Some(values) = resp["result"]["value"].as_array() else { continue };

            if values.is_empty() {
                debug!(%bundle_id, slot = context_slot, "Bundle not yet indexed");
                if context_slot > last_slot {
                    last_slot    = context_slot;
                    empty_polls += 1;
                    if empty_polls >= 3 {
                        warn!(%bundle_id, empty_polls, "Bundle outcome: DROPPED (absent from 3 advancing slots)");
                        return BundleOutcome::Dropped;
                    }
                }
                continue;
            }

            info!(%bundle_id, raw = %resp, "Bundle status response");
            let Some(entry)  = values.first()                       else { continue };
            let slot         = entry["slot"].as_u64().unwrap_or(0);
            let confirmation = entry["confirmationStatus"].as_str().unwrap_or("unknown");
            let txs: Vec<&str> = entry["transactions"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            let err = &entry["err"];
            if err.get("Ok").is_some() {
                info!(%bundle_id, slot, %confirmation, ?txs, "Bundle LANDED ✓");
                return BundleOutcome::Landed;
            } else {
                warn!(%bundle_id, slot, err = %err, ?txs, "Bundle FAILED on-chain");
                return BundleOutcome::FailedOnChain;
            }
        }
    }

    /// Get the raw status JSON for a previously submitted bundle.
    /// Tries regions in order (Frankfurt first) and skips to the next on rate-limit (-32097).
    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id]]
        });

        for &(region, url) in REGIONS {
            let response = match self.http.post(url).json(&body).send().await {
                Ok(r)  => r,
                Err(e) => { warn!(region, "Status request failed: {e}"); continue; }
            };
            let json: Value = match response.json().await {
                Ok(v)  => v,
                Err(e) => { warn!(region, "Status parse failed: {e}"); continue; }
            };
            if json["error"]["code"].as_i64() == Some(-32097) {
                debug!(region, "Status endpoint rate-limited, trying next region");
                continue;
            }
            return Ok(json);
        }
        anyhow::bail!("All regions rate-limited or unavailable for bundle status query")
    }
}
