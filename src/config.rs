use anyhow::{Context, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub grpc_endpoint: String,
    pub grpc_token: Option<String>,
    pub wallet_keypair_path: String,
    pub rpc_url: String,
    pub pools_config_path: String,
    pub min_profit_lamports: u64,
    pub input_sol_lamports: u64,
    pub slippage_bps: u64,
    pub tip_ratio: f64,
    pub max_tip_lamports: u64,
    pub dry_run: bool,
    /// When true, simulate one swap per pool and exit. Does not start the gRPC stream.
    pub check_pools: bool,
    /// Minimum milliseconds between Bellman-Ford runs (debounce).
    pub bellman_ford_debounce_ms: u64,
    /// Maximum acceptable price impact per hop in basis points (default 100 = 1%).
    /// Any hop exceeding this threshold rejects the whole opportunity — the pool
    /// is too small relative to the trade size for the graph's marginal rate to
    /// reflect what you'll actually receive.
    pub max_price_impact_bps: u64,
    /// Compute unit limit per swap transaction (default 600_000).
    /// Used both in bundle construction and in the evaluator's fee estimate.
    pub compute_unit_limit: u64,
    /// Priority fee in micro-lamports per compute unit (default 1_000).
    /// Each swap tx pays: compute_unit_limit * compute_unit_price_micro_lamports / 1_000_000 lamports.
    pub compute_unit_price_micro_lamports: u64,
    /// Gross profit threshold in bps above which the cycle path is logged at INFO level (default 5.0).
    pub log_cycle_threshold_bps: f64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            grpc_endpoint: env::var("GRPC_ENDPOINT")
                .unwrap_or_default(), // optional when CHECK_POOLS=true
            grpc_token: env::var("GRPC_TOKEN").ok(),
            wallet_keypair_path: env::var("WALLET_KEYPAIR_PATH")
                .unwrap_or_else(|_| "~/.config/solana/id.json".to_string()),
            rpc_url: env::var("RPC_URL")
                .unwrap_or_else(|_| "https://api.mainnet-beta.solana.com".to_string()),
            pools_config_path: env::var("POOLS_CONFIG_PATH")
                .unwrap_or_else(|_| "pools.json".to_string()),
            min_profit_lamports: env::var("MIN_PROFIT_LAMPORTS")
                .unwrap_or_else(|_| "10000".to_string())
                .parse()
                .context("MIN_PROFIT_LAMPORTS must be a number")?,
            input_sol_lamports: env::var("INPUT_SOL_LAMPORTS")
                .unwrap_or_else(|_| "1000000000".to_string())
                .parse()
                .context("INPUT_SOL_LAMPORTS must be a number")?,
            slippage_bps: env::var("SLIPPAGE_BPS")
                .unwrap_or_else(|_| "50".to_string())
                .parse()
                .context("SLIPPAGE_BPS must be a number")?,
            tip_ratio: env::var("TIP_RATIO")
                .unwrap_or_else(|_| "0.51".to_string())
                .parse()
                .context("TIP_RATIO must be a float")?,
            max_tip_lamports: env::var("MAX_TIP_LAMPORTS")
                .unwrap_or_else(|_| "1000000".to_string())
                .parse()
                .context("MAX_TIP_LAMPORTS must be a number")?,
            dry_run: env::var("DRY_RUN")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            check_pools: env::var("CHECK_POOLS")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            bellman_ford_debounce_ms: env::var("BELLMAN_FORD_DEBOUNCE_MS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("BELLMAN_FORD_DEBOUNCE_MS must be a number")?,
            max_price_impact_bps: env::var("MAX_PRICE_IMPACT_BPS")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .context("MAX_PRICE_IMPACT_BPS must be a number")?,
            compute_unit_limit: env::var("COMPUTE_UNIT_LIMIT")
                .unwrap_or_else(|_| "600000".to_string())
                .parse()
                .context("COMPUTE_UNIT_LIMIT must be a number")?,
            compute_unit_price_micro_lamports: env::var("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS")
                .unwrap_or_else(|_| "1000".to_string())
                .parse()
                .context("COMPUTE_UNIT_PRICE_MICRO_LAMPORTS must be a number")?,
            log_cycle_threshold_bps: env::var("LOG_CYCLE_THRESHOLD_BPS")
                .unwrap_or_else(|_| "5.0".to_string())
                .parse()
                .context("LOG_CYCLE_THRESHOLD_BPS must be a float")?,
        })
    }

    pub fn grpc_connect_timeout_secs(&self) -> u64 { 10 }
    pub fn grpc_request_timeout_secs(&self) -> u64 { 60 }
    pub fn grpc_max_message_size(&self) -> usize { 10 * 1024 * 1024 }
}
