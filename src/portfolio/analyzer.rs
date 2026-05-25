use std::collections::{HashMap, VecDeque};
use std::fmt;

use serde::Serialize;

use super::history::PriceSnapshot;
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
        if let Some(old) = price_5m {
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
        if let Some(old) = price_1h {
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
    monthly_sma: &HashMap<String, f64>,
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
            AlertKind::New7dHigh { .. } if current_price > *sma => {
                sell_candidates.push((alert, asset.current_value_eur));
            }
            AlertKind::New7dLow { .. } if current_price < *sma => {
                buy_candidates.push((alert, asset.current_value_eur));
            }
            _ => {}
        }
    }

    let mut suggestions = Vec::new();
    for (sell_alert, sell_value_eur) in &sell_candidates {
        let sell_sma = monthly_sma[sell_alert.symbol.as_str()];
        for (buy_alert, buy_value_eur) in &buy_candidates {
            let buy_sma = monthly_sma[buy_alert.symbol.as_str()];
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

/// Native SOL mint address — used so the rebalancer can route SOL through
/// Jupiter alongside SPL token holdings without special-casing the price key.
pub const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Configuration for the stricter 30-day reversal signal used by the auto-rebalancer.
/// Kept separate from `AnalysisConfig` because the email-alert path (7-day) and the
/// execution path (30-day) are intentionally different — see docs/portfolio/auto-rebalance.md.
#[derive(Debug, Clone)]
pub struct RebalanceSignalConfig {
    pub lookback_days: u32,
    pub extreme_window_hours: u32,
    pub reversal_window_min: u32,
    pub reversal_pct: f64,
}

/// A fully-formed candidate trade: sell asset B (touched 30d high + declining),
/// buy asset A (touched 30d low + rising). One signal per matched pair.
#[derive(Debug, Clone)]
pub struct RebalanceSignal {
    pub sell_symbol: String,
    pub sell_mint: String,
    pub sell_price_usd: f64,
    pub sell_30d_high: f64,
    pub sell_hours_since_high: f64,
    pub sell_decline_pct: f64,
    pub buy_symbol: String,
    pub buy_mint: String,
    pub buy_price_usd: f64,
    pub buy_30d_low: f64,
    pub buy_hours_since_low: f64,
    pub buy_rise_pct: f64,
    /// Current sell-side holdings × current price × eur_rate. Drives the
    /// "largest economic position first" ordering used by the rebalancer.
    pub sell_value_eur: f64,
    /// Current buy-side holdings × current price × eur_rate. Used by the
    /// rebalancer's minimum-position gate so dust positions on either leg
    /// can't trigger a swap.
    pub buy_value_eur: f64,
}

#[derive(Debug, Clone, Copy)]
struct ExtremeInfo {
    extreme_price: f64,
    hours_since: f64,
}

/// Compute either the max or min of `key`'s price within the lookback window,
/// and how many hours ago the extreme was set. Returns `None` if there's no
/// data point within the window.
fn find_extreme(
    history: &VecDeque<PriceSnapshot>,
    key: &str,
    now_ts: u64,
    lookback_secs: u64,
    is_high: bool,
) -> Option<ExtremeInfo> {
    let cutoff = now_ts.saturating_sub(lookback_secs);
    let mut best: Option<(f64, u64)> = None;
    for snap in history.iter() {
        if snap.ts < cutoff {
            continue;
        }
        let Some(&p) = snap.prices.get(key) else { continue; };
        if p <= 0.0 { continue; }
        best = Some(match best {
            None => (p, snap.ts),
            Some((cur, _)) if (is_high && p > cur) || (!is_high && p < cur) => (p, snap.ts),
            Some(prev) => prev,
        });
    }
    best.map(|(extreme_price, ts)| ExtremeInfo {
        extreme_price,
        hours_since: (now_ts.saturating_sub(ts)) as f64 / 3600.0,
    })
}

/// Find the price closest to `target_ts` seconds. Returns `None` if no
/// snapshot has the key in the relevant time window.
fn price_at(history: &VecDeque<PriceSnapshot>, key: &str, target_ts: u64) -> Option<f64> {
    let mut best: Option<(u64, f64)> = None;
    for snap in history.iter() {
        let Some(&p) = snap.prices.get(key) else { continue; };
        if p <= 0.0 { continue; }
        let diff = snap.ts.abs_diff(target_ts);
        best = Some(match best {
            None => (diff, p),
            Some((d, _)) if diff < d => (diff, p),
            Some(prev) => prev,
        });
    }
    best.map(|(_, p)| p)
}

/// Identify executable rotation opportunities. Stricter than `generate_swap_suggestions`:
/// requires the extreme to have been touched within `extreme_window_hours` AND a
/// confirmed reversal of at least `reversal_pct` over `reversal_window_min`.
/// Returns paired (sell, buy) candidates sorted by sell-side EUR value descending.
pub fn generate_rebalance_signals(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    risk: &RiskReport,
    cfg: &RebalanceSignalConfig,
) -> Vec<RebalanceSignal> {
    let Some(latest) = history.back() else { return vec![]; };
    let now_ts = latest.ts;
    let lookback_secs = (cfg.lookback_days as u64) * 24 * 3600;
    let reversal_target_ts = now_ts.saturating_sub((cfg.reversal_window_min as u64) * 60);
    let extreme_window_secs = (cfg.extreme_window_hours as u64) * 3600;

    let asset_value: HashMap<String, f64> = risk.assets
        .iter()
        .map(|a| (a.symbol.clone(), a.current_value_eur))
        .collect();

    // Build (symbol, mint, current_price_usd) triples for every held asset.
    // SOL uses the well-known mint; the price key stays "SOL" because that's
    // how the pricer stores it.
    let mut assets: Vec<(String, String, f64)> = Vec::new();
    if let Some(&sol_px) = latest.prices.get("SOL") {
        if sol_px > 0.0 && portfolio.sol_amount > 0.0 {
            assets.push(("SOL".to_string(), SOL_MINT.to_string(), sol_px));
        }
    }
    for token in &portfolio.tokens {
        if token.amount <= 0.0 { continue; }
        let key = if latest.prices.contains_key(&token.mint) {
            &token.mint
        } else {
            &token.symbol
        };
        if let Some(&px) = latest.prices.get(key) {
            if px > 0.0 {
                assets.push((token.symbol.clone(), token.mint.clone(), px));
            }
        }
    }

    // Classify each asset into sell-candidate / buy-candidate / neither.
    let mut sells: Vec<(String, String, f64, ExtremeInfo, f64)> = Vec::new();
    let mut buys:  Vec<(String, String, f64, ExtremeInfo, f64)> = Vec::new();

    for (symbol, mint, current_price) in &assets {
        // Use the same key the pricer used to store the asset (SOL or mint).
        let price_key = if symbol == "SOL" { "SOL" } else { mint.as_str() };
        let alt_key   = symbol.as_str();

        let high = find_extreme(history, price_key, now_ts, lookback_secs, true)
            .or_else(|| find_extreme(history, alt_key, now_ts, lookback_secs, true));
        let low = find_extreme(history, price_key, now_ts, lookback_secs, false)
            .or_else(|| find_extreme(history, alt_key, now_ts, lookback_secs, false));
        let prior = price_at(history, price_key, reversal_target_ts)
            .or_else(|| price_at(history, alt_key, reversal_target_ts));

        let Some(prior_px) = prior else { continue };
        if prior_px <= 0.0 { continue; }

        // Sell side: at-or-near 30d high recently, now declining.
        if let Some(h) = high {
            if h.hours_since * 3600.0 <= extreme_window_secs as f64 {
                let decline_pct = (prior_px - current_price) / prior_px * 100.0;
                if decline_pct >= cfg.reversal_pct {
                    sells.push((symbol.clone(), mint.clone(), *current_price, h, decline_pct));
                }
            }
        }
        // Buy side: at-or-near 30d low recently, now rising.
        if let Some(l) = low {
            if l.hours_since * 3600.0 <= extreme_window_secs as f64 {
                let rise_pct = (current_price - prior_px) / prior_px * 100.0;
                if rise_pct >= cfg.reversal_pct {
                    buys.push((symbol.clone(), mint.clone(), *current_price, l, rise_pct));
                }
            }
        }
    }

    // Cartesian product, skipping (X, X) self-pairs, sorted by sell EUR value DESC.
    let mut signals: Vec<RebalanceSignal> = Vec::new();
    for (s_sym, s_mint, s_px, s_high, s_decline) in &sells {
        let sell_value_eur = asset_value.get(s_sym).copied().unwrap_or(0.0);
        if sell_value_eur <= 0.0 { continue; }
        for (b_sym, b_mint, b_px, b_low, b_rise) in &buys {
            if s_sym == b_sym { continue; }
            let buy_value_eur = asset_value.get(b_sym).copied().unwrap_or(0.0);
            signals.push(RebalanceSignal {
                sell_symbol: s_sym.clone(),
                sell_mint: s_mint.clone(),
                sell_price_usd: *s_px,
                sell_30d_high: s_high.extreme_price,
                sell_hours_since_high: s_high.hours_since,
                sell_decline_pct: *s_decline,
                buy_symbol: b_sym.clone(),
                buy_mint: b_mint.clone(),
                buy_price_usd: *b_px,
                buy_30d_low: b_low.extreme_price,
                buy_hours_since_low: b_low.hours_since,
                buy_rise_pct: *b_rise,
                sell_value_eur,
                buy_value_eur,
            });
        }
    }
    // Highest-stake trade first — minimises wasted cost-gate evaluations.
    signals.sort_by(|a, b| b.sell_value_eur.partial_cmp(&a.sell_value_eur).unwrap_or(std::cmp::Ordering::Equal));
    signals
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

#[cfg(test)]
mod tests {
    use super::*;
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
        sma.insert("NVDAx".to_string(), 185.0);  // price 200 > sma 185 → sell
        sma.insert("GOOGLx".to_string(), 360.0); // price 340 < sma 360 → buy

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
        sma.insert("NVDAx".to_string(), 185.0); // price 180 < sma 185 → NOT a sell
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
        sma.insert("GOOGLx".to_string(), 360.0); // price 370 > sma 360 → NOT a buy
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
    fn test_price_above_threshold_fires() {
        // TODO: mirror test_price_below_threshold_fires for the ceiling case.
        // 1. Build history where USDY trades at e.g. 1.00, set a ceiling of 1.04 in cfg.
        // 2. Assert NO PriceAbove alert fires (current price 1.00 < ceiling 1.04).
        // 3. Mutate the latest snapshot so USDY = 1.05, re-run analyze.
        // 4. Assert a PriceAbove alert fires with threshold ≈ 1.04.
    }

    fn rebalance_cfg() -> RebalanceSignalConfig {
        RebalanceSignalConfig {
            lookback_days: 30,
            extreme_window_hours: 24,
            reversal_window_min: 60,
            reversal_pct: 0.3,
        }
    }

    fn two_asset_portfolio() -> Portfolio {
        Portfolio {
            sol_amount: 0.0,
            tokens: vec![
                TokenEntry { mint: "MINT_A".to_string(), symbol: "AAA".to_string(), amount: 10.0 },
                TokenEntry { mint: "MINT_B".to_string(), symbol: "BBB".to_string(), amount: 10.0 },
            ],
        }
    }

    fn rebalance_risk(values: &[(&str, f64)]) -> RiskReport {
        let mut r = RiskReport::empty();
        for (sym, v) in values {
            r.assets.push(AssetRisk {
                symbol: (*sym).to_string(),
                z_score: None, sigma_ann: None,
                current_drawdown_pct: 0.0, max_drawdown_pct: 0.0,
                current_value_eur: *v, drawdown_eur: 0.0,
                is_warm: true, n_obs: 100,
            });
        }
        r
    }

    /// Build a 31-day price history at hourly cadence for two mints A and B.
    /// `a_pattern` and `b_pattern` receive (hours_ago, hours_total) and return a price.
    /// The most recent snapshot is at the end of the deque.
    fn make_dual_history(
        a_pattern: impl Fn(u64, u64) -> f64,
        b_pattern: impl Fn(u64, u64) -> f64,
    ) -> VecDeque<PriceSnapshot> {
        let total_hours: u64 = 31 * 24;
        let now: u64 = 1_700_000_000;
        let mut deque = VecDeque::new();
        for i in 0..total_hours {
            let ts = now - (total_hours - 1 - i) * 3600;
            let hours_ago = total_hours - 1 - i;
            let mut prices = HashMap::new();
            prices.insert("MINT_A".to_string(), a_pattern(hours_ago, total_hours));
            prices.insert("MINT_B".to_string(), b_pattern(hours_ago, total_hours));
            deque.push_back(PriceSnapshot { ts, prices });
        }
        deque
    }

    #[test]
    fn rebalance_signal_fires_on_low_plus_rise_and_high_plus_decline() {
        // A: hit low 6h ago at $50, now at $51 (1h ago was $50.5 → rise ~1%)
        // B: hit high 4h ago at $200, now at $198 (1h ago was $199.5 → decline ~0.75%)
        let history = make_dual_history(
            |h_ago, _| if h_ago == 6 { 50.0 } else if h_ago == 1 { 50.5 } else if h_ago == 0 { 51.0 } else { 60.0 },
            |h_ago, _| if h_ago == 4 { 200.0 } else if h_ago == 1 { 199.5 } else if h_ago == 0 { 198.0 } else { 180.0 },
        );
        let portfolio = two_asset_portfolio();
        let risk = rebalance_risk(&[("AAA", 510.0), ("BBB", 1980.0)]);
        let signals = generate_rebalance_signals(&history, &portfolio, &risk, &rebalance_cfg());
        assert_eq!(signals.len(), 1, "exactly one (B sell, A buy) pair expected");
        assert_eq!(signals[0].sell_symbol, "BBB");
        assert_eq!(signals[0].buy_symbol, "AAA");
        assert!(signals[0].sell_decline_pct >= 0.3);
        assert!(signals[0].buy_rise_pct >= 0.3);
        // The minimum-position gate in rebalancer.rs reads both legs — make sure
        // generate_rebalance_signals populates both with the values from the
        // risk report.
        assert!((signals[0].sell_value_eur - 1980.0).abs() < 1e-9);
        assert!((signals[0].buy_value_eur  -  510.0).abs() < 1e-9);
    }

    #[test]
    fn rebalance_signal_populates_buy_value_for_tiny_position() {
        // Same shape as the previous test but with a dust position on the buy
        // side. The signal is still emitted by the analyzer — the rebalancer's
        // min-position gate is what filters it out, so we just verify the data
        // is correctly threaded through here.
        let history = make_dual_history(
            |h_ago, _| if h_ago == 6 { 50.0 } else if h_ago == 1 { 50.5 } else if h_ago == 0 { 51.0 } else { 60.0 },
            |h_ago, _| if h_ago == 4 { 200.0 } else if h_ago == 1 { 199.5 } else if h_ago == 0 { 198.0 } else { 180.0 },
        );
        let portfolio = two_asset_portfolio();
        let risk = rebalance_risk(&[("AAA", 2.50), ("BBB", 1980.0)]);
        let signals = generate_rebalance_signals(&history, &portfolio, &risk, &rebalance_cfg());
        assert_eq!(signals.len(), 1);
        assert!(signals[0].buy_value_eur < 25.0, "buy side is dust");
        assert!(signals[0].sell_value_eur >= 25.0, "sell side is healthy");
    }

    #[test]
    fn rebalance_signal_skips_when_extreme_too_old() {
        // A hit its low 48h ago — outside the 24h extreme window
        let history = make_dual_history(
            |h_ago, _| if h_ago == 48 { 50.0 } else if h_ago == 1 { 99.5 } else if h_ago == 0 { 100.0 } else { 60.0 },
            |h_ago, _| if h_ago == 4 { 200.0 } else if h_ago == 1 { 199.5 } else if h_ago == 0 { 198.0 } else { 180.0 },
        );
        let portfolio = two_asset_portfolio();
        let risk = rebalance_risk(&[("AAA", 1000.0), ("BBB", 1980.0)]);
        let signals = generate_rebalance_signals(&history, &portfolio, &risk, &rebalance_cfg());
        assert!(signals.is_empty(), "A's low is 48h old, outside 24h window");
    }

    #[test]
    fn rebalance_signal_skips_when_no_reversal() {
        // A hit low 2h ago but price keeps falling (no uptick) — should skip
        let history = make_dual_history(
            |h_ago, _| if h_ago == 2 { 50.0 } else if h_ago == 1 { 49.5 } else if h_ago == 0 { 49.0 } else { 60.0 },
            |h_ago, _| if h_ago == 4 { 200.0 } else if h_ago == 1 { 199.5 } else if h_ago == 0 { 198.0 } else { 180.0 },
        );
        let portfolio = two_asset_portfolio();
        let risk = rebalance_risk(&[("AAA", 490.0), ("BBB", 1980.0)]);
        let signals = generate_rebalance_signals(&history, &portfolio, &risk, &rebalance_cfg());
        // A is still falling so it's not a buy candidate → no pair → empty
        assert!(signals.is_empty(), "A is still declining, not a buy candidate");
    }

    #[test]
    fn rebalance_signal_sorts_by_sell_value() {
        // Three assets: A and B are both sell candidates (touched high, declining);
        // C is the only buy candidate (touched low, rising). Two paired signals come
        // out (A→C and B→C); the one with the larger sell-side EUR value comes first.
        let total_hours: u64 = 31 * 24;
        let now: u64 = 1_700_000_000;
        let mut deque = VecDeque::new();
        for i in 0..total_hours {
            let ts = now - (total_hours - 1 - i) * 3600;
            let hours_ago = total_hours - 1 - i;
            let mut prices = HashMap::new();
            // A: high 5h ago at 100, now at 95 (decline relative to 1h-ago 99.5)
            prices.insert("MINT_A".to_string(),
                match hours_ago { 5 => 100.0, 1 => 99.5, 0 => 95.0, _ => 90.0 });
            // B: high 4h ago at 200, now at 190 (decline relative to 1h-ago 199.5)
            prices.insert("MINT_B".to_string(),
                match hours_ago { 4 => 200.0, 1 => 199.5, 0 => 190.0, _ => 180.0 });
            // C: low 3h ago at 30, now at 35 (rise relative to 1h-ago 30.5)
            prices.insert("MINT_C".to_string(),
                match hours_ago { 3 => 30.0, 1 => 30.5, 0 => 35.0, _ => 40.0 });
            deque.push_back(PriceSnapshot { ts, prices });
        }
        let portfolio = Portfolio {
            sol_amount: 0.0,
            tokens: vec![
                TokenEntry { mint: "MINT_A".into(), symbol: "AAA".into(), amount: 1.0 },
                TokenEntry { mint: "MINT_B".into(), symbol: "BBB".into(), amount: 1.0 },
                TokenEntry { mint: "MINT_C".into(), symbol: "CCC".into(), amount: 1.0 },
            ],
        };
        let risk = rebalance_risk(&[("AAA", 100.0), ("BBB", 999.0), ("CCC", 50.0)]);
        let signals = generate_rebalance_signals(&deque, &portfolio, &risk, &rebalance_cfg());
        assert!(!signals.is_empty(), "expected at least one (sell→buy) pair");
        // BBB has higher sell EUR value, so the first signal sells BBB.
        assert_eq!(signals[0].sell_symbol, "BBB");
        assert_eq!(signals[0].buy_symbol, "CCC");
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
        sma.insert("NVDAx".to_string(), 185.0); // only NVDAx in SMA, GOOGLx missing
        let suggestions = generate_swap_suggestions(&alerts, &sma, &risk);
        assert!(suggestions.is_empty(), "no buy candidate → no swap");
    }
}
