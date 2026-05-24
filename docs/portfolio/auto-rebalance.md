# Auto-Rebalance (`portfolio-watcher`)

Mean-reversion–driven automatic token rotation, wired into the existing
`portfolio-watcher` 60-second loop. **Off by default.** Set
`ENABLE_AUTO_REBALANCE=true` to arm it.

## What it does

Each tick the watcher checks the portfolio for a rebalancing opportunity. The
signal fires when one held asset is at its **30-day high** and is currently
declining, while another held asset is at its **30-day low** and is currently
rising. When both conditions appear simultaneously and every cost / safety gate
clears, the bot uses **Jupiter v6** to swap a configurable fraction of the
high-side holding into the low-side holding.

The behaviour is intentionally conservative:

- Two gates block trading **before** any quote is requested (master switch,
  recovery gate, daily cap), so most ticks do zero network work.
- A 30-day extreme alone is not enough — it must have been touched within the
  last 24 hours **and** the price must have moved by ≥ 0.3 % in the opposite
  direction over the last 60 minutes. This filters dead-cat bounces and the
  short-horizon momentum regime that contradicts naive reversal entries.
- After every swap the bot writes a portfolio snapshot to disk. It will not
  fire another swap (in any pair) until the portfolio's EUR value rises back
  above that snapshot — the "wait to gain money" rule. Restart-safe.

## Architecture

```
portfolio_watcher tick (60s)
  ├── existing: fetch prices → analyze risk → email alerts (7-day, unchanged)
  └── rebalancer::maybe_rebalance(&ctx)
        ├── gate: ENABLE_AUTO_REBALANCE?
        ├── gate: recovery — current_total_eur ≥ latest_snapshot.total_eur?
        ├── gate: daily cap (≤ REBALANCE_MAX_SWAPS_PER_DAY)?
        ├── analyzer::generate_rebalance_signals
        │     (30-day extreme in last 24h + 60-minute reversal confirmed)
        ├── per signal: hold cooldown? quote? cost gate?
        ├── snapshot portfolio state → rebalancer_snapshots.jsonl
        ├── log BEFORE prices + cost breakdown
        ├── jupiter::swap → sign → submit → confirm
        ├── log AFTER prices + realized fill + variance
        ├── persist execution to rebalancer_state.json
        ├── send execution email (bypasses ALERT_COOLDOWN_MIN)
        └── return ExecutedSwap
```

### New files

| File | Purpose |
|---|---|
| [`src/portfolio/jupiter.rs`](../../src/portfolio/jupiter.rs) | Thin async client for Jupiter v6 `/quote` + `/swap` and decimals lookup. |
| [`src/portfolio/rebalancer_snapshots.rs`](../../src/portfolio/rebalancer_snapshots.rs) | Append-only `rebalancer_snapshots.jsonl` — restart-safe baseline for the recovery gate. |
| [`src/portfolio/rebalancer_state.rs`](../../src/portfolio/rebalancer_state.rs) | `rebalancer_state.json` — execution history backing the daily cap, hold cooldown, and take-profit checks. Also holds the `HaltRecord` type for the loss-halt circuit breaker. |
| [`src/portfolio/rebalancer_actions.rs`](../../src/portfolio/rebalancer_actions.rs) | Append-only JSONL audit log of every decision (consider, skip, halt, dry-run, execute). |
| [`src/portfolio/rebalancer.rs`](../../src/portfolio/rebalancer.rs) | Orchestration: gate stack → signal selection → Jupiter quote/swap → sign/submit → email. |

### Files modified

- [`src/portfolio/mod.rs`](../../src/portfolio/mod.rs) — registered the four new modules and added 14 fields to `PortfolioConfig` (see env table below). Three new helpers parse the env vars: `parse_bool_env`, `parse_f64_env`, `parse_u32_env`.
- [`src/portfolio/analyzer.rs`](../../src/portfolio/analyzer.rs) — added `RebalanceSignalConfig`, `RebalanceSignal`, `SOL_MINT`, and `generate_rebalance_signals`. The existing 7-day `generate_swap_suggestions` is unchanged and still drives the informational email alerts.
- [`src/portfolio/scanner.rs`](../../src/portfolio/scanner.rs) — added `load_keypair` (mirrors the existing `load_pubkey`), used by the rebalancer to sign Jupiter transactions.
- [`src/portfolio/watcher.rs`](../../src/portfolio/watcher.rs) — fetches Jupiter token decimals at startup, logs the snapshot baseline once, and invokes `rebalancer::maybe_rebalance(&ctx)` inside the existing tick body after `compute_risk` and before the alert/email block.

## Configuration (env vars)

All new env vars are **additive** — they're only read when
`ENABLE_AUTO_REBALANCE=true`. Defaults are conservative.

| Var | Default | Purpose |
|---|---|---|
| `ENABLE_AUTO_REBALANCE` | `false` | Master kill switch. |
| `REBALANCE_SIZE_FRACTION` | `0.25` | Fraction of the sell-side holding to rotate per trigger. |
| `REBALANCE_MIN_POSITION_EUR` | `25.0` | Skip the swap if either leg's current EUR value is below this floor. Filters dust positions where round-trip costs would dominate. |
| `REBALANCE_MAX_COST_BPS` | `50` | Abort if gas + price impact > 0.5 % of trade size. |
| `REBALANCE_MAX_SLIPPAGE_BPS` | `30` | Slippage tolerance passed to Jupiter `/quote`. |
| `REBALANCE_MAX_SWAPS_PER_DAY` | `2` | Hard cap on executions per rolling 24h. |
| `REBALANCE_HOLD_DAYS` | `14` | Same-pair reverse swap is blocked for this many days unless the take-profit hits. |
| `REBALANCE_TAKE_PROFIT_PCT` | `5.0` | If the bought asset is up by this % vs. its entry, the reverse swap is allowed before the hold expires. |
| `REBALANCE_LOOKBACK_DAYS` | `30` | Window for 30-day high/low. |
| `REBALANCE_REVERSAL_PCT` | `0.3` | Minimum 60-minute reversal to confirm "small increase / decline". |
| `REBALANCE_REVERSAL_WINDOW_MIN` | `60` | Lookback for the reversal-confirmation return. |
| `REBALANCE_EXTREME_WINDOW_HOURS` | `24` | The extreme must have been touched within this many hours to be actionable. |
| `REBALANCE_LOSS_HALT_DAYS` | `21` | Auto-halt the rebalancer if the portfolio is still below the latest snapshot value after this many days. Re-arm by deleting the halt file. |
| `JUPITER_API_URL` | `https://quote-api.jup.ag/v6` | Jupiter v6 base URL. |
| `REBALANCER_STATE_PATH` | `assets/rebalancer_state.json` | Execution history file. |
| `REBALANCER_SNAPSHOTS_PATH` | `assets/rebalancer_snapshots.jsonl` | Append-only pre-action portfolio snapshots. |
| `REBALANCER_HALT_PATH` | `assets/rebalancer_halt.json` | Persistent "halted due to persistent loss" marker. Present ⇒ all swaps blocked. |
| `REBALANCER_ACTIONS_PATH` | `assets/rebalancer_actions.jsonl` | Append-only audit log of every decision (skips with reasons, halts, dry-runs, executions). |
| `REBALANCE_REQUIRE_RECOVERY` | `true` | Block new actions until portfolio EUR value ≥ latest snapshot value ("wait to gain money"). |
| `REBALANCE_DRY_RUN` | `false` | If true, log the full BEFORE banner but skip signing / submitting / emailing / snapshot persistence. |

## Decision pipeline (step-by-step)

1. **Master switch.** If `ENABLE_AUTO_REBALANCE=false` → return immediately. No
   logs, no work.
1b. **Halt-file gate.** If `assets/rebalancer_halt.json` exists, log once
    per tick and return. The circuit breaker (step 2) writes this file; once
    present, the only way to resume is to delete it manually.
2. **Recovery gate + persistent-loss circuit breaker.** Read the last line of
   `rebalancer_snapshots.jsonl`. If `current_portfolio_eur < snapshot.total_eur`:
    - If the snapshot age exceeds `REBALANCE_LOSS_HALT_DAYS` (21 default) →
      **trip the circuit breaker.** Write a `HaltRecord` to
      `assets/rebalancer_halt.json` with the snapshot timestamp, deficit, and
      age in days, then send a one-time halt email. All future ticks exit at
      step 1b until the user deletes the halt file.
    - Otherwise → log the deficit and the days-until-halt countdown, and
      return. This is the "wait to gain money before any next action" rule.
3. **Daily cap.** Count executions in the last 24h. If at or over the cap → log
   and return.
4. **Signal generation** (`analyzer::generate_rebalance_signals`):
   - For every held asset, find the max and min of the price within
     `lookback_days` days of history, and note how long ago they were touched.
   - For each asset whose 30d high was touched in the last
     `extreme_window_hours` AND whose price has dropped by at least
     `reversal_pct` over the last `reversal_window_min` → sell candidate.
   - Mirror logic for buy candidates.
   - Cartesian product of (sell, buy), skipping self-pairs, sorted by sell-side
     EUR value descending.
5. **Per signal** (in priority order):
   1. **Min-position gate.** Both legs must satisfy `current_value_eur ≥
      REBALANCE_MIN_POSITION_EUR`. Cheapest check (just two comparisons), runs
      first to short-circuit dust-position swaps before any network work.
   2. **Hold cooldown.** If this exact `(sell_mint, buy_mint)` was traded
      within `REBALANCE_HOLD_DAYS` AND the buy-side asset has not yet appreciated
      by `REBALANCE_TAKE_PROFIT_PCT`, skip.
   3. **Quote.** `jupiter::quote(...)` with `sell_amount = holdings × size_fraction`.
   4. **Cost gate.** `gas_bps + price_impact_bps ≤ REBALANCE_MAX_COST_BPS`. Try
      the next signal on rejection.
6. **Snapshot.** Build a `PortfolioSnapshot` (live holdings + EUR prices +
   planned action + EUR total) and append to
   `rebalancer_snapshots.jsonl`. Skipped in dry-run mode so test runs never
   poison the recovery baseline.
7. **BEFORE banner.** Yellow stderr block: sell side, buy side, 30d extremes,
   60m reversals, and the full cost breakdown.
8. **Execute.** Load the keypair via `scanner::load_keypair`, hit
   `jupiter::swap`, decode the base64 v0 transaction, sign it, submit via
   `RpcClient::send_transaction`, then poll `get_signature_statuses` for up to
   45s for confirmation.
9. **Persist + record.** Append the `ExecutionRecord` to
   `rebalancer_state.json` (atomic temp + rename). Status is `confirmed` or
   `unconfirmed`.
10. **AFTER banner.** Green stderr block: tx sig, Solscan link, sold/bought
    amounts, realised cost, status.
11. **Execution email.** Sent through `emailer::send_alert` so it shares the
    existing SMTP plumbing but bypasses `ALERT_COOLDOWN_MIN`. Subject includes
    the pair and EUR size; body has the BEFORE / COST / AFTER / NEXT sections.

## Snapshot & restart recovery

`rebalancer_snapshots.jsonl` is append-only and crash-safe by construction.
Each line is one full `PortfolioSnapshot`:

```json
{
  "ts": 1737715200,
  "reason": "pre-swap",
  "sol_amount": 2.0,
  "tokens": [{ "mint": "...", "symbol": "NVDAx", "amount": 1.5 }],
  "prices_eur": { "SOL": 95.5, "NVDAx": 290.0 },
  "total_eur": 626.0,
  "planned_action": {
    "sell_symbol": "NVDAx", "sell_mint": "...", "sell_amount": 0.375,
    "buy_symbol":  "TSLAx", "buy_mint":  "...", "expected_buy_amount": 1.06
  }
}
```

On restart the watcher reads only the file's last well-formed line (O(1) via
4 KB tail-reads from disk). The number reads `info!("rebalancer: baseline =
€..., last action ts=...")` at startup. If the file is missing the log says
`no prior baseline, all gates open` and the recovery gate trivially passes
until the first swap creates a baseline.

A trailing newline or a partial line at the tail (from a crash mid-write) is
silently skipped — the next-to-last complete line is used instead.

**Design choice — permanent deficit lock + auto-halt.** If the portfolio
never recovers above the snapshot, the bot stops trading. After
`REBALANCE_LOSS_HALT_DAYS` (21 default), the recovery gate trips into a
persistent halt state by writing `assets/rebalancer_halt.json`. From that
point on, every tick exits at step 1b — no signal generation, no quote
requests, no trades — until a human deletes the halt file. A one-time
"AUTO-REBALANCE HALTED" email goes out at the moment the halt is written so
the operator is notified out-of-band.

## Loss-halt circuit breaker

The recovery gate alone is too soft if the strategy is genuinely broken — it
just waits forever. The halt gate complements it: it lets the strategy wait
for normal mean reversion (days, not weeks), then **stops trying** if the
position is still underwater after the configured horizon.

Flow:

```
recovery gate fails (live < snapshot)
        │
        ├── age < REBALANCE_LOSS_HALT_DAYS
        │     └── log deficit + countdown, return (keep waiting)
        │
        └── age ≥ REBALANCE_LOSS_HALT_DAYS
              ├── write rebalancer_halt.json (idempotent)
              ├── send one-time halt email
              └── return — every future tick exits at step 1b
```

**Re-arming** is intentionally manual. Delete `assets/rebalancer_halt.json`
after investigating. Consider whether to widen the asset universe, tighten
signal filters, or pause the strategy entirely (`ENABLE_AUTO_REBALANCE=false`)
before re-arming.

**The halt only applies when `ENABLE_AUTO_REBALANCE=true`.** Price tracking,
risk reporting, and the existing 7-day swap-suggestion email path continue
normally — the watcher itself never stops.

## Action log

Every decision the rebalancer makes is appended to
`assets/rebalancer_actions.jsonl` (one JSON object per line). This is the
audit trail — separate from the snapshot file and the execution state, so the
operator can answer "what did the bot do today?" without scrubbing
journalctl.

Logged variants (kept tagged via `serde(tag = "kind")` so each line is
self-describing):

- **`RecoveryWait`** — fired on every tick while the recovery gate is
  blocking. Carries the deficit, snapshot age, and days-until-halt.
- **`HaltTriggered`** — fired exactly once when the loss-halt circuit breaker
  trips. Includes the final deficit and snapshot age.
- **`ConsideredSignal`** — fired for every signal that reaches per-signal
  evaluation. The next line for the same `(sell, buy)` tells you the outcome.
- **`SkipMinPosition`** / **`SkipHoldCooldown`** / **`SkipCostGate`** —
  per-signal skips with the full numeric reason.
- **`DryRun`** — fired in `REBALANCE_DRY_RUN=true` mode in place of a real
  execution.
- **`Executed`** — fired for every real swap, with tx sig and confirmation
  status (`confirmed` / `unconfirmed`).

Sample lines:

```jsonl
{"ts":1737715200,"kind":"ConsideredSignal","sell":"NVDAx","buy":"TSLAx","sell_value_eur":1980.0,"buy_value_eur":510.0,"sell_decline_pct":0.74,"buy_rise_pct":1.0}
{"ts":1737715200,"kind":"SkipCostGate","sell":"NVDAx","buy":"TSLAx","total_cost_bps":85,"gas_bps":12,"slip_bps":73,"budget_bps":50}
{"ts":1737718800,"kind":"ConsideredSignal","sell":"NVDAx","buy":"TSLAx","sell_value_eur":1985.0,"buy_value_eur":512.0,"sell_decline_pct":0.74,"buy_rise_pct":1.0}
{"ts":1737718800,"kind":"Executed","sell":"NVDAx","buy":"TSLAx","sell_amount":0.375,"buy_amount":1.42,"total_cost_bps":32,"tx_sig":"5K...","status":"confirmed"}
```

The file is append-only. Failures to write a line are logged via `warn!` but
never abort a swap — the audit log can never block an action.

What's intentionally NOT logged: the chatty tick-level no-ops (master switch
off, halt already active, no signals, daily cap reached with no signals).
Those would dominate the file. They're still visible in the tracing
subscriber output if needed.

## Email notifications

Two email subjects exist for executions:

- `[portfolio-watcher] Swap executed: NVDAx → TSLAx (€156.50)` — when the
  RPC confirmation poll succeeds within 45s.
- `[portfolio-watcher] Swap UNCONFIRMED: <tx_sig>` — when submission
  succeeded but no confirmation arrived. The execution record is still saved
  with `status=unconfirmed`; the snapshot is on disk so the recovery gate
  blocks further trading until the user resolves the situation.

Pre-submit aborts (cost gate / hold cooldown / daily cap / recovery deficit)
**do not** email — they only log. The email surface is reserved for things
that touched on-chain state.

## Testing

15 new unit tests cover the rebalancer surface area:

- `analyzer::generate_rebalance_signals`
  - fires on (30d low + uptick) ⊕ (30d high + decline)
  - returns empty when the extreme is older than 24h
  - returns empty when there's no reversal in the 60m window
  - sorts paired signals by sell-side EUR value descending
  - populates both `sell_value_eur` and `buy_value_eur` from the risk report (so the min-position gate has data to work with)
- `rebalancer_snapshots`
  - append → latest round-trip
  - latest() of a multi-line file returns the last line
  - latest() skips trailing newlines
  - latest() returns None for a missing file
  - build() computes EUR total from USD prices + EUR rate correctly
- `rebalancer_state`
  - save → load round-trip
  - count_last_24h is bounded by `now − 86_400`
  - last_execution_of finds the latest matching pair
  - pnl_pct_since handles gains, losses, and divide-by-zero
  - halt round-trip (write → read → equality, missing file → None)
- `rebalancer`
  - iso_ts agrees with known unix timestamps
  - lamports_to_eur unit-conversion

Total: `cargo test` shows 40 lib tests passing (was 25 before), plus 113
MEV-bot tests unchanged.

## Operational checklist

Before flipping `ENABLE_AUTO_REBALANCE=true` for the first time:

1. Make sure the `.env` file has a real `WALLET_KEYPAIR_PATH` whose keypair
   has SOL for gas.
2. Run the watcher with `ENABLE_AUTO_REBALANCE=true` and
   `REBALANCE_DRY_RUN=true` for at least a day to observe the BEFORE banners
   without any on-chain action.
3. Inspect `assets/rebalancer_snapshots.jsonl` — in dry-run it stays empty.
4. Tighten `REBALANCE_SIZE_FRACTION` to `0.05`–`0.10` for the first live run
   and watch one full swap end-to-end on Solscan.
5. Verify the execution email arrived and the next tick correctly shows the
   recovery-gate baseline log line.
6. Confirm the reverse swap is blocked by the hold cooldown (or by the
   recovery gate if the swap lost money to costs).

## Caveats / known design choices

- **xStock liquidity.** The default 50 bps cost ceiling is binding on thin
  tokenised-stock pools. Lower `REBALANCE_SIZE_FRACTION` reduces price
  impact at the cost of slower portfolio rotation.
- **Single-tick atomicity.** The rebalancer runs to completion inside one
  tick. A new tick can't start until the swap confirms (or the 45s timeout
  fires). This keeps the snapshot and state files consistent.
- **No Jito-tip line item yet.** Jupiter chooses priority fees automatically
  via `dynamicComputeUnitLimit`. The `jito_tip_lamports` field exists in the
  execution record for future use but is currently always 0.
- **EUR rate refresh cadence.** The watcher refreshes the EUR rate every 10
  ticks. A swap fired during the gap uses the most recent rate, which is
  fine for cost-budget computations at sub-second precision.
- **Symbol vs. mint price keys.** The price store uses either the mint
  address or the symbol depending on which the pricer returned. The
  rebalancer tolerates both via the `if prices.contains_key(mint) { mint }
  else { symbol }` pattern, mirroring the rest of the codebase.

## Academic grounding

The 30-day lookback and ≥ 14-day hold horizon are taken from Dobrynskaya
(2023), which found that crypto markets show clean cross-sectional reversal
on horizons > 1 month — annualising > 100 % pre-cost on a universe of ~2 000
tokens. Sub-monthly horizons are contradicted by Liu & Tsyvinski (2021)
"Risks and Returns of Cryptocurrency" (time-series momentum at 1–7 day
horizons) and George & Hwang (2004) "The 52-Week High" (near-high
anchoring). Adams (2023) is the basis for the strict cost gating — empirical
Uniswap data shows price impact + gas routinely consumes the entire
pre-cost edge in thin-liquidity pools.

Full research notes:
[`~/.claude/plans/your-goal-is-when-toasty-lightning-agent-a417cf231479fac45.md`](../../../../.claude/plans/your-goal-is-when-toasty-lightning-agent-a417cf231479fac45.md).
