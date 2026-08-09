use anyhow::{anyhow, Result};
use reqwest::Client;
use std::collections::{HashMap, VecDeque};

use super::history::PriceSnapshot;

/// 30-day daily statistics for one asset, keyed by both mint and symbol.
/// `sma` is the mean of the daily series; `sigma` is its sample standard
/// deviation (Bessel's correction, /(n-1)); `n` is the number of daily points.
/// Bollinger bands are `sma ± k·sigma`.
#[derive(Debug, Clone, Copy)]
pub struct DailyBands {
    pub sma: f64,
    pub sigma: f64,
    pub n: usize,
}

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT_MINT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
/// Quote tokens whose own USD price DexScreener derives reliably (USD-stables + SOL).
/// A base token's `priceUsd` is only trusted when discovered against one of these:
/// pricing against a thin/exotic quote (e.g. a JUP/MET pool) inherits that quote
/// token's mis-derived USD value and yields garbage — a ghost JUP/MET pool reported
/// JUP at ~$1110 (vs the real ~$0.218 from every JUP/USDC and JUP/SOL pool), and its
/// spoofed $260M volume won the volume ranking, poisoning the price feed.
const TRUSTED_QUOTE_MINTS: [&str; 3] = [USDC_MINT, USDT_MINT, SOL_MINT];
const BIRDEYE_HISTORY_URL: &str = "https://public-api.birdeye.so/defi/history_price";
const COINGECKO_URL: &str = "https://api.coingecko.com/api/v3";
// `latest/dex/tokens/{mint}` returns the FULL pool list for ONE mint as
// `{ "pairs": [...] }`, letting us pick the genuinely deepest pool. We query per-mint
// rather than batching because this endpoint caps the response at ~30 pairs *total* —
// a multi-mint batch silently drops mints once the cap is hit. The older
// `tokens/v1/solana/{addrs}` batched but returned one arbitrary (often mispriced) pool
// per mint. Per-mint gives both full coverage and the deepest-pool price.
const DEXSCREENER_URL: &str = "https://api.dexscreener.com/latest/dex/tokens";
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

/// DexScreener per-mint token price — free, no key. For each mint, returns the USD
/// price from the pool with the most **24h volume** (real price discovery) in which the
/// mint is the *base* token. A mint with no usable base pool is simply absent from the
/// result (callers carry forward the previous value rather than recording $0).
async fn fetch_token_prices_dexscreener(
    client: &Client,
    token_mints: &[String],
) -> Result<HashMap<String, f64>> {
    let mut prices = HashMap::new();
    for mint in token_mints {
        match best_base_pair_price(client, mint).await {
            Ok(Some(price)) => {
                prices.insert(mint.clone(), price);
            }
            Ok(None) => {} // no base pool — leave it to carry-forward
            Err(e) => tracing::warn!("portfolio: DexScreener price for {mint} failed: {e}"),
        }
    }
    Ok(prices)
}

/// Query one mint's full pool list and return the USD price of the most-traded pool in
/// which that mint is the *base* token (so `priceUsd` is the mint's own price, not the
/// counter token's). Pools are ranked by **24h volume first, liquidity as tiebreak**:
/// raw TVL is trivially spoofed (a ghost pool can report hundreds of millions in
/// liquidity at a bogus price while seeing almost no trades), whereas sustained volume
/// tracks where the asset actually changes hands. Liquidity only breaks ties / covers
/// the rare token that has pools but zero recent volume. `Ok(None)` = no base pool.
async fn best_base_pair_price(client: &Client, mint: &str) -> Result<Option<f64>> {
    let url = format!("{DEXSCREENER_URL}/{mint}");
    let body: serde_json::Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    // `latest/dex/tokens` nests the pool list under `pairs`.
    let pairs = body.get("pairs").and_then(|p| p.as_array());
    Ok(pairs.and_then(|p| select_base_pair_price(p, mint)))
}

/// Pure pool-selection: from a DexScreener `pairs` array, return the `priceUsd` of the
/// pool where `mint` is the *base* token **and** the quote token is a trusted USD
/// reference (`TRUSTED_QUOTE_MINTS`), ranked by (24h volume, liquidity). Kept I/O-free
/// so the ranking — the part that defends against mispriced ghost pools — is
/// unit-tested directly. `None` if no such pool exists (caller carries the previous
/// value forward rather than recording a garbage price).
fn select_base_pair_price(pairs: &[serde_json::Value], mint: &str) -> Option<f64> {
    let mut best: Option<(f64, f64, f64)> = None; // (price, volume_24h, liquidity)
    for pair in pairs {
        // Only trust pairs where our mint is the BASE token — otherwise `priceUsd` is
        // the counter token's price.
        let is_base = pair
            .get("baseToken")
            .and_then(|bt| bt.get("address"))
            .and_then(|a| a.as_str())
            == Some(mint);
        if !is_base {
            continue;
        }
        // ...AND the quote token must be a stable/SOL whose USD price is reliable.
        // A pool quoted in a thin token (e.g. JUP/MET) derives `priceUsd` from that
        // token's mis-priced USD value → garbage, regardless of volume.
        let quote_trusted = pair
            .get("quoteToken")
            .and_then(|qt| qt.get("address"))
            .and_then(|a| a.as_str())
            .is_some_and(|a| TRUSTED_QUOTE_MINTS.contains(&a));
        if !quote_trusted {
            continue;
        }
        let Some(price) = pair
            .get("priceUsd")
            .and_then(|p| p.as_str())
            .and_then(|s| s.parse::<f64>().ok())
        else {
            continue;
        };
        let vol = pair
            .get("volume")
            .and_then(|v| v.get("h24"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let liq = pair
            .get("liquidity")
            .and_then(|l| l.get("usd"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        // Rank by (volume, liquidity) lexicographically.
        if best.is_none_or(|(_, best_vol, best_liq)| (vol, liq) > (best_vol, best_liq)) {
            best = Some((price, vol, liq));
        }
    }
    best.map(|(price, _, _)| price)
}

/// One gRPC-wireable venue for a mint, as resolved from DexScreener: the pool address,
/// the DexScreener `dexId` (which decoder script can turn it into a `PoolConfig`), and the
/// quote side in the `PoolRef` convention ("SOL" | "USDC").
///
/// Used for tokens that are NOT in the curated watch list — an adopted unwatched wallet
/// holding has no `pool`/`quote` in `momentum_tokens.json`, and `spawn_grpc_feed` wires a
/// token *only* through `WatchedToken::pool_refs()`, so without a resolved venue such a
/// position stays REST-priced for its whole life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPool {
    pub pool: String,
    pub dex: String,
    pub quote: String,
}

/// DexScreener `dexId`s the watcher can decode into a `PoolConfig` (see
/// `watcher::dex_to_decode_script`). Anything else has no `--pools` decoder, so admitting
/// it would only produce a failed decode and a needless feed re-spawn.
const WIREABLE_DEX_IDS: [&str; 4] = ["pumpswap", "raydium", "orca", "meteora"];

/// Pure ranking half of `resolve_best_pool` (unit-tested): from a DexScreener
/// `{"pairs":[…]}` body, pick the highest **24h volume** pair whose `dexId` has a decoder
/// and whose quote token is WSOL or USDC. Volume — never liquidity — is the ranking key:
/// raw TVL is trivially spoofed, and a fake-TVL pool would wire the feed to a price nobody
/// trades at. USDT quotes are deliberately excluded: `PoolRef.quote` is SOL|USDC only, so a
/// USDT venue is not expressible. A missing `volume.h24` counts as 0.0 so a brand-new venue
/// is still wireable when it is the only one. `None` = nothing wireable → caller stays REST.
pub fn pick_best_pool(pairs_json: &serde_json::Value) -> Option<ResolvedPool> {
    let pairs = pairs_json.get("pairs").and_then(|p| p.as_array())?;
    let mut best: Option<(ResolvedPool, f64)> = None;
    for pair in pairs {
        let Some(dex) = pair.get("dexId").and_then(|d| d.as_str()) else { continue };
        if !WIREABLE_DEX_IDS.contains(&dex) {
            continue;
        }
        let Some(pool) = pair.get("pairAddress").and_then(|p| p.as_str()) else { continue };
        let Some(quote_mint) = pair
            .get("quoteToken")
            .and_then(|q| q.get("address"))
            .and_then(|a| a.as_str())
        else {
            continue;
        };
        let quote = match quote_mint {
            SOL_MINT => "SOL",
            USDC_MINT => "USDC",
            _ => continue, // exotic (or USDT) quote — not expressible as a PoolRef
        };
        let vol = pair
            .get("volume")
            .and_then(|v| v.get("h24"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if best.as_ref().is_none_or(|(_, best_vol)| vol > *best_vol) {
            best = Some((
                ResolvedPool {
                    pool: pool.to_string(),
                    dex: dex.to_string(),
                    quote: quote.to_string(),
                },
                vol,
            ));
        }
    }
    best.map(|(r, _)| r)
}

/// Resolve the best gRPC-wireable venue for one mint from DexScreener (same per-mint
/// endpoint the price path uses). Any transport/parse failure → `None`: venue resolution
/// is an optimization (gRPC instead of REST pricing), so it **fails open** rather than
/// disturbing a held position. The caller logs the miss (once per streak, not per tick).
pub async fn resolve_best_pool(http: &Client, mint: &str) -> Option<ResolvedPool> {
    let url = format!("{DEXSCREENER_URL}/{mint}");
    let resp = http.get(&url).send().await.ok()?.error_for_status().ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    pick_best_pool(&body)
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
    // Query per-mint (same ~30-pair cap reason as the price path) and take the symbol
    // from the deepest pool where the mint is the base token.
    let mut symbols = HashMap::new();
    for mint in mints {
        let url = format!("{DEXSCREENER_URL}/{mint}");
        let Ok(resp) = client.get(&url).send().await else { continue };
        let Ok(body) = resp.json::<serde_json::Value>().await else { continue };
        let pairs = body.get("pairs").and_then(|p| p.as_array());
        let mut best: Option<(String, f64)> = None; // (symbol, volume_24h)
        for pair in pairs.into_iter().flatten() {
            let Some(base) = pair.get("baseToken") else { continue };
            if base.get("address").and_then(|a| a.as_str()) != Some(mint.as_str()) {
                continue;
            }
            let Some(symbol) = base.get("symbol").and_then(|s| s.as_str()) else { continue };
            let vol = pair
                .get("volume")
                .and_then(|v| v.get("h24"))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            if best.as_ref().is_none_or(|(_, best_vol)| vol > *best_vol) {
                best = Some((symbol.to_string(), vol));
            }
        }
        if let Some((symbol, _)) = best {
            symbols.insert(mint.clone(), symbol);
        }
    }
    symbols
}

/// Fetch 30 days of hourly price data from CoinGecko for a single mint.
/// SOL uses the coin-ID endpoint; every other token uses the contract-address endpoint.
///
/// Pass `demo_key` from `COINGECKO_DEMO_KEY` env var (free registration at coingecko.com/en/developers)
/// to get a reliable 30 req/min limit. Without a key the public tier allows ~10 req/min;
/// callers should space requests ≥6 s apart to stay safe.
///
/// Retries once after 12 s on a 429 response before returning an error.
pub async fn fetch_monthly_history_coingecko(
    client: &Client,
    mint: &str,
    demo_key: Option<&str>,
) -> Result<Vec<PriceSnapshot>> {
    let url = if mint == SOL_MINT {
        format!("{COINGECKO_URL}/coins/solana/market_chart")
    } else {
        format!("{COINGECKO_URL}/coins/solana/contract/{mint}/market_chart")
    };

    let send = |client: &Client| {
        let mut req = client
            .get(&url)
            .header("User-Agent", "Mozilla/5.0")
            .query(&[("vs_currency", "usd"), ("days", "30")]);
        if let Some(key) = demo_key {
            req = req.header("x-cg-demo-api-key", key);
        }
        req.send()
    };

    let resp = send(client).await?;
    let resp = if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        // Honour Retry-After if present, otherwise wait 15 s.
        let wait = resp
            .headers()
            .get("Retry-After")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(15);
        tokio::time::sleep(std::time::Duration::from_secs(wait + 2)).await;
        send(client).await?
    } else {
        resp
    };

    let body: serde_json::Value = resp.error_for_status()?.json().await?;

    let prices = body
        .get("prices")
        .and_then(|p| p.as_array())
        .ok_or_else(|| anyhow!("unexpected CoinGecko market_chart response shape"))?;

    Ok(prices
        .iter()
        .filter_map(|entry| {
            let arr = entry.as_array()?;
            // CoinGecko returns [timestamp_ms, price]
            let ts = (arr.first()?.as_f64()? / 1000.0) as u64;
            let price = arr.get(1)?.as_f64()?;
            let mut map = HashMap::new();
            map.insert(mint.to_string(), price);
            Some(PriceSnapshot { ts, prices: map })
        })
        .collect())
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

/// Sample standard deviation (Bessel's correction, ÷ n-1) of `values` given its
/// precomputed `mean`. Returns 0.0 for fewer than 2 values (undefined variance).
fn sample_sigma(values: &[f64], mean: f64) -> f64 {
    let n = values.len();
    if n < 2 { return 0.0; }
    (values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1) as f64).sqrt()
}

/// Fetch 30-day daily band statistics — mean (SMA), sample σ, and point count —
/// for every asset in the portfolio using Birdeye daily (`1D`) candles.  Returns
/// a `DailyBands` map keyed by **both** mint address and symbol so callers can
/// look up by either.  Assets with fewer than 7 daily candles are omitted
/// (insufficient history to form a meaningful average).
pub async fn fetch_monthly_sma(
    client: &Client,
    api_key: &str,
    portfolio: &super::Portfolio,
) -> HashMap<String, DailyBands> {
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

    let mut sma_map: HashMap<String, DailyBands> = HashMap::new();

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

        let n = prices.len();
        let sma = prices.iter().sum::<f64>() / n as f64;
        let sigma = sample_sigma(&prices, sma);
        tracing::info!(
            "portfolio: 30d SMA {symbol} = ${sma:.4} σ=${sigma:.4} ({n} candles)"
        );
        let bands = DailyBands { sma, sigma, n };
        sma_map.insert(mint.clone(), bands);
        sma_map.insert(symbol.clone(), bands);
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
) -> HashMap<String, DailyBands> {
    const SECS_PER_DAY: u64 = 86_400;

    let mut assets: Vec<(String, String)> = vec![
        (SOL_MINT.to_string(), "SOL".to_string()),
    ];
    for token in &portfolio.tokens {
        assets.push((token.mint.clone(), token.symbol.clone()));
    }

    let mut sma_map: HashMap<String, DailyBands> = HashMap::new();

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
        let n = values.len();
        let sma = values.iter().sum::<f64>() / n as f64;
        let sigma = sample_sigma(&values, sma);
        tracing::info!(
            "portfolio: {n}-day SMA {symbol} = ${sma:.4} σ=${sigma:.4} (local history)"
        );
        let bands = DailyBands { sma, sigma, n };
        sma_map.insert(mint.clone(), bands);
        sma_map.insert(symbol.clone(), bands);
    }

    sma_map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::history::PriceSnapshot;
    use crate::portfolio::{Portfolio, TokenEntry};
    use std::collections::{HashMap, VecDeque};

    #[test]
    fn test_daily_bands_sigma_from_history() {
        // Three distinct UTC days with closes 100, 110, 120.
        // mean = 110; sample σ = sqrt((100+0+100)/2) = 10.
        const DAY: u64 = 86_400;
        let mut history: VecDeque<PriceSnapshot> = VecDeque::new();
        for (i, p) in [100.0_f64, 110.0, 120.0].iter().enumerate() {
            let mut prices = HashMap::new();
            prices.insert("SOL".to_string(), *p);
            history.push_back(PriceSnapshot { ts: i as u64 * DAY, prices });
        }
        let portfolio = Portfolio { sol_amount: 1.0, tokens: Vec::<TokenEntry>::new() };

        let bands = compute_sma_from_history(&history, &portfolio);
        let sol = bands.get("SOL").expect("SOL bands present");
        assert!((sol.sma - 110.0).abs() < 1e-9, "sma was {}", sol.sma);
        assert!((sol.sigma - 10.0).abs() < 1e-9, "sigma was {}", sol.sigma);
        assert_eq!(sol.n, 3);
    }

    #[test]
    fn test_daily_bands_sigma_keyed_by_mint() {
        const DAY: u64 = 86_400;
        let mut history: VecDeque<PriceSnapshot> = VecDeque::new();
        for (i, p) in [100.0_f64, 110.0, 120.0].iter().enumerate() {
            let mut prices = HashMap::new();
            prices.insert(SOL_MINT.to_string(), *p);
            history.push_back(PriceSnapshot { ts: i as u64 * DAY, prices });
        }
        let portfolio = Portfolio { sol_amount: 1.0, tokens: Vec::<TokenEntry>::new() };

        let bands = compute_sma_from_history(&history, &portfolio);
        let sol = bands.get("SOL").expect("SOL bands present via mint-keyed prices");
        assert!((sol.sma - 110.0).abs() < 1e-9, "sma was {}", sol.sma);
        assert!((sol.sigma - 10.0).abs() < 1e-9, "sigma was {}", sol.sigma);
        assert_eq!(sol.n, 3);
    }

    // Build one DexScreener pool object for `select_base_pair_price` tests. Quote
    // defaults to USDC (a trusted quote) so existing volume/liquidity-ranking tests
    // exercise selection rather than the quote filter.
    fn pool(base_mint: &str, price: &str, vol_h24: f64, liq_usd: f64) -> serde_json::Value {
        pool_q(base_mint, USDC_MINT, price, vol_h24, liq_usd)
    }

    fn pool_q(base_mint: &str, quote_mint: &str, price: &str, vol_h24: f64, liq_usd: f64) -> serde_json::Value {
        serde_json::json!({
            "baseToken": { "address": base_mint },
            "quoteToken": { "address": quote_mint },
            "priceUsd": price,
            "volume": { "h24": vol_h24 },
            "liquidity": { "usd": liq_usd },
        })
    }

    #[test]
    fn select_base_pair_prefers_volume_over_fake_tvl() {
        // The real bug: a ghost pool reports $342M liquidity at a bogus $663 but trades
        // almost nothing, while the genuine pool sits at $0.15 with millions in volume.
        // Ranking by volume (not liquidity) must return the real price.
        const MINT: &str = "METvsvVRapdj9cFLzq4Tr43xK4tAjQfwX76z3n6mWQL";
        let pairs = vec![
            pool(MINT, "663.39", 141_184.0, 342_581_729.0), // fake-TVL ghost pool
            pool(MINT, "0.1510", 2_464_333.0, 761_320.0),   // real, most-traded
            pool(MINT, "0.1498", 801_728.0, 1_957_393.0),   // real, deeper but less volume
        ];
        let price = select_base_pair_price(&pairs, MINT).expect("price present");
        assert!((price - 0.1510).abs() < 1e-9, "picked {price}, expected the high-volume $0.1510 pool");
    }

    #[test]
    fn select_base_pair_rejects_untrusted_quote_token() {
        // The JUP bug: a ghost JUP/MET pool with spoofed $260M volume reports JUP at
        // $1110 (MET's USD value is mis-derived), out-ranking the real JUP/USDC and
        // JUP/SOL pools at ~$0.218. Volume ranking alone picks the ghost; the quote
        // whitelist must reject the MET-quoted pool and return the real price.
        const JUP: &str = "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN";
        const MET: &str = "METvsvVRapdj9cFLzq4Tr43xK4tAjQfwX76z3n6mWQL";
        let pairs = vec![
            pool_q(JUP, MET, "1110.34", 261_522_958.0, 90_306_928.0), // ghost — must be ignored
            pool_q(JUP, USDC_MINT, "0.2186", 6_179_678.0, 1_438_182.0), // real, highest trusted volume
            pool_q(JUP, SOL_MINT, "0.2182", 1_571_355.0, 592_766.0),    // real SOL pool
        ];
        let price = select_base_pair_price(&pairs, JUP).expect("price present");
        assert!((price - 0.2186).abs() < 1e-9, "picked {price}, expected the real $0.2186 USDC pool");

        // If the ONLY base pool has an untrusted quote → None (carry-forward, never garbage).
        let only_ghost = select_base_pair_price(&pairs[..1], JUP);
        assert!(only_ghost.is_none(), "untrusted-quote-only must yield None, got {only_ghost:?}");
    }

    #[test]
    fn select_base_pair_ignores_quote_side_and_falls_back_to_liquidity() {
        const MINT: &str = "So11111111111111111111111111111111111111112";
        // A pool where MINT is the QUOTE token would carry the counter token's price — it
        // must be ignored (baseToken.address != MINT).
        let quote_side = serde_json::json!({
            "baseToken": { "address": "OtherTokenMint11111111111111111111111111111" },
            "quoteToken": { "address": USDC_MINT },
            "priceUsd": "999999.0",
            "volume": { "h24": 9_999_999.0 },
            "liquidity": { "usd": 9_999_999.0 },
        });
        // Two base pools with ZERO volume → liquidity breaks the tie (deeper wins).
        let pairs = vec![
            quote_side,
            pool(MINT, "150.0", 0.0, 10_000.0),
            pool(MINT, "152.0", 0.0, 50_000.0), // deeper → chosen on the tiebreak
        ];
        let price = select_base_pair_price(&pairs, MINT).expect("price present");
        assert!((price - 152.0).abs() < 1e-9, "picked {price}, expected the deeper $152 base pool");

        // No base pool at all → None (caller carries the previous value forward).
        let none = select_base_pair_price(&pairs[..1], MINT);
        assert!(none.is_none(), "quote-only match must yield None, got {none:?}");
    }

    #[test]
    fn pick_best_pool_ranks_by_volume_filters_quote_and_dex() {
        let j: serde_json::Value = serde_json::json!({ "pairs": [
            { "dexId": "meteora", "pairAddress": "LOW",
              "quoteToken": { "address": "So11111111111111111111111111111111111111112" },
              "volume": { "h24": 100.0 } },
            { "dexId": "pumpswap", "pairAddress": "BEST",
              "quoteToken": { "address": "So11111111111111111111111111111111111111112" },
              "volume": { "h24": 900.0 } },
            { "dexId": "pumpswap", "pairAddress": "EXOTIC_QUOTE",
              "quoteToken": { "address": "SomeRandomQuoteMint111111111111111111111111" },
              "volume": { "h24": 5000.0 } },
            { "dexId": "unknown_dex", "pairAddress": "UNSUPPORTED",
              "quoteToken": { "address": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" },
              "volume": { "h24": 9000.0 } }
        ]});
        let got = pick_best_pool(&j).expect("one eligible pool");
        assert_eq!(got.pool, "BEST");
        assert_eq!(got.dex, "pumpswap");
        assert_eq!(got.quote, "SOL");
    }

    #[test]
    fn pick_best_pool_none_when_no_eligible_pairs() {
        let j = serde_json::json!({ "pairs": [] });
        assert!(pick_best_pool(&j).is_none());
    }

    #[test]
    fn pick_best_pool_maps_usdc_quote_and_ignores_usdt() {
        // USDT is a trusted PRICE quote but not a wireable PoolRef quote (the feed's
        // quote is SOL|USDC only), so a USDT pool must never win even on volume.
        let j = serde_json::json!({ "pairs": [
            { "dexId": "raydium", "pairAddress": "USDT_POOL",
              "quoteToken": { "address": "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" },
              "volume": { "h24": 9_000_000.0 } },
            { "dexId": "orca", "pairAddress": "USDC_POOL",
              "quoteToken": { "address": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" },
              "volume": { "h24": 12.0 } }
        ]});
        let got = pick_best_pool(&j).expect("the USDC pool is the only wireable venue");
        assert_eq!(got.pool, "USDC_POOL");
        assert_eq!(got.dex, "orca");
        assert_eq!(got.quote, "USDC");
    }

    #[test]
    fn pick_best_pool_missing_volume_counts_as_zero() {
        // A pair with no `volume` object must still be eligible (ranked at 0.0) so a
        // freshly-listed venue is wireable when it is the only one.
        let j = serde_json::json!({ "pairs": [
            { "dexId": "raydium", "pairAddress": "NO_VOL",
              "quoteToken": { "address": "So11111111111111111111111111111111111111112" } }
        ]});
        assert_eq!(pick_best_pool(&j).expect("eligible").pool, "NO_VOL");
        // …but any pair with real volume out-ranks it.
        let j2 = serde_json::json!({ "pairs": [
            { "dexId": "raydium", "pairAddress": "NO_VOL",
              "quoteToken": { "address": "So11111111111111111111111111111111111111112" } },
            { "dexId": "raydium", "pairAddress": "WITH_VOL",
              "quoteToken": { "address": "So11111111111111111111111111111111111111112" },
              "volume": { "h24": 1.0 } }
        ]});
        assert_eq!(pick_best_pool(&j2).expect("eligible").pool, "WITH_VOL");
    }

    #[test]
    fn pick_best_pool_none_when_pairs_key_absent_or_null() {
        // DexScreener returns `{"pairs": null}` for an unknown mint.
        assert!(pick_best_pool(&serde_json::json!({ "pairs": serde_json::Value::Null })).is_none());
        assert!(pick_best_pool(&serde_json::json!({})).is_none());
    }
}
