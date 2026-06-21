# On-chain market-neutral pairs trader

A trading subsystem for the one strategy the backtests proved has a robust edge:
**market-neutral pairs on correlated xStocks** — long the statistically-cheap leg
on spot, short the rich leg by borrowing it on Kamino, and trade the `ln(A/B)`
spread back to its mean.

> **Status: Phase 2a (paper foundation) shipped.** The engine runs in dry-run,
> computing the live signal and simulating fills. **No on-chain execution yet** —
> Kamino borrow + DEX legs are Phase 2b–2d, planned but not built. See
> [plans/2026-06-21-onchain-pairs-trader.md](superpowers/plans/2026-06-21-onchain-pairs-trader.md).

---

## Why this strategy (the evidence)

From the `momentum-sim` investigation ([momentum-sim.md](momentum-sim.md)):
momentum, mean-reversion, and long-only relative value all returned **0 robust
configs** on 43 days of history. Market-neutral pairs returned **13** — and only
on correlated xStocks (NVDAx/SPYx, GOOGLx/NVDAx, QQQx/NVDAx, …).

**The hedge is the edge.** Long-only relative value (same signal, no short) failed
where the hedged version succeeded — proving the convergence profit lives in
shorting the rich leg, which cancels the market drift that sinks every long-only
strategy in a down/choppy regime. The edge **survives borrow costs to ~30% APY**.

**On-chain unlock confirmed:** Kamino runs a live, ~92%-utilized xStocks lending
market — you can borrow the xStock token itself ("deposit USDC, borrow tokenized
equities, take a directional position against SPYx/NVDAx, onchain"). Kraken also
offers xStock perps (long/short, 20×) but those are **CEX/custodial**, not
on-chain; on-chain perp DEXs (Drift/Jupiter) don't list xStocks.

---

## The strategy

- **Signal:** z-score of the `ln(A/B)` spread over a lookback window. `z < 0` ⇒ A
  is cheap relative to B ⇒ long A / short B; `z > 0` ⇒ the reverse.
- **Entry:** when `z_entry ≤ |z| < z_stop` (stretched but not broken).
- **Exit:** when `|z| ≤ z_exit` (reverted) or `|z| ≥ z_stop` (relationship broke).
- **Market-neutral & cross-margined (Phase 2b design):** post `USDC + the long-leg
  xStock` as collateral and borrow the `short-leg xStock` against it in ONE Kamino
  obligation. Kamino's health factor then nets the hedge — if the rich leg rallies
  (short loses), the deposited long leg gains and props up health. Self-hedging at
  the liquidation layer, not just in P&L.

This logic is identical between the backtest (`sim::replay_pairs`) and the live
engine (`pairs_signal::pair_decision`) — verified line-for-line.

---

## Phase 2a architecture (paper mode — shipped)

| Module | Responsibility |
|---|---|
| `src/portfolio/pairs_config.rs` | `PairSpec`, `PairsConfig` (env + `assets/pairs.json`) |
| `src/portfolio/pairs_signal.rs` | pure `pair_decision`, `estimate_health_factor`, `borrow_apy_ok` |
| `src/portfolio/pairs_state.rs` | `PairPosition`, `PairTradeRecord`, `PairsTraderState` + atomic persistence |
| `src/portfolio/pairs_trader.rs` | engine: live z-spread → decision → simulated fill → persist |
| `src/portfolio/watcher.rs` | calls `pairs_trader::tick` each 60s loop, gated by config |

The engine holds at most one pair at a time, applies cooldown + daily-cap gates,
and (in dry-run) simulates a dollar-neutral fill. It performs **zero** on-chain
calls — verified by review.

---

## Configuration (env vars)

All default to safe/off. See `.env.example`.

| Var | Default | Meaning |
|---|---|---|
| `ENABLE_PAIRS_TRADER` | `false` | Master switch |
| `DRY_RUN_PAIRS_TRADER` | `true` | Paper mode (no on-chain calls) |
| `PAIRS_PATH` | `assets/pairs.json` | Pair list `[{symbol_a,mint_a,symbol_b,mint_b}]` |
| `PAIRS_LOOKBACK_OBS` | `240` | z-score window |
| `PAIRS_Z_ENTRY` | `2.0` | Enter when `|z| ≥` this |
| `PAIRS_Z_EXIT` | `0.5` | Exit when `|z| ≤` this |
| `PAIRS_Z_STOP` | `4.5` | Stop when `|z| ≥` this |
| `PAIRS_TRADE_USDC` | `50` | Notional per leg |
| `PAIRS_REENTRY_COOLDOWN_SECS` | `3600` | Per-pair bench after a close |
| `PAIRS_MAX_TRADES_PER_DAY` | `6` | Rolling 24h entry cap |
| `PAIRS_MAX_BORROW_APY_PCT` | `30` | Skip/close if live borrow APY exceeds (Phase 2b gate) |
| `PAIRS_MIN_HEALTH_FACTOR` | `1.5` | Min Kamino health to open (Phase 2b gate) |
| `PAIRS_SLIPPAGE_BPS` | `50` | Per-leg slippage assumption |

`assets/pairs.json` is local config (gitignored, like `momentum_tokens.json`).

---

## Paper-trade it now

```bash
ENABLE_PAIRS_TRADER=true DRY_RUN_PAIRS_TRADER=true cargo run --release --bin solana-mev
# logs: "pairs(paper): OPEN NVDAx/SPYx z=-2.41 long NVDAx short SPYx"
#       "pairs(paper): CLOSE NVDAx/SPYx z=0.30 simulated pnl=+1.84 USDC"
# state persisted to assets/pairs_state.json
```

Cross-check the live opens/closes against the backtest over the same window:
```bash
./target/release/momentum-sim run --strategy pairs
```
They should agree directionally. (Paper P&L omits the short-leg borrow cost — it's
directionally correct but optimistic; see the `TODO(Phase 2b)` in
`simulate_pair_pnl`.)

---

## Roadmap — Phase 2b–2d (planned, not built)

Full task-by-task plan:
[plans/2026-06-21-onchain-pairs-trader.md](superpowers/plans/2026-06-21-onchain-pairs-trader.md).

- **2b — Kamino `klend` plumbing (BUILD, hand-rolled Rust instructions):**
  borrow/repay/deposit/withdraw + obligation health read; cross-margin proof on
  tiny funds. *Account layouts must be derived from the live IDL.*
- **2c — DEX legs + orchestration:** slippage-capped Jupiter swaps; open
  (long-first, then borrow+short) and close sequences with rollback.
- **2d — Live, minimal size:** risk layer (health floor, borrow-APY gate, loss
  circuit breaker) + one pair at $5 notional, then scale.

### Before any Phase 2b live work
- Confirm on-chain xStock access from your jurisdiction (xStocks are gated).
- Pull the live Kamino borrow APY and re-validate:
  `momentum-sim run --strategy pairs --pair-funding-bps-day <APY÷365×100>` — if it
  still shows `≥1 ROBUST`, it's a go.

### Known risks
- **NVDAx-concentration** — re-validate as data grows.
- **Inter-leg exposure** — briefly unhedged between open legs.
- **Liquidation** on the borrowed short — the cross-margin structure mitigates it.
- **xStock pool slippage** at larger size — start tiny.

---

## Tests
`cargo test --lib pairs` (11 unit tests across the four modules). The pure
decision/health/P&L functions are fully covered; `tick` is integration glue
verified by build + the paper run above.
