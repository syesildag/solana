use anyhow::Result;
use super::{parse_bool_env, parse_env};

/// USDC mint — the asset seized collateral is sold into for the profit estimate.
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Configuration for the Kamino liquidation **detection** bot (Phase A: paper-only).
#[derive(Debug, Clone)]
pub struct LiquidationConfig {
    pub enable: bool,
    /// Phase A is detection-only; this is always treated as paper (no submission). Kept so
    /// the flag exists for Phase B and the convention matches the other subsystems.
    pub dry_run: bool,
    /// Lending market to scan (default = the Kamino xStocks market, where Phase 0 recon
    /// found an active near-liquidation pipeline).
    pub market: String,
    /// Seconds between full scans. The sidecar scan is a heavy bulk getProgramAccounts, so
    /// this is deliberately slow; Phase B switches to gRPC streaming for speed.
    pub scan_every_secs: i64,
    /// Scan obligations with health factor below this (1.0 = liquidatable now; >1.0 watches
    /// the near-edge pipeline so we see opportunities forming).
    pub scan_max_hf: f64,
    /// Fraction of the largest debt leg repaid per liquidation (Kamino close factor).
    pub close_factor: f64,
    /// Assumed liquidation bonus % (collateral premium). MVP default; Phase B should read the
    /// real per-reserve `liquidationBonus`.
    pub liq_bonus_pct: f64,
    /// Minimum net USD profit for a detection to count as "profitable".
    pub min_profit_usd: f64,
    /// Flash-loan fee (bps) charged on the repay leg — 0 for Phase A detection; set for
    /// Phase B once flash-loan funding is wired.
    pub flash_fee_bps: u32,
    /// klend-builder sidecar base URL (serves `/liquidatable`); shared with the pairs trader.
    pub klend_sidecar_url: String,
    /// Jupiter swap-api base URL for the live seize→USDC impact quote.
    pub jupiter_api_url: String,
    pub state_path: String,
    pub actions_path: String,
    /// Send a summary email per scan that finds new profitable opportunities (paper too).
    /// Gated by SMTP being configured.
    pub notify_email: bool,
}

impl LiquidationConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            enable: parse_bool_env("ENABLE_LIQUIDATION_BOT", false),
            dry_run: parse_bool_env("DRY_RUN_LIQUIDATION_BOT", true),
            market: std::env::var("LIQUIDATION_MARKET")
                .unwrap_or_else(|_| super::kamino::XSTOCKS_MARKET.to_string()),
            scan_every_secs: parse_env("LIQUIDATION_SCAN_SECS", 300_i64)?,
            scan_max_hf: parse_env("LIQUIDATION_SCAN_MAX_HF", 1.05_f64)?,
            close_factor: parse_env("LIQUIDATION_CLOSE_FACTOR", 0.5_f64)?,
            liq_bonus_pct: parse_env("LIQUIDATION_BONUS_PCT", 5.0_f64)?,
            min_profit_usd: parse_env("LIQUIDATION_MIN_PROFIT_USD", 1.0_f64)?,
            flash_fee_bps: parse_env("LIQUIDATION_FLASH_FEE_BPS", 0_u32)?,
            klend_sidecar_url: std::env::var("LIQUIDATION_KLEND_SIDECAR_URL")
                .or_else(|_| std::env::var("PAIRS_KLEND_SIDECAR_URL"))
                .unwrap_or_else(|_| "http://127.0.0.1:8181".to_string()),
            jupiter_api_url: std::env::var("JUPITER_API_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:8080".to_string()),
            state_path: std::env::var("LIQUIDATION_STATE_PATH")
                .unwrap_or_else(|_| "assets/liquidation_state.json".to_string()),
            actions_path: std::env::var("LIQUIDATION_ACTIONS_PATH")
                .unwrap_or_else(|_| "assets/liquidation_actions.jsonl".to_string()),
            notify_email: parse_bool_env("LIQUIDATION_NOTIFY_EMAIL", true),
        })
    }
}

#[cfg(test)]
impl LiquidationConfig {
    pub(crate) fn test_default() -> Self {
        Self {
            enable: true,
            dry_run: true,
            market: super::kamino::XSTOCKS_MARKET.to_string(),
            scan_every_secs: 300,
            scan_max_hf: 1.05,
            close_factor: 0.5,
            liq_bonus_pct: 5.0,
            min_profit_usd: 1.0,
            flash_fee_bps: 0,
            klend_sidecar_url: String::new(),
            jupiter_api_url: String::new(),
            state_path: String::new(),
            actions_path: String::new(),
            notify_email: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_has_sane_paper_defaults() {
        let c = LiquidationConfig::test_default();
        assert!(c.dry_run, "Phase A is paper");
        assert!((c.scan_max_hf - 1.05).abs() < 1e-9);
        assert_eq!(c.flash_fee_bps, 0, "no flash fee in detection");
        assert_eq!(c.market, super::super::kamino::XSTOCKS_MARKET);
    }
}
