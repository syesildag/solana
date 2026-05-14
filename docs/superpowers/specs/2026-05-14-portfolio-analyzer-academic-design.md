# Portfolio Analyzer — Academic Risk Metrics Design

**Date:** 2026-05-14
**Status:** Draft — awaiting implementation

---

## Context

The current portfolio analyzer (`src/portfolio/analyzer.rs`) uses naive fixed-threshold comparisons:
- 5-minute price change ≥ 3%
- 1-hour price change ≥ 10%
- 7-day rolling high/low breach

These thresholds are statistically blind to volatility regimes: a 3% move in a quiet market is very different from a 3% move during a volatile period. The goal is to replace or augment these with academically grounded risk metrics — specifically EWMA-based z-scores, annualized volatility, and drawdown — that self-calibrate to each asset's historical behavior.

---

## Goal

Add a `RiskReport` (pure metrics, no alerts) that is computed each tick and surfaced in three places:
- Console log (every tick, in the watcher)
- Email digest (risk summary section, when an alert email is sent)
- CLI `show` command (on demand)

Also add `ZScoreSpike` as a new alert kind in the existing alert path, triggered when `|z| > threshold`.

---

## Architecture

### New Types

**`EwmaState`** (internal, per asset)
```rust
struct EwmaState {
    ewma_mean: f64,   // EWMA of log-returns
    ewma_var: f64,    // EWMA variance of log-returns
    n_obs: usize,     // number of valid observations processed
}
```

**`AssetRisk`** (public, one per asset)
```rust
pub struct AssetRisk {
    pub symbol: String,
    pub z_score: Option<f64>,         // None until warm (n_obs >= MIN_OBS)
    pub sigma_ann: Option<f64>,       // annualized volatility (%)
    pub current_drawdown_pct: f64,    // current price vs. rolling peak (%)
    pub max_drawdown_pct: f64,        // worst drawdown in history window (%)
    pub current_value_eur: f64,       // amount × price_usd / eur_usd_rate
    pub drawdown_eur: f64,            // peak_value_eur - current_value_eur (EUR loss)
    pub is_warm: bool,                // false while n_obs < MIN_OBS
}
```

**`RiskReport`** (public, output of `compute_risk`)
```rust
pub struct RiskReport {
    pub assets: Vec<AssetRisk>,
    pub total_value_eur: f64,
    pub total_drawdown_eur: f64,      // sum of drawdown_eur across all assets
}
```

**New `AlertKind` variant** (added to `mod.rs`)
```rust
AlertKind::ZScoreSpike {
    z: f64,              // z-score of the triggering log-return
    threshold: f64,      // configured threshold (e.g. 2.5)
    return_pct: f64,     // the raw % return that triggered it
}
```

### New Config Fields (env-loaded, `PortfolioConfig`)

| Variable | Default | Meaning |
|---|---|---|
| `ALERT_ZSCORE_LAMBDA` | 0.97 | EWMA decay factor (half-life ~23 min at 1-min ticks) |
| `ALERT_ZSCORE_THRESHOLD` | 2.5 | Alert when `|z| >` this value |
| `ALERT_ZSCORE_MIN_OBS` | 30 | Minimum snapshots before z-score alerts fire |

### Files Changed

| File | Change |
|---|---|
| `src/portfolio/analyzer.rs` | Add `compute_risk()`, EWMA helpers, drawdown helpers |
| `src/portfolio/mod.rs` | Add `AssetRisk`, `RiskReport`, `ZScoreSpike` variant, 3 config fields |
| `src/portfolio/watcher.rs` | Call `compute_risk()` each tick; log to console; add risk section to email |
| `src/bin/portfolio_cli.rs` | Extend `show` to call `compute_risk()` and print risk table |

---

## Data Flow

```
Each tick (every 60s):
                                       eur_usd_rate
                                           │
  history_deque ──────────────────► compute_risk(history, portfolio, eur_rate, config)
                                           │
                                      RiskReport
                                      ┌────┴────┐
                                 console log   email risk section
                                 (every tick)  (when alerts fire)

  history_deque + RiskReport ──► generate_alerts(history, risk_report, config)
                                           │
                                      Vec<Alert>    ← includes ZScoreSpike
                                           │
                                     cooldown filter → SMTP

CLI show:
  load history.jsonl → fetch live prices → append live snapshot → compute_risk → print table
  (drawdown requires full history to know the peak; history.jsonl is always loaded)
```

---

## EWMA Computation

Iterate through the history deque once per asset to accumulate EWMA state:

```
For each asset in portfolio:
    prices = [snap.prices[asset] for snap in history if asset in snap.prices]
    
    Initialize:
        ewma_mean = 0.0
        ewma_var  = 0.0
        peak      = prices[0]
        max_dd    = 0.0
    
    For each consecutive pair (p_prev, p_curr):
        r = ln(p_curr / p_prev)          // log-return; skip if p_prev == 0
        prev_mean = ewma_mean
        ewma_mean = λ·ewma_mean + (1−λ)·r
        ewma_var  = λ·ewma_var  + (1−λ)·(r − prev_mean)²  // use prev_mean to avoid bias
        peak = max(peak, p_curr)
        dd = (p_curr − peak) / peak      // ≤ 0
        max_dd = min(max_dd, dd)

    z_score = (r_last − ewma_mean) / sqrt(ewma_var)  // only if ewma_var > EPSILON
    σ_ann   = sqrt(ewma_var × 525_600)               // 525,600 min/year
    current_dd_pct = (p_last − peak) / peak × 100
    max_dd_pct     = max_dd × 100
    
    current_value_eur = amount × p_last / eur_usd_rate
    peak_value_eur    = amount × peak  / eur_usd_rate
    drawdown_eur      = peak_value_eur − current_value_eur
```

**Why λ = 0.97?** Half-life = `ln(0.5) / ln(0.97)` ≈ 23 minutes. Observations older than ~1 hour contribute < 10% of the total weight. This is the RiskMetrics (JP Morgan, 1994) recommended value for intraday data.

**Why log-returns?** Log-returns are additive across time, symmetric for up/down moves, and have better statistical properties (closer to normal) than arithmetic returns — all required for z-scores to be valid.

---

## Alert Generation

`generate_alerts()` receives both `history` and the pre-computed `RiskReport` to avoid double-iteration:

```
For each AssetRisk in risk_report.assets:
    if is_warm && z_score.is_some() && |z_score| > config.zscore_threshold:
        emit Alert { kind: ZScoreSpike { z, threshold, return_pct }, ... }

// Existing 5m/1h/7d signals remain unchanged
```

---

## Console Output (per tick)

```
Risk Report ─────────────────────────────────────────
  SOL       z=+1.23  σ_ann=82.4%  dd=-3.1%  (€-42.10)
  BONK      z=-0.41  σ_ann=241%   dd=-18.2% (€-12.30) 
  [NEW]     warming up (12/30 obs)
Total drawdown from peak: €-54.40
──────────────────────────────────────────────────────
```

---

## Email Risk Section

Added after "Current Holdings", before footer:

```
Risk Summary
────────────────────────────────────────
  SOL    σ_ann=82.4%  dd=-3.1%  (€-42.10 from peak)
  BONK   σ_ann=241%   dd=-18.2% (€-12.30 from peak)
Portfolio drawdown: €-54.40
```

---

## CLI Show Extension

After the existing holdings table, print:

```
Risk Metrics (EWMA λ=0.97)
──────────────────────────────────────────
Symbol   Z-score   σ_ann    DrawDown   DD (€)
──────────────────────────────────────────
SOL      +1.23     82.4%    -3.1%      -42.10
BONK     (warming) 241%     -18.2%     -12.30
──────────────────────────────────────────
Portfolio drawdown from peak: €-54.40
```

---

## Error Handling

| Case | Behavior |
|---|---|
| `n_obs < MIN_OBS` | `is_warm: false`, z-score is `None`, no `ZScoreSpike` alert, show "warming" in display |
| `ewma_var < 1e-12` (constant price) | Skip z-score computation, output `None` |
| `p_prev == 0.0` | Skip that log-return (avoid `ln(0) = -inf`), decrement effective n_obs |
| NaN/Inf in EWMA state | Reset state for that asset, mark not warm |
| EUR rate unavailable | Fall back to rate = 1.0 (show as USD), log warning |

---

## Tests

All in `#[cfg(test)]` at the bottom of `analyzer.rs`:

1. **EWMA convergence** — synthetic constant-return series → `ewma_var` stabilizes within known band after 60 steps
2. **Z-score spike detection** — inject single 10×-return into stable series → `|z| > 2.5`
3. **Annualized vol formula** — series with known per-tick σ → `σ_ann` matches `σ × sqrt(525_600)`
4. **Drawdown** — monotonically falling price series → `max_drawdown_pct == (last/first − 1) × 100`
5. **Warm-up guard** — only 10 snapshots → `is_warm == false`, no `ZScoreSpike` alert generated
6. **EUR conversion** — known price + known eur_rate → `current_value_eur` exact match
7. **Zero variance guard** — all prices identical → no div-by-zero, `z_score == None`

---

## References

- RiskMetrics Technical Document (JP Morgan, 1994) — EWMA λ values for intraday data
- Cont, R. (2001). "Empirical properties of asset returns: stylized facts and statistical issues." *Quantitative Finance* — motivation for log-returns
- Sharpe, W. F. (1966). "Mutual fund performance." *Journal of Business* — risk-adjusted return framing
