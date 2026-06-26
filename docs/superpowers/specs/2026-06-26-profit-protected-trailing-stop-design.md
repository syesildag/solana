# Profit-protected trailing stop (`max_trail_pct`) — design

**Date:** 2026-06-26
**Status:** design approved; backtest-first, default-off (mirrors the vol-stop work)

## Context

The momentum trader exits a position on a single fixed trailing stop:
`price ≤ peak·(1 − MOMENTUM_TRAIL_PCT/100)` ([momentum.rs `maybe_exit`](../../../src/portfolio/momentum.rs)).
A tight trail (5–8%) shakes you out of a still-profitable runner — e.g. entry $100,
peak $150, an 8% trail bails at $138, and you miss the rest of the move if it resumes.

**The idea:** let a winner *give back* unrealized gains (lose money relative to the
peak) **as long as the trade stays net-positive vs entry** — ride winners further,
but never let a green trade turn into a loss. Expose it as a config var `max_trail_pct`.

**Related existing primitives (both backtest-only, not live):**
- The sim has `breakeven_exit` (bool): once `peak > entry`, exit when price falls
  back to/under the *raw* entry price. The new mechanism generalizes this.
- Phase A added a volatility-scaled stop (ATR/σ), default off, validated as a
  backtest capability only (0/896 robust on the recorded sample).

Same discipline applies: validate in the walk-forward backtest first; wire live only
if it clears the robustness gate; default off so today's behavior is unchanged.

## The exit rule

Let `floor` be the **cost-adjusted breakeven** and `give_back` the capped pullback
from the peak:

```
round_trip_cost_frac = (2·slippage_bps + 2·gas_bps) / 10_000   # entry + exit
floor      = entry_price × (1 + round_trip_cost_frac)          # exiting here nets ~$0
give_back  = peak_price  × (1 − max_trail_pct / 100)
effective_stop = max(floor, give_back)
```

The position is **green** once `peak_price > floor`. Behavior:

- **Green position** → exit when `price ≤ effective_stop`.
  - Big winner (`give_back > floor`): exits at `peak − max_trail%` — caps how much
    profit is handed back.
  - Small winner (`give_back ≤ floor`): exits at the breakeven floor — never red.
- **Not-yet-green position** (`peak ≤ floor`): unchanged — the existing tight
  `trail_pct` (or the Phase-A vol-stop, if ever enabled) governs the stop-loss, so
  losers are still cut at the configured width.
- **`max_trail_pct == 0` ⇒ disabled** — byte-for-byte today's behavior.

### Worked examples (entry $100, round-trip cost 1% → floor $101)

| peak | max_trail_pct | give_back | effective_stop | meaning |
|---|---|---|---|---|
| $150 | 20% | $120 | **$120** | give back ≤20% of peak; keep ~$20 profit |
| $150 | 40% | $90 | **$101** | floored at breakeven; ride the full pullback |
| $105 | 20% | $84 | **$101** | barely green → breakeven floor |
| $100 (never green) | — | — | governed by `trail_pct` | normal stop-loss |

## Architecture

One shared pure predicate, used by both the backtest and (later) the live trader so
they cannot drift — the same pattern as `vol_stop_triggered`.

- **`momentum.rs`** — new pure fn (name TBD in plan, e.g. `profit_protected_stop`):
  given `price, peak, entry, round_trip_cost_frac, max_trail_pct, fallback_stop_hit:
  bool` → returns whether to exit. When `max_trail_pct == 0` it returns
  `fallback_stop_hit` (the existing stop), preserving current behavior. When `> 0`
  and green, it applies the `max(floor, give_back)` rule; when `> 0` and not green,
  it returns `fallback_stop_hit`.
- **`sim.rs`** — `ParamSet` gains `max_trail_pct: f64` (additive; `breakeven_exit`
  left untouched). The replay exit branch routes the trailing decision through the
  shared predicate, passing the existing fixed/vol stop as `fallback_stop_hit` and
  computing `round_trip_cost_frac` from the params' `slippage_bps` (+ gas).
- **`run_grid`** — add `max_trail_pct` to the sweep as an additive dimension
  (a small set, e.g. `[0, 15, 25, 40]`, `0` = the current fixed-trail baseline), so
  enabling it grows the grid by a constant, not a multiple. CSV gains a
  `max_trail_pct` column; `print_env_block` emits `MOMENTUM_MAX_TRAIL_PCT`.
- **CLI** — `run` gains `--max-trail-pcts` (comma list, default the grid set);
  `per-token` gains `--max-trail-pct` for spot-checks.

### Phase B (conditional — only if the gate clears)
- **`mod.rs`** — `PortfolioConfig.momentum_max_trail_pct: f64`, parsed from
  `MOMENTUM_MAX_TRAIL_PCT` (default `0.0` = off).
- **`maybe_exit`** — compute `round_trip_cost_frac` from `cfg.momentum_slippage_bps`
  (+ gas estimate), call the shared predicate with the current `stop_hit` as the
  fallback. Log the effective stop / floor / reason for the audit trail.
- **`.env.example`** — document `MOMENTUM_MAX_TRAIL_PCT`, off by default, with the
  "backtest-promising, validate before enabling" caveat.

## Verdict gate

Run the walk-forward grid; compare the best **robust** config using `max_trail_pct > 0`
against the best **robust** config at `max_trail_pct = 0` (today's fixed trail), by
worst-slice P&L (`config_is_robust` / `worst_slice`). **Proceed to Phase B only if a
capped-give-back config is robust AND beats the fixed-trail baseline out-of-sample.**
Otherwise stop at Phase A and report — the live trader stays unchanged.

## Testing

- Pure-predicate unit tests: `max_trail_pct = 0` ≡ the fallback stop (regression);
  green big-winner exits at `peak − max_trail%`; green small-winner exits at the
  cost floor; not-yet-green defers to the fallback stop; non-positive peak never fires.
- Sim replay test: a position that runs up then pulls back partially is *held* under a
  generous `max_trail_pct` where the tight `trail_pct` would have exited, and is
  closed at the cost-breakeven floor when it round-trips (never realizing a loss).
- All existing momentum/sim tests remain green.

## Out of scope / non-goals

- No change to entry logic, rotation, market-closed, or `max_hold` exits.
- No change to `breakeven_exit` (kept as-is; `max_trail_pct` with a large value plus
  the cost floor supersedes it in practice — possible future cleanup, not now).
- No new dependency.
