# Momentum-ordered token discovery — design

**Date:** 2026-06-27
**Status:** design approved; opt-in, reversible, default = today's behavior

## Context

The live token-discovery overlay (`scripts/scan_tokens.js`, run hourly by the momentum
watcher when `MOMENTUM_SCAN_ENABLE=true`) surfaces the **top-3 by 24h volume** among
liquid, Jupiter-verified, non-curated tokens. That ranks by *size*, not *momentum* — so
a hot mover like **KLED** (24h volume $3.97M, liquidity $1.25M, vol/liq 3.2, **+28%**)
never reaches the top-3, which is dominated by high-volume-but-flat giants (cbBTC $111M,
MU $27M, PUMP $21M). The goal: order discovery by momentum so movers surface into the
watched set, while keeping volume/liquidity as the tradeability gate.

**Hard feasibility constraint (verified):** the bot has *no* per-minute price history for
a token it has never watched, so it cannot compute its own `return`/`slope_r2` at scan
time. Birdeye's `tokenlist` exposes `v24hUSD` and `price` but **no 24h price-change**;
the per-token `token_overview` endpoint does (`priceChange24hPercent`). So the only
momentum signal available at scan time is **Birdeye's 24h price-change %**, fetched
per-candidate.

**Scope note:** discovery is a *curation policy*, not a trading edge. It is NOT exercised
by the `momentum-sim` walk-forward backtest (which uses the fixed curated
`momentum_tokens.json`), so there is no backtest gate here — it's judged on heuristic
soundness. The momentum ranker and entry guards still gate every actual trade, so the
blast radius is bounded: worst case the watch list holds hot-but-un-enterable names.

## The change

Only the **ordering** of discovery candidates changes; every safety gate stays. New
pipeline in `scan_tokens.js` (when `MOMENTUM_SCAN_RANK=change`):

```
Birdeye top-by-volume (paged to the volume floor)
 → GATE (unchanged): drop stables/wrapped/curated;
        require vol ≥ $250k, liq ≥ $200k, vol/liq ≤ 30
 → fetch 24h price-change for the top-N-by-volume survivors
        (token_overview, ~25 paced calls/scan — mirrors the existing Jupiter-verify pass)
 → BAND: keep 0 < change24h ≤ MOMENTUM_SCAN_MAX_CHANGE_PCT   (default 50; 0 = no ceiling)
 → SORT survivors by change24h descending
 → emit survivors (each gains a `change24h` field)
watcher keeps the top-3 as today — now momentum-ordered
```

When `MOMENTUM_SCAN_RANK=volume` (the default), behavior is byte-identical to today
(sort by `v24hUSD` desc, no price-change fetch, no band).

### Why these choices
- **Gate-then-sort.** Volume/liquidity/wash floors keep doing their real job — a
  tradeability/anti-pump filter — so we never surface a thin pump. Only the *ordering*
  among already-safe candidates flips from "biggest" to "hottest."
- **Fetch price-change only for the top-N-by-volume survivors** (the set already passed
  to Jupiter-verify, capped at `SCAN_VERIFY_MAX`, default 25). Bounds API cost and
  inherently favors *liquid* movers — a $300k-volume token up 200% won't be seen, which
  is intended (don't chase the thinnest pumps). KLED's $4M is comfortably in this set.
- **Band `(0, ceiling]`.** Lower bound 0 → up-movers only (long-only trader). Upper
  ceiling drops already-parabolic names that the entry over-extension guard
  (`MOMENTUM_MAX_RUN_PCT`) would immediately `SkipOverextended` and that tend to dump.
  This makes discovery and the entry guard *agree*: surface "strong but not blown-up"
  movers (KLED +28% lands in the sweet spot; a +200% spike never enters the watch list).
  Default 50% admits KLED, cuts blow-ups; `0` disables the ceiling.

## New config (all opt-in, reversible)

| Env var | Default | Meaning |
|---|---|---|
| `MOMENTUM_SCAN_RANK` | `volume` | `volume` = today's behavior; `change` = momentum order |
| `MOMENTUM_SCAN_MAX_CHANGE_PCT` | `50` | 24h-change ceiling when ranking by change; `0` = off |

Read by `scan_tokens.js` from the environment (the watcher already passes the bot's env
through when it spawns the scan). No Rust config change required unless we also want the
values surfaced in `PortfolioConfig` — out of scope for v1 (the scan reads env directly).

## Components touched

- **`scripts/scan_tokens.js`** — the only behavioral change:
  - A pure ranking/banding function (network-free, unit-testable): given gated survivors
    each annotated with `change24h`, plus `rank` mode + `maxChangePct`, return the
    ordered list (volume-desc, or change-desc within `(0, ceiling]`).
  - A `token_overview` fetch helper for `change24h`, called only for the top-N-by-volume
    survivors when `rank=change` (paced like `verifyAll`).
  - `--json` output gains `change24h` per survivor; the diagnostic stdout line shows it.
- **No Rust change** for v1 — the watcher consumes the same `--json` shape and still
  takes the top-3; it just receives a momentum-ordered list.

## Testing

- Unit-test the pure ranking function:
  - `rank=volume` → identical to current volume-desc order (regression).
  - `rank=change` → sorts by `change24h` desc; drops `change ≤ 0`; drops
    `change > ceiling`; `ceiling=0` keeps all positive.
  - KLED-like case (+28%, in-band) ranks above a flat giant (+1%, huge volume).
- The Birdeye `token_overview` fetch is a network call — verified offline like the
  existing `tokenlist`/Jupiter wire calls (not unit-tested).
- `node scripts/scan_tokens.js` (diagnostic mode) and `--json` run cleanly with both
  `MOMENTUM_SCAN_RANK` values.

## Verification (manual, live)

1. `MOMENTUM_SCAN_RANK=change node scripts/scan_tokens.js` → KLED-class movers appear at
   the top; parabolic (>50%) names excluded; giants (flat) drop off.
2. `MOMENTUM_SCAN_RANK=volume node scripts/scan_tokens.js` → unchanged from today.
3. With the watcher live + `MOMENTUM_SCAN_ENABLE=true`, confirm the discovered top-3 in
   the rank snapshot reflect movers, and that over-extended ones still `SkipOverextended`
   at entry (expected — discovery is a watch gate, not a buy decision).

## Out of scope / non-goals

- No OHLCV/slope_r2 computation for discovery candidates (heavier; deferred).
- No volume×momentum hybrid score (chose pure change-ordering within the volume gate).
- No change to the momentum ranker, entry guards, or `momentum_tokens.json` curation.
- No `PortfolioConfig` plumbing — the scan reads the two env vars directly.
