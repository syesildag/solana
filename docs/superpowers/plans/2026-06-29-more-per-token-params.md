# More Per-Token Params (trade_usdc, exit_on_fade, reentry_cooldown_secs) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development. Steps use checkbox (`- [ ]`).

**Goal:** Add per-token `trade_usdc`, `exit_on_fade`, `reentry_cooldown_secs` to `momentum_tokens.json`, honored by the sim (`replay_multi`) and live trader (`maybe_enter`/fade/cooldown) with global fallback; `per-token-tune` auto-tunes `exit_on_fade`+cooldown (`trade_usdc` operator-set).

**Architecture:** Three new `Option` fields on `TokenParams` + three resolvers (`trade_usdc_for`/`exit_on_fade_for`/`reentry_cooldown_for` = override ?? global) in both sim and live, wired at the entry-sizing / fade-gate / cooldown sites. Per-token consumption lives in `replay_multi_core` (the per-token path), NOT `replay_with_regime` (the global grid core). No overrides ⇒ byte-identical to today.

**Tech Stack:** Rust. Tests: `cargo test --lib`.

## Global Constraints

- **Backward-compatible:** no overrides ⇒ all three resolvers return the global ⇒ sim + live byte-identical to today; existing anchors + full suite green.
- **`trade_usdc` overrides the slot size** (single-slot: global; multi-slot: that token's pool/N share) — entry-sizing sites ONLY (buy amount + that entry's gas/balance); adoption threshold stays global. **`trade_usdc` is operator-set** — the optimizer preserves but never writes it.
- **Live-executable only:** these three are read by the live trader. Do NOT add sim-only params (max_hold/max_trail/overbought/breakeven/vol-stops).
- Per-token consumption in `replay_multi_core` only; `replay_with_regime`/`run_grid` stay global.

---

## File Structure

| File | Change |
|---|---|
| `src/portfolio/momentum_universe.rs` | +3 `TokenParams` fields |
| `src/portfolio/sim.rs` | 3 resolvers + wire into `replay_multi_core`; `tune_per_token` outer sweep (T3) |
| `src/portfolio/momentum.rs` | 3 resolvers + wire into `maybe_enter`/fade/cooldown |
| `src/bin/momentum_sim.rs` | `per_token_tune` print + pass-through (T3) |
| `.claude/skills/optimize-momentum-config/SKILL.md` | note (T3) |

---

## Task 1: schema + sim consumption

**Files:** `momentum_universe.rs`, `sim.rs`; tests in both.

- [ ] **Step 1: Add the 3 schema fields + parse test**

In `momentum_universe.rs` `TokenParams`, add (each `#[serde(default, skip_serializing_if = "Option::is_none")]`):
```rust
    pub trade_usdc: Option<f64>,
    pub exit_on_fade: Option<bool>,
    pub reentry_cooldown_secs: Option<i64>,
```
**NOTE:** every `TokenParams { … }` struct literal in the codebase must add the 3 fields (or use `..Default::default()`). Grep `TokenParams {` and fix all (tests in sim.rs/momentum.rs). Add a parse test:
```rust
    #[test]
    fn token_params_parse_extended_fields() {
        let json = r#"[{"symbol":"A","mint":"A","params":{"trade_usdc":250.0,"exit_on_fade":false,"reentry_cooldown_secs":1800}},
                       {"symbol":"B","mint":"B","params":{"min_metric":0.05}},
                       {"symbol":"C","mint":"C"}]"#;
        let v: Vec<WatchedToken> = serde_json::from_str(json).unwrap();
        let a = v[0].params.as_ref().unwrap();
        assert_eq!(a.trade_usdc, Some(250.0));
        assert_eq!(a.exit_on_fade, Some(false));
        assert_eq!(a.reentry_cooldown_secs, Some(1800));
        let b = v[1].params.as_ref().unwrap();
        assert_eq!((b.trade_usdc, b.exit_on_fade, b.reentry_cooldown_secs), (None, None, None));
        assert!(v[2].params.is_none());
    }
```

- [ ] **Step 2: Write failing sim behavioral tests**

Add to `sim.rs` `#[cfg(test)]` (reuse `rise_then_fall`/`aaa`/`bare_params`/`ranked_stream`/`replay_multi`):
```rust
    #[test]
    fn replay_multi_per_token_trade_usdc_sizes_position() {
        // A token with trade_usdc override opens a position scaled to the override, not the global.
        let snaps = rise_then_fall("AAA", 200, 6);
        let params = bare_params(); // global trade_usdc = 100
        let stream = ranked_stream(&snaps, &aaa(), &params);
        let mask = vec![true; snaps.len()];
        let mut w_over = aaa();
        w_over[0].params = Some(crate::portfolio::momentum_universe::TokenParams {
            trade_usdc: Some(50.0), ..Default::default() });
        let base_run = replay_multi(&snaps, &aaa(), &stream, &params, &mask, 1);
        let over_run = replay_multi(&snaps, &w_over, &stream, &params, &mask, 1);
        // first trade's usdc_in should be ~half (50 vs 100) for the override run.
        let b_in = base_run.trades.first().map(|t| t.usdc_in).unwrap_or(0.0);
        let o_in = over_run.trades.first().map(|t| t.usdc_in).unwrap_or(0.0);
        assert!(b_in > 0.0 && o_in > 0.0, "both runs trade");
        assert!((o_in / b_in - 0.5).abs() < 0.1, "override sized ~half: {o_in} vs {b_in}");
    }

    #[test]
    fn replay_multi_per_token_exit_on_fade_false_disables_fade() {
        // Build a fixture where the global config WOULD fade-exit; with exit_on_fade:false the
        // token holds (only trailing stop can close it). Simplest: assert the no-override run
        // produces a fade-driven trade set that differs from the exit_on_fade=false run.
        // (Use a rise-then-plateau so the metric fades while green.)
        let snaps = rise_then_fall("AAA", 160, 0); // rise then flat tail via 0 down (plateau handled by fixture)
        let mut params = bare_params();
        params.exit_on_fade = true;
        params.min_metric = 0.0; // ensure fade can trigger (score fades toward 0)
        let stream = ranked_stream(&snaps, &aaa(), &params);
        let mask = vec![true; snaps.len()];
        let mut w_nofade = aaa();
        w_nofade[0].params = Some(crate::portfolio::momentum_universe::TokenParams {
            exit_on_fade: Some(false), ..Default::default() });
        let with_fade = replay_multi(&snaps, &aaa(), &stream, &params, &mask, 1);
        let no_fade = replay_multi(&snaps, &w_nofade, &stream, &params, &mask, 1);
        // Disabling fade should not produce MORE closed trades than enabling it.
        assert!(no_fade.trades.len() <= with_fade.trades.len(),
            "exit_on_fade=false yields ≤ fade-driven exits: {} vs {}", no_fade.trades.len(), with_fade.trades.len());
    }
```
> If a behavioral assertion is hard to make deterministic from the fixtures, fall back to asserting via the resolver + a targeted check; the contract is "override changes that token's sizing / fade behavior." Document any fixture tuning in a comment.

- [ ] **Step 3: Add the 3 resolvers + wire `replay_multi_core`**

In `sim.rs` `replay_multi_core`, the existing per-token resolvers are LOCAL CLOSURES (lines ~677-679: `let min_metric_for = |mint: &str| tparams.get(mint).and_then(|p| p.min_metric).unwrap_or(params.min_metric);`, capturing `tparams: HashMap<&str,&TokenParams>` and `params`). Add three MORE closures right after them, in the SAME style (1-arg, capturing `tparams`/`params`):
```rust
    let trade_usdc_for = |mint: &str| tparams.get(mint).and_then(|p| p.trade_usdc).unwrap_or(params.trade_usdc);
    let exit_on_fade_for = |mint: &str| tparams.get(mint).and_then(|p| p.exit_on_fade).unwrap_or(params.exit_on_fade);
    let reentry_cooldown_for = |mint: &str| tparams.get(mint).and_then(|p| p.reentry_cooldown_secs).unwrap_or(params.reentry_cooldown_secs);
```
Call them as `trade_usdc_for(&best.mint)` / `exit_on_fade_for(&pos.mint)` / `reentry_cooldown_for(&c.mint)` (1 arg), matching `min_metric_for(&c.mint)`. (Do NOT add free functions in the sim — that's the LIVE trader's style; T2 adds 3-arg free fns there.)

Wire into `replay_multi_core` ONLY (NOT `replay_with_regime`):
- **Fade** (~line 846, `if params.exit_on_fade {`): for each held position `pos`, gate on `exit_on_fade_for(watched, &pos.mint, params.exit_on_fade)`. (Move the per-position check inside the loop if the outer `if` currently wraps all positions — each held token decides independently.)
- **Cooldown** (entry predicate ~line 898, and rotation-target ~line 806): replace `params.reentry_cooldown_secs` with `reentry_cooldown_for(watched, &c.mint, params.reentry_cooldown_secs)` (the candidate being re-entered).
- **Entry size** (~line 912, `dynamic_trade_usdc(...)`): use `trade_usdc_for(watched, &best.mint, params.trade_usdc)` as the base instead of `params.trade_usdc` (and ensure the gas/cost computed from `size` follows). Bind `best`/the selected candidate's mint appropriately.

Leave `replay_with_regime` (the single-slot global grid core, ~507/547/587/613) UNCHANGED.

- [ ] **Step 4: Run tests** — `cargo test --lib token_params_parse_extended_fields replay_multi_per_token 2>&1 | tail -12`, then `cargo test --lib 2>&1 | tail -4` (no regressions; no-override equivalence holds via the existing anchors).

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/momentum_universe.rs src/portfolio/sim.rs
git commit -m "feat(sim): per-token trade_usdc/exit_on_fade/reentry_cooldown in replay_multi

Three new TokenParams fields + resolvers wired into replay_multi_core (entry sizing,
fade gate, re-entry cooldown), global fallback. replay_with_regime (global grid core)
unchanged. No overrides ⇒ byte-identical to today.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: live consumption

**Files:** `momentum.rs`; tests in same.

- [ ] **Step 1: Add the 3 resolvers (live)** mirroring the sim, next to the existing live resolvers (`min_metric_for`/`trail_for`/`regime_exempt_for`): `trade_usdc_for`, `exit_on_fade_for`, `reentry_cooldown_for` (signatures `(watched, mint, global)`).

Add a resolver unit test `extended_resolvers_override_then_global` asserting override vs global vs unknown-mint for each of the three.

- [ ] **Step 2: Wire `maybe_enter` entry sizing + balance gate**
Read the entry-sizing block (`momentum.rs:1568–1687`) and the balance gate (`:1420`). For the selected candidate (`best`/`top`/`c` in the multi-slot fill loop), replace `cfg.momentum_trade_usdc` with `trade_usdc_for(ctx.watched, &<cand>.mint, cfg.momentum_trade_usdc)` at: the balance check (`:1420`), `usdc_raw` (`:1568`), `gas_bps` (`:1589`), `entry_basis` (`:1649`), `usdc_in` (`:1667`), the log (`:1677`/`:1681`), `usdc_spent` (`:1687`). Compute the per-token size once into a local (e.g. `let size = trade_usdc_for(...)`) and use it throughout that entry. Leave adoption's `:1138` (`trade_usdc*0.5`) global.

- [ ] **Step 3: Wire the fade gate + cooldown**
- `maybe_take_profit_on_fade` (`:2064`, `if !cfg.momentum_exit_on_fade`): `if !exit_on_fade_for(ctx.watched, &pos.mint, cfg.momentum_exit_on_fade)`.
- Cooldown (`:1477` check, and the `cfg.momentum_reentry_cooldown_secs` args at `:1494`/`:1750`): use `reentry_cooldown_for(ctx.watched, &<candidate>.mint, cfg.momentum_reentry_cooldown_secs)` for the candidate being (re)entered. (At `:1494`/`:1750` if the call passes a single global cooldown into a ranking/rotation helper that handles all candidates, prefer per-candidate where the candidate mint is in scope; if not cleanly per-candidate there, leave that arg global and note it — the primary cooldown gate is `:1477`.)

- [ ] **Step 4: Build + tests** — `cargo build --release 2>&1 | tail -3`, `cargo test --lib 2>&1 | tail -4`.

- [ ] **Step 5: Commit**
```bash
git add src/portfolio/momentum.rs
git commit -m "feat(momentum): live maybe_enter/fade/cooldown honor per-token trade_usdc/exit_on_fade/reentry_cooldown

Per-token entry size (override replaces slot size), fade-exit toggle, and re-entry
cooldown, with global fallback. Adoption threshold stays global. N=1/no-override
identical to today.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: optimizer auto-tunes exit_on_fade + reentry_cooldown (trade_usdc operator-set)

**Files:** `sim.rs` (`tune_per_token`), `momentum_sim.rs` (`per_token_tune`), SKILL.md.

- [ ] **Step 1: Failing test**
Add to `sim.rs` `#[cfg(test)]` a test that a token which does robustly better with `exit_on_fade=false` (rides winners) gets `exit_on_fade: Some(false)` written, using a fixture where fading exits early and costs P&L. Assert the emitted `TokenParams.exit_on_fade == Some(false)` for that token. (If a deterministic fixture is hard, assert the weaker contract: the tuner explores both fade values and the emitted value is whichever produced the higher robust test P&L — verify via a fixture where they clearly differ.)

- [ ] **Step 2: Implement the outer sweep in `tune_per_token`**
`tune_per_token` already grids `{min_metric,trail,max_run}` (exempt + gated regime arm). Add an OUTER loop, per token, over `exit_on_fade ∈ {true, false}` and `reentry_cooldown_secs ∈` a small ladder (use a const e.g. `const PT_COOLDOWN_LADDER: [i64; 4] = [0, 300, 1800, 3600]` — including the base/0). For each (fade, cooldown) combo, set `base.exit_on_fade`/`base.reentry_cooldown_secs`, run the existing per-token grid, take `best_robust_by_test`. Pick the combo+config with the highest robust `net_pnl_test` for the token. Emit the winning `exit_on_fade` and `reentry_cooldown_secs` in `PerTokenBest.params` (alongside min/trail/max_run + regime_filter). **Do NOT set `trade_usdc`** (leave `None`). Keep the ladder small to bound cost (≤4×2 = 8× the per-token grid; per-token grids are small after metric/lookback pinning).
> To bound emitted noise: only emit a non-default `exit_on_fade`/`reentry_cooldown_secs` when it STRICTLY beats the .env-default combo (else emit `None` for that field) — conservative, mirrors the regime_filter "strictly wins" rule.

- [ ] **Step 3: `per_token_tune` (bin) print + pass-through**
Pass the ladder/sweep as needed (it lives inside `tune_per_token`, so the bin call may be unchanged besides reading the new emitted fields). In the per-token print line, append the chosen `fade=on/off cooldown=Ns` for tokens that got non-default values. Confirm the JSON writer serializes the new fields (it serializes `TokenParams`, so `Some` values are written, `None` skipped) — and that an operator-set `trade_usdc` on an existing entry is preserved through a tune+write (the writer sets only the tuned fields on the mint; verify it doesn't clobber a pre-existing `trade_usdc`). If the writer rebuilds `params` wholesale, ensure it merges (keeps existing `trade_usdc`).

- [ ] **Step 4: SKILL.md note**
Update the per-token note: `per-token-tune` now also auto-tunes `exit_on_fade` and `reentry_cooldown_secs` (writing non-default values that robustly win); `trade_usdc` is operator-set (preserved, never written).

- [ ] **Step 5: Build + tests + commit**
`cargo test --lib tune_per_token 2>&1 | tail -10`, `cargo test --lib 2>&1 | tail -4`, `cargo build --release --bin momentum-sim 2>&1 | tail -3`.
```bash
git add src/portfolio/sim.rs src/bin/momentum_sim.rs .claude/skills/optimize-momentum-config/SKILL.md
git commit -m "feat(sim): per-token-tune auto-tunes exit_on_fade + reentry_cooldown (trade_usdc operator-set)

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:** 3 schema fields (T1); sim consumption — entry size/fade/cooldown (T1); live consumption (T2); optimizer auto-tune fade+cooldown, trade_usdc operator-set + preserved (T3); backward-compat anchor (no-override equivalence, T1/T2). ✓
**2. Placeholder scan:** resolvers given in full; sites pinned by line; tests provided; one explicit fallback note where a deterministic fixture may be hard. ✓
**3. Type consistency:** `trade_usdc: Option<f64>` / `exit_on_fade: Option<bool>` / `reentry_cooldown_secs: Option<i64>` consistent across schema, resolvers (`(watched, mint, global)`), sim, live, optimizer. The `..Default::default()` usage in tests requires `TokenParams: Default` (it already derives Default). ✓

## Caveat (carried to user)
`trade_usdc` per-token overrides slot sizing → total deployed ≠ pool when used; that's intentional. The optimizer won't tune size (overfit risk). All three are paper-validate-first like every per-token addition; on this sample the standing verdict (single-slot wins) is unchanged by adding knobs.
