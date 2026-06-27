# Birdeye-trending discovery source for `scan_tokens.js`

**Date:** 2026-06-27
**Status:** design approved → implementation pending

## Context

The momentum trader's live discovery (`scripts/scan_tokens.js --json`, spawned hourly by the
watcher — see `2026-06-26-token-scan-design.md`) builds its candidate universe from Birdeye's
**volume-sorted tokenlist** (`/defi/tokenlist?sort_by=v24hUSD`), paged up to 15 pages.

Problem found live: on the **Birdeye free tier**, paged tokenlist calls get rate-limited/401'd
after the first page (`fetchBirdeyeTopVolume` keeps page-0 only), so discovery effectively sees
just the **top ~50 by volume**. Mid-cap movers sit far below that cutoff and are never in the
candidate pool — e.g. **SLX** ($1.67M 24h vol, +33.5% 24h, well above both floors) is invisible
to discovery even though the **per-mint** `token_overview` endpoint returns it fine. The
pagination order, not the floors or the ranking, is the gate.

The operator already runs `MOMENTUM_SCAN_RANK=change` (rank survivors by 24h price-change within
a `0 < chg ≤ MOMENTUM_SCAN_MAX_CHANGE_PCT` band, default 85%) — so discovery is meant to surface
*hot movers*, but pagination starves it of candidates.

Decisions (operator):
- Switch the discovery source to the **Birdeye trending feed** (`/defi/token_trending`).
- Keep the volume-pagination path behind an **env toggle** (reversible, both paths tested).
- Curated `momentum_tokens.json` (MET, BP, SLX, ARX) remains the backbone; discovery's job is
  finding genuinely hot **new** names, gated by the existing floors + change band.

Findings (probed live, free tier with the existing `BIRDEYE_API_KEY`):
- `/defi/token_trending?sort_by=rank&sort_type=asc&limit=20` returns **success** and carries
  every field the pipeline needs **in one call**: `address`, `symbol`, `name`, `volume24hUSD`,
  `liquidity`, **and** `price24hChangePercent`.
- The trending population is **parabolic-memecoin-heavy** (BASE +454%, BARRON +1382%, FIFA
  +520%). The existing `0 < chg ≤ 85%` band rejects most of these, so survivor counts will be
  **small (0–3)** — acceptable, since curated names are the backbone. This is the accepted
  trade-off: trending will **not** reliably surface steady mid-caps like SLX (those stay in the
  curated list); it surfaces novel hot names within the band.
- DexScreener trending/boosts were rejected: boosts are paid ads; search needs a query term and
  returns fake-TVL pairs — neither is a momentum movers feed.

## Goal

Add `MOMENTUM_SCAN_SOURCE=trending|volume` (default `trending`). With `trending`, discovery is
sourced from `/defi/token_trending` in a single call, runs through the **unchanged** floors,
verify, change-band, and ranking, and emits the **same `--json` contract**. The Rust watcher is
untouched. `volume` preserves today's pagination path exactly.

## Architecture / control flow

`main()` branches once on `OPTS.source`:

```
source = MOMENTUM_SCAN_SOURCE ("trending" default | "volume")
  ├─ "trending" → fetchBirdeyeTrending(limit)   → rows incl. change24h inline
  └─ "volume"   → fetchBirdeyeTopVolume(min,pp)  → rows (UNCHANGED)
           ↓
   filterCandidates(rows, curatedMints, OPTS)    ← floors + wash + dedup-vs-curated (UNCHANGED)
           ↓
   verifyAll(filtered.slice(0, verifyMax))       ← Jupiter verify (UNCHANGED)
           ↓
   if rank=="change": annotate only rows missing change24h  ← trending already has it → no-op
           ↓
   rankSurvivors(survivors, OPTS)                ← band + sort by change desc (UNCHANGED)
           ↓
   --json → stdout: [{symbol, mint, name, vol24, liq, change24h}]   (SAME shape)
```

## Components

**New: `fetchBirdeyeTrending(limit)`**
- One GET `https://public-api.birdeye.so/defi/token_trending?sort_by=rank&sort_type=asc&offset=0&limit=<limit>`
  with the existing headers (`X-API-KEY`, `x-chain: solana`, `accept: application/json`).
- Maps each `data.tokens[]` entry to the existing candidate row shape:
  `{ address, symbol||"", name||"", v24hUSD: +volume24hUSD||0, liquidity: +liquidity||0,
     change24h: Number.isFinite(+price24hChangePercent) ? +price24hChangePercent : null }`.
- `!res.ok` handling mirrors the existing fetchers: throw on a hard failure with nothing
  collected; otherwise return what was parsed. Single call ⇒ no mid-pagination partial state.
- `limit` from `MOMENTUM_SCAN_TRENDING_LIMIT` (default 20).

**Changed: `OPTS`** — add `source: (MOMENTUM_SCAN_SOURCE||"trending").toLowerCase()` and
`trendingLimit: numEnv("MOMENTUM_SCAN_TRENDING_LIMIT", 20)`.

**Changed: `main()`** — source branch (above); make the `annotateChange24h` call conditional:
run it only for survivors whose `change24h` is not already finite (keeps the volume path
identical, makes the trending path skip the per-mint fetches).

**Unchanged:** `filterCandidates`, `rankSurvivors`, `verifyAll`, `fetchBirdeyeTopVolume`,
`fetchChange24h`, the `--apply` path, the `--json` output shape, and the Rust consumer
(`MOMENTUM_SCAN_TOP_N` still slices the top-N downstream).

## Error handling / edge cases

- **Sparse survivors:** trending ∩ floors ∩ band ∩ verified may be 0–3 rows. Expected; the
  existing summary log reports `scanned → passed filters → kept`. No special-casing.
- **Curated dedup:** `filterCandidates` already drops already-curated mints, so SLX/ARX never
  double-appear via discovery.
- **Missing/garbage fields:** coerced with `+x||0`; non-finite `change24h` → dropped by the
  band (unchanged behavior).
- **Floors still apply:** trending names below `SCAN_MIN_VOLUME`/`SCAN_MIN_LIQUIDITY` or above
  the wash ratio are filtered exactly as volume-path names are.

## Testing

- `filterCandidates` and `rankSurvivors` are already `module.exports`'d and unit-tested.
- Add unit tests: (a) trending-shaped rows (with inline `change24h`) flow through
  `filterCandidates` + `rankSurvivors` and land in the right order/band; (b) the annotate-skip
  predicate — rows that already carry a finite `change24h` are not re-fetched.
- Network fetchers (`fetchBirdeyeTrending`, `fetchBirdeyeTopVolume`, `fetchChange24h`) stay
  untested, matching the current convention (live-only).
- No Rust changes → `cargo` untouched.

## Scope guardrails (YAGNI)

- No paging of trending (single call, `limit` default 20).
- No data source beyond Birdeye trending (DexScreener/boosts explicitly out).
- No change to floors, change band, Jupiter verify, `--apply`, or the Rust watcher.
- Default `MOMENTUM_SCAN_SOURCE=trending`; set `=volume` to restore today's behavior with zero
  code change.
