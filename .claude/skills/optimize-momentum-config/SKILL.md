---
name: optimize-momentum-config
description: >-
  Use when the user wants to tune, optimize, re-grid, sweep or refresh the momentum
  trader's parameters — the global MOMENTUM_* knobs in .env or the per-token `params`
  blocks in assets/momentum_tokens.json — e.g. "optimize the momentum config", "sweep
  per-token params", "what is the best min_metric/trail/lookback for JitoSOL", "show me
  best $/hour / max pnl / least drawdown / pareto combinations", "the watch list changed,
  re-optimize", "re-tune HYPE" — even without the words grid or backtest. Specific to
  this repo's momentum-sim (`run`, `per-token-sweep`, `per-token-tune`, `maxn-compare`).
---

# Optimize Momentum Config

Two artifacts, two tools. **Global knobs (`.env`)** → the `run` grid via
`scripts/optimize_momentum.py` (section "Steps" below). **Per-token `params` blocks
(`assets/momentum_tokens.json`)** → `momentum-sim per-token-sweep` (this section — the
default per-token procedure since 2026-09-06; `per-token-tune` remains for the 3-arm
validation question). Every rule here was a real failure; do not skip them.

## Per-token multi-objective sweep (`per-token-sweep`)

**What it does.** For ONE token it replays the WHOLE book — every other token pinned at its
live `params`, `MOMENTUM_MAX_POSITIONS` slots (or `--max-n`), the `.env` globals and cost — over
the full factorial `min_metric × trail_pct × lookback_obs × z-gate × regime_filter`, then
prints the incumbent row and the top rows under FIVE objectives (max test P&L, best worst-slice
P&L, best worst-slice $/hour, least test drawdown, best test SQN), the (worst-slice P&L ↑,
test trade-σ ↓) Pareto frontier, a **consensus** list (families in the top-N of ≥2
objectives), and a ready-to-paste `params` JSON per objective winner. Identical outcomes are
collapsed into **families** whose label lists the interchangeable knob values —
`trail={10,15,20,30}` means the trail never bound on those trades (an inert knob; keep the
incumbent's value), a single value means the knob is load-bearing. Book-level numbers by
design: an isolated $/hour is a mirage (2026-08-01), so the `tok` column shows the token's own
contribution next to the book's. Full factorial by design: 1-D sweeps gave three wrong
answers on 2026-07-29.

**Run (one token, one history file):**

```bash
HISTORY_MAX_SNAPSHOTS=1000000 target/release/momentum-sim per-token-sweep \
  --history assets/price_history.<token-file>.jsonl --token JitoSOL \
  --trade-usdc 1000 --max-n 1 --top 3 --csv /tmp/sweep_jitosol.csv
# defaults: min_metric = incumbent bar × {0.5,0.75,1,1.5,2}; --trails 10,15,20,30;
# --lookbacks 240,480,720,1440; --entry-max-zs 0,1.0,1.5 (@480); regime gated|exempt;
# override any of them with the same-named comma lists; --no-regime-sweep pins the incumbent.
```

Rules that decide whether the table is trustworthy:

0. **The history file, not the live file.** `assets/price_history.jsonl` is a 30-day rolling
   window of one regime. Use the validated per-token files (`price_history.jitosol_0829_clean`,
   `price_history.hypezec_0829`, …; `chmod 444` them) and ALWAYS export
   `HISTORY_MAX_SNAPSHOTS=1000000` — without it `load_history` truncates the file to 43,200
   rows and silently invalidates the sweep. Gate before sweeping: the token's mint must be
   present in the file's rows (`tail -n1 <file> | jq '.prices|keys'`), and the SOL series must
   span the file (the regime gate is blind otherwise). Glitch-scan a new file first (a
   1.5× spike-and-revert print inverted the JitoSOL rankings on 2026-08-29).
1. **Read the consensus list first, not the P&L column.** A family in the top-3 of ≥2
   objectives with a positive `d_test` AND a train slice at least as good as the incumbent's is
   a candidate. A family that tops ONE objective (typically "max test P&L" or "least drawdown")
   and sits mid-table elsewhere is a specialist — the least-drawdown winner is often a
   5-trade 100%-win row with a near-zero train slice.
2. **Both slices, then a second cut.** Re-run the finalists at `--train-frac 0.8`: a config
   whose held-out win depends on an open position straddling the slice end flips sign there
   (boundary-straddling artifact, 2026-07-27). If one trade carries a slice, delete-the-event
   (the `--csv` plus the `trades` subcommand) and require the residual to stay positive.
3. **Inert knobs keep the incumbent value.** When a family lists several values for a knob,
   the pasted JSON already resolves to the incumbent's value if it is in the set; do not
   "tighten" an inert trail because a narrower number looks safer — it changes nothing on the
   sample and is untested off it.
4. **Unit-scale law.** `min_metric` is denominated in the GLOBAL metric's units
   (`MOMENTUM_RANK_METRIC`); a global metric change invalidates every per-token bar — re-sweep
   all tokens in the same session. Lookback IS per-token (since 2026-07-24) and is the knob most
   often load-bearing; it counts observations, not time (mixed cadence regimes in the history —
   check reachability of the bar in the CURRENT cadence before trusting a long lookback).
5. **What the sweep cannot see.** No volume, no order flow, no discovered/adopted mints (not
   recorded), and the fixed `.env` cost (`MOMENTUM_SLIPPAGE_BPS`) for every token — an LST's
   real cost is ~5–10 bps, a thin meme's 50+; sweep a meme at its realistic cost via a
   `MOMENTUM_SLIPPAGE_BPS=50` prefix. `MOMENTUM_REENTRY_COOLDOWN_SECS` is `.env`-frozen and
   global (a 300 s cooldown lifted the 2026-09-06 HYPE+ZEC baseline +617/+766 → +701/+799 —
   sweep it via the env prefix, not per token).

**Apply (only on explicit confirmation).** Back up the tokens file
(`cp assets/momentum_tokens.json scratchpad/momentum_tokens.pre_<token>_<date>.bak`), paste
the chosen objective's JSON into that entry's `params` block preserving `pool`/`quote`/`name`
(never the whole entry), keep operator-set fields (`trade_usdc`, `regime_exit_obs` for LSTs),
then **restart the watcher** (params load at startup only). Multi-slot or notional changes
stay `DRY_RUN_MOMENTUM_TRADER=true` first (repo rule). Record the applied row and its
train/test numbers in the session memory (`project_momentum_met_bp_config`).

**Verification (2026-09-06; tables in `assets/per_token_sweep_2026-09-06/`; nothing applied —
all await the 0.8 cut):** JitoSOL (clean 80 d, N=1, $1000): incumbent `min 3.4/trail 10/lb 720`
+250/+191; consensus `min 2.55 lb 480` +261/+297 (+106 held-out, 12 trades) and `min 1.7 lb 720`
+272/+273; trail 10–30 inert everywhere, z and the regime exemption never bind. HYPE (183 d
HYPE+ZEC book, N=2, base +1608/+960): [4/5 objectives] `min 4.875 trail 30 lb 1440 z off`
+1835/+1023 (+62 held-out, +228 train, trueDD 1.77 vs 3.77, 46 trades vs 71) — trail 30 is
load-bearing here (single value), the live z1.0@480 gate is not. ZEC (same book): [2/5]
`min 5.85 trail 30 lb 240 z off` +1623/+1029 (+69/+16); the incumbent already sits on the
frontier. Every winner is a LOWER bar than the incumbent — the monotone "lower = better both
slices" response seen since July — so the 0.8 cut matters more than usual.

## Global grid + legacy per-token procedure

## Multi-slot + per-token procedure (the deployed architecture since 2026-07-23)

The live trader runs **multi-slot (`MOMENTUM_MAX_POSITIONS`≥2) with per-token param
overrides**, so a full optimization produces TWO artifacts: the **global config → `.env`**
(metric/lookback are global-only; trail/min/max_run/regime/z are global defaults) and
**per-token overrides → `momentum_tokens.json` `params` blocks**
(`min_metric`/`trail_pct`/`max_run_pct`/`entry_max_z_obs`+`entry_max_z`, where
`entry_max_z_obs: 0` = z-gate exempt; `regime_filter`/`trade_usdc` stay operator-set).
Every rule below was a real failure on 2026-07-23; do not skip them:

0. **Backfill BEFORE the grid, then gate on coverage.** Build/refresh a combined
   extended history for the CURRENT list: `scripts/backfill_history.js --days 150
   --no-splice --tokens "MINT::PINNED_POOL,…"` per new token (ALWAYS pin each token's
   wired pool — volume-ranked auto-pick chose a 5-week-old JitoSOL pool and produced a
   150d file with no head; the sim then dies with "grid produced no results"), then
   ts-union-merge with the existing good file (donor series are immutable GT candles).
   Acceptance gate before any grid: `grep -c <mint>` per token + first/last-row spans.
   Run everything with `HISTORY_PATH=<file> HISTORY_MAX_SNAPSHOTS=300000`.
1. **Distrust the default test-pnl pick when slices are asymmetric.** Read the winner's
   trade list first: a token that exists ONLY in the test slice (launched mid-window)
   can carry the whole test P&L (Jimothy's launch week was +808 of +832). Prefer the
   sim's dependability (worst-slice) winner whenever the test-pnl pick's train slice is
   thin (it was +20 vs +274 on 2026-07-23), and say which trades dominate.
2. **`per-token-tune` can bail** ("No robust single-slot (N=1) config — cannot establish
   a global baseline"). Fall back to the hand-rolled isolation sweep: `momentum-sim
   per-token` × ~12 configs — `min_metric` ∈ {½×, 1×, 2× global} × trail {20, 30} ×
   z {global, off} — with metric/lookback fixed at the global winner and a
   **params-stripped copy of the tokens file** (existing overrides contaminate the
   sweep). zsh does not word-split `$VAR`: iterate pairs as `"480:1.0"` and split with
   `${Z%%:*}`/`${Z##*:}`. Select per token by worst-slice P&L.
3. **Per-token verdict rules:** a token with test-only data gets **no params** (tuning
   it = fitting one week); a token negative in EVERY sweep config gets the least-bad
   HIGH bar plus an explicit "evidence says watch-only" flag in the report; an LST/majors
   token keeps params derived from its OWN single-token grid at its OWN realistic cost
   (~10 bps) — a 50 bps pump-cost sweep cannot refute a 10 bps-validated override.
4. **Unit-scale law:** per-token `min_metric` is denominated in the GLOBAL metric's
   units over the GLOBAL lookback. Changing `MOMENTUM_RANK_METRIC` or
   `MOMENTUM_LOOKBACK_OBS` silently invalidates EVERY existing per-token `min_metric`
   (a return-units 0.2353 becomes a near-zero slope_r2 bar). Any global metric/lookback
   change ⇒ re-derive every per-token bar in the same run, no exceptions.
5. **Apply + rollout:** global via the managed-knob rewrite (backup `.env.bak`);
   per-token params written preserving each entry's `pool`/`quote`/`name`; multi-slot
   changes stay `DRY_RUN_MOMENTUM_TRADER=true` first (repo rule) and the watcher needs
   a restart to load any of it.

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
