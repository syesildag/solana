# Auto-adopt unwatched wallet tokens (trail-only) — design

**Date:** 2026-08-09
**Status:** approved (serkan, 2026-08-09)
**Motivation:** the wallet held a deeply-drawn-down CATE bag the trader could not manage
because adoption (`MOMENTUM_ADOPT_WALLET_POSITION`) only joins against the curated
watch list. The operator wants every meaningful wallet holding managed automatically —
without having to wire each token into `momentum_tokens.json` first.

## Summary

A second, separately-gated adoption pass in the momentum trader: adopt **unwatched**
wallet SPL tokens into free `MOMENTUM_MAX_POSITIONS` slots and manage them with a
**trailing stop only** (no fade exit). Ships **default-off**; an unset `.env` behaves
byte-identically to today.

## Env knobs

| Var | Default | Meaning |
|---|---|---|
| `MOMENTUM_ADOPT_ALL_TOKENS` | `false` | Master gate for the unwatched-adoption pass. Also requires `MOMENTUM_ADOPT_WALLET_POSITION=true`. |
| `MOMENTUM_ADOPT_EXCLUDE_MINTS` | empty | Comma-separated mints excluded on top of the built-in set. |
| `MOMENTUM_ADOPT_TRAIL_PCT` | = `MOMENTUM_TRAIL_PCT` | Trail width for adopted-unwatched positions. |

Built-in exclusions (always, not configurable): native SOL, WSOL
(`So111…1112`), USDC, USDT. Rationale: gas, the arb bot's working capital, and the
trader's own cash side must never be auto-liquidated.

## Adoption pass

Runs where watched adoption already runs (startup + every slow tick), **after** the
watched pass, only if `MOMENTUM_ADOPT_ALL_TOKENS=true` and free slots remain.
Implemented as a separate `adopt_unwatched_holdings` function (approach B) — the
existing, unit-tested watched path is not modified.

Candidate = wallet SPL token that is:

1. not in the watched list (watched tokens use the existing pass),
2. not currently held, not in the built-in or configured exclusion set,
3. worth ≥ 0.5 × `MOMENTUM_TRADE_USDC` at the **risk-report pricer** price
   (DexScreener deepest-trusted-pool map, already fetched every slow tick — this is
   the pricing source for unwatched mints; they have no gRPC/watch feed),
4. not inside its `MOMENTUM_REENTRY_COOLDOWN_SECS` window (prevents
   adopt → stop-out → re-adopt churn on leftover dust or a hovering price),
5. **sellable**: a Jupiter sell-quote for the full **raw** balance succeeds at
   adoption time (raw units via `scanner::fetch_token_balance_raw`, never
   ui × 10^dec — Token-2022 scaledUiAmount lesson). Failed quote ⇒ log + skip this
   tick; no retry storm, re-evaluated next slow tick.

Surviving candidates fill free slots **USD-value descending**. Each adopted position
is stored with basis = adoption-time price (real cost basis unknown, PnL measured
from adoption), `adopted_unwatched: true`, `entry_sig: "adopted-unwatched"`, and an
`Adopted` audit record carrying the flag.

## Position state

`Position.adopted_unwatched: bool` with `serde(default)` — pre-upgrade state files
deserialize to `false`, so a restart never re-classifies an existing position.

## Exit semantics for flagged positions

- **Trailing stop** at `MOMENTUM_ADOPT_TRAIL_PCT`, evaluated on the slow tick
  (~60 s REST cadence). These tokens have no gRPC feed, so the 1-s fast exit arm
  does not cover them — accepted and documented trade-off.
- **Fade exit: never** (also structurally impossible — no rank history ⇒ no metric).
- **Stagnation eviction: applies** (`MOMENTUM_STAGNATION_HOURS`/band, currently
  96 h / 2 %) — a dead bag eventually yields its slot to a watched challenger. The
  held-side predicate (`is_stalled`) is metric-free, so this composes cleanly.
- **Rotation / weakest-green eviction: exempt** — both compare rank metrics the
  position doesn't have.
- **Liquidity-drain exit: not applicable** (no depth feed for unwatched pools);
  fails open like every uncovered pool today.
- Global guards unchanged at the sell path: daily trade cap, `MOMENTUM_MAX_LOSS_USDC`
  halt, cost and slippage caps.

## Observability

- Adoption pass logs every skip with its reason (excluded / floor / cooldown /
  unsellable / no price), mirroring the watched pass's "never silently inert" rule.
- When the master gate is on but the trader is in paper mode
  (`DRY_RUN_MOMENTUM_TRADER=true`), adoption itself is skipped (existing
  `momentum_dry_run` gate) but the pass still logs **"would adopt X (…)"** selection
  lines so the rollout can be validated on paper before the live flip.

## Testing

- Pure selection function (like `choose_adoption`): exclusion sets, floor, cooldown,
  cap interplay with the watched pass, USD-descending order, watched-pass priority.
- Serde round-trip: old state file (no flag) ⇒ `false`; flagged position survives
  save/load.
- Exit path: a flagged position never takes the fade branch; uses
  `MOMENTUM_ADOPT_TRAIL_PCT`; stagnation predicate applies; rotation selection
  never returns it.

## Rollout

1. Ship default-off (no `.env` change ⇒ no behavior change).
2. Enable with `DRY_RUN_MOMENTUM_TRADER=true` + `MOMENTUM_ADOPT_ALL_TOKENS=true`,
   watch the "would adopt" lines for one session.
3. Flip live. First live effect on the current wallet: the CATE bag is adopted at
   the then-current price and managed from there (its −74 % drawdown is sunk;
   the trader will not wait to break even).

## Out of scope

- Entries for unwatched tokens (adoption-only; the trader never buys them).
- gRPC wiring / fast-arm exits for unwatched pools.
- A separate quota for adoptees (they share `MOMENTUM_MAX_POSITIONS`, per the
  operator's explicit request).
