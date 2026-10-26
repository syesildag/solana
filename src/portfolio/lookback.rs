//! The window a momentum signal is computed over — by row count (today's semantics) or by
//! wall-clock minutes (`lookback_min` / `entry_max_z_min`).
//!
//! Rows are not time: the backtest book holds a GeckoTerminal candle only on minutes that
//! traded, while the live trader writes a row every ~60 s tick and none while the host sleeps.
//! So `lookback_obs: 480` spanned ~52.8 h of HYPE in the book but ~8 h live, and the live score
//! barely correlated with the backtested one (Spearman 0.31; 0.94+ once both use a time window).
//! One implementation, shared by the live trader and `momentum-sim`, so the two cannot drift.
//! See docs/superpowers/specs/2026-10-26-time-based-lookback-design.md.

use crate::portfolio::momentum_universe::{TokenParams, WatchedToken};

/// A minutes window is valid only if its oldest point is at or before `now − COVER·L`:
/// a 52-h window computed on 2 h of prices (a fresh add, a restart without history, an outage
/// longer than L) is a fake signal and must read as warming. Only the oldest point is checked, so
/// a hole anywhere inside a covered window — a sleep shorter than L included — is accepted.
pub const COVER: f64 = 0.75;

/// Point floor for a z-score (the live `entry_dip_z` floor; the sim's `zscore_last` uses 30 too).
pub const Z_MIN_POINTS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookback {
    /// The last N points of the token's own series (today's semantics).
    Obs(usize),
    /// The points within the last N minutes of wall-clock time, subject to [`COVER`].
    Minutes(u32),
}

impl Lookback {
    /// `Obs(0)` / `Minutes(0)`: no window (for the z-gate, the gate is disabled).
    pub fn is_off(&self) -> bool {
        matches!(self, Lookback::Obs(0) | Lookback::Minutes(0))
    }
}

/// Select the window from an oldest-first `(ts, price)` series at time `now`.
///
/// `Obs(n)` is exactly the old `rank_candidates` slice: the last `n` points, or the whole
/// series when shorter (never `None` unless `n == 0`). `Minutes(m)` is the points with
/// `ts > now − m·60`, or `None` when that set is empty or not covered (see [`COVER`]).
/// Point floors are NOT applied here — each consumer keeps its own.
pub fn window(series: &[(u64, f64)], now: u64, lb: Lookback) -> Option<&[(u64, f64)]> {
    match lb {
        Lookback::Obs(0) | Lookback::Minutes(0) => None,
        Lookback::Obs(n) => Some(&series[series.len().saturating_sub(n)..]),
        Lookback::Minutes(m) => {
            // Signed arithmetic: `now` can be smaller than the window in tests, and
            // saturating to 0 would wrongly drop a point stamped exactly 0.
            let span = i64::from(m) * 60;
            let cutoff = now as i64 - span;
            let start = series.partition_point(|&(ts, _)| ts as i64 <= cutoff);
            let w = &series[start..];
            let oldest = w.first()?.0 as i64;
            let cover_by = now as i64 - (COVER * span as f64) as i64;
            (oldest <= cover_by).then_some(w)
        }
    }
}

/// Ranking-metric lookback for a token: `lookback_min > 0` wins, else `lookback_obs`, else the
/// global row count.
pub fn resolve_metric(p: Option<&TokenParams>, global_obs: usize) -> Lookback {
    match p.and_then(|p| p.lookback_min).filter(|&m| m > 0) {
        Some(m) => Lookback::Minutes(m),
        None => Lookback::Obs(p.and_then(|p| p.lookback_obs).unwrap_or(global_obs)),
    }
}

/// Overbought z-gate lookback for a token: `entry_max_z_min > 0` wins (and enables the gate even
/// when `entry_max_z_obs` is 0), else `entry_max_z_obs`, else the global row count. `Obs(0)` = off.
pub fn resolve_z(p: Option<&TokenParams>, global_obs: usize) -> Lookback {
    match p.and_then(|p| p.entry_max_z_min).filter(|&m| m > 0) {
        Some(m) => Lookback::Minutes(m),
        None => Lookback::Obs(p.and_then(|p| p.entry_max_z_obs).unwrap_or(global_obs)),
    }
}

/// Longest minutes window any watched token uses (`lookback_min` or `entry_max_z_min`); 0 when
/// no token opts in. Sizes the sim's trailing deque and the live cold-backfill window.
pub fn max_minutes(watched: &[WatchedToken]) -> u32 {
    watched
        .iter()
        .filter_map(|w| w.params.as_ref())
        .flat_map(|p| [p.lookback_min, p.entry_max_z_min])
        .flatten()
        .max()
        .unwrap_or(0)
}

/// Population z-score of the last price against the window mean. `None` below
/// [`Z_MIN_POINTS`] or on a flat window. Same arithmetic as `momentum::entry_dip_z`.
pub fn zscore(points: &[(u64, f64)]) -> Option<f64> {
    if points.len() < Z_MIN_POINTS {
        return None;
    }
    let n = points.len() as f64;
    let m = points.iter().map(|&(_, p)| p).sum::<f64>() / n;
    let sd = (points.iter().map(|&(_, p)| (p - m).powi(2)).sum::<f64>() / n).sqrt();
    if sd < 1e-12 {
        return None;
    }
    Some((points.last().unwrap().1 - m) / sd)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One point per 60 s starting at `t0`, price `100 + i` (strictly rising).
    fn per_min(n: u64, t0: u64) -> Vec<(u64, f64)> {
        (0..n).map(|i| (t0 + i * 60, 100.0 + i as f64)).collect()
    }

    #[test]
    fn obs_window_is_todays_tail() {
        let s = per_min(500, 0);
        assert_eq!(window(&s, s.last().unwrap().0, Lookback::Obs(480)).unwrap().len(), 480);
        assert_eq!(window(&s[..100], 5940, Lookback::Obs(480)).unwrap().len(), 100); // shorter ⇒ whole series
    }

    #[test]
    fn minutes_equals_obs_at_one_row_per_minute() {
        let s = per_min(1000, 0);
        let now = s.last().unwrap().0;
        assert_eq!(window(&s, now, Lookback::Minutes(480)).unwrap(), window(&s, now, Lookback::Obs(480)).unwrap());
    }

    #[test]
    fn uncovered_window_is_warming() {
        let s = per_min(120, 0); // 2 h of data
        assert!(window(&s, s.last().unwrap().0, Lookback::Minutes(2880)).is_none());
    }

    #[test]
    fn hole_inside_covered_window_still_scores() {
        let mut s = per_min(600, 0); // 10 h
        s.extend(per_min(600, 600 * 60 + 15 * 3600)); // 15 h sleep hole, then 10 h (35 h in all)
        let w = window(&s, s.last().unwrap().0, Lookback::Minutes(40 * 60));
        assert!(w.is_some() && w.unwrap().len() == 1200);
    }

    #[test]
    fn just_woke_after_long_sleep_is_warming() {
        let mut s = per_min(600, 0);
        s.extend(per_min(120, 600 * 60 + 15 * 3600)); // only 2 h since wake
        assert!(window(&s, s.last().unwrap().0, Lookback::Minutes(8 * 60)).is_none());
    }

    #[test]
    fn pre_sleep_prices_still_cover_a_long_window_after_a_shorter_sleep() {
        // Characterization of the binding rule (only the OLDEST point is checked): 12.8 h of prices,
        // a 40-h sleep, one fresh tick — a 52.8-h window still reaches back far enough, so it scores
        // across the gap (as today's row window does). Warming needs the data to stop reaching back
        // to now − COVER·L (a fresh add, or an outage longer than L). Pinned so the docs can't drift.
        let mut s = per_min(770, 0);
        s.extend(per_min(1, 770 * 60 + 40 * 3600));
        let w = window(&s, s.last().unwrap().0, Lookback::Minutes(3170));
        assert_eq!(w.map(|w| w.len()), Some(770));
    }

    #[test]
    fn zero_lookback_is_none() {
        let s = per_min(500, 0);
        let now = s.last().unwrap().0;
        assert!(window(&s, now, Lookback::Obs(0)).is_none());
        assert!(window(&s, now, Lookback::Minutes(0)).is_none());
    }

    #[test]
    fn resolve_z_minutes_enables_gate_even_when_obs_zero() {
        let p = TokenParams { entry_max_z_obs: Some(0), entry_max_z_min: Some(2880), ..Default::default() };
        assert_eq!(resolve_z(Some(&p), 480), Lookback::Minutes(2880));
        let off = TokenParams { entry_max_z_obs: Some(0), ..Default::default() };
        assert!(resolve_z(Some(&off), 480).is_off());
        assert_eq!(resolve_z(None, 480), Lookback::Obs(480));
    }

    #[test]
    fn resolve_metric_prefers_minutes() {
        let p = TokenParams { lookback_obs: Some(480), lookback_min: Some(3170), ..Default::default() };
        assert_eq!(resolve_metric(Some(&p), 121), Lookback::Minutes(3170));
        assert_eq!(resolve_metric(None, 121), Lookback::Obs(121));
    }

    #[test]
    fn zscore_matches_entry_dip_z_floor() {
        assert!(zscore(&per_min(29, 0)).is_none());
        assert!(zscore(&per_min(40, 0)).unwrap() > 0.0);
    }
}
