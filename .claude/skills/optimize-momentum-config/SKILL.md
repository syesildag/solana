---
name: optimize-momentum-config
description: >-
  Use when the user wants to tune, optimize, re-grid or refresh the momentum trader's GLOBAL
  knobs — the MOMENTUM_* variables in .env (rank metric, min metric, trail, lookback, regime
  mode/window/threshold, overbought z-gate, max run, rotate margin) — e.g. "optimize the
  momentum config", "re-run the global grid", "is the regime gate still right", "the rank
  metric changed, re-grid the .env" — even without the words grid or backtest. Specific to
  this repo's momentum-sim `run` grid via scripts/optimize_momentum.py. For the per-token
  `params` blocks in assets/momentum_tokens.json (any token, any trail comparison, "re-tune
  HYPE") use optimize-momentum-tokens instead.
---

# Optimize Momentum Config

One artifact, one tool: the **global knobs in `.env`** → the `run` grid via
`scripts/optimize_momentum.py` (section "Steps" below). Every rule here was a real failure; do
not skip them.

**Per-token `params` blocks (`assets/momentum_tokens.json`) moved to the
`optimize-momentum-tokens` skill (2026-09-27).** Its `references/reading-rules.md` now holds the
per-token reading rules that used to live here (trust rules 0–5, walk-forward rule 2b, the `fb=`
axis, the 2026-09-06 verification log), and its scripts enforce them: isolated single-slot jobs
at each token's measured cost, five disjoint time windows, trust gates and one REPORT.md per run.
Use that skill for any per-token question ("re-tune HYPE", "best trail for JitoSOL", "the watch
list changed"). One rule still binds the two skills together — the **unit-scale law**: a change
of `MOMENTUM_RANK_METRIC` (or the global lookback) here invalidates every per-token `min_metric`
at once, so re-run optimize-momentum-tokens for ALL tokens in the same session.

## Global grid procedure

The live trader runs multi-slot (`MOMENTUM_MAX_POSITIONS`≥2) with per-token overrides: the
**global config → `.env`** (metric/lookback are global-only; trail/min/max_run/regime/z are the
defaults every token without an override inherits) and the per-token `params` blocks, which
belong to optimize-momentum-tokens. Every rule below was a real failure on 2026-07-23:

0. **History BEFORE the grid, gated on coverage.** Never grid the 30-day live file. The
   optimize-momentum-tokens skill builds and validates a ≥150-day book
   (`python3 .claude/skills/optimize-momentum-tokens/scripts/ensure_history.py --build`:
   pinned pools, one series at a time, `--no-splice`, no plain `"SOL"` key, chmod 444 + gz
   backup) — reuse that book here: `HISTORY_PATH=<book> HISTORY_MAX_SNAPSHOTS=100000000`, in the
   same command as the binary (the loader truncates its input without it).
1. **Distrust the default test-pnl pick when slices are asymmetric.** Read the winner's
   trade list first: a token that exists ONLY in the test slice (launched mid-window)
   can carry the whole test P&L (Jimothy's launch week was +808 of +832). Prefer the
   sim's dependability (worst-slice) winner whenever the test-pnl pick's train slice is
   thin (it was +20 vs +274 on 2026-07-23), and say which trades dominate.
2. **Unit-scale law:** per-token `min_metric` is denominated in the GLOBAL metric's units.
   Changing `MOMENTUM_RANK_METRIC` or `MOMENTUM_LOOKBACK_OBS` silently invalidates EVERY
   per-token `min_metric` (a return-units 0.2353 becomes a near-zero slope_r2 bar) ⇒ re-run
   optimize-momentum-tokens for all tokens in the same session, no exceptions.
3. **Apply + rollout:** global via the managed-knob rewrite (backup `.env.bak`); multi-slot
   changes stay `DRY_RUN_MOMENTUM_TRADER=true` first (repo rule) and the watcher needs a
   restart to load any of it.
4. **Do not use this script's `--per-token` path.** It runs the legacy `per-token-tune`,
   whose `--apply` rewrites each token's `params` wholesale — dropping `lookback_obs`, the
   z-gate and `regime_exit_obs`. Per-token work belongs to optimize-momentum-tokens, whose
   `apply_params.py` changes only the six swept knobs.

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
printed in the run banner. It optimizes ONLY the global config (`.env`) and never touches
`momentum_tokens.json` — per-token work is optimize-momentum-tokens' (rule 4 above: do not use
the script's legacy `--per-token` flag).

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
- **The script's `--per-token` flag is legacy — do not use it.** It invokes `momentum-sim
  per-token-tune` (whose 3-arm validation came back NOT SUPPORTED), and its `--apply` replaces
  each token's `params` wholesale, dropping `lookback_obs`, the z-gate and `regime_exit_obs`.
  All per-token tuning goes through optimize-momentum-tokens.

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
   `--no-trades` (skip the trade listing — on by default). Never `--per-token` (legacy, see
   above).

   **Use a ≥150-day book, never the live file** — the live file holds ≤30–45 days (one regime;
   configs picked on it can be regime specialists). Reuse the newest
   `assets/price_history.book_*.jsonl` built by optimize-momentum-tokens' `ensure_history.py`:

   ```bash
   HISTORY_PATH=$(ls assets/price_history.book_*.jsonl | tail -1) HISTORY_MAX_SNAPSHOTS=100000000 \
     python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py
   ```

2. **Show the user and decide.** Relay the head-to-head and the proposed changes. The
   script prints a `NOTE:` if the best config does **not** beat the current one
   out-of-sample — surface that prominently. If there are no changes, or the winner doesn't
   beat the incumbent, recommend keeping the current config and stop.

3. **Apply only on explicit confirmation.** If the user says go ahead, re-run with
   `--apply`. It backs up `.env` to `.env.bak` and rewrites only the changed `.env` lines
   (comments and all other vars preserved). `momentum_tokens.json` is not touched:

   ```bash
   python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py --apply
   ```

4. **Report.** Confirm what changed in `.env` (before → after). `.env` is **gitignored
   (local only)** — nothing is committed. The trader picks up new values on its next config
   reload (the operator restarts it). If `MOMENTUM_RANK_METRIC` or the global lookback changed,
   say that every per-token bar is now stale and run optimize-momentum-tokens for all tokens.
   Paper mode if `DRY_RUN_MOMENTUM_TRADER=true`.

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
