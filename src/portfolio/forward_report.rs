//! Read-only forward-test reconciliation for the paper momentum trader.
//! Parses momentum_actions.jsonl, computes realized metrics, replays the live
//! config over the same forward window for the prediction, and reconciles.
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;

pub const PERIODS_PER_YEAR: f64 = 365.0 * 86_400.0 / 184.0;

#[derive(Debug, Clone)]
pub struct ClosedTrip { pub symbol: String, pub entry_ts: u64, pub exit_ts: u64, pub usdc_in: f64, pub usdc_out: f64, pub reason: String, pub dry_run: bool }
#[derive(Debug, Clone)]
pub struct OpenPosition { pub symbol: String, pub entry_ts: u64, pub usdc_in: f64, pub entry_price_usd: f64 }
#[derive(Debug, Clone)]
pub struct ConfigPoint { pub ts: u64, pub metric: String, pub min_score: f64 }
#[derive(Debug, Clone, Default)]
pub struct ParsedLog { pub closed: Vec<ClosedTrip>, pub open: Vec<OpenPosition>, pub real_filtered: usize, pub config_points: Vec<ConfigPoint>, pub first_ts: Option<u64>, pub last_ts: Option<u64> }

#[derive(Debug, Clone)]
pub struct RealizedMetrics { pub net_pnl: f64, pub n_trades: usize, pub win_rate: f64, pub sortino: f64, pub max_dd_pct: f64 }

fn rfc3339(s: &str) -> Option<u64> {
    chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp() as u64)
}

pub fn parse_actions(path: &Path, since: Option<u64>, paper_only: bool) -> Result<ParsedLog> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    // symbol -> (entry_ts, usdc_in, entry_price)
    let mut open: HashMap<String, (u64, f64, f64)> = HashMap::new();
    let mut out = ParsedLog::default();
    for line in text.lines() {
        if line.trim().is_empty() { continue; }
        let v: Value = match serde_json::from_str(line) { Ok(v) => v, Err(_) => continue };
        let ts = match v.get("ts").and_then(|x| x.as_str()).and_then(rfc3339) { Some(t) => t, None => continue };
        out.first_ts = Some(out.first_ts.map_or(ts, |f| f.min(ts)));
        out.last_ts = Some(out.last_ts.map_or(ts, |f| f.max(ts)));
        if since.is_some_and(|s| ts < s) { continue; }
        let dry = v.get("dry_run").and_then(|x| x.as_bool()).unwrap_or(true);
        let kind = v.get("kind").and_then(|x| x.as_str()).unwrap_or("");
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        let sym = v.get("symbol").and_then(|x| x.as_str()).unwrap_or("").to_string();
        match kind {
            "RankSnapshot" => out.config_points.push(ConfigPoint {
                ts,
                metric: v.get("metric").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                min_score: f("min_score"),
            }),
            "Entered" | "Rotated" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                open.insert(sym, (ts, f("usdc_in"), f("entry_price_usd")));
            }
            "Exited" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                if let Some((entry_ts, usdc_in, _)) = open.remove(&sym) {
                    out.closed.push(ClosedTrip {
                        symbol: sym, entry_ts, exit_ts: ts, usdc_in, usdc_out: f("usdc_out"),
                        reason: v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(), dry_run: dry,
                    });
                }
            }
            _ => {}
        }
    }
    for (symbol, (entry_ts, usdc_in, entry_price_usd)) in open {
        out.open.push(OpenPosition { symbol, entry_ts, usdc_in, entry_price_usd });
    }
    out.closed.sort_by_key(|t| t.exit_ts);
    out.open.sort_by_key(|o| o.entry_ts);
    Ok(out)
}

pub fn realized_metrics(closed: &[ClosedTrip], start_pool: f64) -> RealizedMetrics {
    let n = closed.len();
    if n == 0 {
        return RealizedMetrics { net_pnl: 0.0, n_trades: 0, win_rate: 0.0, sortino: 0.0, max_dd_pct: 0.0 };
    }
    let mut cum = start_pool;
    let mut equity: Vec<(u64, f64)> = vec![(closed[0].entry_ts, start_pool)];
    let mut wins = 0usize;
    let mut net = 0.0;
    for t in closed {
        let pnl = t.usdc_out - t.usdc_in;
        net += pnl;
        cum += pnl;
        if pnl > 0.0 { wins += 1; }
        equity.push((t.exit_ts, cum));
    }
    // Per-trade annualization: scale by average trades/year over the realized span.
    let span_secs = closed.last().unwrap().exit_ts.saturating_sub(closed[0].entry_ts).max(1) as f64;
    let trades_per_year = (n as f64) * (365.0 * 86_400.0) / span_secs;
    let rm = crate::portfolio::sim::risk_metrics(&equity, trades_per_year.max(1.0));
    RealizedMetrics {
        net_pnl: net,
        n_trades: n,
        win_rate: 100.0 * wins as f64 / n as f64,
        sortino: rm.sortino,
        max_dd_pct: rm.true_max_dd_pct,
    }
}

// ── Predicted metrics ──────────────────────────────────────────────────────

use crate::portfolio::{history, momentum_universe, sim, PortfolioConfig, RegimeMode};
use crate::portfolio::sim::ParamSet;
use crate::portfolio::momentum::VolStopMode;

/// Build a `ParamSet` that mirrors exactly what the live momentum trader uses.
/// This is the crate-private equivalent of `momentum_sim::base_params`; kept
/// here so `forward_report` does not depend on the binary.
fn build_base_params(cfg: &PortfolioConfig) -> ParamSet {
    ParamSet {
        metric: cfg.momentum_rank_metric,
        min_metric: cfg.momentum_min_score,
        trail_pct: cfg.momentum_trail_pct,
        lookback_obs: cfg.momentum_lookback_obs,
        max_run_pct: cfg.momentum_max_run_pct,
        rotate_margin: cfg.momentum_rotate_margin,
        regime_filter_obs: 0,
        regime_mode: RegimeMode::Level,
        regime_threshold: 0.0,
        decel_lookback_min: cfg.momentum_decel_lookback_min,
        confirm_lag_obs: cfg.momentum_confirm_lag_obs,
        stale_minutes: cfg.momentum_stale_minutes,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc: cfg.momentum_trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
        exit_on_fade: cfg.momentum_exit_on_fade,
        vol_stop_mode: VolStopMode::Off,
        chandelier_k: 0.0,
        vol_obs: 0,
        overbought_z: 0.0,
        entry_dip_obs: 0,
        entry_dip_z: 0.0,
        dip_confirm_obs: 0,
        optimistic_fill: false,
        max_hold_min: 0,
        breakeven_exit: false,
        max_trail_pct: 0.0,
        reinvest_frac: 0.0,
        size_ceiling_usdc: cfg.momentum_trade_usdc,
    }
}

#[derive(Debug, Clone)]
pub struct PredictedMetrics {
    pub net_pnl: f64,
    pub n_trades: usize,
    pub sortino: f64,
    pub max_dd_pct: f64,
}

/// Replay the live `.env` momentum config over `window` to predict what the
/// backtest expects. Mirrors the live trader's regime gate exactly.
pub fn predicted_metrics(
    window: &[history::PriceSnapshot],
    watched: &[momentum_universe::WatchedToken],
    cfg: &PortfolioConfig,
) -> PredictedMetrics {
    if window.len() < 2 || watched.is_empty() {
        return PredictedMetrics { net_pnl: 0.0, n_trades: 0, sortino: 0.0, max_dd_pct: 0.0 };
    }
    let mut base = build_base_params(cfg);
    base.regime_filter_obs = cfg.momentum_regime_obs;
    base.regime_mode = cfg.momentum_regime_mode;
    base.regime_threshold = cfg.momentum_regime_trend_min;
    let stream = sim::ranked_stream(window, watched, &base);
    let regime: Vec<bool> = match base.regime_mode {
        RegimeMode::Off => vec![true; window.len()],
        RegimeMode::Level => sim::regime_mask(window, base.regime_filter_obs),
        RegimeMode::Trend => sim::regime_mask_trend(window, base.regime_filter_obs, base.regime_threshold),
    };
    let (run, equity) = sim::replay_multi_mtm(window, watched, &stream, &base, &regime, cfg.momentum_max_positions);
    let rm = sim::risk_metrics(&equity, PERIODS_PER_YEAR);
    PredictedMetrics {
        net_pnl: run.net_pnl(),
        n_trades: run.n_trades(),
        sortino: rm.sortino,
        max_dd_pct: rm.true_max_dd_pct,
    }
}

// ── Graduation scorecard ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct GraduationBar {
    pub min_weeks: f64,
    pub min_trades: usize,
    pub min_pnl_frac: f64,
    pub max_dd_pct: f64,
}

impl Default for GraduationBar {
    fn default() -> Self {
        GraduationBar {
            min_weeks: 6.0,
            min_trades: 20,
            min_pnl_frac: 0.6,
            max_dd_pct: 50.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    Insufficient,
    KeepPapering,
    EligibleForSmallLive,
}

#[derive(Debug, Clone)]
pub struct Scorecard {
    pub verdict: Verdict,
    pub window_weeks: f64,
    pub realized: RealizedMetrics,
    pub predicted: PredictedMetrics,
    pub pnl_frac: f64,
    pub config_changed: bool,
    pub lines: Vec<(String, String, bool)>,
}

pub fn reconcile(
    parsed: &ParsedLog,
    realized: &RealizedMetrics,
    predicted: &PredictedMetrics,
    bar: &GraduationBar,
) -> Scorecard {
    let window_secs = parsed.last_ts.unwrap_or(0).saturating_sub(parsed.first_ts.unwrap_or(0)) as f64;
    let window_weeks = window_secs / (7.0 * 86_400.0);
    let pnl_frac = if predicted.net_pnl > 0.0 {
        realized.net_pnl / predicted.net_pnl
    } else if realized.net_pnl >= 0.0 {
        1.0
    } else {
        0.0
    };
    let config_changed = parsed
        .config_points
        .windows(2)
        .any(|w| w[0].metric != w[1].metric || (w[0].min_score - w[1].min_score).abs() > 1e-9);

    let mut lines = Vec::new();
    let c_window = window_weeks >= bar.min_weeks;
    lines.push((
        "Forward window".into(),
        format!("{:.1}w ≥ {:.0}w", window_weeks, bar.min_weeks),
        c_window,
    ));
    let c_trades = realized.n_trades >= bar.min_trades;
    lines.push((
        "Closed trades".into(),
        format!("{} ≥ {}", realized.n_trades, bar.min_trades),
        c_trades,
    ));
    let c_sortino = realized.sortino > 0.0;
    lines.push((
        "Realized Sortino".into(),
        format!("{:.2} > 0", realized.sortino),
        c_sortino,
    ));
    let c_pnl = pnl_frac >= bar.min_pnl_frac;
    lines.push((
        "Realized vs predicted P&L".into(),
        format!("{:.0}% ≥ {:.0}%", pnl_frac * 100.0, bar.min_pnl_frac * 100.0),
        c_pnl,
    ));
    let c_dd = realized.max_dd_pct <= bar.max_dd_pct;
    lines.push((
        "Max drawdown".into(),
        format!("{:.1}% ≤ {:.0}%", realized.max_dd_pct, bar.max_dd_pct),
        c_dd,
    ));

    // Too few trades is a hard short-circuit: never emit a PASS/FAIL verdict on a tiny sample.
    let verdict = if realized.n_trades < bar.min_trades {
        Verdict::Insufficient
    } else if c_window && c_sortino && c_pnl && c_dd {
        Verdict::EligibleForSmallLive
    } else {
        Verdict::KeepPapering
    };

    Scorecard {
        verdict,
        window_weeks,
        realized: realized.clone(),
        predicted: predicted.clone(),
        pnl_frac,
        config_changed,
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_log(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines { writeln!(f, "{l}").unwrap(); }
        f
    }

    #[test]
    fn parses_one_closed_round_trip() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"BP","mint":"m","usdc_in":100.0,"token_amount":1.0,"entry_price_usd":100.0,"cost_bps":25,"sig":"s","dry_run":true}"#,
            r#"{"ts":"2025-06-21T12:00:00Z","kind":"Exited","symbol":"BP","mint":"m","usdc_out":105.0,"exit_price_usd":105.0,"peak_price_usd":106.0,"pnl_pct":5.0,"reason":"trailing stop","sig":"s","dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 1);
        assert_eq!(p.open.len(), 0);
        let t = &p.closed[0];
        assert_eq!(t.symbol, "BP");
        assert!((t.usdc_out - 105.0).abs() < 1e-9);
        assert_eq!(t.entry_ts, 1750492800); // 2025-06-21T08:00:00Z
    }

    #[test]
    fn unmatched_entry_is_open_not_closed() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"MET","usdc_in":100.0,"entry_price_usd":0.16,"dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 0);
        assert_eq!(p.open.len(), 1);
        assert_eq!(p.open[0].symbol, "MET");
    }

    #[test]
    fn paper_only_filters_real_trades() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"BP","usdc_in":100.0,"entry_price_usd":1.0,"dry_run":false}"#,
            r#"{"ts":"2025-06-21T12:00:00Z","kind":"Exited","symbol":"BP","usdc_out":110.0,"reason":"x","dry_run":false}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 0);
        assert_eq!(p.real_filtered, 2);
    }

    #[test]
    fn since_excludes_earlier_events() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-20T08:00:00Z","kind":"Entered","symbol":"BP","usdc_in":100.0,"entry_price_usd":1.0,"dry_run":true}"#,
            r#"{"ts":"2025-06-20T12:00:00Z","kind":"Exited","symbol":"BP","usdc_out":110.0,"reason":"x","dry_run":true}"#,
        ]);
        let since = chrono::DateTime::parse_from_rfc3339("2025-06-21T00:00:00Z").unwrap().timestamp() as u64;
        let p = parse_actions(f.path(), Some(since), true).unwrap();
        assert_eq!(p.closed.len(), 0);
        // Verify first_ts/last_ts track ALL lines, including those before --since cutoff
        let first_ts_expected = chrono::DateTime::parse_from_rfc3339("2025-06-20T08:00:00Z").unwrap().timestamp() as u64;
        let last_ts_expected = chrono::DateTime::parse_from_rfc3339("2025-06-20T12:00:00Z").unwrap().timestamp() as u64;
        assert_eq!(p.first_ts, Some(first_ts_expected));
        assert_eq!(p.last_ts, Some(last_ts_expected));
    }

    #[test]
    fn rotated_is_treated_as_entry_and_closes() {
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Rotated","symbol":"ZZ","usdc_in":100.0,"entry_price_usd":2.0,"dry_run":true}"#,
            r#"{"ts":"2025-06-21T10:00:00Z","kind":"Exited","symbol":"ZZ","usdc_out":107.0,"reason":"x","dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 1);
        assert_eq!(p.closed[0].symbol, "ZZ");
        assert!((p.closed[0].usdc_out - 107.0).abs() < 1e-9);
    }

    #[test]
    fn realized_metrics_basic() {
        let closed = vec![
            ClosedTrip { symbol:"A".into(), entry_ts:0, exit_ts:100, usdc_in:100.0, usdc_out:105.0, reason:"x".into(), dry_run:true },
            ClosedTrip { symbol:"B".into(), entry_ts:200, exit_ts:300, usdc_in:100.0, usdc_out:98.0, reason:"x".into(), dry_run:true },
        ];
        let m = realized_metrics(&closed, 100.0);
        assert_eq!(m.n_trades, 2);
        assert!((m.net_pnl - 3.0).abs() < 1e-9);     // +5 -2
        assert!((m.win_rate - 50.0).abs() < 1e-9);
        assert!(m.max_dd_pct > 0.0 && m.max_dd_pct.is_finite());
        assert!(m.sortino.is_finite());
    }

    #[test]
    fn realized_metrics_empty_is_zero() {
        let m = realized_metrics(&[], 100.0);
        assert_eq!(m.n_trades, 0);
        assert!(m.net_pnl.abs() < 1e-9 && m.win_rate == 0.0 && m.sortino == 0.0 && m.max_dd_pct == 0.0);
    }

    #[test]
    fn predicted_runs_over_window() {
        use crate::portfolio::history::PriceSnapshot;
        use crate::portfolio::momentum_universe::WatchedToken;
        use std::collections::HashMap;
        // 600 snapshots, 184 s apart: token rising steadily, SOL flat.
        // regime_mode defaults to Level with regime_obs=0 → filter off → entries are allowed.
        let mut snaps = Vec::new();
        for i in 0..600u64 {
            let mut p = HashMap::new();
            let px = 1.0 + (i as f64) * 0.002; // steady uptrend → momentum fires
            p.insert("MINT".to_string(), px);
            p.insert("SOL".to_string(), 100.0);
            snaps.push(PriceSnapshot { ts: 1_750_000_000 + i * 184, prices: p });
        }
        let watched = vec![WatchedToken {
            symbol: "TOK".into(),
            mint: "MINT".into(),
            name: None,
            equity: None,
            params: None,
        }];
        let cfg = crate::portfolio::PortfolioConfig::from_env().unwrap();
        let m = predicted_metrics(&snaps, &watched, &cfg);
        // Smoke: returns without panicking and yields a finite P&L.
        assert!(m.net_pnl.is_finite());
        assert!(m.n_trades > 0, "expected trades from steady uptrend fixture, got 0");
    }

    // ── Scorecard tests ────────────────────────────────────────────────────

    fn mk(net: f64, n: usize, sortino: f64, dd: f64) -> RealizedMetrics {
        RealizedMetrics {
            net_pnl: net,
            n_trades: n,
            win_rate: 60.0,
            sortino,
            max_dd_pct: dd,
        }
    }

    fn pred(net: f64, n: usize) -> PredictedMetrics {
        PredictedMetrics {
            net_pnl: net,
            n_trades: n,
            sortino: 1.0,
            max_dd_pct: 10.0,
        }
    }

    fn parsed_with_span(weeks: f64, metric_changes: bool) -> ParsedLog {
        let start = 1_750_000_000u64;
        let end = start + (weeks * 7.0 * 86_400.0) as u64;
        let cps = if metric_changes {
            vec![
                ConfigPoint {
                    ts: start,
                    metric: "return".into(),
                    min_score: 0.05,
                },
                ConfigPoint {
                    ts: end,
                    metric: "return".into(),
                    min_score: 0.09,
                },
            ]
        } else {
            vec![
                ConfigPoint {
                    ts: start,
                    metric: "return".into(),
                    min_score: 0.05,
                },
                ConfigPoint {
                    ts: end,
                    metric: "return".into(),
                    min_score: 0.05,
                },
            ]
        };
        ParsedLog {
            first_ts: Some(start),
            last_ts: Some(end),
            config_points: cps,
            ..Default::default()
        }
    }

    #[test]
    fn insufficient_when_too_few_trades() {
        let bar = GraduationBar::default();
        let s = reconcile(&parsed_with_span(8.0, false), &mk(5.0, 3, 1.0, 10.0), &pred(8.0, 4), &bar);
        assert!(matches!(s.verdict, Verdict::Insufficient));
    }

    #[test]
    fn eligible_when_all_pass() {
        let bar = GraduationBar {
            min_weeks: 6.0,
            min_trades: 20,
            min_pnl_frac: 0.6,
            max_dd_pct: 50.0,
        };
        // realized 25 trades, +$12 vs predicted +$15 (80% ≥ 60%), Sortino>0, DD 20%<50%, 8 weeks.
        let s = reconcile(
            &parsed_with_span(8.0, false),
            &mk(12.0, 25, 1.4, 20.0),
            &pred(15.0, 22),
            &bar,
        );
        assert!(matches!(s.verdict, Verdict::EligibleForSmallLive));
    }
}
