# Per-Token Regime Opt-Out — Design

**Date:** 2026-06-29
**Status:** Design approved (user). Spec finalized.
**Scope:** `momentum_universe.rs` (schema), `sim.rs` (`replay_multi` consumption),
`momentum.rs` (`maybe_enter` live consumption). Builds on the per-token params feature
(SP1/SP3).

## Problem & goal

The momentum regime filter is a single **global** gate derived from SOL
(`MOMENTUM_REGIME_MODE/OBS/TREND_MIN`): "only enter when the broad market is risk-on." It
gates **every** token's entries uniformly. The user wants a per-token **opt-out**: let a
specific token stay tradeable even when SOL is risk-off, without changing the gate for
everyone else.

Regime is an **entry gate (binary eligibility), not a score**, so making it per-token is
coherent at any `MOMENTUM_MAX_POSITIONS` — it never affects cross-token ranking or
weakest-green eviction (the reason per-token *metric* was rejected).

## Decisions (locked during brainstorming)

1. **Opt-out only.** A per-token `regime_filter: bool` (default `true` = obey the global
   gate). `false` ⇒ the token is **exempt** — regime-eligible even when SOL is risk-off. It
   never *forces* a gate onto a token the global config doesn't gate (when the global regime
   is off, the toggle is a no-op).
2. **Backward-compatible.** Absent / `true` ⇒ today's behavior. With no token exempt, every
   path is byte-identical to current (the regime-off bar still admits nobody).
3. **The regime mask stays global + SOL-derived.** Only *who consults it* changes. No
   per-token regime source/params (those were rejected as redundant/overfit).
4. **Operator-set, not auto-tuned.** `regime_filter` is a hand-set strategic toggle (like
   the global `REGIME_MODE`, which `optimize-momentum-config` reports but never flips).
   Neither `optimize_momentum.py` nor `per-token-tune` writes it.

## Schema (`src/portfolio/momentum_universe.rs`)

`TokenParams` gains:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub regime_filter: Option<bool>,
```

`momentum_tokens.json` example (exempt one token from the market gate):

```json
{ "symbol": "PUMP", "mint": "pump…", "name": "Pump",
  "params": { "regime_filter": false } }
```

Per-field optional, alongside `{min_metric, trail_pct, max_run_pct}`; a token may set only
`regime_filter`. Effective rule: `regime_exempt(mint) = token.params.regime_filter == Some(false)`.

## Sim consumption (`replay_multi` / `replay_multi_core` in `sim.rs`)

Today the entry section is:

```rust
if regime[i] {
    let mut capacity = …;
    while capacity > 0 { let best = stream[i].iter().find(|c| <gates>); … }
}
```

Change to run the entry loop **every** bar, folding the regime check into the per-candidate
predicate:

```rust
let mut capacity = …;
while capacity > 0 {
    let best = stream[i].iter().find(|c|
        (regime[i] || regime_exempt(c.mint))   // ← global gate OR token-exempt
        && <existing gates: !stale, !overextended, !falling, !metric_fading,
            !held, cooldown, score > min_metric_for(mint), …>);
    …
}
```

- `regime_exempt(mint)` is an O(1) lookup into a `HashSet<&str>` of exempt mints built once
  at entry (from `watched[*].params.regime_filter == Some(false)`).
- **Backward-compat:** with no exempt mints, `(regime[i] || false) == regime[i]`; on a
  risk-off bar the `find` returns `None` → no entry → byte-identical to today's `if
  regime[i]` skip. N=1 anchor + all existing `replay_multi`/`run_grid_multi` tests unchanged.
- The MTM push (when `record_mtm`) already sits after the entry block and runs every bar;
  removing the `if regime[i]` wrapper does not change that.
- The regime **mask** (`regime`, computed globally from SOL) is untouched.

## Live consumption (`maybe_enter` in `momentum.rs`)

Add a resolver mirroring the SP3 per-token resolvers:

```rust
fn regime_exempt_for(watched: &[WatchedToken], mint: &str) -> bool {
    token_params_for(watched, mint).and_then(|p| p.regime_filter) == Some(false)
}
```

In `maybe_enter`, the live regime gate currently blocks all entries when the market is
risk-off. Make it per-candidate: a candidate is regime-eligible if the global gate is
risk-on **or** `regime_exempt_for(ctx.watched, &cand.mint)`. Non-exempt tokens behave
exactly as today. At `MOMENTUM_MAX_POSITIONS=1` with no exemptions, identical to current.

## Not auto-tuned (docs)

`per-token-tune` and `optimize_momentum.py` do **not** emit `regime_filter` — they only
write `{min_metric, trail_pct, max_run_pct}`, preserving any operator-set `regime_filter`
on existing entries (the writer already preserves untouched fields/entries). A one-line note
in the optimize SKILL.md: `regime_filter` is operator-set, like the global regime stance.

## Testing

- **Schema:** parse an entry with `regime_filter:false`; with only `regime_filter`; with no
  `params` ⇒ `None`. Round-trip; `None` not serialized.
- **Resolver:** `regime_exempt` true only for `Some(false)`; `Some(true)`/`None`/unknown
  mint ⇒ not exempt.
- **Sim no-exemption equivalence:** a `replay_multi` run with a non-trivial regime mask and
  no exempt tokens produces a `SimRun` identical to the pre-change behavior (compare via the
  existing N=1 anchor and a 2-token run).
- **Sim opt-out behavioral:** construct a forced risk-off window (regime mask false over a
  span where a token would otherwise qualify). A token with `regime_filter:false` opens a
  position in that window; an identical non-exempt token does not.
- **Live:** `regime_exempt_for` resolver unit test (override vs default).

## Out of scope

- Per-token regime *source* (token's own trend) or per-token regime *params* (rejected:
  redundant / overfit).
- Forcing regime ON for a token when the global gate is off (opt-out only).
- Auto-tuning `regime_filter`.

## Success criterion

`momentum_tokens.json` accepts `params.regime_filter: false` to exempt a token from the
global SOL regime gate, in both the sim (`replay_multi`) and the live trader
(`maybe_enter`); a file with no exemptions reproduces today's behavior exactly (all existing
tests green); and the optimizer preserves but never writes the toggle.
