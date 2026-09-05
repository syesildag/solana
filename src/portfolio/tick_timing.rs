//! Per-tick phase timing for the portfolio watcher's slow tick.
//!
//! The momentum trader's trailing stop is evaluated by the same single `select!` loop
//! that runs every network-bound slow-tick step (wallet re-scan, discovery scan, REST
//! pricing, adoption, …). Any of them stalling stalls the stop. `TickTimer` records how
//! long each named phase took so the audit log names the blocker instead of the
//! operator guessing from recorder gaps; the pure predicates decide when to warn / alert.

use std::time::{Duration, Instant};

/// Records named phase durations across one tick. Time is passed in explicitly
/// (`*_at`) so the arithmetic is unit-testable; the plain variants use `Instant::now()`.
#[derive(Debug, Clone)]
pub struct TickTimer {
    t0: Instant,
    last: Instant,
    steps: Vec<(String, u64)>,
}

impl TickTimer {
    pub fn start_at(now: Instant) -> Self {
        Self { t0: now, last: now, steps: Vec::new() }
    }

    pub fn start() -> Self {
        Self::start_at(Instant::now())
    }

    /// Close the phase that began at the previous lap (or at start) under `name`.
    pub fn lap_at(&mut self, name: &str, now: Instant) {
        let ms = now.saturating_duration_since(self.last).as_millis() as u64;
        self.steps.push((name.to_string(), ms));
        self.last = now;
    }

    pub fn lap(&mut self, name: &str) {
        self.lap_at(name, Instant::now());
    }

    /// `(total_ms since start, steps)`. Does not consume the timer so a caller can
    /// finish once on every early-exit path.
    pub fn finish_at(&self, now: Instant) -> (u64, Vec<(String, u64)>) {
        let total = now.saturating_duration_since(self.t0).as_millis() as u64;
        (total, self.steps.clone())
    }

    pub fn finish(&self) -> (u64, Vec<(String, u64)>) {
        self.finish_at(Instant::now())
    }
}

/// `true` when a tick ran strictly longer than `budget_ms`. `budget_ms == 0` disables.
pub fn over_budget(total_ms: u64, budget_ms: u64) -> bool {
    budget_ms > 0 && total_ms > budget_ms
}

/// `true` when the start-to-start gap exceeds `max_gap_secs` (0 = off) and no alert
/// was sent within `cooldown`.
pub fn gap_alert_due(
    gap_secs: u64,
    max_gap_secs: u64,
    last_alert: Option<Instant>,
    now: Instant,
    cooldown: Duration,
) -> bool {
    if max_gap_secs == 0 || gap_secs <= max_gap_secs {
        return false;
    }
    last_alert.is_none_or(|t| now.saturating_duration_since(t) >= cooldown)
}

/// The `n` slowest phases as `name=NNms`, slowest first — for the over-budget warning.
pub fn top_steps(steps: &[(String, u64)], n: usize) -> String {
    let mut sorted: Vec<&(String, u64)> = steps.iter().collect();
    sorted.sort_by_key(|s| std::cmp::Reverse(s.1));
    sorted
        .into_iter()
        .take(n)
        .map(|(name, ms)| format!("{name}={ms}ms"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whole seconds between the previous tick's start and this one's; `0` for the first tick.
pub fn gap_secs(prev_start: Option<Instant>, now: Instant) -> u64 {
    prev_start.map_or(0, |p| now.saturating_duration_since(p).as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn laps_record_named_durations_and_finish_sums_to_total() {
        let t0 = Instant::now();
        let mut t = TickTimer::start_at(t0);
        t.lap_at("wallet_scan", t0 + Duration::from_millis(100));
        t.lap_at("prices", t0 + Duration::from_millis(350));
        let (total_ms, steps) = t.finish_at(t0 + Duration::from_millis(400));
        assert_eq!(total_ms, 400);
        assert_eq!(
            steps,
            vec![("wallet_scan".to_string(), 100), ("prices".to_string(), 250)]
        );
    }

    #[test]
    fn zero_length_lap_records_zero() {
        let t0 = Instant::now();
        let mut t = TickTimer::start_at(t0);
        t.lap_at("noop", t0);
        let (_, steps) = t.finish_at(t0);
        assert_eq!(steps, vec![("noop".to_string(), 0)]);
    }

    #[test]
    fn over_budget_is_strict_and_zero_budget_never_fires() {
        assert!(!over_budget(5_000, 0));
        assert!(!over_budget(30_000, 30_000));
        assert!(over_budget(30_001, 30_000));
    }

    #[test]
    fn gap_alert_off_when_max_gap_is_zero() {
        let now = Instant::now();
        assert!(!gap_alert_due(10_000, 0, None, now, Duration::from_secs(600)));
    }

    #[test]
    fn gap_alert_fires_once_then_respects_cooldown() {
        let now = Instant::now();
        let cd = Duration::from_secs(600);
        assert!(gap_alert_due(301, 300, None, now, cd));
        assert!(!gap_alert_due(300, 300, None, now, cd));
        let just_alerted = Some(now - Duration::from_secs(10));
        assert!(!gap_alert_due(900, 300, just_alerted, now, cd));
        let long_ago = Some(now - Duration::from_secs(601));
        assert!(gap_alert_due(900, 300, long_ago, now, cd));
    }

    #[test]
    fn top_steps_lists_slowest_first_and_truncates() {
        let steps = vec![
            ("a".to_string(), 5u64),
            ("b".to_string(), 50u64),
            ("c".to_string(), 20u64),
        ];
        assert_eq!(top_steps(&steps, 2), "b=50ms, c=20ms");
        assert_eq!(top_steps(&[], 3), "");
    }

    #[test]
    fn gap_secs_is_zero_without_a_previous_tick_and_floors_otherwise() {
        let now = Instant::now();
        assert_eq!(gap_secs(None, now), 0);
        assert_eq!(gap_secs(Some(now - Duration::from_millis(61_900)), now), 61);
    }
}
