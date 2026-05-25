pub mod analyzer;
pub mod emailer;
pub mod history;
pub mod jupiter;
pub mod pricer;
pub mod rebalancer;
pub mod rebalancer_actions;
pub mod rebalancer_snapshots;
pub mod rebalancer_state;
pub mod scanner;
pub mod suggestions;
pub mod watcher;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};

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

    // ----- Auto-rebalance (off by default) -----
    pub enable_auto_rebalance: bool,
    pub rebalance_size_fraction: f64,
    pub rebalance_min_position_eur: f64,
    pub rebalance_max_cost_bps: u32,
    pub rebalance_max_slippage_bps: u32,
    pub rebalance_max_swaps_per_day: u32,
    pub rebalance_hold_days: u32,
    pub rebalance_take_profit_pct: f64,
    pub rebalance_lookback_days: u32,
    pub rebalance_reversal_pct: f64,
    pub rebalance_reversal_window_min: u32,
    pub rebalance_extreme_window_hours: u32,
    pub rebalance_loss_halt_days: u32,
    pub rebalance_retry_attempts: u32,
    pub rebalance_retry_backoff_ms: u64,
    pub jupiter_api_url: String,
    pub rebalancer_state_path: String,
    pub rebalancer_snapshots_path: String,
    pub rebalancer_halt_path: String,
    pub rebalancer_actions_path: String,
    pub rebalance_require_recovery: bool,
    pub rebalance_dry_run: bool,
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

            enable_auto_rebalance: parse_bool_env("ENABLE_AUTO_REBALANCE", false),
            rebalance_size_fraction: parse_f64_env("REBALANCE_SIZE_FRACTION", 0.25)?,
            rebalance_min_position_eur: parse_f64_env("REBALANCE_MIN_POSITION_EUR", 25.0)?,
            rebalance_max_cost_bps: parse_u32_env("REBALANCE_MAX_COST_BPS", 50)?,
            rebalance_max_slippage_bps: parse_u32_env("REBALANCE_MAX_SLIPPAGE_BPS", 30)?,
            rebalance_max_swaps_per_day: parse_u32_env("REBALANCE_MAX_SWAPS_PER_DAY", 2)?,
            rebalance_hold_days: parse_u32_env("REBALANCE_HOLD_DAYS", 14)?,
            rebalance_take_profit_pct: parse_f64_env("REBALANCE_TAKE_PROFIT_PCT", 5.0)?,
            rebalance_lookback_days: parse_u32_env("REBALANCE_LOOKBACK_DAYS", 30)?,
            rebalance_reversal_pct: parse_f64_env("REBALANCE_REVERSAL_PCT", 0.3)?,
            rebalance_reversal_window_min: parse_u32_env("REBALANCE_REVERSAL_WINDOW_MIN", 60)?,
            rebalance_extreme_window_hours: parse_u32_env("REBALANCE_EXTREME_WINDOW_HOURS", 24)?,
            rebalance_loss_halt_days: parse_u32_env("REBALANCE_LOSS_HALT_DAYS", 21)?,
            rebalance_retry_attempts: parse_u32_env("REBALANCE_RETRY_ATTEMPTS", 3)?,
            rebalance_retry_backoff_ms: parse_u32_env("REBALANCE_RETRY_BACKOFF_MS", 1500)? as u64,
            jupiter_api_url: std::env::var("JUPITER_API_URL")
                .unwrap_or_else(|_| "https://quote-api.jup.ag/v6".to_string()),
            rebalancer_state_path: std::env::var("REBALANCER_STATE_PATH")
                .unwrap_or_else(|_| "assets/rebalancer_state.json".to_string()),
            rebalancer_snapshots_path: std::env::var("REBALANCER_SNAPSHOTS_PATH")
                .unwrap_or_else(|_| "assets/rebalancer_snapshots.jsonl".to_string()),
            rebalancer_halt_path: std::env::var("REBALANCER_HALT_PATH")
                .unwrap_or_else(|_| "assets/rebalancer_halt.json".to_string()),
            rebalancer_actions_path: std::env::var("REBALANCER_ACTIONS_PATH")
                .unwrap_or_else(|_| "assets/rebalancer_actions.jsonl".to_string()),
            rebalance_require_recovery: parse_bool_env("REBALANCE_REQUIRE_RECOVERY", true),
            rebalance_dry_run: parse_bool_env("REBALANCE_DRY_RUN", false),
        })
    }
}

fn parse_bool_env(key: &str, default: bool) -> bool {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn parse_f64_env(key: &str, default: f64) -> Result<f64> {
    match std::env::var(key) {
        Ok(s) => s.parse().with_context(|| format!("{key} must be a float")),
        Err(_) => Ok(default),
    }
}

fn parse_u32_env(key: &str, default: u32) -> Result<u32> {
    match std::env::var(key) {
        Ok(s) => s.parse().with_context(|| format!("{key} must be a non-negative integer")),
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
    tokio::spawn(watcher::run(cfg, http))
}
