use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use std::env;

/// Accounts required for MarginFi flash loan execution.
/// Only populated when ENABLE_FLASH_LOAN=true.
#[derive(Debug, Clone)]
pub struct FlashLoanConfig {
    /// The user's MarginFi lending account (created via MarginFi UI or CLI before running).
    pub marginfi_account: Pubkey,
    /// MarginFi group (mainnet default: 4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG5).
    pub marginfi_group: Pubkey,
    /// MarginFi SOL bank (mainnet default: CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAntD6LYGh).
    pub marginfi_sol_bank: Pubkey,
    /// Price oracle for the SOL bank used by EndFlashloan health check.
    pub marginfi_sol_bank_oracle: Pubkey,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub grpc_endpoint: String,
    pub grpc_token: Option<String>,
    pub wallet_keypair_path: String,
    pub rpc_url: String,
    pub pools_config_path: String,
    pub min_profit_lamports: u64,
    /// Minimum Jito tip required to submit a bundle. Cycles that cannot generate
    /// a competitive tip — because gross profit is too small relative to capital —
    /// are rejected before submission, preserving cooldown time for better cycles.
    /// Set to 0 to disable (default). A value of 10_000_000 (0.01 SOL) is a
    /// reasonable starting point when drops dominate the submission log.
    pub min_tip_lamports: u64,
    pub input_sol_lamports: u64,
    pub slippage_bps: u64,
    pub tip_ratio: f64,
    pub max_tip_lamports: u64,
    /// Minimum tip expressed as a multiple of the Jito EMA-50 tip floor.
    /// Final tip = max(gross_profit * tip_ratio, tip_floor * tip_floor_multiplier),
    /// clamped to [1_000, max_tip_lamports]. Set to 0.0 to disable floor anchoring.
    pub tip_floor_multiplier: f64,
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
    /// When true, skip pre-submission simulation and submit bundles directly to Jito.
    /// Default false. Set DISABLE_SIMULATION=true to trade latency for opportunity capture.
    pub disable_simulation: bool,
    /// When true, fund arb capital via a MarginFi flash loan instead of the wallet balance.
    /// The flash loan fee (~9 bps) is factored into profit calculations.
    /// Default false. Set ENABLE_FLASH_LOAN=true to arb without holding SOL capital.
    pub enable_flash_loan: bool,
    /// Upper bound on borrowed amount when flash loan is active (lamports).
    /// INPUT_SOL_LAMPORTS is ignored; the ternary search finds the slippage-optimal
    /// amount within [1_000_000, flash_loan_max_input_lamports].
    /// Default: 50 SOL. Tune up for deeper pools once vault-impact estimates are verified.
    pub flash_loan_max_input_lamports: u64,
    /// Populated when enable_flash_loan=true. Contains MarginFi account addresses.
    pub flash_loan: Option<FlashLoanConfig>,
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
            min_tip_lamports: env::var("MIN_TIP_LAMPORTS")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("MIN_TIP_LAMPORTS must be a number")?,
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
            tip_floor_multiplier: env::var("TIP_FLOOR_MULTIPLIER")
                .unwrap_or_else(|_| "1.2".to_string())
                .parse()
                .context("TIP_FLOOR_MULTIPLIER must be a float")?,
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
            disable_simulation: env::var("DISABLE_SIMULATION")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            enable_flash_loan: env::var("ENABLE_FLASH_LOAN")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            flash_loan_max_input_lamports: env::var("FLASH_LOAN_MAX_INPUT_SOL_LAMPORTS")
                .unwrap_or_else(|_| "50000000000".to_string()) // default: 50 SOL
                .parse()
                .context("FLASH_LOAN_MAX_INPUT_SOL_LAMPORTS must be a number")?,
            flash_loan: {
                let enabled = env::var("ENABLE_FLASH_LOAN")
                    .unwrap_or_default()
                    .parse::<bool>()
                    .unwrap_or(false);
                if enabled {
                    let marginfi_account = env::var("MARGINFI_ACCOUNT")
                        .context("MARGINFI_ACCOUNT is required when ENABLE_FLASH_LOAN=true")?
                        .parse::<Pubkey>()
                        .context("MARGINFI_ACCOUNT must be a valid pubkey")?;
                    let marginfi_group = env::var("MARGINFI_GROUP")
                        .unwrap_or_else(|_| "4qp6Fx6tnZkY5Wropq9wUYgtFxXKwE6viZxFHg3rdAG5".to_string())
                        .parse::<Pubkey>()
                        .context("MARGINFI_GROUP must be a valid pubkey")?;
                    let marginfi_sol_bank = env::var("MARGINFI_SOL_BANK")
                        .unwrap_or_else(|_| "CCKtUs6Cgwo4aaQUmBPmyoApH2gUDErxNZCAntD6LYGh".to_string())
                        .parse::<Pubkey>()
                        .context("MARGINFI_SOL_BANK must be a valid pubkey")?;
                    let marginfi_sol_bank_oracle = env::var("MARGINFI_SOL_BANK_ORACLE")
                        .context("MARGINFI_SOL_BANK_ORACLE is required when ENABLE_FLASH_LOAN=true")?
                        .parse::<Pubkey>()
                        .context("MARGINFI_SOL_BANK_ORACLE must be a valid pubkey")?;
                    Some(FlashLoanConfig {
                        marginfi_account,
                        marginfi_group,
                        marginfi_sol_bank,
                        marginfi_sol_bank_oracle,
                    })
                } else {
                    None
                }
            },
        })
    }

    pub fn grpc_connect_timeout_secs(&self) -> u64 { 10 }
    pub fn grpc_request_timeout_secs(&self) -> u64 { 60 }
    pub fn grpc_max_message_size(&self) -> usize { 10 * 1024 * 1024 }
}
