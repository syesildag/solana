//! Append-only portfolio snapshots — the baseline used by the recovery gate.
//!
//! Every executed swap writes one snapshot recording the pre-action state.
//! On restart the watcher reads `latest()` and refuses to fire a new swap
//! until the live portfolio value rises back above the snapshot total.
//!
//! JSONL is used (not a single `latest.json`) so writes are append-only and
//! crash-safe: a partially-written line at the tail is skipped, never breaking
//! the file. `latest()` reads only the last well-formed line (O(1) regardless
//! of file size).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::Portfolio;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub ts: i64,
    pub reason: String,
    pub sol_amount: f64,
    pub tokens: Vec<TokenHolding>,
    /// symbol → EUR price at snapshot. SOL uses key "SOL"; tokens use their symbol.
    pub prices_eur: HashMap<String, f64>,
    pub total_eur: f64,
    pub planned_action: PlannedAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenHolding {
    pub mint: String,
    pub symbol: String,
    pub amount: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedAction {
    pub sell_symbol: String,
    pub sell_mint: String,
    pub sell_amount: f64,
    pub buy_symbol: String,
    pub buy_mint: String,
    pub expected_buy_amount: f64,
}

/// Build a snapshot in memory. Does not touch disk — call `append` to persist.
pub fn build(
    ts: i64,
    reason: &str,
    portfolio: &Portfolio,
    prices_usd: &HashMap<String, f64>,
    eur_rate: f64,
    action: PlannedAction,
) -> PortfolioSnapshot {
    let mut prices_eur: HashMap<String, f64> = HashMap::new();
    let mut total_eur = 0.0_f64;

    if let Some(&sol_usd) = prices_usd.get("SOL") {
        let sol_eur = sol_usd * eur_rate;
        prices_eur.insert("SOL".to_string(), sol_eur);
        total_eur += sol_eur * portfolio.sol_amount;
    }

    let mut tokens = Vec::with_capacity(portfolio.tokens.len());
    for t in &portfolio.tokens {
        let key = if prices_usd.contains_key(&t.mint) { &t.mint } else { &t.symbol };
        let px_eur = prices_usd.get(key).copied().unwrap_or(0.0) * eur_rate;
        prices_eur.insert(t.symbol.clone(), px_eur);
        total_eur += px_eur * t.amount;
        tokens.push(TokenHolding {
            mint: t.mint.clone(),
            symbol: t.symbol.clone(),
            amount: t.amount,
        });
    }

    PortfolioSnapshot {
        ts,
        reason: reason.to_string(),
        sol_amount: portfolio.sol_amount,
        tokens,
        prices_eur,
        total_eur,
        planned_action: action,
    }
}

/// Append a snapshot as one JSON line. The parent directory is created if missing.
pub fn append(path: &Path, snap: &PortfolioSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create snapshots directory")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("could not open snapshots file for append")?;
    let line = serde_json::to_string(snap).context("snapshot serialise failed")?;
    writeln!(file, "{line}").context("snapshot write failed")?;
    Ok(())
}

/// Return the most recent valid snapshot, or `None` if the file is missing or empty.
/// Reads from the file tail in 4 KB chunks until a newline is found, so this is
/// O(1) regardless of total file size.
pub fn latest(path: &Path) -> Result<Option<PortfolioSnapshot>> {
    if !path.exists() {
        return Ok(None);
    }
    let mut file = std::fs::File::open(path).context("could not open snapshots file")?;
    let len = file.metadata().context("could not stat snapshots file")?.len();
    if len == 0 {
        return Ok(None);
    }

    // Read backwards in chunks. Most lines are far less than 4 KB so one chunk
    // is enough; the loop covers giant lines or trailing partial writes.
    let mut buf: Vec<u8> = Vec::new();
    let mut cursor = len;
    let chunk: u64 = 4096;
    while cursor > 0 {
        let read_start = cursor.saturating_sub(chunk);
        let read_len = (cursor - read_start) as usize;
        let mut tmp = vec![0u8; read_len];
        file.seek(SeekFrom::Start(read_start)).context("seek failed")?;
        file.read_exact(&mut tmp).context("snapshot read failed")?;
        tmp.extend_from_slice(&buf);
        buf = tmp;
        cursor = read_start;

        // Try to parse the last complete line from `buf`.
        if let Some(snap) = last_line_snapshot(&buf) {
            return Ok(Some(snap));
        }
    }
    // File contains data but no parseable line — treat as no baseline.
    Ok(None)
}

fn last_line_snapshot(buf: &[u8]) -> Option<PortfolioSnapshot> {
    // Trim trailing newlines so we don't pick up an empty tail line.
    let mut end = buf.len();
    while end > 0 && (buf[end - 1] == b'\n' || buf[end - 1] == b'\r') {
        end -= 1;
    }
    if end == 0 { return None; }
    let slice = &buf[..end];
    // Walk back to the previous newline (or the start of the buffer).
    let mut start = end;
    while start > 0 && slice[start - 1] != b'\n' {
        start -= 1;
    }
    let line = &slice[start..end];
    serde_json::from_slice::<PortfolioSnapshot>(line).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::TokenEntry;

    fn sample_snapshot(ts: i64, total: f64) -> PortfolioSnapshot {
        PortfolioSnapshot {
            ts,
            reason: "pre-swap".to_string(),
            sol_amount: 1.0,
            tokens: vec![],
            prices_eur: HashMap::new(),
            total_eur: total,
            planned_action: PlannedAction {
                sell_symbol: "AAA".into(),
                sell_mint: "MA".into(),
                sell_amount: 1.0,
                buy_symbol: "BBB".into(),
                buy_mint: "MB".into(),
                expected_buy_amount: 2.0,
            },
        }
    }

    #[test]
    fn append_then_latest_round_trip() {
        let tmp = tempfile_path();
        let s = sample_snapshot(1, 100.0);
        append(&tmp, &s).unwrap();
        let got = latest(&tmp).unwrap().unwrap();
        assert_eq!(got.ts, 1);
        assert!((got.total_eur - 100.0).abs() < 1e-9);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn latest_returns_last_of_many() {
        let tmp = tempfile_path();
        for i in 1..=10 {
            append(&tmp, &sample_snapshot(i, 100.0 * i as f64)).unwrap();
        }
        let got = latest(&tmp).unwrap().unwrap();
        assert_eq!(got.ts, 10);
        assert!((got.total_eur - 1000.0).abs() < 1e-9);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn latest_skips_trailing_newlines() {
        let tmp = tempfile_path();
        append(&tmp, &sample_snapshot(1, 100.0)).unwrap();
        // Simulate a sloppy editor that adds blank lines at the end.
        let mut f = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
        f.write_all(b"\n\n\n").unwrap();
        let got = latest(&tmp).unwrap().unwrap();
        assert_eq!(got.ts, 1);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn latest_none_when_missing() {
        let tmp = tempfile_path();
        let _ = std::fs::remove_file(&tmp);
        assert!(latest(&tmp).unwrap().is_none());
    }

    #[test]
    fn build_computes_eur_total() {
        let mut prices = HashMap::new();
        prices.insert("SOL".to_string(), 100.0);
        prices.insert("MA".to_string(), 50.0);
        let portfolio = Portfolio {
            sol_amount: 2.0,
            tokens: vec![TokenEntry { mint: "MA".into(), symbol: "AAA".into(), amount: 10.0 }],
        };
        let action = PlannedAction {
            sell_symbol: "AAA".into(),
            sell_mint: "MA".into(),
            sell_amount: 2.5,
            buy_symbol: "SOL".into(),
            buy_mint: "So111".into(),
            expected_buy_amount: 1.0,
        };
        let snap = build(123, "pre-swap", &portfolio, &prices, 0.92, action);
        // SOL: 2.0 * 100 * 0.92 = 184
        // AAA: 10.0 * 50 * 0.92 = 460
        // total = 644
        assert!((snap.total_eur - 644.0).abs() < 1e-6);
        assert_eq!(snap.tokens.len(), 1);
    }

    fn tempfile_path() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let name = format!("rebalancer_snap_test_{}.jsonl", rand::random::<u32>());
        dir.join(name)
    }
}
