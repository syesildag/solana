pub mod analyzer;
pub mod emailer;
pub mod forward_report;
pub mod grpc_pricer;
pub mod history;
pub mod jupiter;
pub mod kamino;
pub mod liquidation;
pub mod liquidation_actions;
pub mod liquidation_config;
pub mod liquidation_signal;
pub mod liquidation_state;
pub mod momentum;
pub mod momentum_actions;
pub mod momentum_state;
pub mod momentum_universe;
pub mod pairs_actions;
pub mod pairs_config;
pub mod pairs_signal;
pub mod pairs_state;
pub mod pairs_trader;
pub mod pricer;
pub mod scanner;
pub mod sim;
pub mod suggestions;
pub mod ts_serde;
pub mod watcher;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

pub use momentum::RegimeMode;
pub use suggestions::RankMetric;

#[derive(Debug, Clone)]
pub struct PortfolioConfig {
    pub rpc_url: String,
    pub wallet_keypair_path: String,
    pub portfolio_path: String,
    pub history_path: String,
    pub birdeye_api_key: Option<String>,
    pub alert_pct_5m: f64,
    pub alert_pct_1h: f64,
    pub alert_cooldown_min: u64,
    pub zscore_lambda: f64,
    pub zscore_threshold: f64,
    pub zscore_min_obs: usize,
    /// Parsed from ALERT_PRICE_BELOW="USDY:0.96,SOL:70.0"
    pub price_thresholds: Vec<(String, f64)>,
    /// Parsed from ALERT_PRICE_ABOVE="USDY:1.04,SOL:300.0"
    pub price_ceilings: Vec<(String, f64)>,
    pub status_path: String,
    pub alert_email: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub smtp_password: String,
    pub smtp_from: String,

    // ----- Momentum trader (off by default) -----
    /// Master switch. When false the watcher is a pure monitor/alert bot.
    pub enable_momentum_trader: bool,
    /// Paper mode via the trader's OWN flag `DRY_RUN_MOMENTUM_TRADER` (independent
    /// of the arb bot's `DRY_RUN`): real `/quote`, never `/swap`.
    pub momentum_dry_run: bool,
    /// Public Jupiter REST endpoint — independent of the arb bot's Metis / `ENABLE_JUPITER`.
    pub momentum_jupiter_api_url: String,
    /// Fixed USDC notional committed per entry.
    pub momentum_trade_usdc: f64,
    /// Trailing-stop width: exit when price ≤ peak·(1 − pct/100).
    pub momentum_trail_pct: f64,
    /// Which metric ranks watched tokens + drives the entry/rotation gates
    /// (`MOMENTUM_RANK_METRIC`). Default `sortino` (historical behavior). All metrics
    /// are computed + logged side-by-side each tick regardless; this picks the one
    /// that sorts and gates. NOTE: the two thresholds below are in THIS metric's units.
    pub momentum_rank_metric: RankMetric,
    /// Entry requires the best candidate's score (in the active metric's units) to
    /// exceed this. Env: `MOMENTUM_MIN_METRIC`.
    pub momentum_min_score: f64,
    /// While holding, rotate into another token only if its score beats the held
    /// token's by at least this much (in the active metric's units; covers the swap
    /// cost; prevents churn). `0` disables rotation entirely.
    pub momentum_rotate_margin: f64,
    /// Over-extension entry guard: skip *buying* a token (fresh entry OR rotation
    /// target) whose lookback-window has already risen more than this many percent
    /// **and** whose trend is decelerating (see `momentum_decel_lookback_min`) — an
    /// exhausted run mean-reverts into the trailing stop, but a still-accelerating
    /// runner is left alone. Run measured off the `Return` metric (`e^ret − 1`). Env:
    /// `MOMENTUM_MAX_RUN_PCT`. `0` disables. NOTE: a momentum cap is a regime bet and
    /// this default is fit to a small trade sample — tune it as trades accumulate.
    pub momentum_max_run_pct: f64,
    /// Recent sub-window (minutes) for the over-extension *deceleration* check: a
    /// big-run token is only skipped when its ln-price slope over the last N minutes
    /// is below its slope over the whole lookback window (decelerating / topping).
    /// `0` disables the deceleration test → `MOMENTUM_MAX_RUN_PCT` becomes a pure run
    /// cap. Env: `MOMENTUM_DECEL_LOOKBACK_MIN`.
    pub momentum_decel_lookback_min: usize,
    /// Entry confirmation guard: refuse to enter (or rotate into) a token whose
    /// ranking metric is *lower* than it was this many observations ago — i.e. the
    /// trend's quality is rolling over even if price still ticks up. Compares the
    /// metric over the current lookback window vs the same-length window ending
    /// `N` obs earlier. `0` disables. Env: `MOMENTUM_CONFIRM_LAG_OBS`.
    pub momentum_confirm_lag_obs: usize,
    /// Adopt a manually-acquired wallet holding into the trader at startup: when FLAT
    /// (live mode) and the wallet holds exactly one watched token worth ≥ half the
    /// trade size, record it as the current position (entry/peak = current price, so
    /// the trailing stop and fade exit manage it from now — the real cost basis is
    /// unknown). Ambiguous (2+ large holdings) → skipped with a warning. Env:
    /// `MOMENTUM_ADOPT_WALLET_POSITION`. `false` (default) = never adopt.
    pub momentum_adopt_wallet_position: bool,
    /// Take-profit-on-fade: while holding a token that is **in profit**, exit to USDC
    /// once its active metric drops to or below `momentum_min_score` (momentum died but
    /// the trailing stop hasn't tripped yet). Losses are left to the trailing stop.
    /// Rotation takes precedence — this only fires when no rotation target qualifies.
    /// Env: `MOMENTUM_EXIT_ON_FADE`. `false` keeps the price-only exit behavior.
    pub momentum_exit_on_fade: bool,
    /// Number of trailing 1-min snapshots used for the entry Sortino. Must exceed
    /// 120 — a window of N prices yields N−1 returns and Sortino needs ≥120.
    pub momentum_lookback_obs: usize,
    /// Skip a token from entry if its price hasn't moved (>0.1%) over the last N
    /// minutes — i.e. its market is closed/halted/illiquid. `0` disables the check.
    pub momentum_stale_minutes: usize,
    /// Market-regime filter: block NEW entries unless SOL is above its moving average
    /// over this many trailing observations (risk-on). Exits are never blocked. `0`
    /// disables. Env: `MOMENTUM_REGIME_OBS`. Backtest it with
    /// `momentum-sim run --regime-obs ...` before enabling.
    pub momentum_regime_obs: usize,
    /// Which regime gate to use: `off | level | trend`. `level` = SOL>MA (original);
    /// `trend` = SOL slope_r2 clean-uptrend (regime momentum — backtests favor it).
    /// Both use `MOMENTUM_REGIME_OBS` as the window. Env: `MOMENTUM_REGIME_MODE`.
    pub momentum_regime_mode: RegimeMode,
    /// Min SOL slope_r2 for `trend` mode (only enter when SOL's clean-uptrend score
    /// clears this). Annualized slope×R² units — use `momentum-sim regime-compare` to
    /// find sane values. Ignored unless mode is `trend`. Env: `MOMENTUM_REGIME_TREND_MIN`.
    pub momentum_regime_trend_min: f64,
    /// Mean-reversion entry confirmation ("both true"): require the chosen strong token
    /// to ALSO be oversold — its z-score over the last `MOMENTUM_ENTRY_DIP_OBS`
    /// observations ≤ −`MOMENTUM_ENTRY_DIP_Z` — before entering (buy the pullback, not
    /// the top). `0` disables. Backtest-promising but UNVALIDATED; default off.
    pub momentum_entry_dip_obs: usize,
    pub momentum_entry_dip_z: f64,
    /// Overbought entry gate (mean-reversion filter): block a NEW entry when the
    /// candidate is extended *above* its own moving average — its z-score over the last
    /// `MOMENTUM_ENTRY_MAX_Z_OBS` observations exceeds `MOMENTUM_ENTRY_MAX_Z`. Only buys
    /// names trading at/below their recent mean (don't chase the top). `0` obs disables.
    /// Independent of the dip gate; both on ⇒ a band `−dip_z ≤ z ≤ max_z`.
    pub momentum_entry_max_z_obs: usize,
    pub momentum_entry_max_z: f64,
    /// Held-token price-poll cadence (seconds) for the trailing-stop loop.
    pub momentum_poll_secs: u64,
    /// Per-mint bench after an exit before it can be re-bought (seconds).
    pub momentum_reentry_cooldown_secs: i64,
    /// Max entries allowed in any rolling 24h window.
    pub momentum_max_trades_per_day: u32,
    /// Reject an entry/exit if gas+slippage exceeds this many bps.
    pub momentum_max_cost_bps: u32,
    /// Loss circuit breaker: halt all momentum trading once cumulative realized
    /// P&L (sum of every closed trade) falls to −this many USDC. `0` disables it.
    pub momentum_max_loss_usdc: f64,
    /// Slippage tolerance passed to `jupiter::quote`. The first exit attempt and
    /// every entry use this; exits escalate from here on consecutive reverts.
    pub momentum_slippage_bps: u32,
    /// Ceiling for the exit's self-escalating slippage. An exit is unconditional,
    /// so on each revert (typically `0x1771` on a volatile token) the next attempt
    /// widens its min-out cushion up to this cap, then holds there and keeps trying.
    pub momentum_exit_slippage_cap_bps: u32,
    /// Ceiling for the entry's self-escalating slippage. An entry is *optional*
    /// (a failed buy just stays FLAT), so this caps tight — chase a fast mover a
    /// little to get filled, but never wide enough to buy a blowoff top.
    pub momentum_entry_slippage_cap_bps: u32,
    pub momentum_tokens_path: String,
    pub momentum_state_path: String,
    pub momentum_halt_path: String,
    pub momentum_actions_path: String,
    /// Realized-PnL summary sidecar (JSON), rewritten after each closed trade.
    pub momentum_pnl_path: String,

    // ----- Live token discovery (momentum overlay; opt-in) -----
    /// Master switch. When false, no scanning happens and the momentum universe
    /// is exactly the curated file (zero behavior change). Env `MOMENTUM_SCAN_ENABLE`.
    pub momentum_scan_enable: bool,
    /// Seconds between scans. The watcher runs `scan_tokens.js --json` this often
    /// (floored to one 60s monitor tick). Env `MOMENTUM_SCAN_INTERVAL_SECS` (3600).
    pub momentum_scan_interval_secs: u64,
    /// How many top-by-volume discoveries to keep in the rolling in-memory overlay.
    /// Env `MOMENTUM_SCAN_TOP_N` (3).
    pub momentum_scan_top_n: usize,
    /// Path to the Node scanner spawned each interval. Env `MOMENTUM_SCAN_SCRIPT`.
    pub momentum_scan_script: String,

    // ----- Multi-slot position management -----
    /// Max concurrent momentum positions (slots). `1` = single-slot (default, identical to
    /// the original trader); >1 enables the multi-slot trader (Tasks 2–6).
    /// Env: `MOMENTUM_MAX_POSITIONS`. Clamped to ≥1.
    pub momentum_max_positions: usize,

    // ----- gRPC pricer for momentum trader (opt-in) -----
    /// Master switch for gRPC price feed. When false, the momentum trader uses
    /// Jupiter REST quotes. Env: `MOMENTUM_GRPC_PRICING` (default false).
    pub momentum_grpc_pricing: bool,
    /// Yellowstone gRPC endpoint for price feed. Optional; required if
    /// `MOMENTUM_GRPC_PRICING` is true. Env: `GRPC_ENDPOINT`.
    pub grpc_endpoint: Option<String>,
    /// Yellowstone gRPC authentication token. Optional; may be required by some
    /// endpoints. Env: `GRPC_TOKEN`.
    pub grpc_token: Option<String>,
    /// Path to the pools config file (populated by `fetch_all.js`).
    /// Env: `POOLS_PATH` (default "pools.json").
    pub pools_path: String,
    /// Staleness threshold for gRPC price updates (seconds).
    /// Env: `MOMENTUM_GRPC_STALE_SECS` (default 30).
    pub momentum_grpc_stale_secs: u64,
}

impl PortfolioConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            rpc_url: std::env::var("RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string()),
            wallet_keypair_path: std::env::var("WALLET_KEYPAIR_PATH")
                .unwrap_or_else(|_| "~/.config/solana/id.json".to_string()),
            portfolio_path: std::env::var("PORTFOLIO_PATH")
                .unwrap_or_else(|_| "assets/portfolio.json".to_string()),
            history_path: std::env::var("HISTORY_PATH")
                .unwrap_or_else(|_| "assets/price_history.jsonl".to_string()),
            birdeye_api_key: std::env::var("BIRDEYE_API_KEY").ok(),
            alert_pct_5m: std::env::var("ALERT_PCT_5M")
                .unwrap_or_else(|_| "3.0".to_string())
                .parse()
                .context("ALERT_PCT_5M must be a float")?,
            alert_pct_1h: std::env::var("ALERT_PCT_1H")
                .unwrap_or_else(|_| "10.0".to_string())
                .parse()
                .context("ALERT_PCT_1H must be a float")?,
            alert_cooldown_min: std::env::var("ALERT_COOLDOWN_MIN")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .context("ALERT_COOLDOWN_MIN must be a number")?,
            zscore_lambda: std::env::var("ALERT_ZSCORE_LAMBDA")
                .unwrap_or_else(|_| "0.97".to_string())
                .parse()
                .context("ALERT_ZSCORE_LAMBDA must be a float")?,
            zscore_threshold: std::env::var("ALERT_ZSCORE_THRESHOLD")
                .unwrap_or_else(|_| "2.5".to_string())
                .parse()
                .context("ALERT_ZSCORE_THRESHOLD must be a float")?,
            zscore_min_obs: std::env::var("ALERT_ZSCORE_MIN_OBS")
                .unwrap_or_else(|_| "30".to_string())
                .parse()
                .context("ALERT_ZSCORE_MIN_OBS must be a number")?,
            price_thresholds: parse_price_thresholds(
                std::env::var("ALERT_PRICE_BELOW").as_deref().unwrap_or(""),
                "ALERT_PRICE_BELOW",
            )?,
            price_ceilings: parse_price_thresholds(
                std::env::var("ALERT_PRICE_ABOVE").as_deref().unwrap_or(""),
                "ALERT_PRICE_ABOVE",
            )?,
            status_path: std::env::var("STATUS_PATH")
                .unwrap_or_else(|_| "assets/portfolio_status.json".to_string()),
            alert_email: std::env::var("ALERT_EMAIL")
                .unwrap_or_else(|_| "you@example.com".to_string()),
            smtp_host: std::env::var("SMTP_HOST")
                .unwrap_or_else(|_| "smtp.gmail.com".to_string()),
            smtp_port: std::env::var("SMTP_PORT")
                .unwrap_or_else(|_| "587".to_string())
                .parse()
                .context("SMTP_PORT must be a number")?,
            smtp_user: std::env::var("SMTP_USER").unwrap_or_default(),
            smtp_password: std::env::var("SMTP_PASSWORD").unwrap_or_default(),
            smtp_from: std::env::var("SMTP_FROM").unwrap_or_default(),

            enable_momentum_trader: parse_bool_env("ENABLE_MOMENTUM_TRADER", false),
            // Dedicated flag so paper/live is independent of the arb bot's DRY_RUN.
            momentum_dry_run: parse_bool_env("DRY_RUN_MOMENTUM_TRADER", true),
            momentum_jupiter_api_url: std::env::var("MOMENTUM_JUPITER_API_URL")
                .unwrap_or_else(|_| "https://lite-api.jup.ag/swap/v1".to_string()),
            momentum_trade_usdc: parse_env("MOMENTUM_TRADE_USDC", 100.0_f64)?,
            momentum_trail_pct: parse_env("MOMENTUM_TRAIL_PCT", 5.0_f64)?,
            // Env key kept as MOMENTUM_RANK_METRIC; parses via RankMetric's FromStr
            // (errors loudly on a typo). Default sortino → no behavior change.
            momentum_rank_metric: parse_env("MOMENTUM_RANK_METRIC", RankMetric::default())?,
            // Min score to enter, in the active metric's units.
            momentum_min_score: parse_env("MOMENTUM_MIN_METRIC", 0.5_f64)?,
            momentum_rotate_margin: parse_env("MOMENTUM_ROTATE_MARGIN", 0.0_f64)?,
            momentum_max_run_pct: parse_env("MOMENTUM_MAX_RUN_PCT", 6.0_f64)?,
            momentum_decel_lookback_min: parse_env("MOMENTUM_DECEL_LOOKBACK_MIN", 10_usize)?,
            momentum_confirm_lag_obs: parse_env("MOMENTUM_CONFIRM_LAG_OBS", 5_usize)?,
            momentum_exit_on_fade: parse_bool_env("MOMENTUM_EXIT_ON_FADE", true),
            momentum_adopt_wallet_position: parse_bool_env("MOMENTUM_ADOPT_WALLET_POSITION", false),
            momentum_lookback_obs: parse_env("MOMENTUM_LOOKBACK_OBS", 121_usize)?,
            momentum_stale_minutes: parse_env("MOMENTUM_STALE_MINUTES", 20_usize)?,
            momentum_regime_obs: parse_env("MOMENTUM_REGIME_OBS", 0_usize)?,
            momentum_regime_mode: parse_env("MOMENTUM_REGIME_MODE", RegimeMode::default())?,
            momentum_regime_trend_min: parse_env("MOMENTUM_REGIME_TREND_MIN", 0.0_f64)?,
            momentum_entry_dip_obs: parse_env("MOMENTUM_ENTRY_DIP_OBS", 0_usize)?,
            momentum_entry_dip_z: parse_env("MOMENTUM_ENTRY_DIP_Z", 1.5_f64)?,
            momentum_entry_max_z_obs: parse_env("MOMENTUM_ENTRY_MAX_Z_OBS", 0_usize)?,
            momentum_entry_max_z: parse_env("MOMENTUM_ENTRY_MAX_Z", 1.0_f64)?,
            momentum_poll_secs: parse_env("MOMENTUM_POLL_SECS", 1_u64)?,
            momentum_reentry_cooldown_secs: parse_env("MOMENTUM_REENTRY_COOLDOWN_SECS", 360_i64)?,
            momentum_max_trades_per_day: parse_env("MOMENTUM_MAX_TRADES_PER_DAY", 10_u32)?,
            momentum_max_cost_bps: parse_env("MOMENTUM_MAX_COST_BPS", 100_u32)?,
            momentum_max_loss_usdc: parse_env("MOMENTUM_MAX_LOSS_USDC", 0.0_f64)?,
            momentum_slippage_bps: parse_env("MOMENTUM_SLIPPAGE_BPS", 50_u32)?,
            momentum_exit_slippage_cap_bps: parse_env("MOMENTUM_EXIT_SLIPPAGE_CAP_BPS", 800_u32)?,
            momentum_entry_slippage_cap_bps: parse_env("MOMENTUM_ENTRY_SLIPPAGE_CAP_BPS", 150_u32)?,
            momentum_tokens_path: std::env::var("MOMENTUM_TOKENS_PATH")
                .unwrap_or_else(|_| "assets/momentum_tokens.json".to_string()),
            momentum_state_path: std::env::var("MOMENTUM_STATE_PATH")
                .unwrap_or_else(|_| "assets/momentum_state.json".to_string()),
            momentum_halt_path: std::env::var("MOMENTUM_HALT_PATH")
                .unwrap_or_else(|_| "assets/momentum_halt.json".to_string()),
            momentum_actions_path: std::env::var("MOMENTUM_ACTIONS_PATH")
                .unwrap_or_else(|_| "assets/momentum_actions.jsonl".to_string()),
            momentum_pnl_path: std::env::var("MOMENTUM_PNL_PATH")
                .unwrap_or_else(|_| "assets/momentum_pnl.json".to_string()),

            momentum_scan_enable: parse_bool_env("MOMENTUM_SCAN_ENABLE", false),
            momentum_scan_interval_secs: parse_env("MOMENTUM_SCAN_INTERVAL_SECS", 3600_u64)?,
            momentum_scan_top_n: parse_env("MOMENTUM_SCAN_TOP_N", 3_usize)?,
            momentum_scan_script: std::env::var("MOMENTUM_SCAN_SCRIPT")
                .unwrap_or_else(|_| "scripts/scan_tokens.js".to_string()),

            momentum_max_positions: parse_env("MOMENTUM_MAX_POSITIONS", 1_usize)?.max(1),

            momentum_grpc_pricing: std::env::var("MOMENTUM_GRPC_PRICING").map(|v| v == "true").unwrap_or(false),
            grpc_endpoint: std::env::var("GRPC_ENDPOINT").ok(),
            grpc_token: std::env::var("GRPC_TOKEN").ok(),
            pools_path: std::env::var("POOLS_PATH").unwrap_or_else(|_| "pools.json".to_string()),
            momentum_grpc_stale_secs: std::env::var("MOMENTUM_GRPC_STALE_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(30),
        })
    }
}

/// Lenient boolean env read: case-insensitive, falls back to `default` on any
/// missing/unparseable value.
pub(crate) fn parse_bool_env(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .and_then(|s| s.trim().to_ascii_lowercase().parse().ok())
        .unwrap_or(default)
}

/// Generic numeric env read: returns `default` when unset, errors on a present
/// but unparseable value (so a typo surfaces at startup rather than silently
/// reverting to the default).
pub(crate) fn parse_env<T>(key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(s) => s
            .trim()
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("{key} is not a valid value: {e}")),
        Err(_) => Ok(default),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenEntry {
    pub mint: String,
    pub symbol: String,
    pub amount: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Portfolio {
    pub sol_amount: f64,
    pub tokens: Vec<TokenEntry>,
}

/// Parse "USDY:0.96,SOL:70.0" into Vec<(symbol, threshold)>.
/// `env_name` is woven into error messages so the user can tell which variable parsed badly.
fn parse_price_thresholds(raw: &str, env_name: &str) -> Result<Vec<(String, f64)>> {
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (sym, val) = entry.split_once(':')
            .with_context(|| format!("{env_name} entry '{entry}' must be SYMBOL:THRESHOLD"))?;
        let threshold: f64 = val.trim().parse()
            .with_context(|| format!("{env_name} threshold '{val}' is not a valid number"))?;
        out.push((sym.trim().to_string(), threshold));
    }
    Ok(out)
}

pub fn load_portfolio(path: &str) -> Result<Portfolio> {
    let data = std::fs::read_to_string(path)?;
    let portfolio = serde_json::from_str(&data)?;
    Ok(portfolio)
}

pub fn save_portfolio(path: &str, portfolio: &Portfolio) -> Result<()> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(portfolio)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn spawn_portfolio_watcher(cfg: PortfolioConfig, http: Client) -> tokio::task::JoinHandle<()> {
    tokio::spawn(watcher::run(cfg, http, None))
}
