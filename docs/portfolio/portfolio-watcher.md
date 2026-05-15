# Portfolio Watcher

A standalone tool suite that runs independently of the MEV bot. It tracks the USD value of your Solana wallet holdings every minute, persists price history to disk, detects significant price moves, and emails you when an opportunity is worth acting on.

---

## Components

Three separate binaries, all in the same Cargo workspace:

| Binary | Purpose | Run style |
|--------|---------|-----------|
| `portfolio-watcher` | Long-running price monitor + alert emailer | Background daemon |
| `portfolio-cli` | Wallet scanner — create or inspect `portfolio.json` | One-shot |
| `solana-mev` | MEV arbitrage bot | Unchanged — knows nothing about portfolio |

---

## Quick Start

```bash
# 1. Add your SMTP and alert settings to .env (see Configuration below)

# 2. Scan your wallet once (optional — the watcher also does this automatically)
cargo run --bin portfolio-cli --release -- init

# 3. Run the watcher
cargo run --bin portfolio-watcher --release
```

On startup the watcher scans your wallet automatically and refreshes `assets/portfolio.json`. You do not need to run `portfolio-cli` first.

---

## Architecture

```
portfolio-watcher
│
├── startup
│   └── scanner::scan_and_save()       RPC → SOL balance + SPL token accounts
│       ├── get_balance()               SOL lamports → f64 SOL
│       ├── get_token_accounts_by_owner() all non-zero SPL token accounts
│       ├── fetch_symbol_map()          Jupiter token list → mint→symbol
│       └── merge() / save()            update assets/portfolio.json
│
└── tick loop (every 60 s)
    ├── pricer::fetch_prices()          Jupiter Price API — batch USD prices
    ├── history::append_snapshot()     append to assets/price_history.jsonl
    ├── analyzer::analyze()            5-min %, 1-hour %, 7-day high/low
    └── emailer::send_alert()          SMTP (lettre, STARTTLS) — 30-min cooldown
```

### Price history backfill

When `assets/price_history.jsonl` does not cover the last 7 days, the watcher calls the **Birdeye API** to backfill up to 7 days of 1-minute candles for every asset. The result is persisted to disk immediately so future restarts skip this step entirely. Requires `BIRDEYE_API_KEY` (free tier at birdeye.so is sufficient).

---

## Source Layout

```
src/
├── lib.rs                      exposes `pub mod portfolio` to sibling binaries
├── bin/
│   ├── portfolio_watcher.rs    binary entry point — scan + run watcher
│   └── portfolio_cli.rs        binary entry point — init / update / show
└── portfolio/
    ├── mod.rs                  PortfolioConfig, Portfolio, TokenEntry types
    ├── scanner.rs              wallet RPC scan, symbol resolution, merge logic
    ├── pricer.rs               Jupiter live prices + Birdeye history backfill
    ├── history.rs              JSONL load / append, in-memory VecDeque
    ├── analyzer.rs             PriceSnapshot, Alert types, trend detection
    ├── emailer.rs              async SMTP via lettre
    └── watcher.rs              60s interval loop, cooldown guard, email body

assets/
├── portfolio.json              your holdings (written by scanner)
└── price_history.jsonl         append-only price log (written by watcher)
```

---

## Data Files

### `assets/portfolio.json`

Written by `scanner::scan_and_save()` on every `portfolio-watcher` startup and on every `portfolio-cli init/update`. Edit manually to adjust amounts between scans.

```json
{
  "sol_amount": 10.5,
  "tokens": [
    { "mint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "symbol": "USDC", "amount": 500.0 },
    { "mint": "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZnysDvCN",  "symbol": "JUP",  "amount": 100.0 }
  ]
}
```

### `assets/price_history.jsonl`

One JSON object per line, one line appended per minute. Survives crashes: a partial line at the tail is silently skipped on load. Capped at **10 080 entries in memory** (7 days at 1-minute intervals); the file itself grows unbounded and can be pruned manually.

```jsonl
{"ts":1715692800,"prices":{"SOL":168.42,"USDC":1.0,"JUP":0.87}}
{"ts":1715692860,"prices":{"SOL":169.10,"USDC":1.0,"JUP":0.88}}
```

---

## Trend Analysis

`analyzer::analyze()` runs on every tick against the in-memory price history. It checks each asset for:

| Alert | Condition | Default threshold |
|-------|-----------|------------------|
| `BigMove5m` | `\|pct_change over last 5 snapshots\|` ≥ threshold | `ALERT_PCT_5M=3.0` % |
| `BigMove1h` | `\|pct_change over last 60 snapshots\|` ≥ threshold | `ALERT_PCT_1H=10.0` % |
| `New7dHigh` | current price > max of previous 10 080 snapshots (requires ≥ 60 obs) | — |
| `New7dLow` | current price < min of previous 10 080 snapshots (requires ≥ 60 obs) | — |
| `ZScoreSpike` | `\|z-score\|` of latest return > threshold (requires warm EWMA) | `ALERT_ZSCORE_THRESHOLD=2.5` |

A maximum of one alert email is sent per `ALERT_COOLDOWN_MIN` (default 30 minutes) regardless of how many assets trigger simultaneously.

---

## Risk Metrics

`analyzer::compute_risk()` runs on every tick and produces a `RiskReport` that is logged to the console, included in alert emails, and printed by `portfolio-cli show`. It uses an **Exponentially Weighted Moving Average (EWMA)** over the 1-minute log-return series for each asset.

### Warming up

```
portfolio:   NVDAx    (warming 19/30)
```

The EWMA needs a minimum number of price observations before its statistics are reliable. Until that threshold is reached (`ALERT_ZSCORE_MIN_OBS`, default 30), the asset shows `(warming N/30)` and no z-score or volatility is reported. This prevents false alerts in the first minutes after a restart.

Once the watcher has run for 30 minutes — or after a Birdeye backfill — all assets go warm immediately.

### z — Z-score

```
portfolio:   SOL      z=+0.07  ...
```

How unusual is *this minute's* price move relative to the asset's own recent volatility?

```
z = (this_return − EWMA_mean) / sqrt(EWMA_variance)
```

| z value | Meaning |
|---------|---------|
| `0` | Completely normal move |
| `±1` | Mildly above/below average |
| `±2.5` | Alert threshold — statistically rare move |
| `> ±3` | Extreme spike or crash |

The score self-calibrates per asset using its own history. A 1% SOL move and a 1% NVDAx move are scored differently because their normal tick-to-tick volatility differs. The decay factor λ (`ALERT_ZSCORE_LAMBDA`, default 0.97) controls how quickly old data is forgotten — at 1-minute intervals, observations older than ~23 minutes carry less than 50% of their original weight.

### sigma_ann — Annualized volatility

```
portfolio:   SOL      ...  sigma_ann=173.2%  ...
```

The EWMA standard deviation of 1-minute log-returns scaled to a yearly figure:

```
sigma_ann = sqrt(EWMA_variance × 525_600) × 100   (525 600 = minutes in a year)
```

| Asset class | Typical range |
|-------------|--------------|
| Stablecoin (USDY) | 10–60% |
| Tokenised equity (NVDAx, GOOGLx) | 30–100% |
| Crypto (SOL, JitoSOL) | 80–200% |

A traditional equity like AAPL has ~25% annualised vol in normal conditions. Higher sigma means larger expected swings per minute.

### dd — Drawdown

```
portfolio:   NVDAx    ...  dd=3.1%  (EUR -3.20)
```

How far the current price is below the highest price seen in the history window:

```
current_drawdown_pct = (current_price − peak_price) / peak_price × 100   (≤ 0)
drawdown_eur         = (peak_value − current_value) in EUR                (≥ 0)
```

`dd=0%` means the asset is at or above its peak within the history window. `dd=-3.1%` means you are sitting 3.1% below the highest recorded price, equivalent to the EUR figure shown in parentheses. The "Total drawdown" line sums the EUR loss across all assets.

---

## `portfolio-cli` Commands

```bash
# Scan wallet from scratch — overwrites assets/portfolio.json
cargo run --bin portfolio-cli --release -- init

# Re-scan and merge: updates amounts, removes sold tokens, appends new ones
cargo run --bin portfolio-cli --release -- update

# Show current holdings in EUR with live prices + EWMA risk table
cargo run --bin portfolio-cli --release -- show

# Generate SVG price charts for every asset and portfolio total
cargo run --bin portfolio-cli --release -- plot
```

`update` and the watcher's startup scan both call the same `scanner::merge()` function, which preserves the existing token ordering and any manual edits to symbols.

---

## `plot` — SVG Price Charts

The `plot` command reads `assets/price_history.jsonl` and generates one SVG file per asset plus a portfolio total, saved to `assets/charts/`.

```
assets/charts/
├── SOL.svg
├── JitoSOL.svg
├── NVDAx.svg
├── GOOGLx.svg
├── TSLAx.svg
├── AAPLx.svg
├── QQQx.svg
├── SPYx.svg
├── USDY.svg
└── portfolio_total.svg
```

**Chart properties:**

| Property | Value |
|---|---|
| Format | SVG (scalable, open in any browser) |
| Dimensions | 900 × 380 px |
| X axis | Time elapsed from oldest snapshot (`0h` → `168h` for 7 days) |
| Y axis | Price in EUR |
| Max data points | 500 (downsampled from up to 10,080 raw snapshots) |
| Annotations | Red dot = historical low, green dot = historical high |

Each asset has a fixed color:

| Asset | Color |
|---|---|
| SOL | Purple |
| JitoSOL | Dark purple |
| NVDAx | Green |
| AAPLx | Dark gray |
| GOOGLx | Blue |
| TSLAx | Red |
| QQQx | Teal |
| SPYx | Orange |
| USDY | Green |
| Portfolio Total | Steel blue |

**Requires:** at least 2 snapshots in `price_history.jsonl`. Run `portfolio-watcher` first to accumulate history, or trigger a Birdeye backfill by setting `BIRDEYE_API_KEY`.

**Implementation:** `src/bin/portfolio_cli.rs` — `render_chart()` uses [`plotters`](https://docs.rs/plotters) v0.3 with the `svg_backend` feature. No extra runtime dependencies beyond what the watcher already uses.

---

## Configuration

All settings are read from `.env`. See `.env.example` for the full list with comments.

### Wallet & RPC (shared with MEV bot)

| Variable | Default | Purpose |
|----------|---------|---------|
| `RPC_URL` | `https://api.mainnet-beta.solana.com` | Solana RPC for balance + token account queries |
| `WALLET_KEYPAIR_PATH` | `~/.config/solana/id.json` | Keypair used to derive the wallet public key |

### File paths

| Variable | Default | Purpose |
|----------|---------|---------|
| `PORTFOLIO_PATH` | `assets/portfolio.json` | Holdings definition |
| `HISTORY_PATH` | `assets/price_history.jsonl` | Append-only price log |

### Price history

| Variable | Default | Purpose |
|----------|---------|---------|
| `BIRDEYE_API_KEY` | — | Free API key for 24h backfill on first run |

### Alert thresholds

| Variable | Default | Purpose |
|----------|---------|---------|
| `ALERT_PCT_5M` | `3.0` | % move in 5 minutes to trigger email |
| `ALERT_PCT_1H` | `10.0` | % move in 1 hour to trigger email |
| `ALERT_COOLDOWN_MIN` | `30` | Minimum minutes between emails |
| `ALERT_EMAIL` | — | Recipient address for alerts |

### EWMA risk metrics

| Variable | Default | Purpose |
|----------|---------|---------|
| `ALERT_ZSCORE_LAMBDA` | `0.97` | EWMA decay factor (half-life ≈ 23 min at 1-min ticks) |
| `ALERT_ZSCORE_THRESHOLD` | `2.5` | Z-score magnitude that triggers a `ZScoreSpike` alert |
| `ALERT_ZSCORE_MIN_OBS` | `30` | Observations required before z-score alerts fire |

### SMTP

| Variable | Example | Purpose |
|----------|---------|---------|
| `SMTP_HOST` | `smtp.gmail.com` | SMTP server (STARTTLS on port 587) |
| `SMTP_PORT` | `587` | SMTP port |
| `SMTP_USER` | `you@gmail.com` | Login username |
| `SMTP_PASSWORD` | `app-password` | Login password (use an app password for Gmail) |
| `SMTP_FROM` | `you@gmail.com` | From address in the email |

> **Gmail note:** Go to Google Account → Security → App Passwords and generate a password for "Mail". Use that as `SMTP_PASSWORD`.

---

## External APIs Used

| API | Auth | Used for |
|-----|------|---------|
| [Binance REST API](https://api.binance.com/api/v3/ticker/price) | None | SOL/USDC live price every minute |
| [DexScreener Token API](https://api.dexscreener.com/tokens/v1/solana) | None | SPL token live prices (batch, up to 30 mints) |
| [Jupiter Token List](https://token.jup.ag/all) | None | Resolving mint addresses → symbols at scan time |
| [Frankfurter (ECB)](https://api.frankfurter.app/latest) | None | USD → EUR exchange rate, refreshed every 10 min |
| [Birdeye History API](https://public-api.birdeye.so/defi/history_price) | Free API key | 7-day 1-minute candle backfill on first run |

---

## Dependencies Added

```toml
lettre                 = { version = "0.11", features = ["tokio1", "tokio1-native-tls"] }
clap                   = { version = "4", features = ["derive"] }
solana-account-decoder = "2"
```
