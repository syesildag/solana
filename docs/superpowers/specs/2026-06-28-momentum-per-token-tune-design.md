# Per-Token Tuning + Validation — SP2 — Design

**Date:** 2026-06-28
**Status:** Auto-designed under delegated autonomy (user away). Decisions chosen by best
judgment, documented below.
**Scope:** `momentum-sim` only. Computes per-token best params, optionally writes them to
`momentum_tokens.json`, and runs the 3-arm validation that **gates SP3**.

## Problem & goal

SP1 made the sim consume optional per-token `{min_metric, trail_pct, max_run_pct}`. SP2:
1. **Computes** each token's best `{min_metric, trail_pct, max_run_pct}` (single-token grid
   at the global metric/lookback).
2. **Writes** them into `momentum_tokens.json` (with `--apply`).
3. **Validates** via a 3-arm comparison at equal capital on the held-out slice, producing
   the verdict that gates SP3 (the multi-position live trader): *does per-token tuning make
   the hold-all basket beat single-slot?*

## Decisions (autonomous)

1. **Global metric/lookback/regime stay fixed during per-token tuning.** They are global
   (SP1 decision). Per-token tuning sweeps only `{min_metric, trail_pct, max_run_pct}` for
   each token *given* the global metric/lookback/regime. Those globals come from the
   **global grid best** computed in the same run (so the comparison is internally
   consistent), falling back to `.env` (`base_params`) values for the sweep's fixed knobs.
2. **Per-token best = best robust by held-out test P&L**, in isolation (single-token
   universe, N=1), reusing `run_grid_multi` + `best_robust_by_test`. A token with no robust
   single-name config gets **no override** (falls back to global) — recorded as "global".
3. **Three validation arms, equal capital, test slice, with risk metrics:**
   - **A — single-slot, global best** (N=1, global config): the incumbent bar.
   - **B — hold-all, global config** (N=#tokens, one config): the prior loser.
   - **C — hold-all, per-token tuned** (N=#tokens, global metric/lookback + each token's own
     `{min_metric, trail, max_run}`): the contender.
   Report for each: held-out P&L, Sharpe, Sortino, true drawdown (MTM, equal capital).
4. **Verdict (the SP3 gate):** per-token tuning "rescues the basket" iff **C beats A on
   P&L OR on risk-adjusted return (Sharpe)** by a clear margin (and is not strictly
   dominated by A). Output states SUPPORTED / NOT-SUPPORTED / MIXED with the numbers; the
   controller reads it to decide whether to build SP3.
5. **In-memory validation; `--apply` is separate.** Arm C applies per-token params via an
   in-memory `watched` (params set on each entry) — no file write needed for the verdict.
   `--apply` additionally persists per-token params into `momentum_tokens.json` (merge by
   mint, preserving symbol/mint/name/equity and untuned tokens). Global best is **not**
   written to `.env` here (that remains `optimize-momentum-config`'s job; out of SP2 scope
   to avoid duplicating the .env writer).

## Architecture

| Unit | Location | Purpose |
|---|---|---|
| `tune_per_token(train, test, watched, base, trails, max_runs, quantile_probs, regime_obs_set, regime_trend_obs, min_trades) -> Vec<(mint, Option<TokenParams>, f64)>` | `src/portfolio/sim.rs` | For each token: single-token universe, `run_grid_multi` at N=1 sweeping `{trail × max_run × min_metric-quantiles}` with metric/lookback/regime from `base`; `best_robust_by_test`; extract `{min_metric, trail_pct, max_run_pct}` (or `None` if no robust config). Returns per-token best params + its isolated test P&L. |
| `PerTokenTune` command + `per_token_tune(args)` | `src/bin/momentum_sim.rs` | Orchestrates: global grid (reuse run_grid_multi N=1 + N=K best-robust) → arms A/B; `tune_per_token` → per-token params; in-memory Arm C; risk-metric all three (test-slice MTM via `replay_multi_mtm` + `risk_metrics`); print per-token table + 3-arm table + verdict. `--apply` writes the JSON. |
| `write_token_params(path, &HashMap<mint, TokenParams>)` | `src/bin/momentum_sim.rs` (or a small helper) | Read raw `Vec<WatchedToken>` from the file (unfiltered), set `params` by mint, write pretty JSON back. Preserves all entries + field order. |

Reuses from prior features: `run_grid_multi`, `best_robust_by_test`, `replay_multi`,
`replay_multi_mtm`, `risk_metrics`, `RiskMetrics`, `base_params`, the `GRID_*` constants,
`regime_mask`/`regime_mask_trend`.

## Data flow

```
load history + watched (K tokens) + base_params(.env); split train/test
  │
  ├─ global grid: run_grid_multi(N=1) & (N=K) → best-robust each → global best config G
  │     Arm A = best-robust @ N=1 (G_A);  Arm B = best-robust @ N=K (G_B)
  │     (metric/lookback/regime for per-token tuning taken from G_A — the single-name best)
  │
  ├─ tune_per_token (metric/lookback/regime fixed = G_A's): per token, single-token grid
  │     over {trail × max_run × min_metric-quantiles} → best {min_metric,trail,max_run} or None
  │
  └─ Arm C: in-memory watched_C = watched with per-token params set; config = G_A's
        metric/lookback/regime + (per-token overrides); replay_multi(N=K) on test
  │
  risk-metric A/B/C on test-slice MTM (equal capital, trade_usdc = pool/N per arm)
  │
  print per-token table + 3-arm P&L/Sharpe/trueDD table + VERDICT (SP3 gate)
  [--apply] write per-token params into momentum_tokens.json
```

Capital: equal `pool` (default `.env momentum_trade_usdc`, `--pool-usdc` override). Arm A
uses `trade_usdc = pool`; Arms B and C use `trade_usdc = pool / K`.

## CLI

```
momentum-sim per-token-tune
  [--pool-usdc N] [--min-trades 3] [--train-frac 0.70]
  [--tokens PATH] [--history PATH] [--max-step 8]
  [--regime-obs 0,480] [--regime-trend-obs 480]
  [--apply]                # also write per-token params into momentum_tokens.json
```

Output (sketch):
```
Per-token tuning — pool $8000, K=8 tokens. Train ~48d / Test ~21d.
Global best (single-name): metric=… lookback=… regime=…

Per-token best {min_metric, trail, max_run} (single-name grid, isolated test P&L):
  MET   min=… trail=…% max_run=…   test +…
  …
  PUMP  (no robust config → global fallback)

3-arm validation (equal $8000, held-out):
  arm                         test P&L   Sharpe  trueDD
  A single-slot (global)      +…         …       …%
  B hold-all (global config)  +…         …       …%
  C hold-all (per-token)      +…         …       …%

VERDICT (SP3 gate): per-token tuning <SUPPORTED|NOT SUPPORTED|MIXED> —
  C vs A: P&L Δ …, Sharpe … vs …. <one-line interpretation>
[--apply] wrote per-token params for N tokens to assets/momentum_tokens.json
```

## Testing

- `tune_per_token`: on a 2-token synthetic history where token A wants a tight trail and
  token B a wide one, the returned params differ per token in the expected direction; a
  token with no robust config returns `None`.
- `write_token_params`: round-trips a tokens file, sets params for one mint, leaves others
  untouched, and the result re-parses via `momentum_universe::load`.
- The CLI is a thin orchestrator (smoke-tested on real history; output captured). Arm A==
  the maxn-optimize N=1 number for the same config (cross-check).

## Out of scope (SP2)

- Writing global best to `.env` (stays with `optimize-momentum-config`).
- Wiring per-token tuning into the Python `optimize_momentum.py` skill (a thin follow-up;
  the Rust subcommand is the engine). Noted for later.
- The live trader (SP3 — gated on this verdict).

## Success criterion

`per-token-tune` prints each token's best `{min_metric, trail, max_run}` and the 3-arm
validation with a clear VERDICT; `--apply` persists per-token params into
`momentum_tokens.json` (re-parseable). The verdict is the decision input for SP3: build the
multi-position live trader only if per-token tuning makes Arm C beat Arm A (single-slot).
