# Risk-Adjusted Robustness vs N (mark-to-market) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Measure whether holding N>1 names is a smoother, lower-variance ride (higher Sharpe/Sortino, smaller true drawdown) than single-slot at equal capital — by adding a mark-to-market equity curve to `replay_multi`, a `risk_metrics` helper, and a RISK VERDICT to `maxn-optimize`.

**Architecture:** `replay_multi`'s body moves into a private `replay_multi_core(..., record_mtm)` that optionally emits a per-snapshot mark-to-market (MTM) equity curve; `replay_multi` and a new `replay_multi_mtm` are thin delegates (public contract and N=1 anchor preserved). A pure `risk_metrics` computes annualized Sharpe/Sortino + true drawdown from any equity curve. `maxn-optimize` runs one extra `replay_multi_mtm` per N's winning config on the test slice and prints risk metrics + a RISK VERDICT.

**Tech Stack:** Rust. Tests are `#[cfg(test)]` blocks at the bottom of `src/portfolio/sim.rs`, run with `cargo test --lib` (these are LIB tests — `cargo test --bin momentum-sim` shows 0 tests).

## Global Constraints

- **Sim-only:** no changes to the live trader (`src/portfolio/momentum.rs`, `momentum_state.rs`).
- **Production grid untouched:** do NOT modify `run_grid`, `replay`, `replay_with_stream`, `replay_with_regime`. `run_grid_multi` (which calls `replay_multi`) keeps working unchanged.
- **`replay_multi` public contract preserved:** same signature `replay_multi(snapshots, watched, stream, params, regime, max_positions) -> SimRun`, same behavior/output. The refactor moves its body into `replay_multi_core` and makes `replay_multi` a delegate — all existing `replay_multi` tests (both N=1 anchors, eviction, dedup, drawdown) and `run_grid_multi` tests MUST still pass unchanged.
- **No new `.env` variable.**
- **Equal total capital:** carried from `maxn-optimize` — `trade_usdc = pool / N`, so `pool = params.trade_usdc × max_positions`.
- **MTM curve:** `equity_t = pool + realized_so_far + Σ_open(tokens_i × mark_i,t − usdc_spent_i)`, one point per snapshot, marks carried forward via a running last-seen-price map. Risk metrics on the **test (held-out)** slice, for the **best-by-test-P&L** config at each N.
- **Risk metric edge cases:** `<2` returns → all metrics `0.0`; `downside_dev == 0` → Sortino `f64::INFINITY`; true drawdown is peak-to-trough / running peak (in `[0,100)` for positive equity).
- **Drop** the realized `max_dd_test` from the per-N output line (it can exceed 100%); show only the MTM `trueDD`. Keep `win %`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/sim.rs` | Refactor `replay_multi` → `replay_multi_core` + delegates incl. `replay_multi_mtm`; add `RiskMetrics` + `risk_metrics`; tests. | Modify (additive + internal refactor) |
| `src/bin/momentum_sim.rs` | Extend `maxn_optimize` to compute + print MTM risk metrics per N and a RISK VERDICT. | Modify |

---

## Task 1: MTM equity curve via `replay_multi_core` refactor

**Files:**
- Modify: `src/portfolio/sim.rs` (the `replay_multi` function, ~lines 652–924)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: everything `replay_multi` already uses; `SOL_KEY`, `Position`.
- Produces:
  - `fn replay_multi_core(snapshots, watched, stream, params, regime, max_positions, record_mtm: bool) -> (SimRun, Vec<(u64, f64)>)` (private)
  - `pub fn replay_multi(...) -> SimRun` (unchanged signature; delegate)
  - `pub fn replay_multi_mtm(snapshots, watched, stream, params, regime, max_positions) -> (SimRun, Vec<(u64, f64)>)`

- [ ] **Step 1: Write the failing MTM tests**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/sim.rs`:

```rust
    #[test]
    fn replay_multi_mtm_curve_has_one_point_per_snapshot_and_ends_flat() {
        // Single token, rise then crash → enters, then stops out. MTM curve must have one
        // point per snapshot; once flat at the end, equity == pool + realized P&L.
        let snaps = rise_then_fall("AAA", 200, 8);
        let watched = aaa();
        let params = bare_params(); // trade_usdc = 100, max_positions below = 1 → pool = 100
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];

        let (run, mtm) = replay_multi_mtm(&snaps, &watched, &stream, &params, &mask, 1);
        assert_eq!(mtm.len(), snaps.len(), "one MTM point per snapshot");
        assert!(mtm.iter().all(|&(_, e)| e.is_finite() && e > 0.0), "equity finite & positive");

        let pool = params.trade_usdc * 1.0;
        // Last snapshot: position has stopped out → flat → equity == pool + realized.
        let expected_last = pool + run.net_pnl();
        assert!(
            (mtm.last().unwrap().1 - expected_last).abs() < 1e-6,
            "flat-at-end equity {} == pool+realized {}", mtm.last().unwrap().1, expected_last
        );
    }

    #[test]
    fn replay_multi_mtm_tracks_unrealized_during_hold() {
        // Pure rise, never stops → position held to the end → final equity carries the
        // unrealized gain (strictly above pool) while realized P&L is still 0.
        let snaps = rise_then_fall("AAA", 200, 0);
        let watched = aaa();
        let params = bare_params();
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];

        let (run, mtm) = replay_multi_mtm(&snaps, &watched, &stream, &params, &mask, 1);
        assert_eq!(run.n_trades(), 0, "pure rise never closes");
        let pool = params.trade_usdc;
        assert!(mtm.last().unwrap().1 > pool, "held winner shows unrealized gain above pool");
    }

    #[test]
    fn replay_multi_unchanged_by_refactor() {
        // The public replay_multi must still equal core(.., false): same trades + equity_curve
        // as before. (Cross-check against replay_with_regime at N=1 — the existing anchor.)
        let snaps = rise_then_fall("AAA", 130, 6);
        let watched = aaa();
        let params = bare_params();
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];
        let single = replay_with_regime(&snaps, &watched, &stream, &params, &mask);
        let multi = replay_multi(&snaps, &watched, &stream, &params, &mask, 1);
        assert_eq!(multi.trades.len(), single.trades.len());
        assert_eq!(multi.equity_curve, single.equity_curve);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib replay_multi_mtm 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'replay_multi_mtm'`.

- [ ] **Step 3: Refactor `replay_multi` into `replay_multi_core` + delegates**

In `src/portfolio/sim.rs`, make these exact edits to the existing `replay_multi` function:

**(3a)** Change the signature line:
```rust
pub fn replay_multi(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<Candidate>],
    params: &ParamSet,
    regime: &[bool],
    max_positions: usize,
) -> SimRun {
```
to:
```rust
/// Core of [`replay_multi`]. When `record_mtm`, also emits a per-snapshot mark-to-market
/// equity curve `(ts, pool + realized + unrealized)` (one point per snapshot); when false,
/// returns an empty curve and does no MTM work, so the grid path pays nothing.
#[allow(clippy::too_many_arguments)]
fn replay_multi_core(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<Candidate>],
    params: &ParamSet,
    regime: &[bool],
    max_positions: usize,
    record_mtm: bool,
) -> (SimRun, Vec<(u64, f64)>) {
```

**(3b)** Just after the existing `let mut pending_free: Vec<usize> = Vec::new();` line (before `for i in 0..n {`), add the MTM accumulators:
```rust
    // Mark-to-market (only when record_mtm): running last-seen price per mint, and the
    // per-snapshot equity curve. `pool` is the equal-capital base (trade_usdc × N).
    let pool = params.trade_usdc * max_positions as f64;
    let mut last_mark: HashMap<String, f64> = HashMap::new();
    let mut mtm: Vec<(u64, f64)> = Vec::with_capacity(if record_mtm { n } else { 0 });
```

**(3c)** As the FIRST statements inside the `for i in 0..n {` loop, right after `let sol_price = ...;`, add the last-mark update:
```rust
        if record_mtm {
            for (m, &p) in &snap.prices {
                if p > 0.0 {
                    last_mark.insert(m.clone(), p);
                }
            }
        }
```

**(3d)** Replace the entries-section regime guard. Find:
```rust
        // ── Entries: greedily fill free capacity, best-ranked first ──
        pending_free.retain(|&f| f > i); // expire returned capacity (every bar, not only regime-on)
        if !regime[i] {
            continue; // risk-off → no entries this bar
        }
        let withheld = pending_free.len();
        let mut capacity = max_positions.saturating_sub(held.len() + withheld);
        while capacity > 0 {
```
and replace it with (wrap the entry loop in `if regime[i]` instead of `continue`, so the MTM push at loop-end always runs):
```rust
        // ── Entries: greedily fill free capacity, best-ranked first ──
        pending_free.retain(|&f| f > i); // expire returned capacity (every bar, not only regime-on)
        if regime[i] {
            let withheld = pending_free.len();
            let mut capacity = max_positions.saturating_sub(held.len() + withheld);
            while capacity > 0 {
```
Then, at the END of that `while capacity > 0 { ... }` loop, after its closing `}` (the `capacity -= 1;` line is the last statement inside it), add a closing `}` for the new `if regime[i]` block, and immediately after it (still inside `for i`) add the MTM push:
```rust
            } // end while capacity
        } // end if regime[i]

        if record_mtm {
            let unrealized: f64 = held
                .iter()
                .map(|p| {
                    let mark = last_mark.get(&p.mint).copied().unwrap_or(p.entry_price_usd);
                    p.token_amount * mark - p.usdc_spent
                })
                .sum();
            mtm.push((snap.ts, pool + realized + unrealized));
        }
```

> Note: the original entry block had no extra braces; you are introducing one `if regime[i] { ... }` block. Make sure the brace count is balanced — the `while capacity > 0` body is unchanged, it's just now nested one level deeper inside `if regime[i]`.

**(3e)** Change the final return. Find the function's last statement:
```rust
    SimRun { trades, equity_curve }
}
```
and replace with:
```rust
    (SimRun { trades, equity_curve }, mtm)
}

/// Single-slot-generalizing multi-position replay (see module docs). Unchanged public
/// contract: returns just the `SimRun`. Delegates to `replay_multi_core` with MTM off.
pub fn replay_multi(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<Candidate>],
    params: &ParamSet,
    regime: &[bool],
    max_positions: usize,
) -> SimRun {
    replay_multi_core(snapshots, watched, stream, params, regime, max_positions, false).0
}

/// Like [`replay_multi`] but also returns the per-snapshot mark-to-market equity curve
/// `(ts, pool + realized + unrealized)` for risk-adjusted analysis. Used only by the
/// max-N comparison (a handful of replays), never the grid hot path.
#[allow(clippy::too_many_arguments)]
pub fn replay_multi_mtm(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<Candidate>],
    params: &ParamSet,
    regime: &[bool],
    max_positions: usize,
) -> (SimRun, Vec<(u64, f64)>) {
    replay_multi_core(snapshots, watched, stream, params, regime, max_positions, true)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib replay_multi 2>&1 | tail -20`
Expected: PASS — the three new MTM tests AND every pre-existing `replay_multi*` test (both N=1 anchors, eviction, dedup, drawdown).
Then the full suite: `cargo test --lib 2>&1 | tail -4` — no regressions, no new warnings.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): mark-to-market equity curve via replay_multi_core refactor

Move replay_multi's body into a private core that optionally emits a per-snapshot
MTM equity curve (pool + realized + unrealized); replay_multi keeps its public
contract as a delegate, plus a new replay_multi_mtm. Existing behavior + N=1 anchor
preserved; grid path pays nothing (record_mtm=false).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `RiskMetrics` + `risk_metrics`

**Files:**
- Modify: `src/portfolio/sim.rs` (add after `replay_multi_mtm`)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Produces:
  - `pub struct RiskMetrics { pub sharpe: f64, pub sortino: f64, pub true_max_dd_pct: f64 }` (derives `Debug, Clone, Default`)
  - `pub fn risk_metrics(equity: &[(u64, f64)], periods_per_year: f64) -> RiskMetrics`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
    fn curve(values: &[f64]) -> Vec<(u64, f64)> {
        values.iter().enumerate().map(|(i, &v)| (i as u64, v)).collect()
    }

    #[test]
    fn risk_metrics_rising_line_high_sharpe_no_drawdown() {
        let eq = curve(&[100.0, 101.0, 102.01, 103.03, 104.06]); // ~+1% each step, monotonic
        let m = risk_metrics(&eq, 252.0);
        assert!(m.sharpe > 0.0, "rising line has positive Sharpe");
        assert!(m.true_max_dd_pct.abs() < 1e-9, "monotonic rise has ~0 drawdown");
        assert!(m.sortino.is_infinite() && m.sortino > 0.0, "no downside → Sortino +inf");
    }

    #[test]
    fn risk_metrics_peak_then_trough_drawdown_is_known() {
        let eq = curve(&[100.0, 150.0, 90.0]); // peak 150 → trough 90
        let m = risk_metrics(&eq, 252.0);
        // (150 − 90) / 150 × 100 = 40%
        assert!((m.true_max_dd_pct - 40.0).abs() < 1e-9, "drawdown is 40%, got {}", m.true_max_dd_pct);
    }

    #[test]
    fn risk_metrics_flat_zigzag_near_zero_sharpe() {
        let eq = curve(&[100.0, 110.0, 100.0, 110.0, 100.0]); // no net drift
        let m = risk_metrics(&eq, 252.0);
        assert!(m.sharpe.abs() < 1.0, "no-drift zigzag has near-zero Sharpe, got {}", m.sharpe);
        assert!(m.true_max_dd_pct > 0.0, "zigzag has a real drawdown");
    }

    #[test]
    fn risk_metrics_degenerate_returns_zeros() {
        assert_eq!(risk_metrics(&curve(&[100.0]), 252.0).sharpe, 0.0);
        assert_eq!(risk_metrics(&curve(&[100.0, 101.0]), 252.0).sharpe, 0.0); // 1 return < 2
        assert_eq!(risk_metrics(&[], 252.0).true_max_dd_pct, 0.0);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib risk_metrics 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'risk_metrics'` / `cannot find type 'RiskMetrics'`.

- [ ] **Step 3: Implement `RiskMetrics` + `risk_metrics`**

Insert after `replay_multi_mtm` in `src/portfolio/sim.rs`:

```rust
/// Risk-adjusted summary of an equity curve. Sharpe/Sortino are annualized; drawdown is a
/// percent of the running peak.
#[derive(Debug, Clone, Default)]
pub struct RiskMetrics {
    pub sharpe: f64,
    pub sortino: f64,
    pub true_max_dd_pct: f64,
}

/// Annualized Sharpe & Sortino plus true max drawdown for an equity curve. Returns are
/// per-step simple returns `(e_k − e_{k−1}) / e_{k−1}` (skipping any `e_{k−1} ≤ 0`).
/// `<2` returns ⇒ all-zero; `downside_dev == 0` ⇒ Sortino `+∞` (no downside observed).
/// Drawdown is `max (peak − e)/peak × 100` over the running peak.
pub fn risk_metrics(equity: &[(u64, f64)], periods_per_year: f64) -> RiskMetrics {
    let mut rets: Vec<f64> = Vec::new();
    for w in equity.windows(2) {
        let prev = w[0].1;
        if prev > 0.0 {
            rets.push((w[1].1 - prev) / prev);
        }
    }
    if rets.len() < 2 {
        return RiskMetrics::default();
    }
    let n = rets.len() as f64;
    let mean = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
    let sd = var.sqrt();
    let ann = periods_per_year.sqrt();
    let sharpe = if sd > 0.0 { mean / sd * ann } else { 0.0 };
    // Downside deviation: RMS of the negative returns over ALL returns (target = 0).
    let downside = (rets.iter().map(|r| r.min(0.0).powi(2)).sum::<f64>() / n).sqrt();
    let sortino = if downside > 0.0 {
        mean / downside * ann
    } else if mean > 0.0 {
        f64::INFINITY // no downward step observed
    } else {
        0.0
    };
    // True max drawdown: peak-to-trough as a percent of the running peak.
    let mut peak = f64::NEG_INFINITY;
    let mut max_dd = 0.0_f64;
    for &(_, e) in equity {
        peak = peak.max(e);
        if peak > 0.0 {
            max_dd = max_dd.max((peak - e) / peak * 100.0);
        }
    }
    RiskMetrics { sharpe, sortino, true_max_dd_pct: max_dd }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib risk_metrics 2>&1 | tail -10`
Expected: PASS — all four `risk_metrics_*` tests.
Then: `cargo test --lib 2>&1 | tail -4` — no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): risk_metrics — annualized Sharpe/Sortino + true drawdown

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: RISK VERDICT in `maxn-optimize`

**Files:**
- Modify: `src/bin/momentum_sim.rs` (the `maxn_optimize` function)

**Interfaces:**
- Consumes: `sim::replay_multi_mtm`, `sim::risk_metrics`, `sim::RiskMetrics`, `sim::ranked_stream`, `sim::regime_mask`, `sim::regime_mask_trend`, `RegimeMode` (already imported in this file), `sim::SimResult`.
- Produces: extended `maxn-optimize` output with per-N risk metrics + a RISK VERDICT.

- [ ] **Step 1: Extend the per-N summary to compute risk metrics**

In `src/bin/momentum_sim.rs`, in `maxn_optimize`, change the `summary` collection. Replace:
```rust
    // (N, best robust config or None, per-slot notional)
    let mut summary: Vec<(usize, Option<sim::SimResult>, f64)> = Vec::new();
    for &n in &n_values {
        let mut base = base_params(cfg);
        base.trade_usdc = pool / n as f64;
        base.size_ceiling_usdc = base.trade_usdc; // fixed notional per slot
        base.reinvest_frac = 0.0;
        // Rotation is a real lever only at N=1; moot at N == token count.
        let rf: Vec<f64> = if n == 1 { rotate_factors.clone() } else { vec![0.0] };
        let results = sim::run_grid_multi(
            train, test, &watched, &base,
            &sim::GRID_METRICS, &sim::GRID_LOOKBACKS, &sim::GRID_MAX_RUNS, &sim::GRID_TRAILS,
            &sim::GRID_MIN_QUANTILES, &rf, &regime_obs, &regime_trend_obs,
            &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, n,
        );
        let best = sim::best_robust_by_test(&results, min_trades).cloned();
        summary.push((n, best, base.trade_usdc));
    }
```
with:
```rust
    // Annualization cadence: nominal 184 s/snapshot (matches span_days). Both N share it.
    let periods_per_year = 365.0 * 86_400.0 / 184.0;
    // (N, best robust config or None, per-slot notional, test-MTM risk or None)
    let mut summary: Vec<(usize, Option<sim::SimResult>, f64, Option<sim::RiskMetrics>)> = Vec::new();
    for &n in &n_values {
        let mut base = base_params(cfg);
        base.trade_usdc = pool / n as f64;
        base.size_ceiling_usdc = base.trade_usdc; // fixed notional per slot
        base.reinvest_frac = 0.0;
        // Rotation is a real lever only at N=1; moot at N == token count.
        let rf: Vec<f64> = if n == 1 { rotate_factors.clone() } else { vec![0.0] };
        let results = sim::run_grid_multi(
            train, test, &watched, &base,
            &sim::GRID_METRICS, &sim::GRID_LOOKBACKS, &sim::GRID_MAX_RUNS, &sim::GRID_TRAILS,
            &sim::GRID_MIN_QUANTILES, &rf, &regime_obs, &regime_trend_obs,
            &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, n,
        );
        let best = sim::best_robust_by_test(&results, min_trades).cloned();
        // For the winning config: mark-to-market the test slice → risk metrics.
        let risk = best.as_ref().map(|r| {
            let p = &r.params;
            let stream = sim::ranked_stream(test, &watched, p);
            let mask: Vec<bool> = match p.regime_mode {
                RegimeMode::Off => vec![true; test.len()],
                RegimeMode::Level => sim::regime_mask(test, p.regime_filter_obs),
                RegimeMode::Trend => sim::regime_mask_trend(test, p.regime_filter_obs, p.regime_threshold),
            };
            let (_, mtm) = sim::replay_multi_mtm(test, &watched, &stream, p, &mask, n);
            sim::risk_metrics(&mtm, periods_per_year)
        });
        summary.push((n, best, base.trade_usdc, risk));
    }
```

- [ ] **Step 2: Update the per-N print block (drop realized maxDD, add risk line)**

Replace:
```rust
    for (n, best, notional) in &summary {
        let label = if *n == 1 { "single slot".to_string() } else { format!("hold {n}") };
        println!("N={n}  ({label}, ${:.2}/slot):", notional);
        match best {
            Some(r) => println!(
                "  {}\n  test {:+.2} | train {:+.2} | trades {} | win {:.0}% | maxDD {:.1}%\n",
                fmt_cfg(r), r.net_pnl_test, r.net_pnl_train, r.n_trades_test,
                r.win_rate_test, r.max_dd_test.abs()
            ),
            None => println!("  no robust config at N={n} (min_trades={min_trades})\n"),
        }
    }
```
with:
```rust
    for (n, best, notional, risk) in &summary {
        let label = if *n == 1 { "single slot".to_string() } else { format!("hold {n}") };
        println!("N={n}  ({label}, ${:.2}/slot):", notional);
        match (best, risk) {
            (Some(r), Some(rm)) => {
                println!("  {}", fmt_cfg(r));
                println!(
                    "  test {:+.2} | train {:+.2} | trades {} | win {:.0}%",
                    r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.win_rate_test
                );
                println!(
                    "  risk(test MTM): Sharpe {:.2} | Sortino {:.2} | trueDD {:.1}%\n",
                    rm.sharpe, rm.sortino, rm.true_max_dd_pct
                );
            }
            _ => println!("  no robust config at N={n} (min_trades={min_trades})\n"),
        }
    }
```
> `{:.2}` on `f64::INFINITY` prints `inf` in Rust — Sortino with no downside reads as `inf`, which is correct.

- [ ] **Step 3: Add the RISK VERDICT and update the caveat**

Replace the verdict block + caveat. Find:
```rust
    // Verdict — only when both endpoints exist and both have a robust winner.
    if n_values.len() == 2 {
        let (n1, b1, _) = &summary[0];
        let (nk, bk, _) = &summary[1];
        match (b1, bk) {
            (Some(r1), Some(rk)) => {
                let (winner, delta) = if rk.net_pnl_test >= r1.net_pnl_test {
                    (format!("hold-all (N={nk})"), rk.net_pnl_test - r1.net_pnl_test)
                } else {
                    (format!("single-slot (N={n1})"), r1.net_pnl_test - rk.net_pnl_test)
                };
                println!(
                    "VERDICT: {winner} wins held-out P&L by {:+.2} USDC (equal ${pool} capital).",
                    delta
                );
            }
            _ => println!("VERDICT: inconclusive — at least one endpoint had no robust config."),
        }
    } else {
        println!("Only one endpoint (N=1 == upper endpoint) — nothing to compare against.");
    }
    println!(
        "\nCaveat: one held-out slice (~{:.0}d) — suggestive, not proven. Fixed-trail, equal-capital backtest.\nmaxDD is % of the running realized-P&L peak and can exceed 100% at N>1 (small early peak vs later concurrent losses) — read it as relative, not as a fraction of capital.",
        span_days(test)
    );
    Ok(())
```
with:
```rust
    // Verdict — only when both endpoints exist and both have a robust winner.
    if n_values.len() == 2 {
        let (n1, b1, _, rm1) = &summary[0];
        let (nk, bk, _, rmk) = &summary[1];
        match (b1, bk) {
            (Some(r1), Some(rk)) => {
                let (winner, delta) = if rk.net_pnl_test >= r1.net_pnl_test {
                    (format!("hold-all (N={nk})"), rk.net_pnl_test - r1.net_pnl_test)
                } else {
                    (format!("single-slot (N={n1})"), r1.net_pnl_test - rk.net_pnl_test)
                };
                println!(
                    "\nP&L VERDICT:  {winner} wins held-out P&L by {:+.2} USDC (equal ${pool} capital).",
                    delta
                );
                if let (Some(s1), Some(sk)) = (rm1, rmk) {
                    let smoother = if sk.sharpe >= s1.sharpe {
                        format!("hold-all (N={nk})")
                    } else {
                        format!("single-slot (N={n1})")
                    };
                    println!(
                        "RISK VERDICT: {smoother} is the smoother ride — Sharpe {:.2} vs {:.2}, trueDD {:.1}% vs {:.1}%.",
                        sk.sharpe, s1.sharpe, sk.true_max_dd_pct, s1.true_max_dd_pct
                    );
                    let supported = sk.sharpe > s1.sharpe && sk.true_max_dd_pct < s1.true_max_dd_pct;
                    println!(
                        "              Intuition \"N>1 more robust though lower P&L\": {}.",
                        if supported { "SUPPORTED on this sample" } else { "NOT clearly supported" }
                    );
                }
            }
            _ => println!("\nVERDICT: inconclusive — at least one endpoint had no robust config."),
        }
    } else {
        println!("Only one endpoint (N=1 == upper endpoint) — nothing to compare against.");
    }
    println!(
        "\nCaveat: one held-out slice (~{:.0}d) — suggestive, not proven. Risk metrics are on the\n\
         held-out test mark-to-market curve, annualized at the nominal 184 s/snapshot cadence;\n\
         crypto names co-move with SOL, so realized variance reduction may be modest.",
        span_days(test)
    );
    Ok(())
```

- [ ] **Step 4: Build and smoke-test**

Run:
```bash
cargo build --release --bin momentum-sim 2>&1 | tail -5
target/release/momentum-sim maxn-optimize --pool-usdc 8000 --min-trades 3
```
Expected: clean build; each N block now prints a `risk(test MTM): Sharpe … | Sortino … | trueDD …%` line (no realized maxDD), followed by a `P&L VERDICT:` line and a `RISK VERDICT:` line stating whether the intuition is SUPPORTED. Capture the actual output in the report.
Then: `cargo test --lib 2>&1 | tail -4` — no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(sim): RISK VERDICT in maxn-optimize (test-MTM Sharpe/Sortino/trueDD)

Per N, mark-to-market the best config on the held-out slice and report annualized
Sharpe/Sortino + true drawdown; add a RISK VERDICT testing the 'N>1 is smoother
even at lower P&L' intuition. Drops the >100%-capable realized maxDD from the line.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:**
- Risk = Sharpe/Sortino/trueDD on test-slice MTM → Task 2 (`risk_metrics`) + Task 3 (test-slice replay). ✓
- MTM curve `pool + realized + unrealized`, one point/snapshot, carry-forward marks → Task 1 (3b/3c/3d hooks, `last_mark`). ✓
- Reported for best-by-test-P&L config at each N → Task 3 (`best` from `best_robust_by_test`, risk on its params). ✓
- Equal capital `pool = trade_usdc × max_positions` → Task 1 (3b) + Task 3 (existing `trade_usdc=pool/n`). ✓
- `replay_multi` contract + N=1 anchor preserved → Task 1 (delegate) + `replay_multi_unchanged_by_refactor` + existing tests. ✓
- Risk edge cases (<2 returns→0, downside 0→inf, trueDD in [0,100)) → Task 2 impl + tests. ✓
- Drop realized maxDD, show trueDD, keep win% → Task 3 Step 2. ✓
- RISK VERDICT + intuition supported/not → Task 3 Step 3. ✓
- Production grid / live trader untouched; no `.env` var → all tasks additive; only `replay_multi` internals refactored (allowed). ✓
- Caveats (one slice; 184s annualization; crypto co-movement) → Task 3 Step 3 caveat. ✓

**2. Placeholder scan:** No TBD/TODO; every code step has complete code or an exact anchored edit; every test step has real assertions. ✓

**3. Type consistency:** `replay_multi_core(.., record_mtm) -> (SimRun, Vec<(u64,f64)>)`; `replay_multi(..) -> SimRun` (delegate `.0`); `replay_multi_mtm(..) -> (SimRun, Vec<(u64,f64)>)` — used in Task 3 as `let (_, mtm) = replay_multi_mtm(...)`. `RiskMetrics{sharpe,sortino,true_max_dd_pct}` defined in Task 2, fields read in Task 3 (`rm.sharpe`, `rm.sortino`, `rm.true_max_dd_pct`). `risk_metrics(&[(u64,f64)], f64) -> RiskMetrics` defined Task 2, called Task 3. The Task 3 `summary` tuple gains a 4th element `Option<RiskMetrics>` and every destructure (`(n,best,notional,risk)`, `(n1,b1,_,rm1)`, `(nk,bk,_,rmk)`) matches. `RegimeMode`, `regime_mask`, `regime_mask_trend` are in scope (used elsewhere in the file / pub in sim). ✓
