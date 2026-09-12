//! External-state series for the momentum simulator (2026-09-12 research track).
//!
//! `scripts/fetch_external_series.js` writes `assets/external_series.jsonl` — one row per point,
//! `{"ts","key","value"}`, already stamped at the moment each value was KNOWABLE (candle close,
//! publication lag, funding settlement). This module turns those native-cadence series into
//! per-snapshot boolean regime masks the replay can consume, strictly as-of (`state.ts ≤ snap.ts`)
//! so no test can read the future, plus the block-shuffled placebo masks every finding must beat.
//!
//! Sim-only: nothing here is wired into the live trader.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::portfolio::history::PriceSnapshot;
use crate::portfolio::momentum::MacroEvent;

/// One external series at its native cadence: `(unix_secs_known_at, value)`, oldest-first.
pub type Series = Vec<(i64, f64)>;
/// One regime state series: `(unix_secs, on)`, oldest-first; the state holds until the next entry.
pub type States = Vec<(i64, bool)>;

#[derive(Deserialize)]
struct Row {
    ts: i64,
    key: String,
    value: f64,
}

/// Parse the JSONL body of `assets/external_series.jsonl`. Unparseable lines are skipped;
/// a duplicate `(ts, key)` keeps the LAST value (the fetcher's merge semantics).
pub fn parse_external_lines(text: &str) -> HashMap<String, Series> {
    let mut by_key: HashMap<String, BTreeMap<i64, f64>> = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(r) = serde_json::from_str::<Row>(line) else { continue };
        if !r.value.is_finite() {
            continue;
        }
        by_key.entry(r.key).or_default().insert(r.ts, r.value);
    }
    by_key.into_iter().map(|(k, m)| (k, m.into_iter().collect())).collect()
}

pub fn load_external(path: &Path) -> Result<HashMap<String, Series>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_external_lines(&text))
}

/// How a native window is turned into a regime bit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExtMode {
    /// Last value vs the window mean (the level gate; `BelowPct` also lives here).
    Level,
    /// Sign of the least-squares slope × R² over the window (the trend gate).
    Trend,
}

/// Which side of the statistic counts as regime ON. Fixed per series BEFORE any result is
/// read (2026-09-12 pre-registration): risk assets `Up`, rates/dollar `Down`, funding
/// `BelowPct(75)` = "not crowded".
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExtDir {
    Up,
    Down,
    /// ON when the last value is at or below the p-th percentile of the trailing window
    /// (nearest-rank). Level-only; with `Trend` it is evaluated as Level.
    BelowPct(f64),
    /// ON when the last value is at or above `f × mean(window)` — the live flow gate's
    /// volume-collapse veto (`MOMENTUM_MIN_VOL_DECAY`: vol_h1 ≥ f × vol_h24/24). Level-only.
    AboveFrac(f64),
}

/// Least-squares slope of `value` on elapsed seconds, scaled by R² so a noisy window scores
/// near zero. `None` below 3 points or on a degenerate time axis. Unlike the ranking helper
/// (`suggestions::compute_slope_r2`, 120-obs floor, ln-price) this works on daily/8-hourly
/// series with a handful of points and on series that can be ≤ 0 (funding, yields).
pub fn slope_stat(window: &[(i64, f64)]) -> Option<f64> {
    if window.len() < 3 {
        return None;
    }
    let t0 = window[0].0;
    let n = window.len() as f64;
    let xs: Vec<f64> = window.iter().map(|&(t, _)| (t - t0) as f64).collect();
    let ys: Vec<f64> = window.iter().map(|&(_, v)| v).collect();
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0.0_f64, 0.0_f64, 0.0_f64);
    for i in 0..xs.len() {
        let dx = xs[i] - mx;
        let dy = ys[i] - my;
        sxx += dx * dx;
        sxy += dx * dy;
        syy += dy * dy;
    }
    if sxx <= 1e-12 {
        return None;
    }
    let slope = sxy / sxx;
    let r2 = if syy <= 1e-18 { 0.0 } else { (sxy * sxy / (sxx * syy)).clamp(0.0, 1.0) };
    Some(slope * r2)
}

fn below_pct(window: &[(i64, f64)], last: f64, pct: f64) -> bool {
    let mut vals: Vec<f64> = window.iter().map(|&(_, v)| v).collect();
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    let rank = ((pct / 100.0) * n as f64).ceil() as usize; // nearest-rank
    let idx = rank.clamp(1, n) - 1;
    last <= vals[idx]
}

/// Regime states at the series' OWN cadence: one bit per point from the first full window on,
/// each computed from that point and the `window − 1` before it — never from later points.
pub fn native_states(points: &[(i64, f64)], window: usize, mode: ExtMode, dir: ExtDir) -> States {
    let w = window.max(match mode {
        ExtMode::Trend => 3,
        ExtMode::Level => 2,
    });
    let mut out = Vec::new();
    if points.len() < w {
        return out;
    }
    for end in w..=points.len() {
        let win = &points[end - w..end];
        let (ts, last) = win[win.len() - 1];
        let on = match (mode, dir) {
            (_, ExtDir::BelowPct(p)) => below_pct(win, last, p),
            (_, ExtDir::AboveFrac(f)) => last >= f * win.iter().map(|&(_, v)| v).sum::<f64>() / win.len() as f64,
            (ExtMode::Trend, ExtDir::Up) => slope_stat(win).is_some_and(|s| s >= 0.0),
            (ExtMode::Trend, ExtDir::Down) => slope_stat(win).is_some_and(|s| s <= 0.0),
            (ExtMode::Level, ExtDir::Up) => last > win.iter().map(|&(_, v)| v).sum::<f64>() / win.len() as f64,
            (ExtMode::Level, ExtDir::Down) => last < win.iter().map(|&(_, v)| v).sum::<f64>() / win.len() as f64,
        };
        out.push((ts, on));
    }
    out
}

/// The state in force at `ts`: the latest entry stamped at or BEFORE `ts`. `None` before the
/// first entry. This is the only join in the module, and it is what keeps the research honest:
/// a value stamped at T is invisible at T − 1.
pub fn state_at(states: &[(i64, bool)], ts: i64) -> Option<bool> {
    let idx = states.partition_point(|&(t, _)| t <= ts);
    idx.checked_sub(1).map(|i| states[i].1)
}

/// Per-snapshot mask from a state series, as-of. Before the first state the mask is `true`
/// — the same warm-up semantics as the SOL masks (`sim::regime_mask*`: regime persists, on).
pub fn mask_from_states(snapshots: &[PriceSnapshot], states: &[(i64, bool)]) -> Vec<bool> {
    snapshots.iter().map(|s| state_at(states, s.ts as i64).unwrap_or(true)).collect()
}

/// Scheduled-event guard as a state series over `[from, to]`: OFF from `k_hours` before each
/// event to `k_hours` after, ON elsewhere; overlapping windows merge. Events are known in
/// advance, so this series has no look-ahead by construction.
pub fn event_states(events: &[MacroEvent], k_hours: f64, from: i64, to: i64) -> States {
    let k = (k_hours.max(0.0) * 3600.0) as i64;
    let mut out: States = vec![(from, true)];
    if k == 0 {
        return out;
    }
    let mut evs: Vec<i64> = events.iter().map(|e| e.ts).filter(|&t| t + k >= from && t - k <= to).collect();
    evs.sort_unstable();
    let mut off_until: Option<i64> = None;
    for t in evs {
        let (start, end) = (t - k, t + k);
        match off_until {
            Some(u) if start <= u => off_until = Some(u.max(end)), // overlapping: extend
            Some(u) => {
                out.push((u, true));
                out.push((start, false));
                off_until = Some(end);
            }
            None => {
                out.push((start, false));
                off_until = Some(end);
            }
        }
    }
    if let Some(u) = off_until {
        out.push((u, true));
    }
    out.retain(|&(t, _)| t >= from - k);
    out
}

/// Summary of one P&L bucket for the trade-conditional table.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct BucketStats {
    pub n: usize,
    pub wins: usize,
    pub sum: f64,
    pub mean: f64,
    pub win_pct: f64,
}

pub fn bucket(pnls: &[f64]) -> BucketStats {
    let n = pnls.len();
    if n == 0 {
        return BucketStats::default();
    }
    let sum: f64 = pnls.iter().sum();
    let wins = pnls.iter().filter(|&&p| p >= 0.0).count();
    BucketStats { n, wins, sum, mean: sum / n as f64, win_pct: 100.0 * wins as f64 / n as f64 }
}

/// Share of `true` in a mask.
pub fn on_share(mask: &[bool]) -> f64 {
    if mask.is_empty() {
        return 0.0;
    }
    mask.iter().filter(|&&b| b).count() as f64 / mask.len() as f64
}

/// Number of ON↔OFF transitions — the count of independent regime states a reader should
/// weigh a result by.
pub fn switches(mask: &[bool]) -> usize {
    mask.windows(2).filter(|w| w[0] != w[1]).count()
}

/// Tiny deterministic PRNG (xorshift64*) so placebo masks are reproducible from `seed`.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xD1B5_4A32_D192_ED03 | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next() % n as u64) as usize }
    }
}

/// `n` placebo masks with the SAME ON share and run-length structure as `mask`: the ON-run
/// lengths are permuted among the ON runs, the OFF-run lengths among the OFF runs, and the whole
/// mask is rotated by a random offset. A real regime must beat these on held-out P&L — a mask
/// that is ON 60% of the time "wins" 60% of a drift sample regardless of what it encodes.
pub fn placebo_masks(mask: &[bool], seed: u64, n: usize) -> Vec<Vec<bool>> {
    if mask.is_empty() || n == 0 {
        return Vec::new();
    }
    // Run-length encode.
    let mut runs: Vec<(bool, usize)> = Vec::new();
    for &b in mask {
        match runs.last_mut() {
            Some((lb, len)) if *lb == b => *len += 1,
            _ => runs.push((b, 1)),
        }
    }
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|_| {
            let mut on_lens: Vec<usize> = runs.iter().filter(|r| r.0).map(|r| r.1).collect();
            let mut off_lens: Vec<usize> = runs.iter().filter(|r| !r.0).map(|r| r.1).collect();
            for v in [&mut on_lens, &mut off_lens] {
                for i in (1..v.len()).rev() {
                    let j = rng.below(i + 1);
                    v.swap(i, j);
                }
            }
            let (mut oi, mut fi) = (0, 0);
            let mut out = Vec::with_capacity(mask.len());
            for &(b, _) in &runs {
                let len = if b { let l = on_lens[oi]; oi += 1; l } else { let l = off_lens[fi]; fi += 1; l };
                out.extend(std::iter::repeat_n(b, len));
            }
            let rot = rng.below(out.len());
            out.rotate_left(rot);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::history::PriceSnapshot;
    use crate::portfolio::momentum::MacroEvent;
    use std::collections::HashMap;

    fn snaps(tss: &[i64]) -> Vec<PriceSnapshot> {
        tss.iter().map(|&t| PriceSnapshot { ts: t as u64, prices: HashMap::new() }).collect()
    }

    #[test]
    fn parse_external_lines_groups_sorts_and_dedups() {
        let text = r#"{"ts":300,"key":"BTC","value":3.0}
{"ts":100,"key":"BTC","value":1.0}
{"ts":100,"key":"DGS10","value":4.1}
not json
{"ts":200,"key":"BTC","value":2.0}
{"ts":200,"key":"BTC","value":2.5}
"#;
        let m = parse_external_lines(text);
        assert_eq!(m["BTC"], vec![(100, 1.0), (200, 2.5), (300, 3.0)], "sorted, last write wins on a duplicate ts");
        assert_eq!(m["DGS10"], vec![(100, 4.1)]);
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn slope_stat_needs_three_points_and_carries_the_sign() {
        assert!(slope_stat(&[(0, 1.0), (60, 2.0)]).is_none(), "2 points: undefined");
        let up = slope_stat(&[(0, 1.0), (60, 2.0), (120, 3.0), (180, 4.0)]).unwrap();
        let down = slope_stat(&[(0, 4.0), (60, 3.0), (120, 2.0), (180, 1.0)]).unwrap();
        assert!(up > 0.0 && down < 0.0);
        let flat = slope_stat(&[(0, 2.0), (60, 2.0), (120, 2.0)]).unwrap();
        assert!(flat.abs() < 1e-12, "flat series → 0, not None");
    }

    #[test]
    fn native_states_trend_and_level_follow_the_series_direction() {
        // Daily cadence: 5 rising points then 5 falling.
        let mut pts = Vec::new();
        for i in 0..5 { pts.push((i as i64 * 86_400, 100.0 + i as f64)); }
        for i in 5..10 { pts.push((i as i64 * 86_400, 104.0 - (i as f64 - 4.0))); }
        let tr = native_states(&pts, 3, ExtMode::Trend, ExtDir::Up);
        // First state at the 3rd point; rising until the window is mostly falling.
        assert_eq!(tr.first().unwrap().0, 2 * 86_400);
        assert!(tr.iter().take(3).all(|&(_, on)| on), "rising windows are ON");
        assert!(!tr.last().unwrap().1, "falling window is OFF");
        let down = native_states(&pts, 3, ExtMode::Trend, ExtDir::Down);
        assert!(down.last().unwrap().1, "Down direction inverts the sign");
        let lv = native_states(&pts, 3, ExtMode::Level, ExtDir::Up);
        assert!(lv[0].1, "rising: last value above the window mean");
        assert!(!lv.last().unwrap().1, "falling: last value below the window mean");
    }

    #[test]
    fn native_states_above_frac_is_the_flow_gates_collapse_veto() {
        // The live flow gate vetoes on vol_h1 < 0.3 × (vol_h24 / 24): ON = NOT collapsed.
        let mk = |last: f64| -> Vec<(i64, f64)> {
            [10.0, 10.0, 10.0, 10.0, last].iter().enumerate().map(|(i, &v)| (i as i64 * 3600, v)).collect()
        };
        let off = native_states(&mk(2.0), 5, ExtMode::Level, ExtDir::AboveFrac(0.3)); // mean 8.4 → floor 2.52
        assert_eq!(off.len(), 1);
        assert!(!off[0].1, "2.0 < 0.3 × 8.4 → collapsed → OFF");
        let on = native_states(&mk(3.0), 5, ExtMode::Level, ExtDir::AboveFrac(0.3));
        assert!(on[0].1, "3.0 ≥ 2.52 → ON");
        let above = native_states(&mk(3.0), 5, ExtMode::Level, ExtDir::Up);
        assert!(!above[0].1, "the plain >MA gate would call the same bar OFF — the two are different gates");
    }

    #[test]
    fn native_states_below_percentile_flags_crowded_funding() {
        let pts: Vec<(i64, f64)> = [1.0, 2.0, 3.0, 4.0, 10.0, 1.5].iter().enumerate()
            .map(|(i, &v)| (i as i64 * 28_800, v * 1e-4)).collect();
        let st = native_states(&pts, 5, ExtMode::Level, ExtDir::BelowPct(75.0));
        assert_eq!(st.len(), 2);
        assert!(!st[0].1, "10 bp funding is above the trailing 75th pct → crowded → OFF");
        assert!(st[1].1, "1.5 bp is below → ON");
    }

    #[test]
    fn mask_from_states_is_as_of_and_never_looks_ahead() {
        let states = vec![(100, false), (200, true)];
        let s = snaps(&[50, 99, 100, 150, 200, 250]);
        assert_eq!(mask_from_states(&s, &states), vec![true, true, false, false, true, true]);
        assert_eq!(state_at(&states, 99), None, "a state stamped at 100 is invisible at 99");
        assert_eq!(state_at(&states, 100), Some(false));
        assert_eq!(state_at(&states, 199), Some(false));
        assert_eq!(state_at(&states, 200), Some(true));
    }

    #[test]
    fn event_states_are_off_inside_the_window_around_each_event() {
        let ev = vec![MacroEvent { name: "FOMC".into(), ts: 100_000 }];
        let st = event_states(&ev, 1.0, 0, 200_000);
        assert_eq!(state_at(&st, 50_000), Some(true));
        assert_eq!(state_at(&st, 100_000 - 3_600), Some(false), "OFF from k hours before");
        assert_eq!(state_at(&st, 100_000), Some(false));
        assert_eq!(state_at(&st, 100_000 + 3_600), Some(true), "back ON at +k hours");
        assert_eq!(state_at(&st, 150_000), Some(true));
    }

    #[test]
    fn bucket_stats_summarise_a_pnl_list() {
        let b = bucket(&[10.0, -4.0, 0.0, 6.0]);
        assert_eq!((b.n, b.wins), (4, 3));
        assert!((b.sum - 12.0).abs() < 1e-12 && (b.mean - 3.0).abs() < 1e-12 && (b.win_pct - 75.0).abs() < 1e-12);
        assert_eq!(bucket(&[]), BucketStats::default());
    }

    #[test]
    fn placebo_masks_keep_on_share_and_are_seeded() {
        let mut mask = Vec::new();
        for (len, on) in [(40, true), (10, false), (30, true), (25, false), (50, true), (5, false)] {
            mask.extend(std::iter::repeat_n(on, len));
        }
        let ons = mask.iter().filter(|&&b| b).count();
        let p = placebo_masks(&mask, 7, 5);
        assert_eq!(p.len(), 5);
        for m in &p {
            assert_eq!(m.len(), mask.len());
            assert_eq!(m.iter().filter(|&&b| b).count(), ons, "ON share preserved");
        }
        assert!(p.iter().any(|m| m != &mask), "at least one shuffle differs from the original");
        assert_eq!(placebo_masks(&mask, 7, 5), p, "seeded → reproducible");
        assert_ne!(placebo_masks(&mask, 8, 5), p, "a different seed shuffles differently");
        assert!((on_share(&mask) - ons as f64 / mask.len() as f64).abs() < 1e-12);
        assert_eq!(switches(&mask), 5);
    }
}
