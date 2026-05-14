use std::collections::VecDeque;
use std::fmt;

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
}

impl fmt::Display for AlertKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AlertKind::BigMove5m { pct } => write!(f, "{:+.2}% in 5 minutes", pct),
            AlertKind::BigMove1h { pct } => write!(f, "{:+.2}% in 1 hour", pct),
            AlertKind::New7dHigh { .. } => write!(f, "new 7-day high"),
            AlertKind::New7dLow { .. } => write!(f, "new 7-day low"),
        }
    }
}

pub struct AnalysisConfig {
    pub alert_pct_5m: f64,
    pub alert_pct_1h: f64,
}

/// Analyse the price history for each asset in the portfolio and return triggered alerts.
pub fn analyze(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
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

        if !window_7d.is_empty() {
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
    }

    alerts
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
