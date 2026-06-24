//! Append-only JSONL audit trail for the pairs trader — the sibling of
//! [`super::momentum_actions`]. One line per decision: the ground truth for "why
//! did/didn't this pair act this tick". Append-only so a crash mid-write at most
//! loses the last partial line. Schema mirrors `momentum_actions` (internally-tagged
//! `kind`, flattened) so both files grep the same way.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairAction {
    pub ts: i64,
    #[serde(flatten)]
    pub kind: PairActionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PairActionKind {
    /// Per-pair, per-tick context line — the audit-file analogue of the console
    /// heartbeat. `signal` is the human-readable decision the raw z implies
    /// (`"hold"` / `"long X / short Y"` / `"close"`); persisted so the decision
    /// context behind every other action is recoverable from the file alone.
    Heartbeat { pair: String, z: f64, holding: bool, signal: String },
    /// A pair position was opened (live or paper — see `dry_run`). `z` is the entry z.
    Opened {
        pair: String,
        long_sym: String,
        long_mint: String,
        short_sym: String,
        short_mint: String,
        z: f64,
        long_amount: f64,
        short_amount: f64,
        usdc: f64,
        borrow_apy_pct: f64,
        dry_run: bool,
    },
    /// A pair position was closed back to USDC. `z` is the exit z, `entry_z` the
    /// z it opened at, `hold_secs` the holding duration; `pnl_usdc` is net of
    /// slippage, gas, and borrow funding.
    Closed {
        pair: String,
        z: f64,
        entry_z: f64,
        pnl_usdc: f64,
        hold_secs: i64,
        dry_run: bool,
    },
    /// A close was signalled but the close call errored — the position stays open
    /// and the next tick retries.
    CloseDeferred { pair: String, reason: String },
    /// No new opens this tick: the portfolio risk gate blocked them. `reason` is
    /// the `RiskVerdict` (`"Halted"` — halt file present / tripped breaker — or
    /// `"DailyCapReached"`).
    SkipNoOpens { reason: String },
    /// The klend preflight gate is enabled but the sidecar `/market` call failed,
    /// so opens are suppressed this tick (fail-safe).
    SkipKlendUnreachable { reason: String },
    /// A pair whose signal fired is still benched after a recent close.
    SkipReentryCooldown { pair: String, secs_remaining: i64 },
    /// A pair whose signal fired failed the borrowability/APY/health preflight.
    /// `reason` is the `Preflight` verdict (e.g. `ShortNotBorrowable("GOOGLx")`).
    SkipPreflight { pair: String, reason: String },
    /// `open_pair` errored for a pair whose signal fired (e.g. missing price).
    OpenFailed { pair: String, reason: String },
}

/// Append one decision line to the JSONL audit file. Best-effort: callers log
/// (not propagate) an error so auditing never blocks a trade.
pub fn append(path: &Path, action: &PairAction) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create audit directory")?;
    }
    let line = serde_json::to_string(action).context("audit serialise failed")?;
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("could not open audit file")?;
    writeln!(f, "{line}").context("audit append failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_one_json_line_per_action() {
        let path = std::env::temp_dir().join(format!("pairs_actions_{}.jsonl", rand::random::<u32>()));
        append(&path, &PairAction { ts: 1, kind: PairActionKind::SkipNoOpens { reason: "DailyCapReached".into() } }).unwrap();
        append(
            &path,
            &PairAction { ts: 2, kind: PairActionKind::SkipReentryCooldown { pair: "NVDAx/SPYx".into(), secs_remaining: 2150 } },
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
        for line in body.lines() {
            let _: PairAction = serde_json::from_str(line).expect("valid PairAction JSON");
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn kind_is_internally_tagged_and_round_trips() {
        let action = PairAction {
            ts: 7,
            kind: PairActionKind::Opened {
                pair: "NVDAx/SPYx".into(),
                long_sym: "NVDAx".into(),
                long_mint: "mN".into(),
                short_sym: "SPYx".into(),
                short_mint: "mS".into(),
                z: -2.33,
                long_amount: 0.5,
                short_amount: 0.1,
                usdc: 50.0,
                borrow_apy_pct: 3.4,
                dry_run: true,
            },
        };
        let line = serde_json::to_string(&action).unwrap();
        // tag is lifted to the top level, fields flattened beside it
        assert!(line.contains("\"kind\":\"Opened\""), "got {line}");
        assert!(line.contains("\"pair\":\"NVDAx/SPYx\""), "got {line}");
        let _: PairAction = serde_json::from_str(&line).expect("round-trips");
    }
}
