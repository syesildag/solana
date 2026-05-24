//! Thin async client for Jupiter v6 (`/quote` + `/swap`).
//!
//! The rebalancer is the only consumer. The flow is:
//!   1. `scanner::fetch_decimals_for_mints` once at startup (Solana RPC,
//!      bounded by portfolio size).
//!   2. `quote(...)` to discover the best route, slippage, price impact.
//!   3. `swap(...)` to receive a base64 v0 transaction ready to sign.
//!   4. Caller signs with the wallet keypair and submits via RPC.
//!
//! Jupiter docs: https://station.jup.ag/docs/apis/swap-api

use std::collections::HashMap;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteResponse {
    #[serde(rename = "inputMint")]
    pub input_mint: String,
    #[serde(rename = "outputMint")]
    pub output_mint: String,
    /// Raw input amount (integer string per Jupiter convention).
    #[serde(rename = "inAmount")]
    pub in_amount: String,
    /// Raw output amount with slippage tolerance applied to the worst case.
    #[serde(rename = "outAmount")]
    pub out_amount: String,
    /// Minimum out amount the user will accept (raw).
    #[serde(rename = "otherAmountThreshold")]
    pub other_amount_threshold: String,
    #[serde(rename = "swapMode")]
    pub swap_mode: String,
    #[serde(rename = "slippageBps")]
    pub slippage_bps: u32,
    /// Approximate price impact as a decimal string, e.g. "0.0021" = 21 bps.
    #[serde(rename = "priceImpactPct")]
    pub price_impact_pct: String,
    /// Full Jupiter route — passed back verbatim to `/swap`.
    #[serde(rename = "routePlan", default)]
    pub route_plan: serde_json::Value,
    /// Pass-through fields Jupiter sometimes adds (contextSlot, timeTaken …).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapResponse {
    /// Base64-encoded v0 `VersionedTransaction`. Must be signed by the user
    /// before submission.
    #[serde(rename = "swapTransaction")]
    pub swap_transaction: String,
    /// Block at which Jupiter built the route — caller can use it for
    /// "last valid block height" decisions when re-broadcasting.
    #[serde(rename = "lastValidBlockHeight", default)]
    pub last_valid_block_height: u64,
    /// Total priority fee Jupiter chose, in lamports. Optional.
    #[serde(rename = "prioritizationFeeLamports", default)]
    pub prioritization_fee_lamports: u64,
}

/// Convert a human-readable amount (e.g. 1.5 SOL) to raw lamports given decimals.
pub fn to_raw_amount(human: f64, decimals: u8) -> u64 {
    let scale = 10f64.powi(decimals as i32);
    (human * scale).max(0.0) as u64
}

/// Convert raw lamports back to a human-readable amount.
pub fn from_raw_amount(raw: u64, decimals: u8) -> f64 {
    raw as f64 / 10f64.powi(decimals as i32)
}

pub async fn quote(
    http: &Client,
    base_url: &str,
    input_mint: &str,
    output_mint: &str,
    amount_raw: u64,
    slippage_bps: u32,
) -> Result<QuoteResponse> {
    let url = format!("{base_url}/quote");
    let resp = http
        .get(&url)
        .query(&[
            ("inputMint", input_mint),
            ("outputMint", output_mint),
            ("amount", &amount_raw.to_string()),
            ("slippageBps", &slippage_bps.to_string()),
            ("onlyDirectRoutes", "false"),
            ("asLegacyTransaction", "false"),
        ])
        .send()
        .await
        .context("jupiter /quote request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("jupiter /quote returned {status}: {body}");
    }
    resp.json().await.context("jupiter /quote JSON decode failed")
}

/// Returned by Jupiter: a base64 v0 transaction that needs to be signed.
pub async fn swap(
    http: &Client,
    base_url: &str,
    quote: &QuoteResponse,
    user_pubkey: &str,
) -> Result<SwapResponse> {
    let url = format!("{base_url}/swap");
    // Jupiter accepts the entire quote response back; flattening it here keeps
    // the request schema independent of any new optional fields Jupiter ships.
    let body = serde_json::json!({
        "quoteResponse": quote,
        "userPublicKey": user_pubkey,
        "wrapAndUnwrapSol": true,
        "useSharedAccounts": true,
        "dynamicComputeUnitLimit": true,
        "asLegacyTransaction": false,
    });
    let resp = http
        .post(&url)
        .json(&body)
        .send()
        .await
        .context("jupiter /swap request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("jupiter /swap returned {status}: {body}");
    }
    resp.json().await.context("jupiter /swap JSON decode failed")
}

/// Best-effort price-impact parser. Jupiter returns the field as a string
/// (e.g. "0.0021"). Returns the value in basis points (21 → 21 bps).
pub fn price_impact_bps(q: &QuoteResponse) -> u32 {
    q.price_impact_pct
        .parse::<f64>()
        .map(|p| (p.abs() * 10_000.0) as u32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_amount_round_trip() {
        let raw = to_raw_amount(1.5, 9);
        assert_eq!(raw, 1_500_000_000);
        let back = from_raw_amount(raw, 9);
        assert!((back - 1.5).abs() < 1e-9);
    }

    #[test]
    fn raw_amount_token_with_six_decimals() {
        let raw = to_raw_amount(2.5, 6);
        assert_eq!(raw, 2_500_000);
    }

    #[test]
    fn price_impact_bps_from_string() {
        let mut q = QuoteResponse {
            input_mint: "x".into(),
            output_mint: "y".into(),
            in_amount: "1".into(),
            out_amount: "1".into(),
            other_amount_threshold: "1".into(),
            swap_mode: "ExactIn".into(),
            slippage_bps: 30,
            price_impact_pct: "0.0021".into(),
            route_plan: serde_json::Value::Null,
            extra: HashMap::new(),
        };
        assert_eq!(price_impact_bps(&q), 21);
        q.price_impact_pct = "bad".into();
        assert_eq!(price_impact_bps(&q), 0);
    }
}
