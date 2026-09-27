# History: coverage gates, the fetch/merge recipe, hazards

## Layout

- `assets/history_raw/<YYYYMMDD>/<SYM>.jsonl` — one GeckoTerminal series per file (+ `<SYM>.log`),
  `SOL.jsonl` for SOL. Raw files are never edited; a later build tops them up into a new dated dir.
- `assets/price_history.book_<YYYYMMDD>[_n].jsonl` — the merged book (all series, WSOL mint key,
  no plain `"SOL"` key), chmod 444, gz copy in `assets/history_backups/`.
- Run dirs never read the book directly: `run_sweeps.py` extracts per-token files into
  `<run>/hist/`: every book row from the token's start, prices limited to its mint and WSOL — the
  book's snapshot grid is kept on purpose (see windows.md, "Why the per-token file keeps the book grid").

## Recipe (what `ensure_history.py --build` does, and why)

1. **One series per `node scripts/backfill_history.js` call, sequentially.** The script holds
   everything in memory until it writes; a failed page silently keeps partial data; parallel runs
   draw 429 storms ("hours per token"). Per-series files make the fetch resumable.
2. **Always `--no-splice --output <file>`.** The default output OVERWRITES
   `assets/price_history.extended.jsonl`; splicing drops GT data inside the live window.
3. **Pin pools**: each token's `pool` from `momentum_tokens.json` (its wired venue), SOL to
   Raydium v4 `58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2`. Volume-ranked auto-pick once chose a
   5-week-old JitoSOL pool and produced a 150-d file with no head ("grid produced no results").
4. **Top-up, don't refetch**: an earlier raw file that reaches back far enough gets only its
   missing tail (`--days gap+1`) union-merged in (the fresh tail wins on overlapping minutes —
   the old last candle may have been in progress). SOL alone is ~40 min for 150 d. A fetch whose
   log shows a page "failed after retries" is kept but marked `<SYM>.partial` and re-fetched by
   the next `--build`.
4b. **A book always holds every deployed series** — `--tokens` narrows the report, never the
   build (a filtered build would become the newest book and shrink every later run).
5. **Merge = ts-union**; on a (ts, key) conflict the first file wins (GT candles are immutable,
   so a conflict is a duplicate). **Strip the plain `"SOL"` key**, keep the WSOL mint: the sim's
   `sanitize_pegged` only runs on files carrying `"SOL"` and at 1-min cadence it classifies every
   token as pegged and deletes real moves (387 HYPE / 1,843 ZEC prints on 2026-09-13); the sim
   aliases `"SOL"` from WSOL *after* that pass, exactly as for the validated research files.
6. **Write a NEW dated book**, `chmod 444`, gzip it to `assets/history_backups/`. chmod alone does
   not protect it: the sim's loader writes `<file>.tmp` and renames over the input when
   `HISTORY_MAX_SNAPSHOTS` is missing (a 177-d file became 30 d; 135 d lost on 2026-09-15).

## Coverage gates (T0, computed on `sanitize-dump` output)

Statistics are computed on what the sim actually sees — raw-JSONL analysis has manufactured false
signals (a 16× ZEC spike that was 2 real events; HYPE's 5000× backfill bug).

| check | level | note |
|---|---|---|
| rows = 43,200 | FAIL | the truncation signature (hypezec_0829, jitosol150, curated150 were cut this way) |
| plain `"SOL"` key | FAIL | see step 5 |
| token has no prints | FAIL | |
| WSOL does not span the book | WARN | the regime gate is blind where SOL is missing (met_ray had 32 d of SOL over 149 d of RAY) |
| glitch candidates | WARN | >4% jump returning to within 1.5% in ≤15 obs — the JitoSOL rule (2026-08-29). `ensure_history.py --build --clean-pegged` drops them for LST-class tokens only (median |1-obs return| < 0.1%); on a meme the same shape is real volatility |
| sanitizer removed >1% | WARN | inspect before trusting the token |
| <60 prints/day | WARN | sparse GT rows vs the live forward-fill: fewer own prints in a lookback window |
| weekly cadence shift >2× | WARN | `lookback_obs` counts rows: a cadence change changes what a lookback means |
| book older than 7 d | WARN | top it up |
| token starts >2 d after the book | WARN | young token, young pinned pool, or a partial fetch — read the raw `.log`; for a young pool, pin an older pool of the same pair for history only |

## Short history

| span | policy |
|---|---|
| ≥ 150 d | normal (book span, K=5) |
| 60–150 d | `SHORT`: the token's own span, K=3 back windows, flagged in the report |
| < 60 d | `INSUFFICIENT`: not swept, verdict `insufficient`, deployed params kept |

As of 2026-09-27: STONK (~41 d) is INSUFFICIENT, CATE (~63–76 d) SHORT.

## Sparse vs forward-filled

GeckoTerminal emits a minute candle only when a trade happened; the live watcher carries the last
price forward every ~60 s tick. The validated research files are sparse, so the book is too
(consistency with every earlier validation). For liquid tokens the difference is negligible; for
thin ones (T0 `<60 prints/day`) the sim sees fewer own prints per lookback window than live does.
`backfill_history.js --forward-fill` exists if a live-parity experiment is ever wanted — as a
separate book, never mixed.
