pub mod analyzer;
pub mod emailer;
pub mod history;
pub mod pricer;
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
    pub alert_email: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_user: String,
    pub smtp_password: String,
    pub smtp_from: String,
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
            )?,
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
        })
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
fn parse_price_thresholds(raw: &str) -> Result<Vec<(String, f64)>> {
    let mut out = Vec::new();
    for entry in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (sym, val) = entry.split_once(':')
            .with_context(|| format!("ALERT_PRICE_BELOW entry '{entry}' must be SYMBOL:THRESHOLD"))?;
        let threshold: f64 = val.trim().parse()
            .with_context(|| format!("ALERT_PRICE_BELOW threshold '{val}' is not a valid number"))?;
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
