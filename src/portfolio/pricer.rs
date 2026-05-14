use anyhow::{anyhow, Result};
use reqwest::Client;
use std::collections::HashMap;

use super::history::PriceSnapshot;

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const BIRDEYE_HISTORY_URL: &str = "https://public-api.birdeye.so/defi/history_price";
const DEXSCREENER_URL: &str = "https://api.dexscreener.com/tokens/v1/solana";
const BINANCE_PRICE_URL: &str = "https://api.binance.com/api/v3/ticker/price";
const FRANKFURTER_URL: &str = "https://api.frankfurter.app/latest";

/// Fetch current USD prices for SOL and all token mints.
///
/// SOL is always fetched from CoinGecko (free, no key, no rate limit).
/// SPL tokens are fetched from Birdeye when an API key is available.
pub async fn fetch_prices(
    client: &Client,
    token_mints: &[String],
    _birdeye_key: Option<&str>,
) -> Result<HashMap<String, f64>> {
    // SOL via CoinGecko — always, regardless of Birdeye key
    let mut prices = fetch_sol_binance(client).await?;

    // SPL tokens via DexScreener — free, no key, no rate limits.
    if !token_mints.is_empty() {
        match fetch_token_prices_dexscreener(client, token_mints).await {
            Ok(token_prices) => prices.extend(token_prices),
            Err(e) => tracing::warn!("portfolio: DexScreener price fetch failed: {e}"),
        }
    }

    Ok(prices)
}

/// DexScreener batch token price — up to 30 mints per request, free, no key.
/// Returns the USD price from the highest-liquidity Solana pair for each mint.
async fn fetch_token_prices_dexscreener(
    client: &Client,
    token_mints: &[String],
) -> Result<HashMap<String, f64>> {
    // DexScreener accepts up to 30 comma-separated addresses per call.
    let mut prices = HashMap::new();
    for chunk in token_mints.chunks(30) {
        let addresses = chunk.join(",");
        let url = format!("{DEXSCREENER_URL}/{addresses}");
        let body: serde_json::Value = client
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let pairs = body.as_array().cloned().unwrap_or_default();
        for pair in &pairs {
            // Pick the base token address and its USD price.
            let mint = pair
                .get("baseToken")
                .and_then(|bt| bt.get("address"))
                .and_then(|a| a.as_str());
            let price = pair
                .get("priceUsd")
                .and_then(|p| p.as_str())
                .and_then(|s| s.parse::<f64>().ok());

            if let (Some(mint), Some(price)) = (mint, price) {
                // Keep the highest-liquidity price when multiple pairs exist.
                prices
                    .entry(mint.to_string())
                    .and_modify(|existing: &mut f64| {
                        let liq = pair
                            .get("liquidity")
                            .and_then(|l| l.get("usd"))
                            .and_then(|v| v.as_f64())
                            .unwrap_or(0.0);
                        if liq > 0.0 {
                            *existing = price;
                        }
                    })
                    .or_insert(price);
            }
        }
    }
    Ok(prices)
}

/// Binance public REST API — SOL/USDC spot price, no key required.
async fn fetch_sol_binance(client: &Client) -> Result<HashMap<String, f64>> {
    let body: serde_json::Value = client
        .get(BINANCE_PRICE_URL)
        .query(&[("symbol", "SOLUSDC")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let price: f64 = body
        .get("price")
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow!("unexpected Binance response"))?
        .parse()?;

    let mut prices = HashMap::new();
    prices.insert("SOL".to_string(), price);
    prices.insert(SOL_MINT.to_string(), price);
    Ok(prices)
}

/// Fetch the current USD → EUR exchange rate from Frankfurter (ECB data, free, no key).
pub async fn fetch_eur_rate(client: &Client) -> Result<f64> {
    let body: serde_json::Value = client
        .get(FRANKFURTER_URL)
        .query(&[("from", "USD"), ("to", "EUR")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    body.get("rates")
        .and_then(|r| r.get("EUR"))
        .and_then(|v| v.as_f64())
        .ok_or_else(|| anyhow!("unexpected Frankfurter response"))
}

/// Resolve token symbols from DexScreener for mints not found in Jupiter's list.
/// Returns mint → symbol for any mint that has at least one Solana trading pair.
pub async fn resolve_symbols_dexscreener(
    client: &Client,
    mints: &[String],
) -> HashMap<String, String> {
    let mut symbols = HashMap::new();
    for chunk in mints.chunks(30) {
        let addresses = chunk.join(",");
        let url = format!("{DEXSCREENER_URL}/{addresses}");
        let Ok(resp) = client.get(&url).send().await else { continue };
        let Ok(body) = resp.json::<serde_json::Value>().await else { continue };
        let pairs = body.as_array().cloned().unwrap_or_default();
        for pair in &pairs {
            let Some(base) = pair.get("baseToken") else { continue };
            let Some(mint) = base.get("address").and_then(|a| a.as_str()) else { continue };
            let Some(symbol) = base.get("symbol").and_then(|s| s.as_str()) else { continue };
            // Only store the first (highest-liquidity) symbol per mint
            symbols.entry(mint.to_string()).or_insert_with(|| symbol.to_string());
        }
    }
    symbols
}

/// Birdeye OHLCV returns at most 1000 candles per request at 1m resolution.
const BIRDEYE_PAGE_SECONDS: u64 = 1000 * 60; // ~16.7 hours per page

/// Backfill price history from Birdeye for a single mint over [from_ts, to_ts].
/// Paginates automatically so the caller can request up to 7 days without
/// worrying about the per-request candle limit.
pub async fn fetch_history_birdeye(
    client: &Client,
    api_key: &str,
    mint: &str,
    from_ts: u64,
    to_ts: u64,
) -> Result<Vec<PriceSnapshot>> {
    let mut all: Vec<PriceSnapshot> = Vec::new();
    let mut chunk_from = from_ts;

    while chunk_from < to_ts {
        let chunk_to = (chunk_from + BIRDEYE_PAGE_SECONDS).min(to_ts);

        if !all.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }

        let body: serde_json::Value = client
            .get(BIRDEYE_HISTORY_URL)
            .header("X-API-KEY", api_key)
            .query(&[
                ("address", mint),
                ("address_type", "token"),
                ("type", "1m"),
                ("time_from", &chunk_from.to_string()),
                ("time_to", &chunk_to.to_string()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let items = body
            .get("data")
            .and_then(|d| d.get("items"))
            .and_then(|i| i.as_array())
            .ok_or_else(|| anyhow!("unexpected Birdeye history response shape"))?;

        let page: Vec<PriceSnapshot> = items
            .iter()
            .filter_map(|item| {
                let ts = item.get("unixTime")?.as_u64()?;
                let price = item.get("value")?.as_f64()?;
                let mut prices = HashMap::new();
                prices.insert(mint.to_string(), price);
                Some(PriceSnapshot { ts, prices })
            })
            .collect();

        all.extend(page);
        chunk_from = chunk_to;
    }

    Ok(all)
}
