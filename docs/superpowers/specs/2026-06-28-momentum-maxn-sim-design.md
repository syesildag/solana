# Multi-Position ("max N") Momentum Simulation — Design

**Date:** 2026-06-28
**Status:** Design approved; awaiting implementation plan
**Scope:** `momentum-sim` only (measurement). No live-trader change, no `.env` knob.

## Problem

The live momentum trader and the backtester both hold **exactly one** position at a
time. `momentum_state.rs` states the invariant outright: `position: Option<Position>` —
"the type cannot represent two open positions." `sim.rs` uses the same `Option<Position>`
in its replay core.

The per-token breakdown (run 2026-06-28, live config, $1000/trade) showed all 8 curated
tokens net-positive *in isolation* (+643.65 regime-off), but those are independent
single-token sims. The live trader rotates **one** capital slot, so it can realize only a
fraction of that — it can hold just one name at any moment. The open question:

> **Would holding up to N concurrent positions beat the single-slot model on our real
> history?**

We cannot answer this today because the simulator itself is single-position. This spec
adds the measurement capability — and **only** the measurement capability. A live
implementation is a separate, later project, gated on this showing a real edge.

## Decisions (locked during brainstorming)

1. **Simulate first, build later.** Change `momentum-sim` only. The live trader is
   untouched until the sim proves N>1 wins.
2. **Capital model: fixed notional per slot.** Each held position is `trade_usdc`
   (faithful to the live model, where every entry is `momentum_trade_usdc`). Consequence:
   total deployed = held-count × `trade_usdc`, so **higher N deploys more capital**.
   Results are therefore reported both absolute *and* per-$1k-deployed so the comparison
   isn't won merely by deploying more money.
3. **When full + a stronger candidate appears:** evict the weakest **only if rotation is
   on**. This reuses the existing `MOMENTUM_ROTATE_MARGIN` semantics — today's single-slot
   `try_rotate` only fires when `rotate_margin > 0`; we generalize "the held position" to
   "the weakest held position." `rotate_margin == 0` ⇒ pure fill-and-hold, no eviction.
4. **Interface: a dedicated `maxn-compare` subcommand**, mirroring the existing
   `regime-compare`: one fixed config, replay at N=1..K, print a per-N table. Not a grid
   dimension (kept out of the production `run` grid until N is proven to help).
5. **Implementation approach A: a new `replay_multi` function**, used only by
   `maxn-compare`. The production `run` grid, `replay`, and `replay_with_regime` are left
   completely untouched (zero blast radius on the validated grid).

## Architecture

All changes are **additive**. Nothing existing is modified.

| Unit | Location | Purpose |
|---|---|---|
| `replay_multi(stream, mask, params, max_positions) -> SimRun` | `src/portfolio/sim.rs` | Multi-position backtest core: holds `Vec<Position>` capped at N; reuses `ranked_stream`, the existing stop/cost helpers, and `SimRun` accounting |
| `MaxnCompare` command + `maxn_compare(args)` | `src/bin/momentum_sim.rs` | New subcommand: build the ranked stream once, replay at each N, print the table |

Data flow (mirrors `regime-compare`):

```
ranked_stream(snapshots, watched, params)   // built ONCE
        │
        ├── replay_multi(stream, mask, params, N=1)   // ≡ replay_with_regime (anchor)
        ├── replay_multi(stream, mask, params, N=2)
        ├── replay_multi(stream, mask, params, N=3)
        └── ... up to --max-n K
        │
   per-N table: pnl_train, pnl_test, trades, win%, maxDD%, pnl_test_per_$1k
```

## `replay_multi` semantics

- **Holdings:** `Vec<Position>`, length ≤ `max_positions`. A mint is never held in two
  slots (dedup check on entry).
- **Sizing:** fixed `trade_usdc` notional per slot.
- **Entry (per tick):** while `held.len() < N`, fill free slots **greedily by rank** —
  take eligible candidates best-first until slots are full, the candidate pool is
  exhausted, or the daily entry cap is hit (so a tick may open more than one position).
  "Eligible" reuses the existing gates verbatim: min-metric threshold, not stale, not
  over-extended, re-entry cooldown elapsed, daily entry cap not hit, and **not already
  held**. The per-tick ordering (process exits before entries) must match the single-slot
  replay so that at N=1 the behavior is identical — this is enforced by the anchor test
  below.
- **Eviction (only when `held.len() == N` and `rotate_margin > 0`):** identify the
  **weakest green held** position (lowest current score among those net-green after cost).
  If the best non-held eligible candidate beats that weakest score by `rotate_margin`,
  close the weakest and open the candidate. At most one eviction per tick.
- **Exit:** each held position is evaluated **independently** against the existing exit
  logic (trailing stop, vol stop, profit-protected give-back, max-hold time stop, fade
  take-profit). A position that exits frees its slot for refill on a later tick.
- **Accounting:** realized P&L sums across all positions into the existing `SimRun`.
  `max_drawdown_pct` is the **existing realized-equity-curve drawdown**, now fed by all N
  positions' realized P&L — a portfolio drawdown of *realized* equity. (Unrealized
  drawdown is intentionally NOT tracked: it would break the N=1 byte-identical anchor,
  since the single-slot metric is realized-only.)

## `maxn-compare` subcommand

CLI (same config surface as `per-token`, plus the N range):

```
momentum-sim maxn-compare \
  --metric sharpe --min-metric 0.0377 --trail 20 --lookback 480 --max-run 6 \
  --regime-obs 480 --trade-usdc 1000 --max-n 5 \
  [--tokens PATH] [--history PATH] [--train-frac 0.7] [--max-step 8] [...]
```

Output (one row per N from 1 to `--max-n`):

```
N   pnl_train  pnl_test  trades  win%  maxDD%   pnl_test_per_$1k
1     +118.0    +118.0      22    64%   4.0%        +118.0
2     +205.0    +205.0      31    61%   6.5%        +102.5
3     +260.0    +260.0      40    59%   9.0%         +86.7
...
```

- Train/test split honored via the existing `--train-frac`.
- `pnl_test_per_$1k = pnl_test / (N × trade_usdc / 1000)` — normalizes the capital
  caveat so N>1 must win *per dollar*, not just by deploying more.
- Regime: the subcommand exposes the level regime (`--regime-obs`) like `per-token`;
  trend-regime is out of scope here (the live `.env` uses trend, noted as a known
  approximation — same limitation `per-token` already has).

## Testing

- **Correctness anchor — `replay_multi` at N=1 ≡ single-position replay.** On the same
  stream and params, `replay_multi(..., max_positions=1)` produces a `SimRun` byte-identical
  to `replay_with_regime`. Guarantees the generalization is faithful.
- **Eviction:** fires only when full, `rotate_margin > 0`, and the margin is beaten; the
  evicted slot is the weakest *green* held.
- **No eviction when `rotate_margin == 0`:** full holdings persist until a stop fires.
- **Dedup:** a mint already held is never opened into a second slot.
- **Daily cap / cooldown:** still gate entries across all N slots (not per-slot).
- **Portfolio drawdown:** with two simultaneously-held losing positions, `max_drawdown_pct`
  reflects the summed equity dip, exceeding either position's individual drawdown.

## Out of scope (explicitly)

- Any change to the live momentum trader (`momentum.rs`, `momentum_state.rs`).
- Any new `.env` variable.
- Adding `max_positions` to the production `run` grid.
- Trend-regime support in the compare subcommand.

## Success criterion

The subcommand runs over the curated universe and prints a trustworthy per-N table whose
N=1 row matches the existing single-slot results. The **decision output** is whether
`pnl_test_per_$1k` improves as N grows: if yes, that triggers a separate live-implementation
spec; if no (or it degrades past some N), the single-slot model stands and we stop.
