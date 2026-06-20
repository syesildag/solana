# Momentum Trader

A single-position, momentum/trend-following swap bot that lives inside the
`portfolio-watcher` binary. It holds **USDC**, rotates into the strongest watched
token, rides the gain, and **trail-stops** back to USDC. Off by default.

> It is the strategic inverse of the (removed) auto-rebalancer: that sold highs /
> bought lows (mean reversion); this buys strength and cuts weakness (momentum).
> They share the same Jupiter execution + state plumbing.

## Strategy

```
FLAT (USDC) ──entry──► HOLDING (one token) ──trailing stop──► FLAT ──► …forever
```

- **Entry** (60s monitoring tick, only when FLAT): rank every watched token by
  **Sortino ratio** (risk-adjusted momentum) over `MOMENTUM_LOOKBACK_OBS` of 1-min
  price history; pick the highest; require its Sortino > `MOMENTUM_MIN_SORTINO`;
  swap a fixed `MOMENTUM_TRADE_USDC` of USDC into it.
- **Hold / Exit** (fast `MOMENTUM_POLL_SECS` loop, only when HOLDING): fetch the
  held token's fresh price, track the **peak since entry**, and sell the whole
  position back to USDC the moment `price ≤ peak · (1 − MOMENTUM_TRAIL_PCT/100)`.
- One position at a time. After an exit the sold mint is benched for
  `MOMENTUM_REENTRY_COOLDOWN_SECS` to avoid churn.

## Dual cadence (why there are two loops)

The 60s monitoring tick is load-bearing for the alert engine, RSI, SMA, and 7-day
windows — they all assume **1-minute** snapshot cadence, and history is stored at
that rate. So the global tick is **not** sped up. Instead the trailing-stop EXIT
runs on a separate fast ticker (`MOMENTUM_POLL_SECS`, default 1s) that polls just
the **one held token** — cheap (1 request/sec) and tight. ENTRY ranking stays on
the 60s tick because Sortino over seconds is noise. Both run on the same task
(`tokio::select!`), so they serialize through `momentum_state.json` and never race.

Realized exit speed is bounded by the price source: DexScreener refreshes every
few seconds, so a 1s poll often re-reads the same value. Truly sub-second exits
would need on-chain/gRPC reads (deliberately out of scope).

## Independence

- Runs in `portfolio-watcher`, **not** the `solana-mev` arb bot. Disable each
  independently: don't run the other binary, or set `ENABLE_MOMENTUM_TRADER=false`
  (then the watcher is a pure monitor/alert bot).
- Uses the **public** Jupiter REST via `MOMENTUM_JUPITER_API_URL` — **no Metis
  binary, no `--binary-key`**, and unaffected by `ENABLE_JUPITER`. If you have no
  Metis key, the arb bot's Jupiter source should be off (`ENABLE_JUPITER=false`);
  this trader still works.

## Configuration

All env vars (see `.env.example`). Master switch `ENABLE_MOMENTUM_TRADER=false`.

| Var | Default | Purpose |
|---|---|---|
| `ENABLE_MOMENTUM_TRADER` | `false` | Master switch. |
| `DRY_RUN_MOMENTUM_TRADER` | `true` | Paper mode: real `/quote`, never `/swap`. Own flag, independent of the arb bot's `DRY_RUN`. |
| `MOMENTUM_JUPITER_API_URL` | `https://lite-api.jup.ag/swap/v1` | Jupiter free public REST (no key, no Metis); client appends `/quote` + `/swap`. |
| `MOMENTUM_TOKENS_PATH` | `assets/momentum_tokens.json` | Hand-curated `[{symbol, mint}]` watch list. |
| `MOMENTUM_TRADE_USDC` | `50.0` | Fixed USDC per entry. |
| `MOMENTUM_TRAIL_PCT` | `8.0` | Trailing-stop width. |
| `MOMENTUM_MIN_SORTINO` | `0.5` | Entry threshold. |
| `MOMENTUM_LOOKBACK_OBS` | `1440` | 1-min snapshots for entry Sortino (≥120). |
| `MOMENTUM_POLL_SECS` | `1` | Held-token poll cadence for the trailing stop. |
| `MOMENTUM_REENTRY_COOLDOWN_SECS` | `3600` | Per-mint bench after an exit. |
| `MOMENTUM_MAX_TRADES_PER_DAY` | `4` | Daily entry cap. |
| `MOMENTUM_MAX_COST_BPS` | `100` | Entry rejected if gas+slippage exceeds (exit is unconditional). |
| `MOMENTUM_MAX_LOSS_USDC` | `0` | Loss circuit breaker: halt all trading once cumulative realized P&L hits −this USDC (`0` = disabled). |
| `MOMENTUM_SLIPPAGE_BPS` | `50` | Slippage tolerance to Jupiter. |
| `MOMENTUM_STATE_PATH` / `MOMENTUM_HALT_PATH` / `MOMENTUM_ACTIONS_PATH` / `MOMENTUM_PNL_PATH` | `assets/momentum_*` | State, circuit breaker, audit log, realized-P&L summary. |

### Watch list (`assets/momentum_tokens.json`)

A JSON array of `{ "symbol", "mint" }` (an optional `"name"` is used in logs/emails).
Distinct from `portfolio.json` (auto-generated holdings — never hand-edit that). Each
token needs a Jupiter route; tokens with no DexScreener price simply never accumulate
history and are skipped. USDC is ignored if listed.

Add entries with the resolver script (resolves ticker/name/mint via Jupiter's verified
list and writes `symbol`+`name`+`mint`, refusing look-alike scams):

```bash
node scripts/add_momentum_token.js MET                # ticker/name → Meteora
node scripts/add_momentum_token.js <mint>             # mint → enriches symbol+name
```

**Warm-up / startup.** A token can only be ranked once it has **>120** one-minute
observations. History is loaded from `price_history.jsonl` at boot, so a previously-tracked
token is ready immediately. For a brand-new token:

- **With `BIRDEYE_API_KEY`:** at startup the trader fetches ~7 days of 1-min candles and
  **grafts** them onto the existing snapshot grid (`graft_mint_backfill` — forward-fill, no
  new snapshots, so the alert engine's count-based windows are untouched). On a
  continuously-running bot the recent grid is dense (~10,080 snapshots/7d), so the graft
  fills the whole window and the token is tradeable **at boot**. If the bot's *recent*
  history is sparse (just deployed, or restarted after downtime), the graft fills what
  exists and the rest accrues live.
- **Without a key:** it warms up **live over ~2 h** (1 snapshot/min), logging
  `momentum: no entry candidate yet … warming up` until ready.

## Files

| Path | Role |
|---|---|
| `assets/momentum_state.json` | The single open position, per-mint cooldowns, closed-trade log. |
| `assets/momentum_halt.json` | Circuit breaker — while present every tick short-circuits. Delete to re-arm. |
| `assets/momentum_actions.jsonl` | Append-only audit: one line per decision (the "why did/didn't it act"). |
| `assets/momentum_pnl.json` | Cumulative realized P&L: net USDC, %, win/loss, win-rate, best/worst. Recomputed from the trade ledger after each closed trade. |

**P&L tracking.** Each closed trade is a `TradeRecord` in `momentum_state.json` (the immutable
ledger). On every exit the bot recomputes the cumulative realized summary, logs it, writes
`momentum_pnl.json`, and includes it in the exit email (**live trades only — paper
trades log + write the sidecar but never email**). While HOLDING, each monitor tick logs the
open position's unrealized PnL. Realized PnL is `Σ(usdc_out − usdc_in)` — net of swap costs, since
those amounts are the actual quote proceeds.

## Safety

- **Switching paper↔live requires being FLAT.** A paper position carries a `dry_run`
  flag; if it disagrees with `DRY_RUN_MOMENTUM_TRADER` the trader refuses to act
  (it would otherwise try to sell tokens never bought). Be FLAT or delete
  `momentum_state.json` before flipping the flag.
- **Exit sells the on-chain balance** (live), not a stale recorded amount, so a
  worse-than-expected entry fill can't oversize the sell and revert.
- **Trailing-stop only, 60s/poll granularity** — a gap-down between polls can exit
  below the nominal stop. No hard intra-poll floor. Quotes are not pre-simulated.
- **Loss circuit breaker** (`MOMENTUM_MAX_LOSS_USDC`) — checked after each exit: once
  the cumulative realized P&L (net sum of all closed trades) reaches −N USDC, the bot
  writes `momentum_halt.json` and every subsequent tick short-circuits until you delete
  it. A winning trade can pull the running total back above −N before it ever trips.
  `0` disables it. Recommended for live trading.
- Start with `DRY_RUN_MOMENTUM_TRADER=true` (default) and small `MOMENTUM_TRADE_USDC`.

## Runbook

```bash
# 1. Paper-trade: prove the full loop with no funds at risk.
ENABLE_MOMENTUM_TRADER=true DRY_RUN_MOMENTUM_TRADER=true \
  cargo run --release --bin portfolio-watcher
#   → watched mints get priced; after warm-up an ENTRY logs (sig="dry-run");
#     momentum_state.json shows a dry_run position with a rising peak; tightening
#     MOMENTUM_TRAIL_PCT forces an EXIT with simulated usdc_out/pnl. portfolio.json
#     is untouched. tail assets/momentum_actions.jsonl to see every decision.

# 2. Tiny live trade (real funds): only after paper looks right. Start FLAT.
DRY_RUN_MOMENTUM_TRADER=false MOMENTUM_TRADE_USDC=10 MOMENTUM_MAX_TRADES_PER_DAY=1 \
  cargo run --release --bin portfolio-watcher

# Halt immediately: create the halt file.
echo '{"ts":0,"reason":"manual"}' > assets/momentum_halt.json   # delete to re-arm
```
