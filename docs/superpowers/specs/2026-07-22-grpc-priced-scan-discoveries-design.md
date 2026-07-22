# gRPC pricing for scanner-discovered momentum tokens

**Date:** 2026-07-22
**Status:** approved (design), pending implementation plan

## Problem

The momentum scanner (`MOMENTUM_SCAN_ENABLE`) injects up to `MOMENTUM_SCAN_TOP_N`
discovered tokens into the watched set each hour, but the gRPC price feed is built
**once at startup** from `pools.json` × the watch list (`spawn_grpc_feed`,
`src/bin/portfolio_watcher.rs`). Discoveries therefore have no vault subscriptions and
are REST-priced for their entire in-memory life: slower ranking updates, and — if one
is ever HELD — a trailing stop riding on REST cadence and rate limits (the class of
failure fixed on 2026-07-21 for wired pools, but structural for discoveries).

## Goals

- A scan discovery whose main pool is on **PumpSwap** is gRPC-priced within one scan
  tick of appearing, with the same vault-reserve CP pricing as curated pumpswap tokens.
- `pools.json` stays untouched — discoveries remain ephemeral (watcher restart resets
  to the curated list, unchanged).
- No regression for curated tokens; no new behavior when the scan is disabled or
  returns an unchanged set.

## Non-goals

- Non-PumpSwap venues (Raydium/Meteora/Orca discoveries stay REST-priced with a log
  line). Decided 2026-07-22: the scanner targets the pump.fun universe; other venues
  are rare in its output and each needs its own ad-hoc decoder.
- Zero-gap live resubscription on the open Yellowstone stream. Decided 2026-07-22:
  a few seconds of REST fallback at scan ticks (only when the pool set changed) is
  acceptable; re-spawning the feed reuses the hardened startup path.
- Auto-promotion of discoveries into the curated file (that remains the manual
  `add-momentum-token` / `vet-momentum-token` flow).

## Design

### 1. Scanner emits pool info (`scripts/scan_tokens.js`)

After ranking, a final enrichment step resolves each survivor's best pool from
DexScreener (`/latest/dex/tokens/<mint>`): pick the pair with the **highest 24h
volume** (fake-TVL rule — never rank pools by liquidity). If and only if that pair's
`dexId == "pumpswap"`, the `--json` output row gains:

```json
{ "symbol": "...", "mint": "...", ..., "pool": "<pumpswap pool address>", "quote": "SOL" }
```

`quote` is the pool's quote-token side symbol (SOL/USDC). Other venues emit no pool
fields (stderr note: `scan: <sym> best pool on <dex> — REST-priced`). The pair-picking
logic is a pure exported function with unit tests in `scan_tokens.test.js`.
DexScreener failures fail open to "no pool fields" — a discovery without pool info is
watched REST-priced, exactly today's behavior.

### 2. Ad-hoc pool decode (existing JS decoder, spawned by the watcher)

When the scan handler in `src/portfolio/watcher.rs` receives discoveries, it collects
their `pool` fields and compares against the currently-wired dynamic set. If the set
changed, it spawns:

```
node scripts/fetch_pumpswap_pools.js --pools <a,b,c> --output <tmpdir>/scan_pools.json
```

— the existing ad-hoc mode with the on-chain layout decode and mandatory vault↔mint
cross-check — then parses the file into `Vec<PoolConfig>`. Timeout ~30 s; any failure
(exit code, parse, cross-check error) keeps the old feed and logs a warning.

### 3. Feed re-spawn with merged pools (`src/bin/portfolio_watcher.rs` + `watcher.rs`)

- `spawn_grpc_feed(cfg, watched)` gains an `extra_pools: &[PoolConfig]` parameter,
  merged over the `pools.json` id-map. On id collision the `pools.json` entry wins
  (curated wiring is authoritative).
- The feed task's `JoinHandle` is retained. The pricing/exit paths read the feed
  through a swappable handle (`Arc<ArcSwap<GrpcFeed>>` or equivalent): on a changed
  pool set the watcher spawns the new feed (curated ∪ discovered, extra pools merged),
  swaps the handle, then aborts the old task. Readers never see a torn state; during
  the swap gap un-fed mints fall back to REST via `select_prices` (existing behavior).
- An unchanged discovered-pool set causes **no** rebuild — the common case (same top-N
  rediscovered hourly) has zero cost.

### 4. Failure containment

| Failure | Behavior |
|---|---|
| DexScreener down / no pairs | Discovery watched REST-priced (today's behavior) |
| Decode script error/timeout | Old feed kept; warn; retry naturally next scan tick |
| One pool fails cross-check | Script errors for that pool → that token REST, rest proceed |
| Feed spawn fails | Old feed kept (swap only happens after successful spawn) |
| Held position during swap | Exit path REST-falls-back for un-fed mints (2026-07-21 fix) |

### 5. Testing

- **JS:** unit tests for the best-pool picker (pumpswap-by-volume wins; non-pumpswap
  → no pool fields; empty pairs → no fields) in `scan_tokens.test.js`.
- **Rust:** unit tests for the pool-set differ (changed/unchanged/empty) and the
  config merge (curated wins on collision).
- **End-to-end:** existing `grpc-smoke` subcommand run with a synthetic extra pool;
  live validation = `momentum: pricing gRPC=[...]` log line listing a discovery.

## Rollout

Ship dark: behavior only changes when `MOMENTUM_SCAN_ENABLE=true` (already live) and a
discovery has a pumpswap main pool. First live validation is watching the pricing
split log after a scan tick that surfaces such a token.
