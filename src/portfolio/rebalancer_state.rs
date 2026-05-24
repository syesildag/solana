//! Execution history for the auto-rebalancer.
//!
//! Stores every completed swap as an `ExecutionRecord`. Provides the lookups
//! needed by the rebalancer's gating logic:
//!   - daily cap        — count executions in the last 24h
//!   - hold cooldown    — find the last execution of (sell_mint, buy_mint) pair
//!   - take-profit      — compare current buy-side price to entry
//!
//! The file is JSON (not JSONL) because writes are infrequent (≤ 2/day default)
//! and the structured form is convenient for the CLI's future "show history"
//! commands. Writes are atomic via temp + rename.

use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRecord {
    pub ts: i64,
    pub sell_symbol: String,
    pub sell_mint: String,
    pub sell_amount: f64,
    pub sell_price_eur: f64,
    pub buy_symbol: String,
    pub buy_mint: String,
    pub buy_amount: f64,
    pub buy_price_eur: f64,
    pub expected_buy_amount: f64,
    pub slippage_bps_realized: u32,
    pub gas_lamports: u64,
    pub jito_tip_lamports: u64,
    pub total_cost_bps: u32,
    pub tx_sig: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecutionLog {
    #[serde(default)]
    pub executions: Vec<ExecutionRecord>,
}

/// Persistent "the auto-rebalancer has stopped itself" marker. Created when the
/// portfolio fails to recover above the most recent snapshot value within
/// `REBALANCE_LOSS_HALT_DAYS`. Once present, every tick short-circuits silently
/// — the user must delete the file (after investigating) to re-arm trading.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaltRecord {
    pub ts: i64,
    pub reason: String,
    pub snapshot_ts: i64,
    pub snapshot_total_eur: f64,
    pub current_total_eur: f64,
    pub deficit_eur: f64,
    pub age_days: f64,
}

pub fn read_halt(path: &Path) -> Result<Option<HaltRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(path).context("could not read halt file")?;
    if data.trim().is_empty() {
        return Ok(None);
    }
    let rec = serde_json::from_str(&data).context("could not parse halt file")?;
    Ok(Some(rec))
}

pub fn write_halt(path: &Path, rec: &HaltRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create halt directory")?;
    }
    let json = serde_json::to_string_pretty(rec).context("halt serialise failed")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("halt write failed")?;
    std::fs::rename(&tmp, path).context("halt rename failed")?;
    Ok(())
}

pub fn load(path: &Path) -> Result<ExecutionLog> {
    if !path.exists() {
        return Ok(ExecutionLog::default());
    }
    let data = std::fs::read_to_string(path).context("could not read state file")?;
    if data.trim().is_empty() {
        return Ok(ExecutionLog::default());
    }
    serde_json::from_str(&data).context("could not parse state file")
}

pub fn save(path: &Path, log: &ExecutionLog) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create state directory")?;
    }
    let json = serde_json::to_string_pretty(log).context("state serialise failed")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("state write failed")?;
    std::fs::rename(&tmp, path).context("state rename failed")?;
    Ok(())
}

/// Count executions newer than `now_ts - 86_400` seconds.
pub fn count_last_24h(log: &ExecutionLog, now_ts: i64) -> usize {
    let cutoff = now_ts - 86_400;
    log.executions.iter().filter(|e| e.ts >= cutoff).count()
}

/// Look up the most recent execution that sold `sell_mint` and bought `buy_mint`.
/// Used by the hold-cooldown check.
pub fn last_execution_of(
    log: &ExecutionLog,
    sell_mint: &str,
    buy_mint: &str,
) -> Option<ExecutionRecord> {
    log.executions
        .iter()
        .rev()
        .find(|e| e.sell_mint == sell_mint && e.buy_mint == buy_mint)
        .cloned()
}

/// Profit-vs-entry of the bought asset, expressed as a percentage. Positive
/// means the bought asset is up since the swap. Used by the take-profit
/// exception inside the hold-cooldown gate.
pub fn pnl_pct_since(entry: &ExecutionRecord, current_buy_price_eur: f64) -> f64 {
    if entry.buy_price_eur <= 0.0 {
        return 0.0;
    }
    (current_buy_price_eur - entry.buy_price_eur) / entry.buy_price_eur * 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(ts: i64, sell: &str, buy: &str, entry_buy_eur: f64) -> ExecutionRecord {
        ExecutionRecord {
            ts,
            sell_symbol: sell.to_string(),
            sell_mint: format!("M_{sell}"),
            sell_amount: 1.0,
            sell_price_eur: 100.0,
            buy_symbol: buy.to_string(),
            buy_mint: format!("M_{buy}"),
            buy_amount: 2.0,
            buy_price_eur: entry_buy_eur,
            expected_buy_amount: 2.0,
            slippage_bps_realized: 5,
            gas_lamports: 5_000,
            jito_tip_lamports: 6_000,
            total_cost_bps: 30,
            tx_sig: "sig".to_string(),
            status: "confirmed".to_string(),
        }
    }

    #[test]
    fn save_load_round_trip() {
        let path = tempfile_path();
        let mut log = ExecutionLog::default();
        log.executions.push(make_record(1, "A", "B", 50.0));
        save(&path, &log).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got.executions.len(), 1);
        assert_eq!(got.executions[0].sell_symbol, "A");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn count_last_24h_only_recent() {
        let mut log = ExecutionLog::default();
        log.executions.push(make_record(1_000_000, "A", "B", 50.0));   // ancient
        log.executions.push(make_record(2_000_000, "A", "B", 50.0));   // 24h+1s old
        log.executions.push(make_record(2_086_400, "A", "B", 50.0));   // exactly 24h newer
        let now = 2_086_400;
        assert_eq!(count_last_24h(&log, now), 2);
    }

    #[test]
    fn last_execution_of_pair_finds_most_recent() {
        let mut log = ExecutionLog::default();
        log.executions.push(make_record(10, "A", "B", 50.0));
        log.executions.push(make_record(20, "A", "C", 50.0));
        log.executions.push(make_record(30, "A", "B", 60.0));
        let last = last_execution_of(&log, "M_A", "M_B").unwrap();
        assert_eq!(last.ts, 30);
        assert!((last.buy_price_eur - 60.0).abs() < 1e-9);
    }

    #[test]
    fn halt_round_trip() {
        let path = std::env::temp_dir().join(format!("halt_test_{}.json", rand::random::<u32>()));
        assert!(read_halt(&path).unwrap().is_none(), "missing file → None");
        let rec = HaltRecord {
            ts: 1_700_000_000,
            reason: "persistent loss".to_string(),
            snapshot_ts: 1_697_500_000,
            snapshot_total_eur: 1000.0,
            current_total_eur: 900.0,
            deficit_eur: 100.0,
            age_days: 28.9,
        };
        write_halt(&path, &rec).unwrap();
        let got = read_halt(&path).unwrap().unwrap();
        assert_eq!(got.ts, rec.ts);
        assert!((got.deficit_eur - 100.0).abs() < 1e-9);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn pnl_pct_handles_gain_and_loss() {
        let r = make_record(1, "A", "B", 100.0);
        assert!((pnl_pct_since(&r, 105.0) - 5.0).abs() < 1e-9);
        assert!((pnl_pct_since(&r, 90.0) - -10.0).abs() < 1e-9);
        assert_eq!(pnl_pct_since(&make_record(1, "A", "B", 0.0), 50.0), 0.0);
    }

    fn tempfile_path() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let name = format!("rebalancer_state_test_{}.json", rand::random::<u32>());
        dir.join(name)
    }
}
