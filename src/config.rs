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

/// DLMM bin-walk quote rollout mode (`DLMM_BIN_QUOTE` env, default shadow).
/// off = haircut quote only; shadow = haircut quote + walk-vs-haircut
/// divergence logging from the evaluator; live = walk is THE quote where bin
/// data exists (haircut fallback otherwise).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DlmmBinQuoteMode { Off, Shadow, Live }

pub fn parse_dlmm_bin_quote_mode(s: &str) -> DlmmBinQuoteMode {
    match s.to_ascii_lowercase().as_str() {
        "off" => DlmmBinQuoteMode::Off,
        "live" => DlmmBinQuoteMode::Live,
        _ => DlmmBinQuoteMode::Shadow,
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub grpc_endpoint: String,
    pub grpc_token: Option<String>,
    pub wallet_keypair_path: String,
    pub rpc_url: String,
    pub pools_config_path: String,
    /// The arbitrage base/starting token. Defaults to SOL (`BASE_MINT` unset).
    pub base_token: crate::dex::types::BaseToken,
    /// Halt if native SOL falls below this (can't pay tips/fees). Only enforced when the
    /// base token is non-native; for a SOL base the P&L guard already covers it.
    pub min_sol_gas_lamports: u64,
    pub min_profit_base_units: u64,
    /// Minimum Jito tip required to submit a bundle. Cycles that cannot generate
    /// a competitive tip — because gross profit is too small relative to capital —
    /// are rejected before submission, preserving cooldown time for better cycles.
    /// Set to 0 to disable (default). A value of 10_000_000 (0.01 SOL) is a
    /// reasonable starting point when drops dominate the submission log.
    pub min_tip_lamports: u64,
    pub input_base_units: u64,
    pub slippage_bps: u64,
    pub tip_ratio: f64,
    pub max_tip_lamports: u64,
    /// Minimum tip expressed as a multiple of the Jito EMA-50 tip floor.
    /// Final tip = max(gross_profit * tip_ratio, tip_floor * tip_floor_multiplier),
    /// clamped to [1_000, max_tip_lamports]. Set to 0.0 to disable floor anchoring.
    pub tip_floor_multiplier: f64,
    /// Pre-submission floor-relative gate. If tip < tip_floor × this value, the cycle
    /// is rejected in the evaluator before any submission attempt. Set to 0.0 to disable
    /// (default). A value of 500 filters cycles bidding less than 500× the Jito EMA floor
    /// — competitive bids typically run 2000–5000× during active blocks.
    pub min_tip_floor_multiple: f64,
    pub dry_run: bool,
    /// When true, simulate one swap per pool and exit. Does not start the gRPC stream.
    pub check_pools: bool,
    /// Minimum milliseconds between Bellman-Ford runs (debounce).
    pub bellman_ford_debounce_ms: u64,
    /// Reject any arbitrage cycle whose stalest leg's last live gRPC update is
    /// older than this many milliseconds. Prevents submitting phantom cycles
    /// built on stale graph edges (they fail Jito Block Engine simulation and
    /// drop regardless of tip). 0 = disabled.
    pub max_cycle_staleness_ms: u64,
    /// Backfill poller: RPC-refresh pools the gRPC feed leaves stale
    /// (free/shared tiers throttle large subscriptions). Complements the
    /// staleness gate — polled pools stay fresh instead of being gated.
    pub stale_poll_enable: bool,
    /// Backfill poller tick interval (ms).
    pub stale_poll_interval_ms: u64,
    /// Poll pools whose last update is older than this (ms). Keep below
    /// MAX_CYCLE_STALENESS_MS so pools refresh before the gate fires.
    pub stale_poll_threshold_ms: u64,
    /// Maximum acceptable price impact per hop in basis points (default 100 = 1%).
    /// Any hop exceeding this threshold rejects the whole opportunity — the pool
    /// is too small relative to the trade size for the graph's marginal rate to
    /// reflect what you'll actually receive.
    pub max_price_impact_bps: u64,
    /// Maximum cycle length searched by Bellman-Ford (MAX_ARB_HOPS, default 3).
    /// 2 = skip the O(E³) 3-hop enumeration entirely — cheaper hot loop, and only
    /// 2-hop cycles qualify for the raw-RPC no-ALT path anyway. Only 2 and 3 are valid.
    pub max_arb_hops: u8,
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
    /// On-chain Address Lookup Tables for versioned transaction account compression.
    /// One ALT holds max 256 accounts; multiple ALTs are used when needed.
    /// Set ALT_ADDRESSES (comma-separated) or ALT_ADDRESS (single) in .env.
    /// Create with: cargo run --bin solana-mev -- --init-alt
    pub alt_addresses: Vec<Pubkey>,
    /// When true + ENABLE_FLASH_LOAN=true: cycles at or below jito_bundle_threshold_bps
    /// use a floor-anchored tip (~6_000L) instead of the profit-ratio tip, so the wallet
    /// keeps most of the margin on thin cycles. Despite the name, the bundle is STILL
    /// submitted via Jito — raw RPC with v0+ALT fails on non-Jito validators
    /// (ProgramAccountNotFound). Cycles above the threshold use the ratio-based tip.
    pub bypass_jito_bundle: bool,
    /// Gross bps threshold splitting thin from fat cycles (only when bypass_jito_bundle=true).
    /// Default: 20 bps. Cycles at or below use the floor tip; above use the ratio tip.
    pub jito_bundle_threshold_bps: f64,
    /// When true (non-native base only): thin 2-hop local-DEX cycles whose wallet-funded
    /// transaction fits in ONE ≤1232-byte tx with NO address lookup tables are submitted
    /// via raw RPC sendTransaction instead of a Jito bundle — no tip, no auction, and a
    /// v0 tx with zero lookups cannot hit the non-Jito-validator ProgramAccountNotFound
    /// failure, so it is valid on every leader slot. Everything else keeps the Jito path.
    /// Force-disabled with a warning when the base token is native (flash-loan shape).
    pub enable_raw_rpc: bool,
    /// DLMM bin-walk quote mode (see DlmmBinQuoteMode). Default: Shadow.
    pub dlmm_bin_quote: DlmmBinQuoteMode,
    /// Base-token units (USDC = 6dp) held back from the wallet balance when sizing cycle
    /// input, AND subtracted from the P&L-halt threshold — reserves capital for the
    /// momentum trader sharing this wallet so its spends neither starve the arb sizing
    /// nor trip the drawdown halt. Default 0 (no reservation).
    pub base_balance_reserve_units: u64,
    /// Minimum swap size (in lamports) for a confirmed transaction to trigger
    /// an immediate BF evaluation, bypassing the normal debounce window.
    /// Set WHALE_MIN_SOL=0 to fire on every vault-touching transaction.
    pub whale_min_sol_lamports: u64,
    /// Milliseconds to sleep after detecting a whale tx before poking BF,
    /// giving the vault account-update time to arrive and update the atomics.
    pub whale_back_run_delay_ms: u64,
    /// When true, load Jupiter pairs from `jupiter_pairs_path` and run the Jupiter
    /// rate poller, injecting Jupiter-aggregated edges into the graph. Default false.
    pub enable_jupiter: bool,
    /// Base URL of the self-hosted Jupiter swap-api (e.g. http://127.0.0.1:8080).
    /// Exposes /quote and /swap-instructions. Default assumes a local instance.
    pub jupiter_api_url: String,
    /// Path to the Metis swap-api binary (jup-ag/metis-binary). When set (and
    /// enable_jupiter=true), the bot launches it as a child process pointed at the same
    /// RPC + gRPC, killing it on exit. Leave unset to run Metis externally yourself.
    /// Assumes Metis serves on its default port 8080 (match jupiter_api_url accordingly).
    pub jupiter_binary_path: Option<String>,
    /// License key for the gated Metis binary (`--binary-key`), obtained from your binary
    /// provider (Triton/QuickNode/Jupiter). Required to auto-launch; auto-launch is skipped
    /// with a warning if the path is set but this is missing. Secret — keep in .env only.
    pub jupiter_binary_key: Option<String>,
    /// Path to the Jupiter pairs config (a flat JSON list of {token_a, token_b}).
    /// These are synthetic, vault-less pools — kept separate from pools.json.
    pub jupiter_pairs_path: String,
    /// Milliseconds between Jupiter rate-poller passes. Edges are only as fresh as
    /// this interval; self-hosted /quote is sub-10ms so 500ms is a safe default.
    pub jupiter_poll_interval_ms: u64,
    /// Reference input size (lamports) used by the poller to probe marginal rates.
    /// get_quote scales price impact relative to this. Default 1 SOL.
    pub jupiter_probe_lamports: u64,
}

/// Flash loan is only valid when the base token is native (WSOL). A non-native base
/// (USDC) is wallet-funded, so a requested flash loan is force-disabled.
pub(crate) fn resolve_flash_loan_enabled(requested: bool, base_is_native: bool) -> bool {
    requested && base_is_native
}

/// Raw RPC submission is only valid for a WALLET-FUNDED cycle: the raw path sends ONE
/// small no-ALT transaction. Flash-loan mode builds the borrow/repay mega-tx (needs
/// ALTs) and stays on Jito. The base token itself is irrelevant — a native base's
/// wrap/close instructions ride the same ≤1232-byte size probe as any other cycle.
pub(crate) fn resolve_raw_rpc_enabled(requested: bool, flash_loan_enabled: bool) -> bool {
    requested && !flash_loan_enabled
}

/// Return the primary env value if present, else the alias, else the default.
pub(crate) fn first_present(primary: Option<String>, alias: Option<String>, default: &str) -> String {
    primary.or(alias).unwrap_or_else(|| default.to_string())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let base_mint = env::var("BASE_MINT")
            .unwrap_or_else(|_| crate::dex::types::WSOL_MINT.to_string());
        let base_token = crate::dex::types::resolve_base_token(&base_mint)
            .map_err(|e| anyhow::anyhow!(e))?;

        let flash_loan_requested = env::var("ENABLE_FLASH_LOAN")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false);
        let enable_flash_loan = resolve_flash_loan_enabled(flash_loan_requested, base_token.is_native);
        if flash_loan_requested && !enable_flash_loan {
            tracing::warn!(
                "ENABLE_FLASH_LOAN=true ignored: base token {} is not native (wallet-funded only).",
                base_token.symbol
            );
        }

        let raw_rpc_requested = env::var("ENABLE_RAW_RPC")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false);
        let enable_raw_rpc = resolve_raw_rpc_enabled(raw_rpc_requested, enable_flash_loan);
        if raw_rpc_requested && !enable_raw_rpc {
            tracing::warn!(
                "ENABLE_RAW_RPC=true ignored: flash loan is active — raw submission needs the wallet-funded cycle shape (set ENABLE_FLASH_LOAN=false to use it)."
            );
        }

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
            base_token,
            min_sol_gas_lamports: env::var("MIN_SOL_GAS_LAMPORTS")
                .unwrap_or_else(|_| "100000000".to_string()) // 0.1 SOL
                .parse()
                .context("MIN_SOL_GAS_LAMPORTS must be a number")?,
            min_tip_lamports: env::var("MIN_TIP_LAMPORTS")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("MIN_TIP_LAMPORTS must be a number")?,
            min_profit_base_units: first_present(
                env::var("MIN_PROFIT_LAMPORTS").ok(),
                env::var("MIN_PROFIT_BASE_UNITS").ok(),
                "10000",
            ).parse().context("MIN_PROFIT_LAMPORTS/MIN_PROFIT_BASE_UNITS must be a number")?,
            input_base_units: first_present(
                env::var("INPUT_SOL_LAMPORTS").ok(),
                env::var("INPUT_BASE_UNITS").ok(),
                "1000000000",
            ).parse().context("INPUT_SOL_LAMPORTS/INPUT_BASE_UNITS must be a number")?,
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
            min_tip_floor_multiple: env::var("MIN_TIP_FLOOR_MULTIPLE")
                .unwrap_or_else(|_| "0.0".to_string())
                .parse()
                .context("MIN_TIP_FLOOR_MULTIPLE must be a float")?,
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
            max_cycle_staleness_ms: env::var("MAX_CYCLE_STALENESS_MS")
                .unwrap_or_else(|_| "2000".to_string())
                .parse()
                .context("MAX_CYCLE_STALENESS_MS must be a number")?,
            stale_poll_enable: env::var("STALE_POLL_ENABLE")
                .unwrap_or_else(|_| "true".to_string())
                .parse()
                .unwrap_or(true),
            stale_poll_interval_ms: env::var("STALE_POLL_INTERVAL_MS")
                .unwrap_or_else(|_| "400".to_string())
                .parse()
                .context("STALE_POLL_INTERVAL_MS must be a number")?,
            stale_poll_threshold_ms: env::var("STALE_POLL_THRESHOLD_MS")
                .unwrap_or_else(|_| "1500".to_string())
                .parse()
                .context("STALE_POLL_THRESHOLD_MS must be a number")?,
            max_price_impact_bps: env::var("MAX_PRICE_IMPACT_BPS")
                .unwrap_or_else(|_| "100".to_string())
                .parse()
                .context("MAX_PRICE_IMPACT_BPS must be a number")?,
            max_arb_hops: {
                let v: u8 = env::var("MAX_ARB_HOPS")
                    .unwrap_or_else(|_| "3".to_string())
                    .parse()
                    .context("MAX_ARB_HOPS must be a number")?;
                anyhow::ensure!((2..=3).contains(&v), "MAX_ARB_HOPS must be 2 or 3, got {v}");
                v
            },
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
            enable_flash_loan,
            flash_loan_max_input_lamports: env::var("FLASH_LOAN_MAX_INPUT_SOL_LAMPORTS")
                .unwrap_or_else(|_| "50000000000".to_string()) // default: 50 SOL
                .parse()
                .context("FLASH_LOAN_MAX_INPUT_SOL_LAMPORTS must be a number")?,
            bypass_jito_bundle: env::var("BYPASS_JITO_BUNDLE")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            jito_bundle_threshold_bps: env::var("JITO_BUNDLE_THRESHOLD")
                .unwrap_or_else(|_| "20.0".to_string())
                .parse()
                .context("JITO_BUNDLE_THRESHOLD must be a number")?,
            enable_raw_rpc,
            dlmm_bin_quote: parse_dlmm_bin_quote_mode(
                &env::var("DLMM_BIN_QUOTE").unwrap_or_else(|_| "shadow".to_string()),
            ),
            base_balance_reserve_units: env::var("BASE_BALANCE_RESERVE_UNITS")
                .unwrap_or_else(|_| "0".to_string())
                .parse()
                .context("BASE_BALANCE_RESERVE_UNITS must be a number")?,
            whale_min_sol_lamports: {
                let sol: f64 = env::var("WHALE_MIN_SOL")
                    .unwrap_or_else(|_| "5.0".to_string())
                    .parse()
                    .context("WHALE_MIN_SOL must be a float")?;
                (sol * 1e9) as u64
            },
            whale_back_run_delay_ms: env::var("WHALE_BACK_RUN_DELAY_MS")
                .unwrap_or_else(|_| "10".to_string())
                .parse()
                .context("WHALE_BACK_RUN_DELAY_MS must be a number")?,
            enable_jupiter: env::var("ENABLE_JUPITER")
                .unwrap_or_else(|_| "false".to_string())
                .parse()
                .unwrap_or(false),
            jupiter_api_url: env::var("JUPITER_API_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string()),
            jupiter_binary_path: env::var("JUPITER_BINARY_PATH").ok().filter(|s| !s.is_empty()),
            jupiter_binary_key: env::var("JUPITER_BINARY_KEY").ok().filter(|s| !s.is_empty()),
            jupiter_pairs_path: env::var("JUPITER_PAIRS_PATH")
                .unwrap_or_else(|_| "jupiter_pairs.json".to_string()),
            jupiter_poll_interval_ms: env::var("JUPITER_POLL_INTERVAL_MS")
                .unwrap_or_else(|_| "500".to_string())
                .parse()
                .context("JUPITER_POLL_INTERVAL_MS must be a number")?,
            jupiter_probe_lamports: env::var("JUPITER_PROBE_LAMPORTS")
                .unwrap_or_else(|_| "1000000000".to_string())
                .parse()
                .context("JUPITER_PROBE_LAMPORTS must be a number")?,
            alt_addresses: if let Ok(s) = env::var("ALT_ADDRESSES") {
                // Comma-separated list for multiple ALTs
                s.split(',')
                    .map(|s| s.trim().parse::<Pubkey>().context("ALT_ADDRESSES must be comma-separated valid pubkeys"))
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else if let Ok(s) = env::var("ALT_ADDRESS") {
                // Backward compat: single address
                vec![s.parse::<Pubkey>().context("ALT_ADDRESS must be a valid pubkey")?]
            } else {
                vec![]
            },
            flash_loan: {
                let enabled = enable_flash_loan;
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

    /// Shared test fixture: a minimal valid Config for unit tests in other modules
    /// (native SOL base, everything optional off). Mirrors evaluator's test_config.
    #[cfg(test)]
    pub(crate) fn test_default() -> Self {
        Self {
            grpc_endpoint: String::new(),
            grpc_token: None,
            wallet_keypair_path: String::new(),
            rpc_url: String::new(),
            pools_config_path: String::new(),
            base_token: crate::dex::types::resolve_base_token(crate::dex::types::WSOL_MINT).unwrap(),
            min_sol_gas_lamports: 100_000_000,
            min_profit_base_units: 1_000,
            input_base_units: 100_000_000,
            slippage_bps: 50,
            tip_ratio: 0.5,
            max_tip_lamports: 1_000_000,
            min_tip_lamports: 0,
            dry_run: false,
            bellman_ford_debounce_ms: 10,
            max_cycle_staleness_ms: 0,
            stale_poll_enable: false,
            stale_poll_interval_ms: 400,
            stale_poll_threshold_ms: 1500,
            max_price_impact_bps: 10_000,
            max_arb_hops: 3,
            compute_unit_limit: 600_000,
            compute_unit_price_micro_lamports: 1_000,
            log_cycle_threshold_bps: 0.0,
            check_pools: false,
            disable_simulation: false,
            enable_flash_loan: false,
            flash_loan_max_input_lamports: 500_000_000_000,
            flash_loan: None,
            tip_floor_multiplier: 1.2,
            min_tip_floor_multiple: 0.0,
            alt_addresses: vec![],
            bypass_jito_bundle: false,
            jito_bundle_threshold_bps: 20.0,
            enable_raw_rpc: false,
            dlmm_bin_quote: DlmmBinQuoteMode::Off,
            base_balance_reserve_units: 0,
            whale_min_sol_lamports: 0,
            whale_back_run_delay_ms: 0,
            enable_jupiter: false,
            jupiter_api_url: String::new(),
            jupiter_binary_path: None,
            jupiter_binary_key: None,
            jupiter_pairs_path: String::new(),
            jupiter_poll_interval_ms: 500,
            jupiter_probe_lamports: 1_000_000_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dlmm_bin_quote_mode_parses() {
        assert_eq!(parse_dlmm_bin_quote_mode("off"), DlmmBinQuoteMode::Off);
        assert_eq!(parse_dlmm_bin_quote_mode("live"), DlmmBinQuoteMode::Live);
        assert_eq!(parse_dlmm_bin_quote_mode("shadow"), DlmmBinQuoteMode::Shadow);
        assert_eq!(parse_dlmm_bin_quote_mode("bogus"), DlmmBinQuoteMode::Shadow, "default");
    }

    #[test]
    fn flash_loan_forced_off_for_non_native_base() {
        // requested=true but base is SPL → must be disabled
        assert!(!super::resolve_flash_loan_enabled(true, false));
        // requested=true and base native → stays on
        assert!(super::resolve_flash_loan_enabled(true, true));
        // requested=false → stays off
        assert!(!super::resolve_flash_loan_enabled(false, true));
    }

    #[test]
    fn raw_rpc_forced_off_only_in_flash_mode() {
        // requested=true but flash loan active (mega-tx shape, ALTs) → must be disabled
        assert!(!super::resolve_raw_rpc_enabled(true, true));
        // requested=true and wallet-funded (flash off; ANY base incl. native) → stays on
        assert!(super::resolve_raw_rpc_enabled(true, false));
        // requested=false → stays off regardless of funding shape
        assert!(!super::resolve_raw_rpc_enabled(false, false));
        assert!(!super::resolve_raw_rpc_enabled(false, true));
    }

    #[test]
    fn first_env_present_prefers_primary_then_alias() {
        assert_eq!(super::first_present(Some("5".into()), None, "9"), "5");
        assert_eq!(super::first_present(None, Some("7".into()), "9"), "7");
        assert_eq!(super::first_present(None, None, "9"), "9");
    }
}
