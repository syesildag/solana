---
name: optimize-momentum-config
description: >-
  Grid-search the momentum trader over the curated token universe
  (assets/momentum_tokens.json) and update the MOMENTUM_* tuning variables in .env
  with the best held-out config. Use this whenever the user wants to tune, optimize,
  re-grid, or refresh the momentum trader's parameters — e.g. "optimize the momentum
  config", "run the full grid and update .env", "re-tune the momentum trader",
  "find the best momentum settings", "the watch list changed, re-optimize" — even if
  they don't say the word "grid". Specific to this repo's momentum-sim backtester.
---

# Optimize Momentum Config

Run the `momentum-sim` walk-forward grid over the curated watch list — a FULL scan whose
dimensions include the **regime gate** (off + level windows 240/480/720 + trend windows
240/480/720 × data-driven thresholds) — pick the fixed-trail config with the **highest
held-out (test-slice) net P&L** among robust configs: the winner maximizes `net_pnl_test`
(the most absolute money on unseen data), still gated to configs profitable in BOTH slices,
with ties broken by the healthier train slice (`--objective test-pnl`, the default).
Compare it head-to-head against what's currently in `.env`, and (after the user confirms)
write the winning values back into `.env`, **regime included**. The anti-overfit selection
(`--objective pareto` — best worst-slice SQN, prints the (P&L, trade-σ) frontier),
absolute-money worst-slice (`--objective net-pnl`), and capital-efficiency
(`--objective pnl-per-hold`) selections remain available. The grid runs at the
**slippage/cost configured in `.env`** (`MOMENTUM_SLIPPAGE_BPS` / `MOMENTUM_MAX_COST_BPS`),
printed in the run banner. **By default it
optimizes ONLY the global config (`.env`) and never touches `momentum_tokens.json`.**
Per-token optimization is **opt-in** via `--per-token` — it's off by default because
per-token tuning overfits this sample (the 3-arm validation comes back NOT SUPPORTED at
both single-slot and hold-all). When you do pass `--per-token`, the per-token step runs and,
with `--apply`, writes the per-token overrides into `momentum_tokens.json`.

## Why it works this way

- **Fixed-trail only.** The live momentum trader honors a fixed-% trailing stop and has no
  vol-stop (ATR/σ) env knob. So the grid runs with `--no-vol-stops`: a winner that relied
  on a vol-stop would look good on paper but the live trader couldn't reproduce it. Keeping
  the search to what the live trader can actually execute is the whole point.
- **11 knobs are auto-tuned:** `MOMENTUM_RANK_METRIC`, `MOMENTUM_MIN_METRIC`,
  `MOMENTUM_TRAIL_PCT`, `MOMENTUM_LOOKBACK_OBS`, `MOMENTUM_MAX_RUN_PCT`,
  `MOMENTUM_ROTATE_MARGIN`, the regime trio `MOMENTUM_REGIME_MODE` /
  `MOMENTUM_REGIME_OBS` / `MOMENTUM_REGIME_TREND_MIN`, and the overbought z-gate pair
  `MOMENTUM_ENTRY_MAX_Z_OBS` / `MOMENTUM_ENTRY_MAX_Z` — the parameters the grid optimizes
  *and* the live trader reads.
- **Regime AND the overbought z-gate are full grid dimensions, applied with the winner.**
  The regime sweep covers off, level (SOL>MA) at 240/480/720 obs, and trend (SOL slope_r2)
  at 240/480/720 obs × three train-quantile thresholds. The z-gate sweep covers off +
  z ∈ {1.0, 1.5, 2.0} over 480 obs by default (~4× grid; `--entry-max-z-obs 0` disables
  the dimension for a fast pass). A config's edge and its gates are selected together, so
  the winner's regime and z-gate are written on `--apply` — deploying the knobs without
  their gates would run an untested combination. The head-to-head's CURRENT row matches
  the live regime and z-gate exactly, so the comparison stays fair.
- **Selection = held-out test-slice P&L (default): the most absolute money on unseen data.**
  Among robust configs the winner maximizes `net_pnl_test` — the held-out slice alone — so
  the pick is the config that made the most money out-of-sample. Ties on the test slice
  (common — many configs share the same peak test P&L) are broken by the healthier **train**
  slice, so equal-test configs resolve to the more robust one (e.g. trail=12/train+50 over
  trail=10/train+26 at equal test+71.71) rather than an arbitrary first-seen row. The
  robustness gate still applies (train must also be profitable), which bounds the
  overfitting risk of selecting on the held-out slice — but it IS selecting on the test
  slice, so the output prints a `NOTE:` with both slices and a reminder to confirm the train
  slice and paper-test. For the anti-overfit pick use **`--objective pareto`**: it maximizes
  worst-slice **SQN** = `sqrt(n) × mean(trade P&L) / std(trade P&L)` (profits both large AND
  evenly distributed; a config carried by one +$200 outlier against a −$50 tail scores low)
  and prints the **(worst-slice P&L, trade-σ) PARETO FRONTIER** so the smoothness-vs-money
  trade is explicit. `--objective net-pnl` (worst-slice absolute P&L) and `--objective
  pnl-per-hold` (worst-slice $/hour-deployed) also remain available. The pareto objective
  requires a momentum-sim built with the `pnl_std_train/test` CSV columns (the script exits
  with a rebuild hint on old CSVs).
- **Execution assumptions come from `.env`.** The grid's `base_params` reads
  `MOMENTUM_SLIPPAGE_BPS` and `MOMENTUM_MAX_COST_BPS` from `.env` (via dotenv), so the scan
  optimizes at the fills you've configured for the live trader. Both are echoed in the run
  banner. To scan a different cost assumption, change `.env` (or prefix the run, e.g.
  `MOMENTUM_SLIPPAGE_BPS=15 python3 …`, which dotenv won't override).
- **Robustness gate.** Only configs profitable in BOTH the train and held-out slices (with
  enough trades in each) are eligible — this is what `momentum-sim` calls "ROBUST".
- **Per-token step (OPT-IN, off by default).** The default run optimizes only the global
  `.env` config and **does not touch `momentum_tokens.json`**. Per-token tuning is disabled
  by default because it overfits this sample — the 3-arm validation (single-slot global vs
  hold-all global vs hold-all per-token) comes back **NOT SUPPORTED** at both single-slot and
  hold-all, and a head-to-head at N=1 showed per-token params *underperform* the global
  config (test-optimized → train-slice collapse). Pass **`--per-token`** to run it anyway:
  it invokes `momentum-sim per-token-tune`, which grid-searches each token's best
  `{min_metric, trail_pct, max_run_pct}` in isolation (metric/lookback fixed at the global
  best), auto-tunes `regime_filter`/`exit_on_fade`/`reentry_cooldown_secs` (writes only
  non-default winners), leaves `trade_usdc` operator-set, prints the 3-arm verdict, and —
  with `--apply` — writes the per-token overrides into `momentum_tokens.json` (preserving
  existing entries). Use it for experiments, not as a default tuning step.

## Steps

1. **Preview (never writes).** Run the bundled script from the repo root:

   ```bash
   python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py
   ```

   It builds `momentum-sim` if needed (first build is slow), runs the grid, and prints: the
   robust-config count, a HEAD-TO-HEAD of the current `.env` config vs the grid's best
   (held-out test/train P&L, trades, win%, maxDD), the winner's regime (managed — part of
   the proposed changes), the exact proposed `.env` changes, and — at the end — a
   **TRADE LIST** of the winning config's individual round-trips (entry/exit time, token,
   entry/exit price, USDC in/out, P&L $/%, win rate) for the TRAIN and TEST slices.
   Nothing is written yet.

   The trade list is a **regime-off single-slot replay of the winning ParamSet's tradeable
   knobs** (from `momentum-sim run --dump-trades`). For the same trades WITH the regime
   gate applied, use `momentum-sim per-token --regime-mode … --regime-trend-min … --dump-trades`.

   Optional flags: `--min-trades N` (stricter robustness gate, default 3),
   `--objective <test-pnl|pareto|net-pnl|pnl-per-hold>` (winner selection; default
   `test-pnl` = highest held-out test-slice P&L, `pareto` = anti-overfit worst-slice SQN,
   `net-pnl` = worst-slice absolute P&L, `pnl-per-hold` = worst-slice $/hour-deployed),
   `--tokens <path>` (different watch list), `--csv <path>` (keep the full grid CSV),
   `--no-trades` (skip the trade listing — on by default),
   `--per-token` (also run the opt-in per-token step — off by default, see above).

   **Prefer the extended history when it exists** — the live file holds ≤30–45 days
   (one regime; configs picked on it can be regime specialists). If
   `assets/price_history.extended.jsonl` is present (built by `scripts/backfill_history.js`),
   prefix the run so the grid judges configs across regimes:

   ```bash
   HISTORY_PATH=assets/price_history.extended.jsonl HISTORY_MAX_SNAPSHOTS=300000 \
     python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py
   ```

2. **Show the user and decide.** Relay the head-to-head and the proposed changes. The
   script prints a `NOTE:` if the best config does **not** beat the current one
   out-of-sample — surface that prominently. If there are no changes, or the winner doesn't
   beat the incumbent, recommend keeping the current config and stop.

3. **Apply only on explicit confirmation.** If the user says go ahead, re-run with
   `--apply`. It backs up `.env` to `.env.bak` and rewrites only the changed `.env` lines
   (comments and all other vars preserved). `momentum_tokens.json` is **not touched** unless
   you ALSO pass `--per-token` (the opt-in per-token step):

   ```bash
   python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py --apply
   ```

4. **Report.** Confirm what changed in `.env` (before → after). `.env` is **gitignored
   (local only)** — nothing is committed. By default the run leaves `momentum_tokens.json`
   alone; only `--per-token --apply` writes per-token overrides (preserving existing
   entries, setting `params` on tuned mints). The trader picks up new values on its next
   config reload; the multi-slot trader (`MOMENTUM_MAX_POSITIONS>1`) consumes the per-token
   overrides. Paper mode if `DRY_RUN_MOMENTUM_TRADER=true`.

## Guardrails

- **Don't auto-apply without confirmation** unless the user explicitly asked for a
  one-shot/unattended update. The default is preview → confirm → apply.
- **A `.env.bak` is always written before any change** — if the user dislikes the result,
  restore with `cp .env.bak .env`.
- **State the caveat honestly.** A grid winner is a backtest optimum on a finite history
  (small trade counts, understated drawdown). It's a hypothesis to validate in paper mode,
  not a proven edge — especially right after the watch list changed, when newly-added
  tokens may have data in only one slice. If the user wants more assurance, suggest watching
  paper-mode results before trusting it live.
- **If the grid finds no robust config**, the script exits without touching `.env`. Don't
  hand-pick a non-robust config to force a change.
