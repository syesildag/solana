# Liquidity drain guard — design

**Date:** 2026-08-01
**Status:** approved, implementing
**Scope:** momentum trader (`portfolio-watcher`) only. No arb-path changes.

## Problem

The momentum trader is price-only. It has no awareness of how much liquidity backs the
token it holds. A pump.fun pool can drain while a position is open, and the trailing stop
then tries to exit into a book that cannot fill it — the stop "fires" at a price nobody
will pay. The risk is concentrated in the unvetted-add path (`/add-momentum-token`), which
skips every screen the discovery scanner applies.

A runtime **entry** gate already exists and is enabled (`MOMENTUM_LOCAL_IMPACT=true`):
`feed_setup::publish_impact` quotes a `MOMENTUM_TRADE_USDC`-sized *buy* from live pool
state on each gRPC update, and `momentum.rs` skips a candidate whose estimate exceeds
`2 × momentum_max_cost_bps`. What does not exist:

- no re-check of liquidity **after** entry — a 40-hour hold through a draining pool is invisible
- **sell**-side impact is never computed, and selling is where capital is trapped
- no liquidity-aware position sizing
- DLMM pools publish no estimate at all (`price_impact` hardcoded `0.0`)

## Non-goals

- Holder-distribution checks at runtime. Concentration changes on a scale of days and needs
  an RPC round-trip; it belongs in curation (`scan_tokens.js` already enforces
  `SCAN_MAX_TOP_HOLDERS_PCT`), not on a 60-second tick.
- Closing the `/add-momentum-token` vetting bypass. Considered and deliberately deferred —
  the chosen scope is runtime-only.
- Any claim of book-wide P&L improvement. JitoSOL/WETH/HYPE/ZEC are deep-liquidity majors
  where this never fires. The value is CATE and future pump.fun adds.

## Architecture

The feed publishes **state**; the trader applies **policy**. One published number per mint
serves both the entry size cap and the exit gate.

```
gRPC account update
        │
        ▼
feed_setup::publish_depth(w, feed)         ── NEW, beside publish_impact
        │   quote-side reserve, in USD
        ▼
GrpcFeed.depth : DashMap<String, (f64, Instant)>
        │
   ┌────┴─────────────────────┐
   ▼                          ▼
try_open_position         holding loop
  entry size cap          sell-impact exit
```

### Impact formula (exact for constant product)

Constant product `X·Y = k`, quote reserve `Y` worth `D` USD, position worth `V` USD.
Selling `Δx` tokens returns `Δy = Y·Δx/(X+Δx)`, and `V = Δx·(Y/X)`, so

```
impact = 1 − Δy/(Δx·Y/X) = Δx/(X+Δx) = V / (D + V)
```

`sell_impact_bps = 10_000 · V/(D+V)`. Exact for CP up to the swap fee, which is charged on
top. No calibration, no fitted constants.

Inverting for the entry cap: for a target impact `b`, the largest position is
`V = D·b/(1−b)`.

## Components

| unit | file | responsibility |
|---|---|---|
| `GrpcFeed.depth` + `publish_depth` / `quote_depth_usd` | `grpc_pricer.rs` | store + serve the latest quote-side depth with a freshness stamp |
| `publish_depth(w, feed)` | `feed_setup.rs` | read CP vault reserves, convert to USD, publish |
| `sell_impact_bps(position_usd, depth_usd)` | `momentum.rs` | pure policy function |
| exit leg | `momentum.rs` holding loop | close on sustained excess sell impact |
| entry size cap | `momentum.rs` `try_open_position` | shrink notional to fit the pool |
| config | `portfolio/mod.rs` | four env vars, all default-off |

## Coverage — deliberately narrow

Depth is published for **CP pools only**: `RaydiumAmmV4`, `PumpSwap`, `Saber`.

Whirlpool and Raydium CLMM expose vault balances, but `snapshot_state` returns them as a CP
approximation; for concentrated liquidity the vault total overstates depth usable near the
current tick. Publishing it would yield a confidently wrong number, which is worse than
none. DLMM publishes nothing (its quote model is pure-linear).

Every uncovered pool **fails open**: no depth ⇒ no exit, no size cap, unchanged behaviour.
This mirrors the existing `MOMENTUM_LOCAL_IMPACT` convention. It is a real gap, not a
rounding: a DLMM-priced token receives no drain protection. The pools that motivate the
feature (pump.fun / PumpSwap) are all CP, so coverage matches the risk.

## Configuration

| var | default | effect |
|---|---|---|
| `MOMENTUM_MAX_EXIT_IMPACT_BPS` | `0` (off) | close the position when estimated sell impact exceeds this |
| `MOMENTUM_EXIT_IMPACT_SHADOW` | `false` | log the trigger, take no action — the rollout stage |
| `MOMENTUM_MAX_ENTRY_IMPACT_BPS` | `0` (off) | cap entry notional so estimated impact ≤ this |
| `MOMENTUM_DEPTH_MAX_AGE_SECS` | `120` | freshness bound, mirrors the existing impact gate |

All-default ⇒ byte-identical behaviour, regression-locked by test.

## Error handling

| condition | behaviour |
|---|---|
| depth missing or older than `MOMENTUM_DEPTH_MAX_AGE_SECS` | fail open — no exit, no cap |
| `sol_usd` unavailable for a SOL-quoted pool | publish nothing ⇒ fail open |
| non-CP pool | never published ⇒ fail open |
| `depth_usd <= 0` or non-finite | `sell_impact_bps` returns `None` ⇒ fail open |
| transient depth dip | dwell-confirm, shared with the other stop legs |
| pool genuinely drained | the exit sell itself slips; unavoidable. The goal is to leave *earlier*, not cheaply |

The exit reason string is `"liquidity drain"`, audited like the other stop legs.

## Testing

- `sell_impact_bps` table: `V=0 → 0`; `V=D → 5000`; `V=D/9 → 1000`; `D=0` → `None`;
  non-finite → `None`.
- Entry-cap inversion round-trips against `sell_impact_bps`.
- `publish_depth`: CP pool with known reserves and `sol_usd` publishes the expected USD;
  missing `sol_usd` publishes nothing; a Whirlpool/DLMM pool publishes nothing.
- `quote_depth_usd` returns `None` past `max_age`.
- Exit leg: fresh depth under threshold → hold; over → exit; stale → hold; shadow → log only.
- Flag-off equality: all four vars at default leave entry and exit paths unchanged.

## Validation and rollout

**This cannot be backtested.** `assets/price_history.jsonl` stores only prices — there is no
liquidity, reserve, or holder history, so `momentum-sim` can never score this mechanism. It
is in the same epistemic class as the gRPC spike detector: the edge is earned live.

Rollout: `MOMENTUM_EXIT_IMPACT_SHADOW=true` (log only) → read the logged triggers for two
weeks and confirm they coincide with real drains rather than noise → paper
(`DRY_RUN_MOMENTUM_TRADER=true`) → live.

**Standing caveat.** This is an exit mechanism, and the recorded track record is that five of
six tested exit mechanisms lost money out-of-sample, with exit rules competing rather than
composing (see `docs/momentum-sim.md`). The one that survived — regime-death — did so because
it fired on a genuinely new trigger (the entry premise) rather than a price level. A drain
exit also has a genuinely new trigger (pool state), which places it in the "could survive"
class. But the failed five were caught *because* they were measurable, and this one is not.
The shadow stage is the only evidence that will ever exist.
