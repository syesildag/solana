//! Append-only audit log of every decision the auto-rebalancer makes.
//!
//! One JSONL line per action — including skips with reasons, the halt trigger,
//! dry-run evaluations, and confirmed executions. Tick-level no-ops (master
//! switch off, halt already active, recovery still pending, daily cap reached
//! with no signals) are *not* logged here: they would dominate the file and
//! they're already visible in the tracing-subscriber output. This file is the
//! "what did the bot decide about each opportunity" record, suitable for later
//! analysis or replay.

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

/// `serde(tag = "kind")` produces flat JSON like
/// `{"ts":1,"kind":"Executed","sell":"NVDAx",...}`
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ActionKind {
    /// The recovery gate failed but the snapshot is still within the wait
    /// window. We are deliberately doing nothing — logged so the operator can
    /// see the countdown without scrubbing journalctl.
    RecoveryWait {
        deficit_eur: f64,
        snapshot_total_eur: f64,
        current_total_eur: f64,
        snapshot_age_days: f64,
        days_until_halt: f64,
    },
    /// Loss-halt circuit breaker tripped. Auto-rebalance is now blocked
    /// until the halt file is manually cleared.
    HaltTriggered {
        deficit_eur: f64,
        snapshot_total_eur: f64,
        current_total_eur: f64,
        snapshot_age_days: f64,
    },
    /// A signal made it past the top-level gates and entered per-signal
    /// evaluation. One of the four outcomes below should follow it on the
    /// same tick (one ConsideredSignal per signal evaluated).
    ConsideredSignal {
        sell: String,
        buy: String,
        sell_value_eur: f64,
        buy_value_eur: f64,
        sell_decline_pct: f64,
        buy_rise_pct: f64,
    },
    /// Per-signal: one leg is below the configured minimum position floor.
    SkipMinPosition {
        sell: String,
        buy: String,
        sell_value_eur: f64,
        buy_value_eur: f64,
        min_eur: f64,
    },
    /// Per-signal: same direction was traded within HOLD_DAYS without enough
    /// take-profit accumulation.
    SkipHoldCooldown {
        sell: String,
        buy: String,
        age_days: f64,
        pnl_pct: f64,
        take_profit_pct: f64,
    },
    /// Per-signal: estimated round-trip cost exceeds the budget.
    SkipCostGate {
        sell: String,
        buy: String,
        total_cost_bps: u32,
        gas_bps: u32,
        slip_bps: u32,
        budget_bps: u32,
    },
    /// Per-signal: dry-run mode — would have executed but submission was
    /// suppressed by `REBALANCE_DRY_RUN=true`.
    DryRun {
        sell: String,
        buy: String,
        sell_amount: f64,
        expected_buy_amount: f64,
        total_cost_bps: u32,
    },
    /// Per-signal: a single submit attempt failed. Logged on EVERY attempt
    /// (not just the final one) so the audit trail shows why each retry was
    /// needed. The next line for the same `(sell, buy)` is either another
    /// `RetryAttempt`, an `Executed`, or an `AllRetriesFailed`.
    RetryAttempt {
        sell: String,
        buy: String,
        attempt: u32,
        max_attempts: u32,
        reason: String,
    },
    /// Per-signal: all retry attempts exhausted without a confirmed swap.
    AllRetriesFailed {
        sell: String,
        buy: String,
        attempts: u32,
        reason: String,
    },
    /// Per-signal: a swap actually went out the wire. `status` is "confirmed"
    /// when RPC confirmation arrived inside the timeout, otherwise
    /// "unconfirmed".
    Executed {
        sell: String,
        buy: String,
        sell_amount: f64,
        buy_amount: f64,
        total_cost_bps: u32,
        tx_sig: String,
        status: String,
        attempts_used: u32,
    },
}

pub fn append(path: &Path, action: &Action) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create actions directory")?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("could not open actions file")?;
    let line = serde_json::to_string(action).context("action serialise failed")?;
    writeln!(file, "{line}").context("action write failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("rebalancer_actions_test_{}.jsonl", rand::random::<u32>()))
    }

    #[test]
    fn append_executed_round_trip() {
        let path = tmp();
        let a = Action {
            ts: 1_700_000_000,
            kind: ActionKind::Executed {
                sell: "NVDAx".into(),
                buy: "TSLAx".into(),
                sell_amount: 0.5,
                buy_amount: 1.2,
                total_cost_bps: 32,
                tx_sig: "abc".into(),
                status: "confirmed".into(),
                attempts_used: 1,
            },
        };
        append(&path, &a).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let line = BufReader::new(file).lines().next().unwrap().unwrap();
        let back: Action = serde_json::from_str(&line).unwrap();
        assert_eq!(back.ts, 1_700_000_000);
        match back.kind {
            ActionKind::Executed { sell, buy, status, .. } => {
                assert_eq!(sell, "NVDAx");
                assert_eq!(buy, "TSLAx");
                assert_eq!(status, "confirmed");
            }
            _ => panic!("wrong variant"),
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn append_skip_variants_all_distinguishable() {
        let path = tmp();
        let cases = [
            Action { ts: 1, kind: ActionKind::SkipMinPosition {
                sell: "A".into(), buy: "B".into(),
                sell_value_eur: 10.0, buy_value_eur: 20.0, min_eur: 25.0 } },
            Action { ts: 2, kind: ActionKind::SkipHoldCooldown {
                sell: "A".into(), buy: "B".into(),
                age_days: 3.0, pnl_pct: 1.5, take_profit_pct: 5.0 } },
            Action { ts: 3, kind: ActionKind::SkipCostGate {
                sell: "A".into(), buy: "B".into(),
                total_cost_bps: 80, gas_bps: 30, slip_bps: 50, budget_bps: 50 } },
        ];
        for a in &cases { append(&path, a).unwrap(); }
        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("SkipMinPosition"));
        assert!(lines[1].contains("SkipHoldCooldown"));
        assert!(lines[2].contains("SkipCostGate"));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn flatten_tag_yields_inline_kind_field() {
        // Spot-check that we're using `tag = "kind"` correctly — the JSON
        // should have "kind" as a top-level field, NOT nested under a
        // "kind" object.
        let a = Action {
            ts: 1,
            kind: ActionKind::RecoveryWait {
                deficit_eur: 1.0, snapshot_total_eur: 2.0, current_total_eur: 1.0,
                snapshot_age_days: 1.0, days_until_halt: 20.0,
            },
        };
        let json = serde_json::to_string(&a).unwrap();
        assert!(json.contains("\"kind\":\"RecoveryWait\""), "got: {json}");
        assert!(!json.contains("\"kind\":{"), "kind must be flat, not nested: {json}");
    }
}
