# Momentum Forward-Report Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a read-only `momentum-sim forward-report` subcommand that reconciles realized paper-trading performance against the backtest's prediction over the same out-of-sample window, and prints a pre-committed graduation scorecard.

**Architecture:** New module `src/portfolio/forward_report.rs` holds all logic: parse the paper action log into closed round-trips, compute realized metrics, replay the live `.env` config over the forward window for the predicted metrics, reconcile, and render. A thin `ForwardReport` command variant in `src/bin/momentum_sim.rs` wires CLI args to the module. Reuses existing helpers (`history::load_history`, `sim::sanitize_history`, `sim::base_params`, `sim::ranked_stream`, `sim::replay_multi_mtm`, `sim::risk_metrics`). Touches nothing in `src/arbitrage/`, `src/graph/`, the `solana-mev` binary, or the live trader's decision code.

**Tech Stack:** Rust, clap (CLI), serde_json (action-log parsing), chrono 0.4 (RFC3339 timestamps — already a dependency).

## Global Constraints

- Additive only: no edits to `src/arbitrage/`, `src/graph/`, `src/bin/solana_mev.rs`, or the live momentum trader's decision logic (`src/portfolio/momentum.rs` entry/exit).
- Read-only at runtime: the subcommand never writes `momentum_actions.jsonl`, `price_history.jsonl`, or any state file.
- Tests live in a `#[cfg(test)]` block at the bottom of the source file (repo convention). Run with `cargo test --bin momentum-sim` / `cargo test --lib forward_report`.
- Do NOT run `cargo fmt` or `rustfmt` on whole files (repo is not rustfmt-clean — causes diff churn).
- Snapshot cadence constant: history snapshots are ~184 s apart; annualization uses `PERIODS_PER_YEAR = 365.0 * 86_400.0 / 184.0`.
- Timestamps: action-log `ts` is an RFC3339 string (`"2026-06-21T08:17:50Z"`); price-history `ts` is `u64` unix seconds. Convert log timestamps with `chrono::DateTime::parse_from_rfc3339(s)?.timestamp() as u64`.

---

## File Structure

- **Create** `src/portfolio/forward_report.rs` — parsing, realized metrics, predicted replay, reconciliation, rendering, and all unit tests.
- **Modify** `src/portfolio/mod.rs` — add `pub mod forward_report;` and re-export the entry function.
- **Modify** `src/bin/momentum_sim.rs` — add the `ForwardReport` command variant + a `forward_report_cmd(...)` handler that calls into the module.
- **Modify** `src/portfolio/sim.rs` — change `regime_mask` and `regime_mask_trend` from private to `pub(crate)` so the new module can build the regime mask (one-word visibility change each; no logic change).

---

### Task 1: Action-log parser → typed events + closed round-trips

**Files:**
- Create: `src/portfolio/forward_report.rs`
- Modify: `src/portfolio/mod.rs` (add `pub mod forward_report;`)
- Test: in `src/portfolio/forward_report.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: nothing (entry point for parsing).
- Produces:
  - `pub struct ClosedTrip { pub symbol: String, pub entry_ts: u64, pub exit_ts: u64, pub usdc_in: f64, pub usdc_out: f64, pub reason: String, pub dry_run: bool }`
  - `pub struct OpenPosition { pub symbol: String, pub entry_ts: u64, pub usdc_in: f64, pub entry_price_usd: f64 }`
  - `pub struct ConfigPoint { pub ts: u64, pub metric: String, pub min_score: f64 }`
  - `pub struct ParsedLog { pub closed: Vec<ClosedTrip>, pub open: Vec<OpenPosition>, pub real_filtered: usize, pub config_points: Vec<ConfigPoint>, pub first_ts: Option<u64>, pub last_ts: Option<u64> }`
  - `pub fn parse_actions(path: &std::path::Path, since: Option<u64>, paper_only: bool) -> anyhow::Result<ParsedLog>`

- [ ] **Step 1: Write the failing test**

```rust
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
            r#"{"ts":"2026-06-21T08:00:00Z","kind":"Entered","symbol":"BP","mint":"m","usdc_in":100.0,"token_amount":1.0,"entry_price_usd":100.0,"cost_bps":25,"sig":"s","dry_run":true}"#,
            r#"{"ts":"2026-06-21T12:00:00Z","kind":"Exited","symbol":"BP","mint":"m","usdc_out":105.0,"exit_price_usd":105.0,"peak_price_usd":106.0,"pnl_pct":5.0,"reason":"trailing stop","sig":"s","dry_run":true}"#,
        ]);
        let p = parse_actions(f.path(), None, true).unwrap();
        assert_eq!(p.closed.len(), 1);
        assert_eq!(p.open.len(), 0);
        let t = &p.closed[0];
        assert_eq!(t.symbol, "BP");
        assert!((t.usdc_out - 105.0).abs() < 1e-9);
        assert_eq!(t.entry_ts, 1750492800); // 2026-06-21T08:00:00Z
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib forward_report::tests::parses_one_closed_round_trip`
Expected: FAIL — `parse_actions` / module not found.

- [ ] **Step 3: Add `tempfile` dev-dependency if absent and register the module**

In `Cargo.toml` under `[dev-dependencies]` add (skip if already present): `tempfile = "3"`.
In `src/portfolio/mod.rs` add: `pub mod forward_report;`

- [ ] **Step 4: Write the parser**

```rust
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
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib forward_report::tests::parses_one_closed_round_trip`
Expected: PASS.

- [ ] **Step 6: Add tests for open positions, dry_run filtering, and `--since`**

```rust
#[test]
fn unmatched_entry_is_open_not_closed() {
    let f = tmp_log(&[
        r#"{"ts":"2026-06-21T08:00:00Z","kind":"Entered","symbol":"MET","usdc_in":100.0,"entry_price_usd":0.16,"dry_run":true}"#,
    ]);
    let p = parse_actions(f.path(), None, true).unwrap();
    assert_eq!(p.closed.len(), 0);
    assert_eq!(p.open.len(), 1);
    assert_eq!(p.open[0].symbol, "MET");
}

#[test]
fn paper_only_filters_real_trades() {
    let f = tmp_log(&[
        r#"{"ts":"2026-06-21T08:00:00Z","kind":"Entered","symbol":"BP","usdc_in":100.0,"entry_price_usd":1.0,"dry_run":false}"#,
        r#"{"ts":"2026-06-21T12:00:00Z","kind":"Exited","symbol":"BP","usdc_out":110.0,"reason":"x","dry_run":false}"#,
    ]);
    let p = parse_actions(f.path(), None, true).unwrap();
    assert_eq!(p.closed.len(), 0);
    assert_eq!(p.real_filtered, 2);
}

#[test]
fn since_excludes_earlier_events() {
    let f = tmp_log(&[
        r#"{"ts":"2026-06-20T08:00:00Z","kind":"Entered","symbol":"BP","usdc_in":100.0,"entry_price_usd":1.0,"dry_run":true}"#,
        r#"{"ts":"2026-06-20T12:00:00Z","kind":"Exited","symbol":"BP","usdc_out":110.0,"reason":"x","dry_run":true}"#,
    ]);
    let since = chrono::DateTime::parse_from_rfc3339("2026-06-21T00:00:00Z").unwrap().timestamp() as u64;
    let p = parse_actions(f.path(), Some(since), true).unwrap();
    assert_eq!(p.closed.len(), 0);
}
```

- [ ] **Step 7: Run all parser tests**

Run: `cargo test --lib forward_report::tests`
Expected: PASS (4 tests).

- [ ] **Step 8: Commit**

```bash
git add src/portfolio/forward_report.rs src/portfolio/mod.rs Cargo.toml
git commit -m "feat(forward-report): paper action-log parser + round-trip reconstruction"
```

---

### Task 2: Realized metrics from closed round-trips

**Files:**
- Modify: `src/portfolio/forward_report.rs`
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Consumes: `ClosedTrip` (Task 1), `sim::risk_metrics(&[(u64,f64)], f64) -> sim::RiskMetrics` (existing; fields `sharpe`, `sortino`, `true_max_dd_pct`).
- Produces:
  - `pub struct RealizedMetrics { pub net_pnl: f64, pub n_trades: usize, pub win_rate: f64, pub sortino: f64, pub max_dd_pct: f64 }`
  - `pub fn realized_metrics(closed: &[ClosedTrip], start_pool: f64) -> RealizedMetrics`

- [ ] **Step 1: Write the failing test**

```rust
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
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib forward_report::tests::realized_metrics_basic`
Expected: FAIL — `realized_metrics` not found.

- [ ] **Step 3: Implement realized metrics**

```rust
pub struct RealizedMetrics { pub net_pnl: f64, pub n_trades: usize, pub win_rate: f64, pub sortino: f64, pub max_dd_pct: f64 }

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
    let span_secs = (closed.last().unwrap().exit_ts - closed[0].entry_ts).max(1) as f64;
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib forward_report::tests::realized_metrics_basic`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/forward_report.rs
git commit -m "feat(forward-report): realized metrics (P&L, win%, Sortino, maxDD) from round-trips"
```

---

### Task 3: Predicted metrics — replay the live config over the forward window

**Files:**
- Modify: `src/portfolio/forward_report.rs`
- Modify: `src/portfolio/sim.rs` (make `regime_mask` + `regime_mask_trend` `pub(crate)`)
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Consumes: `sim::base_params(&PortfolioConfig) -> ParamSet`, `sim::ranked_stream(&[PriceSnapshot], &[WatchedToken], &ParamSet)`, `sim::replay_multi_mtm(...) -> (SimRun, Vec<(u64,f64)>)`, `sim::risk_metrics`, `sim::regime_mask`, `sim::regime_mask_trend`, `RegimeMode`.
- Produces:
  - `pub struct PredictedMetrics { pub net_pnl: f64, pub n_trades: usize, pub sortino: f64, pub max_dd_pct: f64 }`
  - `pub fn predicted_metrics(window: &[history::PriceSnapshot], watched: &[momentum_universe::WatchedToken], cfg: &PortfolioConfig) -> PredictedMetrics`

- [ ] **Step 1: Make regime helpers crate-visible**

In `src/portfolio/sim.rs`, change `fn regime_mask(` → `pub(crate) fn regime_mask(` and `fn regime_mask_trend(` → `pub(crate) fn regime_mask_trend(`. No logic change.

Run: `cargo build --bin momentum-sim` — Expected: compiles.

- [ ] **Step 2: Write the failing test (predicted runs on a synthetic uptrend)**

```rust
#[test]
fn predicted_runs_over_window() {
    use crate::portfolio::history::PriceSnapshot;
    use crate::portfolio::momentum_universe::WatchedToken;
    use std::collections::HashMap;
    // 600 snapshots, 184s apart: a token rising then flat, SOL flat (regime off via cfg in test).
    let mut snaps = Vec::new();
    for i in 0..600u64 {
        let mut p = HashMap::new();
        let px = 1.0 + (i as f64) * 0.002; // steady uptrend → momentum should fire
        p.insert("MINT".to_string(), px);
        p.insert("SOL".to_string(), 100.0);
        snaps.push(PriceSnapshot { ts: 1_750_000_000 + i * 184, prices: p });
    }
    let watched = vec![WatchedToken { symbol: "TOK".into(), mint: "MINT".into(), name: "Tok".into(), params: Default::default() }];
    let cfg = crate::portfolio::PortfolioConfig::from_env().unwrap_or_default();
    let m = predicted_metrics(&snaps, &watched, &cfg);
    // Smoke: it returns without panicking and yields a finite P&L.
    assert!(m.net_pnl.is_finite());
}
```

> NOTE for implementer: confirm `WatchedToken`'s field names/`Default` and `PortfolioConfig::from_env`/`Default` against `src/portfolio/momentum_universe.rs` and `src/config.rs`; adjust the literal to match (this is the only struct-literal in the plan that depends on fields not shown verbatim here).

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test --lib forward_report::tests::predicted_runs_over_window`
Expected: FAIL — `predicted_metrics` not found.

- [ ] **Step 4: Implement predicted_metrics (faithful to the live multi-token single-slot trader)**

```rust
use crate::portfolio::{history, momentum_universe, sim, PortfolioConfig, RegimeMode};

pub struct PredictedMetrics { pub net_pnl: f64, pub n_trades: usize, pub sortino: f64, pub max_dd_pct: f64 }

pub fn predicted_metrics(window: &[history::PriceSnapshot], watched: &[momentum_universe::WatchedToken], cfg: &PortfolioConfig) -> PredictedMetrics {
    // Build the SAME ParamSet the live trader uses, including its regime gate.
    let mut base = sim::base_params(cfg);
    base.regime_filter_obs = cfg.momentum_regime_obs;
    base.regime_mode = cfg.momentum_regime_mode;
    base.regime_threshold = cfg.momentum_regime_threshold; // confirm field name in config.rs
    if window.len() < 2 || watched.is_empty() {
        return PredictedMetrics { net_pnl: 0.0, n_trades: 0, sortino: 0.0, max_dd_pct: 0.0 };
    }
    let stream = sim::ranked_stream(window, watched, &base);
    let regime: Vec<bool> = match base.regime_mode {
        RegimeMode::Off => vec![true; window.len()],
        RegimeMode::Level => sim::regime_mask(window, base.regime_filter_obs),
        RegimeMode::Trend => sim::regime_mask_trend(window, base.regime_filter_obs, base.regime_threshold),
    };
    let (run, equity) = sim::replay_multi_mtm(window, watched, &stream, &base, &regime, cfg.momentum_max_positions);
    let rm = sim::risk_metrics(&equity, PERIODS_PER_YEAR);
    PredictedMetrics { net_pnl: run.net_pnl(), n_trades: run.n_trades(), sortino: rm.sortino, max_dd_pct: rm.true_max_dd_pct }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test --lib forward_report::tests::predicted_runs_over_window`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/forward_report.rs src/portfolio/sim.rs
git commit -m "feat(forward-report): predicted metrics via live-config replay over the forward window"
```

---

### Task 4: Reconciliation + graduation scorecard

**Files:**
- Modify: `src/portfolio/forward_report.rs`
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Consumes: `RealizedMetrics` (Task 2), `PredictedMetrics` (Task 3), `ParsedLog` (Task 1).
- Produces:
  - `pub struct GraduationBar { pub min_weeks: f64, pub min_trades: usize, pub min_pnl_frac: f64, pub max_dd_pct: f64 }` with `impl Default`.
  - `pub enum Verdict { Insufficient, KeepPapering, EligibleForSmallLive }`
  - `pub struct Scorecard { pub verdict: Verdict, pub window_weeks: f64, pub realized: RealizedMetrics, pub predicted: PredictedMetrics, pub pnl_frac: f64, pub config_changed: bool, pub lines: Vec<(String, String, bool)> }` (each line: criterion, detail, pass?)
  - `pub fn reconcile(parsed: &ParsedLog, realized: &RealizedMetrics, predicted: &PredictedMetrics, bar: &GraduationBar) -> Scorecard`

- [ ] **Step 1: Write the failing tests (insufficient + eligible)**

```rust
fn mk(net: f64, n: usize, sortino: f64, dd: f64) -> RealizedMetrics {
    RealizedMetrics { net_pnl: net, n_trades: n, win_rate: 60.0, sortino, max_dd_pct: dd }
}
fn pred(net: f64, n: usize) -> PredictedMetrics { PredictedMetrics { net_pnl: net, n_trades: n, sortino: 1.0, max_dd_pct: 10.0 } }
fn parsed_with_span(weeks: f64, metric_changes: bool) -> ParsedLog {
    let start = 1_750_000_000u64;
    let end = start + (weeks * 7.0 * 86_400.0) as u64;
    let cps = if metric_changes {
        vec![ConfigPoint{ts:start,metric:"return".into(),min_score:0.05}, ConfigPoint{ts:end,metric:"return".into(),min_score:0.09}]
    } else {
        vec![ConfigPoint{ts:start,metric:"return".into(),min_score:0.05}, ConfigPoint{ts:end,metric:"return".into(),min_score:0.05}]
    };
    ParsedLog { first_ts: Some(start), last_ts: Some(end), config_points: cps, ..Default::default() }
}

#[test]
fn insufficient_when_too_few_trades() {
    let bar = GraduationBar::default();
    let s = reconcile(&parsed_with_span(8.0, false), &mk(5.0, 3, 1.0, 10.0), &pred(8.0, 4), &bar);
    assert!(matches!(s.verdict, Verdict::Insufficient));
}

#[test]
fn eligible_when_all_pass() {
    let bar = GraduationBar { min_weeks: 6.0, min_trades: 20, min_pnl_frac: 0.6, max_dd_pct: 50.0 };
    // realized 25 trades, +$12 vs predicted +$15 (80% ≥ 60%), Sortino>0, DD 20%<50%, 8 weeks.
    let s = reconcile(&parsed_with_span(8.0, false), &mk(12.0, 25, 1.4, 20.0), &pred(15.0, 22), &bar);
    assert!(matches!(s.verdict, Verdict::EligibleForSmallLive));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib forward_report::tests::insufficient_when_too_few_trades forward_report::tests::eligible_when_all_pass`
Expected: FAIL — `reconcile`/`GraduationBar` not found.

- [ ] **Step 3: Implement reconcile + scorecard**

```rust
pub struct GraduationBar { pub min_weeks: f64, pub min_trades: usize, pub min_pnl_frac: f64, pub max_dd_pct: f64 }
impl Default for GraduationBar {
    fn default() -> Self { GraduationBar { min_weeks: 6.0, min_trades: 20, min_pnl_frac: 0.6, max_dd_pct: 50.0 } }
}
#[derive(Debug, PartialEq)]
pub enum Verdict { Insufficient, KeepPapering, EligibleForSmallLive }
pub struct Scorecard { pub verdict: Verdict, pub window_weeks: f64, pub realized: RealizedMetrics, pub predicted: PredictedMetrics, pub pnl_frac: f64, pub config_changed: bool, pub lines: Vec<(String, String, bool)> }

pub fn reconcile(parsed: &ParsedLog, realized: &RealizedMetrics, predicted: &PredictedMetrics, bar: &GraduationBar) -> Scorecard {
    let window_secs = parsed.last_ts.unwrap_or(0).saturating_sub(parsed.first_ts.unwrap_or(0)) as f64;
    let window_weeks = window_secs / (7.0 * 86_400.0);
    let pnl_frac = if predicted.net_pnl > 0.0 { realized.net_pnl / predicted.net_pnl } else if realized.net_pnl >= 0.0 { 1.0 } else { 0.0 };
    let config_changed = parsed.config_points.windows(2).any(|w| w[0].metric != w[1].metric || (w[0].min_score - w[1].min_score).abs() > 1e-9);

    let mut lines = Vec::new();
    let c_window = window_weeks >= bar.min_weeks;
    lines.push(("Forward window".into(), format!("{:.1}w ≥ {:.0}w", window_weeks, bar.min_weeks), c_window));
    let c_trades = realized.n_trades >= bar.min_trades;
    lines.push(("Closed trades".into(), format!("{} ≥ {}", realized.n_trades, bar.min_trades), c_trades));
    let c_sortino = realized.sortino > 0.0;
    lines.push(("Realized Sortino".into(), format!("{:.2} > 0", realized.sortino), c_sortino));
    let c_pnl = pnl_frac >= bar.min_pnl_frac;
    lines.push(("Realized vs predicted P&L".into(), format!("{:.0}% ≥ {:.0}%", pnl_frac * 100.0, bar.min_pnl_frac * 100.0), c_pnl));
    let c_dd = realized.max_dd_pct <= bar.max_dd_pct;
    lines.push(("Max drawdown".into(), format!("{:.1}% ≤ {:.0}%", realized.max_dd_pct, bar.max_dd_pct), c_dd));

    // Too few trades is a hard short-circuit: never emit a PASS/FAIL verdict on a tiny sample.
    let verdict = if realized.n_trades < bar.min_trades {
        Verdict::Insufficient
    } else if c_window && c_sortino && c_pnl && c_dd {
        Verdict::EligibleForSmallLive
    } else {
        Verdict::KeepPapering
    };
    Scorecard { verdict, window_weeks, realized: realized.clone(), predicted: predicted.clone(), pnl_frac, config_changed, lines }
}
```

> Add `#[derive(Clone)]` to `RealizedMetrics` and `PredictedMetrics` (needed by `Scorecard`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib forward_report::tests`
Expected: PASS (all parser + metrics + reconcile tests).

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/forward_report.rs
git commit -m "feat(forward-report): three-way reconciliation + graduation scorecard"
```

---

### Task 5: Rendering + CLI wiring

**Files:**
- Modify: `src/portfolio/forward_report.rs` (render + top-level `run_forward_report`)
- Modify: `src/bin/momentum_sim.rs` (`ForwardReport` command + handler)
- Test: same module `#[cfg(test)]` (render smoke test)

**Interfaces:**
- Consumes: everything above; `history::load_history`, `sim::sanitize_history`, `momentum_universe::load`.
- Produces:
  - `pub fn render(scorecard: &Scorecard, parsed: &ParsedLog, coverage_pct: f64) -> String`
  - `pub fn run_forward_report(cfg: &PortfolioConfig, actions_path: &str, history_path: &str, since: Option<u64>, paper_only: bool, bar: GraduationBar, max_step: f64) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing render smoke test**

```rust
#[test]
fn render_contains_verdict_and_warnings() {
    let bar = GraduationBar::default();
    let sc = reconcile(&parsed_with_span(2.0, true), &mk(1.0, 3, 0.5, 10.0), &pred(2.0, 2), &bar);
    let out = render(&sc, &parsed_with_span(2.0, true), 95.0);
    assert!(out.contains("INSUFFICIENT") || out.contains("KEEP PAPERING") || out.contains("ELIGIBLE"));
    assert!(out.contains("config")); // config-drift warning present since metric/min_score changed
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib forward_report::tests::render_contains_verdict_and_warnings`
Expected: FAIL — `render` not found.

- [ ] **Step 3: Implement render + run_forward_report**

```rust
pub fn render(sc: &Scorecard, parsed: &ParsedLog, coverage_pct: f64) -> String {
    let mut s = String::new();
    s.push_str(&format!("\n{}\n", "=".repeat(72)));
    s.push_str(&format!("MOMENTUM FORWARD-TEST REPORT  ({:.1} weeks, {:.0}% history coverage)\n", sc.window_weeks, coverage_pct));
    s.push_str(&format!("{}\n", "=".repeat(72)));
    if sc.config_changed { s.push_str("⚠  config drift detected in the window (metric/min_score changed) — realized trades may not match the predicted config.\n"); }
    if parsed.real_filtered > 0 { s.push_str(&format!("ℹ  excluded {} non-paper (real) action records (--paper-only).\n", parsed.real_filtered)); }
    if !parsed.open.is_empty() { s.push_str(&format!("ℹ  {} open position(s) excluded from realized P&L (informational only).\n", parsed.open.len())); }
    s.push_str(&format!("\nRealized:  net ${:+.2}, {} trades, win {:.0}%, Sortino {:.2}, maxDD {:.1}%\n",
        sc.realized.net_pnl, sc.realized.n_trades, sc.realized.win_rate, sc.realized.sortino, sc.realized.max_dd_pct));
    s.push_str(&format!("Predicted: net ${:+.2}, {} trades, Sortino {:.2}, maxDD {:.1}%   (gap: realized = {:.0}% of predicted)\n\n",
        sc.predicted.net_pnl, sc.predicted.n_trades, sc.predicted.sortino, sc.predicted.max_dd_pct, sc.pnl_frac * 100.0));
    for (name, detail, pass) in &sc.lines {
        s.push_str(&format!("  [{}] {:<26} {}\n", if *pass { "PASS" } else { "----" }, name, detail));
    }
    let verdict = match sc.verdict {
        Verdict::Insufficient => "INSUFFICIENT DATA — keep accumulating paper trades before judging.",
        Verdict::KeepPapering => "KEEP PAPERING — edge not yet confirmed out-of-sample.",
        Verdict::EligibleForSmallLive => "ELIGIBLE FOR SMALL LIVE — forward test tracks the backtest; consider a small live allocation.",
    };
    s.push_str(&format!("\n=> {verdict}\n{}\n", "=".repeat(72)));
    s
}

pub fn run_forward_report(cfg: &PortfolioConfig, actions_path: &str, history_path: &str, since: Option<u64>, paper_only: bool, bar: GraduationBar, max_step: f64) -> Result<()> {
    let parsed = parse_actions(Path::new(actions_path), since, paper_only)?;
    if since.is_none() {
        eprintln!("⚠  --since not set: the forward window may overlap the backtest's training data — the comparison is NOT out-of-sample.");
    }
    let raw: Vec<_> = history::load_history(Path::new(history_path))?.into_iter().collect();
    let all = sim::sanitize_history(&raw, max_step);
    let win_start = since.unwrap_or(0);
    let window: Vec<_> = all.iter().filter(|s| s.ts >= win_start).cloned().collect();
    let coverage_pct = if window.is_empty() { 0.0 } else {
        // expected ≈ span / 184s; coverage = actual / expected
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib forward_report::tests::render_contains_verdict_and_warnings`
Expected: PASS.

- [ ] **Step 5: Add the CLI command variant**

In `src/bin/momentum_sim.rs`, add to the `Command` enum:

```rust
    /// Reconcile realized paper performance vs the backtest prediction over the forward window.
    ForwardReport {
        #[arg(long, default_value = "assets/momentum_actions.jsonl")]
        actions: String,
        #[arg(long)]
        history: Option<String>,
        /// Forward-window start (RFC3339). Defaults to the config-lock date you pass.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = true)]
        paper_only: bool,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        #[arg(long, default_value_t = 6.0)]
        min_weeks: f64,
        #[arg(long, default_value_t = 20)]
        min_trades: usize,
        #[arg(long, default_value_t = 0.6)]
        min_pnl_frac: f64,
        #[arg(long, default_value_t = 50.0)]
        max_dd_pct: f64,
    },
```

- [ ] **Step 6: Add the dispatch arm in `main()`**

In the `match cli.command { ... }` block:

```rust
        Command::ForwardReport { actions, history, since, paper_only, max_step, min_weeks, min_trades, min_pnl_frac, max_dd_pct } => {
            let since_ts = match since {
                Some(s) => Some(chrono::DateTime::parse_from_rfc3339(&s).map(|d| d.timestamp() as u64)
                    .map_err(|e| anyhow::anyhow!("bad --since (want RFC3339 like 2026-06-21T00:00:00Z): {e}"))?),
                None => None,
            };
            let history_path = history.unwrap_or_else(|| cfg.history_path.clone());
            let bar = solana_mev::portfolio::forward_report::GraduationBar { min_weeks, min_trades, min_pnl_frac, max_dd_pct };
            solana_mev::portfolio::forward_report::run_forward_report(&cfg, &actions, &history_path, since_ts, paper_only, bar, max_step)
        }
```

- [ ] **Step 7: Build + run the whole suite + a real smoke run**

Run: `cargo build --bin momentum-sim`
Expected: compiles.
Run: `cargo test --bin momentum-sim forward_report`
Expected: PASS.
Run: `./target/release/momentum-sim forward-report --since 2026-06-21T00:00:00Z` (after `cargo build --release --bin momentum-sim`)
Expected: prints the report against the live `assets/momentum_actions.jsonl` without panicking; verdict is `INSUFFICIENT DATA` (only ~6 closed paper trades exist today).

- [ ] **Step 8: Commit**

```bash
git add src/portfolio/forward_report.rs src/bin/momentum_sim.rs
git commit -m "feat(forward-report): rendering + momentum-sim forward-report subcommand"
```

---

## Self-Review

**Spec coverage:**
- Read-only subcommand on momentum-sim → Task 5. ✓
- Parse Entered/Exited/Rotated, filter dry_run + --since → Task 1. ✓
- Realized metrics (P&L/win%/Sortino/maxDD) → Task 2. ✓
- Predicted via backtest on same window, live config → Task 3. ✓
- Three-way reconciliation + graduation scorecard → Task 4. ✓
- Edge cases: open positions (Task 1 + render), real-vs-paper (Task 1 `real_filtered`), too-few-trades short-circuit (Task 4 `Verdict::Insufficient`), in-sample warning (Task 5 `run_forward_report`), config drift (Task 4 `config_changed` + Task 5 render), coverage % (Task 5). ✓
- Tests in `#[cfg(test)]` → every task. ✓
- No arb/trader changes → only `sim.rs` visibility tweak + new module + CLI variant. ✓

**Placeholder scan:** One explicit implementer NOTE in Task 3 Step 2 flags the single struct-literal (`WatchedToken`/`PortfolioConfig`) whose field names aren't shown verbatim and must be confirmed against `momentum_universe.rs`/`config.rs`; everything else is concrete. The `base.regime_threshold = cfg.momentum_regime_threshold` line is marked "confirm field name."

**Type consistency:** `ClosedTrip`, `RealizedMetrics`, `PredictedMetrics`, `Scorecard`, `GraduationBar`, `Verdict` names are used consistently across Tasks 2–5. `realized_metrics`/`predicted_metrics`/`reconcile`/`render`/`run_forward_report` signatures match between their defining task and their call site in Task 5.

## Known implementer confirmations (not placeholders — exact values to verify in-repo)
- `WatchedToken` field names + `Default` (Task 3 test literal) — `src/portfolio/momentum_universe.rs`.
- `cfg.momentum_regime_obs`, `cfg.momentum_regime_mode`, `cfg.momentum_regime_threshold`, `cfg.momentum_max_positions`, `cfg.momentum_tokens_path`, `cfg.history_path`, `cfg.momentum_trade_usdc` — `src/config.rs` (names follow the established `momentum_*` convention; correct if any differ).
- `RegimeMode` import path + variants (`Off`/`Level`/`Trend`) — `src/portfolio/`.
