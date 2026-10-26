# Time-based lookback for momentum entry signals — design

**Date:** 2026-10-26
**Status:** approved 2026-10-26 ("continue implementation"); plan in
`docs/superpowers/plans/2026-10-26-time-based-lookback.md`
**Scope:** momentum trader (`portfolio-watcher`) and the `momentum-sim` that validates it. No arb-path
changes (the arb bot is the flagship; nothing here touches its hot path).

## Why

The momentum trader's per-token params are tuned on a backtest "book" and then run live. They do not
mean the same thing in the two places, because **`lookback_obs` and `entry_max_z_obs` count rows, not
time**, and the two data sources have very different row densities:

- The book (`assets/price_history.book_*.jsonl`) holds a GeckoTerminal candle only for minutes in which
  the pool traded: HYPE ≈ 278 candles/day, JitoSOL ≈ 371/day, the liquid names ≥ 1,200/day.
- The live trader writes a row every ~60–90 s tick, and none while the host is asleep.

So a 480-row window spans a median **52.8 h** of HYPE in the book but **8 h** live. The bar that was
tuned on the first is applied to the second.

Evidence, 2026-09-13 → 2026-10-26 (`assets/entry_autopsy_2026-10-26/live_vs_sim/`):

| Measure | Result |
|---|---|
| Live P&L vs a replay of the same per-epoch configs on the same tokens | +$29 vs +$88 |
| Trades both took | live +$11 better (fills are fine) |
| Live entries taken while the **book** score was below the bar | **106 trades, −$109**: the whole gap |
| Rank correlation, live-trader score vs book score (rows, today) | HYPE 0.31, JitoSOL 0.27, BP/wNEAR/MET 0.66–0.70, dense tokens 0.85–0.94 |
| Same comparison with a **time** window (8 / 24 / 48 h) on the live recorder vs the book | **0.94–0.98 for every token** (`time_window_premise.txt`) |

The last row is the premise of this design: the two price sources agree once both are measured over the
same span of time. Only the row-counting disagrees.

A second, smaller inconsistency goes away with the same change. The live z-gate (`momentum::entry_dip_z`)
counts the token's **own** prices, while the sim's (`sim::token_dip_z`) counts **snapshots** (rows of any
token) and then filters to the token. For a sparse token those are different windows even on identical data.

## Goals

1. For a token that opts in, the live trader and the sim compute the **same entry score and the same
   z-score** from the same prices, whatever the row density, tick cadence or host sleep.
2. One implementation shared by live and sim, so they cannot drift.
3. Opt-in per token. A token without the new fields behaves **byte-identically** to today, in live and in
   every sim subcommand.
4. The tuning workflow (per-token sweep → apply → whole-book A/B) can search and apply the new fields.

## Non-goals

- The SOL regime window (`MOMENTUM_REGIME_OBS`). It is global, SOL is dense in both sources, and changing
  it moves every token at once. Follow-up if wanted.
- `confirm_lag_obs` (off live), `regime_exit_obs`, the spike-exit dynamic-bar windows, the pairs trader's
  lookback. All stay row-based.
- Global `.env` equivalents of the new fields. Every curated token carries per-token params; discovered and
  adopted tokens keep the row-based globals. (Builder may add them later; not needed for v1.)
- Fixing host sleep. That is an operations fix (a host that doesn't sleep). This design only makes sure a
  sleep gap cannot fake a signal.
- Re-tuning the whole book. Migration covers the tokens whose agreement is poor (below).

## Anti-goals (a failure even if it "works")

- Any change in behaviour for a token that has not opted in.
- Scoring a time window that the data does not actually cover: after a restart or a wake, a 52-h window
  computed on 2 h of prices is a fake signal and must read as *warming*, not as a score.
- A live-only or sim-only code path for the window selection.

## Decided design

### 1. Two new per-token params

In `TokenParams` (`src/portfolio/momentum_universe.rs`), both `Option<u32>`, `serde(default)`:

- `lookback_min`: the ranking-metric window in minutes. When set (> 0) it **replaces** `lookback_obs`
  for that token everywhere the metric window is used (ranking, the fade exit's score, the rank panel).
- `entry_max_z_min`: the overbought z-gate window in minutes. When set (> 0) it **replaces**
  `entry_max_z_obs` and **enables** the gate for that token, even if `entry_max_z_obs` is 0.
  `entry_max_z` (the threshold) is unchanged. Absent ⇒ today's `entry_max_z_obs` resolution, including
  `entry_max_z_obs: 0` = disabled.

### 2. Window rule (one helper, used by both live and sim)

Inputs: the token's `(ts, price)` series oldest-first, `now` = the timestamp of the newest history snapshot
(live: the current tick; sim: snapshot `i`), and a lookback that is either rows or minutes.

- **Rows:** exactly today's slice (last N points). Unchanged.
- **Minutes:** the points with `ts > now − L·60`. The window is **valid** only if
  (a) its oldest point is at or before `now − COVER·L·60`, with `COVER = 0.75` (my call: a named constant),
  and (b) it meets the existing point floor of the consumer. Invalid ⇒ the token is *warming*, exactly as
  a token with too few rows is today (not ranked; the rank panel shows it as warming).

The existing point floors stay in force on top of the time rule: the ranking metric needs
`SORTINO_MIN_OBS` (120) returns, and the z-score needs its own floor (30 points live, `MEANREV_MIN_OBS`
in the sim; the builder should make them one value). **Consequence for tuning:** a minutes value must
be long enough to hold 121 points on the sparsest source the token is validated on. HYPE in the book
needs ≥ ~10.5 h. The sweep must report a cell whose window cannot meet the floor as *insufficient*, not
as a zero-trade result.

**Consequence after sleep (corrected at review, 2026-10-26):** rule (a) checks only the OLDEST point,
so a token warms only while its data does not reach back to `now − COVER·L`: a fresh add, a restart
without history, or an outage longer than `L`. After a shorter sleep the window still holds pre-sleep
prices and scores across the gap (a 40-h sleep inside a 52.8-h window scores on 12.8 h of old prices
plus the fresh ticks). That is no worse than today's row window, which also spans the gap. While a
token is warming, entries pause and the fade exit cannot fire, but the trailing stop and every
price-based exit keep working. A stricter "the most recent part of the window must be covered too"
rule would warm a token after any long sleep; it is left as an operator decision (pinned by
`lookback::tests::pre_sleep_prices_still_cover_a_long_window_after_a_shorter_sleep`). The real fix is
a host that doesn't sleep.

### 3. Where it plugs in

- `momentum::rank_candidates` is already shared: `sim::ranked_stream` calls it. Resolve the token's
  lookback (minutes ?? rows ?? global rows) and select the window through the new helper.
- z-gate: replace the two implementations (`momentum::entry_dip_z` and `sim::token_dip_z`) with one
  helper over the token's own `(ts, price)` series that takes the same rows-or-minutes lookback. For the
  **rows** case each call site must keep its current semantics, so tokens that haven't opted in stay
  byte-identical (the sim keeps counting snapshots, live keeps counting own prices). Only the minutes
  path is shared. Include a test that pins both row behaviours.
- `sim::ranked_stream` sizes its trailing deque in snapshots off the largest `lookback_obs`. It must also
  retain at least the largest `lookback_min` of wall-clock time (pop by timestamp), or long time windows
  are silently truncated, which is the starvation bug that sizing rule exists to prevent.
- Live cold backfill (`watcher::backfill_watched_cold` / `fetch_cold_candles`) requests
  `lookback_obs + 240` minutes, capped at 7 days. It must request enough to cover the largest
  `lookback_min` in use (same 7-day cap).
- Live history depth is not a constraint: the in-memory deque holds `MAX_HISTORY = 43,200` snapshots
  (~30 days at 1/min).

### 4. Tooling

- `momentum-sim per-token-sweep`: a `--lookback-mins` axis, used *instead of* `--lookbacks` in a run (the
  two axes are not mixed in one grid), and a single-value `--entry-max-z-min` (instead of
  `--entry-max-z-obs`). **The unit rides in the label value, not a new key** (my call, so the skill's
  Python keeps one `lb` knob): `lb=3170m`, `z=1.5@3170m`; rows stay `lb=480`, `z=1.5@480`. The pasted
  params JSON carries `lookback_min` / `entry_max_z_min`.
- Skill scripts in `.claude/skills/optimize-momentum-tokens/scripts/` (`common.py` `SWEPT_KEYS` /
  `deployed_knobs` / label parsing, `apply_params.py`, `book_ab.py` `one_cell_axes`) recognise
  `lookback_min` and `entry_max_z_min`, so a deployed minutes row is read back as the incumbent and
  can be applied.
- `maxn-compare`, `forward-report` and every subcommand that loads the tokens file pick the fields up
  automatically through `TokenParams`; nothing else to wire.
- Rank panel: show the window in hours for a minutes token (e.g. `lb=52.8h`) so live logs show which
  semantics a token is on. Format is the builder's call.

### 5. Migration (sequence)

**A. Build and test**, with nothing deployed. Unit tests (bottom-of-file `#[cfg(test)]`, repo convention):

- On a series with exactly one row per minute, a minutes window equals the rows window of the same length.
- Coverage rule: a window whose oldest point is younger than `now − COVER·L` is invalid; a hole in the
  middle of a covered window is fine.
- A live-vs-sim parity test: the same `(ts, price)` series gives the same score and z through the live
  call path and the sim call path.
- No-opt-in regression: a tokens file without the new fields yields identical `ranked_stream` output and
  identical replay trades before and after the change.

**B. Equivalence on the book.** For each target token set `lookback_min` (and `entry_max_z_min` where the
token's z-gate is on) to the median book span of its current rows window, measured 09-22 → 10-25:

| Token | Current rows | Median book span | Starting `lookback_min` |
|---|---|---|---|
| HYPE | lb 480, z 480 | 52.8 h | ≈ 3170 |
| JitoSOL | lb 480, z 480 | 30.9 h | ≈ 1850 |
| BP | lb 720, z off | 22.7 h | ≈ 1360 |
| wNEAR | lb 240, z 480 | 7.9 h | ≈ 475 |
| MET | lb 240, z 480 | 8.9 h | ≈ 535 |

(z spans are measured the same way at the z row count.) **Caveat found at review:** the sim's trailing
deque is sized in book ROWS (`(max lookback + lag)·3 + 50` snapshots), and a sparse token's own points
don't fill its lookback inside it, so the sim has been scoring HYPE over ~2,210 book rows (~36.8 h,
426 points) in whole-book replays and ~1,490 rows (~24.8 h) in isolated per-token sweeps, not the
52.8 h above. Re-derive the starting values from the window the sim actually used (whole-book vs
isolated) before converting; the live trader, whose history is long, is not starved. Replay the whole book with the converted file vs
the deployed one (`book_ab.py`). Book-side results should be close but not identical, because the rows span
varies over time while the minutes span is fixed. A large difference means the converted value is not an
equivalent and must not be deployed blind.

**C. Re-tune** the converted tokens with a `--lookback-mins` axis using the normal skill rules (full
factorial, disjoint folds, min-trail pick, whole-book A/B). Dense tokens (ZEC, Jupiter, KMNO, CATE, STONK,
RAY) agree at 0.85–0.94 already and are not migrated in v1.

**D. Deploy and verify.** Watcher restart required (config is read once at startup). Verification, all
offline from existing logs:

1. Within days: `score_agreement.py` on live `RankSnapshot` vs the book. HYPE and JitoSOL rank correlation
   must rise from ~0.3 to ≥ 0.9.
2. After ~4 weeks: re-run the live-vs-sim comparison (`run_epochs.py` / `compare.py` / `attribute.py`).
   For migrated tokens, live entries taken while the book score is below the bar should be near zero.
   Today that bucket is 106 trades and −$109 across the book.

## Constraints and repo conventions

- Never run `cargo fmt` / `rustfmt` on a whole file; the repo isn't rustfmt-clean.
- Tests live in `#[cfg(test)]` blocks at the bottom of each source file; `cargo test --lib sim::` /
  `cargo test --lib momentum`.
- Any validation run on a history file must set `HISTORY_MAX_SNAPSHOTS=100000000` in the same environment
  (the loader can truncate the file), and ad-hoc analysis reads `momentum-sim sanitize-dump` output, never
  raw JSONL.
- `momentum_tokens.json` edits go through `apply_params.py` (it writes the `.pre_*.bak` that the config
  timeline relies on). Hand edits without a backup broke the epoch reconstruction this design is based on.
- Paper-test before live is the house rule for trader changes.

## Left to the builder

Helper and type names (an enum `Lookback { Obs, Minutes }` or two fields), log format, CLI flag spelling,
how the shared z floor is named, test layout, whether `COVER` is a `const` or a per-token param (`const`
is enough for v1).

## Decided at approval (my calls, operator said "continue implementation")

- The z-gate window (`entry_max_z_min`) is in v1: HYPE and JitoSOL both run the z-gate, so parity on the
  score alone would leave them half-mismatched.
- Regression proof for goal 3 is a byte-identical `maxn-compare --dump-trades` replay of the deployed
  tokens file on a frozen 60-day book slice, before vs after.

## Still open for the operator (not blocking the build)

- While the fix is built, cut HYPE and JitoSOL from $500 to $100 per trade, and set STONK and Jupiter to
  watch-only? (Both live and a trustworthy replay lose on the last two.)
- Deploy migrated tokens straight to live after the A/B, or paper first? There is only a global
  `DRY_RUN_MOMENTUM_TRADER`, so paper means pausing real trading for every token.
