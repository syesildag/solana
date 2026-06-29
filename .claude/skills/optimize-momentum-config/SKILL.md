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

Run the `momentum-sim` walk-forward grid over the curated watch list, pick the most
dependable fixed-trail config, compare it head-to-head against what's currently in `.env`,
and (after the user confirms) write the winning values back into `.env`. By default it then
also optimizes the **per-token** params (each token's best `{min_metric, trail_pct,
max_run_pct}`) and, with `--apply`, writes them into `momentum_tokens.json` — so one run
tunes both the global config (`.env`) and the per-token overrides (which the multi-slot
live trader consumes, falling back to global where absent).

## Why it works this way

- **Fixed-trail only.** The live momentum trader honors a fixed-% trailing stop and has no
  vol-stop (ATR/σ) env knob. So the grid runs with `--no-vol-stops`: a winner that relied
  on a vol-stop would look good on paper but the live trader couldn't reproduce it. Keeping
  the search to what the live trader can actually execute is the whole point.
- **Only 6 knobs are auto-tuned:** `MOMENTUM_RANK_METRIC`, `MOMENTUM_MIN_METRIC`,
  `MOMENTUM_TRAIL_PCT`, `MOMENTUM_LOOKBACK_OBS`, `MOMENTUM_MAX_RUN_PCT`,
  `MOMENTUM_ROTATE_MARGIN` — the parameters the grid optimizes *and* the live trader reads.
- **Regime is reported, not flipped.** `MOMENTUM_REGIME_MODE/OBS/TREND_MIN` express a
  deliberate strategic stance. The script prints the winner's regime as advisory; it never
  silently changes it. If the winner's regime differs and the user wants it, set it by hand.
- **Robustness gate.** Only configs profitable in BOTH the train and held-out slices (with
  enough trades in each) are eligible — this is what `momentum-sim` calls "ROBUST".
- **Per-token step (default on).** After the global grid, the script invokes
  `momentum-sim per-token-tune`, which grid-searches each token's best `{min_metric,
  trail_pct, max_run_pct}` in isolation (metric/lookback fixed at the global best; regime
  off) and runs a 3-arm validation (single-slot global vs hold-all global vs hold-all
  per-token) printing a verdict. With `--apply` it writes the per-token params into
  `momentum_tokens.json`. Pass `--no-per-token` to optimize the global `.env` config only.
  (Note: `per-token-tune` re-grids the global config internally for its validation arms, so
  the global grid runs twice in a full invocation — fast, and keeps both tools
  self-contained.) Per-token `regime_filter` (opt out of the global SOL regime gate) is
  now **auto-tuned**: `per-token-tune` grids each token exempt (regime off) vs gated
  (global SOL regime) and writes `regime_filter: false` for tokens that do robustly better
  exempt; tokens where gated wins or neither is robust retain `regime_filter: null` (obey
  global). `exit_on_fade` and `reentry_cooldown_secs` are **also auto-tuned** per token (a
  tiny fade×cooldown ladder swept alongside the grid; only a non-default winner is written,
  else the field stays `null` = use global). **`trade_usdc` is operator-set** — `per-token-tune`
  preserves any hand-set per-token size but never writes it (auto-tuning position magnitude
  overfits a finite sample), so size by conviction/volatility by hand.

## Steps

1. **Preview (never writes).** Run the bundled script from the repo root:

   ```bash
   python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py
   ```

   It builds `momentum-sim` if needed (first build is slow), runs the grid, and prints: the
   robust-config count, a HEAD-TO-HEAD of the current `.env` config vs the grid's best
   (held-out test/train P&L, trades, win%, maxDD), the winner's advisory regime, and the
   exact proposed `.env` changes. Nothing is written yet.

   Optional flags: `--min-trades N` (stricter robustness gate, default 3),
   `--tokens <path>` (different watch list), `--csv <path>` (keep the full grid CSV).

2. **Show the user and decide.** Relay the head-to-head and the proposed changes. The
   script prints a `NOTE:` if the best config does **not** beat the current one
   out-of-sample — surface that prominently. If there are no changes, or the winner doesn't
   beat the incumbent, recommend keeping the current config and stop.

3. **Apply only on explicit confirmation.** If the user says go ahead, re-run with
   `--apply`. It backs up `.env` to `.env.bak`, rewrites only the changed `.env` lines
   (comments and all other vars preserved), **and** writes the best per-token params into
   `momentum_tokens.json` (deduped, entries preserved) unless `--no-per-token` was passed:

   ```bash
   python3 .claude/skills/optimize-momentum-config/scripts/optimize_momentum.py --apply
   ```

4. **Report.** Confirm what changed in `.env` (before → after) and that per-token params
   were written to `momentum_tokens.json`. Both `.env` and `momentum_tokens.json` are
   **gitignored (local only)** — nothing is committed. `--apply` preserves all existing
   `momentum_tokens.json` entries (including hand-added ones and manual overrides), only
   setting `params` on the tuned mints. The trader picks up the new values on its next
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
