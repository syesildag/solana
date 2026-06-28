# Best-Tuned N=#curated vs N=1 Comparison (`maxn-optimize`) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a grid search at arbitrary N (`run_grid_multi`) and a `maxn-optimize` subcommand that grid-tunes the hold-all-curated portfolio (N = #tokens) and the single-slot trader (N=1) each to its own best robust config at equal total capital, then prints a head-to-head.

**Architecture:** Additive. `run_grid_multi` is a copy of the production `run_grid` whose two inner replays call `replay_multi(..., max_positions)` instead of `replay_with_regime`; at N=1 it reproduces `run_grid` (since `replay_multi(...,1)≡replay_with_regime`, already proven). A `best_robust_by_test` selector picks each N's winner. The `maxn-optimize` CLI runs the grid at N=1 (capital = pool) and N=#curated (capital = pool/N per slot), selects the best robust config of each, and prints the comparison. The production `run_grid`, `replay*`, and the live trader are never modified.

**Tech Stack:** Rust, clap (CLI), rayon (already used by the grid). Tests are `#[cfg(test)]` blocks at the bottom of `src/portfolio/sim.rs`, run with `cargo test --lib` (NOTE: these are LIB tests — `cargo test --bin momentum-sim` shows 0 tests).

## Global Constraints

- **Sim-only:** no changes to the live trader (`src/portfolio/momentum.rs`, `momentum_state.rs`).
- **Production grid untouched:** do NOT modify `run_grid`, `replay`, `replay_with_stream`, `replay_with_regime`. `run_grid_multi` is a NEW function. (The deliberate duplication mirrors the existing `replay_with_regime`/`replay_multi` split — it keeps the perf-sensitive production grid byte-stable.)
- **No new `.env` variable:** `--pool-usdc`, `--max-n`, etc. are CLI flags only.
- **Equal total capital:** each grid run uses `trade_usdc = pool / N`. N=1 → whole pool in one slot; N=#curated → pool/N per slot. Compare absolute held-out (`net_pnl_test`).
- **Fixed notional per slot, no compounding:** set `base.reinvest_frac = 0.0` and `base.size_ceiling_usdc = base.trade_usdc` so each slot is an independent fixed bet (clean equal-capital math). Sweep fixed trails only (no vol stops, no max-trail) — matches what the live trader can reproduce.
- **Objective:** best ROBUST config (profitable in BOTH slices, ≥ `min_trades` each — `config_is_robust`), ranked by highest `net_pnl_test`. If an N has no robust config, report it; never emit a verdict treating a missing side as 0.
- **Rotation moot at N=#curated:** the N=#curated grid uses `rotate_factors = [0.0]` (eviction can't fire when N == token count). The N=1 grid sweeps the caller's `--rotate-factors`.
- **Endpoints:** compare N=1 vs N=`watched.len()` (override upper via `--max-n`). If `watched.len() == 1`, the two coincide — print once, note identical.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/sim.rs` | Add `run_grid_multi` (grid at fixed N) + `best_robust_by_test` selector + tests. | Modify (additive) |
| `src/bin/momentum_sim.rs` | Add `MaxnOptimize` command variant, match arm, `MaxnOptimizeArgs`, `maxn_optimize` printer. | Modify (additive) |

---

## Task 1: `run_grid_multi` + `best_robust_by_test` in sim.rs

**Files:**
- Modify: `src/portfolio/sim.rs` (add both fns after `run_grid`, ~line 1422)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: `ranked_stream`, `replay_multi` (from the prior feature), `stop_variants`, `sizing_variants`, `regime_variants`, `min_metric_candidates`, `regime_mask`, `regime_mask_trend`, `SimResult`, `config_is_robust`, `RankMetric`, `RegimeMode`, `ParamSet`, `WatchedToken`, `PriceSnapshot` — all in scope in `sim.rs`. Test helpers `rise_then_fall`, `aaa`, `bare_params` already exist in the test module.
- Produces:
  - `pub fn run_grid_multi(train, test, watched, base, metrics, lookbacks, max_runs, trails, quantile_probs, rotate_factors, regime_obs_set, regime_trend_obs, atr_ks, sigma_ks, vol_obs_set, max_trails, reinvest_fracs, size_ceiling_mults, max_positions: usize) -> Vec<SimResult>` (same arg list and order as `run_grid`, plus a trailing `max_positions: usize`)
  - `pub fn best_robust_by_test(results: &[SimResult], min_trades: usize) -> Option<&SimResult>`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src/portfolio/sim.rs`:

```rust
    #[test]
    fn run_grid_multi_n1_matches_run_grid() {
        // Anchor: at N=1, run_grid_multi reproduces the production single-slot run_grid
        // row-for-row (replay_multi(...,1) ≡ replay_with_regime is already proven).
        let snaps = rise_then_fall("AAA", 200, 8);
        let watched = aaa();
        let base = bare_params();
        let split = (snaps.len() as f64 * 0.7) as usize;
        let (train, test) = snaps.split_at(split);

        let metrics = [RankMetric::Return];
        let lookbacks = [121usize];
        let max_runs = [0.0f64];
        let trails = [8.0f64, 12.0];
        let quants = [0.5f64, 0.7];
        let rotate = [0.0f64];
        let regime = [0usize];
        let no_f: [f64; 0] = [];
        let no_u: [usize; 0] = [];

        let single = run_grid(
            train, test, &watched, &base, &metrics, &lookbacks, &max_runs, &trails, &quants,
            &rotate, &regime, &no_u, &no_f, &no_f, &no_u, &no_f, &no_f, &no_f,
        );
        let multi = run_grid_multi(
            train, test, &watched, &base, &metrics, &lookbacks, &max_runs, &trails, &quants,
            &rotate, &regime, &no_u, &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, 1,
        );

        assert!(!single.is_empty(), "fixture must produce grid results");
        assert_eq!(single.len(), multi.len(), "same number of grid rows");
        // Compare as multisets keyed by (rounded test P&L, train P&L, trades) to be robust
        // to any tie-ordering differences in the parallel collect.
        let key = |r: &SimResult| (
            (r.net_pnl_test * 1e6).round() as i64,
            (r.net_pnl_train * 1e6).round() as i64,
            r.n_trades_test,
            r.n_trades_train,
        );
        let mut ks: Vec<_> = single.iter().map(key).collect();
        let mut km: Vec<_> = multi.iter().map(key).collect();
        ks.sort();
        km.sort();
        assert_eq!(ks, km, "every single-slot grid row is reproduced at N=1");
    }

    #[test]
    fn run_grid_multi_n2_produces_results() {
        // Smoke: the multi path runs end-to-end through the grid at N=2 on a 2-token
        // history and yields finite, robust-classifiable rows.
        let sol = 150.0;
        let mk = |ts: u64, a: f64, b: f64| {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), a);
            m.insert("BBB".to_string(), b);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None },
            WatchedToken { symbol: "BBB".into(), mint: "BBB".into(), name: None, equity: None },
        ];
        let mut snaps = Vec::new();
        let (mut a, mut b) = (1.0f64, 1.0f64);
        for i in 0..200u64 {
            snaps.push(mk(1000 + i * 180, a, b));
            a *= 1.004;
            b *= 1.003;
        }
        for i in 200..212u64 {
            snaps.push(mk(1000 + i * 180, a * 0.9f64.powi((i - 199) as i32), b * 0.9f64.powi((i - 199) as i32)));
        }
        let base = bare_params();
        let split = (snaps.len() as f64 * 0.7) as usize;
        let (train, test) = snaps.split_at(split);
        let no_f: [f64; 0] = [];
        let no_u: [usize; 0] = [];
        let res = run_grid_multi(
            train, test, &watched, &base, &[RankMetric::Return], &[121usize], &[0.0f64],
            &[8.0f64], &[0.5f64], &[0.0f64], &[0usize], &no_u, &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, 2,
        );
        assert!(!res.is_empty(), "N=2 grid yields rows");
        assert!(res.iter().all(|r| r.net_pnl_test.is_finite() && r.net_pnl_train.is_finite()));
    }

    #[test]
    fn best_robust_by_test_picks_highest_test_pnl_among_robust() {
        let row = |tr: f64, te: f64, ntr: usize, nte: usize| SimResult {
            params: bare_params(),
            net_pnl_train: tr,
            n_trades_train: ntr,
            net_pnl_test: te,
            n_trades_test: nte,
            win_rate_test: 0.0,
            max_dd_test: 0.0,
        };
        // robust (both>0, ≥3 trades each): A test=10, C test=20 ; B not robust (test<0)
        let a = row(5.0, 10.0, 5, 5);
        let b = row(5.0, -1.0, 5, 5);   // test loss → not robust
        let c = row(5.0, 20.0, 5, 5);
        let d = row(5.0, 99.0, 1, 5);   // too few train trades → not robust
        let results = vec![a, b, c, d];
        let best = best_robust_by_test(&results, 3).expect("a robust config exists");
        assert!((best.net_pnl_test - 20.0).abs() < 1e-9, "picks C (highest robust test P&L)");

        // none robust → None
        let none = vec![row(-1.0, 5.0, 5, 5), row(5.0, -1.0, 5, 5)];
        assert!(best_robust_by_test(&none, 3).is_none());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib 'run_grid_multi' 2>&1 | tail -20` and `cargo test --lib best_robust_by_test 2>&1 | tail -10`
Expected: FAIL — `cannot find function 'run_grid_multi'` / `cannot find function 'best_robust_by_test'`.

- [ ] **Step 3: Implement `best_robust_by_test`**

Insert after `run_grid`'s closing brace (~line 1422) in `src/portfolio/sim.rs`:

```rust
/// The robust config with the highest held-out (test) P&L, or `None` if no config is
/// robust. "Robust" = profitable in BOTH slices with ≥ `min_trades` in each
/// (`config_is_robust`). Used to pick each N's winner for the max-N comparison.
pub fn best_robust_by_test(results: &[SimResult], min_trades: usize) -> Option<&SimResult> {
    results
        .iter()
        .filter(|r| r.is_robust(min_trades))
        .max_by(|a, b| {
            a.net_pnl_test
                .partial_cmp(&b.net_pnl_test)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}
```

- [ ] **Step 4: Implement `run_grid_multi`**

Insert immediately after `best_robust_by_test` in `src/portfolio/sim.rs`. This is `run_grid` verbatim with two changes: a trailing `max_positions: usize` parameter, and the two inner `replay_with_regime(...)` calls replaced by `replay_multi(..., max_positions)`:

```rust
/// Like [`run_grid`] but replays each config at `max_positions` concurrent slots via
/// [`replay_multi`] instead of the single-slot `replay_with_regime`. At
/// `max_positions == 1` it reproduces `run_grid` row-for-row (anchor test). The caller
/// sets `base.trade_usdc` (= pool / max_positions) before calling, for equal-capital
/// comparisons across N. Production `run_grid` is intentionally left untouched; the
/// duplication mirrors the existing `replay_with_regime`/`replay_multi` split.
#[allow(clippy::too_many_arguments)]
pub fn run_grid_multi(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    watched: &[WatchedToken],
    base: &ParamSet,
    metrics: &[RankMetric],
    lookbacks: &[usize],
    max_runs: &[f64],
    trails: &[f64],
    quantile_probs: &[f64],
    rotate_factors: &[f64],
    regime_obs_set: &[usize],
    regime_trend_obs: &[usize],
    atr_ks: &[f64],
    sigma_ks: &[f64],
    vol_obs_set: &[usize],
    max_trails: &[f64],
    reinvest_fracs: &[f64],
    size_ceiling_mults: &[f64],
    max_positions: usize,
) -> Vec<SimResult> {
    let variants = stop_variants(trails, atr_ks, sigma_ks, vol_obs_set, max_trails);
    let sizing = sizing_variants(base.trade_usdc, reinvest_fracs, size_ceiling_mults);
    let regime_variants = regime_variants(train, regime_obs_set, regime_trend_obs);
    let regime_masks: Vec<(RegimeMode, usize, f64, Vec<bool>, Vec<bool>)> = regime_variants
        .iter()
        .map(|&(m, o, t)| {
            let mask = |snaps: &[PriceSnapshot]| match m {
                RegimeMode::Off => vec![true; snaps.len()],
                RegimeMode::Level => regime_mask(snaps, o),
                RegimeMode::Trend => regime_mask_trend(snaps, o, t),
            };
            (m, o, t, mask(train), mask(test))
        })
        .collect();

    let tuples: Vec<(RankMetric, usize, f64)> = metrics
        .iter()
        .flat_map(|&m| {
            lookbacks
                .iter()
                .flat_map(move |&l| max_runs.iter().map(move |&mr| (m, l, mr)))
        })
        .collect();

    let mut results: Vec<SimResult> = tuples
        .par_iter()
        .flat_map_iter(|&(metric, lookback, max_run)| {
            let mut rp = base.clone();
            rp.metric = metric;
            rp.lookback_obs = lookback;
            rp.max_run_pct = max_run;

            let train_stream = ranked_stream(train, watched, &rp);
            let test_stream = ranked_stream(test, watched, &rp);

            let train_best_scores: Vec<f64> =
                train_stream.iter().filter_map(|r| r.first().map(|c| c.score)).collect();
            let mins = min_metric_candidates(&train_best_scores, quantile_probs);

            let mut local = Vec::new();
            for v in &variants {
                for &min_metric in &mins {
                    for &rf in rotate_factors {
                        for (rmode, robs, rthr, tr_mask, te_mask) in &regime_masks {
                            for &(reinvest, ceil) in &sizing {
                                let mut p = rp.clone();
                                p.trail_pct = v.trail_pct;
                                p.vol_stop_mode = v.mode;
                                p.chandelier_k = v.k;
                                p.vol_obs = v.vol_obs;
                                p.max_trail_pct = v.max_trail_pct;
                                p.min_metric = min_metric;
                                p.rotate_margin = if rf > 0.0 { rf * min_metric } else { 0.0 };
                                p.regime_mode = *rmode;
                                p.regime_filter_obs = *robs;
                                p.regime_threshold = *rthr;
                                p.reinvest_frac = reinvest;
                                p.size_ceiling_usdc = ceil;
                                let tr = replay_multi(train, watched, &train_stream, &p, tr_mask, max_positions);
                                let te = replay_multi(test, watched, &test_stream, &p, te_mask, max_positions);
                                local.push(SimResult {
                                    params: p,
                                    net_pnl_train: tr.net_pnl(),
                                    n_trades_train: tr.n_trades(),
                                    net_pnl_test: te.net_pnl(),
                                    n_trades_test: te.n_trades(),
                                    win_rate_test: te.win_rate(),
                                    max_dd_test: te.max_drawdown_pct(),
                                });
                            }
                        }
                    }
                }
            }
            local
        })
        .collect();
    results.sort_by(|a, b| {
        b.net_pnl_test
            .partial_cmp(&a.net_pnl_test)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --lib 'run_grid_multi' 2>&1 | tail -15` then `cargo test --lib best_robust_by_test 2>&1 | tail -8`
Expected: PASS — `run_grid_multi_n1_matches_run_grid`, `run_grid_multi_n2_produces_results`, `best_robust_by_test_picks_highest_test_pnl_among_robust`.
Then full suite: `cargo test --lib 2>&1 | tail -4` — no regressions.

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): run_grid_multi (grid at fixed N) + best_robust_by_test

Grid-searches each config at max_positions concurrent slots via replay_multi;
reproduces run_grid row-for-row at N=1 (anchor test). best_robust_by_test picks
the highest-test-P&L config among robust ones. Production run_grid untouched.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `maxn-optimize` CLI subcommand

**Files:**
- Modify: `src/bin/momentum_sim.rs` (Command variant ~after `MaxnCompare`; match arm ~after the `MaxnCompare` arm; `MaxnOptimizeArgs` + `maxn_optimize` ~after `maxn_compare`)

**Interfaces:**
- Consumes: `sim::run_grid_multi`, `sim::best_robust_by_test`, `sim::SimResult`, `base_params`, `sim::sanitize_history`, `history::load_history`, `momentum_universe::load`, `sim::{GRID_METRICS, GRID_LOOKBACKS, GRID_MAX_RUNS, GRID_TRAILS, GRID_MIN_QUANTILES}`, `RankMetric`, `PortfolioConfig`.
- Produces: a runnable `momentum-sim maxn-optimize ...` subcommand.

- [ ] **Step 1: Add the `MaxnOptimize` command variant**

In `src/bin/momentum_sim.rs`, inside `enum Command { ... }`, after the `MaxnCompare { ... }` variant, add:

```rust
    /// Grid-tune the hold-all-curated portfolio (N = #tokens) and the single-slot trader
    /// (N=1) each to its own best ROBUST config at EQUAL total capital (trade_usdc =
    /// pool/N), then print a head-to-head. Answers: does a best-tuned basket beat a
    /// best-tuned single name on the same money? Fixed-trail only (live-reproducible).
    MaxnOptimize {
        /// Total capital both models compete for. Default: live MOMENTUM_TRADE_USDC.
        #[arg(long)]
        pool_usdc: Option<f64>,
        /// Upper endpoint N to compare against N=1. Default: number of curated tokens.
        #[arg(long)]
        max_n: Option<usize>,
        /// Robustness gate: a config must trade ≥ this in BOTH slices to be eligible.
        #[arg(long, default_value_t = 3)]
        min_trades: usize,
        /// Rotation factors swept for the N=1 grid (× min_metric; 0 = off). The N=#curated
        /// grid forces [0.0] (rotation is moot when N == token count).
        #[arg(long, value_delimiter = ',', default_value = "0.0")]
        rotate_factors: Vec<f64>,
        /// Level regime-gate MA windows to sweep (0 = off). e.g. 0,480
        #[arg(long, value_delimiter = ',', default_value = "0,480")]
        regime_obs: Vec<usize>,
        /// Trend regime-gate windows to sweep (thresholds from train quantiles). e.g. 480
        #[arg(long, value_delimiter = ',', default_value = "480")]
        regime_trend_obs: Vec<usize>,
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
    },
```

- [ ] **Step 2: Add the match arm**

In `fn main()`, after the `Command::MaxnCompare { ... } => { ... }` arm, add:

```rust
        Command::MaxnOptimize {
            pool_usdc, max_n, min_trades, rotate_factors, regime_obs, regime_trend_obs,
            train_frac, tokens, history, max_step,
        } => maxn_optimize(MaxnOptimizeArgs {
            cfg: &cfg, pool_usdc, max_n, min_trades, rotate_factors, regime_obs,
            regime_trend_obs, train_frac, tokens, history_override: history, max_step,
        }),
```

- [ ] **Step 3: Add `MaxnOptimizeArgs` + `maxn_optimize`**

After the `maxn_compare` function (before `fn base_params`) in `src/bin/momentum_sim.rs`, add:

```rust
struct MaxnOptimizeArgs<'a> {
    cfg: &'a PortfolioConfig,
    pool_usdc: Option<f64>,
    max_n: Option<usize>,
    min_trades: usize,
    rotate_factors: Vec<f64>,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    train_frac: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
}

/// Format a winning config's params into one human line.
fn fmt_cfg(r: &sim::SimResult) -> String {
    let p = &r.params;
    format!(
        "metric={} min={:.4} trail={}% lookback={} max_run={} regime={}@{} rotate={:.4}",
        p.metric, p.min_metric, p.trail_pct, p.lookback_obs, p.max_run_pct,
        p.regime_mode, p.regime_filter_obs, p.rotate_margin,
    )
}

/// Grid-tune N=1 and N=#curated each to its best robust config at equal total capital,
/// then print the head-to-head. Fixed-trail only (no vol/max-trail/compounding) so the
/// winner is reproducible by the live trader.
fn maxn_optimize(a: MaxnOptimizeArgs) -> Result<()> {
    let MaxnOptimizeArgs {
        cfg, pool_usdc, max_n, min_trades, rotate_factors, regime_obs, regime_trend_obs,
        train_frac, tokens, history_override, max_step,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?
        .into_iter()
        .collect();
    let snapshots = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snapshots.len() >= 200, "only {} snapshots — need more history", snapshots.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let k_tokens = watched.len();
    anyhow::ensure!(k_tokens >= 1, "no curated tokens");
    let pool = pool_usdc.unwrap_or(cfg.momentum_trade_usdc);
    anyhow::ensure!(pool > 0.0, "--pool-usdc must be > 0");
    let upper = max_n.unwrap_or(k_tokens).max(1);

    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);

    // Endpoints: always N=1; add the upper endpoint when it differs.
    let n_values: Vec<usize> = if upper <= 1 { vec![1] } else { vec![1, upper] };

    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    println!("Best-tuned hold-all vs single-slot — pool ${pool} (equal total capital)");
    println!(
        "Loaded {} snapshots (max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens. min_trades={min_trades}\n",
        snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), k_tokens
    );

    let no_f: [f64; 0] = [];
    let no_u: [usize; 0] = [];
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
        println!("Only one endpoint (watched.len()==1) — N=1 and N=#curated coincide.");
    }
    println!(
        "\nCaveat: one held-out slice (~{:.0}d) — suggestive, not proven. Fixed-trail, equal-capital backtest.",
        span_days(test)
    );
    Ok(())
}
```

- [ ] **Step 4: Build and smoke-test**

Run:
```bash
cargo build --release --bin momentum-sim 2>&1 | tail -5
target/release/momentum-sim maxn-optimize --pool-usdc 8000 --min-trades 3
```
Expected: builds clean (no new warnings); prints the pool/split header, an `N=1 (single slot, $8000.00/slot)` block with its best-robust config + perf, an `N=8 (hold 8, $1000.00/slot)` block, and a `VERDICT:` line. If an endpoint has no robust config it prints "no robust config at N=…" and the verdict is "inconclusive". Capture the actual output in the report.

Then confirm no regressions: `cargo test --lib 2>&1 | tail -5` (LIB tests — not `--bin`).

- [ ] **Step 5: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(sim): maxn-optimize — best-tuned hold-all vs single-slot at equal capital

Grid-tunes N=1 (pool/1 per slot) and N=#curated (pool/N per slot) each to its
best robust config, prints head-to-head + verdict on held-out P&L. Rotation
swept only at N=1 (moot at N=#tokens). Sim-only; production grid untouched.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:**
- Equal total capital (`trade_usdc = pool/N`) → Task 2 Step 3 (`base.trade_usdc = pool / n`). ✓
- Two endpoints N∈{1, watched.len()}, `--max-n` override → Task 2 (`n_values`, `upper`). ✓
- Best robust by test P&L → Task 1 `best_robust_by_test`; Task 2 uses it. ✓
- Additive `run_grid_multi`, N=1 ≡ run_grid anchor → Task 1 (`run_grid_multi_n1_matches_run_grid`). ✓
- Rotation moot at N=#curated → Task 2 (`rf = if n==1 {flag} else {[0.0]}`). ✓
- Fixed notional, no compounding, fixed-trail only → Task 2 (`reinvest_frac=0`, `size_ceiling=trade_usdc`, empty atr/sigma/vol/max_trail). ✓
- Edge: K==1 coincide → Task 2 (`n_values` single element + message). ✓
- Edge: no robust config → Task 2 ("no robust config at N=…" + "inconclusive" verdict). ✓
- Edge: thin slots eaten by cost gate → handled implicitly (grid's existing `max_cost_bps` gate; surfaces as "no robust config"). ✓
- Production grid / live trader untouched → Task 1 adds a new fn; Task 2 is bin-only. ✓

**2. Placeholder scan:** No TBD/TODO; every code step has complete code; every test step has real assertions. The output sketch uses `{:+.2}`-style format specifiers (real code), not prose placeholders. ✓

**3. Type consistency:** `run_grid_multi` arg list matches `run_grid` + trailing `max_positions: usize`, and the Task 2 call passes args in that exact order with the `max_positions` last. `best_robust_by_test(results, min_trades) -> Option<&SimResult>` is defined in Task 1 and called (`.cloned()`) in Task 2. `SimResult` fields (`params, net_pnl_train, n_trades_train, net_pnl_test, n_trades_test, win_rate_test, max_dd_test`) used consistently in both tasks. `MaxnOptimizeArgs` fields match the Command variant and the match arm. `fmt_cfg` reads `params` fields that exist on `ParamSet` (`metric, min_metric, trail_pct, lookback_obs, max_run_pct, regime_mode, regime_filter_obs, rotate_margin`), all of which impl Display (metric, regime_mode) or are numeric. ✓
