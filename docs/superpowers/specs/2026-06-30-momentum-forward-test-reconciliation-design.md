# Momentum forward-test reconciliation — design

**Date:** 2026-06-30
**Status:** Approved (design); pending implementation plan
**Scope:** Additive read-only analysis tool. Does NOT modify the MEV arb path or the live momentum trader.

## Problem

The `solana-mev` framework's atomic-arbitrage edge is structurally lost to a
latency-disadvantaged, non-co-located operator. The realistic path to profit is the
directional/statistical trading already started in `src/portfolio/` (momentum, pairs).
The user wants to pursue this **paper-only for now**, focused on the **momentum** strategy
(the one edge the backtests found robust), while keeping the arb bot intact for future
co-located use.

The blocker to trusting the momentum edge is honesty of measurement. The `momentum-sim`
walk-forward backtest is **necessary but not sufficient**: it derives entry thresholds from
recorded history and replays that same history, so a "robust" verdict can still be curve-fit
to the ~53 days of data on hand. **The only true out-of-sample test is the forward paper run
on data that did not exist when the config was chosen.** The paper trader already generates
that data (`assets/momentum_actions.jsonl`), but nothing systematically asks: *did forward
paper performance match what the backtest predicted?* That reconciliation is the missing piece.

## Goal

A read-only tool that, over a forward window, computes realized paper performance, re-runs
the backtest on the same window/config, reconciles the two, and reports a graduation
scorecard against a pre-committed bar — so the decision to risk real capital is made against
fixed criteria, not hindsight.

Non-goals: no live execution, no Kamino borrow/short (pairs Phase 2b stays parked), no
changes to the trader's decision logic or the arb path, no capital sizing.

## Solution: `momentum-sim forward-report` subcommand

A new **read-only** subcommand on the existing `momentum-sim` binary
(`src/bin/momentum_sim.rs`), reusing the sim engine (`src/portfolio/sim.rs`) and
`PortfolioConfig::from_env()` (the same config the live trader reads). New code is one command
handler plus one analysis module; metric/trade-record helpers (`build_trade_record`,
Sortino/maxDD) are reused so realized and predicted numbers are produced by identical code.

```
momentum-sim forward-report \
  --actions assets/momentum_actions.jsonl \
  --history assets/price_history.jsonl \
  --since 2026-06-21T00:00:00Z      # forward-window start = when the live config was locked
  [--paper-only]                    # default true: exclude dry_run:false real trades
  [--min-trades N] [--window-weeks W] [--min-pnl-frac F] [--max-dd D]  # graduation bar overrides
```

### Data flow

```
momentum_actions.jsonl ─► parse Entered/Exited/Rotated (filter by --since, dry_run)
                             │
                      reconstruct round-trips ─► REALIZED metrics
                             │                    (net P&L, win%, Sortino, maxDD, #trades)
price_history.jsonl ─► slice to [--since, now] ─► re-run backtest, SAME .env config, SAME tokens
                             │                  ─► PREDICTED metrics + predicted trades
                             ▼
                   RECONCILE realized vs predicted ─► scorecard + verdict
```

### Input data shapes (confirmed from the live log)

- `Entered`: `{ ts, kind:"Entered", symbol, mint, usdc_in, token_amount, entry_price_usd, cost_bps, sig, dry_run }`
- `Exited`:  `{ ts, kind:"Exited", symbol, mint, usdc_out, exit_price_usd, peak_price_usd, pnl_pct, reason, sig, dry_run }`
- `Rotated`: position swap (reconstructed as exit-of-old + entry-of-new).
- `RankSnapshot`: per-tick ranking; carries `metric` + `min_score` (used to detect config drift).
- Skip\* events: diagnostic, not P&L.

### Reconciliation logic (three comparisons)

Each catches a distinct failure mode:

1. **Performance gap** — realized P&L/Sortino vs the backtest's predicted P&L/Sortino on the
   same forward window. Large negative gap = live underperforms its own simulation
   (cost/timing/slippage drag, or the edge decaying on new data).
2. **Trade alignment** — did the trader enter when the sim said to? Count matched / extra /
   missed entries. Separates execution bugs (staleness skips, cooldowns firing wrong) from a
   genuine signal problem.
3. **In-sample vs forward** — forward-window realized metrics vs the original backtest verdict
   metrics. Edge holding ≈ same Sortino sign and trade frequency; decaying = forward Sortino
   collapses toward zero.

### Graduation scorecard (pre-committed, tunable via flags)

| Criterion | Default bar | Rationale |
|---|---|---|
| Forward window length | ≥ 6 weeks | enough independent data |
| Closed paper trades | ≥ 20 | small samples lie (the recurring lesson) |
| Realized Sortino | > 0 | positive risk-adjusted return |
| Realized vs predicted P&L | ≥ 60% of predicted | live tracks sim, not fantasy |
| Max drawdown | ≤ user tolerance | survivable |

Output: per-criterion **PASS / PROGRESS / FAIL**, an overall verdict
(`KEEP PAPERING` / `ELIGIBLE FOR SMALL LIVE` / `EDGE NOT CONFIRMED`), and the headline gap.

## Edge cases & error handling

- **Open positions** (currently ~5: 11 Entered − 6 Exited): excluded from the realized
  scorecard (only closed round-trips count); shown separately as informational unrealized
  mark-to-market. Never folded into the verdict.
- **Rotated events**: synthetic exit + entry, P&L attributed to the closed leg.
- **Real vs paper**: `--paper-only` (default) excludes `dry_run:false`; both counts reported.
- **Too few trades**: below the trade floor the tool prints `INSUFFICIENT DATA — N/M trades`
  and **refuses a PASS/FAIL verdict** (guard against the 3-trade-fluke trap).
- **In-sample contamination**: if `--since` is omitted or precedes the config-lock date, the
  tool warns loudly that the comparison is no longer out-of-sample.
- **Config drift mid-window**: `RankSnapshot.min_score`/`metric` are compared across the
  window; a change is flagged (realized trades used a different config than the prediction).
- **Price-history gaps**: report coverage % over the forward window; thin coverage caveats the
  predicted side.

## Testing

Unit tests in a `#[cfg(test)]` block at the bottom of the new module (repo convention):

- Synthetic action log with known Entered/Exited pairs → assert realized P&L, win%, Sortino,
  trade count.
- Unmatched Entered → asserted excluded from realized, surfaced as unrealized.
- Mixed `dry_run` log → `--paper-only` filtering asserted.
- Tiny synthetic history + fixed config → assert predicted metrics and the gap math.
- Threshold tests: scorecard PASS/PROGRESS/FAIL boundaries, and the `INSUFFICIENT DATA`
  short-circuit below the trade floor.

## Affected files

- `src/bin/momentum_sim.rs` — new `ForwardReport` command variant + handler.
- `src/portfolio/sim.rs` (or a new `src/portfolio/forward_report.rs`) — the analysis module
  and its tests. Reuses existing metric/trade-record helpers; adds no new dependencies.
- No changes to `src/arbitrage/`, `src/graph/`, the `solana-mev` binary, or the live trader's
  decision code.

## Open questions / future

- Graduation bar defaults are a starting point; tune after first real run.
- Phase 2b (live execution, Kamino borrow for pairs) remains out of scope.
- A scheduled weekly auto-run of this report is a possible later convenience (Approach C),
  deferred as YAGNI.
