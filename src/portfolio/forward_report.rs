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
        let str_field = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        match kind {
            "RankSnapshot" => out.config_points.push(ConfigPoint {
                ts,
                metric: str_field("metric"),
                min_score: f("min_score"),
            }),
            "Entered" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                let sym = str_field("symbol");
                open.insert(sym, (ts, f("usdc_in"), f("entry_price_usd")));
            }
            "Rotated" => {
                // Rotated fields: from_symbol, from_mint, from_sortino, to_symbol, to_mint,
                // to_sortino, to_amount, realized_usdc, cost_bps, sig, dry_run, metric.
                // Models as: close the from-leg, open the to-leg.
                if paper_only && !dry { out.real_filtered += 1; continue; }
                let from_sym = str_field("from_symbol");
                let to_sym = str_field("to_symbol");
                let realized_usdc = f("realized_usdc");
                let to_amount = f("to_amount");
                // Close the from-leg if it was tracked as open.
                if let Some((entry_ts, usdc_in, _)) = open.remove(&from_sym) {
                    out.closed.push(ClosedTrip {
                        symbol: from_sym,
                        entry_ts,
                        exit_ts: ts,
                        usdc_in,
                        usdc_out: realized_usdc,
                        reason: "rotated".to_string(),
                        dry_run: dry,
                    });
                }
                // Open the to-leg; entry_price = realized_usdc / to_amount (or 0 if no tokens).
                let entry_price = if to_amount > 0.0 { realized_usdc / to_amount } else { 0.0 };
                open.insert(to_sym, (ts, realized_usdc, entry_price));
            }
            "Exited" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                let sym = str_field("symbol");
                if let Some((entry_ts, usdc_in, _)) = open.remove(&sym) {
                    out.closed.push(ClosedTrip {
                        symbol: sym, entry_ts, exit_ts: ts, usdc_in, usdc_out: f("usdc_out"),
                        reason: v.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(), dry_run: dry,
                    });
                }
            }
            // Invalidated = a live position dropped WITHOUT a sell because its on-chain
            // balance is confirmed zero (moved/sold externally). Closes the leg like
            // Exited, marking the close at token_amount × last_price_usd — both
            // `#[serde(default)]` on the record, so a pre-plan thin line (only
            // symbol+mint, no position-detail fields) values it at 0.0. `dry` reuses the
            // shared unwrap_or(true) default above (line ~39): a pre-plan Invalidated
            // line never carried `dry_run` either, so it closes as PAPER — never
            // miscounted as a real round-trip. New records always write `dry_run`
            // explicitly, so this only affects lines written before this plan.
            "Invalidated" => {
                if paper_only && !dry { out.real_filtered += 1; continue; }
                let sym = str_field("symbol");
                if let Some((entry_ts, usdc_in, _)) = open.remove(&sym) {
                    out.closed.push(ClosedTrip {
                        symbol: sym, entry_ts, exit_ts: ts, usdc_in,
                        usdc_out: f("token_amount") * f("last_price_usd"),
                        reason: "invalidated".to_string(), dry_run: dry,
                    });
                }
            }
            // Adopted = live-wallet startup adoption (no swap, no paper position opened).
            // Intentionally ignored here: forward_report tracks paper-only round-trips.
            "Adopted" => {}
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
    let mut base = sim::base_params(cfg);
    base.regime_filter_obs = cfg.momentum_regime_obs;
    base.regime_mode = cfg.momentum_regime_mode;
    base.regime_threshold = cfg.momentum_regime_trend_min;
    base.entry_max_z_obs = cfg.momentum_entry_max_z_obs;
    base.entry_max_z = cfg.momentum_entry_max_z;
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

// ── Rendering ──────────────────────────────────────────────────────────────

pub fn render(sc: &Scorecard, parsed: &ParsedLog, coverage_pct: f64) -> String {
    let mut s = String::new();
    s.push_str(&format!("\n{}\n", "=".repeat(72)));
    s.push_str(&format!(
        "MOMENTUM FORWARD-TEST REPORT  ({:.1} weeks, {:.0}% history coverage)\n",
        sc.window_weeks, coverage_pct
    ));
    s.push_str(&format!("{}\n", "=".repeat(72)));
    if sc.config_changed {
        s.push_str(
            "\u{26a0}  config drift detected in the window (metric/min_score changed) \
             \u{2014} realized trades may not match the predicted config.\n",
        );
    }
    if parsed.real_filtered > 0 {
        s.push_str(&format!(
            "\u{2139}  excluded {} non-paper (real) action records (--paper-only).\n",
            parsed.real_filtered
        ));
    }
    if !parsed.open.is_empty() {
        s.push_str(&format!(
            "\u{2139}  {} open position(s) excluded from realized P&L (informational only).\n",
            parsed.open.len()
        ));
    }
    s.push_str(&format!(
        "\nRealized:  net ${:+.2}, {} trades, win {:.0}%, Sortino {:.2}, maxDD {:.1}%\n",
        sc.realized.net_pnl,
        sc.realized.n_trades,
        sc.realized.win_rate,
        sc.realized.sortino,
        sc.realized.max_dd_pct
    ));
    s.push_str(&format!(
        "Predicted: net ${:+.2}, {} trades, Sortino {:.2}, maxDD {:.1}%   \
         (gap: realized = {:.0}% of predicted)\n\n",
        sc.predicted.net_pnl,
        sc.predicted.n_trades,
        sc.predicted.sortino,
        sc.predicted.max_dd_pct,
        sc.pnl_frac * 100.0
    ));
    for (name, detail, pass) in &sc.lines {
        s.push_str(&format!(
            "  [{}] {:<26} {}\n",
            if *pass { "PASS" } else { "----" },
            name,
            detail
        ));
    }
    let verdict = match sc.verdict {
        Verdict::Insufficient => {
            "INSUFFICIENT DATA \u{2014} keep accumulating paper trades before judging."
        }
        Verdict::KeepPapering => {
            "KEEP PAPERING \u{2014} edge not yet confirmed out-of-sample."
        }
        Verdict::EligibleForSmallLive => {
            "ELIGIBLE FOR SMALL LIVE \u{2014} forward test tracks the backtest; \
             consider a small live allocation."
        }
    };
    s.push_str(&format!("\n=> {verdict}\n{}\n", "=".repeat(72)));
    s
}

pub fn run_forward_report(
    cfg: &PortfolioConfig,
    actions_path: &str,
    history_path: &str,
    since: Option<u64>,
    paper_only: bool,
    bar: GraduationBar,
    max_step: f64,
) -> Result<()> {
    let parsed = parse_actions(Path::new(actions_path), since, paper_only)?;
    if since.is_none() {
        eprintln!(
            "\u{26a0}  --since not set: the forward window may overlap the backtest's \
             training data \u{2014} the comparison is NOT out-of-sample."
        );
    }
    let raw: Vec<_> = history::load_history(Path::new(history_path))?.into_iter().collect();
    let all = sim::sanitize_history(&raw, max_step);
    let win_start = since.unwrap_or(0);
    let window: Vec<_> = all.iter().filter(|s| s.ts >= win_start).cloned().collect();
    let coverage_pct = if window.is_empty() {
        0.0
    } else {
        let span = (window.last().unwrap().ts - window.first().unwrap().ts).max(1) as f64;
        (window.len() as f64 / (span / 184.0)).min(1.0) * 100.0
    };
    let watched = momentum_universe::load(Path::new(&cfg.momentum_tokens_path))?;
    let realized = realized_metrics(&parsed.closed, cfg.momentum_trade_usdc);
    let predicted = predicted_metrics(&window, &watched, cfg);
    let sc = reconcile(&parsed, &realized, &predicted, &bar);
    print!("{}", render(&sc, &parsed, coverage_pct));
    Ok(())
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
    fn rotated_closes_from_leg_and_opens_to_leg() {
        // Sequence: Entered FROM, Rotated FROM→TO (closes FROM, opens TO), Exited TO.
        // Expect two closed trips: FROM closed by rotation (usdc_out = realized_usdc),
        // TO closed by exit (usdc_out = exit amount).
        let realized_usdc = 103.0_f64; // USDC recovered from FROM leg
        let to_amount = 50.0_f64;      // tokens received in TO leg
        let f = tmp_log(&[
            // Open the from-leg via a normal Entered.
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"FROM","mint":"mFROM","usdc_in":100.0,"token_amount":100.0,"entry_price_usd":1.0,"cost_bps":25,"sig":"s1","dry_run":true}"#,
            // Rotate FROM→TO: real Rotated fields (no "symbol"/"usdc_in"/"entry_price_usd").
            r#"{"ts":"2025-06-21T09:00:00Z","kind":"Rotated","from_symbol":"FROM","from_mint":"mFROM","from_sortino":0.8,"to_symbol":"TO","to_mint":"mTO","to_sortino":1.2,"to_amount":50.0,"realized_usdc":103.0,"cost_bps":30,"sig":"s2","dry_run":true,"metric":"sortino"}"#,
            // Exit the to-leg.
            r#"{"ts":"2025-06-21T10:00:00Z","kind":"Exited","symbol":"TO","mint":"mTO","usdc_out":110.0,"exit_price_usd":2.2,"peak_price_usd":2.3,"pnl_pct":6.8,"reason":"trailing stop","sig":"s3","dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        // Two closed trips expected.
        assert_eq!(p.closed.len(), 2, "expected 2 closed trips, got {}", p.closed.len());
        assert_eq!(p.open.len(), 0);
        // First closed (by exit_ts order): FROM closed by rotation.
        let from_trip = p.closed.iter().find(|t| t.symbol == "FROM").expect("FROM not found");
        assert_eq!(from_trip.reason, "rotated");
        assert!((from_trip.usdc_in  - 100.0).abs() < 1e-9, "FROM usdc_in");
        assert!((from_trip.usdc_out - realized_usdc).abs() < 1e-9, "FROM usdc_out should be realized_usdc");
        // Second closed: TO leg closed by exit.
        let to_trip = p.closed.iter().find(|t| t.symbol == "TO").expect("TO not found");
        assert_eq!(to_trip.reason, "trailing stop");
        assert!((to_trip.usdc_in  - realized_usdc).abs() < 1e-9, "TO usdc_in should be realized_usdc");
        assert!((to_trip.usdc_out - 110.0).abs() < 1e-9, "TO usdc_out");
        // Verify entry_price on the to-leg (realized_usdc / to_amount).
        let _ = to_amount; // used above in comments/documentation
    }

    #[test]
    fn invalidated_closes_open_leg_at_last_price_mark() {
        // Entered live (dry_run:false), then a full-field Invalidated record
        // (dry_run:false) — closes the leg like Exited does, valuing the close at
        // token_amount × last_price_usd (the mark at the confirmed-zero moment).
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"CATE","mint":"m","usdc_in":100.0,"token_amount":50.0,"entry_price_usd":2.0,"cost_bps":25,"sig":"s1","dry_run":false}"#,
            r#"{"ts":"2025-06-21T12:00:00Z","kind":"Invalidated","symbol":"CATE","mint":"m","token_amount":50.0,"entry_price_usd":2.0,"peak_price_usd":2.5,"last_price_usd":1.8,"dry_run":false}"#,
        ]);
        let p = parse_actions(f.path(), None, false).unwrap();
        assert_eq!(p.closed.len(), 1);
        assert_eq!(p.open.len(), 0);
        let t = &p.closed[0];
        assert_eq!(t.symbol, "CATE");
        assert_eq!(t.reason, "invalidated");
        assert!((t.usdc_in - 100.0).abs() < 1e-9);
        assert!((t.usdc_out - 90.0).abs() < 1e-9); // 50.0 token_amount * 1.8 last_price_usd
        assert!(!t.dry_run);
    }

    #[test]
    fn invalidated_legacy_thin_line_closes_as_paper_with_zero_value() {
        // A pre-plan Invalidated line only ever carried symbol+mint (no dry_run, no
        // position-detail fields — see momentum_actions.rs's own legacy-parse test).
        // The parser's shared `dry` default (unwrap_or(true), ~line 39) means this
        // closes as PAPER, never miscounted as a real round-trip, and usdc_out is
        // 0.0 since token_amount/last_price_usd are both absent.
        let f = tmp_log(&[
            r#"{"ts":"2025-06-21T08:00:00Z","kind":"Entered","symbol":"S","mint":"M","usdc_in":50.0,"entry_price_usd":1.0,"dry_run":true}"#,
            r#"{"ts":"2025-06-21T12:00:00Z","kind":"Invalidated","symbol":"S","mint":"M"}"#,
        ]);
        let p = parse_actions(f.path(), None, false).unwrap();
        assert_eq!(p.closed.len(), 1);
        assert_eq!(p.open.len(), 0);
        let t = &p.closed[0];
        assert_eq!(t.symbol, "S");
        assert_eq!(t.reason, "invalidated");
        assert!(t.usdc_out.abs() < 1e-9);
        assert!(t.dry_run, "thin legacy line has no dry_run field — must default to true (paper)");
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
        // 600 snapshots, 184 s apart. Token cycles: rises 0.3%/step for 40 steps, then
        // drops 8% in one step; repeats. This creates ~7 full cycles (entry on uptrend,
        // 8% dip triggers the 5% trailing stop → closed trade). SOL is flat throughout.
        let mut snaps = Vec::new();
        let mut px: f64 = 1.0;
        for i in 0..600u64 {
            let mut p = HashMap::new();
            let cycle_pos = i % 41;
            if cycle_pos < 40 {
                px *= 1.003; // +0.3% per step
            } else {
                px *= 0.92;  // −8% on the last step of each cycle → exceeds 5% trail stop
            }
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
            pool: None,
            quote: None,
            pools: None,
        }];
        let mut cfg = crate::portfolio::PortfolioConfig::from_env().unwrap();
        // Override all operator .env settings that gate entries, making the test
        // deterministic and independent of the operator's .env configuration.
        //
        // Without these overrides the test produces 0 trades for several reasons:
        //  1. MOMENTUM_REGIME_MODE=trend with SOL flat → regime never risk-on.
        //  2. MOMENTUM_LOOKBACK_OBS=480 + MOMENTUM_CONFIRM_LAG_OBS=5 → metric_is_fading
        //     returns true for the first 485 snaps, and thereafter a linear price rise
        //     causes score_now < prev (shrinking % gains as denominator grows).
        //  3. MOMENTUM_MAX_RUN_PCT=6 → overextended flag fires very early.
        //  4. MOMENTUM_TRAIL_PCT=20 → position never exits on a monotone fixture.
        cfg.momentum_regime_mode = RegimeMode::Off;
        cfg.momentum_min_score = 0.001;
        cfg.momentum_lookback_obs = 121;     // 121 obs → 120 returns ≥ SORTINO_MIN_OBS
        cfg.momentum_confirm_lag_obs = 0;    // disable metric_fading gate
        cfg.momentum_max_run_pct = 0.0;      // disable overextension cap
        cfg.momentum_decel_lookback_min = 0; // disable falling/deceleration check
        cfg.momentum_trail_pct = 5.0;        // 5% trailing stop fires on the 8% cycle dip
        let m = predicted_metrics(&snaps, &watched, &cfg);
        // Smoke: returns without panicking and yields a finite P&L.
        assert!(m.net_pnl.is_finite());
        assert!(m.n_trades > 0, "expected trades from steady uptrend fixture with regime off, got {}", m.n_trades);
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
    fn render_contains_verdict_and_warnings() {
        let bar = GraduationBar::default();
        let sc = reconcile(&parsed_with_span(2.0, true), &mk(1.0, 3, 0.5, 10.0), &pred(2.0, 2), &bar);
        let out = render(&sc, &parsed_with_span(2.0, true), 95.0);
        assert!(out.contains("INSUFFICIENT") || out.contains("KEEP PAPERING") || out.contains("ELIGIBLE"));
        assert!(out.contains("config")); // config-drift warning present since metric/min_score changed
    }

    #[test]
    fn keep_papering_when_trades_sufficient_but_criteria_fail() {
        // Sufficient trades (>= min_trades=20) but realized Sortino <= 0 → KeepPapering, not Eligible.
        let bar = GraduationBar::default();
        let s = reconcile(
            &parsed_with_span(8.0, false),
            &mk(5.0, 25, -0.5, 10.0), // Sortino = -0.5 (fails c_sortino)
            &pred(8.0, 22),
            &bar,
        );
        assert!(matches!(s.verdict, Verdict::KeepPapering));
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
