# Equity-compounding position sizing (`reinvest_frac`) — design

**Date:** 2026-06-26
**Status:** design approved; backtest-first, default-off (mirrors the vol-stop / max-trail work)

## Context

The momentum trader commits a **fixed** USDC notional per entry
(`MOMENTUM_TRADE_USDC` → `cfg.momentum_trade_usdc`, used throughout `maybe_enter`).

**The idea:** start small and let the size grow as the strategy banks profit — an
anti-martingale / equity-compounding scheme: bet more when winning, fall back toward
the small base when profit gives back. Never turn a fixed-notional trader into a
fixed-notional-but-bigger one blindly — scale with the realized equity curve.

The signal already exists on both sides:
- **Live:** cumulative realized PnL via `summarize(&state.trades).realized_usdc`
  ([momentum.rs:766](../../../src/portfolio/momentum.rs)).
- **Sim:** the replay maintains a running `realized` accumulator + `equity_curve`
  ([sim.rs:316,395](../../../src/portfolio/sim.rs)).

Same discipline as the prior two ideas: validate in the walk-forward backtest first;
wire live only if it clears the gate; default off so today's behavior is unchanged.

## The sizing rule (computed at each entry)

```
realized = cumulative realized PnL across closed trades
size = clamp( base + reinvest_frac × max(0, realized),  base,  ceiling )
size = min(size, available_usdc)          # live only — never exceed the wallet
```

- `base` = `MOMENTUM_TRADE_USDC` (the small start).
- `reinvest_frac` = new knob. **`0` ⇒ fixed size = today's behavior (the off switch).**
- `ceiling` = hard max size (risk cap). **Default = `base`** (fail-safe: even if
  `reinvest_frac > 0`, size can't grow until `ceiling` is raised above `base`).
- Only **realized/banked** profit compounds — open-position paper gains never count.
- Floors at `base`: a drawdown walks size back to `base`, never below (catastrophic
  loss is already handled by the separate `MOMENTUM_MAX_LOSS_USDC` breaker).
- **Basis = cumulative realized PnL since inception** (not a rolling window — the
  give-back term already provides responsiveness; rolling is a future refinement).

### Worked example (base $100, reinvest_frac 0.5, ceiling $500)

| banked realized PnL | size |
|---|---|
| ≤ $0 | $100 (base) |
| +$300 | $250 |
| +$800 | $500 (ceiling) |
| back to +$100 | $150 |

## Architecture

One shared pure fn so sim and live can't drift (same pattern as the stop predicates):

- **`momentum.rs`** — `dynamic_trade_usdc(base, reinvest_frac, ceiling, realized) -> f64`
  returns `base` when `reinvest_frac == 0` (preserving current behavior), else the
  clamped compounding size above.
- **`sim.rs`** — `ParamSet` gains `reinvest_frac: f64` and `size_ceiling_usdc: f64`.
  At entry the replay computes `size = dynamic_trade_usdc(params.trade_usdc, …, realized)`
  and uses it for `token_amount`, the gas estimate, and `usdc_in` (so the realized
  accumulator and equity curve reflect the actual size). Rotation legs keep their
  current sizing (size changes apply to fresh USDC→token entries only).
- **Live (`maybe_enter`)** — compute `realized` from `summarize`, derive `size`.
  Balance gate keeps today's semantics: if `ctx.usdc_balance < base` → skip
  (`SkipInsufficientUsdc`, unchanged); otherwise trade `size = min(computed, balance)`.
  Use `size` everywhere `cfg.momentum_trade_usdc` is the notional. Daily cap and the
  `MAX_LOSS_USDC` breaker are unchanged. Record the actual `size` in the `Position` /
  `Entered` audit so the trade log is honest.

### Phase B config (conditional — only if the gate clears)
- `PortfolioConfig`: `momentum_reinvest_frac` (default `0.0`),
  `momentum_size_ceiling_usdc` (default = `momentum_trade_usdc`). Env:
  `MOMENTUM_REINVEST_FRAC`, `MOMENTUM_SIZE_CEILING_USDC`. `.env.example` documents both
  with the "validate before enabling" caveat.

### Grid (Phase A)
- Additive sweep `reinvest_frac {0, 0.25, 0.5, 1.0}` × `ceiling {2×, 3×, 5× base}`
  (the `0` arm is the fixed baseline). CLI `--reinvest-fracs` / `--size-ceilings`,
  CSV columns, env-block lines — same wiring as the prior knobs.

## Verdict gate (must be risk-adjusted, not raw P&L)

Dynamic sizing inflates **absolute** `net_pnl` purely by betting bigger — a sized-up
config shows higher P&L even with *zero* edge improvement. So the comparison must
normalize for that. The gate:

1. Standard `config_is_robust` (profitable in both slices, ≥ `min_trades` each) still
   applies.
2. Compare a dynamic config against the fixed baseline on a **drawdown-adjusted**
   basis: P&L per unit of max-drawdown (a Calmar-like ratio, using the sim's existing
   `max_dd_test`). **Proceed to Phase B only if a dynamic config improves the
   drawdown-adjusted return out-of-sample** — i.e. it earns more *per unit of risk*,
   not merely more dollars. If it only scales P&L and drawdown together, that's
   leverage, not edge → stop at Phase A, keep it off.

Report P&L, max_dd, and the P&L/max-DD ratio per config so the leverage-vs-edge
distinction is visible.

## Testing

- Pure-fn unit tests: `reinvest_frac = 0` ≡ `base` (regression); compounding grows
  with realized PnL; clamps at `ceiling`; floors at `base` when realized ≤ 0.
- Sim replay test: on a winning sequence, a `reinvest_frac > 0` config deploys a
  larger notional on later entries than the fixed baseline (and identical on the first
  trade, when realized = 0).
- All existing momentum/sim tests stay green.

## Out of scope / non-goals

- No rolling-window basis, no below-base de-risking, no Kelly fraction-of-equity
  (considered and rejected for v1 — revisit if the cumulative scheme validates).
- No change to entry selection, rotation sizing, or the exit logic.
- No new dependency.
