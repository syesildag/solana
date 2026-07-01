# Momentum gRPC CL-state Pricing — Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Extend the opt-in gRPC price feed to concentrated-liquidity pools (Orca Whirlpool, Raydium CLMM, Meteora DLMM, Invariant) priced from their state-account sqrt-price, and wire JTO + SLX (Orca) as the first real watch-list tokens.

**Architecture:** Reuse the arb's real `dex::Pool` + public parsers (Approach A). The watcher builds an `Arc<dex::Pool>` per wired momentum pool, subscribes the right accounts per DEX kind (vaults for constant-product, state account for CL), drives the pool's atomics with the same `parse_*` fns the arb uses, and derives USD via a shared rate→USD core. No changes to the arb path.

**Tech Stack:** Rust, tokio, tonic/yellowstone-grpc-proto, the in-repo `dex` module (included in the watcher binary via `#[path]`).

## Global Constraints

- **Opt-in, default-off:** `MOMENTUM_GRPC_PRICING` default false → REST-only, byte-identical. Unchanged.
- **Additive, no arb changes:** modify only `src/portfolio/grpc_pricer.rs`, `src/bin/portfolio_watcher.rs`, `.env.example`, `scripts/fetch_orca_pools.js`, and (generated) `pools.json`/`momentum_tokens.json`. Do NOT modify `src/arbitrage/`, `src/graph/`, `src/streamer/`, `src/dex/`, `src/main.rs`, or the arb `Config`/`PoolRegistry`.
- **Never hand-edit `pools.json`** — regenerate via `scripts/fetch_orca_pools.js` + `scripts/merge_pools.js` (it's generated; see the memory note).
- **COMMIT ONLY, never push.** NEVER `cargo fmt`/`rustfmt`.
- **Lib tests:** `cargo test --lib grpc_pricer`. Binary build: `cargo build --bin portfolio-watcher`.
- **CL price semantics (verbatim from the arb):** `parse_cl_pool_state(data, &pool) -> Option<(f64, u64)>` returns `(price, fee_bps)` where `price` = **token_b per token_a in raw/atomic units** (`(sqrt_price_x64 / 2^64)^2`). Store it into `pool.sqrt_price_x64` as `price.to_bits()` and `fee_bps` (if > 0) into `pool.fee_bps`, exactly as `src/main.rs:461-465`.
- **Decimal/USD math:** `human_rate = raw_rate * 10^(dec_momentum - dec_quote)`; `usd = human_rate` (quote USDC) or `human_rate * sol_usd` (quote SOL).
- **Live smoke needs the real endpoint** (`GRPC_ENDPOINT` in `.env`); it's an operator/`GRPC_PRICE_SMOKE=1` step, not CI.

---

## File Structure

- **`src/portfolio/grpc_pricer.rs`** (lib): add pure `rate_to_usd`; `price_usd` delegates to it. Stays `dex`-free.
- **`src/bin/portfolio_watcher.rs`** (binary): `spawn_grpc_feed`/`run_grpc_stream` rebuilt around `Arc<dex::types::Pool>`, routing CP-vaults vs CL-state by `DexKind`.
- **`scripts/fetch_orca_pools.js`**: add JTO/SLX mints to its target set.
- **`.env.example`**: note the newly supported DEX kinds.

---

### Task 1: Shared `rate_to_usd` core (pure, lib)

**Files:**
- Modify: `src/portfolio/grpc_pricer.rs`
- Test: same file `#[cfg(test)]`

**Interfaces:**
- Produces: `pub fn rate_to_usd(raw_rate: f64, dec_momentum: u8, dec_quote: u8, quote_is_usdc: bool, sol_usd: f64) -> Option<f64>` — `raw_rate` is atomic quote-per-momentum. `price_usd` refactored to call it.

- [ ] **Step 1: Write the failing test**

```rust
    #[test]
    fn rate_to_usd_cl_style_both_orientations() {
        // raw a->b rate 2.0 (atomic b per atomic a), equal dp, USDC → $2.0
        assert!((rate_to_usd(2.0, 6, 6, true, 0.0).unwrap() - 2.0).abs() < 1e-9);
        // momentum=token_b uses 1/price at the call site; here just the inverse rate 0.5 → $0.5
        assert!((rate_to_usd(0.5, 6, 6, true, 0.0).unwrap() - 0.5).abs() < 1e-9);
        // SOL quote: 2.0 * sol_usd(150) = 300
        assert!((rate_to_usd(2.0, 9, 9, false, 150.0).unwrap() - 300.0).abs() < 1e-6);
        // decimal scale 10^(9-6)=1000
        assert!((rate_to_usd(2.0, 9, 6, true, 0.0).unwrap() - 2000.0).abs() < 1e-6);
        // degenerate
        assert!(rate_to_usd(0.0, 6, 6, true, 0.0).is_none());
        assert!(rate_to_usd(f64::INFINITY, 6, 6, true, 0.0).is_none());
    }
```

- [ ] **Step 2: Run — expect FAIL** (`rate_to_usd` not defined): `cargo test --lib grpc_pricer::tests::rate_to_usd_cl_style_both_orientations`

- [ ] **Step 3: Implement + refactor `price_usd` to delegate**

Add:
```rust
/// Convert an atomic quote-per-momentum rate to a USD price. Shared by the CP path
/// (rate from PoolState) and the CL path (rate from parse_cl_pool_state's price).
pub fn rate_to_usd(
    raw_rate: f64,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    sol_usd: f64,
) -> Option<f64> {
    if !raw_rate.is_finite() || raw_rate <= 0.0 {
        return None;
    }
    let price_in_quote = raw_rate * 10f64.powi(dec_momentum as i32 - dec_quote as i32);
    let usd = if quote_is_usdc { price_in_quote } else { price_in_quote * sol_usd };
    if usd.is_finite() && usd > 0.0 { Some(usd) } else { None }
}
```
Change `price_usd`'s body to delegate (keep its existing signature and the `PoolRates` trait):
```rust
pub fn price_usd(
    state: &dyn PoolRates,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    sol_usd: f64,
) -> Option<f64> {
    let raw = if momentum_is_token_a { state.rate_a_to_b() } else { state.rate_b_to_a() };
    rate_to_usd(raw, dec_momentum, dec_quote, quote_is_usdc, sol_usd)
}
```

- [ ] **Step 4: Run — expect PASS** (new test + all existing `price_usd`/`select_prices` tests):
`cargo test --lib grpc_pricer` → all pass.

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/grpc_pricer.rs
git commit -m "feat(grpc-pricer): factor shared rate_to_usd core out of price_usd"
```

---

### Task 2: Migrate the CP path onto `dex::Pool` (binary; behavior-preserving)

**Files:**
- Modify: `src/bin/portfolio_watcher.rs`

**Interfaces:**
- Consumes: `dex::types::{Pool, PoolConfig, DexKind}`, `Arc::<Pool>::try_from(PoolConfig)`, `Pool::snapshot_state`, `PoolState::rate_a_to_b/rate_b_to_a`, `grpc_pricer::rate_to_usd`, `dex::parse_spl_token_amount`.
- Produces: `spawn_grpc_feed`/`run_grpc_stream` rebuilt around `Arc<Pool>` + an `AccountRole` index. Same public behavior for raydium_amm_v4 as today.

- [ ] **Step 1: Replace `TrackedCp` with a `dex::Pool`-based tracker**

Delete the `TrackedCp` struct + its `impl`. Add:
```rust
#[derive(Clone, Copy)]
enum Role { VaultA, VaultB, State }

struct WiredPool {
    pool: std::sync::Arc<dex::types::Pool>,
    token_mint: String,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
}

impl WiredPool {
    /// Current USD price of the momentum token, or None if the pool state isn't ready.
    fn price_usd(&self, sol_usd: f64) -> Option<f64> {
        // Phase 1: CP pools only (raydium_amm_v4/saber). CL handled in Task 3.
        let st = self.pool.snapshot_state();
        let raw = if self.momentum_is_token_a { st.rate_a_to_b() } else { st.rate_b_to_a() };
        grpc_pricer::rate_to_usd(raw, self.dec_momentum, self.dec_quote, self.quote_is_usdc, sol_usd)
    }
}
```

- [ ] **Step 2: Rebuild `spawn_grpc_feed` to build `Arc<Pool>` + a role index**

Replace the body's pool-resolution + tracking with (keep the flag/endpoint guard, decimals fetch, and return-None-on-empty exactly as they are):
```rust
    // Resolve each wired token's PoolConfig from pools.json → Arc<Pool>; index accounts by role.
    let mut wired: Vec<WiredPool> = Vec::new();
    let mut acct_index: HashMap<String, (usize, Role)> = HashMap::new();
    for p in &pending {                       // `pending` = (WatchedToken, &PoolConfig, quote_is_usdc), as today
        let momentum_is_token_a = p.tok.mint == p.pc.token_a;
        let (dm, dq) = if momentum_is_token_a {
            (decimals.get(&p.pc.token_a).copied(), decimals.get(&p.pc.token_b).copied())
        } else {
            (decimals.get(&p.pc.token_b).copied(), decimals.get(&p.pc.token_a).copied())
        };
        let (Some(dec_momentum), Some(dec_quote)) = (dm, dq) else {
            warn!("gRPC: decimals missing for pool {} — REST", p.pc.id); continue;
        };
        let pool: std::sync::Arc<dex::types::Pool> = match std::sync::Arc::try_from(p.pc.clone()) {
            Ok(pool) => pool,
            Err(e) => { warn!("gRPC: Pool::try_from failed for {} ({e}) — REST", p.pc.id); continue; }
        };
        let idx = wired.len();
        match pool.dex {
            dex::types::DexKind::RaydiumAmmV4 | dex::types::DexKind::Saber => {
                acct_index.insert(pool.vault_a.to_string(), (idx, Role::VaultA));
                acct_index.insert(pool.vault_b.to_string(), (idx, Role::VaultB));
            }
            // CL kinds are wired in Task 3; skip here so Task 2 stays CP-only + behavior-preserving.
            other => {
                warn!("gRPC: pool {} is {:?} (not yet supported in this build) — REST", p.pc.id, other);
                continue;
            }
        }
        wired.push(WiredPool { pool, token_mint: p.tok.mint.clone(), momentum_is_token_a, dec_momentum, dec_quote, quote_is_usdc: p.quote_is_usdc });
    }
    if wired.is_empty() { warn!("gRPC: no eligible pools — REST only"); return Ok(None); }
    let accounts: Vec<String> = acct_index.keys().cloned().collect();
    info!("gRPC price feed: subscribing {} accounts for {} pool(s)", accounts.len(), wired.len());
```
Update the spawned task + `run_grpc_stream` signature to take `&mut [WiredPool]` and `&HashMap<String,(usize,Role)>` instead of the old `TrackedCp` slice.

- [ ] **Step 3: Update `run_grpc_stream`'s per-update handler (CP arm)**

Replace the update body with:
```rust
        let Some(&(idx, role)) = acct_index.get(&pk.to_string()) else { continue };
        let w = &mut wired[idx];
        match role {
            Role::VaultA | Role::VaultB => {
                let Some(amt) = dex::parse_spl_token_amount(&info.data) else { continue };
                if matches!(role, Role::VaultA) {
                    w.pool.reserve_a.store(amt, std::sync::atomic::Ordering::Relaxed);
                } else {
                    w.pool.reserve_b.store(amt, std::sync::atomic::Ordering::Relaxed);
                }
            }
            Role::State => { /* handled in Task 3 */ }
        }
        if let Some(usd) = w.price_usd(feed.sol_usd()) {
            feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));
        }
```
(`Ordering` is used; add `use std::sync::atomic::Ordering;` at the top if not present, or keep the fully-qualified form as above.)

- [ ] **Step 4: Build + verify the CP path still works (SOL/USDC smoke)**

Run: `cargo build --bin portfolio-watcher` → compiles.
Run (release, live endpoint): `cargo build --release --bin portfolio-watcher` then `GRPC_PRICE_SMOKE=1 ./target/release/portfolio-watcher`.
Expected: with nothing wired, the smoke falls back to the SOL/USDC raydium pool (`58oQChx4…`) and prints `SOL = $<spot>` (≈ current SOL price) → PASS. This proves the `dex::Pool` migration preserved CP behavior.

- [ ] **Step 5: Commit**
```bash
git add src/bin/portfolio_watcher.rs
git commit -m "feat(grpc-pricer): migrate CP price path onto dex::Pool + role index (behavior-preserving)"
```

---

### Task 3: CL-state pricing (Orca / Raydium CLMM / Meteora DLMM / Invariant)

**Files:**
- Modify: `src/bin/portfolio_watcher.rs`

**Interfaces:**
- Consumes: `dex::parse_cl_pool_state(&[u8], &Pool) -> Option<(f64, u64)>`, `Pool.sqrt_price_x64`/`fee_bps` (AtomicU64), `Pool.state_account: Option<Pubkey>`, `grpc_pricer::rate_to_usd`.

- [ ] **Step 1: Route CL kinds to the state account in `spawn_grpc_feed`**

In the `match pool.dex` from Task 2, add the CL arm before the catch-all:
```rust
            dex::types::DexKind::OrcaWhirlpool
            | dex::types::DexKind::RaydiumClmm
            | dex::types::DexKind::MeteoraDlmm
            | dex::types::DexKind::Invariant => {
                let Some(state) = pool.state_account else {
                    warn!("gRPC: {:?} pool {} has no state_account — REST", pool.dex, p.pc.id); continue;
                };
                acct_index.insert(state.to_string(), (idx, Role::State));
            }
```

- [ ] **Step 2: Implement the CL price in `WiredPool` + the `Role::State` update arm**

Add a CL-aware price to `WiredPool` (replace the Phase-1 `price_usd` body):
```rust
    fn price_usd(&self, sol_usd: f64) -> Option<f64> {
        use dex::types::DexKind::*;
        let raw = match self.pool.dex {
            // CL: sqrt_price_x64 holds parse_cl_pool_state's `price` (token_b per token_a, raw units).
            OrcaWhirlpool | RaydiumClmm | MeteoraDlmm | Invariant => {
                let price = f64::from_bits(self.pool.sqrt_price_x64.load(std::sync::atomic::Ordering::Relaxed));
                if !(price > 0.0) { return None; }        // not initialised yet
                if self.momentum_is_token_a { price } else { 1.0 / price }
            }
            // CP: reserve-based rate from snapshot_state.
            _ => {
                let st = self.pool.snapshot_state();
                if self.momentum_is_token_a { st.rate_a_to_b() } else { st.rate_b_to_a() }
            }
        };
        grpc_pricer::rate_to_usd(raw, self.dec_momentum, self.dec_quote, self.quote_is_usdc, sol_usd)
    }
```
Fill in the `Role::State` arm in `run_grpc_stream` (mirrors `main.rs:461-465`):
```rust
            Role::State => {
                if let Some((price, fee_bps)) = dex::parse_cl_pool_state(&info.data, &w.pool) {
                    w.pool.sqrt_price_x64.store(price.to_bits(), std::sync::atomic::Ordering::Relaxed);
                    if fee_bps > 0 {
                        w.pool.fee_bps.store(fee_bps, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            }
```

- [ ] **Step 3: Build**

Run: `cargo build --bin portfolio-watcher` → compiles.

- [ ] **Step 4: Live smoke against an existing Orca pool (units/orientation safety net)**

Temporarily point the smoke at the existing Orca SOL/USDC whirlpool to verify CL pricing. In `run_grpc_smoke`, the fallback `WatchedToken` currently uses `SMOKE_SOL_USDC_POOL` (a raydium pool). Add an env override so no code edit is needed to test different pools: if `GRPC_SMOKE_POOL` is set, use it as the fallback pool id and `GRPC_SMOKE_QUOTE` (default USDC) as the quote, with mint = SOL_MINT.
```rust
    // inside run_grpc_smoke, replacing the hardcoded fallback pool:
    let smoke_pool = std::env::var("GRPC_SMOKE_POOL").unwrap_or_else(|_| SMOKE_SOL_USDC_POOL.to_string());
    let smoke_quote = std::env::var("GRPC_SMOKE_QUOTE").unwrap_or_else(|_| "USDC".to_string());
    // ... WatchedToken { mint: SOL_MINT, pool: Some(smoke_pool), quote: Some(smoke_quote), .. }
```
Run: `cargo build --release --bin portfolio-watcher` then
`GRPC_PRICE_SMOKE=1 GRPC_SMOKE_POOL=Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE ./target/release/portfolio-watcher`
Expected: prints `SOL = $<spot>` from the **Orca** whirlpool, matching the current SOL price (and the earlier raydium smoke) within a few percent. This confirms CL sqrt-price parsing + orientation + decimals are correct. Record the printed price in your report.

- [ ] **Step 5: Commit**
```bash
git add src/bin/portfolio_watcher.rs
git commit -m "feat(grpc-pricer): CL-state pricing for Orca/CLMM/DLMM/Invariant (verified vs Orca SOL/USDC)"
```

---

### Task 4: Wire JTO + SLX (add their Orca pools to pools.json) + docs

**Files:**
- Modify: `scripts/fetch_orca_pools.js` (add JTO/SLX to its `MINTS` target set)
- Regenerate: `pools.json` (via the script + `merge_pools.js`) — do NOT hand-edit
- Modify: `assets/momentum_tokens.json` (via `add_momentum_token.js --pool/--quote`)
- Modify: `.env.example`

**Interfaces:** consumes the `add_momentum_token.js --pool <id> --quote <USDC|SOL>` flag (already shipped).

- [ ] **Step 1: Add JTO + SLX mints to the Orca fetcher's target set**

In `scripts/fetch_orca_pools.js`, add to the `MINTS` map:
```js
  JTO: "jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL",
  SLX: "SLXdx4BUt2v9uJQNzWqSfzTJ9UKLUDsvxHFMEEdrfgq",
```
(Match the file's existing entry style; keep the SOL/USDC quote pairs the script already targets.)

- [ ] **Step 2: Regenerate + merge pools**

Run: `node scripts/fetch_orca_pools.js` (writes its per-DEX file), then `node scripts/merge_pools.js`.
Expected: `pools.json` now contains Orca whirlpool entries for JTO and SLX (verify:
`node -e 'const p=require("./pools.json");for(const s of ["jtojto","SLXdx"])console.log(s, p.filter(x=>x.dex==="orca_whirlpool"&&(x.token_a.startsWith(s)||x.token_b.startsWith(s))).map(x=>x.id))'`).
Pick each token's **deepest** Orca pool (the fetcher records liquidity; choose the highest). If neither a JTO nor SLX Orca pool is found, STOP and report — do not fabricate a pool id.

- [ ] **Step 3: Wire JTO + SLX with their pool ids**

Run (substituting the deepest pool ids found in Step 2, and the correct quote — USDC if a USDC pool exists, else SOL):
```bash
node scripts/add_momentum_token.js JTO --pool <JTO_ORCA_POOL_ID> --quote <USDC|SOL>
node scripts/add_momentum_token.js SLX --pool <SLX_ORCA_POOL_ID> --quote <USDC|SOL>
```
Expected: both print `Updated … gRPC pool → …` (they're already in the list).

- [ ] **Step 4: Live smoke — verify JTO + SLX price from gRPC**

Run: `GRPC_PRICE_SMOKE=1 ./target/release/portfolio-watcher` (real list now has JTO+SLX wired).
Expected: prints `JTO = $<spot>` and `SLX = $<spot>` matching DexScreener within a few percent. Record both. If a pool is too inactive to update within 25s (like the earlier JUP dust pool), note it — a deep Orca pool should update quickly.

- [ ] **Step 5: Doc + commit**

Update `.env.example`'s `MOMENTUM_GRPC_PRICING` note: "Supported: raydium_amm_v4 (vaults) + Orca/Raydium-CLMM/Meteora-DLMM (state account). Meteora DAMM = not yet."
```bash
git add scripts/fetch_orca_pools.js pools.json .env.example
git commit -m "feat(grpc-pricer): add JTO+SLX Orca pools + wire them; doc supported DEX kinds"
```
(Note: `assets/momentum_tokens.json` is gitignored — the `add_momentum_token.js` edits are local, not committed.)

---

## Self-Review

**Spec coverage:**
- Shared rate→USD core → Task 1. ✓
- Real `dex::Pool` + route CP-vaults vs CL-state by kind → Tasks 2 (CP) + 3 (CL). ✓
- CL price from `parse_cl_pool_state` (store sqrt_price_x64/fee; price/1-over-price by orientation) → Task 3. ✓
- Unsupported kinds (DAMM/Phoenix/…) → REST fallback via the catch-all `continue` → Task 2/3. ✓
- Verify vs existing pool before wiring → Task 3 Step 4 (Orca SOL/USDC smoke). ✓
- Wire JTO/SLX via fetch+merge+add-token → Task 4. ✓
- Default-off + REST fallback safety → preserved (flag guard untouched; every skip path `continue`s → REST). ✓
- Tests: rate_to_usd unit tests (Task 1); live smokes (Tasks 2-4, operator). ✓

**Placeholder scan:** Task 4 uses `<JTO_ORCA_POOL_ID>`/`<SLX_ORCA_POOL_ID>` because the pool ids are *discovered by running the fetcher in Step 2* — they can't be known at plan-time; Step 2 explicitly says to STOP+report if not found rather than fabricate. Everything else is concrete code.

**Type consistency:** `rate_to_usd` (Task 1) is called by `WiredPool::price_usd` (Tasks 2/3); `WiredPool`/`Role`/`acct_index` names are consistent across Tasks 2-3; `parse_cl_pool_state`/`parse_spl_token_amount`/`snapshot_state`/`Arc::try_from` match the confirmed dex API.

## Known implementer confirmations (verify in-repo, not placeholders)
- `pending` struct shape (`tok`/`pc`/`quote_is_usdc`) + the flag/endpoint guard + decimals fetch already exist in `spawn_grpc_feed` from the shipped feature — reuse as-is (Task 2 only swaps the tracking model).
- `run_grpc_smoke`'s current fallback-`WatchedToken` construction (Task 3 Step 4 adds two env overrides to it).
- `fetch_orca_pools.js` `MINTS` entry format + whether it emits USDC and/or SOL quote pools for a given mint (Task 4 Step 1-2).
