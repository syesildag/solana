# Per-Token Tuning + Validation (SP2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** A `per-token-tune` subcommand that computes each token's best `{min_metric, trail_pct, max_run_pct}` (single-name grid at the global metric/lookback), runs a 3-arm equal-capital validation (single-slot global vs hold-all global vs hold-all per-token) with risk metrics, prints the verdict that gates SP3, and (with `--apply`) persists per-token params into `momentum_tokens.json`.

**Architecture:** `tune_per_token` (sim.rs) reuses `run_grid_multi` on single-token universes (regime off, fixed global metric/lookback, sweeping trail×max_run×min_metric) + `best_robust_by_test`. The CLI orchestrates the global grid (Arms A/B), per-token tuning, in-memory Arm C, risk-metrics each via `replay_multi_mtm`+`risk_metrics`, prints, and optionally writes the JSON.

**Tech Stack:** Rust. Tests: `cargo test --lib` (LIB tests, not `--bin`).

## Global Constraints

- **Sim only.** No live trader, no `.env` write (global→.env stays with optimize-momentum-config). Do NOT modify production `run_grid`/`replay*`.
- Per-token tuning sweeps only `{min_metric, trail_pct, max_run_pct}`; metric/lookback fixed at the global best's; **regime OFF during per-token tuning** (simplification — token knobs are regime-independent; validation applies global regime uniformly).
- Per-token best = `best_robust_by_test` (profitable both slices, ≥min_trades) in a single-token N=1 grid; **no robust config ⇒ no override** (global fallback).
- Equal capital: `pool` (default `.env momentum_trade_usdc`, `--pool-usdc`); Arm A `trade_usdc=pool`; Arms B/C `trade_usdc=pool/K`.
- `--apply` writes per-token params by mint into `momentum_tokens.json`, preserving all entries + other fields; the result must re-parse via `momentum_universe::load`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/sim.rs` | `PerTokenBest` + `tune_per_token` + test | Modify (additive) |
| `src/bin/momentum_sim.rs` | `PerTokenTune` command + `per_token_tune` + `write_token_params` | Modify (additive) |

---

## Task 1: `tune_per_token` core

**Files:**
- Modify: `src/portfolio/sim.rs` (add after `best_robust_by_test`)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `run_grid_multi`, `best_robust_by_test`, `GRID_TRAILS`, `GRID_MAX_RUNS`, `GRID_MIN_QUANTILES`, `WatchedToken`, `TokenParams`, `ParamSet`.
- Produces:
  - `pub struct PerTokenBest { pub mint: String, pub symbol: String, pub params: Option<TokenParams>, pub test_pnl: f64 }`
  - `pub fn tune_per_token(train: &[PriceSnapshot], test: &[PriceSnapshot], watched: &[WatchedToken], base: &ParamSet, min_trades: usize) -> Vec<PerTokenBest>`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/sim.rs`:

```rust
    #[test]
    fn tune_per_token_picks_per_token_best_and_none_when_no_edge() {
        // Token GUD: rises then has small pullbacks → a robust single-name config exists.
        // Token BAD: pure noise/decline → no robust config → None (global fallback).
        let sol = 150.0;
        let mk = |ts: u64, g: f64, b: f64| {
            let mut m = std::collections::HashMap::new();
            m.insert("GUD".to_string(), g);
            m.insert("BAD".to_string(), b);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let mut snaps = Vec::new();
        let (mut g, mut b) = (1.0f64, 1.0f64);
        for i in 0..260u64 {
            // GUD: steady rise with periodic 6% dips that recover (gives entries + stops)
            g *= if i % 20 == 19 { 0.94 } else { 1.01 };
            b *= 0.999; // BAD: steady bleed → never a profitable long
            snaps.push(mk(1000 + i * 180, g, b));
        }
        let watched = vec![
            WatchedToken { symbol: "GUD".into(), mint: "GUD".into(), name: None, equity: None, params: None },
            WatchedToken { symbol: "BAD".into(), mint: "BAD".into(), name: None, equity: None, params: None },
        ];
        let mut base = bare_params();
        base.metric = RankMetric::Return;
        base.lookback_obs = 121;
        let split = (snaps.len() as f64 * 0.6) as usize;
        let (train, test) = snaps.split_at(split);

        let res = tune_per_token(train, test, &watched, &base, 1);
        assert_eq!(res.len(), 2);
        let gud = res.iter().find(|r| r.mint == "GUD").unwrap();
        let bad = res.iter().find(|r| r.mint == "BAD").unwrap();
        // GUD: a robust config may or may not exist on this synthetic path, but if Some,
        // the params carry all three fields; BAD (pure bleed) must have no robust edge.
        assert!(bad.params.is_none(), "a steadily-bleeding token has no robust long edge");
        if let Some(p) = &gud.params {
            assert!(p.min_metric.is_some() && p.trail_pct.is_some() && p.max_run_pct.is_some(),
                "a tuned token carries all three override fields");
        }
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib tune_per_token 2>&1 | tail -12`
Expected: FAIL — `cannot find function 'tune_per_token'`.

- [ ] **Step 3: Implement `PerTokenBest` + `tune_per_token`**

Insert after `best_robust_by_test` in `src/portfolio/sim.rs`:

```rust
/// One token's best per-token override (or `None` if no robust single-name config), plus
/// its isolated held-out P&L. Produced by [`tune_per_token`].
#[derive(Debug, Clone)]
pub struct PerTokenBest {
    pub mint: String,
    pub symbol: String,
    pub params: Option<TokenParams>,
    pub test_pnl: f64,
}

/// For each watched token, grid-search its best `{min_metric, trail_pct, max_run_pct}` in
/// isolation (single-token universe, N=1), with metric/lookback fixed at `base`'s and
/// regime OFF (token knobs are regime-independent; the caller applies the global regime in
/// validation). Sweeps `GRID_TRAILS × GRID_MAX_RUNS × GRID_MIN_QUANTILES` (fixed-trail
/// only). Returns the best-robust override per token (`None` when no robust config).
pub fn tune_per_token(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    watched: &[WatchedToken],
    base: &ParamSet,
    min_trades: usize,
) -> Vec<PerTokenBest> {
    let no_f: [f64; 0] = [];
    let no_u: [usize; 0] = [];
    watched
        .iter()
        .map(|w| {
            // Single-token universe with overrides stripped, so the grid's swept values
            // are what's evaluated (not any pre-existing per-token override).
            let single = vec![WatchedToken {
                symbol: w.symbol.clone(),
                mint: w.mint.clone(),
                name: w.name.clone(),
                equity: w.equity,
                params: None,
            }];
            let mut b = base.clone();
            b.reinvest_frac = 0.0;
            b.size_ceiling_usdc = b.trade_usdc;
            let results = run_grid_multi(
                train, test, &single, &b,
                &[base.metric], &[base.lookback_obs], &GRID_MAX_RUNS, &GRID_TRAILS,
                &GRID_MIN_QUANTILES, &[0.0_f64], &[0usize], &no_u, // regime off (obs=[0]); no trend
                &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, 1,
            );
            match best_robust_by_test(&results, min_trades) {
                Some(r) => PerTokenBest {
                    mint: w.mint.clone(),
                    symbol: w.symbol.clone(),
                    params: Some(TokenParams {
                        min_metric: Some(r.params.min_metric),
                        trail_pct: Some(r.params.trail_pct),
                        max_run_pct: Some(r.params.max_run_pct),
                    }),
                    test_pnl: r.net_pnl_test,
                },
                None => PerTokenBest {
                    mint: w.mint.clone(),
                    symbol: w.symbol.clone(),
                    params: None,
                    test_pnl: 0.0,
                },
            }
        })
        .collect()
}
```

> Ensure `TokenParams` is imported in `sim.rs` (add to the existing `use super::momentum_universe::{…}` line if not already present from SP1).

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib tune_per_token 2>&1 | tail -10` then `cargo test --lib 2>&1 | tail -4`
Expected: PASS; no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): tune_per_token — per-token best {min_metric,trail,max_run} grid

Single-token N=1 grid at fixed global metric/lookback, regime off; best-robust-by-test
per token, None when no robust single-name config. Basis for the per-token basket.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: `per-token-tune` CLI (validation + verdict + `--apply`)

**Files:**
- Modify: `src/bin/momentum_sim.rs` (Command variant after `MaxnOptimize`; match arm; `PerTokenTuneArgs` + `per_token_tune` + `write_token_params` after `maxn_optimize`)

**Interfaces:**
- Consumes: `sim::tune_per_token`, `sim::PerTokenBest`, `sim::run_grid_multi`, `sim::best_robust_by_test`, `sim::replay_multi`, `sim::replay_multi_mtm`, `sim::risk_metrics`, `base_params`, `sim::GRID_*`, `RegimeMode`, `sim::regime_mask`/`regime_mask_trend`, `momentum_universe::{WatchedToken, TokenParams}`.
- Produces: runnable `momentum-sim per-token-tune [...] [--apply]`.

- [ ] **Step 1: Add the `PerTokenTune` command variant**

In `src/bin/momentum_sim.rs`, after the `MaxnOptimize { … }` variant in `enum Command`:

```rust
    /// Compute each token's best {min_metric,trail,max_run} (single-name grid at the global
    /// metric/lookback), run a 3-arm equal-capital validation (single-slot global vs
    /// hold-all global vs hold-all per-token) with risk metrics, and print the verdict.
    /// --apply persists per-token params into momentum_tokens.json.
    PerTokenTune {
        #[arg(long)]
        pool_usdc: Option<f64>,
        #[arg(long, default_value_t = 3)]
        min_trades: usize,
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        #[arg(long, value_delimiter = ',', default_value = "0,480")]
        regime_obs: Vec<usize>,
        #[arg(long, value_delimiter = ',', default_value = "480")]
        regime_trend_obs: Vec<usize>,
        /// Also write the computed per-token params into momentum_tokens.json.
        #[arg(long, default_value_t = false)]
        apply: bool,
    },
```

- [ ] **Step 2: Add the match arm**

After the `Command::MaxnOptimize { … } => …` arm in `fn main()`:

```rust
        Command::PerTokenTune {
            pool_usdc, min_trades, train_frac, tokens, history, max_step,
            regime_obs, regime_trend_obs, apply,
        } => per_token_tune(PerTokenTuneArgs {
            cfg: &cfg, pool_usdc, min_trades, train_frac, tokens,
            history_override: history, max_step, regime_obs, regime_trend_obs, apply,
        }),
```

- [ ] **Step 3: Add `PerTokenTuneArgs`, `write_token_params`, and `per_token_tune`**

After `maxn_optimize` (before `fn base_params`) in `src/bin/momentum_sim.rs`:

```rust
struct PerTokenTuneArgs<'a> {
    cfg: &'a PortfolioConfig,
    pool_usdc: Option<f64>,
    min_trades: usize,
    train_frac: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    apply: bool,
}

/// Merge per-token params into a tokens JSON file by mint, preserving all entries and
/// other fields. Reads the RAW (unfiltered) array so USDC/invalid entries aren't dropped.
fn write_token_params(
    path: &str,
    overrides: &std::collections::HashMap<String, momentum_universe::TokenParams>,
) -> Result<usize> {
    let data = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let mut toks: Vec<momentum_universe::WatchedToken> =
        serde_json::from_str(&data).with_context(|| format!("parsing {path}"))?;
    let mut n = 0;
    for t in toks.iter_mut() {
        if let Some(p) = overrides.get(&t.mint) {
            t.params = Some(p.clone());
            n += 1;
        }
    }
    let out = serde_json::to_string_pretty(&toks)? + "\n";
    std::fs::write(path, out).with_context(|| format!("writing {path}"))?;
    Ok(n)
}

/// Build the per-snapshot regime mask for a config's own regime params.
fn regime_mask_for(slice: &[history::PriceSnapshot], p: &ParamSet) -> Vec<bool> {
    match p.regime_mode {
        RegimeMode::Off => vec![true; slice.len()],
        RegimeMode::Level => sim::regime_mask(slice, p.regime_filter_obs),
        RegimeMode::Trend => sim::regime_mask_trend(slice, p.regime_filter_obs, p.regime_threshold),
    }
}

fn per_token_tune(a: PerTokenTuneArgs) -> Result<()> {
    let PerTokenTuneArgs {
        cfg, pool_usdc, min_trades, train_frac, tokens, history_override, max_step,
        regime_obs, regime_trend_obs, apply,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");
    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?.into_iter().collect();
    let snapshots = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snapshots.len() >= 200, "only {} snapshots — need more history", snapshots.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let k = watched.len();
    anyhow::ensure!(k >= 1, "no curated tokens");
    let pool = pool_usdc.unwrap_or(cfg.momentum_trade_usdc);
    anyhow::ensure!(pool > 0.0, "--pool-usdc must be > 0");
    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);
    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    let periods_per_year = 365.0 * 86_400.0 / 184.0;
    let no_f: [f64; 0] = [];
    let no_u: [usize; 0] = [];

    println!("Per-token tuning — pool ${pool}, K={k} tokens. Train ~{:.0}d / Test ~{:.0}d. min_trades={min_trades}",
        span_days(train), span_days(test));

    // ── Global grid → Arm A (N=1 best) and Arm B (N=K best) ──
    let global_grid = |n: usize, td: f64| -> Option<sim::SimResult> {
        let mut base = base_params(cfg);
        base.trade_usdc = td;
        base.size_ceiling_usdc = td;
        base.reinvest_frac = 0.0;
        let rf = if n == 1 { vec![0.0_f64] } else { vec![0.0_f64] };
        let results = sim::run_grid_multi(
            train, test, &watched, &base,
            &sim::GRID_METRICS, &sim::GRID_LOOKBACKS, &sim::GRID_MAX_RUNS, &sim::GRID_TRAILS,
            &sim::GRID_MIN_QUANTILES, &rf, &regime_obs, &regime_trend_obs,
            &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, n,
        );
        best_robust_by_test_owned(&results, min_trades)
    };
    let arm_a = global_grid(1, pool);
    let arm_b = global_grid(k, pool / k as f64);

    // The global single-name best (Arm A) fixes metric/lookback/regime for per-token tuning.
    let Some(ref ga) = arm_a else {
        println!("\nNo robust single-slot (N=1) config — cannot establish a global baseline. Stopping.");
        return Ok(());
    };
    println!("Global best (single-name, Arm A): metric={} lookback={} regime={}@{} min={:.4} trail={}% max_run={}",
        ga.params.metric, ga.params.lookback_obs, ga.params.regime_mode, ga.params.regime_filter_obs,
        ga.params.min_metric, ga.params.trail_pct, ga.params.max_run_pct);

    // ── Per-token tuning (metric/lookback from Arm A; regime off inside tune_per_token) ──
    let mut tune_base = base_params(cfg);
    tune_base.metric = ga.params.metric;
    tune_base.lookback_obs = ga.params.lookback_obs;
    tune_base.trade_usdc = pool / k as f64; // per-slot notional for isolated grids
    let per_token = sim::tune_per_token(train, test, &watched, &tune_base, min_trades);

    println!("\nPer-token best {{min_metric, trail, max_run}} (single-name grid, isolated test P&L):");
    let mut overrides: std::collections::HashMap<String, momentum_universe::TokenParams> = Default::default();
    for pt in &per_token {
        match &pt.params {
            Some(p) => {
                println!("  {:<6} min={:.4} trail={}% max_run={}   test {:+.2}",
                    pt.symbol, p.min_metric.unwrap(), p.trail_pct.unwrap(), p.max_run_pct.unwrap(), pt.test_pnl);
                overrides.insert(pt.mint.clone(), p.clone());
            }
            None => println!("  {:<6} (no robust single-name config → global fallback)", pt.symbol),
        }
    }

    // ── Arm C: hold-all with per-token overrides applied in-memory ──
    // Config = Arm A's metric/lookback/regime + global threshold/trail/max_run as fallback;
    // per-token overrides win for tokens that have them.
    let mut c_params = ga.params.clone();
    c_params.trade_usdc = pool / k as f64;
    c_params.size_ceiling_usdc = c_params.trade_usdc;
    c_params.reinvest_frac = 0.0;
    let watched_c: Vec<momentum_universe::WatchedToken> = watched.iter().map(|w| {
        let mut w2 = w.clone();
        w2.params = overrides.get(&w.mint).cloned();
        w2
    }).collect();
    let stream_c = sim::ranked_stream(test, &watched_c, &c_params);
    let mask_c = regime_mask_for(test, &c_params);
    let (run_c, mtm_c) = sim::replay_multi_mtm(test, &watched_c, &stream_c, &c_params, &mask_c, k);
    let risk_c = sim::risk_metrics(&mtm_c, periods_per_year);

    // Risk for arms A and B (replay their best configs on test with MTM).
    let arm_risk = |best: &Option<sim::SimResult>, n: usize| -> Option<(f64, sim::RiskMetrics)> {
        best.as_ref().map(|r| {
            let stream = sim::ranked_stream(test, &watched, &r.params);
            let mask = regime_mask_for(test, &r.params);
            let (run, mtm) = sim::replay_multi_mtm(test, &watched, &stream, &r.params, &mask, n);
            (run.net_pnl(), sim::risk_metrics(&mtm, periods_per_year))
        })
    };
    let a = arm_risk(&arm_a, 1);
    let b = arm_risk(&arm_b, k);

    println!("\n3-arm validation (equal ${pool}, held-out test):");
    println!("  {:<28} {:>10} {:>8} {:>8}", "arm", "test P&L", "Sharpe", "trueDD");
    if let Some((pnl, rm)) = &a {
        println!("  {:<28} {:>+10.2} {:>8.2} {:>7.1}%", "A single-slot (global)", pnl, rm.sharpe, rm.true_max_dd_pct);
    }
    if let Some((pnl, rm)) = &b {
        println!("  {:<28} {:>+10.2} {:>8.2} {:>7.1}%", "B hold-all (global cfg)", pnl, rm.sharpe, rm.true_max_dd_pct);
    } else {
        println!("  {:<28} {:>10}", "B hold-all (global cfg)", "no robust");
    }
    println!("  {:<28} {:>+10.2} {:>8.2} {:>7.1}%", "C hold-all (per-token)", run_c.net_pnl(), risk_c.sharpe, risk_c.true_max_dd_pct);

    // ── Verdict (the SP3 gate): does per-token (C) beat single-slot (A)? ──
    if let Some((a_pnl, a_rm)) = &a {
        let c_pnl = run_c.net_pnl();
        let pnl_win = c_pnl > *a_pnl;
        let sharpe_win = risk_c.sharpe > a_rm.sharpe;
        let verdict = if pnl_win && sharpe_win {
            "SUPPORTED — per-token hold-all beats single-slot on BOTH P&L and Sharpe"
        } else if pnl_win || sharpe_win {
            "MIXED — per-token hold-all wins one of {P&L, Sharpe} vs single-slot"
        } else {
            "NOT SUPPORTED — single-slot still dominates per-token hold-all"
        };
        println!("\nVERDICT (SP3 gate): {verdict}.");
        println!("  C vs A — P&L {:+.2} vs {:+.2} (Δ {:+.2}); Sharpe {:.2} vs {:.2}; trueDD {:.1}% vs {:.1}%.",
            c_pnl, a_pnl, c_pnl - a_pnl, risk_c.sharpe, a_rm.sharpe, risk_c.true_max_dd_pct, a_rm.true_max_dd_pct);
    }
    println!("\nCaveat: one held-out slice; per-token tuned with regime off; crypto names co-move. Suggestive, not proven.");

    if apply {
        let n = write_token_params(&tokens_path, &overrides)?;
        println!("\n--apply: wrote per-token params for {n} token(s) to {tokens_path}.");
    } else {
        println!("\n(preview only — re-run with --apply to write per-token params into {tokens_path})");
    }
    Ok(())
}

/// `best_robust_by_test` returning an owned clone (the borrow can't outlive the local
/// `results` in the global_grid closure).
fn best_robust_by_test_owned(results: &[sim::SimResult], min_trades: usize) -> Option<sim::SimResult> {
    best_robust_by_test(results, min_trades).cloned()
}
```

> Add `use solana_mev::portfolio::sim::best_robust_by_test;` or call it as `sim::best_robust_by_test` — match the file's existing import style (it already uses `sim::` paths and imports `SimResult`). If `best_robust_by_test` isn't imported, use `sim::best_robust_by_test` directly and drop the `_owned` helper in favor of inline `sim::best_robust_by_test(&results, min_trades).cloned()`.

- [ ] **Step 4: Build and smoke-test**

Run:
```bash
cargo build --release --bin momentum-sim 2>&1 | tail -5
target/release/momentum-sim per-token-tune --pool-usdc 8000 --min-trades 3
```
Expected: clean build; prints the global best line, the per-token table (each token's chosen `{min_metric,trail,max_run}` or "global fallback"), the 3-arm table (A/B/C with P&L/Sharpe/trueDD), and a `VERDICT (SP3 gate):` line. Capture the full output in the report — **this is the gate input.** Then confirm no `--apply` was needed for the verdict; optionally test `--apply` writes (on a COPY, or verify it re-parses): after `--apply`, run `target/release/momentum-sim per-token-tune --pool-usdc 8000` again to confirm the file still loads. Then `cargo test --lib 2>&1 | tail -4`.

- [ ] **Step 5: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(sim): per-token-tune subcommand — per-token params + 3-arm validation (SP3 gate)

Computes each token's best {min_metric,trail,max_run}; runs single-slot-global vs
hold-all-global vs hold-all-per-token at equal capital with risk metrics; prints the
verdict gating the multi-position live trader. --apply writes momentum_tokens.json.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:** per-token best via single-name grid (T1 `tune_per_token`, regime off, fixed metric/lookback); 3-arm validation + verdict (T2); `--apply` writer preserving entries (T2 `write_token_params`); equal capital (pool/N per arm); global→.env out of scope. ✓
**2. Placeholders:** complete code throughout; the import-style note is an instruction (pick the compiling path), not a gap. ✓
**3. Type consistency:** `PerTokenBest{mint,symbol,params:Option<TokenParams>,test_pnl}` (T1) consumed in T2's per-token loop; `tune_per_token(train,test,watched,base,min_trades)->Vec<PerTokenBest>` called in T2; `TokenParams{min_metric,trail_pct,max_run_pct:Option<f64>}` built in T1, written by `write_token_params` (T2); `run_grid_multi` arg order matches the SP-maxn-optimize signature (+ trailing max_positions). ✓
