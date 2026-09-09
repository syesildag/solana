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

/// Whole seconds between the previous tick's start and this one's on the MONOTONIC clock;
/// `0` for the first tick. This measures *work time* — on macOS `Instant` is backed by
/// `CLOCK_UPTIME_RAW`, which does not advance while the system is asleep, so a suspended
/// host reads as a normal gap here. Pair it with [`wall_gap_secs`]; the difference is
/// [`dark_secs`].
pub fn gap_secs(prev_start: Option<Instant>, now: Instant) -> u64 {
    prev_start.map_or(0, |p| now.saturating_duration_since(p).as_secs())
}

/// Whole seconds between the previous tick's start and this one's on the WALL clock
/// (unix seconds); `0` for the first tick. This is the honest answer to "how long was the
/// trailing stop unevaluated", because it counts host suspend — which is what the
/// monotonic [`gap_secs`] structurally cannot see and what
/// `MOMENTUM_MAX_TICK_GAP_SECS` therefore has to be judged against.
///
/// A backwards step (NTP correction) clamps to `0` rather than wrapping.
pub fn wall_gap_secs(prev_start_unix: Option<i64>, now_unix: i64) -> u64 {
    prev_start_unix.map_or(0, |p| now_unix.saturating_sub(p).max(0) as u64)
}

/// Wall-clock seconds the process was not merely slow but *not running* — the host slept,
/// the process was SIGSTOPped, or the VM was paused.
///
/// This is the discriminator between the two incidents that both present as "the loop went
/// quiet", and they have opposite fixes:
///
/// * `dark_secs == 0` — both clocks advanced together: a phase blocked the loop. A code or
///   network problem; the `steps` breakdown names it.
/// * `dark_secs > 0`  — wall time ran while monotonic time did not: the host was suspended.
///   No phase is at fault and no `steps` entry will show anything; this is an ops problem.
///
/// Differences at or below [`CLOCK_SKEW_TOLERANCE_SECS`] are treated as clock noise so a
/// routine NTP slew doesn't stamp a spurious suspend on every record.
pub fn dark_secs(wall_gap_secs: u64, mono_gap_secs: u64) -> u64 {
    let excess = wall_gap_secs.saturating_sub(mono_gap_secs);
    if excess <= CLOCK_SKEW_TOLERANCE_SECS { 0 } else { excess }
}

/// Disagreement between `SystemTime` and `Instant` below this is NTP slew, not a suspend.
pub const CLOCK_SKEW_TOLERANCE_SECS: u64 = 2;

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

    #[test]
    fn wall_gap_is_zero_without_a_previous_tick() {
        assert_eq!(wall_gap_secs(None, 1_757_000_000), 0);
    }

    #[test]
    fn wall_gap_measures_calendar_seconds() {
        assert_eq!(wall_gap_secs(Some(1_757_000_000), 1_757_000_060), 60);
    }

    #[test]
    fn wall_gap_clamps_a_backwards_clock_step_to_zero() {
        // An NTP step backwards must not read as a negative (or huge unsigned) gap.
        assert_eq!(wall_gap_secs(Some(1_757_000_060), 1_757_000_000), 0);
    }

    #[test]
    fn dark_secs_is_the_wall_minus_monotonic_excess() {
        // Tonight's real incident: 1535s of wall-clock, 59s of monotonic.
        assert_eq!(dark_secs(1535, 59), 1476);
    }

    #[test]
    fn dark_secs_is_zero_when_the_loop_merely_blocked() {
        // A hung phase burns BOTH clocks equally — that is not darkness, it is a stall.
        assert_eq!(dark_secs(600, 600), 0);
    }

    #[test]
    fn dark_secs_absorbs_small_clock_skew() {
        // Sub-tolerance disagreement between SystemTime and Instant (NTP slew) is noise,
        // not a suspend — otherwise every record would carry a spurious dark_secs.
        assert_eq!(dark_secs(61, 60), 0);
        assert_eq!(dark_secs(62, 60), 0);
        assert_eq!(dark_secs(63, 60), 3);
    }

    #[test]
    fn dark_secs_never_underflows_when_monotonic_leads() {
        assert_eq!(dark_secs(59, 60), 0);
    }
}
