//! Append-only JSONL audit trail for the liquidation detection bot — sibling of
//! [`super::pairs_actions`]. One line per scan/decision: the ground truth for "what did the
//! scanner see, and why was/wasn't an obligation worth liquidating". Append-only; a crash
//! mid-write loses at most the last partial line.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidationAction {
    #[serde(with = "crate::portfolio::ts_serde::rfc3339")]
    pub ts: i64,
    #[serde(flatten)]
    pub kind: LiquidationActionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum LiquidationActionKind {
    /// Per-scan summary: how many obligations were below the watch threshold and how many
    /// of those were profitable to liquidate at current liquidity.
    Heartbeat {
        market: String,
        scanned: usize,
        profitable: usize,
    },
    /// A profitable liquidation opportunity (paper — nothing is submitted in Phase A).
    Detected {
        obligation: String,
        owner: String,
        health_factor: f64,
        repay_sym: String,
        repay_usd: f64,
        seize_sym: String,
        seize_usd: f64,
        seize_impact_bps: u32,
        est_net_usd: f64,
    },
    /// A liquidatable obligation whose seize→USDC economics don't clear the profit floor —
    /// `reason` typically names the killer (e.g. seize impact > bonus on thin collateral).
    SkipUnprofitable {
        obligation: String,
        seize_sym: String,
        seize_impact_bps: u32,
        est_net_usd: f64,
        reason: String,
    },
    /// The scan itself failed (sidecar down, RPC error, etc.).
    ScanFailed { reason: String },
}

/// Append one decision line to the JSONL audit file. Best-effort: callers log (not
/// propagate) an error so auditing never blocks the scanner.
pub fn append(path: &Path, action: &LiquidationAction) -> Result<()> {
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
    fn append_and_round_trip() {
        let path =
            std::env::temp_dir().join(format!("liq_actions_{}.jsonl", rand::random::<u32>()));
        append(
            &path,
            &LiquidationAction {
                ts: 1,
                kind: LiquidationActionKind::Heartbeat {
                    market: "M".into(),
                    scanned: 3,
                    profitable: 1,
                },
            },
        )
        .unwrap();
        append(
            &path,
            &LiquidationAction {
                ts: 2,
                kind: LiquidationActionKind::Detected {
                    obligation: "O".into(),
                    owner: "W".into(),
                    health_factor: 0.98,
                    repay_sym: "USDC".into(),
                    repay_usd: 300.0,
                    seize_sym: "SPYx".into(),
                    seize_usd: 315.0,
                    seize_impact_bps: 5,
                    est_net_usd: 14.5,
                },
            },
        )
        .unwrap();
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 2);
        assert!(body.contains("\"kind\":\"Detected\""));
        for line in body.lines() {
            let _: LiquidationAction = serde_json::from_str(line).expect("valid LiquidationAction");
        }
        std::fs::remove_file(&path).ok();
    }
}
