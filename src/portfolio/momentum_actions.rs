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
    #[serde(with = "crate::portfolio::ts_serde::rfc3339")]
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
    /// Adopted a manually-acquired wallet holding into the trader at startup (no swap).
    /// `entry_price_usd` is the current price used as the cost basis (real basis unknown).
    Adopted {
        symbol: String,
        mint: String,
        token_amount: f64,
        entry_price_usd: f64,
    },
    /// Rotated the held position directly into a higher-scoring token (one A→B swap).
    /// `from_sortino`/`to_sortino` carry the score in the active metric's units (field
    /// names kept for back-compat); `metric` names that metric.
    Rotated {
        from_symbol: String,
        from_mint: String,
        from_sortino: f64,
        to_symbol: String,
        to_mint: String,
        to_sortino: f64,
        to_amount: f64,
        realized_usdc: f64,
        cost_bps: u32,
        sig: String,
        dry_run: bool,
        #[serde(default)]
        metric: String,
    },
    /// A position was closed back to USDC. `reason` names the trigger
    /// (`trailing stop` / `market closed` / `momentum faded`); `#[serde(default)]`
    /// keeps lines written before this field was added parseable.
    Exited {
        symbol: String,
        mint: String,
        usdc_out: f64,
        exit_price_usd: f64,
        peak_price_usd: f64,
        pnl_pct: f64,
        #[serde(default)]
        reason: String,
        sig: String,
        dry_run: bool,
    },
    /// Best candidate's score did not clear the entry threshold. `best_sortino`/
    /// `min_sortino` are in the active metric's units (names kept for back-compat).
    SkipBelowThreshold {
        best_symbol: String,
        best_sortino: f64,
        min_sortino: f64,
        #[serde(default)]
        metric: String,
    },
    /// A candidate lacked enough history for a Sortino.
    SkipWarmup {
        symbol: String,
        have_obs: usize,
        need_obs: usize,
    },
    /// A candidate is still benched after a recent exit.
    SkipReentryCooldown { symbol: String, secs_remaining: i64 },
    /// A candidate's price is frozen (market closed/halted/illiquid) — skipped.
    SkipMarketClosed { symbol: String },
    /// A candidate's lookback window already ran more than `MOMENTUM_MAX_RUN_PCT`
    /// (`run_pct`) **and** is decelerating — momentum likely spent, skipped to avoid
    /// buying the top.
    SkipOverextended {
        symbol: String,
        run_pct: f64,
        max_run_pct: f64,
    },
    /// A candidate's price is actively falling over the recent window
    /// (`MOMENTUM_DECEL_LOOKBACK_MIN`) — never buy into a drop, regardless of run size.
    SkipFalling { symbol: String },
    /// A candidate's ranking `metric` is lower than it was `lag_obs` observations
    /// ago — the trend is rolling over (the JUP case). Skipped so we don't enter a
    /// fading signal. See `MOMENTUM_CONFIRM_LAG_OBS`.
    SkipMetricFading {
        symbol: String,
        metric: String,
        lag_obs: usize,
    },
    /// Entry/exit rejected because cost exceeded the budget.
    SkipCostGate {
        symbol: String,
        total_cost_bps: u32,
        gas_bps: u32,
        slip_bps: u32,
        budget_bps: u32,
    },
    /// Entry/rotation-buy rejected because the Jupiter quote's implied fill price
    /// diverged from the live gRPC price beyond `MOMENTUM_ENTRY_DIVERGENCE_BPS` — the
    /// ranking signal was computed from a price that has since moved on-chain.
    SkipDivergence {
        symbol: String,
        implied: f64,
        grpc: f64,
        dev_bps: u32,
        budget_bps: u32,
    },
    /// Entry rejected by the local gRPC impact pre-gate (`MOMENTUM_LOCAL_IMPACT`)
    /// *before* a Jupiter REST quote was even requested: the ingestion task's estimated
    /// price impact (`est_bps`) of a `MOMENTUM_TRADE_USDC`-sized buy, from live pool
    /// state, exceeded 2x `budget_bps` (`MOMENTUM_MAX_COST_BPS`) — obviously doomed
    /// regardless of routing. Anything within the 2x margin is left to the
    /// authoritative Jupiter quote's `SkipCostGate`.
    SkipLocalImpact {
        symbol: String,
        est_bps: u32,
        budget_bps: u32,
    },
    /// Daily entry cap reached.
    SkipDailyCap { used: usize, cap: u32 },
    /// Wallet's USDC balance is below the trade size — no entry.
    SkipInsufficientUsdc { have: f64, need: f64 },
    /// Jupiter `/quote` failed for a candidate (no route, rate-limit, etc.).
    QuoteFailed { symbol: String, reason: String },
    /// An exit submission reverted (typically `0x1771` slippage on a volatile
    /// token). The position stays armed; the next attempt re-quotes at a wider
    /// tolerance. `attempt` is the new consecutive-failure count.
    ExitReverted {
        symbol: String,
        attempt: u32,
        slippage_bps: u32,
        next_slippage_bps: u32,
        reason: String,
    },
    /// An entry submission reverted (typically `0x1771` — a fast mover ran past
    /// the min-out before the tx landed). Benign: stays FLAT, re-quotes next tick
    /// at a wider (tightly-capped) tolerance. `attempt` is the new consecutive
    /// failure count for this candidate.
    EntryReverted {
        symbol: String,
        attempt: u32,
        slippage_bps: u32,
        next_slippage_bps: u32,
        reason: String,
    },
    /// A staged (TWAP) entry tranche failed mid-ladder (`MOMENTUM_ENTRY_STEPS`):
    /// tranche `step` of `steps` errored (quote or submit), buying stopped, and
    /// the tranches already filled were KEPT as the position. Deliberately
    /// distinct from `EntryReverted`, which means "stayed FLAT, will retry with
    /// escalation" — a mid-ladder failure never touches the escalation record.
    EntryStepFailed {
        symbol: String,
        step: u32,
        steps: u32,
        reason: String,
    },
    /// Open position's dry_run flag disagrees with the configured `DRY_RUN` —
    /// trading refused until the operator resolves it.
    ModeMismatch {
        position_dry_run: bool,
        config_dry_run: bool,
    },
    /// Circuit breaker tripped.
    Halt { reason: String },
    /// Per-tick snapshot of the full ranked watch-list — the same panel printed to
    /// the console by `log_rank_line`, persisted so the decision context behind every
    /// other action is recoverable from the audit file alone. `metric` names the
    /// active ranking metric; `tokens` is best-first in that metric.
    RankSnapshot {
        metric: String,
        min_score: f64,
        tokens: Vec<TokenRank>,
    },
}

/// One token's line in a [`ActionKind::RankSnapshot`]: its symbol plus its state,
/// mirroring the three states `log_rank_line` renders (scored / closed / warming).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenRank {
    pub symbol: String,
    #[serde(flatten)]
    pub state: TokenState,
}

/// A watched token's state in a rank snapshot. `Scored` carries all four metrics
/// (so/sh/sl/rt in the console panel); `Closed` = price frozen (market closed);
/// `Warming` = not enough history yet to compute metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "state")]
pub enum TokenState {
    Scored {
        sortino: f64,
        sharpe: f64,
        slope_r2: f64,
        ret: f64,
    },
    Closed,
    Warming,
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
        let path =
            std::env::temp_dir().join(format!("momentum_actions_{}.jsonl", rand::random::<u32>()));
        append(
            &path,
            &Action {
                ts: 1,
                kind: ActionKind::Halt { reason: "x".into() },
            },
        )
        .unwrap();
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

    #[test]
    fn exited_reason_round_trips_and_defaults() {
        // New `reason` field serializes and parses back.
        let action = Action {
            ts: 7,
            kind: ActionKind::Exited {
                symbol: "BP".into(),
                mint: "m".into(),
                usdc_out: 101.0,
                exit_price_usd: 0.7,
                peak_price_usd: 0.72,
                pnl_pct: 1.0,
                reason: "momentum faded".into(),
                sig: "s".into(),
                dry_run: true,
            },
        };
        let line = serde_json::to_string(&action).unwrap();
        assert!(line.contains("\"reason\":\"momentum faded\""));
        let _: Action = serde_json::from_str(&line).expect("round-trips");

        // A pre-`reason` line (field absent) still parses — defaults to empty.
        let legacy = r#"{"ts":1,"kind":"Exited","symbol":"BP","mint":"m","usdc_out":1.0,"exit_price_usd":0.7,"peak_price_usd":0.7,"pnl_pct":0.0,"sig":"s","dry_run":false}"#;
        let parsed: Action = serde_json::from_str(legacy).expect("legacy line parses");
        match parsed.kind {
            ActionKind::Exited { reason, .. } => assert_eq!(reason, ""),
            _ => panic!("expected Exited"),
        }
    }
}
