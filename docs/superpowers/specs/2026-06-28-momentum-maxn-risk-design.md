# Risk-Adjusted Robustness vs N (mark-to-market) — Design

**Date:** 2026-06-28
**Status:** Design approved; awaiting implementation plan
**Scope:** `momentum-sim` only (measurement). No live-trader change, no `.env` knob.
**Builds on:** `replay_multi` / `run_grid_multi` / `maxn-optimize`
(`docs/superpowers/plans/2026-06-28-momentum-maxn-optimize.md`).

## Problem

The hypothesis: **holding N>1 names is more *robust* — a smoother, lower-variance ride —
even though total P&L drops.** Both prior experiments measured P&L *level* (where
single-slot wins). Neither measured variance / risk-adjusted return, so the intuition is
untested.

"Robust" is operationalized here as **risk-adjusted, lower-variance** (the classic
diversification benefit): for equal total capital, does a basket of N names produce a
smoother equity curve — higher Sharpe/Sortino, smaller true drawdown — than the single
best name, even at lower total return?

Critically, this must be measured on a **mark-to-market (MTM)** equity curve (portfolio
value every snapshot, including unrealized swings of open positions), because that is where
diversification reduces variance. The realized-only equity curve we have today (updated
only on closes) structurally cannot see holding-period smoothing.

Crypto caveat: alts are highly correlated with SOL, so the variance-reduction benefit may
be muted — measuring it (rather than assuming it) is the point.

## Decisions (locked during brainstorming)

1. **Robustness = risk-adjusted / lower variance.** Report annualized Sharpe, Sortino, and
   true max drawdown (% of running-peak equity) per N.
2. **Measured on the mark-to-market equity curve**, per snapshot, on the **test (held-out)**
   slice — the honest out-of-sample ride.
3. **Reported for the best-by-test-P&L config** at each N (the config one would actually
   deploy — same selection `maxn-optimize` already does). Selecting *best-by-Sharpe* is a
   possible follow-up, explicitly out of scope.
4. **Equal total capital** carries over from `maxn-optimize`: `trade_usdc = pool / N`, so
   `pool = trade_usdc × max_positions`. Sharpe/trueDD are then comparable across N.
5. **Additive, existing behavior preserved.** `replay_multi`'s public signature and output
   are unchanged (its body moves into a private core that `replay_multi` delegates to). The
   N=1 anchor and all existing `replay_multi` / `run_grid_multi` tests must still pass. The
   forbidden production functions (`run_grid`, `replay`, `replay_with_stream`,
   `replay_with_regime`, live trader) are untouched.

## The mark-to-market equity curve

At snapshot `t`:

```
equity_t = pool + realized_pnl_so_far + Σ_open( tokens_i × mark_i,t − usdc_spent_i )
```

- `pool = params.trade_usdc × max_positions` (the equal-capital base).
- `mark_i,t` = token i's price at `t`; if absent at `t`, **carry forward** the position's
  last observed mark (avoids a spurious unrealized jump on a gap bar).
- One `(ts, equity_t)` point per snapshot. The curve starts at `pool` (no positions, no
  realized P&L) and stays positive under normal conditions, so standard peak-to-trough
  drawdown is well-defined — **fixing the >100% artifact** of the realized-only
  `max_drawdown_pct` (which divides by a possibly-tiny running P&L peak seeded at 0).

## Risk metrics

`risk_metrics(equity: &[(u64, f64)], periods_per_year: f64) -> RiskMetrics`:

- Per-step **simple returns** `r_k = (equity_k − equity_{k−1}) / equity_{k−1}` for
  `equity_{k−1} > 0`.
- **Sharpe** = `mean(r) / stdev(r) × sqrt(periods_per_year)` (sample stdev; 0 if <2 returns
  or stdev == 0).
- **Sortino** = `mean(r) / downside_dev(r) × sqrt(periods_per_year)`, where
  `downside_dev` is the RMS of `min(r_k, 0)`. Edge cases (unambiguous): `<2` returns → all
  metrics `0.0`; `downside_dev == 0` (no negative returns observed) → Sortino =
  `f64::INFINITY` (reads correctly as "no downside risk"; printed as `inf`). Sortino ≥
  Sharpe when downside is a subset of total deviation.
- **`true_max_dd_pct`** = `max over k of (peak_k − equity_k) / peak_k × 100`, peak being the
  running max of `equity` (standard drawdown). Always in `[0, 100)` given positive equity.
- `periods_per_year` is supplied by the caller as `365 × 86_400 / 184.0` (the nominal
  184 s/snapshot cadence the codebase already uses for `span_days`). Both N use the same
  cadence, so the comparison is fair; the absolute annualized number is approximate (gappy
  snapshots), and the spec/output says so.

```rust
pub struct RiskMetrics {
    pub sharpe: f64,
    pub sortino: f64,
    pub true_max_dd_pct: f64,
}
```

## Architecture

Additive. `replay_multi`'s public contract is preserved by extracting its body into a
private core.

| Unit | Location | Purpose |
|---|---|---|
| `replay_multi_core(snapshots, watched, stream, params, regime, max_positions, record_mtm: bool) -> (SimRun, Vec<(u64,f64)>)` | `src/portfolio/sim.rs` | The existing `replay_multi` loop, moved verbatim. When `record_mtm`, pushes `equity_t` per snapshot (after that snapshot's exits/eviction/entries are processed); when false, returns an empty Vec and does no MTM work. |
| `replay_multi(...) -> SimRun` | `src/portfolio/sim.rs` | **Unchanged signature.** Now `replay_multi_core(.., false).0`. |
| `replay_multi_mtm(...) -> (SimRun, Vec<(u64,f64)>)` | `src/portfolio/sim.rs` | `replay_multi_core(.., true)`. Used only by the comparison (2 replays total). |
| `RiskMetrics` + `risk_metrics(...)` | `src/portfolio/sim.rs` | Pure metric computation (above). |
| extend `maxn_optimize` | `src/bin/momentum_sim.rs` | After selecting each N's best-robust config, `replay_multi_mtm` on the test slice, compute `risk_metrics`, print Sharpe/Sortino/trueDD per N and a RISK verdict. |

Data flow (extends the existing `maxn-optimize` flow):

```
… existing: run_grid_multi at N=1 and N=K → best_robust_by_test per N …
   │  (for each N's winner)
   ├── replay_multi_mtm(test, watched, test_stream(best.params), best.params, mask, N)
   │       → (SimRun, mtm_curve)
   └── risk_metrics(mtm_curve, 365*86400/184)  → Sharpe, Sortino, trueDD
   │
   print per-N: P&L + Sharpe + Sortino + trueDD
   P&L VERDICT (net_pnl_test, existing) + RISK VERDICT (Sharpe/trueDD)
```

Note: `maxn_optimize` re-builds the ranked stream for the winning `params` to feed
`replay_multi_mtm` (the grid does not retain per-config streams). One `ranked_stream` +
one `replay_multi_mtm` per N — negligible next to the grid itself.

## Output (added to the existing per-N blocks)

```
N=1  (single slot, $8000.00/slot):
  metric=… trail=…% … rotate=…
  test +3841.01 | train +… | trades … | win …% 
  risk(test MTM): Sharpe 1.21 | Sortino 1.68 | trueDD 14.3%

N=8  (hold 8, $1000.00/slot):
  …
  risk(test MTM): Sharpe 2.10 | Sortino 3.04 | trueDD 4.1%

P&L VERDICT:  single-slot (N=1) wins held-out P&L by +3361.11 USDC (equal $8000 capital).
RISK VERDICT: hold-all (N=8) is the smoother ride — Sharpe 2.10 vs 1.21, trueDD 4.1% vs 14.3%.
              Intuition "N>1 more robust though lower P&L": SUPPORTED on this sample.
              (or NOT SUPPORTED, when single-slot also wins risk-adjusted)
Caveat: one held-out slice; MTM annualized at nominal 184s cadence; crypto names co-move,
so realized variance reduction may be modest. Suggestive, not proven.
```

The realized `max_dd_test` (the >100%-capable metric) is **dropped** from the per-N line;
only the MTM `trueDD` is shown, to avoid two conflicting drawdown numbers. (The `win %`
field from the existing block is retained.)

## Testing

- **Existing behavior preserved:** all current `replay_multi` tests (both N=1 anchors,
  eviction, dedup, drawdown) and `run_grid_multi` tests pass unchanged after the
  core-extraction refactor.
- **`replay_multi_mtm`:** on a single-token rise-then-fall fixture — the MTM curve has one
  point per snapshot, the first point equals `pool`, unrealized rises during the hold and is
  folded into realized after the exit (post-exit equity ≈ pool + realized, no open
  position). On a two-token fixture at N=2, both holds contribute to unrealized
  simultaneously.
- **`risk_metrics`:** a strictly increasing line → Sharpe > 0 and `true_max_dd_pct ≈ 0`;
  a symmetric zig-zag with no net drift → Sharpe ≈ 0; a curve with a peak then trough →
  `true_max_dd_pct` equals the known peak-to-trough percentage; Sortino ≥ Sharpe when the
  upside steps are larger than the downside steps. Degenerate (<2 points, constant) → zeros,
  no NaN/panic.
- **CLI smoke:** `maxn-optimize` prints the `risk(test MTM)` line for each N and a `RISK
  VERDICT`; output captured in the implementation report.

## Out of scope (explicitly)

- Selecting the best config *by* Sharpe (we report risk for the best-by-P&L config).
- Any change to the live trader, `run`, `run_grid`, `replay`, `replay_with_stream`,
  `replay_with_regime`, or the production grid path.
- A risk-free-rate term in Sharpe (excess return over 0 is fine for this relative
  comparison).
- Correlation-matrix / factor analysis of the basket (a deeper study; not needed to answer
  the smoother-ride question).

## Success criterion

`maxn-optimize` reports, for N=1 and N=#curated at equal capital, the held-out
Sharpe/Sortino/true-drawdown of each side's deployable config, and a RISK VERDICT stating
whether N>1 is risk-adjusted-smoother. The decision output: whether the intuition holds —
if N>1 is materially smoother (higher Sharpe, smaller trueDD) at acceptable P&L cost, that
is a real argument for a multi-position live trader on a risk-adjusted basis, even though
single-slot wins raw P&L; if not, single-slot stands on both axes.
