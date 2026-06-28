# Per-Token Regime Opt-Out Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Let a token opt out of the global SOL regime gate via `params.regime_filter: false` in `momentum_tokens.json` — exempt tokens stay tradeable when the market is risk-off — in both the sim (`replay_multi`) and the live trader (`maybe_enter`).

**Architecture:** Add `regime_filter: Option<bool>` to the per-token `TokenParams`. The regime *mask* stays global+SOL-derived; only the per-candidate eligibility check changes from `regime[i]` to `(regime[i] || regime_exempt(mint))`. Opt-out only; default obeys the global gate; no token is exempt ⇒ byte-identical to today.

**Tech Stack:** Rust. Tests: `cargo test --lib`.

## Global Constraints

- **Opt-out only, backward-compatible:** `regime_filter` defaults to obey-global; `false` = exempt. With no token exempt, behavior is byte-identical to today (the N=1 anchor + all existing `replay_multi`/`run_grid_multi` tests must stay green).
- **Regime mask unchanged:** still global, SOL-derived; only *who consults it* changes.
- **Operator-set, not auto-tuned:** `per-token-tune`/`optimize_momentum.py` must NOT emit `regime_filter` (they preserve it on existing entries, like other untouched fields).
- Reuses the existing per-token resolver pattern (`min_metric_for`/`max_run_for` in both sim and live).

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/momentum_universe.rs` | `TokenParams.regime_filter` | add field |
| `src/portfolio/sim.rs` | `replay_multi_core` per-candidate regime check | modify entry loop |
| `src/portfolio/momentum.rs` | `maybe_enter` per-candidate regime check + `regime_exempt_for` | modify live entry |
| `.claude/skills/optimize-momentum-config/SKILL.md` | note `regime_filter` is operator-set | docs |

---

## Task 1: schema + sim consumption

**Files:** Modify `src/portfolio/momentum_universe.rs`, `src/portfolio/sim.rs`; Test: both files' `#[cfg(test)]`.

**Interfaces:**
- Produces: `TokenParams.regime_filter: Option<bool>`; in `sim.rs`, a `regime_exempt` set consulted in `replay_multi_core`'s entry predicate.

- [ ] **Step 1: Add the schema field + parse test**

In `src/portfolio/momentum_universe.rs`, add to `TokenParams`:
```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regime_filter: Option<bool>,
```
Add a test in that file's `#[cfg(test)]`:
```rust
    #[test]
    fn token_params_parse_regime_filter() {
        let json = r#"[{"symbol":"A","mint":"A","params":{"regime_filter":false}},
                       {"symbol":"B","mint":"B","params":{"min_metric":0.05}},
                       {"symbol":"C","mint":"C"}]"#;
        let v: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        assert_eq!(v[0].params.as_ref().unwrap().regime_filter, Some(false)); // exempt
        assert_eq!(v[1].params.as_ref().unwrap().regime_filter, None);        // field absent
        assert!(v[2].params.is_none());                                       // no params
    }
```
Run: `cargo test --lib token_params_parse_regime_filter 2>&1 | tail -6` → after adding the field it passes (write it first to see it fail to compile, then add the field).

- [ ] **Step 2: Write the failing sim behavioral test**

Add to `src/portfolio/sim.rs` `#[cfg(test)]` (reuse helpers `rise_then_fall`/`aaa`/`bare_params`/`ranked_stream`):
```rust
    #[test]
    fn replay_multi_regime_exempt_token_enters_when_market_off() {
        // Single token, monotonic rise (always a buy candidate). Regime mask ALL FALSE
        // (market risk-off the whole time). Non-exempt → never enters; exempt → enters.
        let snaps = rise_then_fall("AAA", 200, 0);
        let watched_gated = aaa(); // no params → obeys global gate
        let mut watched_exempt = aaa();
        watched_exempt[0].params = Some(crate::portfolio::momentum_universe::TokenParams {
            min_metric: None, trail_pct: None, max_run_pct: None, regime_filter: Some(false),
        });
        let params = bare_params();
        let stream = ranked_stream(&snaps, &watched_gated, &params);
        let mask_off = vec![false; snaps.len()]; // market risk-off throughout

        let gated = replay_multi(&snaps, &watched_gated, &stream, &params, &mask_off, 1);
        let exempt = replay_multi(&snaps, &watched_exempt, &stream, &params, &mask_off, 1);
        // Force a close so the gated run's "never entered" vs exempt's "entered" is visible:
        // gated never opens a position (regime off, not exempt) → 0 entries reflected in MTM/trades.
        assert_eq!(gated.trades.len(), 0, "non-exempt token blocked by risk-off market");
        // exempt token entered (and rides to end → no closed trade, but it WAS held):
        // prove via MTM that exempt deployed capital while gated did not.
        let (_, mtm_gated) = replay_multi_mtm(&snaps, &watched_gated, &stream, &params, &mask_off, 1);
        let (_, mtm_exempt) = replay_multi_mtm(&snaps, &watched_exempt, &stream, &params, &mask_off, 1);
        let pool = params.trade_usdc;
        assert!(mtm_gated.iter().all(|&(_, e)| (e - pool).abs() < 1e-6), "gated: never deployed (flat at pool)");
        assert!(mtm_exempt.last().unwrap().1 > pool, "exempt: deployed + rode an unrealized gain");
    }
```
Run: `cargo test --lib replay_multi_regime_exempt 2>&1 | tail -10` → FAILS (exempt token still blocked, because the predicate doesn't yet honor regime_filter).

- [ ] **Step 3: Implement the per-candidate regime check in `replay_multi_core`**

In `src/portfolio/sim.rs`, near the top of `replay_multi_core` (where `last_exit_ts`/`entry_tss` are initialized), build the exempt set once:
```rust
    let regime_exempt: std::collections::HashSet<&str> = watched
        .iter()
        .filter(|w| w.params.as_ref().and_then(|p| p.regime_filter) == Some(false))
        .map(|w| w.mint.as_str())
        .collect();
```
Then in the entry section, replace the `if regime[i] { … }` wrapper so the loop runs every bar and the regime check moves into the candidate predicate. Change:
```rust
        pending_free.retain(|&f| f > i); // expire returned capacity (every bar, not only regime-on)
        if regime[i] {
            let withheld = pending_free.len();
            let mut capacity = max_positions.saturating_sub(held.len() + withheld);
            while capacity > 0 {
                …
                let best = stream[i].iter().find(|c| {
                    !c.stale
                        && !is_overextended(c.metrics.ret, max_run_for(&c.mint), c.slope_recent, c.slope_full)
                        && !c.falling
                        && !c.metric_fading
                        && c.score > min_metric_for(&c.mint)
                        && !held.iter().any(|p| p.mint == c.mint)
                        && last_exit_ts.get(&c.mint).is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
                });
                …
            }
        }
```
to (remove the `if regime[i]` wrapper + its closing brace; add the regime term as the FIRST conjunct):
```rust
        pending_free.retain(|&f| f > i); // expire returned capacity (every bar)
        let withheld = pending_free.len();
        let mut capacity = max_positions.saturating_sub(held.len() + withheld);
        while capacity > 0 {
            …
            let best = stream[i].iter().find(|c| {
                // Regime gate: global market mask, OR the token is regime-exempt
                // (params.regime_filter == false). No exempt tokens ⇒ identical to `if regime[i]`.
                (regime[i] || regime_exempt.contains(c.mint.as_str()))
                    && !c.stale
                    && !is_overextended(c.metrics.ret, max_run_for(&c.mint), c.slope_recent, c.slope_full)
                    && !c.falling
                    && !c.metric_fading
                    && c.score > min_metric_for(&c.mint)
                    && !held.iter().any(|p| p.mint == c.mint)
                    && last_exit_ts.get(&c.mint).is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
            });
            …
        }
```
Keep everything inside the `while` (the `used`/daily-cap check, `entry_dip` block, sizing, push) unchanged — just un-indent one level after removing the wrapper. The `if record_mtm { … }` MTM push that follows the entry block is unchanged.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib replay_multi_regime_exempt 2>&1 | tail -8` → PASS.
Run: `cargo test --lib 2>&1 | tail -4` → full suite green (no-exemption equivalence preserves the N=1 anchor + all existing tests).

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum_universe.rs src/portfolio/sim.rs
git commit -m "feat(sim): per-token regime opt-out (params.regime_filter) in replay_multi

A token with params.regime_filter=false is exempt from the global SOL regime gate
(stays eligible when the market is risk-off); the gate moves from an `if regime[i]`
wrapper into a per-candidate `(regime[i] || regime_exempt)` predicate. No exempt
tokens ⇒ byte-identical to today (N=1 anchor + suite unchanged).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: live `maybe_enter` consumption + docs

**Files:** Modify `src/portfolio/momentum.rs`, `.claude/skills/optimize-momentum-config/SKILL.md`; Test: `momentum.rs` `#[cfg(test)]`.

**Interfaces:**
- Consumes: `token_params_for` (existing SP3 resolver), `regime_risk_on`.
- Produces: `regime_exempt_for(watched, mint) -> bool`.

- [ ] **Step 1: Read the current `maybe_enter` regime gate**

Read `maybe_enter` (~line 1320) around the regime check: it computes `let (risk_on, diag) = regime_risk_on(...)` then `if !risk_on { return Ok(slow_tick_outcomes); }` — an early return that blocks ALL entries on a risk-off market. Note how it then ranks/selects candidates to fill free slots (the SP3 multi-slot fill loop).

- [ ] **Step 2: Add the resolver + per-candidate regime test**

Add near the other per-token resolvers in `momentum.rs`:
```rust
/// A token is regime-exempt (ignores the global SOL gate) iff params.regime_filter == false.
fn regime_exempt_for(watched: &[WatchedToken], mint: &str) -> bool {
    token_params_for(watched, mint).and_then(|p| p.regime_filter) == Some(false)
}
```
Add a unit test:
```rust
    #[test]
    fn regime_exempt_for_only_on_explicit_false() {
        let mk = |rf: Option<bool>| WatchedToken {
            symbol: "A".into(), mint: "A".into(), name: None, equity: None,
            params: Some(crate::portfolio::momentum_universe::TokenParams {
                min_metric: None, trail_pct: None, max_run_pct: None, regime_filter: rf }),
        };
        assert!(regime_exempt_for(&[mk(Some(false))], "A"));   // exempt
        assert!(!regime_exempt_for(&[mk(Some(true))], "A"));   // obey
        assert!(!regime_exempt_for(&[mk(None)], "A"));         // absent → obey
        assert!(!regime_exempt_for(&[mk(Some(false))], "Z"));  // unknown mint → obey
        let none = WatchedToken { symbol: "B".into(), mint: "B".into(), name: None, equity: None, params: None };
        assert!(!regime_exempt_for(&[none], "B"));             // no params → obey
    }
```
Run: `cargo test --lib regime_exempt_for_only_on_explicit_false 2>&1 | tail -6` → FAILS (function not defined), then passes after Step 3 adds it.

- [ ] **Step 3: Make the live regime gate per-candidate**

In `maybe_enter`, replace the blanket early-return:
```rust
    if !risk_on {
        return Ok(slow_tick_outcomes);
    }
```
with logic that keeps going when risk-off but restricts the candidate pool to regime-exempt tokens. Concretely, do NOT return early; instead, in the candidate-eligibility used by the slot-fill loop, require `(risk_on || regime_exempt_for(ctx.watched, &cand.mint))` alongside the existing per-candidate gates (score > `min_metric_for`, not stale/falling/over-extended, not held, off cooldown). Keep the `diag` log (it's informative). When `risk_on` is true OR a candidate is exempt, that candidate is regime-eligible; otherwise it's skipped — non-exempt tokens behave exactly as today (a fully risk-off market with no exempt tokens admits nobody, same as the old early return). Preserve all other gates and the multi-slot capacity logic unchanged.

> Implementer: mirror the sim's predicate. If the early-return is load-bearing for other reasons (e.g. skipping expensive work), keep a fast path: `if !risk_on && !ctx.watched.iter().any(|w| regime_exempt_for(ctx.watched, &w.mint)) { return Ok(slow_tick_outcomes); }` — i.e. still short-circuit when risk-off AND no token is exempt (the common case), preserving today's behavior exactly.

- [ ] **Step 4: Docs note**

In `.claude/skills/optimize-momentum-config/SKILL.md`, in the per-token step description, add one sentence: per-token `regime_filter` (opt out of the global SOL regime gate) is **operator-set** — the optimizer preserves it but never writes it (it only tunes `{min_metric, trail_pct, max_run_pct}`).

- [ ] **Step 5: Build + tests**

Run: `cargo build --release 2>&1 | tail -3` (compiles the live binary). `cargo test --lib 2>&1 | tail -4` (green).

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/momentum.rs .claude/skills/optimize-momentum-config/SKILL.md
git commit -m "feat(momentum): live maybe_enter honors per-token regime opt-out

regime_exempt_for resolver; the risk-off early-return becomes a per-candidate
`(risk_on || regime_exempt)` gate (with a fast-path short-circuit when risk-off and
no token is exempt → identical to today). SKILL.md notes regime_filter is operator-set.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:** schema `regime_filter` (T1); sim per-candidate gate (T1); live per-candidate gate + resolver (T2); operator-set/not-tuned doc (T2); backward-compat anchor (no-exemption equivalence in both T1 sim test + T2 fast-path). ✓
**2. Placeholder scan:** T1 gives complete schema + sim edits + tests. T2 gives the resolver + test in full and a precise transformation of the live gate (read-then-fold), with the exact fast-path to preserve today's behavior. No vague directives. ✓
**3. Type consistency:** `regime_filter: Option<bool>` defined T1, consulted in sim (`regime_exempt` set) T1 and live (`regime_exempt_for`) T2; `TokenParams` literal in tests includes all four fields (`min_metric, trail_pct, max_run_pct, regime_filter`) consistently. ✓

## Caveat (carried to the user)
On this universe the global regime gate has been net-negative (regime-off runs earned more), and per-token-own-trend gating is largely redundant with `min_metric`. This opt-out is a *capability* (exempt a chosen name from the market gate); whether it helps is an operator judgment to paper-test, not a proven edge.
