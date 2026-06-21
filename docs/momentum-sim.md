# `momentum-sim` — strategy backtest engine

A walk-forward backtest harness that replays recorded price history through the
**production** decision code to find — and honestly judge — trading-strategy
parameters. Built to answer "what is the best momentum rank metric and the env
vars supporting it?", then extended to test the strategies that question led to.

- **Binary:** `src/bin/momentum_sim.rs` (`cargo run --release --bin momentum-sim`)
- **Engine:** `src/portfolio/sim.rs` (pure, unit-tested; reused by the live trader)
- **Data:** `assets/price_history.jsonl` (the watcher's recorded snapshots)

---

## Quick start

```bash
cargo build --release --bin momentum-sim

# Full momentum grid (the original question), walk-forward, with verdict
./target/release/momentum-sim run

# Fast smoke subset
./target/release/momentum-sim run --quick

# Other strategies
./target/release/momentum-sim run --strategy meanrev
./target/release/momentum-sim run --strategy pairs --min-trades 3
./target/release/momentum-sim run --strategy relval
```

Each run prints a ranked table, a **VERDICT** line, writes a full-grid CSV, and
(for momentum) a ready-to-paste `.env` block for the best config.

---

## Why it exists / methodology

The bot's momentum trader (`src/portfolio/momentum.rs`) is governed by ~25
`MOMENTUM_*` env vars that were hand-tuned. This tool turns tuning into evidence.

Core design principles:

1. **Reuse the production decision code, don't reimplement it.** The signal
   functions (`compute_sortino`/`sharpe`/`slope_r2`/`return`, `rank_candidates`,
   `is_overextended`, `trailing_stop_triggered`, `fade_take_profit`,
   `rotation_target`, `is_stale_ts`, `est_gas_usdc`/`est_gas_bps`,
   `build_trade_record`) are called verbatim, so the backtest matches live
   behavior. Re-deriving them would be the #1 source of misleading results.

2. **Walk-forward split.** History is split into a **train** slice (default first
   70%) and a held-out **test** slice. Per-metric entry thresholds are derived
   from the train slice only; both slices are replayed. Results sort by held-out
   (`test`) P&L, with `train` shown beside it so over-fit is visible.

3. **Robustness verdict.** A config is **ROBUST** only if it is profitable in
   **both** slices AND trades at least `--min-trades` times in each (default 3) —
   so a single lucky trade can never masquerade as an edge. Every run prints
   `VERDICT: N/M configs ROBUST`.

4. **Isolated-spike data filter.** The history contains glitch prints (e.g. a
   token jumping ~5000× for one snapshot then back). `sanitize_history` drops a
   price that diverges from BOTH time-neighbors by more than `--max-step`
   (default 8×) — removing one-tick glitches while keeping real sustained moves.
   Without this, a single bad print fabricates a fake +476k "best" result.

5. **Conservative fills (with an optimistic bracket).** A tripped trailing stop
   fills at the *next* snapshot's price (~3 min later on this data) plus
   slippage + gas — pessimistic, never flattering. `--optimistic-fill` brackets
   the upper bound (same-bar fill). On this data the two differ by only ~$3, so
   execution timing was ruled out as the deciding factor.

6. **Performance factoring.** `rank_candidates` output depends only on
   `(lookback, max_run, decel, confirm_lag, metric)` — not on `trail`/`min_metric`.
   The expensive ranked stream is computed once per ranking tuple; the cheap
   state-machine knobs are swept on top. ~1280 combos run in seconds–minutes.

---

## Strategies (`--strategy`)

| Strategy | Idea | Swept knobs |
|---|---|---|
| `momentum` (default) | Rank tokens by a metric, ride the leader, trailing-stop out | metric, min_metric, trail_pct, lookback_obs, max_run_pct, rotate-factors, regime-obs |
| `meanrev` | Buy oversold (z ≤ −entry), sell on reversion to the mean | lookback, z_entry, z_exit, z_stop |
| `pairs` | Market-neutral: long the cheap leg + short the rich leg of a correlated pair, trade the `ln(A/B)` spread (Phase-0 edge check; shorting not modeled) | per-pair: lookback, z_entry, z_exit, z_stop; cost + funding |
| `relval` | Long-only spot capture of the pairs signal: buy only the cheap leg | per-pair: lookback, z_entry, z_exit, z_stop |

The momentum strategy also models **rotation** (one-swap A→B, gas on the A-leg)
and a **SOL>MA regime filter** (block entries while the broad market is risk-off).

---

## CLI reference (`run` subcommand)

| Flag | Default | Applies to | Purpose |
|---|---|---|---|
| `--train-frac <f>` | 0.70 | all | Train/test split fraction |
| `--quick` | off | all | Trim grid to a fast smoke subset |
| `--top <n>` | 20 | all | Rows to print |
| `--min-trades <n>` | 3 | all | Robustness gate: ≥ trades in BOTH slices |
| `--max-step <x>` | 8.0 | all | Spike filter factor (≤1 disables) |
| `--tokens <path>` | env | all | Override the watched/universe token list |
| `--history <path>` | env | all | Override the price-history file |
| `--csv <path>` | `assets/momentum_sim_results.csv` | all | Full-grid CSV output |
| `--strategy <s>` | momentum | all | `momentum｜meanrev｜pairs｜relval` |
| `--lookbacks a,b,c` | grid | all | Override the swept lookback windows |
| `--optimistic-fill` | off | momentum | Same-bar stop fill (upper bound) |
| `--rotate-factors a,b` | `0` | momentum | Rotation margin as ×min_metric (0 = off) |
| `--regime-obs a,b` | `0` | momentum | SOL>MA window to gate entries (0 = off) |
| `--pair-cost-bps <n>` | 15 | pairs | Per-leg trading cost (×4 per round-trip) |
| `--pair-funding-bps-day <f>` | 0 | pairs | Borrow/funding drag per day (live Kamino APY ÷ 365) |

---

## Findings (43 days of history, 18 tokens, ~184 s cadence)

The investigation that produced this tool, with its honest conclusions:

| Strategy | Robust configs | Notes |
|---|---|---|
| Momentum (incl. rotation, regime filter, both fill models) | **0** | All train-negative; "best" test rows are over-fit (train-loser or 1–2-trade flukes) |
| Mean-reversion | **0** | Worse — 6–17% win rate; dips kept falling (down-trend persistence) |
| Long-only relative value | **0** | The hedge *was* the edge — removing the short leg kills it |
| **Market-neutral pairs** | **13 / 720** | The only robust edge; all on correlated xStocks (NVDAx-centric) |

Key takeaways:
- **No long-only single-name timing strategy clears costs** on this sample — the
  period was a choppy/down regime where net-long exposure loses.
- **Momentum's low win rate at short lookback is a mean-reversion signature**, but
  buying the dips loses too — these tokens trend, choppily, mostly down.
- **The win rate rises with lookback**, but longer lookbacks just trade rarely and
  surface 1–2-trade flukes — not a broad robust plateau.
- **Market-neutral pairs works** because shorting the rich leg cancels the market
  drift and isolates the spread convergence. The edge **survives borrow costs to
  ~30% APY** (9–13 robust configs across 0→30% APY) — convergence is fast, so
  funding-per-day stays small relative to the move.
- **Caveat:** the pairs edge is **NVDAx-concentrated** (NVDA dispersion vs the
  indices) — partly regime, not a permanent law. 13 test days is one regime.
  Re-run as history grows.

This led directly to the on-chain pairs trader — see
[pairs-trader.md](pairs-trader.md).

---

## Output

- **stdout table** — top-N configs by held-out P&L (columns vary per strategy).
- **VERDICT line** — `N/M configs ROBUST`; if 0, the best-by-test rows are shown
  flagged as over-fit / not deployable.
- **CSV** — the full grid for offline pivoting.
- **`.env` block** (momentum, when a robust config exists) — paste-ready knobs.

---

## Limitations

- **`maxDD%` is misleading for all-losing runs** (drawdown as a % of a
  non-positive equity peak is undefined; it can read 0% or absurd values). Net
  P&L is the real objective.
- **Cadence ≠ 1 min.** History is ~184 s and irregular; per-bar metrics
  (sortino/sharpe/return) see longer windows than nominal. `slope_r2` uses real
  timestamps and is the only cadence-robust metric.
- **Coarse stops** — ~3-min snapshots can't see intra-bar dips; the conservative
  next-snapshot fill is a deliberate floor.
- **One regime** — 43 days. Treat any winner as "best supported by available
  data," and re-run as the recorder accumulates more.

---

## Code map

- `sim.rs::run_grid` / `run_grid_meanrev` / `run_grid_pairs` / `run_grid_relval` —
  walk-forward grid drivers.
- `sim.rs::replay_with_stream` / `replay_meanrev` / `replay_pairs` / `replay_relval` —
  per-strategy state machines.
- `sim.rs::ranked_stream` / `meanrev_stream` — cached per-snapshot candidate streams.
- `sim.rs::sanitize_history` — isolated-spike filter.
- `sim.rs::min_metric_candidates` — per-metric quantile thresholds.
- `sim.rs::SimResult::is_robust` / `config_is_robust` — the robustness gate.
- Tests: `cargo test --lib sim::` (19 unit tests).
