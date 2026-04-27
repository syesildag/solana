use anyhow::{Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use tracing::info;

use crate::jito::bundle::JitoBundle;

const BLOCK_ENGINE_URL: &str = "https://mainnet.block-engine.jito.wtf/api/v1/bundles";

/// HTTP client for the Jito Block Engine.
pub struct JitoClient {
    http: Client,
    endpoint: String,
    dry_run: bool,
}

impl JitoClient {
    pub fn new(dry_run: bool) -> Self {
        Self {
            http: Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("Failed to build HTTP client"),
            endpoint: BLOCK_ENGINE_URL.to_string(),
            dry_run,
        }
    }

    /// Submit a Jito bundle to the Block Engine.
    /// Returns the bundle UUID on success.
    pub async fn submit_bundle(&self, bundle: &JitoBundle) -> Result<String> {
        let encoded = bundle.encode().context("Failed to encode bundle")?;

        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [encoded]
        });

        if self.dry_run {
            // tx[0..n-1] = swap txs (first carries setup: ATA creation + SOL wrap,
            //              last carries teardown: close WSOL ATA)
            // tx[n]      = Jito tip transfer
            let swap_count = encoded.len().saturating_sub(1);
            info!(
                "[DRY RUN] Would submit bundle: {} swap tx(s) + 1 tip tx  (tx[0] prefix: {}…)",
                swap_count,
                &encoded[0][..20]
            );
            return Ok("dry-run-no-id".to_string());
        }

        let response = self.http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .context("HTTP request to Block Engine failed")?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if !status.is_success() {
            anyhow::bail!("Block Engine returned {status}: {text}");
        }

        let json: Value = serde_json::from_str(&text)
            .context("Failed to parse Block Engine response")?;

        if let Some(err) = json.get("error") {
            anyhow::bail!("Block Engine error: {err}");
        }

        let bundle_id = json["result"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        info!("Bundle submitted: {bundle_id}");
        Ok(bundle_id)
    }

    /// Get the status of a previously submitted bundle.
    #[allow(dead_code)]
    pub async fn get_bundle_status(&self, bundle_id: &str) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id]]
        });

        let response = self.http
            .post(&self.endpoint)
            .json(&body)
            .send()
            .await
            .context("Failed to query bundle status")?;

        let json: Value = response.json().await.context("Failed to parse status response")?;
        Ok(json)
    }
}
