# Momentum gRPC Price Feed — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Give the momentum trader an opt-in, low-latency, quota-free on-chain price source (Yellowstone gRPC) with automatic REST fallback, without touching the MEV arb path or momentum decision logic.

**Architecture:** A self-contained `grpc_pricer` runs inside the `portfolio_watcher` process. It resolves each momentum token's configured pool (id in `momentum_tokens.json`, structure from `pools.json`), opens its own Yellowstone subscription to those pools' accounts, derives a USD price from streamed pool state (reusing `dex::PoolState` rate methods + decimal/SOL-USD adjustment), and maintains a shared `mint → (usd, Instant)` map. The watcher prefers a fresh gRPC price and REST-fills the rest. Everything is behind a default-off flag.

**Tech Stack:** Rust, tokio, `yellowstone-grpc-proto`/`tonic` (already deps), `dashmap`, serde.

## Global Constraints

- **Opt-in, default-off:** `MOMENTUM_GRPC_PRICING` defaults `false`. Flag off → byte-identical to today's REST behavior. (Safety property — assert it.)
- **Additive only:** create `src/portfolio/grpc_pricer.rs`; modify `src/portfolio/mod.rs`, `src/portfolio/momentum_universe.rs`, `src/portfolio/watcher.rs`, `.env.example`. Do NOT modify `src/arbitrage/`, `src/graph/`, `src/streamer/`, `src/bin/solana_mev.rs`, the arb `Config`/`PoolRegistry`, or momentum signal/execution code.
- **Do not couple to arb:** the pricer must NOT construct the arb `Config` or `PoolRegistry`. Use the raw `yellowstone-grpc` client directly (mirror the pattern in `src/streamer/client.rs`, but as a separate consumer).
- **COMMIT ONLY, never push.** NEVER run `cargo fmt`/`rustfmt` (repo is not rustfmt-clean).
- **Lib tests:** `cargo test --lib grpc_pricer` (these are library tests, NOT `--bin`).
- **Live gRPC cannot be CI-tested** (needs a real endpoint). Task 4's subscription is verified by `cargo build` + unit tests on its pure sub-functions; live behavior is an operator smoke test, documented, not asserted in CI.

---

## File Structure

- **Create** `src/portfolio/grpc_pricer.rs` — config-driven gRPC ingestion task + pure price derivation (`price_usd`) + pure merge logic (`select_prices`) + tests.
- **Modify** `src/portfolio/mod.rs` — `pub mod grpc_pricer;` + new `PortfolioConfig` fields.
- **Modify** `src/portfolio/momentum_universe.rs` — `WatchedToken` optional `pool`/`quote`.
- **Modify** `src/portfolio/watcher.rs` — spawn pricer (flag-gated) + gRPC/REST merge in the poll loop + publish SOL price.
- **Modify** `.env.example` — document the new env vars.

---

### Task 1: Config + schema (PortfolioConfig fields + WatchedToken pool/quote)

**Files:**
- Modify: `src/portfolio/mod.rs` (PortfolioConfig struct + `from_env` + `pub mod grpc_pricer;`)
- Modify: `src/portfolio/momentum_universe.rs` (WatchedToken)
- Test: `#[cfg(test)]` in `momentum_universe.rs`

**Interfaces:**
- Produces (consumed by later tasks): `PortfolioConfig` fields `momentum_grpc_pricing: bool`, `grpc_endpoint: Option<String>`, `grpc_token: Option<String>`, `pools_path: String`, `momentum_grpc_stale_secs: u64`; `WatchedToken` fields `pool: Option<String>`, `quote: Option<String>`.

- [ ] **Step 1: Add the WatchedToken fields + a failing round-trip test**

In `src/portfolio/momentum_universe.rs`, add to the `WatchedToken` struct (keep all existing fields):
```rust
    #[serde(default)]
    pub pool: Option<String>,
    #[serde(default)]
    pub quote: Option<String>,
```
Add test:
```rust
#[test]
fn watched_token_pool_quote_optional_roundtrip() {
    // entry WITHOUT pool/quote (back-compat) deserializes with None
    let legacy: WatchedToken = serde_json::from_str(
        r#"{"symbol":"MET","mint":"METxxxx","name":"Meteora"}"#).unwrap();
    assert!(legacy.pool.is_none() && legacy.quote.is_none());
    // entry WITH pool/quote
    let withpool: WatchedToken = serde_json::from_str(
        r#"{"symbol":"BP","mint":"BPxxxx","pool":"PoolPubkey","quote":"USDC"}"#).unwrap();
    assert_eq!(withpool.pool.as_deref(), Some("PoolPubkey"));
    assert_eq!(withpool.quote.as_deref(), Some("USDC"));
}
```
> Confirm `WatchedToken`'s existing fields (symbol/mint/name/params) and that `name` is `Option<String>` — match the real struct; only ADD the two fields.

- [ ] **Step 2: Run the test — expect FAIL** (field not present): `cargo test --lib momentum_universe::tests::watched_token_pool_quote_optional_roundtrip`

- [ ] **Step 3: (already added in step 1)** — run again, expect PASS.

- [ ] **Step 4: Add PortfolioConfig fields**

In `src/portfolio/mod.rs`, add fields to `PortfolioConfig` and populate in `from_env()` (mirror the existing `env::var(...).ok()` / parse-with-default pattern used by neighboring momentum fields):
```rust
    pub momentum_grpc_pricing: bool,     // MOMENTUM_GRPC_PRICING, default false
    pub grpc_endpoint: Option<String>,   // GRPC_ENDPOINT
    pub grpc_token: Option<String>,      // GRPC_TOKEN
    pub pools_path: String,              // POOLS_PATH, default "pools.json"
    pub momentum_grpc_stale_secs: u64,   // MOMENTUM_GRPC_STALE_SECS, default 30
```
In `from_env()`:
```rust
    momentum_grpc_pricing: std::env::var("MOMENTUM_GRPC_PRICING").map(|v| v == "true").unwrap_or(false),
    grpc_endpoint: std::env::var("GRPC_ENDPOINT").ok(),
    grpc_token: std::env::var("GRPC_TOKEN").ok(),
    pools_path: std::env::var("POOLS_PATH").unwrap_or_else(|_| "pools.json".to_string()),
    momentum_grpc_stale_secs: std::env::var("MOMENTUM_GRPC_STALE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30),
```
Add `pub mod grpc_pricer;` with the other `pub mod` lines.

- [ ] **Step 5: Build + commit**

Run: `cargo build --lib` → compiles (note: `grpc_pricer` module must at least exist as an empty file with the public items stubbed, or add `pub mod grpc_pricer;` in Task 2 — to keep this task compiling, create `src/portfolio/grpc_pricer.rs` containing only a doc comment for now, and add the items in Task 2/3/4).
Run: `cargo test --lib momentum_universe` → passes.
```bash
git add src/portfolio/mod.rs src/portfolio/momentum_universe.rs src/portfolio/grpc_pricer.rs
git commit -m "feat(grpc-pricer): config flags + WatchedToken pool/quote schema"
```

---

### Task 2: Pure USD price derivation from pool state

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs`
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Consumes: `dex::types::PoolState` (existing: `rate_a_to_b()`, `rate_b_to_a()` — fee-adjusted raw atomic-unit rates).
- Produces: `pub fn price_usd(state: &PoolState, momentum_is_token_a: bool, dec_momentum: u8, dec_quote: u8, quote_is_usdc: bool, sol_usd: f64) -> Option<f64>`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::PoolState;

    // Constant-product, momentum=token_a, quote=token_b=USDC, equal decimals (6/6).
    // reserve_b/reserve_a = 200/100 = 2.0 (fee 0 for simplicity via fee_bps=0).
    #[test]
    fn cp_usdc_quote_equal_decimals() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s, true, 6, 6, true, 0.0).unwrap();
        assert!((p - 2.0).abs() < 1e-9);
    }

    // SOL quote: price_in_sol × sol_usd. reserveB/reserveA=2.0 SOL per token, SOL=$150 → $300.
    #[test]
    fn cp_sol_quote_applies_sol_usd() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s, true, 9, 9, false, 150.0).unwrap();
        assert!((p - 300.0).abs() < 1e-6);
    }

    // Decimal adjustment: momentum has 6 dp, quote(USDC) 6 dp already covered;
    // here momentum=token_a 9dp, quote=token_b 6dp → ×10^(9-6)=1000.
    #[test]
    fn decimal_adjustment_scales_price() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s, true, 9, 6, true, 0.0).unwrap();
        assert!((p - 2000.0).abs() < 1e-6); // 2.0 × 10^3
    }

    // momentum=token_b path uses rate_b_to_a.
    #[test]
    fn momentum_is_token_b_uses_inverse_rate() {
        let s = PoolState::ConstantProduct { reserve_a: 200, reserve_b: 100, fee_bps: 0 };
        // momentum=token_b, quote=token_a=USDC, equal dp: rate_b_to_a = reserve_a/reserve_b = 2.0
        let p = price_usd(&s, false, 6, 6, true, 0.0).unwrap();
        assert!((p - 2.0).abs() < 1e-9);
    }

    // Degenerate input → None (not a panic, not a zero price).
    #[test]
    fn zero_reserves_returns_none() {
        let s = PoolState::ConstantProduct { reserve_a: 0, reserve_b: 200, fee_bps: 0 };
        assert!(price_usd(&s, true, 6, 6, true, 0.0).is_none());
    }
}
```

- [ ] **Step 2: Run — expect FAIL** (`price_usd` not defined): `cargo test --lib grpc_pricer::tests`

- [ ] **Step 3: Implement `price_usd`**

```rust
//! gRPC on-chain price source for the momentum trader (opt-in; REST fallback).
use crate::dex::types::PoolState;

/// USD price of the momentum token from current pool state.
/// `rate_a_to_b`/`rate_b_to_a` are atomic-unit rates (quote-atomic per momentum-atomic),
/// so we convert to human units with 10^(dec_momentum - dec_quote), then to USD
/// (quote=USDC → identity; quote=SOL → × sol_usd). Returns None on degenerate state.
pub fn price_usd(
    state: &PoolState,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    sol_usd: f64,
) -> Option<f64> {
    let raw = if momentum_is_token_a { state.rate_a_to_b() } else { state.rate_b_to_a() };
    if !raw.is_finite() || raw <= 0.0 { return None; }
    let price_in_quote = raw * 10f64.powi(dec_momentum as i32 - dec_quote as i32);
    let usd = if quote_is_usdc { price_in_quote } else { price_in_quote * sol_usd };
    if usd.is_finite() && usd > 0.0 { Some(usd) } else { None }
}
```

- [ ] **Step 4: Run — expect PASS.** Also add one CL test:
```rust
    #[test]
    fn cl_pool_uses_sqrt_price() {
        // sqrt_price_x64 = 2^64 → price = 1.0; equal dp, USDC quote → $1.0
        let s = PoolState::ConcentratedLiquidity { sqrt_price_x64: 1u128 << 64, liquidity: 0, fee_bps: 0 };
        let p = price_usd(&s, true, 6, 6, true, 0.0).unwrap();
        assert!((p - 1.0).abs() < 1e-9);
    }
```
Run: `cargo test --lib grpc_pricer::tests` → all pass.

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/grpc_pricer.rs
git commit -m "feat(grpc-pricer): pure USD price derivation from pool state"
```

---

### Task 3: Pure gRPC/REST merge + staleness selection

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs`
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Produces: `pub type GrpcPriceMap = std::sync::Arc<dashmap::DashMap<String, (f64, std::time::Instant)>>;` and
  `pub fn select_prices(map: &GrpcPriceMap, watched_mints: &[String], stale: std::time::Duration, now: std::time::Instant) -> (std::collections::HashMap<String, f64>, Vec<String>)` — returns (fresh gRPC prices to use, mints to REST-fetch).

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn select_prices_prefers_fresh_grpc_rest_fills_rest() {
        use std::time::{Duration, Instant};
        let map: GrpcPriceMap = std::sync::Arc::new(dashmap::DashMap::new());
        let now = Instant::now();
        let fresh = now;                                   // age 0
        let stale = now - Duration::from_secs(120);        // older than window
        map.insert("FRESH".into(), (1.23, fresh));
        map.insert("STALE".into(), (9.99, stale));
        // "MISS" not in map
        let watched = vec!["FRESH".to_string(), "STALE".to_string(), "MISS".to_string()];
        let (use_grpc, to_rest) = select_prices(&map, &watched, Duration::from_secs(30), now);
        assert_eq!(use_grpc.get("FRESH"), Some(&1.23));
        assert!(!use_grpc.contains_key("STALE") && !use_grpc.contains_key("MISS"));
        let mut rest_sorted = to_rest.clone(); rest_sorted.sort();
        assert_eq!(rest_sorted, vec!["MISS".to_string(), "STALE".to_string()]);
    }
```

- [ ] **Step 2: Run — expect FAIL.** `cargo test --lib grpc_pricer::tests::select_prices_prefers_fresh_grpc_rest_fills_rest`

- [ ] **Step 3: Implement**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use dashmap::DashMap;

pub type GrpcPriceMap = Arc<DashMap<String, (f64, Instant)>>;

/// Split the watched mints into (fresh gRPC prices to use, mints that need REST).
pub fn select_prices(
    map: &GrpcPriceMap,
    watched_mints: &[String],
    stale: Duration,
    now: Instant,
) -> (HashMap<String, f64>, Vec<String>) {
    let mut use_grpc = HashMap::new();
    let mut to_rest = Vec::new();
    for m in watched_mints {
        match map.get(m) {
            Some(e) if now.duration_since(e.value().1) <= stale && e.value().0 > 0.0 => {
                use_grpc.insert(m.clone(), e.value().0);
            }
            _ => to_rest.push(m.clone()),
        }
    }
    (use_grpc, to_rest)
}
```
> Confirm `dashmap` is a dependency (it is — used by `dex`). If not present for the lib target, add it.

- [ ] **Step 4: Run — expect PASS.**

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/grpc_pricer.rs
git commit -m "feat(grpc-pricer): pure gRPC/REST merge + staleness selection"
```

---

### Task 4: gRPC ingestion task (subscription + per-pool state tracking)

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs`
- Test: same file `#[cfg(test)]` (pure per-pool update→price; the live subscription is NOT unit-tested)

**Interfaces:**
- Consumes: `PortfolioConfig` (Task 1), `WatchedToken` (Task 1), `price_usd` (Task 2), `GrpcPriceMap` (Task 3), `dex` parsers (`parse_spl_token_amount`, `parse_cl_pool_state`), `PoolConfig`/`Pool` loading from `pools.json`.
- Produces: `pub fn spawn(cfg: &PortfolioConfig, watched: &[WatchedToken], sol_usd: Arc<std::sync::atomic::AtomicU64>) -> anyhow::Result<(GrpcPriceMap, tokio::task::JoinHandle<()>)>` and a pure helper `pub fn pool_price_from_state(...) -> Option<f64>` (the per-pool update→USD step, unit-tested).

**IMPORTANT — implementer reads first:** `src/streamer/client.rs` is the reference for the Yellowstone connection (tonic `Channel` + `ClientTlsConfig`, `GeyserClient`, building a `SubscribeRequest` with `accounts` filters, consuming `inbound` via `StreamExt`, matching `UpdateOneof::Account`, reconnect-with-backoff). Mirror that connection/loop pattern here as a SEPARATE consumer — do NOT import or construct the arb `Config`/`PoolRegistry`/`GrpcStreamer`. Read `src/dex/mod.rs` for `parse_spl_token_amount(data) -> Option<u64>` (SPL vault balance) and `parse_cl_pool_state(data, &Pool) -> Option<(f64, u64)>` (CL sqrt-price; confirm return shape), and how `pools.json` entries become `Pool` (`PoolConfig` → `Pool::try_from`).

- [ ] **Step 1: Write a failing test for the pure per-pool step**

`pool_price_from_state` takes a tracked pool's current reserves/sqrt-price + metadata and returns USD. Test it directly (no network):
```rust
    #[test]
    fn pool_price_from_state_cp_usdc() {
        // a tracked CP pool: momentum=token_a (6dp), quote=USDC token_b (6dp), reserves 100/250
        let st = TrackedPool {
            kind: PoolKind::ConstantProduct { fee_bps: 0 },
            reserve_a: Some(100), reserve_b: Some(250), sqrt_price_x64: None,
            momentum_is_token_a: true, dec_momentum: 6, dec_quote: 6, quote_is_usdc: true,
        };
        let p = pool_price_from_state(&st, 0.0).unwrap();
        assert!((p - 2.5).abs() < 1e-9);
    }
    #[test]
    fn pool_price_from_state_incomplete_returns_none() {
        let st = TrackedPool {
            kind: PoolKind::ConstantProduct { fee_bps: 0 },
            reserve_a: Some(100), reserve_b: None, sqrt_price_x64: None,
            momentum_is_token_a: true, dec_momentum: 6, dec_quote: 6, quote_is_usdc: true,
        };
        assert!(pool_price_from_state(&st, 0.0).is_none()); // missing reserve_b
    }
```

- [ ] **Step 2: Run — expect FAIL** (types/fn missing): `cargo test --lib grpc_pricer::tests::pool_price_from_state_cp_usdc`

- [ ] **Step 3: Implement the tracked-pool model + pure price step**

```rust
use crate::dex::types::PoolState;

#[derive(Debug, Clone)]
pub enum PoolKind { ConstantProduct { fee_bps: u64 }, ConcentratedLiquidity { fee_bps: u64 } }

/// Live-tracked state for one momentum pool, updated as account updates arrive.
#[derive(Debug, Clone)]
pub struct TrackedPool {
    pub kind: PoolKind,
    pub reserve_a: Option<u64>,
    pub reserve_b: Option<u64>,
    pub sqrt_price_x64: Option<u128>,
    pub momentum_is_token_a: bool,
    pub dec_momentum: u8,
    pub dec_quote: u8,
    pub quote_is_usdc: bool,
}

/// Build a PoolState from whatever is currently tracked, then derive USD. None if
/// the state isn't complete enough yet (e.g. only one vault seen so far).
pub fn pool_price_from_state(t: &TrackedPool, sol_usd: f64) -> Option<f64> {
    let state = match &t.kind {
        PoolKind::ConstantProduct { fee_bps } => PoolState::ConstantProduct {
            reserve_a: t.reserve_a?, reserve_b: t.reserve_b?, fee_bps: *fee_bps,
        },
        PoolKind::ConcentratedLiquidity { fee_bps } => PoolState::ConcentratedLiquidity {
            sqrt_price_x64: t.sqrt_price_x64?, liquidity: 0, fee_bps: *fee_bps,
        },
    };
    price_usd(&state, t.momentum_is_token_a, t.dec_momentum, t.dec_quote, t.quote_is_usdc, sol_usd)
}
```

- [ ] **Step 4: Run — expect PASS.**

- [ ] **Step 5: Implement `spawn` (the live subscription) — compile-verified, live-smoke by operator**

Implement `spawn(cfg, watched, sol_usd)`:
1. Load `cfg.pools_path` into `PoolConfig`s; index by pool id (string).
2. For each `watched` token with `Some(pool)`+`Some(quote)`: find the `PoolConfig`, build a `Pool` (`Pool::try_from`), determine `momentum_is_token_a` (compare `token.mint` to the pool's token_a mint), `quote_is_usdc` (`quote == "USDC"`), fetch decimals (the watcher already resolves decimals — accept a `&HashMap<String,u8>` arg OR read from the pool config; confirm the cleanest source and wire it). Build a `TrackedPool` and record which account pubkeys feed it (AMM: `vault_a`→reserve_a, `vault_b`→reserve_b; CL: `state_account`→sqrt_price). Skip + `warn!` any token whose pool id is missing/unparseable.
3. Build the shared `GrpcPriceMap`. Spawn a tokio task that connects to `cfg.grpc_endpoint` (with `cfg.grpc_token` as `x-token`) mirroring `streamer/client.rs`, subscribes to the collected account pubkeys, and on each `UpdateOneof::Account`: map the pubkey → its pool + role, update the `TrackedPool` field (`parse_spl_token_amount` for vaults; `parse_cl_pool_state` for CL state), recompute `pool_price_from_state(&tracked, f64::from_bits(sol_usd.load(Relaxed)))`, and on `Some(usd)` insert `(usd, Instant::now())` into the map keyed by the token mint. Reconnect with capped backoff on stream error.
4. Return `(map, handle)`.

Run: `cargo build --bin portfolio-watcher` (or `cargo build` for the workspace) → MUST compile.

- [ ] **Step 6: Commit**
```bash
git add src/portfolio/grpc_pricer.rs
git commit -m "feat(grpc-pricer): Yellowstone subscription + per-pool state tracking (live path; operator smoke required)"
```

---

### Task 5: Watcher integration (flag-gated spawn + merge) + docs

**Files:**
- Modify: `src/portfolio/watcher.rs`
- Modify: `.env.example`
- Test: `#[cfg(test)]` in `watcher.rs` if a pure seam exists; otherwise the merge is covered by Task 3's `select_prices` test and this task is verified by build + flag-off reasoning.

**Interfaces:**
- Consumes: `grpc_pricer::{spawn, select_prices, GrpcPriceMap}`, `PortfolioConfig`.

- [ ] **Step 1: Wire the spawn (flag-gated)**

In `watcher::run`, after the watched set + decimals are known, before the poll loop:
```rust
    let sol_usd_bits = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let grpc_map: Option<grpc_pricer::GrpcPriceMap> =
        if cfg.momentum_grpc_pricing && cfg.grpc_endpoint.is_some() {
            match grpc_pricer::spawn(&cfg, &watched, sol_usd_bits.clone()) {
                Ok((map, _handle)) => { tracing::info!("momentum: gRPC price feed active"); Some(map) }
                Err(e) => { tracing::warn!("momentum: gRPC pricer failed to start ({e}); REST only"); None }
            }
        } else { None };
```

- [ ] **Step 2: Wire the merge into the poll loop**

Where the loop currently builds `last_prices` via `pricer::fetch_prices`:
1. Fetch SOL/USD as today; publish: `sol_usd_bits.store(sol_price.to_bits(), Relaxed);`
2. If `grpc_map` is Some: `let (grpc_prices, rest) = grpc_pricer::select_prices(map, &watched_mints, Duration::from_secs(cfg.momentum_grpc_stale_secs), Instant::now());` then REST-fetch only `rest`, and merge `grpc_prices` over the REST result. If None: today's path (REST for all) unchanged.

> Confirm the exact variable names (`watched_mints`, the price map var) against `watcher.rs` and integrate minimally; the flag-off branch MUST be the existing code path verbatim.

- [ ] **Step 3: Build + flag-off equivalence check**

Run: `cargo build` → compiles.
Run: `cargo test --lib` → all pass (no regression).
Manually confirm: with `momentum_grpc_pricing=false` (default), `grpc_map` is `None` and the loop runs the exact existing REST path.

- [ ] **Step 4: Document the env vars**

Add to `.env.example` (under a momentum section):
```
# Opt-in: price momentum tokens from Yellowstone gRPC (on-chain pool state) instead of REST.
# Requires GRPC_ENDPOINT (+ optional GRPC_TOKEN) and a `pool`+`quote` per token in momentum_tokens.json
# (the pool must also exist in pools.json). Default off = REST only.
MOMENTUM_GRPC_PRICING=false
MOMENTUM_GRPC_STALE_SECS=30
# POOLS_PATH=pools.json
```

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/watcher.rs .env.example
git commit -m "feat(grpc-pricer): flag-gated watcher integration (gRPC-preferred, REST fallback) + env docs"
```

---

## Self-Review

**Spec coverage:**
- Config flags + schema → Task 1. ✓
- `grpc_pricer` module, price derivation → Tasks 2, 4. ✓
- Merge/staleness → Task 3. ✓
- Watcher integration (flag-gated, SOL publish, merge) → Task 5. ✓
- Error handling (reconnect, missing pool → REST, stale → REST, flag-off no-op) → Task 4 step 5 + Task 5 flag gate. ✓
- Testing (pure price, USD conversion, merge, schema) → Tasks 1–4 tests. ✓
- Default-off safety → Task 1 (default false) + Task 5 (flag gate); flag-off equivalence is the key assertion. ✓
- Additive-only / no arb coupling → Global Constraints; Task 4 uses raw client, not arb Config/PoolRegistry. ✓

**Placeholder scan:** Task 4 step 5 and Task 5 steps 1–2 describe integration against `watcher.rs`/`pools.json` internals not shown verbatim — these are flagged "confirm against real code" with the exact functions named (`parse_spl_token_amount`, `parse_cl_pool_state`, `Pool::try_from`, `select_prices`). The pure tasks (1–3, and Task 4's `pool_price_from_state`) have complete code. This is the deliberate verified/unverified split: live subscription is compile-checked + operator-smoke, pure logic is fully unit-tested.

**Type consistency:** `price_usd` (Task 2) ← `pool_price_from_state` (Task 4) ← `spawn` (Task 4); `GrpcPriceMap`/`select_prices` (Task 3) ← watcher (Task 5). Names consistent.

## Known implementer confirmations (exact values to verify in-repo, not placeholders)
- `WatchedToken` existing fields + `name: Option<String>` (Task 1) — `momentum_universe.rs`.
- `PortfolioConfig::from_env` parse pattern + neighboring field style (Task 1) — `mod.rs`.
- `parse_cl_pool_state` return shape `(f64, u64)` and what the f64 is (sqrt-price vs price) (Task 4) — `dex/mod.rs:411`.
- Cleanest decimals source for the watched tokens (Task 4) — the watcher already calls `scanner::fetch_decimals_for_mints`; pass that map into `spawn`.
- `pools.json` → `PoolConfig` → `Pool::try_from`, and the `extra` accounts per DEX (vault_a/vault_b/state_account) (Task 4) — `dex/types.rs`, `dex/mod.rs`.
- Watcher poll-loop variable names + the `pricer::fetch_prices` call site (Task 5) — `watcher.rs`.

## Autonomy / operator notes
- Built default-off; merging to `main` is safe (no behavior change until `MOMENTUM_GRPC_PRICING=true`).
- **Operator smoke test required before relying on it:** set `GRPC_ENDPOINT`/`GRPC_TOKEN`, add `pool`+`quote` to a couple of `momentum_tokens.json` entries (pools must be in `pools.json`), set `MOMENTUM_GRPC_PRICING=true`, run `portfolio-watcher`, and confirm the log shows "gRPC price feed active" and those tokens' prices track on-chain (compare against DexScreener). The controller cannot run this (no live endpoint).
