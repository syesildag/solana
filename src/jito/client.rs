use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::jito::bundle::JitoBundle;

/// All five Jito regional Block Engines. Submitting to all in parallel maximises the
/// probability that the bundle reaches the current slot leader regardless of region.
/// Status queries only need one endpoint — the NY region is used as the canonical one.
const REGIONS: &[(&str, &str)] = &[
    ("ny",        "https://ny.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("amsterdam", "https://amsterdam.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("frankfurt", "https://frankfurt.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("tokyo",     "https://tokyo.mainnet.block-engine.jito.wtf/api/v1/bundles"),
    ("slc",       "https://slc.mainnet.block-engine.jito.wtf/api/v1/bundles"),
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

    /// Submit a Jito bundle to all regional Block Engines in parallel.
    /// Returns the first bundle ID on success; fails only if every region rejects.
    pub async fn submit_bundle(&self, bundle: &JitoBundle) -> Result<String> {
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
            return Ok("dry-run-no-id".to_string());
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
        let mut first_id: Option<String> = None;
        let mut n_ok = 0usize;
        let mut n_fail = 0usize;

        while let Some((region, result)) = rx.recv().await {
            match result {
                Ok(id) => {
                    n_ok += 1;
                    if first_id.is_none() {
                        first_id = Some(id.clone());
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
                Err((true,  msg)) => { n_fail += 1; debug!(region, "BE rate-limited: {msg}"); }
                Err((false, msg)) => { n_fail += 1; warn!(region,  "BE error: {msg}"); }
            }
        }

        match first_id {
            Some(id) => {
                info!(bundle_id = %id, "Bundle accepted by first region");
                Ok(id)
            }
            None => anyhow::bail!("All {} Block Engine regions rejected the bundle", REGIONS.len()),
        }
    }

    /// Poll getBundleStatuses every 2 s until the bundle lands, fails on-chain, or 20 s elapse.
    /// Spawned as a fire-and-forget background task after submit_bundle.
    pub async fn log_bundle_outcome(&self, bundle_id: String) {
        if bundle_id == "dry-run-no-id" {
            return;
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if std::time::Instant::now() >= deadline {
                warn!(%bundle_id, "Bundle outcome: DROPPED (no confirmation in 20s)");
                return;
            }
            let resp = match self.get_bundle_status(&bundle_id).await {
                Ok(v)  => v,
                Err(e) => { warn!("Bundle status poll failed: {e}"); continue; }
            };
            let Some(values) = resp["result"]["value"].as_array() else { continue };
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
            } else {
                warn!(%bundle_id, slot, err = %err, ?txs, "Bundle FAILED on-chain");
            }
            return;
        }
    }

    /// Get the raw status JSON for a previously submitted bundle.
    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id]]
        });

        let response = self.http
            .post(REGIONS[0].1)  // ny — canonical endpoint for status queries
            .json(&body)
            .send()
            .await
            .context("Failed to query bundle status")?;

        let json: Value = response.json().await.context("Failed to parse status response")?;
        Ok(json)
    }
}
