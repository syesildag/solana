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

Run the `momentum-sim` walk-forward grid over the curated watch list, pick the fixed-trail
config with the best **total net P&L** — highest worst-slice absolute net P&L
(`--objective net-pnl`, the default) — compare it head-to-head against what's currently in
`.env`, and (after the user confirms) write the winning values back into `.env`. A
capital-efficiency selection (`--objective pnl-per-hold`, worst-slice $/hour-deployed) is
available opt-in, but it optimizes efficiency, **not** total money, so it can pick a config
that makes far fewer dollars — use it deliberately, not by default. The grid runs at the
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
- **Only 6 knobs are auto-tuned:** `MOMENTUM_RANK_METRIC`, `MOMENTUM_MIN_METRIC`,
  `MOMENTUM_TRAIL_PCT`, `MOMENTUM_LOOKBACK_OBS`, `MOMENTUM_MAX_RUN_PCT`,
  `MOMENTUM_ROTATE_MARGIN` — the parameters the grid optimizes *and* the live trader reads.
- **The grid runs at 2× the configured slippage.** The script overrides
  `MOMENTUM_SLIPPAGE_BPS` to double the `.env` value for the momentum-sim subprocess
  (shell env → `.env` → sim default 50, then ×2). A config that is only profitable at
  your best-case fill isn't worth deploying — optimizing under deliberately pessimistic
  execution keeps the winner honest, and live fills still happen at the real (lower)
  `.env` slippage. Both the incumbent and every candidate are judged at the same doubled
  cost, so the head-to-head stays fair. The script warns if 2× slippage approaches
  `MOMENTUM_MAX_COST_BPS` (the cost gate would block all entries). The per-token step
  runs under the same override.
- **Regime is reported, not flipped.** `MOMENTUM_REGIME_MODE/OBS/TREND_MIN` express a
  deliberate strategic stance. The script prints the winner's regime as advisory; it never
  silently changes it. If the winner's regime differs and the user wants it, set it by hand.
- **Selection = worst-slice total net P&L (default).** Among robust configs the winner
  maximizes `min(net_pnl_train, net_pnl_test)` — the most total money it *dependably* makes
  across both slices, at N=1. This is the plain "best P&L" objective. `--objective
  pnl-per-hold` instead maximizes worst-slice `$/hour-deployed` (`net_pnl / hold_hours`),
  a capital-efficiency proxy that favors short holds and can pick a config making far fewer
  dollars — opt-in only. The script prints a `NOTE:` when the winner's worst-slice P&L does
  not beat the incumbent's, so a no-improvement result is always visible.
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
   (held-out test/train P&L, trades, win%, maxDD), the winner's advisory regime, the
   exact proposed `.env` changes, and — at the end — a **TRADE LIST** of the winning
   config's individual round-trips (entry/exit time, token, entry/exit price, USDC in/out,
   P&L $/%, win rate) for the TRAIN and TEST slices. Nothing is written yet.

   The trade list is a **regime-off single-slot replay of the winning ParamSet** — i.e. the
   exact tradeable knobs the optimizer writes to `.env` (it does not apply the advisory
   regime gate, which is operator-set). It comes from `momentum-sim run --dump-trades`,
   which replays the most-dependable (worst-slice) robust config and prints each trade.

   Optional flags: `--min-trades N` (stricter robustness gate, default 3),
   `--objective pnl-per-hold` (opt-in capital-efficiency selection = worst-slice
   $/hour-deployed; default is `net-pnl` = worst-slice total P&L),
   `--tokens <path>` (different watch list), `--csv <path>` (keep the full grid CSV),
   `--no-trades` (skip the trade listing — on by default),
   `--per-token` (also run the opt-in per-token step — off by default, see above).

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
