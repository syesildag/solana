# Time-split windows, maximin, and the whole-book A/B

## Why windows, not a second cut

The 0.7 and 0.8 train fractions both end at the file's last row; only the start moves. On the
185-day book (2026-09-15): 0.7 test = 61 d, 0.8 test = 47 d, **77% shared**, and both contained
the 2026-08-19..22 run that produced 79% of the P&L. Two nested cuts agreeing is the same event
counted twice. That config read `+670 / +684` on the two cuts and **3/5 windows positive with one
window carrying 79%** on disjoint windows. The live set of the time was 5/5 (+1077) and the
retune 3/5 (+727): the windows reversed the whole trail-5 session.

## Construction (run_sweeps.py)

Calendar boundaries from the book span `[t0, t1]`, shared by every OK token:

```
full   train [t0, t0+0.7·span)  test [.., t1]        ← the classic split objectives
f0     [t0, mid)                                     ← front half, one window (tail coverage)
f1..f5 five equal blocks tiling [mid, t1]            ← the time-split axes (rule 2b)
cost3x = full at 3× the token's cost
```

SHORT tokens (60–150 d) use their own span `[first print, t1]` with K=3 back windows.

**Why a window can be its own small file.** `per_token_sweep` does `snapshots.split_at(split)` and
then builds a separate `ranked_stream` and `regime_mask_for` per slice, and replays each slice
from flat. A fixed cell's test result therefore depends only on the test rows; the "expanding
train window" of rule 2b only matters when parameters are *selected* on train. So each window
job is `[one prefix row] + [window rows]` with `train_frac = 1.5 / n` (the sim's
`(n × frac) as usize` lands the split exactly on the window's first row). Sanitizing only removes
individual prices (rows are preserved), so the row arithmetic is exact.

**Exactness caveat.** Sanitizing runs over the whole file before the split: its 50× median band
and its neighbour-based spike filter can see different context in a window file than in the full
prefix. `run_sweeps.py --check-exact` compares both methods on real cells (test columns to the
cent). If it ever reports MISMATCH for a token, score that token's windows the 09-15 way —
`rows[:b]` with `train_frac = rows(ts<a) / rows(ts<b)` — slower but reference-exact.

Every window loses its first ~max(lookback, 480) observations to warm-up (8–24 h), as the 09-15
method did. Window jobs contribute only their test-side columns; robustness (both slices > 0)
is judged on `full`.

## The verdict line

For a combination: **"P/K back windows positive, best window = X% of Σ"** — never a single
"+670 held-out". `✓win` requires P ≥ K−1 (4/5) AND X < 50%. A best window above ~50% of the
total is a config fitted to one event: a hypothesis, not an edge.

**Maximin** (best worst window) is the operator's selection axis ("I am interested to have params
to gain in each time shift window"). It is a consistency criterion — choosing on the windows is
in-sample for them. Tie-break on Σ. The ✓win **pool size** is a filter, never a ranking.

## Why the per-token file keeps the book grid

`lookback_obs`, the regime mask and the rows where stops and fades are checked all count ROWS.
A GeckoTerminal book has a row for every minute in which ANY of its series traded; a file of only
"rows where this token or SOL printed" is a sparser grid. Measured 2026-09-27 (deployed HYPE,
book3full, same split timestamp, 1 bps): HYPE's own test P&L inside the book at N=10 was
**+75.30**; isolated on the token-only grid **+92.90** (+23%); isolated on the book grid
**+79.39** (+5%). So each per-token file keeps every book row (prices limited to the mint and
WSOL, a row may be empty). The book grid is also closer to live, whose watcher writes one row per
minute (95% vs 89% of minutes on that book).

## Whole-book A/B (book_ab.py)

The grid scores tokens alone; live, N slots are shared by all tokens. Per-token winners from
separate sweeps are never applied together without a book-level check.

- Candidate file = deployed + every `change` pick (`apply_params.py --from-verdicts`).
- Both files replay the combined book at the live `MOMENTUM_MAX_POSITIONS` on `full` + `f0..f5`
  (1-cell sweeps; the INCUMBENT row is the book at that file's params).
- One cost for both arms: the notional-weighted mean of the per-token costs (the book replay can
  apply only one `MOMENTUM_SLIPPAGE_BPS`). The A/B measures the Δ from interaction, not absolute
  P&L. A gap has three sources: real interaction (slots, the daily trade cap), the small
  isolated-vs-book residual (+5% on HYPE above), and the uniform-vs-per-token cost difference.

**Additivity check:** book Δ Σ(back windows) vs Σ of the per-token Δs (recomputed from the job
CSVs). A material gap (> max($5, 25%)) means the picks interact. Then run arms — A: all
deployed; B: only change 1; C: only change 2; D: both — and keep only the changes whose effect
survives in the book (the 2026-09-15 HYPE N=1 winner +432/+399 lost at N=2: +598/+471 vs
+988/+528). SHORT tokens are left out of the per-token sum (their own-span windows are not the
book's windows); their effect is only inside the book Δ, and the A/B says so.
