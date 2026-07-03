# gRPC Pricing Architecture Upgrade — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make gRPC the near-permanent price source for all watched momentum tokens (seed at boot, trust-until-changed with REST divergence cross-check, multi-pool), and guard entry execution with live pool state.

**Architecture:** All changes live in the momentum pricing path: `src/bin/portfolio_watcher.rs` (ingestion: seeding, per-account apply helper, impact publish), `src/portfolio/grpc_pricer.rs` (selection semantics, distrust/xcheck state on `GrpcFeed`), `src/portfolio/watcher.rs` (xcheck orchestration), `src/portfolio/momentum_universe.rs` (multi-pool schema), `src/portfolio/momentum.rs` (entry guards), `src/portfolio/mod.rs` (config). Spec: `docs/superpowers/specs/2026-07-03-grpc-pricing-architecture-design.md`.

**Tech Stack:** Rust (tokio, dashmap, yellowstone-grpc), solana_client blocking RpcClient via `spawn_blocking` (existing scanner pattern).

## Global Constraints

- **Off = byte-identical.** Every behavior change is gated: `MOMENTUM_GRPC_STALE_SECS` keeps default `90` (TTL mode, today's behavior); new vars default off (`MOMENTUM_ENTRY_DIVERGENCE_BPS=0`, `MOMENTUM_LOCAL_IMPACT=false`). Trust-until-changed activates ONLY at `MOMENTUM_GRPC_STALE_SECS=0`.
- **Do NOT run `cargo fmt` or `rustfmt` on any file** (repo is not rustfmt-clean; see project memory).
- Tests live in `#[cfg(test)]` blocks at the bottom of each source file. Lib tests: `cargo test --lib <filter>`. The `portfolio_watcher` binary's code compiles with `cargo build --release --bin portfolio-watcher`; it has no test harness — binary-side logic must be testable via lib-side pure functions where required.
- Commit after each task; do not push.
- Env var names (exact): `MOMENTUM_GRPC_XCHECK_SECS` (default 300), `MOMENTUM_GRPC_XCHECK_BPS` (default 100), `MOMENTUM_ENTRY_DIVERGENCE_BPS` (default 0), `MOMENTUM_LOCAL_IMPACT` (default false).
- Config field names (exact): `momentum_grpc_xcheck_secs: u64`, `momentum_grpc_xcheck_bps: u32`, `momentum_entry_divergence_bps: u32`, `momentum_local_impact: bool`.

---

### Task 1: Seed pool state at connect + factor the apply helper (spec A + G doc fixes)

**Files:**
- Modify: `src/bin/portfolio_watcher.rs` (factor `apply_update` out of `run_grpc_stream`; add `seed_pool_state`; call seed at the top of every `run_grpc_stream` cycle; add 10 s unpriced-retry)
- Modify: `src/portfolio/grpc_pricer.rs:9-11` (module doc: remove the "Only constant-product (raydium_amm_v4)" claim)
- Modify: `src/bin/portfolio_watcher.rs:86-89` (`spawn_grpc_feed` docstring: same stale claim)

**Interfaces:**
- Produces: `fn apply_update(w: &WiredPool, role: Role, data: &[u8], feed: &GrpcFeed)` — parses account data by role, stores pool atomics, computes `price_usd`, inserts into `feed.map`, calls `feed.note_update`. Used by the stream loop, the seeder, and (Task 5) extended for impact publish.
- Produces: `async fn seed_pool_state(rpc_url: String, acct_index: HashMap<String,(usize,Role)>, ...)` — see Step 3; runs via `tokio::task::spawn_blocking` with `solana_client::rpc_client::RpcClient::get_multiple_accounts` (chunks of 100), then feeds each account through `apply_update`.
- Consumes: `dex::parse_spl_token_amount`, `dex::parse_cl_pool_state`, `WiredPool::price_usd` (all existing).

- [ ] **Step 1: Factor the apply helper.** In `src/bin/portfolio_watcher.rs`, extract the body of the `while let Some(msg)` match arms (lines ~266-288) into:

```rust
/// Apply one account write (from the stream or from seeding) to its wired pool:
/// parse by role, store the pool atomics, recompute the USD price, publish it.
fn apply_update(w: &WiredPool, role: Role, data: &[u8], feed: &GrpcFeed) {
    match role {
        Role::VaultA | Role::VaultB => {
            let Some(amt) = dex::parse_spl_token_amount(data) else { return };
            if matches!(role, Role::VaultA) {
                w.pool.reserve_a.store(amt, std::sync::atomic::Ordering::Relaxed);
            } else {
                w.pool.reserve_b.store(amt, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Role::State => {
            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(data, &w.pool) {
                w.pool.sqrt_price_x64.store(price.to_bits(), std::sync::atomic::Ordering::Relaxed);
                if fee_bps > 0 {
                    w.pool.fee_bps.store(fee_bps, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    if let Some(usd) = w.price_usd(feed.sol_usd()) {
        feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));
        feed.note_update(&w.token_mint);
    }
}
```

The stream loop body becomes: `apply_update(&wired[idx], role, &info.data, feed);` (note `wired` no longer needs `&mut` — `apply_update` takes `&WiredPool`; change `run_grpc_stream`'s param to `wired: &[WiredPool]` and the spawn loop accordingly).

- [ ] **Step 2: Build.** `cargo build --release --bin portfolio-watcher` — expect success, behavior unchanged.

- [ ] **Step 3: Add the seeder.** Add to `src/bin/portfolio_watcher.rs`:

```rust
/// Seed every subscribed account's current state via RPC so wired pools have a price
/// from t=0 (the gRPC stream only delivers *changes*; a quiet pool would otherwise
/// have no price until its first post-boot trade). Called at the top of every
/// `run_grpc_stream` cycle, so reconnect gaps are also re-seeded.
async fn seed_pool_state(
    rpc_url: &str,
    acct_index: &HashMap<String, (usize, Role)>,
    wired: &[WiredPool],
    feed: &GrpcFeed,
) {
    let keys: Vec<String> = acct_index.keys().cloned().collect();
    let rpc_url = rpc_url.to_string();
    let fetched = tokio::task::spawn_blocking(move || {
        let rpc = solana_client::rpc_client::RpcClient::new(rpc_url);
        let mut out: Vec<(String, Vec<u8>)> = Vec::new();
        for chunk in keys.chunks(100) {
            let pks: Vec<solana_sdk::pubkey::Pubkey> =
                chunk.iter().filter_map(|k| k.parse().ok()).collect();
            match rpc.get_multiple_accounts(&pks) {
                Ok(accts) => {
                    for (pk, acct) in pks.iter().zip(accts) {
                        if let Some(a) = acct {
                            out.push((pk.to_string(), a.data));
                        }
                    }
                }
                Err(e) => tracing::warn!("gRPC seed: getMultipleAccounts failed: {e}"),
            }
        }
        out
    })
    .await
    .unwrap_or_default();

    let mut seeded = 0usize;
    for (key, data) in &fetched {
        if let Some(&(idx, role)) = acct_index.get(key) {
            apply_update(&wired[idx], role, data, feed);
            seeded += 1;
        }
    }
    info!("gRPC seed: applied {seeded}/{} accounts, {} price(s) live", acct_index.len(), feed.map.len());
}
```

Call it as the FIRST statement of `run_grpc_stream` (before the channel connect):
`seed_pool_state(rpc_url, acct_index, wired, feed).await;` — add `rpc_url: &str` to `run_grpc_stream`'s params and thread `cfg.rpc_url.clone()` through `spawn_grpc_feed`'s task (capture it in the spawned closure).

- [ ] **Step 4: Unpriced retry (SOL/USD bootstrap).** SOL-quoted pools seed their *state* but `price_usd` returns `None` until the watcher publishes `sol_usd`. In `run_grpc_stream`, replace the plain `while let Some(msg) = inbound.next().await` with a `tokio::select!` loop: one arm is the stream message (existing logic), the other a `tokio::time::interval(Duration::from_secs(10))` tick that, for each wired pool with no `feed.map` entry, re-attempts `if let Some(usd) = w.price_usd(feed.sol_usd()) { feed.map.insert(...); feed.note_update(...); }`. Exit the loop when the stream returns `None` (preserve current return-to-reconnect behavior).

- [ ] **Step 5: Doc fixes (spec G).** `src/portfolio/grpc_pricer.rs` lines 9-11: replace the "Only constant-product (raydium_amm_v4) pools are priced via gRPC so far" sentence with "CP (raydium_amm_v4/saber) pools are priced from vault reserves; CL pools (Orca Whirlpool, Raydium CLMM, Meteora DLMM, Invariant) from their state account. Other DEX kinds fall back to REST." Same correction in the `spawn_grpc_feed` doc comment (`src/bin/portfolio_watcher.rs:86-89`).

- [ ] **Step 6: Build + smoke.** `cargo build --release --bin portfolio-watcher` then `GRPC_PRICE_SMOKE=1 ./target/release/portfolio-watcher` — expect ALL wired tokens (MET, BP, JTO, SLX, ARX, PUMP, JUP, ORE) to print prices within seconds (USDC-quoted immediately from seed; SOL-quoted may print via retry only if sol_usd is published — in smoke mode sol_usd stays 0, so USDC-quoted pools printing from seed without waiting for a trade is the pass signal; note this in the report).

- [ ] **Step 7: Run lib tests.** `cargo test --lib grpc_pricer` — all existing tests pass (no lib change beyond docs).

- [ ] **Step 8: Commit.** `git add -A src/bin/portfolio_watcher.rs src/portfolio/grpc_pricer.rs && git commit -m "feat(grpc-pricing): seed pool state at connect; factor apply_update; unpriced retry; fix stale docs"`

---

### Task 2: Trust-until-changed + divergence cross-check (spec B)

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs` (`GrpcFeed` xcheck/distrust state + methods; `select_prices` semantics; tests)
- Modify: `src/portfolio/mod.rs` (config fields + env parsing, lines ~213 and ~340)
- Modify: `src/portfolio/watcher.rs` (xcheck orchestration around lines 515-560)
- Modify: `.env.example`, `docs/portfolio/momentum-trader.md` (document the new vars; if that doc path doesn't exist, `grep -rl "MOMENTUM_GRPC_PRICING" docs/` and update the file found)

**Interfaces:**
- Produces on `GrpcFeed`: `xcheck: Arc<RwLock<HashMap<String, XcheckState>>>` where `struct XcheckState { last: Option<Instant>, distrusted: bool }`; methods `distrusted_snapshot() -> HashSet<String>`, `xcheck_due(mint: &str, every: Duration, now: Instant) -> bool`, `record_xcheck(mint: &str, ok: bool, now: Instant)`. `note_update` additionally clears `distrusted` for the mint (a fresh write re-earns trust).
- Changes: `select_prices(map, watched_mints, stale, now, distrusted: &HashSet<String>)` — new final param; `stale == Duration::ZERO` means trust-until-changed (no TTL); distrusted mints always go to `to_rest`.
- Produces config: `momentum_grpc_xcheck_secs: u64` (300), `momentum_grpc_xcheck_bps: u32` (100).

- [ ] **Step 1: Write failing tests** in `grpc_pricer.rs` `#[cfg(test)]`:

```rust
#[test]
fn select_prices_zero_stale_trusts_forever_but_respects_distrust() {
    let map: GrpcPriceMap = Arc::new(DashMap::new());
    let now = Instant::now();
    map.insert("OLD".into(), (1.0, now - Duration::from_secs(100_000)));
    map.insert("BAD".into(), (2.0, now));
    let watched = vec!["OLD".to_string(), "BAD".to_string()];
    let distrusted: HashSet<String> = ["BAD".to_string()].into();
    let (use_grpc, to_rest) = select_prices(&map, &watched, Duration::ZERO, now, &distrusted);
    assert_eq!(use_grpc.get("OLD"), Some(&1.0)); // age irrelevant in trust mode
    assert_eq!(to_rest, vec!["BAD".to_string()]); // distrust forces REST
}

#[test]
fn xcheck_due_and_record_lifecycle() {
    let feed = GrpcFeed::new();
    let now = Instant::now();
    let every = Duration::from_secs(300);
    assert!(feed.xcheck_due("M", every, now));            // never checked → due
    feed.record_xcheck("M", true, now);
    assert!(!feed.xcheck_due("M", every, now));            // just checked → not due
    assert!(feed.xcheck_due("M", every, now + Duration::from_secs(301)));
    feed.record_xcheck("M", false, now);                   // diverged
    assert!(feed.distrusted_snapshot().contains("M"));
    feed.note_update("M");                                 // fresh write clears distrust
    assert!(!feed.distrusted_snapshot().contains("M"));
}
```

Also update the existing `select_prices_prefers_fresh_grpc_rest_fills_rest` test to pass `&HashSet::new()` as the new param. Run `cargo test --lib grpc_pricer` — expect the two new tests to FAIL to compile (missing methods/param), which is the failing state for signature-change TDD.

- [ ] **Step 2: Implement** the `XcheckState` struct + `GrpcFeed` field (init in `new()`) + three methods + `note_update` distrust-clear + `select_prices` new param and `stale.is_zero()` branch:

```rust
Some(e) if !distrusted.contains(m)
    && (stale.is_zero() || now.duration_since(e.value().1) <= stale)
    && e.value().0 > 0.0 => { use_grpc.insert(m.clone(), e.value().0); }
```

- [ ] **Step 3: Run tests.** `cargo test --lib grpc_pricer` — all pass.

- [ ] **Step 4: Config.** In `src/portfolio/mod.rs` add the two fields next to `momentum_grpc_stale_secs` (with doc comments: xcheck only active when `momentum_grpc_stale_secs == 0`), and parsing:

```rust
momentum_grpc_xcheck_secs: std::env::var("MOMENTUM_GRPC_XCHECK_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(300),
momentum_grpc_xcheck_bps: std::env::var("MOMENTUM_GRPC_XCHECK_BPS").ok().and_then(|v| v.parse().ok()).unwrap_or(100),
```

- [ ] **Step 5: Watcher orchestration.** In `src/portfolio/watcher.rs` (around line 518), replace the `select_prices` call and REST merge with:

```rust
let distrusted = grpc_feed.as_ref().map(|f| f.distrusted_snapshot()).unwrap_or_default();
let (mut grpc_prices, mut rest_mints) = match &grpc_feed {
    Some(feed) => grpc_pricer::select_prices(
        &feed.map, &token_mints,
        Duration::from_secs(cfg.momentum_grpc_stale_secs),
        Instant::now(), &distrusted,
    ),
    None => (HashMap::new(), token_mints.clone()),
};
// Trust-until-changed mode: periodically REST-fetch gRPC-priced mints anyway and
// compare. Divergence > threshold distrusts the mint back to REST until it re-agrees
// or a fresh on-chain write arrives.
let mut xcheck_mints: Vec<String> = Vec::new();
if cfg.momentum_grpc_stale_secs == 0 && cfg.momentum_grpc_xcheck_secs > 0 {
    if let Some(feed) = &grpc_feed {
        let every = Duration::from_secs(cfg.momentum_grpc_xcheck_secs);
        let now = Instant::now();
        for m in grpc_prices.keys() {
            if feed.xcheck_due(m, every, now) { xcheck_mints.push(m.clone()); }
        }
        rest_mints.extend(xcheck_mints.iter().cloned());
    }
}
```

and after `pricer::fetch_prices` succeeds (inside the `Ok(mut p)` arm, BEFORE `p.extend(grpc_prices)`):

```rust
if let Some(feed) = &grpc_feed {
    let now = Instant::now();
    for m in &xcheck_mints {
        if let (Some(&g), Some(&r)) = (grpc_prices.get(m), p.get(m)) {
            let dev_bps = ((g - r).abs() / r * 10_000.0) as u32;
            let ok = dev_bps <= cfg.momentum_grpc_xcheck_bps;
            feed.record_xcheck(m, ok, now);
            if !ok {
                warn!("momentum: xcheck DIVERGED {m}: grpc=${g:.6} rest=${r:.6} ({dev_bps}bps > {}bps) — distrusting", cfg.momentum_grpc_xcheck_bps);
                grpc_prices.remove(m); // REST value already in p wins this tick
            }
        }
    }
}
p.extend(grpc_prices);
```

(The `Err` arm's `grpc_prices` fallback is unchanged.) Note `grpc_prices` must now be `mut` and the extend moves inside both arms consistently — keep the existing `Err` behavior identical.

- [ ] **Step 6: Build + full lib tests.** `cargo build --release --bin portfolio-watcher && cargo test --lib` — pass.

- [ ] **Step 7: Docs.** Add the three vars (`MOMENTUM_GRPC_STALE_SECS=0` semantics, `MOMENTUM_GRPC_XCHECK_SECS`, `MOMENTUM_GRPC_XCHECK_BPS`) to `.env.example` under the momentum gRPC block and to the momentum trader doc's env table.

- [ ] **Step 8: Commit.** `git commit -m "feat(grpc-pricing): MOMENTUM_GRPC_STALE_SECS=0 trust-until-changed + REST divergence cross-check (xcheck secs/bps)"`

---

### Task 3: Multi-pool per token (spec D)

**Files:**
- Modify: `src/portfolio/momentum_universe.rs` (`PoolRef` + `pools` field + `pool_refs()`; tests)
- Modify: `src/bin/portfolio_watcher.rs` (`spawn_grpc_feed` iterates `pool_refs()`; cross-venue divergence log in `apply_update`)
- Modify: `src/portfolio/watcher.rs:536` (wired-label check uses `pool_refs()`)

**Interfaces:**
- Produces: `pub struct PoolRef { pub pool: String, pub quote: String }` (serde `Deserialize`/`Serialize`, `Clone`, `Debug`).
- Produces: `WatchedToken.pools: Option<Vec<PoolRef>>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`).
- Produces: `impl WatchedToken { pub fn pool_refs(&self) -> Vec<PoolRef> }` — the `pools` list if present, else the single `pool`+`quote` pair as a one-element vec, else empty. Single-pool shorthand and list may NOT be combined; if both present, `pools` wins and a `warn!` is logged once at load.

- [ ] **Step 1: Failing tests** in `momentum_universe.rs`:

```rust
#[test]
fn pool_refs_single_shorthand_and_list() {
    let single: WatchedToken = serde_json::from_str(
        r#"{"symbol":"A","mint":"M1","pool":"P1","quote":"USDC"}"#).unwrap();
    assert_eq!(single.pool_refs().len(), 1);
    assert_eq!(single.pool_refs()[0].pool, "P1");
    let multi: WatchedToken = serde_json::from_str(
        r#"{"symbol":"B","mint":"M2","pools":[{"pool":"P2","quote":"USDC"},{"pool":"P3","quote":"SOL"}]}"#).unwrap();
    assert_eq!(multi.pool_refs().len(), 2);
    let none: WatchedToken = serde_json::from_str(r#"{"symbol":"C","mint":"M3"}"#).unwrap();
    assert!(none.pool_refs().is_empty());
}
```

Run `cargo test --lib momentum_universe` — fails (no `pools`/`pool_refs`). Implement, re-run, pass.

- [ ] **Step 2: Wire in `spawn_grpc_feed`.** Replace the `let (Some(pool_id), Some(quote)) = (w.pool.as_deref(), w.quote.as_deref())` destructure with a loop over `w.pool_refs()` — each ref producing its own `Pending`/`WiredPool` (same mint, own pool + quote_is_usdc). `acct_index` collisions (same account in two refs) keep first — log and skip duplicates.

- [ ] **Step 3: Cross-venue log.** In `apply_update`, before inserting the new price: if the existing map entry for the mint is `< 5 s` old and differs by more than 100 bps (hardcode 100 here — observability only, not the config xcheck), `info!("gRPC: venues disagree for {mint}: new=${new:.6} prev=${prev:.6}")`. Then insert (last write wins).

- [ ] **Step 4: Watcher label.** `src/portfolio/watcher.rs:536`: `w.pool.is_some() && w.quote.is_some()` → `!w.pool_refs().is_empty()`.

- [ ] **Step 5: Build + tests + commit.** `cargo build --release --bin portfolio-watcher && cargo test --lib` → `git commit -m "feat(grpc-pricing): multi-pool per token — pools[] schema, last-write-wins, cross-venue divergence log"`

---

### Task 4: Entry price-freshness guard (spec F)

**Files:**
- Modify: `src/portfolio/momentum.rs` (pure fn + two call sites: entry ~line 1657, rotation ~line 1871; tests)
- Modify: `src/portfolio/momentum_actions.rs` (new `ActionKind::SkipDivergence` variant — follow the existing variant pattern, serde-tagged like its neighbors)
- Modify: `src/portfolio/mod.rs` (config `momentum_entry_divergence_bps: u32`, env `MOMENTUM_ENTRY_DIVERGENCE_BPS`, default 0)
- Modify: `.env.example` + momentum doc (document; default 0 = off)

**Interfaces:**
- Produces in `momentum.rs`: `pub fn quote_divergence_bps(implied_price: f64, reference_price: f64) -> Option<u32>` — `None` if either input is not finite/positive; else `((implied - reference).abs() / reference * 10_000.0) as u32`.
- Guard applies at BOTH entry and rotation buy, AFTER the cost gate, gated on `cfg.momentum_entry_divergence_bps > 0` and a trusted gRPC price being available (`ctx.grpc_feed` map entry for the mint, not distrusted; in TTL mode also within stale window — reuse `select_prices`-equivalent logic via a small helper `fn trusted_grpc_price(feed, mint, cfg) -> Option<f64>`).

- [ ] **Step 1: Failing tests** (pure fn):

```rust
#[test]
fn quote_divergence_bps_math_and_guards() {
    assert_eq!(quote_divergence_bps(101.0, 100.0), Some(100));
    assert_eq!(quote_divergence_bps(99.0, 100.0), Some(100));
    assert_eq!(quote_divergence_bps(100.0, 100.0), Some(0));
    assert_eq!(quote_divergence_bps(0.0, 100.0), None);
    assert_eq!(quote_divergence_bps(100.0, f64::NAN), None);
}
```

- [ ] **Step 2: Implement** the pure fn + config + `ActionKind::SkipDivergence { symbol: String, implied: f64, grpc: f64, dev_bps: u32, budget_bps: u32 }`.

- [ ] **Step 3: Entry call site** (after the `expected_token <= 0.0` check, before submit): `implied = size / expected_token` (USD per token). If `cfg.momentum_entry_divergence_bps > 0`, `trusted_grpc_price` returns `Some(g)`, and `quote_divergence_bps(implied, g)` exceeds the budget → `audit(SkipDivergence)`, `warn!`, `continue 'candidates;`. Mirror at the rotation buy site with its `continue`/return equivalent (match the surrounding control flow — rotation uses a function return, check the actual structure at the site).

- [ ] **Step 4: Build + tests + commit.** `cargo test --lib momentum && cargo build --release --bin portfolio-watcher` → `git commit -m "feat(grpc-pricing): entry price-freshness guard — skip when Jupiter fill diverges from live gRPC price"`

---

### Task 5: Local impact pre-gate (spec E)

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs` (`GrpcFeed.impact: DashMap<String,(u32, Instant)>` + `est_impact_bps(&self, mint) -> Option<u32>`; test)
- Modify: `src/bin/portfolio_watcher.rs` (publish estimate in `apply_update` for CP + Whirlpool pools; needs `trade_usdc` — thread `cfg.momentum_trade_usdc` into the wired setup as a captured value; buy direction = quote→momentum, amount_in = `trade_usdc` in quote atomic units, ÷ `sol_usd` first when quote is SOL; use `dex::get_quote(&w.pool, amount_in, /*a_to_b=*/ !w.momentum_is_token_a).price_impact` → bps; skip DLMM/other kinds)
- Modify: `src/portfolio/momentum.rs` (pre-gate BEFORE `jupiter::quote` at entry: if `cfg.momentum_local_impact` and `ctx.grpc_feed`'s `est_impact_bps(mint)` is `Some(est)` and fresh (< 120 s) and `est > 2 * cfg.momentum_max_cost_bps` → audit `ActionKind::SkipLocalImpact { symbol, est_bps, budget_bps }`, skip candidate without the REST round-trip)
- Modify: `src/portfolio/momentum_actions.rs` (`SkipLocalImpact` variant), `src/portfolio/mod.rs` (config `momentum_local_impact: bool`, env `MOMENTUM_LOCAL_IMPACT`, default false), `.env.example` + doc

**Interfaces:**
- Produces: `GrpcFeed::publish_impact(&self, mint: &str, bps: u32)` and `est_impact_bps(&self, mint: &str, max_age: Duration) -> Option<u32>`.
- The 2× margin is a Global-Constraint-level requirement: the local model ignores routing, so ONLY obviously-doomed entries may be skipped locally. Pre-skip threshold: `est > 2 * cfg.momentum_max_cost_bps`. Jupiter remains authoritative for everything else.

- [ ] **Step 1: Failing test** for the feed accessors (insert → readable, stale age → None). Implement, pass.
- [ ] **Step 2: Publish in `apply_update`** (CP + Whirlpool only — match on `w.pool.dex`; compute after the price insert; any `None`/zero path publishes nothing).
- [ ] **Step 3: Pre-gate in `momentum.rs`** entry loop before the quote call, flag-gated.
- [ ] **Step 4: Build + tests + commit.** `git commit -m "feat(grpc-pricing): local impact pre-gate (MOMENTUM_LOCAL_IMPACT) — skip doomed entries without a REST quote"`

---

### Task 6: End-to-end validation + docs sweep

- [ ] **Step 1:** `cargo test --lib` (all) + `cargo test --bin solana-mev` (unchanged arb suite still green) + `cargo clippy --bin portfolio-watcher` (no new warnings in touched files).
- [ ] **Step 2:** `GRPC_PRICE_SMOKE=1 ./target/release/portfolio-watcher` — PASS with all 8 tokens priced from seed.
- [ ] **Step 3:** Verify every new env var appears in `.env.example` and the momentum doc; verify defaults reproduce old behavior by diffing a config loaded from an empty env (spot-check `from_env` defaults: stale=90 → TTL mode, xcheck vars present but inert, divergence=0, local_impact=false).
- [ ] **Step 4:** Commit any doc-only leftovers → `git commit -m "docs(grpc-pricing): env reference + validation notes"`.

## Paper rollout (operator, post-merge)

`.env`: set `MOMENTUM_GRPC_STALE_SECS=0` (trust-until-changed on), keep `MOMENTUM_ENTRY_DIVERGENCE_BPS=0` and `MOMENTUM_LOCAL_IMPACT=false` initially; restart watcher; expect `pricing gRPC=[all 8] REST(wired)=[]` steady-state with occasional xcheck lines. Enable the two entry guards after a clean day.
