//! Persistent state for the pairs trader.
//!
//! One JSON file holds the current open position (if any), last-close timestamps
//! per pair for cooldowns, and the closed-trade log. Writes are atomic (temp + rename).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One open pair position: long one token, short another, USD collateral.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairPosition {
    pub pair_key: String,
    pub long_mint: String,
    pub long_sym: String,
    pub long_amount: f64,
    pub short_mint: String,
    pub short_sym: String,
    pub short_amount: f64,
    pub usdc_collateral: f64,
    pub entry_ts: i64,
    pub entry_z: f64,
    pub entry_long_px: f64,
    pub entry_short_px: f64,
    pub dry_run: bool,
}

/// A closed pair trade: entry through exit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairTradeRecord {
    pub pair_key: String,
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry_z: f64,
    pub exit_z: f64,
    pub pnl_usdc: f64,
    pub dry_run: bool,
}

/// Persistent state for the pairs trader.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PairsTraderState {
    /// Current open position, if any.
    #[serde(default)]
    pub position: Option<PairPosition>,
    /// Per-pair last-exit timestamp, for the cooldown.
    #[serde(default)]
    pub last_close_ts_per_pair: HashMap<String, i64>,
    /// Closed pair trades, oldest first.
    #[serde(default)]
    pub trades: Vec<PairTradeRecord>,
}

pub fn load(path: &Path) -> Result<PairsTraderState> {
    if !path.exists() {
        return Ok(PairsTraderState::default());
    }
    let data = std::fs::read_to_string(path).context("read pairs state")?;
    if data.trim().is_empty() {
        return Ok(PairsTraderState::default());
    }
    serde_json::from_str(&data).context("parse pairs state")
}

pub fn save(path: &Path, s: &PairsTraderState) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).ok();
    }
    let json = serde_json::to_string_pretty(s)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn trades_last_24h(s: &PairsTraderState, now: i64) -> usize {
    let cutoff = now - 86_400;
    let closed = s.trades.iter().filter(|t| t.entry_ts >= cutoff).count();
    let open = matches!(&s.position, Some(p) if p.entry_ts >= cutoff) as usize;
    closed + open
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("ps_{}.json", rand::random::<u32>()))
    }

    fn pos() -> PairPosition {
        PairPosition {
            pair_key: "NVDAx/SPYx".into(),
            long_mint: "MA".into(),
            long_sym: "NVDAx".into(),
            long_amount: 1.0,
            short_mint: "MB".into(),
            short_sym: "SPYx".into(),
            short_amount: 0.2,
            usdc_collateral: 50.0,
            entry_ts: 1_700_000_000,
            entry_z: -2.4,
            entry_long_px: 50.0,
            entry_short_px: 250.0,
            dry_run: true,
        }
    }

    #[test]
    fn save_load_round_trip() {
        let p = tmp();
        let mut s = PairsTraderState::default();
        s.position = Some(pos());
        s.last_close_ts_per_pair.insert("X/Y".into(), 42);
        save(&p, &s).unwrap();
        let got = load(&p).unwrap();
        assert_eq!(got.position.as_ref().unwrap().pair_key, "NVDAx/SPYx");
        assert_eq!(got.last_close_ts_per_pair.get("X/Y"), Some(&42));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_file_is_flat() {
        assert!(load(&tmp()).unwrap().position.is_none());
    }

    #[test]
    fn trades_last_24h_counts_recent_closed_and_open() {
        let now = 2_000_000_000i64;
        let mut s = PairsTraderState::default();
        s.trades.push(PairTradeRecord {
            pair_key: "A/B".into(),
            entry_ts: now - 100_000,
            exit_ts: now - 90_000,
            entry_z: -2.0,
            exit_z: 0.1,
            pnl_usdc: 1.0,
            dry_run: true,
        }); // outside 24h
        s.trades.push(PairTradeRecord {
            pair_key: "A/B".into(),
            entry_ts: now - 3_600,
            exit_ts: now - 1_000,
            entry_z: -2.0,
            exit_z: 0.1,
            pnl_usdc: 1.0,
            dry_run: true,
        }); // inside
        s.position = Some({ let mut p = pos(); p.entry_ts = now - 60; p }); // open, recent
        assert_eq!(trades_last_24h(&s, now), 2);
    }
}
