# Best-Tuned N=#curated vs N=1 Momentum Comparison — Design

**Date:** 2026-06-28
**Status:** Design approved; awaiting implementation plan
**Scope:** `momentum-sim` only (measurement). No live-trader change, no `.env` knob.
**Builds on:** `replay_multi` / `maxn_rows` / `maxn-compare` (merged in
`docs/superpowers/plans/2026-06-28-momentum-maxn-sim.md`).

## Problem

`maxn-compare` replays ONE fixed config across N=1..K. That is biased: the config
optimal for a single-slot rotation trader is not the config optimal for holding the whole
basket. A fair comparison tunes each model to its *own* optimum, then compares.

The question: **does a hold-all-curated-names portfolio (N = number of curated tokens),
grid-searched to its best config, beat a single-slot trader (N=1) grid-searched to its
best config — given the same total capital?**

We cannot answer this today: the production `run` grid (`run_grid`) is single-position
only. This spec adds a grid search at arbitrary N and a comparison subcommand. It is
measurement only — no live-trader change.

## Decisions (locked during brainstorming)

1. **Equal total capital.** The two models compete for the SAME money. A `--pool-usdc`
   flag sets the total; each grid run uses `trade_usdc = pool / N`. N=1 puts the whole
   pool in one slot (matches the live single-slot trader); N=#curated splits it evenly.
   Held-out (test) **absolute** P&L is then directly comparable — neither side wins merely
   by deploying more capital.
2. **Two endpoints.** Compare N=1 (single slot) vs N = `watched.len()` (hold all curated
   entries). A `--max-n` flag can override the upper endpoint to inspect an intermediate N.
3. **Objective = best ROBUST config, ranked by held-out test P&L.** Mirrors the existing
   grid: a config is eligible only if profitable in BOTH train and test slices with
   ≥ `min_trades` in each (`config_is_robust`). Among eligible configs, the one with the
   highest `net_pnl_test` wins. If an N has no robust config, report that (don't fabricate
   a winner from a non-robust config).
4. **Additive — production grid untouched.** Add a parallel `run_grid_multi` that calls
   `replay_multi(..., max_positions)`; do NOT modify `run_grid`, `replay`,
   `replay_with_stream`, or `replay_with_regime`. At N=1, `run_grid_multi` reproduces
   `run_grid` (because `replay_multi(..., 1)` ≡ `replay_with_regime`, already proven).
5. **Rotation is moot at N=#curated.** When `max_positions == watched.len()`, you can never
   be "full with a stronger candidate outside the basket," so eviction never fires. The
   N=#curated grid therefore sweeps `rotate_factors = [0.0]` (saves grid time, no effect on
   the result). The N=1 grid sweeps the caller-supplied `rotate_factors` (single-slot
   rotation is a real lever there).

## Architecture

All changes additive. Nothing existing is modified.

| Unit | Location | Purpose |
|---|---|---|
| `run_grid_multi(train, test, watched, base, …grid ranges…, max_positions) -> Vec<SimResult>` | `src/portfolio/sim.rs` | Walk-forward grid at a fixed `max_positions`. Identical structure to `run_grid` but the two inner replays call `replay_multi(slice, watched, stream, &p, mask, max_positions)` instead of `replay_with_regime`. Returns the same `SimResult` rows, sorted by `net_pnl_test` descending. |
| `MaxnOptimize` command + `maxn_optimize(args)` | `src/bin/momentum_sim.rs` | Loads history + watched, splits train/test, runs `run_grid_multi` at N=1 and N=`watched.len()` (each with `trade_usdc = pool/N`), selects each N's best robust `SimResult`, prints the head-to-head. |

Data flow:

```
load history → sanitize → split(train_frac) → load watched (count = K)
   │
   ├── run_grid_multi(..., trade_usdc=pool/1, max_positions=1,  rotate_factors=<swept>)  → best robust @ N=1
   └── run_grid_multi(..., trade_usdc=pool/K, max_positions=K,  rotate_factors=[0.0])    → best robust @ N=K
   │
   head-to-head: params + test/train P&L + trades + win% + maxDD for each; verdict by net_pnl_test
```

## `run_grid_multi` semantics

- Signature mirrors `run_grid` exactly, plus a trailing `max_positions: usize` parameter.
- Body is a copy of `run_grid` with the two replay calls changed:
  `replay_with_regime(train, watched, &train_stream, &p, tr_mask)` →
  `replay_multi(train, watched, &train_stream, &p, tr_mask, max_positions)` (and likewise
  for test). Everything else — `ranked_stream` per `(metric, lookback, max_run)` tuple,
  `min_metric_candidates` from the train distribution, `stop_variants`, `sizing_variants`,
  `regime_variants`, rayon fan-out, final sort by `net_pnl_test` — is identical.
- `base.trade_usdc` is set by the caller before the call (= `pool / max_positions`); the
  grid's `sizing_variants(base.trade_usdc, …)` and per-trade gas model pick it up
  unchanged.
- **Anchor:** `run_grid_multi(..., max_positions = 1)` returns results equal to `run_grid`
  on the same inputs (same params, same train/test P&L per row). This is the regression
  guarantee that the generalization is faithful.

## `maxn-optimize` subcommand

CLI (same config surface as the existing grid-driving subcommands):

```
momentum-sim maxn-optimize \
  [--pool-usdc N]       # default: live momentum_trade_usdc from .env
  [--max-n K]           # default: watched.len(); upper endpoint to compare against N=1
  [--min-trades N]      # default 3 (robustness gate, per slice)
  [--train-frac 0.70] [--tokens PATH] [--history PATH] [--max-step 8]
  [--rotate-factors a,b,c]   # swept for the N=1 grid; forced [0.0] for the N=K grid
  [grid-range overrides as the `run` subcommand exposes, optional]
```

Behavior:
1. Load + sanitize history, load watched (count K), split at `train_frac`.
2. `base = base_params(cfg)`; set `base.reinvest_frac = 0.0`, `base.size_ceiling_usdc =
   base.trade_usdc` (fixed notional per slot — no compounding, so each slot is an
   independent fixed bet and equal-capital math is clean).
3. For N=1: `base.trade_usdc = pool / 1`; `run_grid_multi(..., rotate_factors=<flag>,
   max_positions=1)`. Pick best robust by `net_pnl_test`.
4. For N=K: `base.trade_usdc = pool / K`; `run_grid_multi(..., rotate_factors=[0.0],
   max_positions=K)`. Pick best robust by `net_pnl_test`.
5. Print each N's best-robust params + (test, train) P&L, trades, win%, maxDD, and a
   one-line verdict comparing `net_pnl_test` (equal capital ⇒ directly comparable). If
   either N has no robust config, print "no robust config at N=…" for that side.

Output sketch:

```
Best-tuned hold-all vs single-slot — pool $8000, equal total capital
Loaded 32130 snapshots (max_step=8×). Train 22491 (~47.9d) / Test 9639 (~20.5d). 8 tokens.

N=1  (single slot, $8000/slot):
  metric=sortino min=0.0600 trail=20% lookback=480 max_run=6 regime=trend@480 rotate=0.100
  test +X.XX | train +Y.YY | trades T | win W% | maxDD D.D%

N=8  (hold all, $1000/slot):
  metric=sharpe min=0.0375 trail=30% lookback=720 max_run=0 regime=off rotate=0
  test +X'.XX | train +Y'.YY | trades T' | win W'% | maxDD D'.D%

VERDICT: <single-slot | hold-all> wins held-out P&L by Δ USDC (equal $8000 capital).
```

## Testing

- **`run_grid_multi` at N=1 == `run_grid`** — on a small synthetic history with identical
  grid ranges, the two return the same set of `SimResult` rows (same params → same
  net_pnl_train/test/trades). The correctness anchor for the generalization.
- **`run_grid_multi` at N=2 produces valid robust-classifiable results** — on a 2-token
  synthetic history, returns non-empty results with finite P&L, and `is_robust` can be
  evaluated (smoke that the multi path runs end-to-end through the grid).
- **CLI smoke** — `maxn-optimize` on the real curated history prints both best-robust
  configs and a verdict line; output captured in the implementation report (the subcommand
  is a thin selector+printer, so no dedicated unit test).

## Edge cases

- **K == 1** (only one curated token): N=1 and N=K coincide. Print the single best-robust
  config once and note the endpoints are identical rather than a misleading "tie."
- **Thin slots:** at large K, `trade_usdc = pool/K` shrinks and `est_gas_bps` rises; the
  existing `slippage_bps + gas_bps > max_cost_bps` gate may legitimately reject entries.
  This is correct modeled behavior (small slots are eaten by costs), not a bug — but if it
  zeroes out trades it will show as "no robust config at N=K," which is itself a finding.
- **No robust config at an N:** print "no robust config at N=…"; do not emit a verdict that
  treats a missing side as P&L 0.

## Out of scope (explicitly)

- Any change to the live momentum trader, `run`, `run_grid`, `replay`,
  `replay_with_stream`, or `replay_with_regime`.
- Any new `.env` variable (`--pool-usdc`, `--max-n` are CLI flags).
- Sweeping all intermediate N (only the two endpoints; `--max-n` picks the upper one).
- Equity-compounding sizing in this comparison (`reinvest_frac` forced 0 — fixed notional
  per slot keeps the equal-capital comparison clean).

## Success criterion

`maxn-optimize` runs over the curated universe and prints, for N=1 and N=#curated, each
side's best **robust** config and held-out P&L at equal total capital, plus a verdict.
The N=1 best-robust config matches what the existing `optimize-momentum-config` grid would
find (same single-slot model). The decision output: whether a best-tuned hold-all basket
beats a best-tuned single-slot trader on the same money — if yes, that motivates a live
multi-position trader (a separate, larger project); if no, the single-slot live trader
stands, now justified against its best basket alternative rather than assumed.
