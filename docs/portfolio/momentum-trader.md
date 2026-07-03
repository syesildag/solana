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

- **Entry** (60s monitoring tick, only when FLAT): rank every watched token by the
  configured **`MOMENTUM_RANK_METRIC`** (default `sortino`; `slope_r2` recommended for
  clean trends — see [Metric selection](#metric-selection)) over `MOMENTUM_LOOKBACK_OBS`
  of 1-min price history; pick the highest; require its score > `MOMENTUM_MIN_METRIC`;
  swap a fixed `MOMENTUM_TRADE_USDC` of USDC into it. **Equity** tokens (xStocks/
  ETFs, auto-detected from the name) whose market is **closed** — price frozen
  over `MOMENTUM_STALE_MINUTES` — are skipped (shown as `closed` in the per-tick
  `rank[<metric>] —` log), so the bot never buys into a stale price that could gap on
  reopen. 24/7 crypto is never frozen-out (a calm low-vol token ≠ a closed one).
- **Hold / Exit** (fast `MOMENTUM_POLL_SECS` loop, only when HOLDING): fetch the
  held token's fresh price, track the **peak since entry**, and sell the whole
  position back to USDC when `price ≤ peak · (1 − MOMENTUM_TRAIL_PCT/100)` **or, for
  an equity, when its market closes** (price frozen over `MOMENTUM_STALE_MINUTES` →
  flatten rather than hold a frozen position across the close; the entry guard then
  keeps it FLAT until the market reopens, so this fires once per close, not in a
  churn). 24/7 crypto only ever exits on the trailing stop. Per-token `equity` in
  the watch list overrides the name-based auto-detection.
- **Rotate** (60s monitor tick, only when HOLDING): keep ranking all tokens; if the
  strongest one B beats the held A's score (in the active metric) by ≥ `MOMENTUM_ROTATE_MARGIN`
  (and B passes the entry gates), swap **directly A→B** in one atomic Jupiter transaction
  (no USDC leg) and carry the value into B. B's cost-basis is the **USDC value of the
  B actually received** (`expected_b × B_price`, which already nets the A→B price impact
  + swap fee); A's realized P&L is that value **minus the swap's gas**, so every cost —
  slippage, fee, and gas — flows into P&L and the loss breaker. (Gas hits only the
  realized side, not the basis, or it would cancel out across a rotation chain.) A is
  benched on rotation and a rotation counts against the daily cap — together with the
  margin, this prevents flip-flop churn. `MOMENTUM_ROTATE_MARGIN=0` disables it.
- One position at a time. After an exit (or rotation) the sold mint is benched for
  `MOMENTUM_REENTRY_COOLDOWN_SECS` to avoid churn. A loss-halt blocks new
  entries/rotations but still lets an open position exit (it won't be stranded).

## Dual cadence (why there are two loops)

The 60s monitoring tick is load-bearing for the alert engine, RSI, SMA, and 7-day
windows — they all assume **1-minute** snapshot cadence, and history is stored at
that rate. So the global tick is **not** sped up. Instead the trailing-stop EXIT
runs on a separate fast ticker (`MOMENTUM_POLL_SECS`, default 1s) that polls just
the **one held token** — cheap (1 request/sec) and tight. ENTRY ranking stays on
the 60s tick because the ranking metric over seconds is noise. Both run on the same task
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
| `MOMENTUM_RANK_METRIC` | `sortino` | Ranking metric: `sortino` \| `sharpe` \| `slope_r2` \| `return`. Picks what sorts + gates; all four are logged side-by-side each tick. `slope_r2` recommended — see [Metric selection](#metric-selection). |
| `MOMENTUM_MIN_METRIC` | `0.5` | Entry threshold — min score **in the active metric's units**. Recalibrate when changing the metric. |
| `MOMENTUM_ROTATE_MARGIN` | `0.5` | While holding, rotate into a token whose score beats the held one's by ≥ this (active metric's units; must clear the swap cost). `0` disables rotation. |
| `MOMENTUM_LOOKBACK_OBS` | `1440` | 1-min snapshots for the ranking window (≥120). |
| `MOMENTUM_STALE_MINUTES` | `20` | Equity close-guard: skip entry / flatten a held token whose price hasn't moved >0.1% in N min. **Equities only** (xStocks/ETFs, auto-detected from the name; 24/7 crypto is never flagged). `0` disables. |
| `MOMENTUM_POLL_SECS` | `1` | Held-token poll cadence for the trailing stop. |
| `MOMENTUM_REENTRY_COOLDOWN_SECS` | `3600` | Per-mint bench after an exit. |
| `MOMENTUM_MAX_TRADES_PER_DAY` | `4` | Daily entry cap. |
| `MOMENTUM_MAX_COST_BPS` | `100` | Entry rejected if gas+slippage exceeds (exit is unconditional). |
| `MOMENTUM_MAX_LOSS_USDC` | `0` | Loss circuit breaker: halt all trading once cumulative realized P&L hits −this USDC (`0` = disabled). |
| `MOMENTUM_SLIPPAGE_BPS` | `50` | Slippage tolerance to Jupiter. |
| `MOMENTUM_STATE_PATH` / `MOMENTUM_HALT_PATH` / `MOMENTUM_ACTIONS_PATH` / `MOMENTUM_PNL_PATH` | `assets/momentum_*` | State, circuit breaker, audit log, realized-P&L summary. |

### gRPC price feed (opt-in)

Enabled via `MOMENTUM_GRPC_PRICING=true` (see `.env.example` for the full wiring
requirements — `GRPC_ENDPOINT`, and a `pool`/`quote` per watched token in
`momentum_tokens.json`): prices wired tokens from on-chain pool state instead of REST,
falling back to REST per-mint when the gRPC price is missing/stale/distrusted.

| Var | Default | Purpose |
|---|---|---|
| `MOMENTUM_GRPC_STALE_SECS` | `30` | A gRPC price older than this falls back to REST for that mint. **`0` = trust-until-changed**: no TTL — an AMM price cannot move without an account write, so a decoded price is trusted indefinitely, gated by the cross-check below. |
| `MOMENTUM_GRPC_XCHECK_SECS` | `300` | Only when `MOMENTUM_GRPC_STALE_SECS=0`: per mint, at most this often, REST-fetch a gRPC-trusted price anyway and compare. `0` disables the cross-check. |
| `MOMENTUM_GRPC_XCHECK_BPS` | `100` | Divergence budget (gRPC vs. REST) before the cross-check distrusts the mint back to REST — until a fresh on-chain write or a later re-agreeing check clears it. Covers a dead gRPC stream or a price that migrated to a venue this bot doesn't watch. |
| `MOMENTUM_ENTRY_DIVERGENCE_BPS` | `0` | Entry/rotation-buy guard: skip if the Jupiter `/quote` implied fill price (`USD in / token out`) diverges from the live gRPC price by more than this many bps — the ranking signal was computed from a price that's since moved. `0` (default) = off, and the gRPC price isn't even fetched. Only fires with a trusted gRPC price available (present, not distrusted, and — outside trust-until-changed mode — within `MOMENTUM_GRPC_STALE_SECS`); otherwise the guard is skipped, not the trade. |

### Metric selection

`MOMENTUM_RANK_METRIC` chooses how tokens are ranked and how the entry/rotation gates
compare. All four are computed and logged side-by-side every tick (`so`/`sh`/`sl`/`rt`,
with `*` on the active one), so you can watch which best separates real trend from noise
on your own tokens before switching. Over a window of log-returns `r` (and `(ts, price)`
for slope):

| Metric | Formula | Notes |
|---|---|---|
| `sortino` (default) | `mean(r) / downside_dev(r)` | Risk-adjusted, only penalizes downside. **Divisor floors to ~0 for a token with no down-ticks → score explodes** (rewards absence-of-noise, not trend). |
| `sharpe` | `mean(r) / stdev(r)` | Like Sortino but total volatility → stays finite on a one-way riser (variance only collapses if returns are *constant*). |
| `slope_r2` (recommended) | annualized OLS slope of `ln(price)` vs time `× R²` | Clenow's "Stocks on the Move" ranker. Rewards a steep **and clean** trend; a choppy/gappy move gets a low R² and is penalized. Bounded — no floor explosion. |
| `return` | `Σ r` = `ln(P_last / P_first)` | Plain cumulative log return. No divisor, no pathology; a hard baseline. |

**⚠ Recalibrate the gates when you change the metric.** `MOMENTUM_MIN_METRIC` and
`MOMENTUM_ROTATE_MARGIN` are in the **active metric's units**, and the scales differ
wildly — a `0.5` that's a sane Sortino floor is a weak bar for `slope_r2` (≈0–5) and
effectively unreachable for cumulative `return` (≈±0.1). Switching the metric without
re-tuning them **will** mis-gate (enter on weak trends, or never enter). Sane starting
points (1-min bars, ~1-day lookback):

| Metric | Typical range | `MIN` start | `ROTATE_MARGIN` start |
|---|---|---|---|
| `sortino` / `sharpe` | ~ −1 … +1 | `0.5` | `0.5` |
| `slope_r2` | ~ 0 … 5 | `1.0` | `0.5` |
| `return` | ~ −0.1 … +0.1 | `0.01` | `0.005` |

Non-default metrics log a one-time startup warning reminding you to recalibrate; the
per-tick `rank[<metric>] — … (min X)` line shows the active metric, all scores, and the
current threshold so miscalibration is visible immediately.

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
| `assets/momentum_halt.json` | Circuit breaker — while present, new entries/rotations are blocked (an open position can still exit). Delete to re-arm. |
| `assets/momentum_actions.jsonl` | Append-only audit: one line per decision (the "why did/didn't it act"). |
| `assets/momentum_pnl.json` | Cumulative realized P&L: net USDC, %, win/loss, win-rate, best/worst. Recomputed from the trade ledger after each closed trade. |

**P&L tracking.** Each closed trade is a `TradeRecord` in `momentum_state.json` (the immutable
ledger). On every exit the bot recomputes the cumulative realized summary, logs it, writes
`momentum_pnl.json`, and includes it in the exit email (**live trades only — paper
trades log + write the sidecar but never email**). While HOLDING, each monitor tick logs the
open position's unrealized PnL. Realized PnL is `Σ(usdc_out − usdc_in)` — net of swap costs, since
those amounts are the actual quote proceeds.

## Safety

- **Switching paper↔live is safe.** Every position carries the `dry_run` flag it was
  opened with. If it disagrees with `DRY_RUN_MOMENTUM_TRADER`, the position belongs to
  the other mode and can't be managed here (paper mode would never sell the real tokens
  a live position holds; live mode would try to sell paper tokens never bought). At
  **startup** the trader detects this and **ignores the persisted position, resetting to
  FLAT** for the current mode (logged as `ignoring persisted … position … resetting to
  FLAT`); the real wallet holding, if any, is left untouched. A mid-run mismatch (e.g. a
  hand-edited state file) is still refused per-tick as a backstop. So you can just flip
  the flag and restart — no need to be FLAT or delete `momentum_state.json` first.
- **Exit sells the on-chain balance** (live), not a stale recorded amount, so a
  worse-than-expected entry fill can't oversize the sell and revert.
- **The wallet is re-scanned on-chain every ~5 min** (not just at startup), so funding
  the wallet or swapping outside the bot is reflected without a restart — the entry gate
  reads the freshly-scanned USDC balance. (You don't have to restart after topping up.)
- **Realized P&L is net of every swap cost.** The Jupiter quote's output already
  reflects price impact + swap fee (paper mode hits the real `/quote`, just never
  `/swap`), and on top of that the swap's estimated **gas** (≈2 base fees + a priority
  buffer, valued in USDC at the live SOL price) is charged too: it's subtracted from the
  realized USDC of each close (exit *and* rotation A-leg) and folded into the entry's
  cost basis, so a full round trip nets the gas of *every* swap. The P&L the loss
  breaker sees is therefore the true net, and paper P&L predicts live P&L (gas is
  modeled even in dry-run for that reason). Gas hits the realized/basis side only —
  never a carried-forward basis mid-chain, which would cancel it out.
- **Trailing-stop only, 60s/poll granularity** — a gap-down between polls can exit
  below the nominal stop. No hard intra-poll floor. Quotes are not pre-simulated.
- **Loss circuit breaker** (`MOMENTUM_MAX_LOSS_USDC`) — checked after each close
  (exit or rotation): once cumulative realized P&L (net sum of all closed trades,
  swap costs included) reaches −N USDC, the bot writes `momentum_halt.json`, which
  **blocks new entries and rotations** while still letting an open position exit
  (so a rotation that trips the halt can't strand you in the new token). A winning
  trade can pull the running total back above −N before it ever trips. `0` disables;
  recommended for live trading.
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
