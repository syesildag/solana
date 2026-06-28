# Multi-Slot Live Momentum Trader (SP3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Let the live momentum trader hold up to `MOMENTUM_MAX_POSITIONS` concurrent positions (default 1 = identical to today), each governed by its per-token params (global fallback), with weakest-green eviction, sharing the daily cap/cooldowns/halt — reusing existing per-slot on-chain execution.

**Architecture:** `TraderState.position: Option<Position>` → `positions: Vec<Position>` (legacy-migrated on load). `maybe_exit`/`maybe_enter`/eviction generalize the single-`Option` sites to iterate slots and return `Vec<TradeOutcome>`; the watcher loops over outcomes and dispatches on free-capacity (enter) / held (exit). At `MAX_POSITIONS=1` with no overrides, every path reduces to current behavior (the live N=1 anchor).

**Tech Stack:** Rust, tokio (async live loop). Tests: `cargo test --lib` (LIB tests, not `--bin`); plus a manual dry-run smoke.

## Global Constraints

- **Default-off & backward-compatible:** `MOMENTUM_MAX_POSITIONS` defaults to `1`; at 1 with no per-token overrides the trader is behaviorally identical to today. Legacy state files (`"position": {…}`) migrate into `positions[0]` on load.
- **Paper-first:** `DRY_RUN_MOMENTUM_TRADER` still gates real submission. Reuse existing `submit_and_confirm`/`flatten_position`/swap builders per slot — no new on-chain primitives.
- **Per-tick order:** exits (all held) → eviction (if full & `rotate_margin>0`) → entries (fill free slots). Dedup by mint; daily cap + cooldown shared across slots.
- **Per-token params** from `WatchedToken.params` (SP1) resolve `min_metric`/`trail_pct`/`max_run_pct` per token (override ?? global) at entry-threshold / trailing-stop / over-extension.
- **Sim untouched** (already multi-position). Do not change `replay_multi`/`run_grid*`.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/mod.rs` | `momentum_max_positions` config | add field |
| `src/portfolio/momentum_state.rs` | `positions: Vec<Position>` + legacy migration + helpers | core change |
| `src/portfolio/momentum.rs` | multi-slot `maybe_exit`/`maybe_enter`/eviction/reconcile + per-token resolvers; `TradeOutcome` plumbing | core change |
| `src/portfolio/watcher.rs` | dispatch `Vec<TradeOutcome>`; capacity-based control flow; `held_token`→held set | rewrite dispatch |
| `.env.example`, `CLAUDE.md` | document `MOMENTUM_MAX_POSITIONS` | docs |

---

## Task 1: config + state model (`positions: Vec<Position>`) + legacy migration

**Files:**
- Modify: `src/portfolio/mod.rs` (config), `src/portfolio/momentum_state.rs` (state)
- Test: `src/portfolio/momentum_state.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `PortfolioConfig.momentum_max_positions: usize`; `TraderState.positions: Vec<Position>`; helpers `capacity(max)`, `held_mints()`, `position_for(&mint)`, and a migrated `entries_last_24h`.

- [ ] **Step 1: Write failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/momentum_state.rs`:

```rust
    #[test]
    fn legacy_position_migrates_into_positions_on_load() {
        // A state file written by the single-slot trader (field `position`) must load with
        // that position in `positions[0]`.
        let legacy = r#"{
            "position":{"mint":"M","symbol":"S","entry_ts":1700000000,
              "entry_price_usd":1.0,"token_amount":1.0,"usdc_spent":1.0,
              "peak_price_usd":1.0,"entry_sig":"dry-run","dry_run":true},
            "last_exit_ts_per_mint":{},"trades":[]
        }"#;
        let path = tmp("legacy_migrate");
        std::fs::write(&path, legacy).unwrap();
        let st = load(&path).unwrap();
        assert_eq!(st.positions.len(), 1, "legacy single position migrates into positions[0]");
        assert_eq!(st.positions[0].mint, "M");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn legacy_null_position_is_empty_positions() {
        let legacy = r#"{"position":null,"last_exit_ts_per_mint":{},"trades":[]}"#;
        let path = tmp("legacy_null");
        std::fs::write(&path, legacy).unwrap();
        let st = load(&path).unwrap();
        assert!(st.positions.is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn positions_round_trip_and_helpers() {
        let mut st = TraderState::default();
        st.positions.push(position(1_700_000_000, 120.0)); // helper builds mint "MINT_A"
        let path = tmp("positions_rt");
        save(&path, &st).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got.positions.len(), 1);
        assert_eq!(got.capacity(3), 2, "3 - 1 held = 2 free");
        assert!(got.position_for("MINT_A").is_some());
        assert!(got.position_for("NOPE").is_none());
        assert_eq!(got.held_mints(), vec!["MINT_A".to_string()]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn entries_last_24h_counts_all_open_positions_plus_recent_closed() {
        let now = 2_000_000_000;
        let mut st = TraderState::default();
        st.positions.push(position(now - 60, 100.0));   // open, recent
        st.positions.push(position(now - 120, 100.0));  // open, recent (2nd slot)
        // (no closed trades) → 2 entries in the window
        assert_eq!(entries_last_24h(&st, now), 2);
    }
```

> The existing `position(entry_ts, peak)` test helper builds a `Position` with mint `"MINT_A"`. For `entries_last_24h_counts_all_open_positions_plus_recent_closed` both positions share that mint; if dedup-by-mint in helpers matters, give the second a different mint inline. Adjust so the test reflects "count of open positions in window".

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib momentum_state 2>&1 | tail -15`
Expected: FAIL — `TraderState` has no field `positions` / no `capacity`/`held_mints`/`position_for`.

- [ ] **Step 3: Change the state model + add migration + helpers**

In `src/portfolio/momentum_state.rs`, in `TraderState`:
- Replace `pub position: Option<Position>` with `#[serde(default)] pub positions: Vec<Position>`.
- Add a legacy field for migration: `#[serde(default, skip_serializing)] position: Option<Position>` (private; read-only for migration).

In `load()`, after parsing, migrate then return:
```rust
    let mut state: TraderState = serde_json::from_str(&data).context("could not parse trader state file")?;
    // Legacy migration: a single-slot state file carried `position`; move it into `positions`.
    if state.positions.is_empty() {
        if let Some(p) = state.position.take() {
            state.positions.push(p);
        }
    }
    state.position = None; // never re-serialized
    Ok(state)
```

Add helpers (impl `TraderState`):
```rust
impl TraderState {
    pub fn capacity(&self, max_positions: usize) -> usize {
        max_positions.saturating_sub(self.positions.len())
    }
    pub fn held_mints(&self) -> Vec<String> {
        self.positions.iter().map(|p| p.mint.clone()).collect()
    }
    pub fn position_for(&self, mint: &str) -> Option<&Position> {
        self.positions.iter().find(|p| p.mint == mint)
    }
}
```

Update `entries_last_24h` to count all open positions in the window + recent closed:
```rust
pub fn entries_last_24h(state: &TraderState, now_ts: i64) -> usize {
    let cutoff = now_ts - 86_400;
    let closed = state.trades.iter().filter(|t| t.entry_ts >= cutoff).count();
    let open = state.positions.iter().filter(|p| p.entry_ts >= cutoff).count();
    closed + open
}
```

Update any other `momentum_state.rs` site that referenced `state.position` (e.g. `reconcile`/peak-repair callers live in momentum.rs — handled in later tasks; within this file fix the test helpers and any `position` usage).

- [ ] **Step 4: Add the config field**

In `src/portfolio/mod.rs`, add to `PortfolioConfig`:
```rust
    /// Max concurrent momentum positions (slots). `1` = single-slot (default, identical to
    /// the original trader); >1 enables the multi-slot trader.
    pub momentum_max_positions: usize,
```
and in `from_env`:
```rust
            momentum_max_positions: parse_env("MOMENTUM_MAX_POSITIONS", 1_usize)?.max(1),
```
(Match the file's `parse_env` idiom; clamp to ≥1.)

- [ ] **Step 5: Make the crate compile (momentum.rs/watcher.rs still use `.position`)**

`momentum.rs` and `watcher.rs` reference `state.position` extensively — they're rewritten in Tasks 2–6. To keep Task 1 self-contained and committable, add a **temporary compatibility shim** so the crate builds: a method `pub fn position(&self) -> Option<&Position> { self.positions.first() }` on `TraderState`, and where existing code *sets* `state.position = Some(p)` / `= None`, the build will error — for Task 1, do the **minimal** mechanical change to those set-sites to `state.positions = vec![p]` / `state.positions.clear()` so it compiles, WITHOUT changing single-slot logic (Tasks 2–6 then properly generalize). Run `cargo build --lib 2>&1 | tail -30` and fix each `state.position` usage minimally until clean.

> This shim keeps single-slot behavior intact (first slot) so the suite stays green after Task 1; Tasks 2–6 replace the shimmed sites with real multi-slot logic.

- [ ] **Step 6: Run tests + full suite**

Run: `cargo test --lib momentum_state 2>&1 | tail -12` then `cargo test --lib 2>&1 | tail -4`
Expected: the 4 new tests pass; full suite green (single-slot behavior preserved by the shim).

- [ ] **Step 7: Commit**

```bash
git add src/portfolio/mod.rs src/portfolio/momentum_state.rs src/portfolio/momentum.rs src/portfolio/watcher.rs
git commit -m "feat(momentum): multi-slot state model (positions: Vec<Position>) + MAX_POSITIONS config

TraderState holds Vec<Position> (legacy `position` migrates into positions[0] on load);
adds MOMENTUM_MAX_POSITIONS (default 1) + capacity/held_mints/position_for helpers. A
compatibility shim keeps single-slot sites working until Tasks 2-6 generalize them.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: multi-slot `maybe_exit` (per-token, all held) → `Vec<TradeOutcome>`

**Files:** Modify `src/portfolio/momentum.rs`; Test: same file.

**Interfaces:**
- Consumes: state `positions`, per-token `WatchedToken.params`, existing `flatten_position`/exit helpers.
- Produces: `pub async fn maybe_exit(ctx) -> Result<Vec<TradeOutcome>>` (was `Option`); per-token resolvers `min_metric_for`/`trail_for`/`max_run_for` (private fns in `momentum.rs`, mirroring the sim).

- [ ] **Step 1: Read the current `maybe_exit` + helpers**

Read `maybe_exit` (~line 1749) and `maybe_take_profit_on_fade`. Understand how it loads state, evaluates the single position's trailing/fade/breakeven/max-hold stop, calls `flatten_position`, records the trade, saves state, returns `Option<TradeOutcome>`.

- [ ] **Step 2: Write the per-token resolver test**

Add to `#[cfg(test)] mod tests` in `momentum.rs`:
```rust
    #[test]
    fn per_token_resolvers_override_then_global() {
        let g_min = 0.04_f64; let g_trail = 20.0_f64; let g_run = 6.0_f64;
        let w_over = WatchedToken { symbol: "A".into(), mint: "A".into(), name: None, equity: None,
            params: Some(crate::portfolio::momentum_universe::TokenParams {
                min_metric: Some(0.09), trail_pct: Some(30.0), max_run_pct: None }) };
        let w_none = WatchedToken { symbol: "B".into(), mint: "B".into(), name: None, equity: None, params: None };
        let watched = vec![w_over, w_none];
        assert_eq!(min_metric_for(&watched, "A", g_min), 0.09); // override
        assert_eq!(trail_for(&watched, "A", g_trail), 30.0);    // override
        assert_eq!(max_run_for(&watched, "A", g_run), 6.0);     // field None → global
        assert_eq!(min_metric_for(&watched, "B", g_min), 0.04); // no params → global
        assert_eq!(trail_for(&watched, "Z", g_trail), 20.0);    // unknown mint → global
    }
```

- [ ] **Step 3: Add the per-token resolvers + generalize `maybe_exit`**

Add private resolvers in `momentum.rs` (used by exit/entry/eviction):
```rust
fn token_params_for<'a>(watched: &'a [WatchedToken], mint: &str) -> Option<&'a crate::portfolio::momentum_universe::TokenParams> {
    watched.iter().find(|w| w.mint == mint).and_then(|w| w.params.as_ref())
}
fn min_metric_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint).and_then(|p| p.min_metric).unwrap_or(global)
}
fn trail_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint).and_then(|p| p.trail_pct).unwrap_or(global)
}
fn max_run_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint).and_then(|p| p.max_run_pct).unwrap_or(global)
}
```

Generalize `maybe_exit` to return `Result<Vec<TradeOutcome>>`:
- Load state. For **each** position in `state.positions` (clone the list to iterate; collect closes), evaluate its stop using `trail_for(ctx.watched, &pos.mint, cfg.momentum_trail_pct)` (and per-token max_run/min_metric where the existing logic used the global `min_metric`/`max_run` for that position's fade/over-extension). When a position trips, `flatten_position` it (as today), record the closed trade, and remove it from `state.positions`. Accumulate `TradeOutcome::Exited`/`Rotated` into a `Vec`.
- Eviction (when `state.positions.len() == cfg.momentum_max_positions` and `rotate_margin>0`) is added in Task 4 — leave a clearly-marked hook here, or place eviction in `maybe_enter`/a dedicated fn called from the tick (Task 4 decides; keep Task 2 to pure exits).
- Save state once after processing. Return the `Vec`.
- **N=1 reduction:** with one position, the loop runs once and behaves as today; return `vec![outcome]` or `vec![]`.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib momentum 2>&1 | tail -15` then `cargo test --lib 2>&1 | tail -4`.
Expected: resolver test passes; suite green. (Callers in `watcher.rs` now mismatch the `Vec` return — Task 5 fixes the watcher; to keep the crate compiling between tasks, temporarily adapt the single `watcher.rs` call site to handle a `Vec` (e.g. `for o in outcomes { … }`) — minimal change, fully done in Task 5.)

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum.rs src/portfolio/watcher.rs
git commit -m "feat(momentum): multi-slot maybe_exit (all held, per-token stops) → Vec<TradeOutcome>

Per-token min_metric/trail/max_run resolvers (override ?? global); maybe_exit evaluates
every held position against its own trailing stop. N=1 reduces to current behavior.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: multi-slot `maybe_enter` (fill free slots, per-token) → `Vec<TradeOutcome>`

**Files:** Modify `src/portfolio/momentum.rs`; Test: same file.

**Interfaces:**
- Consumes: `capacity`, per-token resolvers, ranking (`rank_candidates`), existing entry-swap execution.
- Produces: `pub async fn maybe_enter(ctx) -> Result<Vec<TradeOutcome>>`.

- [ ] **Step 1: Read the current `maybe_enter`** (~line 1151) — how it ranks, applies gates (`min_metric`, over-extension, stale/falling/fading, cooldown, daily cap, cost), sizes `momentum_trade_usdc`, executes the entry swap, records, saves, returns `Option`.

- [ ] **Step 2: Generalize `maybe_enter` to fill free capacity**

- Load state. `let cap = state.capacity(cfg.momentum_max_positions);` If `cap == 0`, return `vec![]` (full — eviction handled in Task 4).
- Rank candidates (as today). Iterate best-first; for each eligible candidate (not held — check `state.position_for(mint).is_none()`; not stale/falling/fading; over-extension via `max_run_for`; score > `min_metric_for(mint)`; off cooldown; daily cap not hit), execute the entry (existing swap path), push to `state.positions`, record, decrement `cap`. Stop when `cap == 0`, candidates exhausted, or the daily cap is reached.
- Save once. Return the `Vec<TradeOutcome>` of `Entered`.
- **N=1 reduction:** `cap` is 0 (holding) or 1 (flat) → at most one entry, as today.
- Reuse the entry execution exactly (size, slippage escalation, submit_and_confirm) per entry.

- [ ] **Step 3: Test** — a pure-helper test that the candidate-selection loop respects capacity and dedup (extract the eligible-candidate selection into a testable pure fn if practical, e.g. `select_entries(ranked, held_mints, cap, …) -> Vec<&Candidate>`, and unit-test it: returns ≤ cap, skips held mints, respects per-token threshold). Then `cargo test --lib 2>&1 | tail -4`.

- [ ] **Step 4: Commit**

```bash
git add src/portfolio/momentum.rs
git commit -m "feat(momentum): multi-slot maybe_enter (fill free slots, per-token gates) → Vec<TradeOutcome>

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: weakest-green eviction (generalize `try_rotate`)

**Files:** Modify `src/portfolio/momentum.rs`; Test: same file.

**Interfaces:**
- Consumes: `rotation_target`, `rotation_net_green`, per-token resolvers, rotation-swap execution.
- Produces: an eviction step (called from the exit tick or a dedicated fn) that, when full & `rotate_margin>0`, rotates the weakest-green held into a stronger candidate; folds into `maybe_exit`'s returned `Vec` or a sibling fn `maybe_evict(ctx) -> Result<Vec<TradeOutcome>>`.

- [ ] **Step 1: Read current `try_rotate`** (~line 1468) — single-position rotation: gross-green pre-filter, `rotation_target`, cost/net-green gate, the A→B swap, basis carry-forward, state update, `Rotated` outcome.

- [ ] **Step 2: Generalize to weakest-green selection**

- Guard: `rotate_margin>0` && `state.positions.len() == cfg.momentum_max_positions` && daily cap allows.
- Find the **weakest green** held: lowest current ranked score among positions that are gross-green (`px > entry`) and non-stale (mirror `replay_multi`'s eviction). Use `rotation_target(&ranked, &weakest.mint, weakest_score, min_metric_for(weakest), rotate_margin, …)`; if a target qualifies and isn't already held and is net-green after cost, execute the A→B rotation swap (existing path), replace the weakest position with the new one, record, return `Rotated`.
- **N=1 reduction:** "weakest of one" == the single held → identical to today's `try_rotate`.
- Decide call site: in the fast-exit tick after exits (matches the sim's stop→evict→enter order). Have `maybe_exit` call it, or the watcher call `maybe_evict` between exit and enter. Keep ordering: exits → eviction → entries.

- [ ] **Step 3: Test** — pure `weakest_green(&positions, &ranked, prices) -> Option<idx>` helper unit-tested (picks lowest-score green; ignores red/stale). `cargo test --lib 2>&1 | tail -4`.

- [ ] **Step 4: Commit** (`feat(momentum): weakest-green eviction for multi-slot (generalize try_rotate)`).

---

## Task 5: watcher control-flow rewrite (Vec outcomes, capacity dispatch)

**Files:** Modify `src/portfolio/watcher.rs`.

- [ ] **Step 1: Read the dispatch** (fast-exit arm ~line 332; slow-tick enter ~line 530; `apply_outcome`, `held_token`).

- [ ] **Step 2: Generalize dispatch**
- Fast-exit arm: `let outcomes = maybe_exit(&mctx).await?` → `for o in outcomes { if !o.dry_run() { apply_outcome(&mut portfolio, &o); } }`. (Eviction `Rotated` outcomes flow through here too.)
- Slow-tick arm: `let outcomes = maybe_enter(&mctx).await?` → same loop. (No FLAT/HOLDING gate — `maybe_enter` self-limits via `capacity`.)
- `held_token(&cfg)` (singular, used for the scan overlay `effective_universe`) → a held-set: read `state.positions` mints; update `effective_universe` to overlay all held mints (so held tokens stay in the watched set). Add/adjust a `held_mints(&cfg)` reader if needed.
- Preserve the borrow-release-before-mutate pattern (build `mctx` in a block, drop it, then mutate `portfolio`).

- [ ] **Step 3: Build + dry-run smoke**
Run `cargo build --release 2>&1 | tail -5` (full binary). Then a dry-run smoke (manual): with `DRY_RUN_MOMENTUM_TRADER=true ENABLE_MOMENTUM_TRADER=true MOMENTUM_MAX_POSITIONS=3`, start the watcher briefly against recent history/prices and confirm it logs opening up to 3 paper positions and exiting them, no panics. Capture the log snippet in the report. (If a full live-loop smoke isn't feasible in the harness, assert via the unit/integration tests that the dispatch compiles and `maybe_enter`/`maybe_exit` return `Vec`, and note the manual smoke as a follow-up.)
Then `cargo test --lib 2>&1 | tail -4`.

- [ ] **Step 4: Commit** (`feat(watcher): multi-slot dispatch — Vec outcomes, capacity-based enter/exit`).

---

## Task 6: multi-position reconciliation/adoption + docs

**Files:** Modify `src/portfolio/momentum.rs` (`reconcile_startup_position`, `adopt_wallet_position`), `.env.example`, `CLAUDE.md`.

- [ ] **Step 1: Generalize startup reconciliation/adoption**
- `reconcile_startup_position` (~916) and `adopt_wallet_position` (~1023): currently reconcile/adopt a single position. Generalize to up to `momentum_max_positions` wallet positions: adopt each watched-token wallet balance ≥ the min as a slot, up to N, dedup by mint. Preserve the dry/live guards and the min-USD threshold. At N=1 → adopts one (today's behavior).

- [ ] **Step 2: Docs**
- `.env.example`: add `MOMENTUM_MAX_POSITIONS=1` with a comment (default single-slot; >1 = multi-slot, paper-test first).
- `CLAUDE.md`: add `MOMENTUM_MAX_POSITIONS` to the momentum env documentation, noting default-1 backward-compat + per-token params + paper-first.

- [ ] **Step 3: Build + full suite**
Run `cargo build --release 2>&1 | tail -3` and `cargo test --lib 2>&1 | tail -4`. Expected: clean, green.

- [ ] **Step 4: Commit** (`feat(momentum): multi-position startup reconciliation/adoption + docs`).

---

## Self-Review

**1. Spec coverage:** state `Vec<Position>` + migration (T1); config `MAX_POSITIONS` (T1); per-token resolvers + multi-slot exit (T2); fill-free-slots entry (T3); weakest-green eviction (T4); watcher Vec/capacity dispatch (T5); multi-position reconciliation + docs (T6); per-tick order exits→evict→enter (T2/T4/T5); N=1 equivalence + paper-first (throughout, anchored by migration + reduction reasoning). ✓
**2. Placeholder scan:** T1 gives complete state/config code + tests. T2–T6 are *generalize-existing-function* tasks: they give the new signature, the per-token resolvers (full code), the exact transformation + the N=1 reduction + test specs, and instruct the implementer to read the current single-slot function as the reference (the existing code IS the spec for the per-slot behavior to preserve). This is appropriate for a 116KB-file generalization where pasting full async bodies is impractical; the N=1-equivalence + migration tests are the safety gates. No vague "add error handling" — each task names the precise sites and invariants. ✓
**3. Type consistency:** `maybe_exit`/`maybe_enter` → `Result<Vec<TradeOutcome>>` (T2/T3) consumed by the watcher loop (T5); `TraderState.positions: Vec<Position>` + `capacity`/`held_mints`/`position_for` (T1) used in T2/T3/T4/T6; per-token resolvers `min_metric_for`/`trail_for`/`max_run_for(watched, mint, global)` defined T2, used T2/T3/T4; `momentum_max_positions` (T1) read in T3/T4/T6. ✓

## Caveat (carried to the user)
This is the live trader. It ships default-off (`MAX_POSITIONS=1` = identical to today) and behind `DRY_RUN_MOMENTUM_TRADER`. The validation gate (SP2) found single-slot still wins P&L on the current sample — multi-slot is **capability to paper-trade**, not a proven live edge. Recommend running paper (`DRY_RUN_MOMENTUM_TRADER=true`, `MAX_POSITIONS>1`) and comparing realized results before any real-capital multi-slot run.
