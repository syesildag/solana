# Portfolio Analyzer — Academic Risk Metrics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace fixed-threshold portfolio alerts with EWMA z-score signals and add a `RiskReport` (volatility, drawdown, EUR-denominated loss) surfaced in the console, email, and CLI.

**Architecture:** `compute_risk()` iterates the price history once per asset to produce EWMA state and drawdown metrics; the result is a `RiskReport` (pure metrics, no alerts). `analyze()` receives the pre-computed `RiskReport` and emits `ZScoreSpike` alerts alongside the existing 5m/1h/7d signals. The watcher calls both each tick; the CLI loads history before calling `compute_risk()`.

**Tech Stack:** Rust std (no new dependencies) — `f64::ln`, `VecDeque`, existing `PriceSnapshot` / `Portfolio` types.

---

## File Map

| File | Change |
|---|---|
| `src/portfolio/analyzer.rs` | Add `AssetRisk`, `RiskReport`, `EwmaState`; add `ZScoreSpike` variant + Display; update `AnalysisConfig`; add `compute_risk()` + helpers; update `analyze()` signature; add 7 unit tests |
| `src/portfolio/mod.rs` | Add `zscore_lambda`, `zscore_threshold`, `zscore_min_obs` to `PortfolioConfig` |
| `src/portfolio/watcher.rs` | Call `compute_risk()` each tick; add `log_risk_report()`; update `build_email()` with risk section; update `analyze()` call |
| `src/bin/portfolio_cli.rs` | Extend `Show` to load history + fetch EUR rate + print risk table |

---

## Task 1: New Types, Config Fields, and AlertKind Variant

**Files:**
- Modify: `src/portfolio/analyzer.rs`
- Modify: `src/portfolio/mod.rs`

- [ ] **Step 1: Add three fields to `PortfolioConfig` in `src/portfolio/mod.rs`**

  Add after `alert_cooldown_min` (line 22):
  ```rust
  pub zscore_lambda: f64,
  pub zscore_threshold: f64,
  pub zscore_min_obs: usize,
  ```

- [ ] **Step 2: Load the new fields from env in `PortfolioConfig::from_env()` in `src/portfolio/mod.rs`**

  Add after the `alert_cooldown_min` parse block (before `alert_email`):
  ```rust
  zscore_lambda: std::env::var("ALERT_ZSCORE_LAMBDA")
      .unwrap_or_else(|_| "0.97".to_string())
      .parse()
      .context("ALERT_ZSCORE_LAMBDA must be a float")?,
  zscore_threshold: std::env::var("ALERT_ZSCORE_THRESHOLD")
      .unwrap_or_else(|_| "2.5".to_string())
      .parse()
      .context("ALERT_ZSCORE_THRESHOLD must be a float")?,
  zscore_min_obs: std::env::var("ALERT_ZSCORE_MIN_OBS")
      .unwrap_or_else(|_| "30".to_string())
      .parse()
      .context("ALERT_ZSCORE_MIN_OBS must be a number")?,
  ```

- [ ] **Step 3: Add `ZScoreSpike` to `AlertKind` in `src/portfolio/analyzer.rs`**

  Extend the enum:
  ```rust
  pub enum AlertKind {
      BigMove5m { pct: f64 },
      BigMove1h { pct: f64 },
      New7dHigh { prev_high: f64 },
      New7dLow { prev_low: f64 },
      ZScoreSpike { z: f64, threshold: f64, return_pct: f64 },
  }
  ```

  Add the Display arm inside `impl fmt::Display for AlertKind`:
  ```rust
  AlertKind::ZScoreSpike { z, return_pct, .. } => {
      write!(f, "z-score spike: z={:+.2} ({:+.2}% return)", z, return_pct)
  }
  ```

- [ ] **Step 4: Update `AnalysisConfig` in `src/portfolio/analyzer.rs`**

  Replace the struct:
  ```rust
  pub struct AnalysisConfig {
      pub alert_pct_5m: f64,
      pub alert_pct_1h: f64,
      pub zscore_lambda: f64,
      pub zscore_threshold: f64,
      pub zscore_min_obs: usize,
  }
  ```

- [ ] **Step 5: Add `AssetRisk`, `RiskReport`, and private `EwmaState` to `src/portfolio/analyzer.rs`**

  Insert before the `pub fn analyze(` line:
  ```rust
  #[derive(Debug, Clone)]
  pub struct AssetRisk {
      pub symbol: String,
      pub z_score: Option<f64>,
      pub sigma_ann: Option<f64>,       // annualized vol as percentage (e.g. 82.4 for 82.4%)
      pub current_drawdown_pct: f64,    // ≤ 0
      pub max_drawdown_pct: f64,        // ≤ 0, worst in window
      pub current_value_eur: f64,
      pub drawdown_eur: f64,            // ≥ 0, EUR loss from peak
      pub is_warm: bool,
      pub n_obs: usize,
  }

  #[derive(Debug, Clone)]
  pub struct RiskReport {
      pub assets: Vec<AssetRisk>,
      pub total_value_eur: f64,
      pub total_drawdown_eur: f64,
  }

  impl RiskReport {
      pub fn empty() -> Self {
          Self { assets: vec![], total_value_eur: 0.0, total_drawdown_eur: 0.0 }
      }
  }

  struct EwmaState {
      ewma_mean: f64,
      ewma_var: f64,
      n_obs: usize,
  }
  ```

- [ ] **Step 6: Verify the crate compiles (types only, `analyze` still has old signature)**

  Run:
  ```
  cargo build --bin portfolio-watcher 2>&1 | head -30
  ```
  Expected: errors about `AnalysisConfig` missing new fields in watcher.rs — that is fine at this stage; the struct definition itself must compile without errors.

- [ ] **Step 7: Commit**
  ```bash
  git add src/portfolio/analyzer.rs src/portfolio/mod.rs
  git commit -m "feat(portfolio): add RiskReport types, ZScoreSpike variant, EWMA config fields"
  ```

---

## Task 2: TDD — `compute_risk()` Implementation

**Files:**
- Modify: `src/portfolio/analyzer.rs`

- [ ] **Step 1: Write 5 failing unit tests**

  Append to the bottom of `src/portfolio/analyzer.rs`:
  ```rust
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
          // 50 alternating-direction ticks → nonzero variance
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
          // 40 identical prices: value = 10 sol * $100 * 0.92 = €920
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
  }
  ```

- [ ] **Step 2: Run tests — confirm they all fail with `compute_risk not found`**
  ```
  cargo test --bin solana-mev test_risk 2>&1 | tail -20
  ```
  Expected: compile error — `compute_risk` is not defined yet.

- [ ] **Step 3: Implement the private helper `ewma_for_asset`**

  Add after the `EwmaState` struct definition (before `pub fn analyze`):
  ```rust
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
          if !r.is_finite() {
              continue;
          }
          let prev_mean = mean;
          mean = lambda * mean + (1.0 - lambda) * r;
          var = lambda * var + (1.0 - lambda) * (r - prev_mean).powi(2);
          n_obs += 1;
      }
      Some(EwmaState { ewma_mean: mean, ewma_var: var, n_obs })
  }
  ```

- [ ] **Step 4: Implement the private helper `drawdown_stats`**

  Add after `ewma_for_asset`:
  ```rust
  /// Returns (current_dd_pct, max_dd_pct, peak_price). Both dd values are ≤ 0.
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
  ```

- [ ] **Step 5: Implement `compute_risk()`**

  Add after `drawdown_stats` (before `pub fn analyze`):
  ```rust
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
              if e.ewma_var > 0.0 {
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

      RiskReport { assets, total_value_eur, total_drawdown_eur }
  }
  ```

- [ ] **Step 6: Run the 6 compute_risk tests — all must pass**
  ```
  cargo test --bin solana-mev test_risk -- --nocapture 2>&1 | tail -20
  ```
  Expected:
  ```
  test tests::test_risk_drawdown ... ok
  test tests::test_risk_empty_history ... ok
  test tests::test_risk_eur_conversion ... ok
  test tests::test_risk_not_warm_below_min_obs ... ok
  test tests::test_risk_warm_after_min_obs ... ok
  test tests::test_risk_zero_variance_no_zscore ... ok
  ```

- [ ] **Step 7: Commit**
  ```bash
  git add src/portfolio/analyzer.rs
  git commit -m "feat(portfolio): implement compute_risk with EWMA volatility and drawdown"
  ```

---

## Task 3: TDD — Update `analyze()` to Emit `ZScoreSpike` Alerts

**Files:**
- Modify: `src/portfolio/analyzer.rs`

- [ ] **Step 1: Write 2 failing tests for z-score alert behavior**

  Add inside the `mod tests` block:
  ```rust
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
  ```

- [ ] **Step 2: Run the 2 new tests — confirm compile failure (wrong `analyze` signature)**
  ```
  cargo test --bin solana-mev test_analyze 2>&1 | head -20
  ```
  Expected: compile error — `analyze` takes 3 args, test passes 4.

- [ ] **Step 3: Update `analyze()` signature and add `ZScoreSpike` branch**

  Replace the entire `pub fn analyze(` function with:
  ```rust
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

          // ── 5-minute change ──────────────────────────────────────────────
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

          // ── 1-hour change ────────────────────────────────────────────────
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
              .skip(1)
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
  ```

- [ ] **Step 4: Run all 8 tests — all must pass**
  ```
  cargo test --bin solana-mev -- --nocapture 2>&1 | tail -15
  ```
  Expected:
  ```
  test tests::test_analyze_emits_zscore_spike ... ok
  test tests::test_analyze_no_zscore_when_not_warm ... ok
  test tests::test_risk_drawdown ... ok
  test tests::test_risk_empty_history ... ok
  test tests::test_risk_eur_conversion ... ok
  test tests::test_risk_not_warm_below_min_obs ... ok
  test tests::test_risk_warm_after_min_obs ... ok
  test tests::test_risk_zero_variance_no_zscore ... ok

  test result: ok. 8 passed
  ```

- [ ] **Step 5: Commit**
  ```bash
  git add src/portfolio/analyzer.rs
  git commit -m "feat(portfolio): update analyze() to emit ZScoreSpike using pre-computed RiskReport"
  ```

---

## Task 4: Update Watcher — Risk Logging + Email Section

**Files:**
- Modify: `src/portfolio/watcher.rs`

- [ ] **Step 1: Update imports in `watcher.rs`**

  Replace the current import line:
  ```rust
  use super::analyzer::{self, Alert, AnalysisConfig};
  ```
  With:
  ```rust
  use super::analyzer::{self, Alert, AnalysisConfig, RiskReport};
  ```

- [ ] **Step 2: Update `analysis_cfg` construction to include z-score fields**

  In the `run()` function, replace:
  ```rust
  let analysis_cfg = AnalysisConfig {
      alert_pct_5m: cfg.alert_pct_5m,
      alert_pct_1h: cfg.alert_pct_1h,
  };
  ```
  With:
  ```rust
  let analysis_cfg = AnalysisConfig {
      alert_pct_5m: cfg.alert_pct_5m,
      alert_pct_1h: cfg.alert_pct_1h,
      zscore_lambda: cfg.zscore_lambda,
      zscore_threshold: cfg.zscore_threshold,
      zscore_min_obs: cfg.zscore_min_obs,
  };
  ```

- [ ] **Step 3: Replace the `analyzer::analyze` call in the main loop**

  In `watcher.rs`, find:
  ```rust
  // Analyse trends
  let alerts = analyzer::analyze(&history, &portfolio, &analysis_cfg);
  if alerts.is_empty() {
      continue;
  }
  ```
  Replace with:
  ```rust
  // Compute risk metrics and log them every tick
  let risk_report = analyzer::compute_risk(&history, &portfolio, eur_rate, &analysis_cfg);
  log_risk_report(&risk_report);

  // Generate alerts using pre-computed risk data
  let alerts = analyzer::analyze(&history, &portfolio, &risk_report, &analysis_cfg);
  if alerts.is_empty() {
      continue;
  }
  ```

- [ ] **Step 4: Update the `build_email` call to pass `risk_report`**

  Find:
  ```rust
  let (subject, body) = build_email(&portfolio, &prices, &alerts, eur_rate);
  ```
  Replace with:
  ```rust
  let (subject, body) = build_email(&portfolio, &prices, &alerts, &risk_report, eur_rate);
  ```

- [ ] **Step 5: Add `log_risk_report()` function**

  Add after `log_values()`:
  ```rust
  fn log_risk_report(report: &RiskReport) {
      info!("portfolio: ── Risk Report ─────────────────────────────────────");
      for a in &report.assets {
          if a.is_warm {
              let z_str = a.z_score.map_or("--".to_string(), |z| format!("{:+.2}", z));
              let vol_str = a.sigma_ann.map_or("--".to_string(), |v| format!("{:.1}%", v));
              info!(
                  "portfolio:   {:<8} z={:<6} σ_ann={:<8} dd={:.1}%  (€-{:.2})",
                  a.symbol, z_str, vol_str, a.current_drawdown_pct.abs(), a.drawdown_eur
              );
          } else {
              info!(
                  "portfolio:   {:<8} (warming {}/{})",
                  a.symbol, a.n_obs, 30
              );
          }
      }
      info!(
          "portfolio:   Total drawdown from peak: €-{:.2}",
          report.total_drawdown_eur
      );
  }
  ```

- [ ] **Step 6: Update `build_email()` signature and add risk section**

  Replace the `build_email` function with:
  ```rust
  fn build_email(
      portfolio: &Portfolio,
      prices: &std::collections::HashMap<String, f64>,
      alerts: &[Alert],
      risk: &RiskReport,
      eur: f64,
  ) -> (String, String) {
      let subject = format!("[Portfolio Alert] {} signal(s) detected", alerts.len());

      let mut body = String::from("Portfolio Alerts\n");
      body.push_str(&"=".repeat(30));
      body.push('\n');

      for alert in alerts {
          body.push_str(&format!(
              "⚠  {} — {} (price: €{:.4}, value: €{:.2})\n",
              alert.symbol,
              alert.kind,
              alert.current_price * eur,
              alert.current_value_usd * eur,
          ));
      }

      body.push('\n');
      body.push_str("Current Holdings\n");
      body.push_str(&"-".repeat(40));
      body.push('\n');

      let sol_eur = prices.get("SOL").copied().unwrap_or(0.0) * eur;
      body.push_str(&format!(
          "SOL   {:.4} × €{:.2} = €{:.2}\n",
          portfolio.sol_amount,
          sol_eur,
          sol_eur * portfolio.sol_amount
      ));
      let mut total = sol_eur * portfolio.sol_amount;

      for token in &portfolio.tokens {
          let key = if prices.contains_key(&token.mint) { &token.mint } else { &token.symbol };
          let price_eur = prices.get(key).copied().unwrap_or(0.0) * eur;
          let value = price_eur * token.amount;
          total += value;
          body.push_str(&format!(
              "{:<8} {:.4} × €{:.4} = €{:.2}\n",
              token.symbol, token.amount, price_eur, value
          ));
      }
      body.push_str(&format!("\nTotal: €{:.2}\n", total));

      // Risk summary section
      body.push('\n');
      body.push_str("Risk Summary (EWMA λ=0.97)\n");
      body.push_str(&"-".repeat(40));
      body.push('\n');
      for a in &risk.assets {
          if a.is_warm {
              let z_str = a.z_score.map_or("--".to_string(), |z| format!("{:+.2}", z));
              let vol_str = a.sigma_ann.map_or("--".to_string(), |v| format!("{:.1}%", v));
              body.push_str(&format!(
                  "{:<8} z={:<6} σ_ann={:<8} dd={:.1}%  (€-{:.2} from peak)\n",
                  a.symbol, z_str, vol_str, a.current_drawdown_pct.abs(), a.drawdown_eur
              ));
          } else {
              body.push_str(&format!("{:<8} (warming up)\n", a.symbol));
          }
      }
      body.push_str(&format!("Portfolio drawdown from peak: €-{:.2}\n", risk.total_drawdown_eur));

      (subject, body)
  }
  ```

- [ ] **Step 7: Build the watcher binary — no errors**
  ```
  cargo build --bin portfolio-watcher 2>&1 | tail -10
  ```
  Expected: `Finished` with no errors.

- [ ] **Step 8: Run all tests — still all passing**
  ```
  cargo test --bin solana-mev 2>&1 | tail -10
  ```
  Expected: `test result: ok. 8 passed`

- [ ] **Step 9: Commit**
  ```bash
  git add src/portfolio/watcher.rs
  git commit -m "feat(portfolio): log risk report each tick and add risk section to alert emails"
  ```

---

## Task 5: CLI `show` — Load History and Print Risk Table

**Files:**
- Modify: `src/bin/portfolio_cli.rs`

- [ ] **Step 1: Update imports in `portfolio_cli.rs`**

  Replace:
  ```rust
  use solana_mev::portfolio::{self, scanner, PortfolioConfig};
  ```
  With:
  ```rust
  use solana_mev::portfolio::{self, analyzer, history, scanner, PortfolioConfig};
  use solana_mev::portfolio::analyzer::{AnalysisConfig, RiskReport};
  use std::path::Path;
  use std::time::{SystemTime, UNIX_EPOCH};
  ```

- [ ] **Step 2: Replace the `Show` command handler**

  Find the `Command::Show => {` block (lines 51-59) and replace with:
  ```rust
  Command::Show => {
      let p = portfolio::load_portfolio(&cfg.portfolio_path)
          .context("portfolio.json not found — run `portfolio-cli init` first")?;

      // Load price history so drawdown and EWMA have data to work with
      let mut hist = history::load_history(Path::new(&cfg.history_path))
          .unwrap_or_default();

      let mints: Vec<String> = p.tokens.iter().map(|t| t.mint.clone()).collect();
      let prices = portfolio::pricer::fetch_prices(
          &http, &mints, cfg.birdeye_api_key.as_deref(),
      )
      .await
      .unwrap_or_default();

      // Append live snapshot so risk metrics reflect current prices
      let ts = SystemTime::now()
          .duration_since(UNIX_EPOCH)
          .unwrap_or_default()
          .as_secs();
      let mut snap_prices = prices.clone();
      hist.push_back(portfolio::history::PriceSnapshot { ts, prices: snap_prices });

      let eur_rate = portfolio::pricer::fetch_eur_rate(&http).await.unwrap_or(0.92);

      let analysis_cfg = AnalysisConfig {
          alert_pct_5m: cfg.alert_pct_5m,
          alert_pct_1h: cfg.alert_pct_1h,
          zscore_lambda: cfg.zscore_lambda,
          zscore_threshold: cfg.zscore_threshold,
          zscore_min_obs: cfg.zscore_min_obs,
      };
      let risk = analyzer::compute_risk(&hist, &p, eur_rate, &analysis_cfg);

      print_portfolio(&p, &prices);
      print_risk_table(&risk, cfg.zscore_lambda, cfg.zscore_min_obs);
  }
  ```

- [ ] **Step 3: Add `print_risk_table()` function**

  Add after `print_portfolio()`:
  ```rust
  fn print_risk_table(report: &RiskReport, lambda: f64, min_obs: usize) {
      println!();
      println!("  Risk Metrics (EWMA λ={lambda:.2})");
      println!("  {}", "─".repeat(60));
      println!("  {:<8}  {:<8}  {:<9}  {:<10}  {}", "Symbol", "Z-score", "σ_ann", "DrawDown", "DD (€)");
      println!("  {}", "─".repeat(60));
      for a in &report.assets {
          if a.is_warm {
              let z_str = a.z_score.map_or("--".to_string(), |z| format!("{:+.2}", z));
              let vol_str = a.sigma_ann.map_or("--".to_string(), |v| format!("{:.1}%", v));
              println!(
                  "  {:<8}  {:<8}  {:<9}  {:<10}  -{:.2}",
                  a.symbol,
                  z_str,
                  vol_str,
                  format!("{:.1}%", a.current_drawdown_pct),
                  a.drawdown_eur,
              );
          } else {
              println!(
                  "  {:<8}  (warming {}/{})",
                  a.symbol, a.n_obs, min_obs
              );
          }
      }
      println!("  {}", "─".repeat(60));
      println!("  Portfolio drawdown from peak: €-{:.2}", report.total_drawdown_eur);
  }
  ```

- [ ] **Step 4: Fix unused variable warning — `snap_prices` is moved**

  In the `Show` handler, the line:
  ```rust
  let mut snap_prices = prices.clone();
  hist.push_back(portfolio::history::PriceSnapshot { ts, prices: snap_prices });
  ```
  should be simplified (no need for the intermediate `mut`):
  ```rust
  hist.push_back(portfolio::history::PriceSnapshot { ts, prices: prices.clone() });
  ```

- [ ] **Step 5: Build the CLI binary — no errors or warnings**
  ```
  cargo build --bin portfolio-cli 2>&1 | tail -10
  ```
  Expected: `Finished` with no errors.

- [ ] **Step 6: Run full test suite — all 8 tests pass**
  ```
  cargo test --bin solana-mev 2>&1 | tail -10
  ```
  Expected: `test result: ok. 8 passed`

- [ ] **Step 7: Lint**
  ```
  cargo clippy --bin portfolio-cli --bin portfolio-watcher --bin solana-mev 2>&1 | grep "^error" | head -20
  ```
  Expected: no lines starting with `error`.

- [ ] **Step 8: Commit**
  ```bash
  git add src/bin/portfolio_cli.rs
  git commit -m "feat(portfolio): extend CLI show with EWMA risk table"
  ```

---

## Verification

**End-to-end smoke test (CLI):**
```bash
cargo run --bin portfolio-cli -- show
```

Expected output (after "Current Holdings" table):
```
  Risk Metrics (EWMA λ=0.97)
  ────────────────────────────────────────────────────────────
  Symbol    Z-score   σ_ann     DrawDown    DD (€)
  ────────────────────────────────────────────────────────────
  SOL       +0.41     82.4%     -3.1%       -42.10
  ...
  ────────────────────────────────────────────────────────────
  Portfolio drawdown from peak: €-54.40
```
(or `(warming N/30)` if history file is sparse)

**Watcher smoke test:**
```bash
DRY_RUN=true cargo run --bin portfolio-watcher 2>&1 | grep "Risk Report" | head -5
```
Expected: one `Risk Report` log line per tick.
