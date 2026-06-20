use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::Path;

/// Maximum snapshots kept in memory — 30 days at 1-minute intervals.
/// 30 × 24 × 60 = 43_200. File is trimmed to this on load (~9 MB on disk).
pub const MAX_HISTORY: usize = 43_200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PriceSnapshot {
    /// Unix timestamp (seconds).
    pub ts: u64,
    /// Symbol/mint → USD price.
    pub prices: HashMap<String, f64>,
}

/// Load existing history from a JSONL file into a capped deque.
/// Lines that fail to parse are silently skipped (corrupt tail after crash).
/// If the file exceeds MAX_HISTORY entries it is rewritten with only the
/// most recent MAX_HISTORY lines so it never grows unboundedly.
pub fn load_history(path: &Path) -> Result<VecDeque<PriceSnapshot>> {
    if !path.exists() {
        return Ok(VecDeque::new());
    }
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut deque: VecDeque<PriceSnapshot> = VecDeque::with_capacity(MAX_HISTORY);
    let mut total_lines = 0usize;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        total_lines += 1;
        if let Ok(snap) = serde_json::from_str::<PriceSnapshot>(&line) {
            if deque.len() == MAX_HISTORY {
                deque.pop_front();
            }
            deque.push_back(snap);
        }
    }
    // Rewrite the file if it had more entries than we keep in memory.
    if total_lines > MAX_HISTORY {
        rewrite_history(path, &deque)?;
    }
    Ok(deque)
}

/// Rewrite the JSONL file with exactly the entries currently in the deque.
/// Call periodically to cap the file at MAX_HISTORY entries.
pub fn rewrite_history(path: &Path, deque: &VecDeque<PriceSnapshot>) -> Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("jsonl.tmp");
    let mut file = std::fs::File::create(&tmp)?;
    for snap in deque {
        let line = serde_json::to_string(snap)?;
        writeln!(file, "{line}")?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Append a single snapshot to the JSONL file without rewriting it.
pub fn append_snapshot(path: &Path, snap: &PriceSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(snap)?;
    writeln!(file, "{line}")?;
    Ok(())
}

/// Merge Birdeye backfill snapshots into the deque, skipping timestamps already present.
/// Graft a single mint's backfilled candles onto the EXISTING snapshot grid by
/// forward-fill: each snapshot that lacks `mint` gets the price of the most
/// recent candle at-or-before its timestamp (within `max_gap_secs`). Live prices
/// already present are never overwritten. Returns how many snapshots were filled.
///
/// Unlike [`merge_backfill`] (which only extends history *backwards*), this fills
/// a brand-new mint across the current time range WITHOUT adding snapshots — so it
/// makes the mint rankable while leaving the count-based windows other consumers
/// (alerts, RSI) rely on untouched.
pub fn graft_mint_backfill(
    deque: &mut VecDeque<PriceSnapshot>,
    mint: &str,
    mut candles: Vec<(u64, f64)>,
    max_gap_secs: u64,
) -> usize {
    if candles.is_empty() {
        return 0;
    }
    candles.sort_by_key(|(ts, _)| *ts);
    let mut ci = 0usize;
    let mut last: Option<(u64, f64)> = None; // most recent candle at-or-before the cursor
    let mut filled = 0usize;
    // deque is oldest-first; advance the candle cursor in lockstep.
    for snap in deque.iter_mut() {
        while ci < candles.len() && candles[ci].0 <= snap.ts {
            last = Some(candles[ci]);
            ci += 1;
        }
        if snap.prices.contains_key(mint) {
            continue; // never clobber a real (live) observation
        }
        if let Some((cts, price)) = last {
            if snap.ts.saturating_sub(cts) <= max_gap_secs {
                snap.prices.insert(mint.to_string(), price);
                filled += 1;
            }
        }
    }
    filled
}

pub fn merge_backfill(deque: &mut VecDeque<PriceSnapshot>, mut backfill: Vec<PriceSnapshot>) {
    if deque.is_empty() {
        backfill.sort_by_key(|s| s.ts);
        for snap in backfill {
            if deque.len() == MAX_HISTORY {
                deque.pop_front();
            }
            deque.push_back(snap);
        }
        return;
    }
    let earliest = deque.front().map(|s| s.ts).unwrap_or(0);
    backfill.retain(|s| s.ts < earliest);
    backfill.sort_by_key(|s| s.ts);
    for snap in backfill.into_iter().rev() {
        if deque.len() == MAX_HISTORY {
            deque.pop_back();
        }
        deque.push_front(snap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn snap(ts: u64, mint: &str, price: f64) -> PriceSnapshot {
        let mut prices = HashMap::new();
        prices.insert(mint.to_string(), price);
        PriceSnapshot { ts, prices }
    }

    #[test]
    fn graft_forward_fills_new_mint_onto_existing_grid() {
        let mut deque: VecDeque<PriceSnapshot> = VecDeque::new();
        deque.push_back(snap(100, "AAA", 1.0));
        deque.push_back(snap(160, "AAA", 1.0));
        deque.push_back(snap(220, "AAA", 1.0));
        // MET candles minute-aligned, offset from the live grid.
        let candles = vec![(90, 10.0), (150, 11.0), (210, 12.0)];
        let filled = graft_mint_backfill(&mut deque, "MET", candles, 300);
        assert_eq!(filled, 3);
        // forward-fill: each snapshot takes the most recent candle at-or-before it.
        assert_eq!(deque[0].prices.get("MET"), Some(&10.0));
        assert_eq!(deque[1].prices.get("MET"), Some(&11.0));
        assert_eq!(deque[2].prices.get("MET"), Some(&12.0));
        assert_eq!(deque.len(), 3, "no snapshots added");
        assert_eq!(deque[0].prices.get("AAA"), Some(&1.0), "existing data untouched");
    }

    #[test]
    fn graft_respects_max_gap_and_keeps_live() {
        let mut deque: VecDeque<PriceSnapshot> = VecDeque::new();
        deque.push_back(snap(1000, "AAA", 1.0)); // nearest candle is >gap back → not filled
        let mut s = snap(2000, "AAA", 1.0);
        s.prices.insert("MET".to_string(), 99.0); // live MET value present
        deque.push_back(s);
        graft_mint_backfill(&mut deque, "MET", vec![(100, 10.0), (1990, 11.0)], 300);
        assert_eq!(deque[0].prices.get("MET"), None, "candle 900s back exceeds 300s gap");
        assert_eq!(deque[1].prices.get("MET"), Some(&99.0), "live value not overwritten");
    }
}
