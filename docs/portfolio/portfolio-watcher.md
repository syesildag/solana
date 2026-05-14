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

When `assets/price_history.jsonl` has fewer than 60 entries (less than one hour of data), the watcher calls the **Birdeye API** to backfill the last 24 hours of 1-minute OHLC candles for each token. This gives the trend analysis meaningful data from the first minute of operation. Requires `BIRDEYE_API_KEY` (free signup at birdeye.so).

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
| `New7dHigh` | current price > max of previous 10 080 snapshots | — |
| `New7dLow` | current price < min of previous 10 080 snapshots | — |

A maximum of one alert email is sent per `ALERT_COOLDOWN_MIN` (default 30 minutes) regardless of how many assets trigger simultaneously.

---

## `portfolio-cli` Commands

```bash
# Scan wallet from scratch — overwrites assets/portfolio.json
cargo run --bin portfolio-cli --release -- init

# Re-scan and merge: updates amounts, removes sold tokens, appends new ones
cargo run --bin portfolio-cli --release -- update

# Show current holdings with live USD prices from Jupiter
cargo run --bin portfolio-cli --release -- show
```

`update` and the watcher's startup scan both call the same `scanner::merge()` function, which preserves the existing token ordering and any manual edits to symbols.

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
| [Jupiter Price API v2](https://price.jup.ag/v6/price) | None | Live USD prices every minute |
| [Jupiter Token List](https://token.jup.ag/all) | None | Resolving mint addresses → symbols at scan time |
| [Birdeye History API](https://public-api.birdeye.so/defi/history_price) | Free API key | 1-minute OHLC backfill on first run |

---

## Dependencies Added

```toml
lettre                 = { version = "0.11", features = ["tokio1", "tokio1-native-tls"] }
clap                   = { version = "4", features = ["derive"] }
solana-account-decoder = "2"
```
