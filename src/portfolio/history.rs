use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
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
/// Merge backfilled candles into the history grid **by timestamp**: snapshots at
/// the same ts have their price maps combined (existing/live values always win),
/// and timestamps not yet present are inserted in time order. The result stays
/// sorted oldest-first and capped at `MAX_HISTORY` (newest kept).
///
/// Unlike [`merge_backfill`] (which only prepends data *older* than the deque),
/// this fills a brand-new mint's full series **regardless of how sparse the
/// existing grid is** — so a just-added token has enough observations to rank
/// immediately, instead of needing a pre-existing dense grid to attach to.
pub fn merge_backfill_grid(deque: &mut VecDeque<PriceSnapshot>, backfill: Vec<PriceSnapshot>) {
    if backfill.is_empty() {
        return;
    }
    let mut by_ts: BTreeMap<u64, HashMap<String, f64>> = BTreeMap::new();
    for s in deque.iter() {
        by_ts
            .entry(s.ts)
            .or_default()
            .extend(s.prices.iter().map(|(k, v)| (k.clone(), *v)));
    }
    for s in backfill {
        let entry = by_ts.entry(s.ts).or_default();
        for (k, v) in s.prices {
            entry.entry(k).or_insert(v); // never overwrite an existing (live) value
        }
    }
    deque.clear();
    for (ts, prices) in by_ts {
        if deque.len() == MAX_HISTORY {
            deque.pop_front(); // BTreeMap is ascending, so this keeps the newest
        }
        deque.push_back(PriceSnapshot { ts, prices });
    }
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
    fn merge_grid_warms_new_mint_on_sparse_grid() {
        // Sparse existing grid: AAA at just two timestamps.
        let mut deque: VecDeque<PriceSnapshot> = VecDeque::new();
        deque.push_back(snap(100, "AAA", 1.0));
        deque.push_back(snap(5000, "AAA", 2.0));
        // Backfill MET every 60s for 8 points — overlapping the sparse grid.
        let backfill: Vec<PriceSnapshot> =
            (0..8u64).map(|i| snap(100 + i * 60, "MET", 10.0 + i as f64)).collect();
        merge_backfill_grid(&mut deque, backfill);
        // MET gets all 8 observations regardless of the sparse AAA grid.
        let met = deque.iter().filter(|s| s.prices.contains_key("MET")).count();
        assert_eq!(met, 8);
        // At ts 100, live AAA and backfilled MET coexist; AAA is preserved.
        let at100 = deque.iter().find(|s| s.ts == 100).unwrap();
        assert_eq!(at100.prices.get("AAA"), Some(&1.0));
        assert_eq!(at100.prices.get("MET"), Some(&10.0));
        // Deque stays time-ordered.
        let tss: Vec<u64> = deque.iter().map(|s| s.ts).collect();
        let mut sorted = tss.clone();
        sorted.sort();
        assert_eq!(tss, sorted);
    }

    #[test]
    fn merge_grid_never_overwrites_live_values() {
        let mut deque: VecDeque<PriceSnapshot> = VecDeque::new();
        let mut s = snap(1000, "AAA", 1.0);
        s.prices.insert("MET".to_string(), 99.0); // a real (live) MET observation
        deque.push_back(s);
        merge_backfill_grid(&mut deque, vec![snap(1000, "MET", 11.0)]);
        assert_eq!(deque[0].prices.get("MET"), Some(&99.0), "live value wins over backfill");
    }
}
