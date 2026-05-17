use anyhow::{anyhow, Result};
use reqwest::Client;
use std::collections::{HashMap, VecDeque};

use super::history::PriceSnapshot;

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const BIRDEYE_HISTORY_URL: &str = "https://public-api.birdeye.so/defi/history_price";
const DEXSCREENER_URL: &str = "https://api.dexscreener.com/tokens/v1/solana";
const KRAKEN_TICKER_URL: &str = "https://api.kraken.com/0/public/Ticker";
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
    let mut prices = fetch_sol_kraken(client).await?;

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

/// Kraken public REST API — SOL/USD spot price, no key required, EU-accessible.
async fn fetch_sol_kraken(client: &Client) -> Result<HashMap<String, f64>> {
    let body: serde_json::Value = client
        .get(KRAKEN_TICKER_URL)
        .query(&[("pair", "SOLUSD")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // Response: {"result":{"SOLUSD":{"c":["123.45","1"],...}}}
    // "c" = last trade closed [price, lot volume]
    let price: f64 = body
        .get("result")
        .and_then(|r| r.get("SOLUSD"))
        .and_then(|t| t.get("c"))
        .and_then(|c| c.get(0))
        .and_then(|p| p.as_str())
        .ok_or_else(|| anyhow!("unexpected Kraken response"))?
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
            .header("x-chain", "solana")
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

/// Fetch 30 days of hourly (`1H`) candles from Birdeye for a single mint.
/// Returns up to 720 snapshots (30 × 24) in a single request — no pagination needed.
/// Each snapshot contains only the requested mint as the price key.
pub async fn fetch_monthly_history(
    client: &Client,
    api_key: &str,
    mint: &str,
) -> Result<Vec<PriceSnapshot>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let from = now.saturating_sub(30 * 24 * 3600);

    let body: serde_json::Value = client
        .get(BIRDEYE_HISTORY_URL)
        .header("X-API-KEY", api_key)
        .header("x-chain", "solana")
        .query(&[
            ("address", mint),
            ("address_type", "token"),
            ("type", "1H"),
            ("time_from", &from.to_string()),
            ("time_to", &now.to_string()),
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
        .ok_or_else(|| anyhow!("unexpected Birdeye monthly history response"))?;

    Ok(items
        .iter()
        .filter_map(|item| {
            let ts = item.get("unixTime")?.as_u64()?;
            let price = item.get("value")?.as_f64()?;
            let mut prices = HashMap::new();
            prices.insert(mint.to_string(), price);
            Some(PriceSnapshot { ts, prices })
        })
        .collect())
}

/// Fetch the 30-day simple moving average price for every asset in the portfolio
/// using Birdeye daily (`1D`) candles.  Returns a map keyed by **both** mint address
/// and symbol so callers can look up by either.  Assets with fewer than 7 daily
/// candles are omitted (insufficient history to form a meaningful average).
pub async fn fetch_monthly_sma(
    client: &Client,
    api_key: &str,
    portfolio: &super::Portfolio,
) -> HashMap<String, f64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let from = now.saturating_sub(30 * 24 * 3600);

    // Build list of (mint, symbol) pairs — SOL uses its native mint.
    let mut assets: Vec<(String, String)> = vec![
        (SOL_MINT.to_string(), "SOL".to_string()),
    ];
    for token in &portfolio.tokens {
        assets.push((token.mint.clone(), token.symbol.clone()));
    }

    let mut sma_map: HashMap<String, f64> = HashMap::new();

    for (i, (mint, symbol)) in assets.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }

        // Fetch with full response body capture so we can log the error detail.
        let raw = match client
            .get(BIRDEYE_HISTORY_URL)
            .header("X-API-KEY", api_key)
            .header("x-chain", "solana")
            .query(&[
                ("address", mint.as_str()),
                ("address_type", "token"),
                ("type", "1D"),
                ("time_from", &from.to_string()),
                ("time_to", &now.to_string()),
            ])
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => { tracing::warn!("portfolio: SMA request failed for {symbol}: {e}"); continue; }
        };
        let status = raw.status();
        let text = raw.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!("portfolio: SMA fetch failed for {symbol}: HTTP {status} — {text}");
            continue;
        }
        let body: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => { tracing::warn!("portfolio: SMA parse failed for {symbol}: {e} body={text}"); continue; }
        };

        let prices: Vec<f64> = body
            .get("data")
            .and_then(|d| d.get("items"))
            .and_then(|i| i.as_array())
            .map(|items| items.iter().filter_map(|item| item.get("value")?.as_f64()).collect())
            .unwrap_or_default();

        if prices.len() < 7 {
            tracing::warn!("portfolio: SMA skipped for {symbol} — only {} daily candles", prices.len());
            continue;
        }

        let sma = prices.iter().sum::<f64>() / prices.len() as f64;
        tracing::info!("portfolio: 30d SMA {symbol} = ${sma:.4} ({} candles)", prices.len());
        sma_map.insert(mint.clone(), sma);
        sma_map.insert(symbol.clone(), sma);
    }

    sma_map
}

/// Compute a simple moving average for every portfolio asset using the local
/// price history — no API calls, no rate limits.  Prices are sampled at daily
/// resolution (last price recorded each UTC day) from the in-memory deque.
/// Returns a map keyed by both mint address and symbol (same shape as
/// `fetch_monthly_sma`) so callers can switch between the two transparently.
/// Assets with fewer than 2 daily data points are omitted.
pub fn compute_sma_from_history(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &super::Portfolio,
) -> HashMap<String, f64> {
    const SECS_PER_DAY: u64 = 86_400;

    let mut assets: Vec<(String, String)> = vec![
        (SOL_MINT.to_string(), "SOL".to_string()),
    ];
    for token in &portfolio.tokens {
        assets.push((token.mint.clone(), token.symbol.clone()));
    }

    let mut sma_map: HashMap<String, f64> = HashMap::new();

    for (mint, symbol) in &assets {
        // Collect the last recorded price for each UTC day.
        let mut daily: HashMap<u64, f64> = HashMap::new();
        for snap in history {
            let day = snap.ts / SECS_PER_DAY;
            if let Some(&p) = snap.prices.get(mint.as_str())
                .or_else(|| snap.prices.get(symbol.as_str()))
            {
                if p > 0.0 {
                    // Later snapshots overwrite earlier ones — keeps daily close.
                    daily.insert(day, p);
                }
            }
        }

        if daily.len() < 2 {
            continue;
        }

        let values: Vec<f64> = daily.values().cloned().collect();
        let sma = values.iter().sum::<f64>() / values.len() as f64;
        tracing::info!(
            "portfolio: {}-day SMA {symbol} = ${sma:.4} (local history)",
            daily.len()
        );
        sma_map.insert(mint.clone(), sma);
        sma_map.insert(symbol.clone(), sma);
    }

    sma_map
}
