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
fn rewrite_history(path: &Path, deque: &VecDeque<PriceSnapshot>) -> Result<()> {
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
