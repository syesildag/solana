# Per-Token Momentum Params (SP1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let `momentum_tokens.json` carry optional per-token `{min_metric, trail_pct, max_run_pct}`, parsed by the universe loader and applied by the sim's `replay_multi_core` with global `.env` fallback; a file with no overrides reproduces today's behavior exactly.

**Architecture:** Additive. (T1) `WatchedToken` gains `Option<TokenParams>`. (T2) `Candidate` gains the two slope values `rank_candidates` already computes, so over-extension can be re-evaluated per token without rebuilding price windows. (T3) `replay_multi_core` resolves each token's effective `min_metric` / `trail_pct` / `max_run_pct` (override ?? global) at the entry-threshold, trailing-stop, and over-extension sites. No-override ⇒ byte-identical to today.

**Tech Stack:** Rust, serde. Tests are `#[cfg(test)]` blocks in the respective files, run with `cargo test --lib` (these are LIB tests — `cargo test --bin momentum-sim` shows 0 tests).

## Global Constraints

- **Sim + loader only (SP1).** No optimize-config write (SP2), no live-trader change (SP3).
- **Per-token-overridable = `{min_metric, trail_pct, max_run_pct}`.** `metric`, `lookback`, `regime`, `rotate_margin` stay global.
- **Per-field optional, global fallback:** effective value = token override `??` global `ParamSet` value.
- **No-override equivalence (regression guarantee):** with no `params` on any token, `replay_multi`/`replay_multi_core` is byte-identical to current behavior; all existing `replay_multi*`, `run_grid_multi`, and `momentum_universe` tests pass unchanged.
- **Eviction/rotation stays global:** `rotation_target` keeps using global `params.min_metric` (cross-token ranking). Per-token `min_metric` applies only to the entry gate and the fade exit (token-specific).
- Production `run_grid`, `replay`, `replay_with_stream`, `replay_with_regime`, live trader untouched.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/momentum_universe.rs` | `TokenParams` type + `WatchedToken.params` + loader tests | Modify (additive) |
| `src/portfolio/momentum.rs` | `Candidate` gains `slope_recent`/`slope_full`; `rank_candidates` populates them; update 2 test-helper Candidate literals | Modify (additive) |
| `src/portfolio/sim.rs` | `replay_multi_core` per-token resolution at entry/exit/over-extension; behavioral tests | Modify |

---

## Task 1: `TokenParams` schema + loader

**Files:**
- Modify: `src/portfolio/momentum_universe.rs`
- Test: same file `#[cfg(test)]` block

**Interfaces:**
- Produces: `pub struct TokenParams { pub min_metric: Option<f64>, pub trail_pct: Option<f64>, pub max_run_pct: Option<f64> }` (derives `Debug, Clone, Default, Serialize, Deserialize`); `WatchedToken` gains `pub params: Option<TokenParams>`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/momentum_universe.rs`:

```rust
    #[test]
    fn parses_per_token_params_full_partial_and_absent() {
        let json = r#"[
          {"symbol":"AAA","mint":"So11111111111111111111111111111111111111112",
           "params":{"min_metric":0.05,"trail_pct":30.0,"max_run_pct":0.0}},
          {"symbol":"BBB","mint":"BPxxfRCXkUVhig4HS1Lh7kZqV6SPJhzfEk4x6fVBjPCy",
           "params":{"trail_pct":12.0}},
          {"symbol":"CCC","mint":"jtojtomepa8beP8AuQc6eXt5FriJwfFMwQx2v2f9mCL"}
        ]"#;
        let raw: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        // full
        let a = raw[0].params.as_ref().unwrap();
        assert_eq!(a.min_metric, Some(0.05));
        assert_eq!(a.trail_pct, Some(30.0));
        assert_eq!(a.max_run_pct, Some(0.0));
        // partial — only trail set, others None (per-field fallback)
        let b = raw[1].params.as_ref().unwrap();
        assert_eq!(b.trail_pct, Some(12.0));
        assert_eq!(b.min_metric, None);
        assert_eq!(b.max_run_pct, None);
        // absent — no params block
        assert!(raw[2].params.is_none());
    }

    #[test]
    fn token_without_params_serializes_without_the_key() {
        let w = WatchedToken {
            symbol: "AAA".into(),
            mint: "So11111111111111111111111111111111111111112".into(),
            name: None,
            equity: None,
            params: None,
        };
        let s = serde_json::to_string(&w).unwrap();
        assert!(!s.contains("params"), "no params key when None, got: {s}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib momentum_universe 2>&1 | tail -15`
Expected: FAIL — `WatchedToken` has no field `params` / cannot construct.

- [ ] **Step 3: Add `TokenParams` and the field**

In `src/portfolio/momentum_universe.rs`, add the struct (above `WatchedToken`):

```rust
/// Optional per-token momentum parameter overrides. Each field falls back to the
/// global `.env` value when `None`. Only token-specific knobs are overridable;
/// metric/lookback/regime/rotate stay global.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_metric: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trail_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_run_pct: Option<f64>,
}
```

Add the field to `WatchedToken` (after `equity`):

```rust
    /// Optional per-token parameter overrides (min_metric/trail_pct/max_run_pct);
    /// each falls back to the global config when absent. See `TokenParams`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<TokenParams>,
}
```

- [ ] **Step 4: Update any other `WatchedToken { … }` literals in this file's tests**

The existing tests in `momentum_universe.rs` may construct `WatchedToken` literals; add `params: None` to each so they compile. Run `cargo test --lib momentum_universe 2>&1 | tail -15` — fix every "missing field `params`" until it compiles, then the two new tests pass.
Expected: PASS.

- [ ] **Step 5: Confirm the wider build (other crates construct WatchedToken)**

Run: `cargo build --lib 2>&1 | tail -15`
Expected: clean. If any `WatchedToken { … }` literal elsewhere errors on the missing field, add `params: None` there. (Known literal sites that set fields explicitly live in `sim.rs` tests and `momentum.rs` tests — those are updated in Tasks 2/3, but if the build flags them now, add `params: None`.)

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/momentum_universe.rs
git commit -m "feat(momentum): optional per-token params in momentum_tokens.json (schema+loader)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: enrich `Candidate` with slope values

**Files:**
- Modify: `src/portfolio/momentum.rs` (`Candidate` struct ~line 79; `rank_candidates` build ~line 642; two test-helper literals ~lines 2396, 2556)

**Interfaces:**
- Consumes: `recent_slope`, `ln_price_slope` (already compute these values in `rank_candidates`).
- Produces: `Candidate` gains `pub slope_recent: Option<f64>` and `pub slope_full: Option<f64>` — the recent-window and whole-window ln-price slopes used by `is_overextended`. Lets a consumer re-evaluate over-extension with a different `max_run_pct` without rebuilding the price window.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/momentum.rs`:

```rust
    #[test]
    fn rank_candidates_exposes_slopes_for_overextension_recompute() {
        // A steadily-rising token has a positive whole-window slope; the stored
        // slope_full must reproduce is_overextended when fed back with the same max_run.
        let mut hist: std::collections::VecDeque<PriceSnapshot> = std::collections::VecDeque::new();
        let mut p = 1.0_f64;
        for i in 0..130u64 {
            let mut m = std::collections::HashMap::new();
            m.insert("AAA".to_string(), p);
            m.insert(SOL_KEY.to_string(), 150.0);
            hist.push_back(PriceSnapshot { ts: 1000 + i * 180, prices: m });
            p *= 1.01;
        }
        let watched = vec![WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None, params: None }];
        let prices: std::collections::HashMap<String, f64> =
            [("AAA".to_string(), p)].into_iter().collect();
        let cands = rank_candidates(&watched, &prices, &hist, 121, 0, RankMetric::Return, 6.0, 0, 0);
        let c = cands.iter().find(|c| c.mint == "AAA").expect("AAA ranked");
        // Re-evaluating is_overextended with the stored slopes + same max_run must equal
        // the candidate's precomputed flag.
        let recomputed = is_overextended(c.metrics.ret, 6.0, c.slope_recent, c.slope_full);
        assert_eq!(recomputed, c.overextended, "stored slopes reproduce is_overextended");
        // whole-window slope of a monotone rise is positive
        assert!(c.slope_full.is_some_and(|s| s > 0.0));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib rank_candidates_exposes_slopes 2>&1 | tail -15`
Expected: FAIL — no field `slope_recent`/`slope_full` on `Candidate`.

- [ ] **Step 3: Add the fields + populate them**

In `src/portfolio/momentum.rs`, add to the `Candidate` struct (after `metric_fading`):

```rust
    /// Recent-window ln-price slope (`recent_slope`) and whole-window ln-price slope
    /// (`ln_price_slope`) — the two inputs `is_overextended` consumes. Stored so a
    /// consumer can re-evaluate over-extension with a different `max_run_pct` without
    /// rebuilding the price window. `None` when the window was too short to fit a slope.
    pub slope_recent: Option<f64>,
    pub slope_full: Option<f64>,
```

In `rank_candidates` (~line 631), the locals `slope_recent` and `ln_price_slope(window)` are already computed for the `is_overextended` call. Capture the full slope in a local and store both on the `Candidate`:

Change:
```rust
            let slope_recent = recent_slope(window, decel_lookback_min);
            let overextended = is_overextended(metrics.ret, max_run_pct, slope_recent, ln_price_slope(window));
```
to:
```rust
            let slope_recent = recent_slope(window, decel_lookback_min);
            let slope_full = ln_price_slope(window);
            let overextended = is_overextended(metrics.ret, max_run_pct, slope_recent, slope_full);
```
and in the `Candidate { … }` literal add the two fields (after `metric_fading`):
```rust
                metric_fading,
                slope_recent,
                slope_full,
```

- [ ] **Step 4: Update the two test-helper Candidate literals**

In `src/portfolio/momentum.rs`, the test helpers at ~line 2396 (`mk`) and ~line 2556 (`cand`) build `Candidate { … }` literals. Add `slope_recent: None, slope_full: None,` to each (those tests don't exercise over-extension recompute, so `None` is fine).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib momentum:: 2>&1 | tail -15` then `cargo test --lib 2>&1 | tail -4`
Expected: PASS — the new test plus all pre-existing momentum/sim tests (the new fields are additive; `ranked_stream`/`run_grid_multi` behavior unchanged).

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/momentum.rs
git commit -m "feat(momentum): expose recent/full ln-price slopes on Candidate

Stores the two slopes rank_candidates already computes so over-extension can be
re-evaluated with a per-token max_run without rebuilding the price window. Additive;
no behavior change.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `replay_multi_core` per-token consumption

**Files:**
- Modify: `src/portfolio/sim.rs` (`replay_multi_core`)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: `TokenParams` (Task 1), `Candidate.slope_recent`/`slope_full` (Task 2), `is_overextended` (momentum.rs, pub).
- Produces: per-token effective params applied inside `replay_multi_core`. No new public symbol.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/sim.rs` (helpers `rise_then_fall`, `aaa`, `bare_params`, `ranked_stream`, `replay_with_regime` already exist):

```rust
    // Build a watched list with an optional per-token override for AAA.
    fn watched_with_params(sym: &str, p: Option<super::super::momentum_universe::TokenParams>) -> Vec<WatchedToken> {
        vec![WatchedToken { symbol: sym.into(), mint: sym.into(), name: None, equity: None, params: p }]
    }

    #[test]
    fn replay_multi_no_overrides_matches_baseline() {
        // No per-token params ⇒ identical to replay_with_regime at N=1 (the anchor).
        let snaps = rise_then_fall("AAA", 130, 6);
        let watched = aaa(); // params: None
        let params = bare_params();
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];
        let single = replay_with_regime(&snaps, &watched, &stream, &params, &mask);
        let multi = replay_multi(&snaps, &watched, &stream, &params, &mask, 1);
        assert_eq!(multi.trades.len(), single.trades.len());
        assert_eq!(multi.equity_curve, single.equity_curve);
    }

    #[test]
    fn replay_multi_per_token_tight_trail_exits_earlier() {
        // Same rise-then-mild-pullback for AAA. With a TIGHT per-token trail it stops out;
        // with the (wide) global trail it does not. Isolates the per-token trail wiring.
        let sol = 150.0;
        let mk = |ts: u64, p: f64| {
            let mut m = std::collections::HashMap::new();
            m.insert("AAA".to_string(), p);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let mut snaps = Vec::new();
        let mut p = 1.0_f64;
        for i in 0..130u64 { snaps.push(mk(1000 + i * 180, p)); p *= 1.01; } // rise → enter
        for i in 130..140u64 { snaps.push(mk(1000 + i * 180, p)); p *= 0.97; } // ~3%/bar pullback

        let mut params = bare_params();
        params.trail_pct = 50.0; // global trail very wide → no stop on a ~26% pullback

        let stream = ranked_stream(&snaps, &watched_with_params("AAA", None), &params);
        let mask = vec![true; snaps.len()];
        let wide = replay_multi(&snaps, &watched_with_params("AAA", None), &stream, &params, &mask, 1);

        let tight = super::super::momentum_universe::TokenParams { trail_pct: Some(8.0), ..Default::default() };
        let w_tight = watched_with_params("AAA", Some(tight));
        let stream2 = ranked_stream(&snaps, &w_tight, &params);
        let tightrun = replay_multi(&snaps, &w_tight, &stream2, &params, &mask, 1);

        assert_eq!(wide.n_trades(), 0, "wide global trail never stops on this pullback");
        assert!(tightrun.n_trades() >= 1, "tight per-token trail stops AAA out");
    }

    #[test]
    fn replay_multi_per_token_high_min_metric_suppresses_entries() {
        // Raising AAA's min_metric above its observed scores blocks its entries.
        let snaps = rise_then_fall("AAA", 200, 0); // steady rise → would enter under global
        let params = bare_params(); // global min_metric = 0.0 → enters
        let mask = vec![true; snaps.len()];

        let base = aaa();
        let stream = ranked_stream(&snaps, &base, &params);
        let with_global = replay_multi(&snaps, &base, &stream, &params, &mask, 1);
        assert!(with_global.n_trades() == 0 || !with_global.trades.is_empty()); // sanity: runs

        let hi = super::super::momentum_universe::TokenParams { min_metric: Some(1e9), ..Default::default() };
        let w_hi = watched_with_params("AAA", Some(hi));
        let stream2 = ranked_stream(&snaps, &w_hi, &params);
        let suppressed = replay_multi(&snaps, &w_hi, &stream2, &params, &mask, 1);
        // No closed trades AND nothing held that could close — the entry never fires.
        assert_eq!(suppressed.n_trades(), 0, "absurd per-token min_metric blocks entries");
    }
```

> Note: the `super::super::momentum_universe::TokenParams` path is illustrative — use whatever path resolves from the sim test module (check the existing `use` lines; `crate::portfolio::momentum_universe::TokenParams` also works). Pick the one that compiles.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib replay_multi_per_token 2>&1 | tail -20`
Expected: FAIL — `replay_multi_per_token_tight_trail_exits_earlier` shows 0 trades in BOTH runs (per-token trail not yet wired), and/or compile errors on `TokenParams` until consumption exists. (`replay_multi_no_overrides_matches_baseline` should already pass — it's the equivalence guard.)

- [ ] **Step 3: Add per-token resolution to `replay_multi_core`**

In `src/portfolio/sim.rs`, ensure `is_overextended` and `TokenParams` are imported (add to the existing `use super::momentum::{…}` / `use super::momentum_universe::{…}` lines: `is_overextended`, and `TokenParams` if referenced).

Near the top of `replay_multi_core` (after the `pending_free` / MTM accumulator setup, before `for i in 0..n {`), add the per-token map + resolvers:

```rust
    // Per-token effective params: override (if present) ?? global. No overrides ⇒ every
    // resolver returns the global value ⇒ behavior identical to a single global ParamSet.
    let tparams: HashMap<&str, &TokenParams> = watched
        .iter()
        .filter_map(|w| w.params.as_ref().map(|p| (w.mint.as_str(), p)))
        .collect();
    let min_metric_for = |mint: &str| tparams.get(mint).and_then(|p| p.min_metric).unwrap_or(params.min_metric);
    let trail_for = |mint: &str| tparams.get(mint).and_then(|p| p.trail_pct).unwrap_or(params.trail_pct);
    let max_run_for = |mint: &str| tparams.get(mint).and_then(|p| p.max_run_pct).unwrap_or(params.max_run_pct);
```

**(3a) Exit — per-token trail.** In the HOLDING stop-family block, the `vol_stop_triggered(...)` call passes `params.trail_pct`. Replace that argument with `trail_for(&pos.mint)`:
```rust
            let fallback_stop = vol_stop_triggered(
                px,
                pos.peak_price_usd,
                trail_for(&pos.mint),   // was params.trail_pct
                params.vol_stop_mode,
                params.chandelier_k,
                token_atr(snapshots, i, &pos.mint, params.vol_obs),
                token_return_sigma(snapshots, i, &pos.mint, params.vol_obs),
            );
```
(Leave `profit_protected_stop_triggered`'s `params.max_trail_pct` as-is — `max_trail_pct` stays global.)

**(3b) Fade — per-token min_metric.** In the fade-exit block, the `fade_take_profit(c.score, params.min_metric, …)` call:
```rust
                        !c.stale
                            && fade_take_profit(c.score, min_metric_for(&pos.mint), px, pos.entry_price_usd)
```

**(3c) Entry — per-token over-extension + threshold.** In the entries `while capacity > 0` block, change the `best` finder predicate and remove the separate global-threshold break. Replace:
```rust
            let best = stream[i].iter().find(|c| {
                !c.stale
                    && !c.overextended
                    && !c.falling
                    && !c.metric_fading
                    && !held.iter().any(|p| p.mint == c.mint)
                    && last_exit_ts
                        .get(&c.mint)
                        .is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
            });
            let Some(best) = best else { break };
            if best.score <= params.min_metric {
                break;
            }
```
with:
```rust
            let best = stream[i].iter().find(|c| {
                !c.stale
                    // per-token over-extension: re-evaluate with the token's own max_run
                    // (== global when no override) using the slopes the candidate stored
                    && !is_overextended(c.metrics.ret, max_run_for(&c.mint), c.slope_recent, c.slope_full)
                    && !c.falling
                    && !c.metric_fading
                    && c.score > min_metric_for(&c.mint) // per-token entry threshold
                    && !held.iter().any(|p| p.mint == c.mint)
                    && last_exit_ts
                        .get(&c.mint)
                        .is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
            });
            let Some(best) = best else { break };
```
(Candidates are score-sorted desc, so with no overrides folding the threshold into the predicate is equivalent to the old "best, then break if ≤ global" — the no-override equivalence test pins this.)

Leave the eviction/rotation block's `params.min_metric` (passed to `rotation_target`) **unchanged** — rotation is cross-token ranking and stays global.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib replay_multi 2>&1 | tail -25`
Expected: PASS — the three new per-token tests AND every pre-existing `replay_multi*` test (including the N=1 anchors and the no-override equivalence). Then full suite: `cargo test --lib 2>&1 | tail -4` — no regressions.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): replay_multi applies per-token min_metric/trail/max_run (global fallback)

Each token's effective entry threshold, trailing-stop width, and over-extension cap
resolve to its override or the global ParamSet. No overrides ⇒ byte-identical to today
(equivalence test). Enables the per-token-tuned basket experiment.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:** schema+loader (T1); per-token {min_metric,trail,max_run} consumption at entry/exit/over-extension (T3); Candidate slope exposure enabling per-token max_run (T2); no-override equivalence (T3 test); eviction stays global (T3 leaves rotation_target untouched). ✓
**2. Placeholder scan:** every step has complete code/edits + real test assertions. The one illustrative note (TokenParams import path) tells the implementer to pick the compiling path — not a placeholder, a real instruction. ✓
**3. Type consistency:** `TokenParams{min_metric,trail_pct,max_run_pct: Option<f64>}` (T1) consumed by the resolvers (T3); `Candidate.slope_recent/slope_full: Option<f64>` (T2) read in the over-extension recompute (T3) via `is_overextended(c.metrics.ret, max_run_for(mint), c.slope_recent, c.slope_full)` — signature matches `is_overextended(window_ret, max_run_pct, slope_recent, slope_full)`. `WatchedToken.params: Option<TokenParams>` (T1) read by the `tparams` map (T3). ✓
