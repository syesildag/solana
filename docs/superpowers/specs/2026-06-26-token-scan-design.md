# Generic liquid-token scanner for the momentum watch list

**Date:** 2026-06-26
**Status:** design approved → implementation pending
**Scope:** a discovery script — no Rust/trader changes.

## Context

The momentum trader watches a hand-curated set (`assets/momentum_tokens.json`), grown one at
a time via `scripts/add_momentum_token.js` (Jupiter-verified, manual, one ticker/mint). The
operator wants to **auto-discover liquid, tradeable tokens** to add — **generically**, not
just xStocks.

Findings (probed live):
- No cheap "list all tokens" query. The generic universe = a **volume-ranked Solana token
  list**: Birdeye `/defi/tokenlist?sort_by=v24hUSD` provides it, and the existing
  `BIRDEYE_API_KEY` reaches it.
- **The top-volume list is full of junk** that a naive "min-N-liquidity" filter would admit:
  stables/wrapped (SOL/USDC/USDT/USD1), and **wash-traded** tokens — `SV151` showed **$50M
  volume on $164 liquidity** (vol/liq ≈ 305,000×), `PARK` ≈ 3,900×. Legit tokens sit at
  ~1–10× (cbBTC 4×, SPCX 6.5×).
- The project already has a scam gate: `add_momentum_token.js` accepts only **Jupiter-verified**
  tokens (exact symbol/name match, to dodge look-alike scams).

**Decision (operator): verified + anti-wash.** A generic scan = top-by-volume ∩
Jupiter-verified, minus stables/wrapped, with volume + liquidity floors **and a vol/liq ratio
cap**. Thresholds are CLI flags.

## Goal

`scripts/scan_tokens.js` — discover genuinely-liquid, verified, tradeable Solana tokens and
propose them for the watch list. **Print by default; `--apply` to merge.**

## Scope

**In:** the Node script — Birdeye volume-ranked source, multi-layer quality filter, dedup vs
the watch list, print candidates, `--apply` merge (reusing `add_momentum_token.js`'s
load/write + Jupiter verify).

**Out:** Rust/trader changes (none — it only reads the watch list); in-snapshot volume/liq
capture (separate parked spec `dedb785`); auto-removal/pruning of existing tokens.

## Design

### Source — Birdeye tokenlist
`GET https://public-api.birdeye.so/defi/tokenlist?sort_by=v24hUSD&sort_type=desc&offset=0&limit=<LIMIT>`
with headers `X-API-KEY: $BIRDEYE_API_KEY`, `x-chain: solana`. Returns `data.tokens[]` of
`{address, symbol, name, v24hUSD, liquidity}`. `LIMIT` = `--limit` (default 100).

### Filter pipeline (cheap → expensive)
1. **Denylist** — drop stablecoins + wrapped SOL + the USDC cash leg, by mint (USDC, USDT,
   USD1, USDG, jlUSDC, wSOL, …) and a symbol heuristic (`/^w?USD|USDT|USDC/i`). Configurable.
2. **Dedup** — drop mints already in `momentum_tokens.json`.
3. **Floors + ratio** — keep if `v24hUSD ≥ --min-volume` AND `liquidity ≥ --min-liquidity`
   AND `v24hUSD / liquidity ≤ --max-ratio`. The ratio cap is the anti-wash filter (rejects
   SV151/PARK; passes cbBTC/SPCX).
4. **Verify survivors** — for each remaining mint, reuse `add_momentum_token.js`'s `search()`
   against `{JUP_HOST}/tokens/v2/search?query=<mint>`; keep only `isVerified === true`
   (matched by `id === mint`). The survivor set is small (post-filter), so few Jupiter calls;
   add a small delay between them for rate limits.

### CLI flags + defaults (starting points; operator tunes)
`--min-volume` (250000), `--min-liquidity` (200000), `--max-ratio` (30), `--limit` (100),
`--apply` (off = dry-run/print).

### Output
- **Default (no `--apply`):** print a table — `symbol | mint | vol24 | liq | ratio | name` —
  sorted by volume, plus a count and the exact flags used. No file change.
- **`--apply`:** append candidates to `momentum_tokens.json` as `{symbol, mint, name}` (name
  kept for humans **and** so the Rust `is_equity()` auto-detects xStocks via "xStock" in the
  name), reusing `add_momentum_token.js`'s `loadList` + dedup + atomic write. Print what was
  added + "restart portfolio-watcher".

### Reuse
Factor the shared bits — `JUP_HOST`, `search()`, `MINT_RE`, `loadList`, `TOKENS_PATH` — into a
small `scripts/lib/jup.js` (or `module.exports` from `add_momentum_token.js`) so the two
scripts don't duplicate the Jupiter/watch-list logic.

## Data flow
Birdeye top-vol → drop stables/wrapped + already-watched → floors + ratio cap →
Jupiter-verify survivors → candidates → (`--apply`) merge into `momentum_tokens.json`.

## Safety / honesty
- This is a **curation heuristic, not a validated edge.** It improves the candidate pool's
  *tradeability* (liquid, verified, real), not the strategy's profitability — the momentum
  edge is a separate question (and the session's finding is single-name momentum isn't robust
  on the recorded sample). Adding tokens ≠ adding edge.
- Verified + ratio + floors + denylist keep scams/wash-trades/stables out; `--apply` is opt-in
  so the candidate list is reviewed first.
- xStocks are included naturally (verified + liquid) — the scan is generic, not xStock-specific.

## Testing
- Extract the filter as a **pure function** `(tokens, opts) -> survivors` and assert: an
  SV151-like row (huge vol, tiny liq) is rejected by the ratio cap; a stable is denylisted; an
  already-watched mint is deduped; a cbBTC-like row passes. (Small inline Node assertion or a
  test file under `scripts/`.)
- Manual: run without `--apply` against live Birdeye and eyeball the table (cbBTC/SPCX in,
  SV151/PARK/stables out).

## Files
- `scripts/scan_tokens.js` (new) · `scripts/lib/jup.js` (optional shared helper) · no Rust changes.

## Future
- Once the parked in-snapshot volume/liquidity capture (`dedb785`) accumulates data, the
  momentum sim can backtest a volume/liquidity **tilt**, and the scan's thresholds become
  data-tuned rather than heuristic.
