use std::collections::{HashMap, VecDeque};
use std::fmt;

use serde::Serialize;

use super::history::PriceSnapshot;
use super::pricer::DailyBands;
use super::Portfolio;

#[derive(Debug, Clone)]
pub struct Alert {
    pub symbol: String,
    pub kind: AlertKind,
    pub current_price: f64,
    pub current_value_usd: f64,
}

#[derive(Debug, Clone)]
pub enum AlertKind {
    BigMove5m { pct: f64 },
    BigMove1h { pct: f64 },
    New7dHigh { prev_high: f64 },
    New7dLow { prev_low: f64 },
    ZScoreSpike { z: f64, threshold: f64, return_pct: f64 },
    PriceBelow { threshold: f64 },
    PriceAbove { threshold: f64 },
}

impl fmt::Display for AlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlertKind::BigMove5m { pct } => write!(f, "{:+.2}% in 5 minutes", pct),
            AlertKind::BigMove1h { pct } => write!(f, "{:+.2}% in 1 hour", pct),
            AlertKind::New7dHigh { .. } => write!(f, "new 7-day high"),
            AlertKind::New7dLow { .. } => write!(f, "new 7-day low"),
            AlertKind::ZScoreSpike { z, return_pct, .. } => {
                write!(f, "z-score spike: z={:+.2} ({:+.2}% return)", z, return_pct)
            }
            AlertKind::PriceBelow { threshold } => {
                write!(f, "price dropped below ${threshold:.4}")
            }
            AlertKind::PriceAbove { threshold } => {
                write!(f, "price rose above ${threshold:.4}")
            }
        }
    }
}

pub struct AnalysisConfig {
    pub alert_pct_5m: f64,
    pub alert_pct_1h: f64,
    pub zscore_lambda: f64,
    pub zscore_threshold: f64,
    pub zscore_min_obs: usize,
    /// Per-asset absolute price floors in USD. Alert fires when price < threshold.
    /// Parsed from ALERT_PRICE_BELOW env var (e.g. "USDY:0.96,SOL:70.0").
    pub price_thresholds: Vec<(String, f64)>,
    /// Per-asset absolute price ceilings in USD. Alert fires when price > threshold.
    /// Parsed from ALERT_PRICE_ABOVE env var (e.g. "USDY:1.04,SOL:300.0").
    pub price_ceilings: Vec<(String, f64)>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssetRisk {
    pub symbol: String,
    pub z_score: Option<f64>,
    pub sigma_ann: Option<f64>,       // annualized vol as percentage (e.g. 82.4 for 82.4%)
    pub current_drawdown_pct: f64,    // <= 0
    pub max_drawdown_pct: f64,        // <= 0, worst in window
    pub current_value_eur: f64,
    pub drawdown_eur: f64,            // >= 0, EUR loss from peak
    pub is_warm: bool,
    pub n_obs: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct RiskReport {
    pub assets: Vec<AssetRisk>,
    pub total_value_eur: f64,
    pub total_drawdown_eur: f64,
    /// Drawdown of the combined portfolio value curve from its historical peak.
    /// Unlike total_drawdown_eur (sum of per-asset peaks), this measures the
    /// single moment the whole portfolio was worth the most.
    pub portfolio_drawdown_pct: f64,
    pub portfolio_drawdown_eur: f64,
}

impl RiskReport {
    pub fn empty() -> Self {
        Self {
            assets: vec![],
            total_value_eur: 0.0,
            total_drawdown_eur: 0.0,
            portfolio_drawdown_pct: 0.0,
            portfolio_drawdown_eur: 0.0,
        }
    }
}

struct EwmaState {
    ewma_mean: f64,
    ewma_var: f64,
    n_obs: usize,
}

fn ewma_for_asset(prices: &[f64], lambda: f64) -> Option<EwmaState> {
    if prices.len() < 2 {
        return None;
    }
    let mut mean = 0.0_f64;
    let mut var = 0.0_f64;
    let mut n_obs = 0usize;
    for i in 1..prices.len() {
        let prev = prices[i - 1];
        let curr = prices[i];
        if prev <= 0.0 || curr <= 0.0 {
            continue;
        }
        let r = (curr / prev).ln();
        // Skip exactly-zero returns: they indicate a stale price (e.g. tokenized
        // stocks outside US market hours), not genuine zero volatility. Including
        // them drives EWMA variance toward zero overnight, causing the first real
        // move after open to appear as an extreme z-score.
        if !r.is_finite() || r == 0.0 {
            continue;
        }
        let prev_mean = mean;
        mean = lambda * mean + (1.0 - lambda) * r;
        var = lambda * var + (1.0 - lambda) * (r - prev_mean).powi(2);
        n_obs += 1;
    }
    Some(EwmaState { ewma_mean: mean, ewma_var: var, n_obs })
}

/// Returns (current_dd_pct, max_dd_pct, peak_price). Both dd values are <= 0.
fn drawdown_stats(prices: &[f64]) -> (f64, f64, f64) {
    if prices.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut peak = prices[0];
    let mut max_dd = 0.0_f64;
    for &p in prices {
        if p > peak { peak = p; }
        if peak > 0.0 {
            let dd = (p - peak) / peak;
            if dd < max_dd { max_dd = dd; }
        }
    }
    let last = *prices.last().unwrap();
    let current_dd = if peak > 0.0 { (last - peak) / peak * 100.0 } else { 0.0 };
    (current_dd, max_dd * 100.0, peak)
}

pub fn compute_risk(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    eur_rate: f64,
    cfg: &AnalysisConfig,
) -> RiskReport {
    let Some(latest) = history.back() else {
        return RiskReport::empty();
    };

    let sol_entry = [("SOL", "SOL", portfolio.sol_amount)];
    let token_entries: Vec<(&str, &str, f64)> = portfolio
        .tokens
        .iter()
        .map(|t| (t.symbol.as_str(), t.mint.as_str(), t.amount))
        .collect();
    let all_assets = sol_entry
        .iter()
        .map(|(s, k, a)| (*s, *k, *a))
        .chain(token_entries.iter().copied());

    let mut assets = Vec::new();
    let mut total_value_eur = 0.0_f64;
    let mut total_drawdown_eur = 0.0_f64;

    for (symbol, key, amount) in all_assets {
        let Some(&current_price) = latest.prices.get(key) else { continue; };

        let prices: Vec<f64> = history
            .iter()
            .filter_map(|snap| snap.prices.get(key).copied())
            .filter(|&p| p > 0.0)
            .collect();

        let ewma = ewma_for_asset(&prices, cfg.zscore_lambda);
        let n_obs = ewma.as_ref().map_or(0, |e| e.n_obs);
        let is_warm = n_obs >= cfg.zscore_min_obs;

        let z_score = if is_warm {
            ewma.as_ref().and_then(|e| {
                if e.ewma_var < 1e-12 { return None; }
                let n = prices.len();
                if n < 2 { return None; }
                let r = (prices[n - 1] / prices[n - 2]).ln();
                if r.is_finite() {
                    Some((r - e.ewma_mean) / e.ewma_var.sqrt())
                } else {
                    None
                }
            })
        } else {
            None
        };

        let sigma_ann = ewma.as_ref().and_then(|e| {
            if e.ewma_var >= 1e-12 {
                Some((e.ewma_var * 525_600.0_f64).sqrt() * 100.0)
            } else {
                None
            }
        });

        let (current_drawdown_pct, max_drawdown_pct, peak_price) = drawdown_stats(&prices);
        let current_value_eur = amount * current_price * eur_rate;
        let peak_value_eur = amount * peak_price * eur_rate;
        let drawdown_eur = (peak_value_eur - current_value_eur).max(0.0);

        total_value_eur += current_value_eur;
        total_drawdown_eur += drawdown_eur;

        assets.push(AssetRisk {
            symbol: symbol.to_string(),
            z_score,
            sigma_ann,
            current_drawdown_pct,
            max_drawdown_pct,
            current_value_eur,
            drawdown_eur,
            is_warm,
            n_obs,
        });
    }

    // Portfolio-level drawdown: compute Σ(asset_value) at each historical tick
    // and find the peak of that combined curve.
    let portfolio_series: Vec<f64> = history
        .iter()
        .map(|snap| {
            let sol = snap.prices.get("SOL").copied().unwrap_or(0.0);
            let mut v = portfolio.sol_amount * sol * eur_rate;
            for token in &portfolio.tokens {
                let p = snap.prices.get(&token.mint)
                    .or_else(|| snap.prices.get(&token.symbol))
                    .copied()
                    .unwrap_or(0.0);
                v += token.amount * p * eur_rate;
            }
            v
        })
        .filter(|&v| v > 0.0)
        .collect();

    let (portfolio_drawdown_pct, _, portfolio_peak_eur) = drawdown_stats(&portfolio_series);
    let portfolio_drawdown_eur = (portfolio_peak_eur - total_value_eur).max(0.0);

    RiskReport { assets, total_value_eur, total_drawdown_eur, portfolio_drawdown_pct, portfolio_drawdown_eur }
}

/// Analyse the price history for each asset in the portfolio and return triggered alerts.
pub fn analyze(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    risk: &RiskReport,
    cfg: &AnalysisConfig,
) -> Vec<Alert> {
    let Some(latest) = history.back() else {
        return vec![];
    };

    let mut alerts = Vec::new();

    // Build an iterator over (symbol, mint, amount) for all assets
    let sol_entry = [("SOL".to_string(), "SOL".to_string(), portfolio.sol_amount)];
    let token_entries: Vec<(String, String, f64)> = portfolio
        .tokens
        .iter()
        .map(|t| (t.symbol.clone(), t.mint.clone(), t.amount))
        .collect();

    let all_assets = sol_entry
        .iter()
        .map(|(sym, mint, amt)| (sym.as_str(), mint.as_str(), *amt))
        .chain(
            token_entries
                .iter()
                .map(|(sym, mint, amt)| (sym.as_str(), mint.as_str(), *amt)),
        );

    for (symbol, key, amount) in all_assets {
        let Some(&current_price) = latest.prices.get(key) else {
            continue;
        };
        let current_value = current_price * amount;

        // ── 5-minute change (5 snapshots back) ──────────────────────────
        let price_5m = lookback_price(history, key, 5);
        if let Some(old) = price_5m.filter(|&old| {
            let bad = implausible_move(old, current_price);
            if bad {
                tracing::warn!(
                    "portfolio: {symbol} 5m baseline ${old:.6} vs ${current_price:.6} is a >{MAX_PLAUSIBLE_MOVE_RATIO}x swing — suppressing as feed error"
                );
            }
            !bad
        }) {
            let pct = pct_change(old, current_price);
            if pct.abs() >= cfg.alert_pct_5m {
                alerts.push(Alert {
                    symbol: symbol.to_string(),
                    kind: AlertKind::BigMove5m { pct },
                    current_price,
                    current_value_usd: current_value,
                });
            }
        }

        // ── 1-hour change (60 snapshots back) ───────────────────────────
        let price_1h = lookback_price(history, key, 60);
        if let Some(old) = price_1h.filter(|&old| {
            let bad = implausible_move(old, current_price);
            if bad {
                tracing::warn!(
                    "portfolio: {symbol} 1h baseline ${old:.6} vs ${current_price:.6} is a >{MAX_PLAUSIBLE_MOVE_RATIO}x swing — suppressing as feed error"
                );
            }
            !bad
        }) {
            let pct = pct_change(old, current_price);
            if pct.abs() >= cfg.alert_pct_1h {
                alerts.push(Alert {
                    symbol: symbol.to_string(),
                    kind: AlertKind::BigMove1h { pct },
                    current_price,
                    current_value_usd: current_value,
                });
            }
        }

        // ── 7-day high / low ─────────────────────────────────────────────
        let window_7d: Vec<f64> = history
            .iter()
            .rev()
            .skip(1) // exclude the very latest so we compare against history
            .take(10_080)
            .filter_map(|snap| snap.prices.get(key).copied())
            .collect();

        if window_7d.len() >= 10_080 {
            let prev_high = window_7d.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let prev_low = window_7d.iter().cloned().fold(f64::INFINITY, f64::min);

            if current_price > prev_high {
                alerts.push(Alert {
                    symbol: symbol.to_string(),
                    kind: AlertKind::New7dHigh { prev_high },
                    current_price,
                    current_value_usd: current_value,
                });
            } else if current_price < prev_low {
                alerts.push(Alert {
                    symbol: symbol.to_string(),
                    kind: AlertKind::New7dLow { prev_low },
                    current_price,
                    current_value_usd: current_value,
                });
            }
        }

        // ── Absolute price floor ─────────────────────────────────────────
        for (thresh_symbol, threshold) in &cfg.price_thresholds {
            if thresh_symbol == symbol && current_price < *threshold {
                alerts.push(Alert {
                    symbol: symbol.to_string(),
                    kind: AlertKind::PriceBelow { threshold: *threshold },
                    current_price,
                    current_value_usd: current_value,
                });
            }
        }

        // ── Absolute price ceiling ───────────────────────────────────────
        for (ceil_symbol, threshold) in &cfg.price_ceilings {
            if ceil_symbol == symbol && current_price > *threshold {
                alerts.push(Alert {
                    symbol: symbol.to_string(),
                    kind: AlertKind::PriceAbove { threshold: *threshold },
                    current_price,
                    current_value_usd: current_value,
                });
            }
        }

        // ── EWMA z-score spike ───────────────────────────────────────────
        if let Some(asset_risk) = risk.assets.iter().find(|a| a.symbol == symbol) {
            if asset_risk.is_warm {
                if let Some(z) = asset_risk.z_score {
                    if z.abs() > cfg.zscore_threshold {
                        let return_pct = lookback_price(history, key, 1)
                            .map(|p| pct_change(p, current_price))
                            .unwrap_or(0.0);
                        alerts.push(Alert {
                            symbol: symbol.to_string(),
                            kind: AlertKind::ZScoreSpike {
                                z,
                                threshold: cfg.zscore_threshold,
                                return_pct,
                            },
                            current_price,
                            current_value_usd: current_value,
                        });
                    }
                }
            }
        }
    }

    alerts
}

/// A suggested rotation trade: sell the overbought asset, buy the oversold one.
#[derive(Debug, Clone)]
pub struct SwapSuggestion {
    pub sell_symbol: String,
    pub buy_symbol: String,
    pub sell_price: f64,
    pub sell_sma: f64,
    pub buy_price: f64,
    pub buy_sma: f64,
    pub sell_value_eur: f64,
    pub buy_value_eur: f64,
}

/// Identify rotation opportunities: assets at a 7-day extreme whose price also
/// deviates from their 30-day SMA in the same direction (confirming the signal).
///
/// Sell candidates: `New7dHigh` alert + current price > 30d SMA
/// Buy  candidates: `New7dLow`  alert + current price < 30d SMA
///
/// Returns one `SwapSuggestion` for every (sell, buy) pair found.
pub fn generate_swap_suggestions(
    alerts: &[Alert],
    monthly_sma: &HashMap<String, DailyBands>,
    risk: &RiskReport,
) -> Vec<SwapSuggestion> {
    if monthly_sma.is_empty() {
        return vec![];
    }

    let asset_map: HashMap<&str, &AssetRisk> = risk
        .assets
        .iter()
        .map(|a| (a.symbol.as_str(), a))
        .collect();

    let mut sell_candidates: Vec<(&Alert, f64)> = vec![];
    let mut buy_candidates: Vec<(&Alert, f64)> = vec![];

    for alert in alerts {
        let symbol = alert.symbol.as_str();
        let Some(sma) = monthly_sma.get(symbol) else { continue; };
        let Some(asset) = asset_map.get(symbol) else { continue; };
        let current_price = alert.current_price;

        match &alert.kind {
            AlertKind::New7dHigh { .. } if current_price > sma.sma => {
                sell_candidates.push((alert, asset.current_value_eur));
            }
            AlertKind::New7dLow { .. } if current_price < sma.sma => {
                buy_candidates.push((alert, asset.current_value_eur));
            }
            _ => {}
        }
    }

    let mut suggestions = Vec::new();
    for (sell_alert, sell_value_eur) in &sell_candidates {
        let sell_sma = monthly_sma[sell_alert.symbol.as_str()].sma;
        for (buy_alert, buy_value_eur) in &buy_candidates {
            let buy_sma = monthly_sma[buy_alert.symbol.as_str()].sma;
            suggestions.push(SwapSuggestion {
                sell_symbol: sell_alert.symbol.clone(),
                buy_symbol: buy_alert.symbol.clone(),
                sell_price: sell_alert.current_price,
                sell_sma,
                buy_price: buy_alert.current_price,
                buy_sma,
                sell_value_eur: *sell_value_eur,
                buy_value_eur: *buy_value_eur,
            });
        }
    }

    suggestions
}

fn lookback_price(history: &VecDeque<PriceSnapshot>, key: &str, n: usize) -> Option<f64> {
    let len = history.len();
    if len <= n {
        return None;
    }
    history[len - 1 - n].prices.get(key).copied()
}

fn pct_change(old: f64, new: f64) -> f64 {
    if old == 0.0 {
        return 0.0;
    }
    (new - old) / old * 100.0
}

/// A 5-minute or 1-hour "move" that crosses this ratio in *either* direction is treated
/// as a data error — a spoofed ghost pool that briefly poisoned the price feed, not a
/// real market move — and is suppressed rather than alerted. A spoofed JUP/MET pool once
/// priced JUP ~5000x too high; the poisoned baseline produced a bogus "JUP -99.98% in
/// 1 hour" email. At 10.0 the guard still lets through any genuine move down to -90% or
/// up to +900%; raise it to suppress less aggressively, lower it to catch glitches sooner.
const MAX_PLAUSIBLE_MOVE_RATIO: f64 = 10.0;

/// True when `old`→`new` is too large to be a real price move (likely a misprice in the
/// feed). Zero/negative inputs are not flagged here — `pct_change` already maps `old==0`
/// to 0% and absent prices never reach this path.
fn implausible_move(old: f64, new: f64) -> bool {
    old > 0.0 && new > 0.0 && (new / old > MAX_PLAUSIBLE_MOVE_RATIO || old / new > MAX_PLAUSIBLE_MOVE_RATIO)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::pricer::DailyBands;
    use crate::portfolio::{Portfolio, TokenEntry};
    use crate::portfolio::history::PriceSnapshot;
    use std::collections::{HashMap, VecDeque};

    fn make_cfg() -> AnalysisConfig {
        AnalysisConfig {
            alert_pct_5m: 3.0,
            alert_pct_1h: 10.0,
            zscore_lambda: 0.97,
            zscore_threshold: 2.5,
            zscore_min_obs: 30,
            price_thresholds: vec![],
            price_ceilings: vec![],
        }
    }

    fn make_history(prices: &[f64], key: &str) -> VecDeque<PriceSnapshot> {
        prices.iter().enumerate().map(|(i, &p)| {
            let mut map = HashMap::new();
            map.insert(key.to_string(), p);
            PriceSnapshot { ts: i as u64 * 60, prices: map }
        }).collect()
    }

    fn sol_portfolio() -> Portfolio {
        Portfolio { sol_amount: 10.0, tokens: vec![] }
    }

    fn _token_portfolio() -> Portfolio {
        Portfolio {
            sol_amount: 0.0,
            tokens: vec![TokenEntry {
                mint: "mint1".to_string(),
                symbol: "TKN".to_string(),
                amount: 100.0,
            }],
        }
    }

    #[test]
    fn test_risk_empty_history() {
        let report = compute_risk(&VecDeque::new(), &sol_portfolio(), 0.92, &make_cfg());
        assert!(report.assets.is_empty());
        assert_eq!(report.total_value_eur, 0.0);
    }

    #[test]
    fn test_risk_not_warm_below_min_obs() {
        let prices: Vec<f64> = (0..20).map(|i| 100.0 + i as f64 * 0.1).collect();
        let history = make_history(&prices, "SOL");
        let report = compute_risk(&history, &sol_portfolio(), 0.92, &make_cfg());
        let sol = &report.assets[0];
        assert!(!sol.is_warm);
        assert!(sol.z_score.is_none());
    }

    #[test]
    fn test_risk_warm_after_min_obs() {
        // 50 alternating-direction ticks -> nonzero variance
        let prices: Vec<f64> = (0..50usize).scan(100.0_f64, |p, i| {
            *p *= if i % 2 == 0 { 1.001 } else { 0.9995 };
            Some(*p)
        }).collect();
        let history = make_history(&prices, "SOL");
        let report = compute_risk(&history, &sol_portfolio(), 0.92, &make_cfg());
        let sol = &report.assets[0];
        assert!(sol.is_warm, "n_obs={} should be >= 30", sol.n_obs);
        assert!(sol.z_score.is_some());
        assert!(sol.sigma_ann.is_some());
    }

    #[test]
    fn test_risk_drawdown() {
        // Rise to 120, then fall to ~91.5
        let prices: Vec<f64> = (0..40)
            .map(|i| if i < 20 { 100.0 + i as f64 } else { 120.0 - (i - 20) as f64 * 1.5 })
            .collect();
        let history = make_history(&prices, "SOL");
        let report = compute_risk(&history, &sol_portfolio(), 0.92, &make_cfg());
        let sol = &report.assets[0];
        assert!(sol.max_drawdown_pct < -20.0,
            "expected > 20% drawdown, got {:.2}%", sol.max_drawdown_pct);
        assert!(sol.current_drawdown_pct < -20.0);
        assert!(sol.drawdown_eur > 0.0);
    }

    #[test]
    fn test_risk_eur_conversion() {
        // 40 identical prices: value = 10 sol * $100 * 0.92 = EUR 920
        let prices = vec![100.0_f64; 40];
        let history = make_history(&prices, "SOL");
        let report = compute_risk(&history, &sol_portfolio(), 0.92, &make_cfg());
        let sol = &report.assets[0];
        assert!((sol.current_value_eur - 920.0).abs() < 0.01,
            "expected 920.0, got {:.4}", sol.current_value_eur);
        assert!(sol.drawdown_eur < 0.01, "no drawdown on flat series");
    }

    #[test]
    fn test_risk_zero_variance_no_zscore() {
        let prices = vec![100.0_f64; 50];
        let history = make_history(&prices, "SOL");
        let report = compute_risk(&history, &sol_portfolio(), 0.92, &make_cfg());
        assert!(report.assets[0].z_score.is_none(),
            "z-score must be None when all returns are identical");
    }

    #[test]
    fn test_analyze_emits_zscore_spike() {
        // Stable series to warm up EWMA, then 10% spike on final tick
        let mut prices: Vec<f64> = (0..50usize).scan(100.0_f64, |p, i| {
            *p *= if i % 2 == 0 { 1.001 } else { 0.9995 };
            Some(*p)
        }).collect();
        prices.push(prices.last().unwrap() * 1.10);

        let history = make_history(&prices, "SOL");
        let cfg = make_cfg();
        let portfolio = sol_portfolio();
        let risk = compute_risk(&history, &portfolio, 0.92, &cfg);
        let alerts = analyze(&history, &portfolio, &risk, &cfg);
        assert!(
            alerts.iter().any(|a| matches!(a.kind, AlertKind::ZScoreSpike { .. })),
            "expected ZScoreSpike alert after 10% spike"
        );
    }

    #[test]
    fn test_analyze_no_zscore_when_not_warm() {
        let prices: Vec<f64> = (0..20usize).scan(100.0_f64, |p, _| { *p *= 1.001; Some(*p) }).collect();
        let history = make_history(&prices, "SOL");
        let cfg = make_cfg();
        let portfolio = sol_portfolio();
        let risk = compute_risk(&history, &portfolio, 0.92, &cfg);
        let alerts = analyze(&history, &portfolio, &risk, &cfg);
        assert!(
            !alerts.iter().any(|a| matches!(a.kind, AlertKind::ZScoreSpike { .. })),
            "should not emit ZScoreSpike before warm-up"
        );
    }

    // ── Swap suggestion tests ────────────────────────────────────────────────

    fn make_alert(symbol: &str, kind: AlertKind, price: f64) -> Alert {
        Alert { symbol: symbol.to_string(), kind, current_price: price, current_value_usd: price * 10.0 }
    }

    fn make_risk_with_asset(symbol: &str, _price_eur: f64, value_eur: f64) -> RiskReport {
        RiskReport {
            assets: vec![AssetRisk {
                symbol: symbol.to_string(),
                z_score: None, sigma_ann: None,
                current_drawdown_pct: 0.0, max_drawdown_pct: 0.0,
                current_value_eur: value_eur,
                drawdown_eur: 0.0,
                is_warm: true, n_obs: 100,
            }],
            total_value_eur: value_eur,
            total_drawdown_eur: 0.0,
            portfolio_drawdown_pct: 0.0,
            portfolio_drawdown_eur: 0.0,
        }
    }

    #[test]
    fn test_swap_suggestion_generated() {
        // NVDAx at 7d high above SMA → sell; GOOGLx at 7d low below SMA → buy
        let alerts = vec![
            make_alert("NVDAx", AlertKind::New7dHigh { prev_high: 190.0 }, 200.0),
            make_alert("GOOGLx", AlertKind::New7dLow { prev_low: 350.0 }, 340.0),
        ];
        let mut risk = make_risk_with_asset("NVDAx", 200.0, 119.0);
        risk.assets.push(AssetRisk {
            symbol: "GOOGLx".to_string(),
            z_score: None, sigma_ann: None,
            current_drawdown_pct: 0.0, max_drawdown_pct: 0.0,
            current_value_eur: 126.0,
            drawdown_eur: 0.0,
            is_warm: true, n_obs: 100,
        });
        let mut sma = HashMap::new();
        sma.insert("NVDAx".to_string(), DailyBands { sma: 185.0, sigma: 1.0, n: 30 });  // price 200 > sma 185 → sell
        sma.insert("GOOGLx".to_string(), DailyBands { sma: 360.0, sigma: 1.0, n: 30 }); // price 340 < sma 360 → buy

        let suggestions = generate_swap_suggestions(&alerts, &sma, &risk);
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].sell_symbol, "NVDAx");
        assert_eq!(suggestions[0].buy_symbol, "GOOGLx");
    }

    #[test]
    fn test_no_swap_without_sma() {
        // Signals present but SMA map is empty → no suggestions
        let alerts = vec![
            make_alert("NVDAx", AlertKind::New7dHigh { prev_high: 190.0 }, 200.0),
            make_alert("GOOGLx", AlertKind::New7dLow { prev_low: 350.0 }, 340.0),
        ];
        let risk = make_risk_with_asset("NVDAx", 200.0, 119.0);
        let suggestions = generate_swap_suggestions(&alerts, &HashMap::new(), &risk);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_no_swap_7dhigh_below_sma() {
        // 7d high but price is still below the SMA → not an overbought sell signal
        let alerts = vec![
            make_alert("NVDAx", AlertKind::New7dHigh { prev_high: 190.0 }, 180.0),
        ];
        let risk = make_risk_with_asset("NVDAx", 180.0, 107.0);
        let mut sma = HashMap::new();
        sma.insert("NVDAx".to_string(), DailyBands { sma: 185.0, sigma: 1.0, n: 30 }); // price 180 < sma 185 → NOT a sell
        let suggestions = generate_swap_suggestions(&alerts, &sma, &risk);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_no_swap_7dlow_above_sma() {
        // 7d low but price is still above the SMA → not an oversold buy signal
        let alerts = vec![
            make_alert("GOOGLx", AlertKind::New7dLow { prev_low: 330.0 }, 370.0),
        ];
        let risk = make_risk_with_asset("GOOGLx", 370.0, 137.0);
        let mut sma = HashMap::new();
        sma.insert("GOOGLx".to_string(), DailyBands { sma: 360.0, sigma: 1.0, n: 30 }); // price 370 > sma 360 → NOT a buy
        let suggestions = generate_swap_suggestions(&alerts, &sma, &risk);
        assert!(suggestions.is_empty());
    }

    #[test]
    fn test_price_below_threshold_fires() {
        let prices = vec![1.00_f64; 5];
        let history = make_history(&prices, "USDY");
        let cfg = AnalysisConfig {
            price_thresholds: vec![("USDY".to_string(), 0.96)],
            ..make_cfg()
        };
        let portfolio = Portfolio {
            sol_amount: 0.0,
            tokens: vec![TokenEntry { mint: "USDY".to_string(), symbol: "USDY".to_string(), amount: 10.0 }],
        };
        let risk = compute_risk(&history, &portfolio, 0.92, &cfg);
        // Price is 1.00, above threshold 0.96 — no alert
        let alerts = analyze(&history, &portfolio, &risk, &cfg);
        assert!(!alerts.iter().any(|a| matches!(a.kind, AlertKind::PriceBelow { .. })));

        // Now drop below threshold
        let mut low_history = make_history(&[1.00_f64; 4], "USDY");
        let mut snap = low_history.back().unwrap().clone();
        snap.prices.insert("USDY".to_string(), 0.94);
        low_history.push_back(snap);
        let risk2 = compute_risk(&low_history, &portfolio, 0.92, &cfg);
        let alerts2 = analyze(&low_history, &portfolio, &risk2, &cfg);
        assert!(alerts2.iter().any(|a| matches!(a.kind, AlertKind::PriceBelow { threshold } if (threshold - 0.96).abs() < 1e-9)),
            "expected PriceBelow alert when price 0.94 < threshold 0.96");
    }

    #[test]
    fn test_implausible_1h_move_suppressed() {
        // A spoofed ghost pool briefly priced the token ~5000x too high, leaving a
        // poisoned baseline 60 snapshots back. The real current price is ~$0.22, so the
        // naive 1-hour change is -99.98% — a data error, not a market move. It must NOT
        // become a BigMove alert (this is the bug that emailed "JUP -99.98% in 1 hour").
        let mut prices = vec![0.22_f64; 62];
        prices[1] = 1100.0; // the 60-snapshot lookback lands here
        let history = make_history(&prices, "mint1");
        let cfg = make_cfg();
        let portfolio = Portfolio {
            sol_amount: 0.0,
            tokens: vec![TokenEntry { mint: "mint1".to_string(), symbol: "JUP".to_string(), amount: 4482.0 }],
        };
        let risk = compute_risk(&history, &portfolio, 0.92, &cfg);
        let alerts = analyze(&history, &portfolio, &risk, &cfg);
        assert!(!alerts.iter().any(|a| matches!(a.kind, AlertKind::BigMove1h { .. })),
            "a ~5000x outlier baseline must be rejected as bad data, not emailed as a -99.98% move");

        // Control: a genuine, plausible -50% move over the hour MUST still alert.
        let mut prices2 = vec![0.22_f64; 62];
        prices2[1] = 0.44; // baseline 0.44 -> current 0.22 = -50%
        let history2 = make_history(&prices2, "mint1");
        let risk2 = compute_risk(&history2, &portfolio, 0.92, &cfg);
        let alerts2 = analyze(&history2, &portfolio, &risk2, &cfg);
        assert!(alerts2.iter().any(|a| matches!(a.kind, AlertKind::BigMove1h { .. })),
            "a real -50% hourly move must still fire");
    }

    #[test]
    fn test_price_above_threshold_fires() {
        // TODO: mirror test_price_below_threshold_fires for the ceiling case.
        // 1. Build history where USDY trades at e.g. 1.00, set a ceiling of 1.04 in cfg.
        // 2. Assert NO PriceAbove alert fires (current price 1.00 < ceiling 1.04).
        // 3. Mutate the latest snapshot so USDY = 1.05, re-run analyze.
        // 4. Assert a PriceAbove alert fires with threshold ≈ 1.04.
    }

    #[test]
    fn test_no_swap_missing_asset_in_sma() {
        // Asset has a 7d signal but is not in the SMA map → skipped gracefully
        let alerts = vec![
            make_alert("NVDAx", AlertKind::New7dHigh { prev_high: 190.0 }, 200.0),
            make_alert("GOOGLx", AlertKind::New7dLow { prev_low: 350.0 }, 340.0),
        ];
        let mut risk = make_risk_with_asset("NVDAx", 200.0, 119.0);
        risk.assets.push(AssetRisk {
            symbol: "GOOGLx".to_string(),
            z_score: None, sigma_ann: None,
            current_drawdown_pct: 0.0, max_drawdown_pct: 0.0,
            current_value_eur: 126.0, drawdown_eur: 0.0,
            is_warm: true, n_obs: 100,
        });
        let mut sma = HashMap::new();
        sma.insert("NVDAx".to_string(), DailyBands { sma: 185.0, sigma: 1.0, n: 30 }); // only NVDAx in SMA, GOOGLx missing
        let suggestions = generate_swap_suggestions(&alerts, &sma, &risk);
        assert!(suggestions.is_empty(), "no buy candidate → no swap");
    }
}
