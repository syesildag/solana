//! Append-only JSONL audit trail for the momentum trader. One line per decision
//! — the ground truth for "why did/didn't it act this tick". Append-only so a
//! crash mid-write at most loses the last partial line.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Action {
    pub ts: i64,
    #[serde(flatten)]
    pub kind: ActionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ActionKind {
    /// A position was opened (live or paper — see `dry_run`).
    Entered {
        symbol: String,
        mint: String,
        usdc_in: f64,
        token_amount: f64,
        entry_price_usd: f64,
        cost_bps: u32,
        sig: String,
        dry_run: bool,
    },
    /// A position was closed back to USDC.
    Exited {
        symbol: String,
        mint: String,
        usdc_out: f64,
        exit_price_usd: f64,
        peak_price_usd: f64,
        pnl_pct: f64,
        sig: String,
        dry_run: bool,
    },
    /// Best candidate's Sortino did not clear the entry threshold.
    SkipBelowThreshold { best_symbol: String, best_sortino: f64, min_sortino: f64 },
    /// A candidate lacked enough history for a Sortino.
    SkipWarmup { symbol: String, have_obs: usize, need_obs: usize },
    /// A candidate is still benched after a recent exit.
    SkipReentryCooldown { symbol: String, secs_remaining: i64 },
    /// Entry/exit rejected because cost exceeded the budget.
    SkipCostGate { symbol: String, total_cost_bps: u32, gas_bps: u32, slip_bps: u32, budget_bps: u32 },
    /// Daily entry cap reached.
    SkipDailyCap { used: usize, cap: u32 },
    /// Jupiter `/quote` failed for a candidate (no route, rate-limit, etc.).
    QuoteFailed { symbol: String, reason: String },
    /// Open position's dry_run flag disagrees with the configured `DRY_RUN` —
    /// trading refused until the operator resolves it.
    ModeMismatch { position_dry_run: bool, config_dry_run: bool },
    /// Circuit breaker tripped.
    Halt { reason: String },
}

/// Append one decision line to the JSONL audit file. Best-effort: callers log
/// (not propagate) an error so auditing never blocks a trade.
pub fn append(path: &Path, action: &Action) -> Result<()> {
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
        let path = std::env::temp_dir()
            .join(format!("momentum_actions_{}.jsonl", rand::random::<u32>()));
        append(&path, &Action { ts: 1, kind: ActionKind::Halt { reason: "x".into() } }).unwrap();
        append(
            &path,
            &Action {
                ts: 2,
                kind: ActionKind::SkipDailyCap { used: 4, cap: 4 },
            },
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
        // each line round-trips back to an Action
        for line in body.lines() {
            let _: Action = serde_json::from_str(line).expect("valid Action JSON");
        }
        std::fs::remove_file(&path).ok();
    }
}
