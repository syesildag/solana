//! Persistent state for the liquidation detection bot.
//!
//! One JSON file holds the last scan timestamp (to pace the heavy bulk scan), per-obligation
//! last-detection timestamps (to throttle duplicate `Detected` logs), and the log of detected
//! opportunities. Writes are atomic (temp + rename), mirroring `pairs_state`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One detected (paper) liquidation opportunity.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DetectionRecord {
    #[serde(with = "crate::portfolio::ts_serde::rfc3339")]
    pub ts: i64,
    pub obligation: String,
    pub owner: String,
    pub health_factor: f64,
    pub repay_sym: String,
    pub repay_usd: f64,
    pub seize_sym: String,
    pub seize_impact_bps: u32,
    pub est_net_usd: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LiquidationState {
    /// Last full-scan timestamp, used to pace scans to `scan_every_secs`.
    #[serde(default, with = "crate::portfolio::ts_serde::rfc3339")]
    pub last_scan_ts: i64,
    /// Per-obligation last `Detected` timestamp, to throttle duplicate logging.
    #[serde(default, with = "crate::portfolio::ts_serde::rfc3339_map")]
    pub last_detected_ts: HashMap<String, i64>,
    /// Detected opportunities, oldest first.
    #[serde(default)]
    pub detections: Vec<DetectionRecord>,
}

pub fn load(path: &Path) -> Result<LiquidationState> {
    if !path.exists() {
        return Ok(LiquidationState::default());
    }
    let data = std::fs::read_to_string(path).context("read liquidation state")?;
    if data.trim().is_empty() {
        return Ok(LiquidationState::default());
    }
    serde_json::from_str(&data).context("parse liquidation state")
}

pub fn save(path: &Path, s: &LiquidationState) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let json = serde_json::to_string_pretty(s)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trip() {
        let path = std::env::temp_dir().join(format!("liq_state_{}.json", rand::random::<u32>()));
        let s = LiquidationState {
            last_scan_ts: 123,
            last_detected_ts: HashMap::from([("ob1".to_string(), 100)]),
            detections: vec![DetectionRecord {
                ts: 100,
                obligation: "ob1".into(),
                owner: "w".into(),
                health_factor: 0.97,
                repay_sym: "USDC".into(),
                repay_usd: 300.0,
                seize_sym: "SPYx".into(),
                seize_impact_bps: 5,
                est_net_usd: 14.0,
            }],
        };
        save(&path, &s).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back.last_scan_ts, 123);
        assert_eq!(back.last_detected_ts.get("ob1"), Some(&100));
        assert_eq!(back.detections.len(), 1);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn load_missing_is_default() {
        let path = std::env::temp_dir().join(format!("liq_missing_{}.json", rand::random::<u32>()));
        assert_eq!(load(&path).unwrap().detections.len(), 0);
    }
}
