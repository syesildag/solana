//! Process-wide SOL/USD price cache.
//!
//! The arbitrage bot's own in-process poller (see `fetch_sol_usd`, spawned from `main.rs`)
//! publishes the latest SOL/USD price; the arbitrage hot loop reads it lock-free to convert
//! a non-native base's profit into a SOL-equivalent lamport value for Jito-tip sizing. The
//! poller MUST run in the same process as the evaluator so `publish` + `get_fresh` share this
//! binary crate's static. When no fresh price is available the conversion yields 0, which
//! makes the tip logic fall back to the floor tip rather than bid on a stale rate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dex::types::BaseToken;

/// Tip sizing treats a price older than this (seconds) as missing. ~13× the in-process Kraken poll interval (45 s).
pub const PRICE_MAX_AGE_SECS: u64 = 600;

static SOL_PRICE_USD_BITS: AtomicU64 = AtomicU64::new(0);
static SOL_PRICE_TS_SECS: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Publish the latest SOL/USD price (called by the arbitrage bot's in-process Kraken poller,
/// spawned from main.rs — not the portfolio watcher).
pub fn publish(price_usd: f64) {
    SOL_PRICE_USD_BITS.store(price_usd.to_bits(), Ordering::Relaxed);
    SOL_PRICE_TS_SECS.store(now_secs(), Ordering::Relaxed);
}

/// Latest SOL/USD price if published within `max_age_secs`, else None.
pub fn get_fresh(max_age_secs: u64) -> Option<f64> {
    fresh_price(
        SOL_PRICE_USD_BITS.load(Ordering::Relaxed),
        SOL_PRICE_TS_SECS.load(Ordering::Relaxed),
        now_secs(),
        max_age_secs,
    )
}

/// Pure staleness/validity check (testable without touching the statics or the clock).
pub(crate) fn fresh_price(price_bits: u64, ts: u64, now: u64, max_age: u64) -> Option<f64> {
    if ts == 0 || now.saturating_sub(ts) > max_age {
        return None;
    }
    let px = f64::from_bits(price_bits);
    if px > 0.0 { Some(px) } else { None }
}

/// Convert `units` of a token with `decimals` to SOL-equivalent lamports at `sol_price_usd`
/// (USD per 1 SOL). Pure.
pub(crate) fn base_units_to_lamports(units: u64, decimals: u8, sol_price_usd: f64) -> u64 {
    if sol_price_usd <= 0.0 {
        return 0;
    }
    let usd_value = units as f64 / 10f64.powi(decimals as i32);
    let sol_value = usd_value / sol_price_usd;
    (sol_value * 1e9) as u64
}

/// SOL-equivalent lamports for the gross profit, used only for Jito-tip sizing.
/// Native base: identity (already lamports). SPL base: convert via the cached price,
/// returning 0 when no fresh price is available so the caller uses the floor tip.
pub fn gross_profit_for_tip(gross_base_units: u64, base: &BaseToken, sol_price_usd: Option<f64>) -> u64 {
    if base.is_native {
        return gross_base_units;
    }
    match sol_price_usd {
        Some(px) if px > 0.0 => base_units_to_lamports(gross_base_units, base.decimals, px),
        _ => 0,
    }
}

/// Convert SOL-equivalent lamports back to base-token smallest units at `sol_price_usd`
/// (USD per 1 SOL). Inverse of `base_units_to_lamports`. Pure. Returns 0 if price invalid.
pub(crate) fn lamports_to_base_units(lamports: u64, decimals: u8, sol_price_usd: f64) -> u64 {
    if sol_price_usd <= 0.0 {
        return 0;
    }
    let sol = lamports as f64 / 1e9;
    let usd = sol * sol_price_usd;
    (usd * 10f64.powi(decimals as i32)) as u64
}

/// Express a lamport-denominated SOL cost (tx fees + Jito tip) in base-token units for the
/// profit gate. Native base: identity (cost already in lamports == base units). SPL base:
/// convert via the price; returns None when no fresh price is available (caller must then
/// treat the cycle as unevaluable and skip it rather than mis-count cost).
pub fn sol_cost_in_base_units(cost_lamports: u64, base: &BaseToken, sol_price_usd: Option<f64>) -> Option<u64> {
    if base.is_native {
        return Some(cost_lamports);
    }
    match sol_price_usd {
        Some(px) if px > 0.0 => Some(lamports_to_base_units(cost_lamports, base.decimals, px)),
        _ => None,
    }
}

const KRAKEN_TICKER_URL: &str = "https://api.kraken.com/0/public/Ticker";

/// Pure parse of Kraken's Ticker JSON for the SOLUSD last-trade price.
pub(crate) fn parse_kraken_sol_usd(body: &serde_json::Value) -> Option<f64> {
    body.get("result")
        .and_then(|r| r.get("SOLUSD"))
        .and_then(|t| t.get("c"))
        .and_then(|c| c.get(0))
        .and_then(|p| p.as_str())
        .and_then(|s| s.parse::<f64>().ok())
}

/// Fetch SOL/USD spot from Kraken (no key, EU-accessible). Called by the arbitrage bot's
/// in-process poller so tip sizing has a fresh rate in the same process/static.
/// A 10 s per-request timeout prevents a slow/hung connection from blocking the poller.
pub async fn fetch_sol_usd(client: &reqwest::Client) -> anyhow::Result<f64> {
    let body: serde_json::Value = client
        .get(KRAKEN_TICKER_URL)
        .query(&[("pair", "SOLUSD")])
        .timeout(std::time::Duration::from_secs(10))
        .send().await?
        .error_for_status()?
        .json().await?;
    parse_kraken_sol_usd(&body).ok_or_else(|| anyhow::anyhow!("unexpected Kraken response"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::resolve_base_token;
    use crate::dex::types::{WSOL_MINT, USDC_MINT};

    #[test]
    fn base_units_to_lamports_usdc() {
        // 10 USDC (6dp) at $200/SOL = 0.05 SOL = 50_000_000 lamports
        assert_eq!(base_units_to_lamports(10_000_000, 6, 200.0), 50_000_000);
    }

    #[test]
    fn gross_for_tip_native_is_identity() {
        let sol = resolve_base_token(WSOL_MINT).unwrap();
        // Price is ignored for native base.
        assert_eq!(gross_profit_for_tip(400_000, &sol, None), 400_000);
        assert_eq!(gross_profit_for_tip(400_000, &sol, Some(200.0)), 400_000);
    }

    #[test]
    fn gross_for_tip_usdc_converts() {
        let usdc = resolve_base_token(USDC_MINT).unwrap();
        assert_eq!(gross_profit_for_tip(10_000_000, &usdc, Some(200.0)), 50_000_000);
    }

    #[test]
    fn gross_for_tip_usdc_stale_price_is_zero() {
        let usdc = resolve_base_token(USDC_MINT).unwrap();
        // None price → 0 so the caller uses the floor tip rather than bidding blind.
        assert_eq!(gross_profit_for_tip(10_000_000, &usdc, None), 0);
        assert_eq!(gross_profit_for_tip(10_000_000, &usdc, Some(0.0)), 0);
    }

    #[test]
    fn lamports_to_base_units_usdc_inverse() {
        // 50_000_000 lamports = 0.05 SOL at $200 = $10 = 10 USDC (6dp) = 10_000_000 units
        assert_eq!(lamports_to_base_units(50_000_000, 6, 200.0), 10_000_000);
        // round-trips with base_units_to_lamports
        assert_eq!(base_units_to_lamports(10_000_000, 6, 200.0), 50_000_000);
    }

    #[test]
    fn sol_cost_in_base_units_native_identity() {
        let sol = resolve_base_token(WSOL_MINT).unwrap();
        assert_eq!(sol_cost_in_base_units(123_456, &sol, None), Some(123_456));
    }

    #[test]
    fn sol_cost_in_base_units_usdc_and_stale() {
        let usdc = resolve_base_token(USDC_MINT).unwrap();
        assert_eq!(sol_cost_in_base_units(50_000_000, &usdc, Some(200.0)), Some(10_000_000));
        assert_eq!(sol_cost_in_base_units(50_000_000, &usdc, None), None); // stale → unevaluable
    }

    #[test]
    fn fresh_price_respects_staleness() {
        let bits = 200.0_f64.to_bits();
        assert_eq!(fresh_price(bits, 100, 150, 60), Some(200.0)); // 50s old, max 60 → fresh
        assert_eq!(fresh_price(bits, 100, 200, 60), None);        // 100s old, max 60 → stale
        assert_eq!(fresh_price(bits, 0,   200, 60), None);        // never published
        assert_eq!(fresh_price(0,    100, 100, 60), None);        // price 0.0 → invalid
    }

    #[test]
    fn parses_kraken_sol_usd() {
        let ok = serde_json::json!({"result":{"SOLUSD":{"c":["123.45","1.0"]}}});
        assert_eq!(parse_kraken_sol_usd(&ok), Some(123.45));
        assert_eq!(parse_kraken_sol_usd(&serde_json::json!({"error":[]})), None);
    }
}
