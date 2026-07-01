# gRPC Event-Based Trailing Stop (wick-confirmed) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Opt-in, event-driven momentum exit — re-evaluate the trailing stop the moment a held token's gRPC price updates, using the fresh on-chain price, guarded by a dwell-time wick-confirmation; poll ticker retained as backstop.

**Architecture:** A pure `stop_decision` dwell predicate + arm-state map drives the exit; `maybe_exit` prefers the live `GrpcFeed` price for held mints (REST fallback) and applies dwell-confirm when the flag is on; the gRPC ingestion task fires a `Notify` on held-token updates and the watcher's `select!` runs `maybe_exit` on it, alongside the retained fast ticker. Default-off ⇒ byte-identical to today. Exit-only.

**Tech Stack:** Rust, tokio (`Notify`), `dashmap` (dwell + price maps), the shipped `grpc_pricer` feed.

**Calibration (done):** exit give-back past the stop measured from `momentum_actions.jsonl` ≈ 1–1.7%/stopped-exit (5-exit sample, historical trail unknown) — modest but real, not an abort. Dwell default `MOMENTUM_STOP_CONFIRM_SECS=3`.

## Global Constraints

- **Opt-in, default-off:** `MOMENTUM_GRPC_EXIT` default false (requires `MOMENTUM_GRPC_PRICING`). Off ⇒ exit path byte-identical to today. This is the paramount invariant — every task preserves it.
- **Exit-only:** no changes to the momentum ENTRY path (`maybe_enter`, ranking, regime).
- **Backstop:** the fast `MOMENTUM_POLL_SECS` ticker that calls `maybe_exit` is RETAINED; the event path is additive. Exits must never depend solely on the gRPC stream.
- **No double-sell / no arb changes:** single exit path (`maybe_exit`); `select!` serializes ticker vs event; do NOT modify `src/arbitrage/`, `src/graph/`, `src/streamer/`, `src/dex/`, `src/main.rs`.
- **COMMIT ONLY, never push. NEVER `cargo fmt`/`rustfmt`.** Lib tests: `cargo test --lib momentum` / `cargo test --lib grpc_pricer`.
- **`maybe_exit` stays un-gated on `halted()`** (a halted bot must still exit).
- Dwell state is in-memory (a restart re-arms fresh); do NOT persist it in `momentum_state.json`.

---

## File Structure

- `src/portfolio/mod.rs` — 2 config fields.
- `src/portfolio/momentum.rs` — `ExitDecision` enum + pure `stop_decision`; `MomentumContext` gains `grpc_feed`/`stop_armed`; `maybe_exit` price-source + dwell logic.
- `src/portfolio/grpc_pricer.rs` (lib) — `GrpcFeed` gains `notify` + `held` set + helpers.
- `src/bin/portfolio_watcher.rs` — ingestion task fires the notify; watcher owns the dwell map + held-set refresh + `select!` event arm + passes the new ctx fields.
- `.env.example` — document the two vars + paper-first guidance.

---

### Task 1: Config fields + pure `stop_decision` dwell predicate

**Files:**
- Modify: `src/portfolio/mod.rs` (PortfolioConfig + from_env)
- Modify: `src/portfolio/momentum.rs` (ExitDecision + stop_decision + tests)

**Interfaces:**
- Produces: `PortfolioConfig.momentum_grpc_exit: bool`, `.momentum_stop_confirm_secs: u64`;
  `pub enum ExitDecision { Sell, Arm, StayArmed, Disarm, Hold }`;
  `pub fn stop_decision(stop_hit: bool, armed_since: Option<std::time::Instant>, now: std::time::Instant, confirm_secs: u64) -> ExitDecision`

- [ ] **Step 1: Add config fields**

In `src/portfolio/mod.rs` `PortfolioConfig`, near the other `momentum_*` fields:
```rust
    pub momentum_grpc_exit: bool,          // MOMENTUM_GRPC_EXIT, default false (requires momentum_grpc_pricing)
    pub momentum_stop_confirm_secs: u64,   // MOMENTUM_STOP_CONFIRM_SECS, default 3
```
In `from_env()` (match the neighboring parse style):
```rust
    momentum_grpc_exit: std::env::var("MOMENTUM_GRPC_EXIT").map(|v| v == "true").unwrap_or(false),
    momentum_stop_confirm_secs: std::env::var("MOMENTUM_STOP_CONFIRM_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(3),
```

- [ ] **Step 2: Write the failing `stop_decision` test**

In `src/portfolio/momentum.rs` `#[cfg(test)]`:
```rust
#[test]
fn stop_decision_dwell_lifecycle() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    // first breach → Arm, do not sell
    assert!(matches!(stop_decision(true, None, t0, 3), ExitDecision::Arm));
    // still breached, dwell not elapsed → StayArmed
    assert!(matches!(stop_decision(true, Some(t0), t0 + Duration::from_secs(1), 3), ExitDecision::StayArmed));
    // still breached, dwell elapsed → Sell
    assert!(matches!(stop_decision(true, Some(t0), t0 + Duration::from_secs(3), 3), ExitDecision::Sell));
    // recovered while armed → Disarm
    assert!(matches!(stop_decision(false, Some(t0), t0 + Duration::from_secs(1), 3), ExitDecision::Disarm));
    // not breached, not armed → Hold
    assert!(matches!(stop_decision(false, None, t0, 3), ExitDecision::Hold));
    // confirm_secs=0 → immediate Sell on first breach (dwell disabled)
    assert!(matches!(stop_decision(true, None, t0, 0), ExitDecision::Sell));
}
```

- [ ] **Step 3: Run — expect FAIL** (`stop_decision` not defined): `cargo test --lib momentum::tests::stop_decision_dwell_lifecycle`

- [ ] **Step 4: Implement**

```rust
/// Outcome of one wick-confirmed stop evaluation for a held position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitDecision { Sell, Arm, StayArmed, Disarm, Hold }

/// Dwell-based wick-confirmation: a stop must stay breached for `confirm_secs`
/// before selling, so a single-block on-chain price wick that reverts doesn't
/// whipsaw the position out. `confirm_secs == 0` ⇒ sell immediately on breach
/// (dwell disabled — today's behavior). `armed_since` is when the breach began.
pub fn stop_decision(
    stop_hit: bool,
    armed_since: Option<std::time::Instant>,
    now: std::time::Instant,
    confirm_secs: u64,
) -> ExitDecision {
    match (stop_hit, armed_since) {
        (true, None) => {
            if confirm_secs == 0 { ExitDecision::Sell } else { ExitDecision::Arm }
        }
        (true, Some(since)) => {
            if now.duration_since(since).as_secs() >= confirm_secs {
                ExitDecision::Sell
            } else {
                ExitDecision::StayArmed
            }
        }
        (false, Some(_)) => ExitDecision::Disarm,
        (false, None) => ExitDecision::Hold,
    }
}
```

- [ ] **Step 5: Run — expect PASS**, and build the lib. `cargo test --lib momentum::tests::stop_decision_dwell_lifecycle` then `cargo build --lib`.

- [ ] **Step 6: Commit**
```bash
git add src/portfolio/mod.rs src/portfolio/momentum.rs
git commit -m "feat(grpc-exit): config flags + pure stop_decision dwell predicate"
```

---

### Task 2: `GrpcFeed` notify + held-set (event plumbing)

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs` (GrpcFeed struct + helpers)
- Modify: `src/bin/portfolio_watcher.rs` (ingestion task fires notify on held-mint updates)

**Interfaces:**
- Consumes: existing `GrpcFeed { map, sol_usd }`.
- Produces: `GrpcFeed.notify: Arc<tokio::sync::Notify>`, `GrpcFeed.held: Arc<std::sync::RwLock<std::collections::HashSet<String>>>`, and helpers `pub fn set_held(&self, mints: impl IntoIterator<Item=String>)` and `pub fn note_update(&self, mint: &str)` (notifies iff `mint` is in `held`).

- [ ] **Step 1: Extend `GrpcFeed`**

In `src/portfolio/grpc_pricer.rs`, add fields + init + helpers (keep existing `map`/`sol_usd`):
```rust
use std::collections::HashSet;
use std::sync::RwLock;
use tokio::sync::Notify;

// in struct GrpcFeed { ... }:
    pub notify: Arc<Notify>,
    pub held: Arc<RwLock<HashSet<String>>>,

// in impl GrpcFeed::new():
    notify: Arc::new(Notify::new()),
    held: Arc::new(RwLock::new(HashSet::new())),

// helpers:
impl GrpcFeed {
    /// Replace the held-mint set the ingestion task uses to decide when to wake the exit path.
    pub fn set_held(&self, mints: impl IntoIterator<Item = String>) {
        if let Ok(mut h) = self.held.write() { *h = mints.into_iter().collect(); }
    }
    /// Called by the ingestion task after storing a price: wake the exit path iff the
    /// updated mint is currently held (cheap read-lock; no-op otherwise).
    pub fn note_update(&self, mint: &str) {
        if self.held.read().map(|h| h.contains(mint)).unwrap_or(false) {
            self.notify.notify_one();
        }
    }
}
```
(`GrpcFeed` already derives `Clone`; `Arc` fields clone by handle — the notify/held are shared across clones. Verify the derive is present; keep it.)

- [ ] **Step 2: Fire the notify from the ingestion task**

In `src/bin/portfolio_watcher.rs` `run_grpc_stream`, right after the successful `feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));`, add:
```rust
            feed.note_update(&w.token_mint);
```

- [ ] **Step 3: Build**

Run: `cargo build --lib` and `cargo build --bin portfolio-watcher`. Expected: compiles (no behavior change yet — `held` is empty until Task 4 populates it, so `note_update` is a no-op).

- [ ] **Step 4: Commit**
```bash
git add src/portfolio/grpc_pricer.rs src/bin/portfolio_watcher.rs
git commit -m "feat(grpc-exit): GrpcFeed notify + held-set; ingestion wakes exit on held-mint updates"
```

---

### Task 3: `maybe_exit` — fresh-gRPC price source + flag-gated dwell (the exit-logic change)

**Files:**
- Modify: `src/portfolio/momentum.rs` (MomentumContext + maybe_exit)

**Interfaces:**
- Consumes: `stop_decision`/`ExitDecision` (Task 1), `GrpcFeed` (Task 2), existing `trailing_stop_hit`/`trailing_stop_triggered`.
- Produces: `MomentumContext` gains `pub grpc_feed: Option<&'a crate::portfolio::grpc_pricer::GrpcFeed>` and `pub stop_armed: Option<&'a dashmap::DashMap<String, std::time::Instant>>`; `maybe_exit` behavior unchanged when `cfg.momentum_grpc_exit == false`.

- [ ] **Step 1: Add the two `MomentumContext` fields**

```rust
pub struct MomentumContext<'a> {
    pub cfg: &'a PortfolioConfig,
    pub watched: &'a [WatchedToken],
    pub prices_usd: &'a HashMap<String, f64>,
    pub history: &'a VecDeque<PriceSnapshot>,
    pub decimals: &'a HashMap<String, u8>,
    pub http: &'a Client,
    pub usdc_balance: f64,
    /// Live on-chain price feed for event-driven exits (Some only when MOMENTUM_GRPC_EXIT).
    pub grpc_feed: Option<&'a crate::portfolio::grpc_pricer::GrpcFeed>,
    /// In-memory wick-confirm arm state: mint -> when the stop breach began.
    pub stop_armed: Option<&'a dashmap::DashMap<String, std::time::Instant>>,
}
```
> This breaks existing `MomentumContext { ... }` literals (watcher builds two). Task 4 fixes the watcher; for THIS task, update any `#[cfg(test)]` `MomentumContext` literal in `momentum.rs` to add `grpc_feed: None, stop_armed: None` so lib tests compile.

- [ ] **Step 2: Prefer the fresh gRPC price for held mints (flag-gated)**

In `maybe_exit`, replace the unconditional REST fetch with a gRPC-preferred source when the flag is on. After computing `held_mints`:
```rust
    // Price source: when MOMENTUM_GRPC_EXIT, prefer the live on-chain price for held
    // mints (fresh within the feed's stale window), REST-fetch only the rest. Flag off ⇒
    // REST for all (today's path).
    let mut prices_map: HashMap<String, f64> = HashMap::new();
    let mut rest_mints: Vec<String> = Vec::new();
    if cfg.momentum_grpc_exit {
        if let Some(feed) = ctx.grpc_feed {
            let stale = std::time::Duration::from_secs(cfg.momentum_grpc_stale_secs);
            let now = std::time::Instant::now();
            for m in &held_mints {
                match feed.map.get(m) {
                    Some(e) if now.duration_since(e.value().1) <= stale && e.value().0 > 0.0 => {
                        prices_map.insert(m.clone(), e.value().0);
                    }
                    _ => rest_mints.push(m.clone()),
                }
            }
        } else {
            rest_mints = held_mints.clone();
        }
    } else {
        rest_mints = held_mints.clone();
    }
    if !rest_mints.is_empty() {
        let rest = pricer::fetch_prices(ctx.http, &rest_mints, cfg.birdeye_api_key.as_deref())
            .await.unwrap_or_default();
        prices_map.extend(rest);
    }
```
(The rest of `maybe_exit` continues to read `prices_map` per position as today.)

- [ ] **Step 3: Apply dwell-confirm to the sell decision (flag-gated)**

Where `maybe_exit` currently decides to sell a position on a tripped stop (the `trailing_stop_hit(...)`/`trailing_stop_triggered(...)` branch that pushes to `to_exit`), route it through `stop_decision` when the flag+arm-map are present:
```rust
            let stop_hit = /* existing trailing_stop_hit(...) / max_trail / fade computation → bool */;
            let armed_map = ctx.stop_armed.filter(|_| cfg.momentum_grpc_exit);
            let sell = match armed_map {
                Some(armed) => {
                    let now = std::time::Instant::now();
                    let armed_since = armed.get(&pos.mint).map(|e| *e.value());
                    match stop_decision(stop_hit, armed_since, now, cfg.momentum_stop_confirm_secs) {
                        ExitDecision::Arm | ExitDecision::StayArmed => { armed.entry(pos.mint.clone()).or_insert(now); false }
                        ExitDecision::Disarm => { armed.remove(&pos.mint); false }
                        ExitDecision::Sell => { armed.remove(&pos.mint); true }
                        ExitDecision::Hold => false,
                    }
                }
                None => stop_hit, // flag off ⇒ immediate, today's behavior
            };
            if sell { to_exit.push((idx, reason)); }
```
> Confirm the exact existing sell-condition expression and `reason` binding in `maybe_exit`; wrap that boolean as `stop_hit` and gate the push through `sell`. The fade-exit and peak-update logic stay as they are (peak updates every eval regardless of arm state).

- [ ] **Step 4: Build + test**

Run: `cargo build --lib` (fix the test-literal `MomentumContext` per Step 1 note) and `cargo test --lib momentum`. Expected: pass. Behavior with `momentum_grpc_exit=false` is unchanged (`sell = stop_hit`, REST for all).

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/momentum.rs
git commit -m "feat(grpc-exit): maybe_exit prefers fresh gRPC price + flag-gated dwell-confirm (off = identical)"
```

---

### Task 4: Watcher wiring — dwell map, held-set refresh, event arm

**Files:**
- Modify: `src/bin/portfolio_watcher.rs`

**Interfaces:**
- Consumes: `GrpcFeed.{notify,set_held}` (Task 2), `MomentumContext.{grpc_feed,stop_armed}` (Task 3).

- [ ] **Step 1: Own the dwell map + refresh the held set**

Near the top of `run` (after `grpc_feed` is available), add:
```rust
    let stop_armed: std::sync::Arc<dashmap::DashMap<String, std::time::Instant>> =
        std::sync::Arc::new(dashmap::DashMap::new());
```
Wherever the momentum state's positions are known each slow tick (where `held_mints_from_state`/positions are handled), refresh the feed's held set so the ingestion task knows which updates should wake the exit:
```rust
    if let Some(feed) = &grpc_feed {
        feed.set_held(momentum::held_mints_from_state(&cfg).into_iter().map(|w| w.mint));
    }
```
> `held_mints_from_state` returns `Vec<WatchedToken>`; confirm it's accessible (it's used in `run` already). If it's a private watcher fn, call the existing local instead. Refresh on the slow `ticker` arm (once per monitor tick is enough — positions change slowly).

- [ ] **Step 2: Pass the new ctx fields at BOTH exit sites**

In the fast-ticker `MomentumContext { ... }` literal, add:
```rust
                            grpc_feed: grpc_feed.as_ref(),
                            stop_armed: Some(&stop_armed),
```
(Any other `MomentumContext` construction — e.g. in `maybe_enter`'s ctx — passes `grpc_feed: None, stop_armed: None` since this is exit-only; add those two fields to every literal so it compiles.)

- [ ] **Step 3: Add the event-driven exit arm to `select!`**

Add an arm (only meaningful when the flag is on and a feed exists):
```rust
            _ = async { match &grpc_feed { Some(f) => f.notify.notified().await, None => std::future::pending().await } },
                if cfg.momentum_grpc_exit && grpc_feed.is_some() => {
                // Event-driven EXIT re-eval on a held-token on-chain price update.
                if cfg.enable_momentum_trader {
                    let outcomes = {
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &effective, prices_usd: &last_prices,
                            history: &history, decimals: &decimals, http: &http,
                            usdc_balance: usdc_balance(&portfolio),
                            grpc_feed: grpc_feed.as_ref(), stop_armed: Some(&stop_armed),
                        };
                        momentum::maybe_exit(&mctx).await
                    };
                    // apply outcomes EXACTLY as the fast-ticker arm does (factor the shared
                    // apply-block into a local closure/fn to avoid divergence).
                }
            }
```
> The fast-ticker arm's outcome-application logic must be reused verbatim (extract it to a local `async fn`/closure so both arms apply fills identically). The `select!` guarantees the ticker and event arms never run concurrently → no state race.

- [ ] **Step 4: Build + flag-off equivalence check**

Run: `cargo build --bin portfolio-watcher` → compiles.
Run: `cargo test --lib` → all pass.
Confirm: with `MOMENTUM_GRPC_EXIT` unset/false, `stop_armed` is passed but `maybe_exit` ignores it (Task 3 gates on the flag), the notify arm's guard is false so it never fires, and `set_held`/`note_update` are harmless → behavior identical to today.

- [ ] **Step 5: Commit**
```bash
git add src/bin/portfolio_watcher.rs
git commit -m "feat(grpc-exit): watcher event arm + dwell map + held-set refresh (backstop ticker retained)"
```

---

### Task 5: Docs + paper smoke

**Files:**
- Modify: `.env.example`

- [ ] **Step 1: Document the vars**

Add under the momentum gRPC section of `.env.example`:
```
# Opt-in: event-driven trailing-stop EXIT off the gRPC feed (requires MOMENTUM_GRPC_PRICING).
# Re-evaluates the stop the moment a held token's on-chain price updates, with a dwell-time
# wick-confirmation. Off (default) = REST fast-tick exit, unchanged. Exit-only. The 1s poll
# ticker is retained as a backstop. Paper-test first (DRY_RUN_MOMENTUM_TRADER=true).
# MOMENTUM_GRPC_EXIT=false
# MOMENTUM_STOP_CONFIRM_SECS=3     # price must stay below the stop this long before selling
```

- [ ] **Step 2: Build release**

Run: `cargo build --release --bin portfolio-watcher` → compiles.

- [ ] **Step 3: Paper smoke (operator step — record the result)**

With a wired held position (e.g. adopt SLX in DRY_RUN), run:
`DRY_RUN_MOMENTUM_TRADER=true MOMENTUM_GRPC_PRICING=true MOMENTUM_GRPC_EXIT=true MOMENTUM_STOP_CONFIRM_SECS=3 RUST_LOG=info ./target/release/portfolio-watcher`
Expected in logs: event-driven exit evaluations fire on SLX on-chain updates (not just each 1s tick); on a simulated breach the position ARMS, and only sells after ~3s of sustained breach (or DISARMS if price recovers). NO live sells (dry-run). Capture representative log lines. (Cannot run in CI — needs the live endpoint + a held position.)

- [ ] **Step 4: Commit**
```bash
git add .env.example
git commit -m "docs(grpc-exit): document MOMENTUM_GRPC_EXIT + STOP_CONFIRM_SECS + paper-first"
```

---

## Self-Review

**Spec coverage:** config flags → T1; gRPC price into exit → T3 Step 2; dwell wick-confirm (pure `stop_decision` + in-memory arm map) → T1 + T3 Step 3 + T4 Step 1; event Notify wiring (held-set + ingestion fire + select! arm) → T2 + T4; backstop ticker retained → T4 (fast-ticker arm untouched); default-off equivalence → T3 (flag gate) + T4 Step 4; docs + paper smoke → T5. ✓

**Placeholder scan:** T3 Step 3 references "the existing sell-condition expression / `reason` binding" — flagged as a confirm-in-place because that exact expression lives in unchanged `maybe_exit` code the implementer must wrap; the wrapping code is given in full. No vague directives.

**Type consistency:** `ExitDecision`/`stop_decision` (T1) used in T3; `GrpcFeed.{notify,held,set_held,note_update}` (T2) used in T4; `MomentumContext.{grpc_feed,stop_armed}` (T3) built in T4; `stop_armed: Arc<DashMap<String, Instant>>` consistent T3↔T4.

## Known implementer confirmations (verify in-repo, not placeholders)
- The exact sell-condition expression + `reason` binding + peak-update block in `maybe_exit` (T3 Step 3) — `src/portfolio/momentum.rs`.
- `held_mints_from_state` visibility + where positions are refreshed each tick (T4 Step 1) — `src/portfolio/watcher.rs`.
- The fast-ticker arm's outcome-application block to factor + reuse in the event arm (T4 Step 3) — `src/bin/portfolio_watcher.rs`.
- `cfg.momentum_grpc_stale_secs` field name (T3 Step 2) — exists from the gRPC-pricing feature.
- `GrpcFeed` `#[derive(Clone)]` present so `notify`/`held` share across clones (T2).
