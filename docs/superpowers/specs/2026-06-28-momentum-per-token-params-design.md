# Per-Token Momentum Params — SP1: Schema, Loader, Sim Consumption — Design

**Date:** 2026-06-28
**Status:** Design approved (user); spec auto-finalized under delegated autonomy.
**Scope:** `momentum-sim` + the universe loader only (SP1 of 3). No optimize-config write
(SP2), no live trader (SP3).

## Context

This is sub-project 1 of a 3-part feature: per-token momentum parameters. The hypothesis:
the hold-all (N = #curated tokens) basket lost to single-slot in prior experiments because
**one global config** dragged laggards; giving each token its **own** optimized params may
rescue the basket (each name traded as its own single-name strategy, sharing capital).

Decomposition (each its own spec→plan→build):
- **SP1 (this):** per-token params in `momentum_tokens.json`, parsed by the loader, applied
  by the sim's `replay_multi` with global `.env` fallback. Adds the *capability* only.
- **SP2:** `optimize-momentum-config` computes + writes per-token best (JSON) and global
  best (`.env`); a validation verdict (per-token basket vs global basket vs single-slot).
- **SP3 (gated on SP2):** multi-position live trader consuming per-token params.

## Decisions (locked during brainstorming)

1. **Per-token-overridable params = `{min_metric, trail_pct, max_run_pct}`** (the
   token-specific knobs). `metric`, `lookback`, `regime`, and `rotate_margin` stay
   **global**. Rationale: keeping `metric`+`lookback` global means candidate scores stay
   comparable across tokens, so `ranked_stream` is built once (no rewrite) and the feature
   works at any N — not just hold-all.
2. **Per-field optional with global fallback.** Each of the three fields is independently
   optional; effective value = token override `??` global `ParamSet` value. A token may
   override just `trail_pct` and inherit the rest.
3. **"No overrides ⇒ identical behavior."** If a token has no `params` (or none in the
   file), its effective params equal the global for every token, so `replay_multi` is
   byte-identical to today. This is the regression guarantee — all existing tests + the
   N=1 anchor pass unchanged.
4. **SP1 adds capability only** — it does not populate params and does not touch the live
   trader. The `maxn-*` subcommands benefit automatically once params exist.

## Schema

`momentum_tokens.json` entries gain an optional `params` object:

```json
{
  "symbol": "ORCA",
  "mint": "orcaEKTdK7LKz57vaAYr9QeNsVEPfiu6QeMU1kektZE",
  "name": "Orca",
  "params": { "min_metric": 0.05, "trail_pct": 30.0, "max_run_pct": 0.0 }
}
```

New type in `src/portfolio/momentum_universe.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenParams {
    #[serde(default)] pub min_metric: Option<f64>,
    #[serde(default)] pub trail_pct: Option<f64>,
    #[serde(default)] pub max_run_pct: Option<f64>,
}
```

`WatchedToken` gains `#[serde(default)] pub params: Option<TokenParams>`. Serialize skips
`None` (`#[serde(skip_serializing_if = "Option::is_none")]`) so the add-token script and
SP2's writer don't litter the file with `null`s, and tokens without overrides round-trip
unchanged.

## Loader

`momentum_universe::load` is unchanged in logic (dedup, USDC drop, mint validation); the
new field deserializes automatically. No validation of param *values* at load time (a
nonsensical override is the operator's responsibility; SP2 writes only grid-derived
values). The existing tests still pass; add tests for params parsing + per-field default.

## Sim consumption (`replay_multi` in `src/portfolio/sim.rs`)

Build once at entry: `let token_params: HashMap<&str, &TokenParams>` keyed by mint (only
for tokens that have `Some(params)`). Define small resolvers that read the override or fall
back to the global `params: &ParamSet`:

- `min_metric_for(mint) -> f64`
- `trail_for(mint) -> f64`
- `max_run_for(mint) -> f64`

Apply at three sites:

- **Entry — `min_metric`:** the entry gate currently does `if best.score <= params.min_metric { break }`. With per-token thresholds the candidate pool is no longer monotone in "passes threshold," so the entry selection changes from "best candidate, then one global threshold check" to: **iterate eligible candidates best-score-first, skip any whose score ≤ its own `min_metric_for(mint)`, take the first that passes** (and isn't held, off cooldown, etc.). This preserves "fill the slot with the strongest qualifying name" while each name uses its own bar. (At N=#tokens every qualifying token gets a slot regardless of order.)
- **Exit — `trail_pct`:** each held position's stop uses `trail_for(pos.mint)` in the `vol_stop_triggered` / `profit_protected_stop_triggered` calls instead of the global `params.trail_pct`.
- **Entry guard — `max_run_pct`:** the candidate's precomputed `overextended` used the global `max_run`. When a token's `max_run_for(mint)` differs from the global, **recompute** over-extension for that candidate with its own value via the existing `is_overextended` helper over the token's recent price window; otherwise use the precomputed flag. `ranked_stream` stays built-once with global params.

Resolution is identical at N=1 (single held token uses its own params, or global if none),
so the N=1 anchor and all existing `replay_multi`/`run_grid_multi` tests are unaffected
when fixtures carry no overrides.

### Where global `params` still rules
`metric`, `lookback_obs`, `regime_*`, `rotate_margin`, `decel_lookback_min`,
`confirm_lag_obs`, `stale_minutes`, cooldown, daily cap, costs, sizing — all stay global
(they are cross-sectional or market-level, not token-specific).

## Testing

- **Loader:** parse an entry with `params` (all three fields); parse with a partial
  `params` (only `trail_pct`); parse with no `params` → `None`; a token without overrides
  serializes back without a `params` key.
- **Effective resolution:** a helper-level test that override wins and `None` falls back to
  global, per field.
- **No-override equivalence:** `replay_multi` over a fixture whose tokens carry no overrides
  produces a `SimRun` identical to the pre-SP1 behavior (compare against `replay_with_regime`
  at N=1 — the existing anchor — and a 2-token N=2 run).
- **`trail_pct` override behavioral:** two-token rise-then-fall fixture; give token A a
  *tight* `trail_pct` and token B a *wide* one (or the global). A exits earlier than B —
  proves the per-token trail is wired to the right position.
- **`min_metric` override behavioral:** raising a token's `min_metric` above its observed
  scores suppresses its entries (fewer/zero trades for that token) while an un-overridden
  token still trades.
- **`max_run_pct` override behavioral:** a token whose `max_run_for` is small is gated out
  as over-extended at a point where the global (larger) `max_run` would have allowed entry.

## Out of scope (SP1)

- Writing per-token params (SP2) and the validation verdict (SP2).
- Any live-trader change (SP3).
- Per-token `metric`/`lookback`/`regime`/`rotate` (deliberately global — see Decision 1).
- Value validation of overrides at load time.

## Success criterion

`momentum_tokens.json` accepts optional per-token `{min_metric, trail_pct, max_run_pct}`;
the loader parses them; `replay_multi` applies each token's effective params (override ??
global) at the entry-threshold, trailing-stop, and over-extension sites; and a file with no
overrides reproduces today's behavior exactly (all existing tests green). This enables SP2
to populate params and measure whether per-token tuning rescues the basket.
