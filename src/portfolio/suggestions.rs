use std::collections::{HashMap, VecDeque};

use super::analyzer::RiskReport;
use super::history::PriceSnapshot;
use super::pricer::DailyBands;
use super::Portfolio;

// ── Public types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub action: String,
    pub signal_name: String,
    pub rationale: Vec<String>,
}

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Extract a clean price series for one asset from the full history deque.
/// Tokens are keyed by mint; SOL by the literal "SOL".
fn price_series(symbol: &str, portfolio: &Portfolio, history: &VecDeque<PriceSnapshot>) -> Vec<f64> {
    let key = if symbol == "SOL" {
        "SOL".to_string()
    } else {
        portfolio
            .tokens
            .iter()
            .find(|t| t.symbol == symbol)
            .map(|t| t.mint.clone())
            .unwrap_or_else(|| symbol.to_string())
    };
    history
        .iter()
        .filter_map(|snap| snap.prices.get(&key).copied())
        .filter(|&p| p > 0.0)
        .collect()
}

pub fn log_returns(prices: &[f64]) -> Vec<f64> {
    prices
        .windows(2)
        .filter_map(|w| {
            if w[0] > 0.0 {
                let r = (w[1] / w[0]).ln();
                if r.is_finite() { Some(r) } else { None }
            } else {
                None
            }
        })
        .collect()
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() { return 0.0; }
    v.iter().sum::<f64>() / v.len() as f64
}

fn std_dev(v: &[f64]) -> f64 {
    if v.len() < 2 { return 0.0; }
    let m = mean(v);
    (v.iter().map(|&x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt()
}

// ── 1. Pairs Divergence ───────────────────────────────────────────────────────
// Reference: Gatev, Goetzmann & Rouwenhorst (2006) "Pairs Trading: Performance
// of a Relative Value Arbitrage Rule", Journal of Finance.

const CANDIDATE_PAIRS: &[(&str, &str)] = &[
    ("AAPLx", "QQQx"),
    ("GOOGLx", "QQQx"),
    ("NVDAx", "QQQx"),
    ("TSLAx", "QQQx"),
    ("NVDAx", "SPYx"),
    ("AAPLx", "SPYx"),
    ("GOOGLx", "AAPLx"),
    ("JitoSOL", "SOL"),
];
const PAIRS_MIN_OBS: usize = 120;
const PAIRS_Z_THRESHOLD: f64 = 2.0;

pub fn generate_pairs_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
) -> Vec<Suggestion> {
    let portfolio_symbols: Vec<&str> = std::iter::once("SOL")
        .chain(portfolio.tokens.iter().map(|t| t.symbol.as_str()))
        .collect();

    let mut suggestions = Vec::new();

    for &(sym_a, sym_b) in CANDIDATE_PAIRS {
        if !portfolio_symbols.contains(&sym_a) || !portfolio_symbols.contains(&sym_b) {
            continue;
        }

        // Build aligned spread series: only ticks where both prices exist.
        let key_a = if sym_a == "SOL" {
            "SOL".to_string()
        } else {
            portfolio.tokens.iter().find(|t| t.symbol == sym_a)
                .map(|t| t.mint.clone()).unwrap_or_else(|| sym_a.to_string())
        };
        let key_b = if sym_b == "SOL" {
            "SOL".to_string()
        } else {
            portfolio.tokens.iter().find(|t| t.symbol == sym_b)
                .map(|t| t.mint.clone()).unwrap_or_else(|| sym_b.to_string())
        };

        let spreads: Vec<f64> = history
            .iter()
            .filter_map(|snap| {
                let a = snap.prices.get(&key_a).copied().filter(|&p| p > 0.0)?;
                let b = snap.prices.get(&key_b).copied().filter(|&p| p > 0.0)?;
                let s = (a / b).ln();
                if s.is_finite() { Some(s) } else { None }
            })
            .collect();

        if spreads.len() < PAIRS_MIN_OBS { continue; }

        let spread_mean = mean(&spreads);
        let spread_std = std_dev(&spreads);
        if spread_std < 1e-8 { continue; }

        let current = *spreads.last().unwrap();
        let z = (current - spread_mean) / spread_std;
        if z.abs() < PAIRS_Z_THRESHOLD { continue; }

        let (overperformer, underperformer) = if z > 0.0 { (sym_a, sym_b) } else { (sym_b, sym_a) };
        let pct = (current - spread_mean).abs() / spread_mean.abs().max(1e-8) * 100.0;

        suggestions.push(Suggestion {
            action: format!("SWAP {} FOR {}", overperformer, underperformer),
            signal_name: "Pairs Divergence".to_string(),
            rationale: vec![
                format!(
                    "{}/{} log-price spread: z={:+.2}σ — {} outperforming {} by {:.1}%",
                    sym_a, sym_b, z, overperformer, underperformer, pct
                ),
                format!(
                    "Spread deviates {:.1}σ from {}-sample rolling mean — reversion expected (Gatev et al., 2006)",
                    z.abs(),
                    spreads.len()
                ),
            ],
        });
    }

    suggestions
}

// ── 2. RSI Extremes + Trend Confirmation ─────────────────────────────────────
// Reference: Wilder (1978) "New Concepts in Technical Trading Systems".
// Validated empirically: Chong & Ng (2008), "Technical analysis and the
// London Stock Exchange", Applied Economics Letters.

const RSI_PERIOD: usize = 840; // 14 × 60 min = 14 "hourly" bars
const RSI_OVERSOLD: f64 = 30.0;
const RSI_OVERBOUGHT: f64 = 70.0;

fn compute_rsi(prices: &[f64]) -> Option<f64> {
    if prices.len() < RSI_PERIOD + 1 { return None; }
    let window = &prices[prices.len() - RSI_PERIOD - 1..];
    let (gains, losses): (Vec<f64>, Vec<f64>) = window
        .windows(2)
        .map(|w| {
            let d = w[1] - w[0];
            if d >= 0.0 { (d, 0.0) } else { (0.0, -d) }
        })
        .unzip();
    let avg_loss = mean(&losses);
    if avg_loss < 1e-12 { return Some(100.0); }
    let rs = mean(&gains) / avg_loss;
    Some(100.0 - 100.0 / (1.0 + rs))
}

pub fn generate_rsi_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    monthly_sma: &HashMap<String, DailyBands>,
) -> Vec<Suggestion> {
    let symbols: Vec<&str> = std::iter::once("SOL")
        .chain(portfolio.tokens.iter().map(|t| t.symbol.as_str()))
        .collect();

    let mut sell_cands: Vec<(String, f64, f64)> = vec![];
    let mut buy_cands: Vec<(String, f64, f64)> = vec![];

    for &sym in &symbols {
        let prices = price_series(sym, portfolio, history);
        let Some(rsi) = compute_rsi(&prices) else { continue; };
        let current = *prices.last().unwrap();
        let sma = monthly_sma.get(sym).map(|b| b.sma);

        if rsi > RSI_OVERBOUGHT && sma.is_none_or(|s| current > s) {
            sell_cands.push((sym.to_string(), rsi, current));
        } else if rsi < RSI_OVERSOLD && sma.is_none_or(|s| current < s) {
            buy_cands.push((sym.to_string(), rsi, current));
        }
    }

    let mut suggestions = Vec::new();
    for (sell_sym, sell_rsi, sell_price) in &sell_cands {
        for (buy_sym, buy_rsi, buy_price) in &buy_cands {
            let sell_sma = monthly_sma.get(sell_sym.as_str()).map(|b| b.sma).unwrap_or(*sell_price);
            let buy_sma = monthly_sma.get(buy_sym.as_str()).map(|b| b.sma).unwrap_or(*buy_price);
            suggestions.push(Suggestion {
                action: format!("SWAP {} FOR {}", sell_sym, buy_sym),
                signal_name: "RSI Extremes".to_string(),
                rationale: vec![
                    format!(
                        "{} RSI={:.0} (overbought >{}) — price €{:.2} vs 30d avg €{:.2}",
                        sell_sym, sell_rsi, RSI_OVERBOUGHT, sell_price, sell_sma
                    ),
                    format!(
                        "{} RSI={:.0} (oversold <{}) — price €{:.2} vs 30d avg €{:.2}",
                        buy_sym, buy_rsi, RSI_OVERSOLD, buy_price, buy_sma
                    ),
                    "Both RSI extreme and SMA deviation confirm the signal (Wilder, 1978)".to_string(),
                ],
            });
        }
    }
    suggestions
}

// ── 3. Sortino Ratio Rotation ─────────────────────────────────────────────────
// Reference: Sortino & Price (1994) "Performance Measurement in a Downside Risk
// Framework", Journal of Portfolio Management.

pub const SORTINO_MIN_OBS: usize = 120;
const SORTINO_MIN_DIFF: f64 = 0.5;

pub fn compute_sortino(returns: &[f64]) -> Option<f64> {
    if returns.len() < SORTINO_MIN_OBS { return None; }
    let m = mean(returns);
    let downside_var = mean(&returns.iter().map(|&r| r.min(0.0).powi(2)).collect::<Vec<_>>());
    // Floor prevents div-by-zero; zero downside is mathematically infinite Sortino.
    let downside_dev = downside_var.max(1e-12).sqrt();
    Some(m / downside_dev)
}

pub fn generate_sortino_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    risk: &RiskReport,
) -> Vec<Suggestion> {
    let mut ratios: Vec<(String, f64, f64)> = vec![]; // (symbol, sortino, value_eur)

    for token in &portfolio.tokens {
        let sym = &token.symbol;
        let returns = log_returns(&price_series(sym, portfolio, history));
        let Some(sortino) = compute_sortino(&returns) else { continue; };
        let value_eur = risk.assets.iter()
            .find(|a| &a.symbol == sym)
            .map_or(0.0, |a| a.current_value_eur);
        ratios.push((sym.clone(), sortino, value_eur));
    }

    if ratios.len() < 2 { return vec![]; }

    let worst = ratios.iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap().clone();
    let best  = ratios.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap().clone();

    if best.0 == worst.0 || best.1 - worst.1 < SORTINO_MIN_DIFF { return vec![]; }

    vec![Suggestion {
        action: format!("SWAP {} FOR {}", worst.0, best.0),
        signal_name: "Sortino Rotation".to_string(),
        rationale: vec![
            format!("{} Sortino={:.2} — worst downside-adjusted return (€{:.0} position)", worst.0, worst.1, worst.2),
            format!("{} Sortino={:.2} — best downside-adjusted return (€{:.0} position)", best.0, best.1, best.2),
            format!(
                "Difference: {:.2} — unlike Sharpe, Sortino only penalises losses (Sortino & Price, 1994)",
                best.1 - worst.1
            ),
        ],
    }]
}

// ── 4. Information Ratio vs SOL ───────────────────────────────────────────────
// Reference: Grinold (1989) "The Fundamental Law of Active Management",
// Journal of Portfolio Management.
// SOL is the natural benchmark for a Solana wallet — the opportunity cost of
// holding any xToken instead of native SOL.

const IR_MIN_OBS: usize = 120;
const IR_THRESHOLD: f64 = -0.3;

pub fn generate_ir_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    risk: &RiskReport,
) -> Vec<Suggestion> {
    let sol_returns = log_returns(&price_series("SOL", portfolio, history));
    if sol_returns.len() < IR_MIN_OBS { return vec![]; }

    let mut suggestions = Vec::new();

    for token in &portfolio.tokens {
        let sym = &token.symbol;
        if sym == "USDY" { continue; } // stablecoin — IR vs SOL is not meaningful

        let token_returns = log_returns(&price_series(sym, portfolio, history));
        if token_returns.len() < IR_MIN_OBS { continue; }

        let n = token_returns.len().min(sol_returns.len());
        let excess: Vec<f64> = token_returns[token_returns.len() - n..]
            .iter()
            .zip(sol_returns[sol_returns.len() - n..].iter())
            .map(|(&r_t, &r_s)| r_t - r_s)
            .collect();

        let ir_std = std_dev(&excess);
        if ir_std < 1e-12 { continue; }
        let ir = mean(&excess) / ir_std;

        if ir < IR_THRESHOLD {
            let value_eur = risk.assets.iter()
                .find(|a| &a.symbol == sym)
                .map_or(0.0, |a| a.current_value_eur);
            suggestions.push(Suggestion {
                action: format!("CONSIDER SWAPPING {} FOR JitoSOL", sym),
                signal_name: "Information Ratio vs SOL".to_string(),
                rationale: vec![
                    format!("{} IR={:.2} vs SOL (threshold: {:.2}) over {} observations", sym, ir, IR_THRESHOLD, n),
                    format!("{} underperforms SOL on risk-adjusted basis — opportunity cost is significant", sym),
                    "JitoSOL = SOL exposure + liquid staking yield as a higher-IR alternative (Grinold, 1989)".to_string(),
                    format!("Current position: €{:.0}", value_eur),
                ],
            });
        }
    }

    suggestions
}

// ── 5. Volatility Squeeze ─────────────────────────────────────────────────────
// Reference: Brenner & Galai (1989) volatility indices; Bollinger (2002)
// "Bollinger on Bollinger Bands". Volatility contracting well below its recent
// baseline precedes sharp directional moves in equity and crypto markets.

const SQUEEZE_MIN_HISTORY: usize = 1440; // 1 day baseline minimum
const SQUEEZE_RECENT_WINDOW: usize = 60;  // 1 hour recent vol
const SQUEEZE_RATIO_THRESHOLD: f64 = 0.5;

pub fn generate_vol_squeeze_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    risk: &RiskReport,
    monthly_sma: &HashMap<String, DailyBands>,
) -> Vec<Suggestion> {
    if history.len() < SQUEEZE_MIN_HISTORY { return vec![]; }

    let symbols: Vec<&str> = std::iter::once("SOL")
        .chain(portfolio.tokens.iter().map(|t| t.symbol.as_str()))
        .collect();

    let mut suggestions = Vec::new();

    for &sym in &symbols {
        let prices = price_series(sym, portfolio, history);
        if prices.len() < SQUEEZE_MIN_HISTORY + SQUEEZE_RECENT_WINDOW { continue; }

        let recent_returns = log_returns(&prices[prices.len() - SQUEEZE_RECENT_WINDOW - 1..]);
        let baseline_returns = log_returns(&prices[prices.len() - SQUEEZE_MIN_HISTORY - 1..]);

        if recent_returns.len() < 30 || baseline_returns.len() < 120 { continue; }

        let recent_std = std_dev(&recent_returns);
        let baseline_std = std_dev(&baseline_returns);
        if baseline_std < 1e-8 { continue; }

        let ratio = recent_std / baseline_std;
        if ratio >= SQUEEZE_RATIO_THRESHOLD { continue; }

        let current_price = *prices.last().unwrap();
        let sma = monthly_sma.get(sym).map(|b| b.sma);
        let (direction, relation) = match sma {
            Some(s) if current_price > s => ("bullish", "above"),
            Some(_) => ("bearish", "below"),
            None => ("neutral", "at"),
        };

        let recent_ann = recent_std * 525_600.0_f64.sqrt() * 100.0;
        let baseline_ann = baseline_std * 525_600.0_f64.sqrt() * 100.0;

        // Also grab sigma_ann from the risk report for cross-check
        let risk_ann = risk.assets.iter()
            .find(|a| a.symbol == sym)
            .and_then(|a| a.sigma_ann)
            .unwrap_or(0.0);

        suggestions.push(Suggestion {
            action: format!("WATCH {} — vol squeeze ({})", sym, direction),
            signal_name: "Volatility Squeeze".to_string(),
            rationale: vec![
                format!(
                    "1h vol: {:.1}%/yr  vs  24h baseline: {:.1}%/yr  (EWMA: {:.1}%/yr)  squeeze ratio={:.2}",
                    recent_ann, baseline_ann, risk_ann, ratio
                ),
                format!(
                    "Vol at {:.0}% of 24h average — compression before breakout (Bollinger, 2002)",
                    ratio * 100.0
                ),
                format!("Price {} 30d SMA → {} breakout bias", relation, direction),
            ],
        });
    }

    suggestions
}

// ── 6. Bollinger Band Reversion ───────────────────────────────────────────────
// Reference: Bollinger (2002) "Bollinger on Bollinger Bands". A price piercing
// the ±Kσ envelope around the 30-day moving average is statistically stretched;
// on a mean-reversion basis it is expected to revert toward the band.

const BOLLINGER_K: f64 = 2.0;          // matches the CLI chart's ±2σ envelope
const BOLLINGER_MIN_DAYS: usize = 14;  // ≥ ~half the 30d window populated, for a meaningful σ

pub fn generate_bollinger_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    bands: &HashMap<String, DailyBands>,
) -> Vec<Suggestion> {
    let symbols: Vec<&str> = std::iter::once("SOL")
        .chain(portfolio.tokens.iter().map(|t| t.symbol.as_str()))
        .collect();

    // (symbol, price, band_edge, pct_b)
    let mut sell_cands: Vec<(String, f64, f64, f64)> = vec![];
    let mut buy_cands: Vec<(String, f64, f64, f64)> = vec![];

    for &sym in &symbols {
        let Some(b) = bands.get(sym) else { continue; };
        if b.n < BOLLINGER_MIN_DAYS { continue; }

        let upper = b.sma + BOLLINGER_K * b.sigma;
        let lower = b.sma - BOLLINGER_K * b.sigma;
        let width = upper - lower;
        if width < 1e-9 { continue; } // flat series (e.g. USDY stablecoin)

        let prices = price_series(sym, portfolio, history);
        let Some(&price) = prices.last() else { continue; };
        let pct_b = (price - lower) / width;

        if price >= upper {
            sell_cands.push((sym.to_string(), price, upper, pct_b));
        } else if price <= lower {
            buy_cands.push((sym.to_string(), price, lower, pct_b));
        }
    }

    let mut suggestions = Vec::new();

    if !sell_cands.is_empty() && !buy_cands.is_empty() {
        // Both sides fired → pair them into SWAPs (cross product, like RSI).
        for (ss, sp, su, sb) in &sell_cands {
            for (bs, bp, bl, bb) in &buy_cands {
                suggestions.push(Suggestion {
                    action: format!("SWAP {} FOR {}", ss, bs),
                    signal_name: "Bollinger Reversion".to_string(),
                    rationale: vec![
                        format!(
                            "{} €{:.2} ≥ upper band €{:.2} (30d, 2σ) — %B={:.2}, stretched above envelope",
                            ss, sp, su, sb
                        ),
                        format!(
                            "{} €{:.2} ≤ lower band €{:.2} (30d, 2σ) — %B={:.2}, stretched below envelope",
                            bs, bp, bl, bb
                        ),
                        "Price outside ±2σ Bollinger band — mean reversion expected (Bollinger, 2002)".to_string(),
                    ],
                });
            }
        }
    } else {
        // Only one side fired → standalone WATCH so the pierce is never dropped.
        for (ss, sp, su, sb) in &sell_cands {
            suggestions.push(Suggestion {
                action: format!("WATCH {} — above upper band", ss),
                signal_name: "Bollinger Reversion".to_string(),
                rationale: vec![
                    format!("{} €{:.2} ≥ upper band €{:.2} (30d, 2σ) — %B={:.2}", ss, sp, su, sb),
                    "Price above +2σ Bollinger band, no buy candidate to pair — mean reversion expected (Bollinger, 2002)".to_string(),
                ],
            });
        }
        for (bs, bp, bl, bb) in &buy_cands {
            suggestions.push(Suggestion {
                action: format!("WATCH {} — below lower band", bs),
                signal_name: "Bollinger Reversion".to_string(),
                rationale: vec![
                    format!("{} €{:.2} ≤ lower band €{:.2} (30d, 2σ) — %B={:.2}", bs, bp, bl, bb),
                    "Price below −2σ Bollinger band, no sell candidate to pair — mean reversion expected (Bollinger, 2002)".to_string(),
                ],
            });
        }
    }

    suggestions
}

// ── Aggregator ────────────────────────────────────────────────────────────────

pub fn generate_all_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    risk: &RiskReport,
    monthly_sma: &HashMap<String, DailyBands>,
) -> Vec<Suggestion> {
    let mut all = Vec::new();
    all.extend(generate_pairs_suggestions(history, portfolio));
    all.extend(generate_rsi_suggestions(history, portfolio, monthly_sma));
    all.extend(generate_sortino_suggestions(history, portfolio, risk));
    all.extend(generate_ir_suggestions(history, portfolio, risk));
    all.extend(generate_vol_squeeze_suggestions(history, portfolio, risk, monthly_sma));
    all.extend(generate_bollinger_suggestions(history, portfolio, monthly_sma));
    all
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::analyzer::{AssetRisk, RiskReport};
    use crate::portfolio::history::PriceSnapshot;
    use crate::portfolio::{Portfolio, TokenEntry};
    use std::collections::{HashMap, VecDeque};

    fn make_snap(ts: u64, pairs: &[(&str, f64)]) -> PriceSnapshot {
        let mut prices = HashMap::new();
        for &(k, v) in pairs {
            prices.insert(k.to_string(), v);
        }
        PriceSnapshot { ts, prices }
    }

    fn single_token_portfolio(symbol: &str, mint: &str) -> Portfolio {
        Portfolio {
            sol_amount: 0.0,
            tokens: vec![TokenEntry {
                mint: mint.to_string(),
                symbol: symbol.to_string(),
                amount: 1.0,
            }],
        }
    }

    fn two_token_portfolio() -> Portfolio {
        Portfolio {
            sol_amount: 0.0,
            tokens: vec![
                TokenEntry { mint: "mintA".to_string(), symbol: "AAPLx".to_string(), amount: 1.0 },
                TokenEntry { mint: "mintB".to_string(), symbol: "QQQx".to_string(),  amount: 1.0 },
            ],
        }
    }

    fn empty_risk(symbols: &[&str]) -> RiskReport {
        RiskReport {
            assets: symbols.iter().map(|&s| AssetRisk {
                symbol: s.to_string(),
                z_score: None, sigma_ann: None,
                current_drawdown_pct: 0.0, max_drawdown_pct: 0.0,
                current_value_eur: 100.0, drawdown_eur: 0.0,
                is_warm: true, n_obs: 200,
            }).collect(),
            total_value_eur: 200.0,
            total_drawdown_eur: 0.0,
            portfolio_drawdown_pct: 0.0,
            portfolio_drawdown_eur: 0.0,
        }
    }

    #[test]
    fn test_rsi_oversold_fires() {
        // 841 prices: 840 steadily declining (RSI→0), then one tick
        let prices: Vec<f64> = (0..=840).map(|i| 100.0 - i as f64 * 0.05).collect();
        let portfolio = single_token_portfolio("TSLAx", "mintT");
        let mut history = VecDeque::new();
        for (i, &p) in prices.iter().enumerate() {
            history.push_back(make_snap(i as u64 * 60, &[("mintT", p)]));
        }
        let rsi = compute_rsi(&prices);
        assert!(rsi.is_some());
        assert!(rsi.unwrap() < RSI_OVERSOLD, "expected oversold RSI, got {:?}", rsi);

        let suggestions = generate_rsi_suggestions(&history, &portfolio, &HashMap::new());
        // No sell candidate so no pair, but buy candidate should exist internally
        // (no suggestions emitted without a sell partner — test internal RSI only)
        let _ = suggestions; // just verify no panic
    }

    #[test]
    fn test_rsi_overbought_fires() {
        let prices: Vec<f64> = (0..=840).map(|i| 100.0 + i as f64 * 0.05).collect();
        let rsi = compute_rsi(&prices);
        assert!(rsi.is_some());
        assert!(rsi.unwrap() > RSI_OVERBOUGHT, "expected overbought RSI, got {:?}", rsi);
    }

    #[test]
    fn test_sortino_suggestion_generated() {
        // AAPLx: all positive returns (high Sortino)
        // QQQx: all negative returns (low Sortino)
        let mut history = VecDeque::new();
        for i in 0..200usize {
            let aapl = 100.0 * (1.001_f64).powi(i as i32);
            let qqq  = 200.0 * (0.999_f64).powi(i as i32);
            history.push_back(make_snap(i as u64 * 60, &[("mintA", aapl), ("mintB", qqq)]));
        }
        let portfolio = two_token_portfolio();
        let risk = empty_risk(&["AAPLx", "QQQx"]);
        let suggestions = generate_sortino_suggestions(&history, &portfolio, &risk);
        assert!(!suggestions.is_empty(), "expected a Sortino rotation suggestion");
        assert!(suggestions[0].action.contains("QQQx"), "should sell QQQx (worst Sortino)");
        assert!(suggestions[0].action.contains("AAPLx"), "should buy AAPLx (best Sortino)");
    }

    #[test]
    fn test_sortino_no_suggestion_when_similar() {
        // Both assets with similar return profiles → no suggestion
        let mut history = VecDeque::new();
        for i in 0..200usize {
            let p = 100.0 + (i as f64 * 0.001).sin();
            history.push_back(make_snap(i as u64 * 60, &[("mintA", p), ("mintB", p * 1.01)]));
        }
        let portfolio = two_token_portfolio();
        let risk = empty_risk(&["AAPLx", "QQQx"]);
        let suggestions = generate_sortino_suggestions(&history, &portfolio, &risk);
        assert!(suggestions.is_empty(), "should not suggest swap when Sortino ratios are close");
    }

    #[test]
    fn test_pairs_divergence_fires() {
        // AAPLx doubles while QQQx stays flat → large positive spread z
        let portfolio = two_token_portfolio();
        let mut history = VecDeque::new();
        // 120 snapshots of normal co-movement
        for i in 0..120usize {
            history.push_back(make_snap(i as u64 * 60, &[("mintA", 100.0), ("mintB", 200.0)]));
        }
        // Final snapshot: AAPLx jumps 30% while QQQx unchanged
        history.push_back(make_snap(120 * 60, &[("mintA", 130.0), ("mintB", 200.0)]));

        let suggestions = generate_pairs_suggestions(&history, &portfolio);
        assert!(!suggestions.is_empty(), "expected pairs divergence suggestion");
        let action = &suggestions[0].action;
        assert!(action.contains("AAPLx"), "should sell overperformer AAPLx");
        assert!(action.contains("QQQx"),  "should buy underperformer QQQx");
    }

    #[test]
    fn test_vol_squeeze_fires() {
        // 1440 ticks of noisy baseline, then 60 ticks of flat (squeeze)
        let portfolio = Portfolio { sol_amount: 1.0, tokens: vec![] };
        let mut history = VecDeque::new();
        let mut rng = 0.5_f64;
        for i in 0..1440usize {
            rng = (rng * 1.6180339887).fract(); // deterministic pseudo-random
            let p = 100.0 + (rng - 0.5) * 4.0;  // ±2% noise
            history.push_back(make_snap(i as u64 * 60, &[("SOL", p)]));
        }
        // 60 flat ticks (near-zero vol)
        for i in 1440..1501usize {
            history.push_back(make_snap(i as u64 * 60, &[("SOL", 100.0)]));
        }
        let risk = empty_risk(&["SOL"]);
        let suggestions = generate_vol_squeeze_suggestions(&history, &portfolio, &risk, &HashMap::new());
        assert!(!suggestions.is_empty(), "expected vol squeeze suggestion for SOL");
        assert!(suggestions[0].signal_name == "Volatility Squeeze");
    }

    fn bands_map(entries: &[(&str, f64, f64, usize)]) -> HashMap<String, DailyBands> {
        let mut m = HashMap::new();
        for &(sym, sma, sigma, n) in entries {
            m.insert(sym.to_string(), DailyBands { sma, sigma, n });
        }
        m
    }

    #[test]
    fn test_bollinger_sell_and_buy_pair() {
        // AAPLx upper=110, lower=90; price 115 → SELL.
        // QQQx  upper=210, lower=190; price 185 → BUY.
        let portfolio = two_token_portfolio();
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintA", 115.0), ("mintB", 185.0)]));
        let bands = bands_map(&[("AAPLx", 100.0, 5.0, 30), ("QQQx", 200.0, 5.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert_eq!(s.len(), 1, "expected one SWAP suggestion");
        assert!(s[0].action.contains("SWAP AAPLx FOR QQQx"), "got {}", s[0].action);
        assert_eq!(s[0].signal_name, "Bollinger Reversion");
    }

    #[test]
    fn test_bollinger_standalone_watch_when_unpaired() {
        // Only AAPLx pierces (above upper); QQQx sits inside its band → WATCH, no SWAP.
        let portfolio = two_token_portfolio();
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintA", 115.0), ("mintB", 200.0)]));
        let bands = bands_map(&[("AAPLx", 100.0, 5.0, 30), ("QQQx", 200.0, 5.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert_eq!(s.len(), 1, "expected one standalone WATCH");
        assert!(s[0].action.contains("WATCH AAPLx — above upper band"), "got {}", s[0].action);
        assert!(!s[0].action.contains("SWAP"));
    }

    #[test]
    fn test_bollinger_standalone_watch_buy_side() {
        // Only QQQx pierces (below lower=190); AAPLx sits inside its band → WATCH, no SWAP.
        let portfolio = two_token_portfolio();
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintA", 100.0), ("mintB", 185.0)]));
        let bands = bands_map(&[("AAPLx", 100.0, 5.0, 30), ("QQQx", 200.0, 5.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert_eq!(s.len(), 1, "expected one standalone WATCH");
        assert!(s[0].action.contains("WATCH QQQx — below lower band"), "got {}", s[0].action);
        assert!(!s[0].action.contains("SWAP"));
    }

    #[test]
    fn test_bollinger_skips_flat_series() {
        // USDY-like: σ=0 → band width 0 → skipped (no %B blow-up).
        let portfolio = single_token_portfolio("USDY", "mintU");
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintU", 1.5)]));
        let bands = bands_map(&[("USDY", 1.0, 0.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert!(s.is_empty(), "flat series must produce no suggestion");
    }

    #[test]
    fn test_bollinger_skips_insufficient_days() {
        // n=5 < BOLLINGER_MIN_DAYS(14) → skipped even though price pierces.
        let portfolio = single_token_portfolio("NVDAx", "mintN");
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintN", 999.0)]));
        let bands = bands_map(&[("NVDAx", 100.0, 5.0, 5)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert!(s.is_empty(), "insufficient daily points must produce no suggestion");
    }

    #[test]
    fn test_aggregator_includes_bollinger() {
        // One asset pierces its upper band → the aggregate must surface a
        // Bollinger Reversion suggestion among its results.
        let portfolio = single_token_portfolio("NVDAx", "mintN");
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintN", 130.0)])); // above upper=110
        let bands = bands_map(&[("NVDAx", 100.0, 5.0, 30)]);
        let risk = empty_risk(&["NVDAx"]);

        let all = generate_all_suggestions(&history, &portfolio, &risk, &bands);
        assert!(
            all.iter().any(|s| s.signal_name == "Bollinger Reversion"),
            "aggregator must include the Bollinger engine"
        );
    }
}
