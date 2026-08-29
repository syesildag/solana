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
    /// A live position was dropped WITHOUT a sell because its on-chain balance is
    /// confirmed zero (sold/moved externally). Benches the mint like an exit. The
    /// confirmation read is mandatory — a wallet-scan miss alone must never emit this
    /// (the silent invalidate→re-adopt loop that reset CATE's trail peak, 2026-08-16).
    Invalidated {
        symbol: String,
        mint: String,
        #[serde(default)]
        token_amount: f64,
        #[serde(default)]
        entry_price_usd: f64,
        #[serde(default)]
        peak_price_usd: f64,
        #[serde(default)]
        last_price_usd: f64,
        #[serde(default)]
        dry_run: bool,
    },
    /// A nominated (scan-missing) live position was KEPT: the on-chain confirmation did
    /// not return a confirmed zero. The counterpart of `Invalidated` — together they make
    /// every nomination auditable, so a position that keeps being nominated and kept
    /// (a flapping RPC, an unparseable ATA) is visible instead of silent.
    /// `reason` ∈ {"non-zero", "unconfirmed", "read-failed", "too-young"}.
    InvalidateSkipped {
        symbol: String,
        mint: String,
        reason: String,
    },
    /// A live sell (exit or rotation) was DEFERRED because the raw on-chain balance
    /// could not be read even after retries. Selling the recorded amount instead is
    /// not safe in either direction: it overshoots a `scaledUiAmount` mint (AAPLx
    /// 2026-08-04) and — worse — a wallet manually topped up mid-hold sells only the
    /// ledger amount, CLOSES the position, and orphans the surplus with no stop
    /// (ZEC 2026-08-23: $10.23 sold of a ~$1458 holding). The position is kept, the
    /// stop stays armed, and the next tick re-reads the balance.
    ExitBalanceReadFailed {
        symbol: String,
        mint: String,
        reason: String,
    },
    /// Adopted a manually-acquired wallet holding into the trader at startup (no swap).
    /// `entry_price_usd` is the current price used as the cost basis (real basis unknown).
    Adopted {
        symbol: String,
        mint: String,
        token_amount: f64,
        entry_price_usd: f64,
        /// True when the adoption came from the unwatched-holdings pass.
        #[serde(default)]
        unwatched: bool,
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
    /// A submitted swap never landed and never can: its blockhash expired with no
    /// signature status ever observed, which is proof of non-inclusion (no fee was
    /// paid, no tokens moved). Distinct from `EntryReverted`/`ExitReverted`, which
    /// mean the tx DID land and failed — a drop never touches slippage escalation,
    /// because widening tolerance cannot fix a propagation failure. `leg` says which
    /// submission it was: `entry`, `entry-tranche-N/M`, `rotate-from-SYM`, or `exit`.
    SubmitDropped {
        symbol: String,
        leg: String,
        sig: String,
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
    /// Periodic order-flow reading for every watched token — the JSONL twin of the
    /// `momentum flow:` console line. Written whether or not any gate is enabled, because
    /// this gate can never be backtested (`price_history.jsonl` carries prices only): this
    /// record IS the dataset that lets the thresholds be judged later.
    FlowSnapshot { tokens: Vec<TokenFlow> },
    /// Entry vetoed by the order-flow gate (volume floor or distribution divergence).
    SkipFlowGate { symbol: String, reason: String },
}

/// One token's order-flow line in an [`ActionKind::FlowSnapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenFlow {
    pub symbol: String,
    pub vol_h1: f64,
    pub vol_h24: f64,
    pub buys_h1: u64,
    pub sells_h1: u64,
    /// Sells per buy. `None` when there were no buys in the window.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sell_buy_ratio: Option<f64>,
    /// 1h volume as a multiple of the token's own 24h hourly average.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vol_decay: Option<f64>,
    pub price_chg_h1: f64,
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
    fn exit_balance_read_failed_round_trips() {
        let action = Action {
            ts: 9,
            kind: ActionKind::ExitBalanceReadFailed {
                symbol: "ZEC".into(),
                mint: "A7bdiYdS5GjqGFtxf17ppRHtDKPkkRqbKtR27dxvQXaS".into(),
                reason: "get_token_accounts_by_owner(mint) failed".into(),
            },
        };
        let line = serde_json::to_string(&action).unwrap();
        assert!(line.contains("\"kind\":\"ExitBalanceReadFailed\""));
        let _: Action = serde_json::from_str(&line).expect("round-trips");
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

    #[test]
    fn invalidated_round_trips_and_legacy_two_field_line_parses() {
        // Full record (the shape the trader writes since the position-detail fields
        // were added) round-trips.
        let action = Action {
            ts: 9,
            kind: ActionKind::Invalidated {
                symbol: "CATE".into(),
                mint: "m".into(),
                token_amount: 6959.393224,
                entry_price_usd: 0.01,
                peak_price_usd: 0.02,
                last_price_usd: 0.015,
                dry_run: false,
            },
        };
        let line = serde_json::to_string(&action).unwrap();
        assert!(line.contains("\"peak_price_usd\":0.02"));
        let _: Action = serde_json::from_str(&line).expect("round-trips");

        // A pre-detail line (only symbol + mint were written) must still parse —
        // the historical actions log is append-only and is read back by tooling.
        let legacy = r#"{"ts":1,"kind":"Invalidated","symbol":"S","mint":"M"}"#;
        let parsed: Action = serde_json::from_str(legacy).expect("legacy line parses");
        match parsed.kind {
            ActionKind::Invalidated {
                symbol,
                mint,
                token_amount,
                entry_price_usd,
                peak_price_usd,
                last_price_usd,
                dry_run,
            } => {
                assert_eq!(symbol, "S");
                assert_eq!(mint, "M");
                assert_eq!(token_amount, 0.0);
                assert_eq!(entry_price_usd, 0.0);
                assert_eq!(peak_price_usd, 0.0);
                assert_eq!(last_price_usd, 0.0);
                assert!(!dry_run);
            }
            _ => panic!("expected Invalidated"),
        }
    }

    #[test]
    fn invalidate_skipped_round_trips() {
        let action = Action {
            ts: 11,
            kind: ActionKind::InvalidateSkipped {
                symbol: "CATE".into(),
                mint: "m".into(),
                reason: "unconfirmed".into(),
            },
        };
        let line = serde_json::to_string(&action).unwrap();
        assert!(line.contains("\"kind\":\"InvalidateSkipped\""));
        assert!(line.contains("\"reason\":\"unconfirmed\""));
        let parsed: Action = serde_json::from_str(&line).expect("round-trips");
        match parsed.kind {
            ActionKind::InvalidateSkipped { reason, .. } => assert_eq!(reason, "unconfirmed"),
            _ => panic!("expected InvalidateSkipped"),
        }
    }
}
