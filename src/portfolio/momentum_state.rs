//! Persistent state for the momentum trader.
//!
//! One JSON file holds the single open position (if any), per-mint re-entry
//! cooldowns, and the closed-trade log. `position: Option<Position>` IS the
//! FLAT/HOLDING invariant — the type cannot represent two open positions.
//! Writes are atomic (temp + rename). A separate halt file is the circuit
//! breaker: while present, every tick short-circuits until the operator deletes
//! it.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// One open position. `None` in `TraderState` ⇒ FLAT; `Some` ⇒ HOLDING exactly
/// one token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub mint: String,
    pub symbol: String,
    #[serde(with = "crate::portfolio::ts_serde::rfc3339")]
    pub entry_ts: i64,
    /// Token USD price at entry.
    pub entry_price_usd: f64,
    /// Tokens acquired (expected-out in dry-run; the real fill in live).
    pub token_amount: f64,
    /// Human USDC committed to this position.
    pub usdc_spent: f64,
    /// Running max of the token USD price since entry — drives the trailing stop.
    pub peak_price_usd: f64,
    /// Entry transaction signature ("dry-run" in paper mode); carried into the
    /// closed `TradeRecord` for the audit trail.
    #[serde(default)]
    pub entry_sig: String,
    /// Provenance: was this opened in paper mode? Guards the dry/live boundary.
    pub dry_run: bool,
}

impl Position {
    /// Heal a missing/invalid persisted peak (≤0, NaN, or below entry) using the
    /// max price observed since entry. Keeps the peak finite, monotone, and at
    /// least the entry price. Called once on load (restart-safety).
    pub fn repair_peak(&mut self, history_max_since_entry: f64) {
        let base = if self.peak_price_usd.is_finite() && self.peak_price_usd >= self.entry_price_usd
        {
            self.peak_price_usd
        } else {
            self.entry_price_usd
        };
        let healed = base.max(history_max_since_entry).max(self.entry_price_usd);
        if healed.is_finite() {
            self.peak_price_usd = healed;
        } else {
            self.peak_price_usd = self.entry_price_usd;
        }
    }
}

/// Tracks how many times the entry into a specific candidate has reverted in a
/// row, so the next attempt can widen its slippage. Reset when the best
/// candidate changes (see `entry_attempt_for`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryAttempt {
    pub mint: String,
    pub count: u32,
}

/// A closed round-trip: USDC → token → USDC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    #[serde(with = "crate::portfolio::ts_serde::rfc3339")]
    pub entry_ts: i64,
    #[serde(with = "crate::portfolio::ts_serde::rfc3339")]
    pub exit_ts: i64,
    pub mint: String,
    pub symbol: String,
    pub entry_price_usd: f64,
    pub exit_price_usd: f64,
    pub peak_price_usd: f64,
    pub usdc_in: f64,
    pub usdc_out: f64,
    /// Realized PnL of the round-trip as a percentage of USDC in.
    pub pnl_pct: f64,
    pub entry_sig: String,
    pub exit_sig: String,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TraderState {
    /// `None` ⇒ FLAT, `Some` ⇒ HOLDING. The single-position invariant.
    #[serde(default)]
    pub position: Option<Position>,
    /// Per-mint last-exit timestamp, for the re-entry cooldown.
    #[serde(default, with = "crate::portfolio::ts_serde::rfc3339_map")]
    pub last_exit_ts_per_mint: HashMap<String, i64>,
    /// Per-mint count of consecutive failed exit submissions. Drives the
    /// self-escalating exit slippage; cleared the moment an exit lands (or the
    /// position is otherwise resolved). Persisted so escalation survives restarts.
    #[serde(default)]
    pub exit_attempts_per_mint: HashMap<String, u32>,
    /// Consecutive failed entry submissions for the *current* best candidate.
    /// Drives the bounded entry-slippage escalation; cleared on a successful
    /// entry and reset whenever the chosen candidate changes (a chase is never
    /// carried across tokens). Only one entry is ever in flight (FLAT ⇒ one
    /// best), so a single record suffices over a per-mint map.
    #[serde(default)]
    pub entry_attempt: Option<EntryAttempt>,
    /// Closed round-trips, oldest first.
    #[serde(default)]
    pub trades: Vec<TradeRecord>,
}

/// Aggregate realized performance over the closed-trade log. Derived (never
/// stored incrementally) so it can't drift from the trades that produced it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PnlSummary {
    pub closed_trades: usize,
    pub wins: usize,
    pub losses: usize,
    /// Σ(usdc_out − usdc_in) across all closed trades.
    pub realized_usdc: f64,
    /// realized_usdc as a percentage of total USDC deployed.
    pub realized_pct: f64,
    pub win_rate_pct: f64,
    pub best_trade_pct: f64,
    pub worst_trade_pct: f64,
}

/// Compute realized performance from the closed-trade log.
pub fn summarize(trades: &[TradeRecord]) -> PnlSummary {
    let mut s = PnlSummary::default();
    if trades.is_empty() {
        return s;
    }
    let mut invested = 0.0;
    s.best_trade_pct = f64::MIN;
    s.worst_trade_pct = f64::MAX;
    for t in trades {
        s.realized_usdc += t.usdc_out - t.usdc_in;
        invested += t.usdc_in;
        if t.usdc_out >= t.usdc_in {
            s.wins += 1;
        } else {
            s.losses += 1;
        }
        s.best_trade_pct = s.best_trade_pct.max(t.pnl_pct);
        s.worst_trade_pct = s.worst_trade_pct.min(t.pnl_pct);
    }
    s.closed_trades = trades.len();
    s.realized_pct = if invested > 0.0 {
        s.realized_usdc / invested * 100.0
    } else {
        0.0
    };
    s.win_rate_pct = s.wins as f64 / s.closed_trades as f64 * 100.0;
    s
}

/// Count entries within the last 24h. An open position counts as today's entry
/// if it was opened inside the window — so the daily cap gates *entries*, not
/// round-trips.
pub fn entries_last_24h(state: &TraderState, now_ts: i64) -> usize {
    let cutoff = now_ts - 86_400;
    let closed = state.trades.iter().filter(|t| t.entry_ts >= cutoff).count();
    let open = match &state.position {
        Some(p) if p.entry_ts >= cutoff => 1,
        _ => 0,
    };
    closed + open
}

pub fn load(path: &Path) -> Result<TraderState> {
    if !path.exists() {
        return Ok(TraderState::default());
    }
    let data = std::fs::read_to_string(path).context("could not read trader state file")?;
    if data.trim().is_empty() {
        return Ok(TraderState::default());
    }
    serde_json::from_str(&data).context("could not parse trader state file")
}

pub fn save(path: &Path, state: &TraderState) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create state directory")?;
    }
    let json = serde_json::to_string_pretty(state).context("state serialise failed")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("state write failed")?;
    std::fs::rename(&tmp, path).context("state rename failed")?;
    Ok(())
}

/// Persistent "the momentum trader has stopped itself" marker. While present,
/// every tick short-circuits silently until the operator deletes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HaltRecord {
    pub ts: i64,
    pub reason: String,
}

pub fn read_halt(path: &Path) -> Result<Option<HaltRecord>> {
    if !path.exists() {
        return Ok(None);
    }
    let data = std::fs::read_to_string(path).context("could not read halt file")?;
    if data.trim().is_empty() {
        return Ok(None);
    }
    let rec = serde_json::from_str(&data).context("could not parse halt file")?;
    Ok(Some(rec))
}

pub fn write_halt(path: &Path, rec: &HaltRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("could not create halt directory")?;
    }
    let json = serde_json::to_string_pretty(rec).context("halt serialise failed")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).context("halt write failed")?;
    std::fs::rename(&tmp, path).context("halt rename failed")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("momentum_{name}_{}.json", rand::random::<u32>()))
    }

    fn position(entry_ts: i64, peak: f64) -> Position {
        Position {
            mint: "MINT_A".into(),
            symbol: "AAA".into(),
            entry_ts,
            entry_price_usd: 100.0,
            token_amount: 5.0,
            usdc_spent: 50.0,
            peak_price_usd: peak,
            entry_sig: "dry-run".into(),
            dry_run: true,
        }
    }

    #[test]
    fn save_load_round_trip() {
        let path = tmp("state");
        let mut state = TraderState::default();
        state.position = Some(position(1_700_000_000, 120.0));
        state.last_exit_ts_per_mint.insert("MINT_B".into(), 42);
        save(&path, &state).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got.position.as_ref().unwrap().symbol, "AAA");
        assert!((got.position.unwrap().peak_price_usd - 120.0).abs() < 1e-9);
        assert_eq!(got.last_exit_ts_per_mint.get("MINT_B"), Some(&42));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn exit_attempts_round_trip_and_legacy_defaults_empty() {
        // A state file written before exit_attempts_per_mint existed must still
        // load — escalation just starts fresh (empty map), not error out.
        let legacy = r#"{"position":null,"last_exit_ts_per_mint":{},"trades":[]}"#;
        let parsed: TraderState = serde_json::from_str(legacy).expect("legacy state parses");
        assert!(parsed.exit_attempts_per_mint.is_empty());

        // And the field survives a save/load cycle so escalation persists restarts.
        let path = tmp("exit_attempts");
        let mut state = TraderState::default();
        state.exit_attempts_per_mint.insert("MINT_A".into(), 2);
        state.entry_attempt = Some(EntryAttempt {
            mint: "MINT_C".into(),
            count: 1,
        });
        save(&path, &state).unwrap();
        let got = load(&path).unwrap();
        assert_eq!(got.exit_attempts_per_mint.get("MINT_A"), Some(&2));
        let ea = got.entry_attempt.expect("entry_attempt round-trips");
        assert_eq!((ea.mint.as_str(), ea.count), ("MINT_C", 1));
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn timestamps_serialize_as_rfc3339_and_read_both_formats() {
        // New format: timestamps are written as RFC3339 strings, not integers.
        let mut state = TraderState::default();
        state.position = Some(position(1_700_000_000, 120.0)); // 2023-11-14T22:13:20Z
        state
            .last_exit_ts_per_mint
            .insert("MINT_B".into(), 1_700_000_000);
        let json = serde_json::to_string(&state).unwrap();
        assert!(
            json.contains("\"entry_ts\":\"2023-11-14T22:13:20Z\""),
            "entry_ts must be an RFC3339 string, got: {json}"
        );
        assert!(
            !json.contains(":1700000000"),
            "no bare epoch integers: {json}"
        );

        // Round-trips back to the same epoch seconds the rest of the code expects.
        let got: TraderState = serde_json::from_str(&json).unwrap();
        assert_eq!(got.position.unwrap().entry_ts, 1_700_000_000);
        assert_eq!(
            got.last_exit_ts_per_mint.get("MINT_B"),
            Some(&1_700_000_000)
        );

        // Legacy format: bare integers still parse (so live state files migrate
        // in place on the next save instead of failing to load).
        let legacy = r#"{
            "position":{"mint":"M","symbol":"S","entry_ts":1700000000,
              "entry_price_usd":1.0,"token_amount":1.0,"usdc_spent":1.0,
              "peak_price_usd":1.0,"entry_sig":"dry-run","dry_run":true},
            "last_exit_ts_per_mint":{"MINT_B":1700000000},"trades":[]
        }"#;
        let parsed: TraderState = serde_json::from_str(legacy).expect("legacy integers parse");
        assert_eq!(parsed.position.unwrap().entry_ts, 1_700_000_000);
        assert_eq!(
            parsed.last_exit_ts_per_mint.get("MINT_B"),
            Some(&1_700_000_000)
        );
    }

    #[test]
    fn missing_file_is_flat() {
        let path = tmp("missing");
        let got = load(&path).unwrap();
        assert!(got.position.is_none());
    }

    #[test]
    fn entries_last_24h_counts_open_and_recent_closed() {
        let now = 2_000_000_000;
        let mut state = TraderState::default();
        // ancient closed trade — outside window
        state.trades.push(TradeRecord {
            entry_ts: now - 100_000,
            exit_ts: now - 90_000,
            mint: "M".into(),
            symbol: "S".into(),
            entry_price_usd: 1.0,
            exit_price_usd: 1.1,
            peak_price_usd: 1.2,
            usdc_in: 50.0,
            usdc_out: 55.0,
            pnl_pct: 10.0,
            entry_sig: "a".into(),
            exit_sig: "b".into(),
            dry_run: true,
        });
        // recent closed trade — inside window
        let mut recent = state.trades[0].clone();
        recent.entry_ts = now - 3_600;
        state.trades.push(recent);
        // open position inside window
        state.position = Some(position(now - 60, 100.0));
        assert_eq!(entries_last_24h(&state, now), 2, "1 recent closed + 1 open");
    }

    #[test]
    fn repair_peak_heals_invalid_and_keeps_valid() {
        // invalid (zero) → max(entry, history)
        let mut p = position(1, 0.0);
        p.repair_peak(130.0);
        assert!((p.peak_price_usd - 130.0).abs() < 1e-9);
        // below entry → entry
        let mut p = position(1, 50.0);
        p.repair_peak(0.0);
        assert!(
            (p.peak_price_usd - 100.0).abs() < 1e-9,
            "peak floored at entry"
        );
        // valid and above history → unchanged
        let mut p = position(1, 140.0);
        p.repair_peak(110.0);
        assert!((p.peak_price_usd - 140.0).abs() < 1e-9);
    }

    #[test]
    fn halt_round_trip() {
        let path = tmp("halt");
        assert!(read_halt(&path).unwrap().is_none());
        write_halt(
            &path,
            &HaltRecord {
                ts: 1,
                reason: "test".into(),
            },
        )
        .unwrap();
        assert_eq!(read_halt(&path).unwrap().unwrap().reason, "test");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn summarize_realized_pnl() {
        let trade = |usdc_in: f64, usdc_out: f64| TradeRecord {
            entry_ts: 0,
            exit_ts: 0,
            mint: "M".into(),
            symbol: "S".into(),
            entry_price_usd: 1.0,
            exit_price_usd: 1.0,
            peak_price_usd: 1.0,
            usdc_in,
            usdc_out,
            pnl_pct: (usdc_out - usdc_in) / usdc_in * 100.0,
            entry_sig: "".into(),
            exit_sig: "".into(),
            dry_run: true,
        };
        assert_eq!(summarize(&[]).closed_trades, 0);
        let s = summarize(&[trade(10.0, 11.0), trade(10.0, 9.5)]); // +1.0, −0.5 → +0.5 net
        assert_eq!(s.closed_trades, 2);
        assert_eq!((s.wins, s.losses), (1, 1));
        assert!((s.realized_usdc - 0.5).abs() < 1e-9);
        assert!(
            (s.realized_pct - 2.5).abs() < 1e-9,
            "0.5 / 20 deployed = 2.5%"
        );
        assert!((s.win_rate_pct - 50.0).abs() < 1e-9);
        assert!((s.best_trade_pct - 10.0).abs() < 1e-9);
        assert!((s.worst_trade_pct - -5.0).abs() < 1e-9);
    }
}
