# Reading the report — trust gates, flags, axes, verdict rules

Read this before writing any verdict. The scripts compute everything that can be computed; this
file is for the judgment they cannot make. Most of it moved here from optimize-momentum-config
(2026-09-27) — every rule was a real failure.

## Contents
1. What a row is
2. Trust gates (token level)
3. Row flags
4. Decision axes
5. Verdict rules
6. Judgment calls the scripts cannot make
7. What the sweep cannot see
8. Output of the sweep binary to ignore

## 1. What a row is

A **cell** = one combination of the six swept knobs for one token: `min_metric` (incumbent ×
{0.5, 0.75, 1, 1.5, 2}), `trail_pct`, `lookback_obs`, the overbought z-gate (off / z@480),
`regime_filter` (gated / exempt) and `fb` (green fade bar as a fraction of the row's own
`min_metric`; the pasted JSON carries the ABSOLUTE bar `round4(fb × min)`, `fb=1` ⇒ no key).
Every other field of the token's entry (`trade_usdc`, `regime_exit_obs`, `max_run_pct`,
`exit_on_fade`, cooldown, spike-exit fields) stays at its deployed value — the report prints
them as "fixed (not swept)".

Each cell is replayed **alone** (one-token book, one slot) at the token's measured cost, on:
`full` (0.7 train/test split), `f0` (front half), `f1..f5` (five disjoint back-half windows) and
`cost3x`. Within a trail rung, cells with identical outcomes everywhere collapse into a
**family**; its label lists interchangeable values — `lb={240,480}` means the lookback never
changed a trade (an inert knob), a single value means the knob is load-bearing. The paste-ready
params resolve a family to its MEMBER closest to the deployed knobs, so they always name a cell
that was replayed. Rungs with identical outcomes collapse into a class represented by the
deployed trail when it belongs to the class (an inert trail keeps its deployed value).

`DEPLOYED@T` = the deployed knobs at trail T: the pure trail ladder of the incumbent. At the
deployed trail it must equal the exact `INCUMBENT` row (T1).

## 2. Trust gates (token level)

| gate | fails when | why it exists |
|---|---|---|
| **T0 data** | FAIL: <60 d; no coverage entry; a back window without the token's own prints; rows = 43,200 (truncation signature); a plain `"SOL"` key. WARN: WSOL does not span the book; glitch candidates; sanitizer removed >1%; <60 prints/day; <100 prints in a window; a job CSV missing; weekly cadence shift >2×; book older than 7 d; token starts late | old rule 0 (history file, not the live file; glitch-scan first — a 1.5× JitoSOL print inverted rankings 2026-08-29); the sanitizer-bypass memory (stats are on `sanitize-dump` output); `sanitize_pegged` mis-fires at 1-min cadence when `"SOL"` is present; `lookback_obs` counts rows, not time |
| **T1 incumbent** | the deployed grid cell ≠ the exact INCUMBENT row (WARN when a hand-set fade bar explains it, e.g. JitoSOL 2.86875 vs the grid's 2.8688) | every Δ is measured against the incumbent; if the grid cannot reproduce it, nothing downstream is comparable |
| **T2 fresh** | `.env` `MOMENTUM_*`, the book or the binary changed since the jobs ran (tokens-file change = WARN) | two identical sweeps disagreed because `.env` changed between them |
| **T3 slices** | deployed made no train-slice trade (FAIL-soft ⇒ `insufficient`, candidates marked not pickable); < 3 trades in a slice (WARN) | legacy rule: a token that exists only in the test slice can carry the whole test P&L (Jimothy +808 of +832) |
| **T4 units** | WARN when `MOMENTUM_RANK_METRIC` / global lookback changed since the previous run (the deployed bars are then in the old units — this run is the re-derivation, so it warns rather than fails); `unreachable?` = DEPLOYED made 0 trades in the last window | unit-scale law: `min_metric` is denominated in the global metric — a metric change invalidates every bar at once (re-derive all tokens in one run) |
| **T5 tripwire** | the canonical book changed during the run (the run aborts) | the loader renames over its input when the cap is missing |

A FAIL on T0–T2 suppresses the token's tables: never quote numbers from them.

## 3. Row flags

| flag | meaning | read it as |
|---|---|---|
| `★` | the deployed config itself (deployed trail) | the baseline |
| `dep@T` | the deployed knobs at another trail | a pure trail change — a real alternative, flagged like any row |
| `✓win` | ≥ K−1 of K back windows positive AND best window < 50% of Σ | the time-split robustness bar (rule 2b); required for "validated" |
| `▲` | worst window above DEPLOYED's | gains in the operator's own maximin sense |
| `both↑` | beats deployed on train AND test | necessary for a candidate (old rule 1), not sufficient |
| `specialist` | tops exactly one axis, outside every other top-3 | e.g. the least-drawdown 5-trade 100%-win row with a ~0 train slice |
| `thin-train` | test above deployed, train below | the test gain was paid for in train — distrust |
| `test-carried` | < 3 train trades | the "max test P&L" trap |
| `few-trades` | < 8 trades in a slice, or 100% wins on ≤ 5 | small-n; do not rank on it |
| `worse-tail` | worst single trade below deployed's | the operator's first axis: less P&L for less drawdown |
| `cost-fragile` | P&L ≤ 0 at 3× cost | tight trails are cost-sensitive (ZEC train +219 → +11 at 15 bps) |
| `1-trade(slice: resid X)` | one trade ≥ 50% of a positive slice; X = slice − best trade | automated delete-the-event: the residual must stay > 0 |
| `straddle(test|train open X)` | open P&L at that slice's end > 25% of the slice's Δ vs deployed | boundary artifact: the slice's result depends on a position still open |
| `edge:min` / `edge:lb` | load-bearing value on the grid boundary | the optimum may lie outside the grid; extend the axis (`--min-mults 0.25,0.375,…` or `--lookbacks …`) and re-run before trusting it — "every winner is a LOWER bar" has been the pattern since July |
| `inert:<knobs>` | the family spans several values of those knobs | keep the deployed value (rule 3); the pasted JSON already does |
| `non-robust` | a slice ≤ 0 or < 3 trades | never pick it (see §5) |

## 4. Decision axes

Each is evaluated within each trail rung. Split objectives consider **robust** families only
(both slices > 0 and ≥ 3 trades each), exactly as `per-token-sweep` does.

| axis | ranks by | origin |
|---|---|---|
| max test P&L | held-out P&L, ties → healthier train | global `test-pnl` / sweep objective |
| best worst-slice P&L | min(train, test) | global `net-pnl` |
| best worst-slice $/h | min over slices of P&L ÷ slot-hours, among rows with ≥ max(8, ½ deployed) trades per slice | global `pnl-per-hold` — guarded, see §6 |
| least drawdown | largest peak-to-trough over all jobs | sweep objective |
| best SQN | worst-slice √n·mean/σ | global `pareto` objective |
| maximin (time split) | best worst window f1..fK among robust rows (a config that never trades would "win" with 0), ties → Σ | the operator's "gain in each time window" (09-15) |
| window-robust best Σ | highest Σ among ✓win | rule 2b |
| evenest (time split) | least lump among ✓win | rule 2b |
| smallest worst trade | the tail over the whole history | drawdown first |
| cost-robust (3×) | test P&L at 3× cost | trail = priority dial |
| Pareto | worst-slice P&L ↑ vs test trade-σ ↓ | sweep frontier |
| consensus[k] | in the top-3 of k ≥ 2 axes that have > 3 eligible families (a top-3 of ≤ 3 is everyone) | sweep consensus — read it first |

## 5. Verdict rules

The pick for a token, in order (the analyst prompt in `agents.md` restates it):
1. **robust**;
2. **`✓win`**;
3. worst window ≥ DEPLOYED's;
4. worst trade not below DEPLOYED's (no `worse-tail`);
5. train ≥ DEPLOYED's train (old rule 1: a candidate's test gain must not cost train);
6. test@3× > 0 (no `cost-fragile`);
7. no `specialist` / `test-carried` / `edge` / `1-trade` / `straddle` unless the verdict says why
   it does not matter;
8. inert knobs keep the deployed value.

**No row clears 1–4 ⇒ `keep`.** Never hand-pick a non-robust row to force a change. A row that
clears 1–6 but not 7 ⇒ `paper-test`. Present the best row of **every trail rung** — the operator
chooses the trail ("choose best among 5% trail params"). Other verdicts:
- T3 FAIL-soft or INSUFFICIENT history ⇒ `insufficient` (no params from this run).
- Negative in EVERY cell ⇒ `keep` + "evidence says watch-only" note with the least-bad HIGH bar.
- An LST or major is judged at its own measured cost — a 50-bps meme cost can never refute a
  10-bps-validated override (automatic here: jobs are per-token).

A change that is not `✓win` is a hypothesis: say so and recommend a paper test
(`DRY_RUN_MOMENTUM_TRADER=true`).

## 6. Judgment calls the scripts cannot make

- **Read consensus first, not the P&L column.** A family in the top-3 of ≥ 2 axes with Δtest > 0
  AND train ≥ deployed's is a candidate; a family topping one axis and mid-table elsewhere is a
  specialist.
- **The window pool size is a filter, not a ranking.** Tightening to trail 2 made the ✓win pool
  bigger while the configs got worse (09-15). Rank on worst window and Σ, not pool size.
- **The trail is a priority dial.** It decides WHICH exit fires first: a tight trail beats the
  fade take-profit to the trade and the worst trade grows 20–130× (−0.47 → −20…−125). At tight
  rungs read `worst`, `trueDD` and the z knob frequency together — the overbought z-gate is the
  compensating knob at trail 5 (HYPE −26.58 → −0.50); the LST is the control (z off wins).
  Above a threshold the trail is inert (≡ rungs) — keep the deployed value there.
- **An isolated $/h is a mirage-prone number.** The highest isolated rate is the config that
  barely trades; the axis is guarded by a trade-count floor, and occupancy effects live in the
  book A/B only. Never pick on $/h alone.
- **The fade exit is drawdown protection** (operator decision 2026-09-12). `fb` below 1 exits
  later: read `worst`/`trueDD`/hold next to Δtest. A family listing `fb={1,0.75,0.5}` means the
  bar never bound — keep `fb=1`. Negative fractions (exit only once the trend turned down) are the
  holder profile and stay out of the default axis (−97 worst trade at N=2 on 2026-09-12).
- **`lookback_obs` is not time.** It counts rows; check the bar is reachable in the CURRENT
  cadence (T0 cadence warning, `unreachable?`). Lookback is the knob most often load-bearing.
- **Maximin over windows is a consistency criterion, not an out-of-sample test** — selecting on
  the windows is in-sample for them. What protects you: several disjoint windows, both slices,
  the tail, cost-robustness, and the paper test.
- **Every change must survive the whole-book A/B** before it is applied together with others.

## 7. What the sweep cannot see

No volume or order flow (price-only history); no discovered/adopted mints; the re-entry cooldown
is `.env`-global (`MOMENTUM_REENTRY_COOLDOWN_SECS` — swept 2026-09-13: inert at 60–600 s on the
current configs); the cost is a one-moment quote; GeckoTerminal rows are sparse where the live
watcher forward-fills (thin tokens rank differently live); backtests understate drawdown.
REPORT.md repeats this as its footer.

## 8. Output of the sweep binary to ignore

- The banner's `~Nd` spans assume 184 s per snapshot — wrong on 1-minute files.
- The closing "re-run the finalists at a second --train-frac (0.8)" advice — superseded by the
  disjoint windows (nested cuts are one observation).
- The binary's own "params JSON per objective winner" in `jobs/*.txt` is computed on ONE job;
  use the report's paste-ready params (built from the joined jobs, inert sets resolved).
