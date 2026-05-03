use anyhow::{Context, Result};
use futures::future::join_all;
use reqwest::Client;
use serde_json::{json, Value};
use tracing::{info, warn};

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

        let futs = REGIONS.iter().map(|(region, url)| {
            let http = self.http.clone();
            let body = body.clone();
            async move {
                let result: Result<String> = async {
                    let resp = http.post(*url).json(&body).send().await
                        .context("HTTP request failed")?;
                    let text = resp.text().await.unwrap_or_default();
                    let json: Value = serde_json::from_str(&text)
                        .context("Failed to parse response")?;
                    if let Some(err) = json.get("error") {
                        anyhow::bail!("Block Engine error: {err}");
                    }
                    Ok(json["result"].as_str().unwrap_or("unknown").to_string())
                }.await;
                (*region, result)
            }
        });

        let results = join_all(futs).await;

        let mut first_id: Option<String> = None;
        let mut n_ok = 0usize;
        let mut n_fail = 0usize;
        for (region, result) in &results {
            match result {
                Ok(id) => {
                    n_ok += 1;
                    if first_id.is_none() { first_id = Some(id.clone()); }
                }
                Err(e) => {
                    n_fail += 1;
                    warn!(region, "Block Engine rejected bundle: {e}");
                }
            }
        }

        match first_id {
            Some(id) => {
                info!(bundle_id = %id, n_ok, n_fail, "Bundle submitted to {n_ok}/{} regions", REGIONS.len());
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
