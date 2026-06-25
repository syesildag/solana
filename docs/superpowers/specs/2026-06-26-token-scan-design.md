# Live token discovery — rolling top-3 in-memory for the momentum trader

**Date:** 2026-06-26
**Status:** design approved → implementation pending

## Context

The momentum trader watches a hand-curated `assets/momentum_tokens.json`, grown one at a time
via `scripts/add_momentum_token.js` (Jupiter-verified, manual). The operator wants the live
`portfolio-watcher` to **auto-discover liquid, tradeable tokens periodically** and feed the
**top 3 most-traded** into the trader **in memory** — *without* mutating the curated file.

Findings (probed live):
- Generic universe = a **volume-ranked Solana list**: Birdeye `/defi/tokenlist?sort_by=v24hUSD`
  (the existing `BIRDEYE_API_KEY` reaches it). Not xStock-specific.
- The top-volume list is **full of junk**: stables/wrapped, and **wash trades** (`SV151`:
  $50M vol on $164 liq ≈ 305,000× vol/liq; `PARK` ≈ 3,900×). Legit tokens sit at ~1–10×.
- The project already trusts **Jupiter-verified** as a scam gate (`add_momentum_token.js`).

Decisions (operator):
- **Generic**, not xStock-only. **Verified + anti-wash** filter (not min-liquidity alone).
- **Live & periodic** inside the watcher — but **~hourly, not 60s** (liquidity/volume move on
  hours; 60s would burn the Birdeye quota / risk a ban for identical data).
- **In memory, top-3** — a transient rolling overlay; **the curated file is never written.**
- Architecture: the watcher **spawns the existing Node script** periodically (reuses the
  tested scan/Jupiter logic, like it already spawns the klend sidecar).

## Goal

Every ~hour, the watcher runs the scan, keeps the **3 most-traded** verified/liquid
discoveries in an in-memory set, and the momentum trader ranks them alongside the curated
list. Nothing persists; a restart resets to the curated list.

## Components

### A. `scripts/scan_tokens.js` (discovery + filter; emits JSON)
Pipeline (cheap → expensive):
1. **Source** — Birdeye `/defi/tokenlist?sort_by=v24hUSD&sort_type=desc&limit=<LIMIT>`,
   headers `X-API-KEY: $BIRDEYE_API_KEY`, `x-chain: solana` → `{address, symbol, name,
   v24hUSD, liquidity}`.
2. **Denylist** — drop stablecoins + wrapped SOL + USDC cash leg (by mint + `/^w?USD/i`).
3. **Dedup** — drop mints already in `momentum_tokens.json` (curated stays authoritative).
4. **Floors + ratio** — keep if `v24hUSD ≥ MIN_VOL` AND `liquidity ≥ MIN_LIQ` AND
   `v24hUSD/liquidity ≤ MAX_RATIO` (anti-wash; rejects SV151/PARK).
5. **Verify** — keep only Jupiter-`isVerified` survivors (reuse `add_momentum_token.js`'s
   `search()` by mint). Small post-filter set → few calls.
- **Output `--json`** (the live path): print the surviving candidates **sorted by 24h volume
  desc** as a JSON array of `{symbol, mint, name, vol24, liq}`. No file write.
- **`--apply`** (optional, manual): also append to `momentum_tokens.json` (reuse `loadList` +
  atomic write) — for a human one-off, not the live path.
- Thresholds via env/flags: `SCAN_MIN_VOLUME` (250k), `SCAN_MIN_LIQUIDITY` (200k),
  `SCAN_MAX_RATIO` (30), `SCAN_LIMIT` (100). Shared bits factored to `scripts/lib/jup.js`.

### B. Watcher integration (`portfolio-watcher`, Rust)
- **Config** (`PortfolioConfig`): `momentum_scan_enable` (`MOMENTUM_SCAN_ENABLE`, default
  false), `momentum_scan_interval_secs` (`MOMENTUM_SCAN_INTERVAL_SECS`, default 3600),
  `momentum_scan_top_n` (`MOMENTUM_SCAN_TOP_N`, default 3), `momentum_scan_script`
  (default `scripts/scan_tokens.js`). Thresholds are the child's env (inherited).
- **Periodic run** — every `interval_secs` (a tick-counter in the existing loop, like the
  ~5-min wallet rescan), `tokio::process::Command::new("node").arg(script).arg("--json")`,
  run to completion, capture stdout. Best-effort: a failed/slow scan logs a warning and keeps
  the previous `discovered` set.
- **In-memory `discovered: Vec<WatchedToken>`** — parse stdout JSON, take the **top
  `top_n`** (already volume-sorted), as `WatchedToken { symbol, mint, name, equity:None }`
  (so `is_equity()` auto-detects xStocks). **Replace** the set each scan. Never written to disk.
- **Effective universe** — wherever the watcher uses `watched`, use
  `effective = curated ∪ discovered ∪ {currently-held token}` (deduped by mint). The held-token
  clause means a position in a discovered name is never orphaned when it rolls off the top-3.
- **Warming** — when a token first enters `discovered`, `backfill_watched_cold` it (so
  momentum can rank it) and union its mint into `token_mints` / `known_price_keys` for pricing.
- **Logging** — on each scan: `momentum: scan → discovered [SYM(vol) ×3]` (or "no change").

## Data flow
hourly tick → spawn `node scan_tokens.js --json` → Birdeye top-vol → denylist + dedup +
floors/ratio + Jupiter-verify → JSON (vol-sorted) → watcher takes top-3 → replace `discovered`
→ warm new entrants → momentum ranks `curated ∪ discovered ∪ held`.

## Safety / honesty
- **Curation heuristic, not an edge** — discovers *tradeable* (liquid, verified, real) names;
  the momentum metric still decides what to trade, and single-name momentum isn't robust on
  the recorded sample. More candidates ≠ more profit.
- **Transient + bounded** — top-3, in memory, replaced hourly, never persisted; a bad pick
  rolls off and a restart resets to curated. Sidesteps file pollution + the live-auto-apply risk.
- **Verified + ratio + floors + denylist** keep scams/wash-trades/stables out. Disabled by
  default (`MOMENTUM_SCAN_ENABLE=false`); opt-in.

## Testing
- **Script:** pure filter `(tokens, opts) -> ranked survivors` — SV151-like rejected by the
  ratio cap, stable denylisted, curated-mint deduped, cbBTC-like passes; `--json` emits valid
  sorted JSON.
- **Watcher:** pure `effective_universe(curated, discovered, held) -> Vec<WatchedToken>` —
  dedup by mint; a held token absent from curated+discovered is retained; top-3 cap honored.

## Files
- `scripts/scan_tokens.js` (new) · `scripts/lib/jup.js` (shared helper).
- `src/portfolio/watcher.rs` — periodic spawn + parse + `discovered` + effective-universe merge + warm.
- `src/portfolio/mod.rs` — `PortfolioConfig` scan fields + `from_env`.
- `.env.example` — the `MOMENTUM_SCAN_*` knobs (default off).

## Future
- When the parked in-snapshot volume/liquidity capture (`dedb785`) accumulates, rank the
  top-N by a backtested volume/liquidity (or momentum) signal instead of raw 24h volume.
