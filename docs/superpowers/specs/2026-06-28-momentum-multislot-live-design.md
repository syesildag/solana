# Multi-Slot Live Momentum Trader — SP3 — Design

**Date:** 2026-06-28
**Status:** Design approved (user). Spec finalized.
**Scope:** The live momentum trader (`src/portfolio/{momentum,momentum_state}.rs`,
`watcher.rs`, `mod.rs`). Sim already supports multi-position (SP1/SP2). Live trader
**consumes** per-token params (SP1). The `optimize-momentum-config` global+per-token
wiring is **SP4** (separate follow-on).

## Problem & goal

The live momentum trader holds exactly one position (`TraderState.position:
Option<Position>` — "the type cannot represent two open positions"). The user wants a
**multi-slot** live trader: hold up to `N` concurrent positions, each tuned by its own
per-token params (with global fallback), mirroring the sim's `replay_multi` (incl.
weakest-green eviction). Built as opt-in infrastructure; the prior validation gate found
single-slot still wins P&L on the current sample, so this ships **default-off** and should
be **paper-traded first**.

## Decisions (locked during brainstorming)

1. **`MOMENTUM_MAX_POSITIONS` env, default `1`.** At `1` the trader is **behaviorally
   identical** to today (the live N=1 anchor). Multi-slot is opt-in.
2. **Fixed `MOMENTUM_TRADE_USDC` notional per slot.** Total deployed scales with N (matches
   the sim and the current per-entry sizing).
3. **Weakest-green eviction (full sim parity).** When all N slots are full and
   `MOMENTUM_ROTATE_MARGIN>0`, sell the weakest net-green held and buy the stronger
   candidate (an A→B rotation), mirroring `replay_multi`'s eviction. `rotate_margin==0` ⇒
   pure fill-and-hold.
4. **Per-token params consumed** from `WatchedToken.params` (SP1) at the entry threshold
   (`min_metric`), trailing stop (`trail_pct`), and over-extension gate (`max_run_pct`),
   each falling back to the global config when absent.
5. **Reuse existing per-slot execution.** Multi-slot calls the existing
   `submit_and_confirm` / `flatten_position` / entry & rotation swap builders once per
   slot — no new on-chain primitives.
6. **`DRY_RUN_MOMENTUM_TRADER` still gates submission** — paper-first.
7. **Per-tick order = exits (all held) → eviction (if full) → entries (fill free slots)** —
   the same order `replay_multi` uses, so paper results match the sim's hold-all run.

## State model (`momentum_state.rs`)

- `TraderState.position: Option<Position>` → **`positions: Vec<Position>`** (length ≤ N,
  deduped by mint).
- **Legacy migration on load (restart-safety):** keep reading a legacy `position` field via
  `#[serde(default)]`; in `load()`, if `positions` is empty and a legacy `position` is
  `Some`, move it into `positions` (so a running single-slot trader upgrades in place). The
  legacy field is not serialized going forward (`positions` is the source of truth).
- Helpers: `entries_last_24h` counts entries across `positions` + recent closed trades;
  `held_mints()`; `capacity(max_positions)` = `max_positions − positions.len()`;
  `position_for(mint)`. `last_exit_ts_per_mint` / `exit_attempts_per_mint` / `entry_attempt`
  / `trades` are unchanged (already per-mint or global). Atomic temp+rename save unchanged.

## Decision logic (`momentum.rs`)

All generalize the single-`Option` site to iterate `positions`, consuming per-token params:

- **`maybe_exit`** — evaluate **every** held position against its **per-token** trailing
  stop / fade / breakeven / max-hold; close those that trip (one `flatten_position` per
  exit). Returns the outcomes (the watcher applies each to the portfolio). At N=1 with the
  single held position, identical to today.
- **`maybe_enter`** — compute `capacity = N − held`; while capacity>0 and the daily cap
  allows, take the best eligible candidate (per-token `min_metric` threshold + per-token
  `max_run` over-extension, not stale/falling/fading, not held, off cooldown), enter it
  (one entry swap), decrement capacity. At N=1, fills the single slot exactly as today.
- **Eviction (generalize `try_rotate`)** — when `held==N` and `rotate_margin>0`, pick the
  weakest net-green held (lowest current score among green, priced, non-stale); if the best
  non-held candidate beats it by `rotate_margin` (and it's net-green after cost), rotate
  A→B (the existing rotation swap). At N=1 this is exactly today's `try_rotate`.
- **`reconcile_startup_position` / `adopt_wallet_position`** — adopt up to N existing wallet
  positions on startup (currently adopts one). Each adopted position becomes a slot.

Per-token resolution mirrors the sim: `min_metric_for(mint)`, `trail_for(mint)`,
`max_run_for(mint)` = override `??` global.

## Watcher control flow (`watcher.rs`)

Today: FLAT → slow-tick `maybe_enter`; HOLDING → fast-poll `maybe_exit`. Multi-slot
generalizes to **partially-filled** states:

- **Fast poll (every `MOMENTUM_POLL_SECS`):** run `maybe_exit` over all held positions
  (and the eviction check when full). Apply each `Exited`/`Rotated` outcome to the
  in-memory portfolio.
- **Slow tick (60s):** if `capacity>0`, run `maybe_enter` to fill free slots. Apply each
  `Entered` outcome.
- The branch is no longer "FLAT vs HOLDING" but "has free capacity" (enter) and "has held
  positions" (exit) — both can be true. `maybe_enter`/`maybe_exit` return `Vec<TradeOutcome>`
  (was `Option`) so a tick can produce multiple fills; the watcher loops over them.

## Config (`mod.rs`)

`momentum_max_positions: usize = parse_env("MOMENTUM_MAX_POSITIONS", 1)`. Clamped to ≥1.
Documented in `.env.example` and CLAUDE.md (env table).

## Capital & guards

- Each entry sizes `MOMENTUM_TRADE_USDC` (per slot). LIVE entry still requires
  `usdc_balance ≥ trade_usdc` per entry; paper spends nothing.
- The dual-guard halt (P&L drawdown 2-strike + SOL gas floor) reads aggregate state
  (realized P&L across all `positions` + closed trades; gas floor unchanged). Halt
  short-circuits the whole tick as today.
- Daily entry cap and per-mint re-entry cooldown shared across slots.

## N=1 equivalence (the safety anchor)

At `MOMENTUM_MAX_POSITIONS=1` and no per-token overrides, every generalized path reduces to
the current single-slot behavior: one slot, one exit check, `try_rotate` on the single
held, one entry when flat. Legacy state migrates so a live trader upgrades without losing
its open position. This is the live analogue of the sim's proven N=1 anchor and the basis
for shipping default-off.

## Testing

- **State migration:** a legacy state file with `"position": {…}` loads into
  `positions[0]`; `"position": null` → empty; round-trips as `positions`. Dedup + cap
  invariants hold.
- **Pure decision helpers (unit):** `capacity`, weakest-green selection, per-token resolver
  (`min_metric_for`/`trail_for`/`max_run_for`), `entries_last_24h` across multiple
  positions. Reuse the existing pure-helper test style in `momentum.rs`.
- **N=1 equivalence:** with `max_positions=1`, the multi-slot helpers pick the same
  position/candidate the single-slot code would (assert on the pure selection helpers; the
  async execution paths are unchanged per-slot).
- **Dry-run smoke:** run the watcher in `DRY_RUN_MOMENTUM_TRADER=true` with
  `MOMENTUM_MAX_POSITIONS=3` over recent history and confirm it opens up to 3 paper
  positions, exits them on their stops, and logs multi-slot state without panics. (Manual /
  integration; captured in the implementation report.)

## Safety / rollout

- Ships **default-off** (`MAX_POSITIONS=1`). Multi-slot is opt-in via env.
- **Paper-first:** run with `DRY_RUN_MOMENTUM_TRADER=true` before committing real capital;
  the validation gate found single-slot still wins P&L on the current sample, so this is
  capability, not a proven live edge.
- Real-money submission path unchanged per slot (reuses audited `submit_and_confirm`).

## Out of scope (SP3)

- `optimize-momentum-config` global+per-token wiring → **SP4** (separate).
- Changing the sim (already multi-position).
- New capital-allocation models (fixed notional per slot only).
- Cross-slot netting / portfolio-margin (each slot is an independent USDC↔token round-trip).

## Success criterion

The live trader holds up to `MOMENTUM_MAX_POSITIONS` concurrent positions, each governed by
its per-token params (global fallback), with weakest-green eviction, sharing the daily
cap/cooldowns/halt; at `MAX_POSITIONS=1` it is identical to today (legacy state migrates);
and it runs cleanly in paper mode at N>1. Real capital is gated behind
`DRY_RUN_MOMENTUM_TRADER` and the default-off env.
