# Momentum gRPC price feed — design

**Date:** 2026-07-01
**Status:** Approved (design); user away — implemented autonomously, left for review.
**Scope:** Additive, opt-in. Gives the momentum trader a low-latency, quota-free on-chain price source
(Yellowstone gRPC) with automatic REST fallback. Does NOT change momentum signal/execution logic, the
MEV arb path, or the live trader's decisions.

## Problem

The momentum trader (`portfolio_watcher` binary) prices tokens entirely via **REST aggregators**
(`src/portfolio/pricer.rs`: DexScreener for SPL tokens, Kraken/CoinGecko for SOL). REST is rate-limited
and quota-capped — the recurring "Birdeye compute units exhausted" / GeckoTerminal 429 failures that
repeatedly blocked work. The MEV framework already ingests real-time on-chain pool state over Yellowstone
gRPC (`src/streamer/`) and already derives prices from pool state (`src/dex/`), but the momentum side uses
none of it (`src/portfolio/` has zero gRPC). The code even notes the intent: *"deliberately slow; Phase B
switches to gRPC streaming for speed."*

Momentum does not need gRPC's microsecond latency (it trades on multi-minute bars). It needs gRPC's
**reliability and zero quota** — a price feed that never hits a rate limit and reads on-chain truth.

## Goal

A self-contained gRPC price source in the watcher that maintains a live `mint → USD` map for the momentum
tokens that have an on-chain pool configured, which the watcher prefers over REST (REST remains the
fallback). Opt-in via a default-off flag, so an unchanged `.env` behaves exactly as today.

Non-goals: no change to momentum signal/execution; no gRPC for pairs/xStock legs or the discovery scanner;
no sub-second reaction (the watcher still samples at its poll cadence — gRPC only makes each sample fresher
and quota-free); no coupling to the arb `Config`/`PoolRegistry`/submission pipeline.

## Approach (chosen: A — dedicated lightweight pricer)

A new `src/portfolio/grpc_pricer.rs` runs a background task **inside the watcher process** (the watcher is a
separate binary from `solana-mev`, so per the base-token finding it cannot read the bot's in-memory state —
it must own its subscription). It reuses the `dex` pool-parsing + price math and the raw
`yellowstone-grpc-client`, but NOT the arb `Config`/`PoolRegistry`/dispatch.

Rejected: (B) reusing the arb `GrpcStreamer`+`PoolRegistry` wholesale — drags arb plumbing into the watcher,
heavy coupling. (C) main-bot publishes prices over IPC — cross-process fragility, requires the arb bot
running.

## Data dependency

`momentum_tokens.json` carries the **mapping** (which pool + quote per token). The pool's **structure**
(DEX kind, vault/state accounts, decimals, orientation) comes from `pools.json` (already managed by
`scripts/fetch_all.js`) by referencing the pool id. A momentum token whose referenced pool id is absent
from `pools.json`, or fails to parse, simply never enters the gRPC map → REST prices it. So the two configs
stay single-purpose: `momentum_tokens.json` = "use pool X (quoted in Y) as this token's price source";
`pools.json` = "pool X is a raydium_clmm with these accounts".

## Config + schema changes

**`PortfolioConfig` (`src/portfolio/mod.rs`)** — new fields, all from env, all with safe defaults:
- `momentum_grpc_pricing: bool` ← `MOMENTUM_GRPC_PRICING` (default **`false`** — the core safety switch).
- `grpc_endpoint: Option<String>` ← `GRPC_ENDPOINT`; `grpc_token: Option<String>` ← `GRPC_TOKEN`.
- `pools_path: String` ← `POOLS_PATH` (default `pools.json`).
- `momentum_grpc_stale_secs: u64` ← `MOMENTUM_GRPC_STALE_SECS` (default `30`) — freshness window for
  preferring a gRPC price over REST.

**`WatchedToken` (`src/portfolio/momentum_universe.rs`) / `momentum_tokens.json`** — two new OPTIONAL fields:
- `pool: Option<String>` — the pool account id (must exist in `pools.json`).
- `quote: Option<String>` — `"USDC"` or `"SOL"` (the pool's quote leg; determines USD conversion).
Absent → token is REST-only. Existing entries (no `pool`/`quote`) deserialize unchanged (serde default).

## New module `src/portfolio/grpc_pricer.rs`

Single responsibility: maintain a live `mint → (usd, Instant)` map from gRPC.

- `pub type GrpcPriceMap = Arc<DashMap<String, (f64, std::time::Instant)>>;`
- `pub struct SolPriceHandle(Arc<AtomicU64>)` — the watcher publishes the latest SOL/USD (f64 bits) each
  poll; the pricer reads it for SOL-quote conversion. (Mirrors the existing atomic-price pattern.)
- `pub fn spawn(cfg: &PortfolioConfig, watched: &[WatchedToken], sol_px: SolPriceHandle) -> Result<(GrpcPriceMap, JoinHandle<()>)>`:
  1. Load `pools.json`; for each watched token with a `pool` id, resolve its `PoolConfig` and build a
     `Pool` (reuse `Pool::try_from`/the `dex` parsers). Build a small `account_pubkey → (Arc<Pool>, token_mint, quote)`
     index over ONLY those pools (a minimal local dispatch, not the arb `PoolRegistry`). Tokens whose pool
     id is missing/invalid are logged and skipped (REST will price them).
  2. Build a `SubscribeRequest` covering those pools' accounts (the accounts `dex` reads: AMM vaults; CL
     state account). Open a Yellowstone subscription to `grpc_endpoint` (+`grpc_token`).
  3. On each account update: look up the pool in the index, apply the update via the existing parse path
     (`PoolRegistry::handle_account_update`-equivalent / `parse_state` / vault parse), compute the token's
     spot price in quote units (`price_from_pool` — see below), convert to USD, store `(usd, Instant::now())`.
  4. On stream error/disconnect: log + reconnect with capped backoff (mirror `src/streamer/client.rs`); the
     map ages out meanwhile so the watcher falls back to REST.

- `pub fn price_from_pool(pool: &Pool, token_is_a: bool) -> Option<f64>` — **pure**, the unit-tested core.
  Returns the token's price in the OTHER (quote) leg's units from current pool state: CL pools from
  `sqrt_price_x64` (decimal-adjusted); constant-product from the reserve ratio (decimal-adjusted). No USD
  here — USD conversion (quote=USDC identity; quote=SOL × SOL/USD) happens in the task using `sol_px`.

## Watcher integration (`src/portfolio/watcher.rs`)

- Startup: if `cfg.momentum_grpc_pricing && cfg.grpc_endpoint.is_some()`, call `grpc_pricer::spawn(...)`
  and hold the `GrpcPriceMap` + `SolPriceHandle`. Otherwise skip entirely (today's pure-REST path, byte-identical).
- Each poll, in the price-building step (currently a `pricer::fetch_prices` call):
  1. Fetch SOL/USD as today; publish it into `SolPriceHandle` (for the pricer's SOL-quote conversion).
  2. For each watched mint, if the gRPC map has a **fresh** entry (`now − ts ≤ momentum_grpc_stale_secs`),
     take it. Collect the remaining (missing/stale) mints.
  3. REST-fetch ONLY the remaining mints via `pricer::fetch_prices`.
  4. Merge (gRPC-fresh preferred), then snapshot to history + run the momentum/pairs tick exactly as today.
- A helper `select_prices(grpc_map, watched_mints, stale_secs, now) -> (use_grpc: HashMap, rest_to_fetch: Vec)`
  is pure and unit-tested (the merge/staleness decision).

## Error handling / degradation

- Endpoint unreachable / connection drop → task reconnects with backoff; map ages out → automatic REST
  fallback. No crash, no price gap.
- Pool id absent from `pools.json` or parse failure → token never in gRPC map → REST prices it (warn once).
- Stale pool (no trades → no updates) → price ages past `momentum_grpc_stale_secs` → REST fallback.
- `MOMENTUM_GRPC_PRICING=false` (default) → the pricer never spawns; zero behavior change. This is the
  critical safety property and is asserted by a test (flag off → watcher uses REST path unchanged).

## Testing

- **`price_from_pool` (pure, core):** fixtures for a CL pool (`sqrt_price_x64` + decimals + orientation) and
  a constant-product pool (reserves + decimals) → assert the quote-unit price. Both token orientations.
- **USD conversion:** quote=USDC → identity; quote=SOL → × SOL/USD ref (assert with a fixed ref).
- **`select_prices` merge/staleness:** map with fresh / stale / missing entries → assert gRPC-fresh chosen
  and the correct remainder routed to REST.
- **Schema:** `momentum_tokens.json` entries with and without `pool`/`quote` round-trip; absent → no
  subscription target for that token.
- **Config:** flag default false; env parsing of the new vars.
- Live subscription (real endpoint) = manual smoke only, not CI.

## Affected files

- Create: `src/portfolio/grpc_pricer.rs` (+ tests).
- Modify: `src/portfolio/mod.rs` (config fields + `pub mod grpc_pricer;`), `src/portfolio/momentum_universe.rs`
  (`WatchedToken` optional fields), `src/portfolio/watcher.rs` (spawn + merge), `.env.example` + relevant
  docs (new env vars).
- Reuse (no change): `src/dex/` (pool parse + price math), `yellowstone-grpc-client`/`-proto` (already deps),
  `src/portfolio/pricer.rs` (REST fallback, unchanged).
- No changes to `src/arbitrage/`, `src/graph/`, `src/bin/solana_mev.rs`, the arb `Config`/`PoolRegistry`, or
  momentum signal/execution code.

## Autonomy notes (user away)

- Implemented on branch `feat/momentum-grpc-pricer`; merged to `main` locally only after a clean opus
  whole-branch review; **not pushed** — left for user review + push.
- The exact `dex` price-derivation entry points and the `yellowstone-grpc-client` subscription API are
  confirmed during plan-writing against the live code; any name corrections are recorded in the ledger.
- If the on-chain price-derivation proves materially harder than reusing existing helpers (e.g. no usable
  spot-price helper and `get_quote` requires more context than a streamed `Pool` carries), the fallback is
  to compute price via a tiny `get_quote(pool, reference_amount, a_to_b)` — recorded as a decision, not a
  silent change.
