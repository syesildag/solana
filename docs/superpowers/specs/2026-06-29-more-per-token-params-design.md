# More Per-Token Params: trade_usdc, exit_on_fade, reentry_cooldown_secs — Design

**Date:** 2026-06-29
**Status:** Design approved (user). Spec finalized.
**Scope:** `momentum_universe.rs` (schema), `sim.rs` (`replay_multi_core` + `tune_per_token`),
`momentum.rs` (`maybe_enter`/`maybe_take_profit_on_fade` + resolvers), `momentum_sim.rs`
(`per_token_tune` output), optimize SKILL.md. Builds on the existing per-token params
(`min_metric`, `trail_pct`, `max_run_pct`, `regime_filter`).

## Problem & goal

Extend per-token overrides with three more **live-executable** params: position size
(`trade_usdc`), fade-exit toggle (`exit_on_fade`), and re-entry cooldown
(`reentry_cooldown_secs`). Only live-executable params are added — `max_hold_min`,
`max_trail_pct`, `overbought_z`, `breakeven_exit`, and vol-stops are **sim-only** (0 uses in
the live trader) and are deliberately excluded (they'd be dead knobs the live bot ignores).

## Decisions (locked during brainstorming)

1. **Three new per-token fields:** `trade_usdc: Option<f64>`, `exit_on_fade: Option<bool>`,
   `reentry_cooldown_secs: Option<i64>`. All optional, per-field fallback to global.
2. **`trade_usdc` overrides the slot size.** Single-slot: replaces global
   `MOMENTUM_TRADE_USDC`; multi-slot: replaces that token's `pool/N` share. Total deployed
   ≠ pool when overrides exist — the operator's explicit sizing choice. Applies only at
   **entry-sizing** sites (buy amount + that entry's gas/balance); portfolio-level uses
   (adoption threshold) stay global.
3. **Auto-tune `exit_on_fade` + `reentry_cooldown_secs`; `trade_usdc` operator-set.**
   `per-token-tune` sweeps `exit_on_fade ∈ {true,false}` and `reentry_cooldown_secs ∈ {small
   ladder}` per token and writes the winners; `trade_usdc` is never written by the tuner
   (auto-tuning position magnitude overfits a finite sample and muddies equal-capital
   comparisons) — but the tuner **preserves** any operator-set `trade_usdc`.
4. **Backward-compatible.** No overrides ⇒ all three resolvers return the global ⇒
   byte-identical to today (the existing N=1 / no-exemption anchors hold).

## Schema (`src/portfolio/momentum_universe.rs`)

`TokenParams` gains (each `#[serde(default, skip_serializing_if = "Option::is_none")]`):
```rust
pub trade_usdc: Option<f64>,
pub exit_on_fade: Option<bool>,
pub reentry_cooldown_secs: Option<i64>,
```
Example: `"params": { "min_metric": 0.04, "trail_pct": 30, "max_run_pct": 15, "trade_usdc": 250, "exit_on_fade": false, "reentry_cooldown_secs": 1800 }`.

## Resolvers (sim + live)

```rust
fn trade_usdc_for(watched, mint, global: f64) -> f64
fn exit_on_fade_for(watched, mint, global: bool) -> bool
fn reentry_cooldown_for(watched, mint, global: i64) -> i64
```
Each = the token's override `??` the global. (Same `token_params_for` lookup the existing
resolvers use.)

## Consumption

### Sim (`replay_multi_core` in `sim.rs`)
- **Entry size:** `dynamic_trade_usdc(trade_usdc_for(&c.mint, params.trade_usdc), params.reinvest_frac, params.size_ceiling_usdc, realized)` (and the gas/cost computed on that size). Per selected candidate.
- **Fade exit:** the per-position fade loop's guard becomes `exit_on_fade_for(&pos.mint, params.exit_on_fade)`.
- **Cooldown:** the entry-eligibility cooldown check uses `reentry_cooldown_for(&c.mint, params.reentry_cooldown_secs)`.

### Live (`momentum.rs`)
- **`maybe_enter` entry-sizing** (`:1568–1687` block — `usdc_raw`, `gas_bps`, `entry_basis`,
  `usdc_in`, `usdc_spent`, the log) and the **balance gate** (`:1420`): use
  `trade_usdc_for(&best.mint, cfg.momentum_trade_usdc)` (per selected candidate in the
  multi-slot fill loop). Adoption threshold (`:1138`) stays global.
- **`maybe_take_profit_on_fade`** (`:2064` `if !cfg.momentum_exit_on_fade`): use
  `exit_on_fade_for(ctx.watched, &pos.mint, cfg.momentum_exit_on_fade)`.
- **Cooldown checks** (`:1477` and the `reentry_cooldown` args at `:1494`/`:1750`): use
  `reentry_cooldown_for(ctx.watched, &<candidate>.mint, cfg.momentum_reentry_cooldown_secs)`
  for the candidate being (re)entered.

## Optimizer (`per_token_tune` in `momentum_sim.rs` + `tune_per_token` in `sim.rs`)

`tune_per_token` gains an outer sweep, per token, over `exit_on_fade ∈ {true,false}` and
`reentry_cooldown_secs ∈` a **small ladder** (e.g. `[300, 1800, 3600]` plus the .env
default), in addition to the existing `{min_metric, trail, max_run}` grid and the
gated/exempt regime arm. For each token it picks the combination with the best robust
held-out P&L and emits the winning `exit_on_fade` + `reentry_cooldown_secs` in its
`TokenParams`. **`trade_usdc` is NOT swept** — left `None` so the writer preserves any
operator-set value. Keep the ladder small (≤4 values × 2 fade) to bound grid cost + overfit.
`per_token_tune` prints the chosen `exit_on_fade`/`cooldown` per token; the JSON writer
already serializes `TokenParams` (new fields written when `Some`, skipped when `None`).

## Backward-compat / anchors

No overrides ⇒ `trade_usdc_for`/`exit_on_fade_for`/`reentry_cooldown_for` all return the
global ⇒ sim and live are byte-identical to today. The existing replay/run_grid/N=1 anchors
and the full suite stay green.

## Testing

- **Schema:** parse all three new fields (full + partial + absent); round-trip; `None` not serialized.
- **Resolvers:** override vs global vs unknown-mint, per field.
- **Sim behavioral:** (a) a token with `trade_usdc` override opens a differently-sized
  position (assert token_amount/usdc_spent scales with the override); (b) `exit_on_fade:false`
  → that token never fade-exits (only trailing stop closes it); (c) a larger
  `reentry_cooldown_secs` delays that token's re-entry vs a token without.
- **No-override equivalence:** a `replay_multi` run with no overrides == pre-change `SimRun`.
- **Optimizer:** a token where `exit_on_fade:false` (or a non-default cooldown) robustly
  wins gets it written; a token where the default wins emits `None` for that field;
  `trade_usdc` is never emitted by the tuner (and an operator-set `trade_usdc` survives a
  tune+write).
- **Live:** resolver unit tests; build.

## Decomposition (3 tasks)

- **T1:** schema (+3 fields) + sim consumption (3 resolvers + wiring in `replay_multi_core`) + sim tests.
- **T2:** live consumption (`maybe_enter` entry-size + balance gate; fade gate; cooldown) + resolver tests.
- **T3:** optimizer auto-tune `exit_on_fade` + `reentry_cooldown_secs` in `tune_per_token`/`per_token_tune` (`trade_usdc` operator-set) + SKILL.md note + test.

## Out of scope

- Sim-only params (`max_hold_min`, `max_trail_pct`, `overbought_z`, `breakeven_exit`,
  vol-stops) — the live trader doesn't read them; adding them per-token would be dead knobs.
- Auto-tuning `trade_usdc` (operator-set; a vol-scaling sizing rule is possible future work).
- Changing how `trade_usdc` composes with the equal-capital comparison beyond "override
  replaces the slot share."

## Success criterion

`momentum_tokens.json` accepts per-token `trade_usdc`, `exit_on_fade`, and
`reentry_cooldown_secs`; both the sim (`replay_multi`) and live trader (`maybe_enter` /
fade / cooldown) honor them with global fallback at any `MOMENTUM_MAX_POSITIONS`;
`per-token-tune` auto-tunes `exit_on_fade` + cooldown (and preserves operator-set
`trade_usdc`); and a file with no overrides reproduces today's behavior exactly.
