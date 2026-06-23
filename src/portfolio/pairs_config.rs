use std::path::Path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use super::{parse_bool_env, parse_env};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairSpec {
    pub symbol_a: String,
    pub mint_a: String,
    pub symbol_b: String,
    pub mint_b: String,
}

pub fn load_pairs(path: &Path) -> Result<Vec<PairSpec>> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("could not read pairs file {}", path.display()))?;
    serde_json::from_str(&data).context("pairs file must be a JSON array of {symbol_a,mint_a,symbol_b,mint_b}")
}

#[derive(Debug, Clone)]
pub struct PairsConfig {
    pub enable: bool,
    pub dry_run: bool,
    pub pairs: Vec<PairSpec>,
    pub lookback_obs: usize,
    pub z_entry: f64,
    pub z_exit: f64,
    pub z_stop: f64,
    pub trade_usdc: f64,
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    pub max_borrow_apy_pct: f64,
    pub min_health_factor: f64,
    pub slippage_bps: u32,
    /// klend-builder sidecar base URL. Empty = the borrowability/APY/health preflight
    /// gate is disabled (pure paper, pre-2c behavior). Set to enforce the gate.
    pub klend_sidecar_url: String,
    pub state_path: String,
    pub halt_path: String,
    pub actions_path: String,
}

impl PairsConfig {
    pub fn from_env() -> Result<Self> {
        let pairs_path = std::env::var("PAIRS_PATH").unwrap_or_else(|_| "assets/pairs.json".to_string());
        let pairs = load_pairs(Path::new(&pairs_path)).unwrap_or_default();
        Ok(Self {
            enable: parse_bool_env("ENABLE_PAIRS_TRADER", false),
            dry_run: parse_bool_env("DRY_RUN_PAIRS_TRADER", true),
            pairs,
            lookback_obs: parse_env("PAIRS_LOOKBACK_OBS", 240_usize)?,
            z_entry: parse_env("PAIRS_Z_ENTRY", 2.0_f64)?,
            z_exit: parse_env("PAIRS_Z_EXIT", 0.5_f64)?,
            z_stop: parse_env("PAIRS_Z_STOP", 4.5_f64)?,
            trade_usdc: parse_env("PAIRS_TRADE_USDC", 50.0_f64)?,
            reentry_cooldown_secs: parse_env("PAIRS_REENTRY_COOLDOWN_SECS", 3600_i64)?,
            max_trades_per_day: parse_env("PAIRS_MAX_TRADES_PER_DAY", 6_u32)?,
            max_borrow_apy_pct: parse_env("PAIRS_MAX_BORROW_APY_PCT", 30.0_f64)?,
            min_health_factor: parse_env("PAIRS_MIN_HEALTH_FACTOR", 1.5_f64)?,
            slippage_bps: parse_env("PAIRS_SLIPPAGE_BPS", 50_u32)?,
            klend_sidecar_url: std::env::var("PAIRS_KLEND_SIDECAR_URL").unwrap_or_default(),
            state_path: std::env::var("PAIRS_STATE_PATH").unwrap_or_else(|_| "assets/pairs_state.json".to_string()),
            halt_path: std::env::var("PAIRS_HALT_PATH").unwrap_or_else(|_| "assets/pairs_halt.json".to_string()),
            actions_path: std::env::var("PAIRS_ACTIONS_PATH").unwrap_or_else(|_| "assets/pairs_actions.jsonl".to_string()),
        })
    }
}

#[cfg(test)]
impl PairsConfig {
    /// Minimal config for unit tests across the pairs modules; override fields with
    /// struct-update syntax (`PairsConfig { trade_usdc: 100.0, ..test_default() }`).
    pub(crate) fn test_default() -> Self {
        Self {
            enable: true,
            dry_run: true,
            pairs: vec![],
            lookback_obs: 240,
            z_entry: 2.0,
            z_exit: 0.5,
            z_stop: 4.5,
            trade_usdc: 50.0,
            reentry_cooldown_secs: 0,
            max_trades_per_day: 6,
            max_borrow_apy_pct: 30.0,
            min_health_factor: 1.5,
            slippage_bps: 50,
            klend_sidecar_url: String::new(),
            state_path: String::new(),
            halt_path: String::new(),
            actions_path: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(json: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("pairs_{}.json", rand::random::<u32>()));
        std::fs::write(&p, json).unwrap();
        p
    }
    #[test]
    fn loads_pairs() {
        let p = tmp(r#"[{"symbol_a":"NVDAx","mint_a":"Xsc9","symbol_b":"SPYx","mint_b":"Xso"}]"#);
        let pairs = load_pairs(&p).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].symbol_a.as_str(), pairs[0].symbol_b.as_str()), ("NVDAx", "SPYx"));
        std::fs::remove_file(&p).ok();
    }
}
