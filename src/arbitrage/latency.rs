//! Detect-to-submit latency instrumentation.
//!
//! Answers one operational question: are Jito bundle races lost on latency
//! (milliseconds) or on tip sizing? Stage stamps ride a [`LatencyTimeline`]
//! from the BF loop through the submission task; the bundle-outcome monitor
//! joins each submission's timing to its resolved outcome in [`LatencyStats`],
//! which prints an outcome-bucketed percentile table at most every 10 min.
//!
//! Measurement only — nothing here influences routing, tips, or cooldowns.
//! Spec: docs/superpowers/specs/2026-07-05-latency-instrumentation-design.md

/// Monotonic ns since process epoch — re-exported from `dex::types` (the clock
/// lives there so `portfolio_watcher`'s `#[path]` include of `src/dex/` never
/// needs the `arbitrage` module).
pub use crate::dex::types::monotonic_now_ns as now_ns;

/// Pin the process epoch at startup so `Pool.last_update_ns == 0` is
/// unambiguously "never stamped".
pub fn init() {
    let _ = now_ns();
}

// ─── Per-submission timeline ────────────────────────────────────────────────

/// Stage stamps (ns since process epoch) for one submission attempt.
/// All fields optional: a missing stamp renders as `?` and is skipped in
/// aggregates — instrumentation must never panic or abort a submission.
#[derive(Debug, Default, Clone, Copy)]
pub struct LatencyTimeline {
    /// Most recent `Pool::last_update_ns` among the cycle's pools — the market
    /// event that (likely) created the edge.
    pub freshest_pool_update_ns: Option<u64>,
    /// Oldest such stamp — bounds quote validity.
    pub oldest_pool_update_ns: Option<u64>,
    pub bf_start: Option<u64>,
    pub bf_done: Option<u64>,
    pub eval_done: Option<u64>,
    pub spawned: Option<u64>,
    pub sem_acquired: Option<u64>,
    pub jup_resolved: Option<u64>,
    pub built: Option<u64>,
    pub submit_started: Option<u64>,
    pub accepted: Option<u64>,
    pub region: Option<&'static str>,
}

fn ms_between(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    Some(b?.saturating_sub(a?) / 1_000_000)
}

fn fmt_ms(v: Option<u64>) -> String {
    match v {
        Some(ms) => format!("{ms}ms"),
        None => "?".to_string(),
    }
}

fn fmt_ratio(tip_lamports: u64, tip_floor: u64) -> String {
    if tip_floor == 0 {
        return "?".to_string();
    }
    format!("{:.1}x", tip_lamports as f64 / tip_floor as f64)
}

impl LatencyTimeline {
    /// Record the freshest/oldest pool-update stamps of the chosen cycle.
    /// Zero stamps ("never updated") are ignored.
    pub fn set_pool_stamps(&mut self, stamps: &[u64]) {
        let nonzero = stamps.iter().copied().filter(|&s| s != 0);
        self.freshest_pool_update_ns = nonzero.clone().max();
        self.oldest_pool_update_ns = nonzero.min();
    }

    pub fn staleness_ms(&self) -> Option<u64> { ms_between(self.freshest_pool_update_ns, self.submit_started) }
    pub fn oldest_ms(&self)    -> Option<u64> { ms_between(self.oldest_pool_update_ns, self.submit_started) }
    pub fn total_ms(&self)     -> Option<u64> { ms_between(self.freshest_pool_update_ns, self.accepted) }
    pub fn bf_ms(&self)        -> Option<u64> { ms_between(self.bf_start, self.bf_done) }
    pub fn eval_ms(&self)      -> Option<u64> { ms_between(self.bf_done, self.eval_done) }
    pub fn sem_ms(&self)       -> Option<u64> { ms_between(self.spawned, self.sem_acquired) }
    pub fn jup_ms(&self)       -> Option<u64> { ms_between(self.sem_acquired, self.jup_resolved) }
    pub fn build_ms(&self)     -> Option<u64> { ms_between(self.jup_resolved, self.built) }

    /// One-line breakdown logged right after the first Block Engine accept.
    /// `accept_ms` comes from the `SubmitReceipt` (measured inside
    /// `submit_bundle`, entry → first region accept).
    pub fn summary_line(&self, accept_ms: u32, tip_lamports: u64, tip_floor: u64) -> String {
        format!(
            "SUBMIT latency staleness={} oldest={} bf={} eval={} sem={} jup={} build={} \
             jito_accept={}ms ({}) total={} tip={}L ratio={}",
            fmt_ms(self.staleness_ms()),
            fmt_ms(self.oldest_ms()),
            fmt_ms(self.bf_ms()),
            fmt_ms(self.eval_ms()),
            fmt_ms(self.sem_ms()),
            fmt_ms(self.jup_ms()),
            fmt_ms(self.build_ms()),
            accept_ms,
            self.region.unwrap_or("?"),
            fmt_ms(self.total_ms()),
            tip_lamports,
            fmt_ratio(tip_lamports, tip_floor),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stamps chosen so every derived duration is a distinct round number:
    /// staleness 48ms, oldest 48ms, bf 2ms, eval 9ms, sem 0ms, jup 0ms,
    /// build 1ms, total 91ms.
    fn full_timeline() -> LatencyTimeline {
        LatencyTimeline {
            freshest_pool_update_ns: Some(1_000_000),
            oldest_pool_update_ns: Some(500_000),
            bf_start: Some(10_000_000),
            bf_done: Some(12_000_000),
            eval_done: Some(21_000_000),
            spawned: Some(22_000_000),
            sem_acquired: Some(22_000_000),
            jup_resolved: Some(22_000_000),
            built: Some(23_000_000),
            submit_started: Some(49_000_000),
            accepted: Some(92_000_000),
            region: Some("ams"),
        }
    }

    #[test]
    fn derived_durations() {
        let t = full_timeline();
        assert_eq!(t.staleness_ms(), Some(48));
        assert_eq!(t.total_ms(), Some(91));
        assert_eq!(t.bf_ms(), Some(2));
        assert_eq!(t.eval_ms(), Some(9));
        assert_eq!(t.sem_ms(), Some(0));
        assert_eq!(t.build_ms(), Some(1));
    }

    #[test]
    fn missing_stamps_yield_none_and_question_marks() {
        let t = LatencyTimeline::default();
        assert_eq!(t.staleness_ms(), None);
        assert_eq!(t.total_ms(), None);
        assert_eq!(
            t.summary_line(5, 100, 0),
            "SUBMIT latency staleness=? oldest=? bf=? eval=? sem=? jup=? build=? \
             jito_accept=5ms (?) total=? tip=100L ratio=?"
        );
    }

    #[test]
    fn out_of_order_stamps_saturate_to_zero() {
        let mut t = LatencyTimeline::default();
        t.freshest_pool_update_ns = Some(50_000_000);
        t.submit_started = Some(10_000_000); // "before" the update — clock misuse
        assert_eq!(t.staleness_ms(), Some(0));
    }

    #[test]
    fn summary_line_full() {
        let t = full_timeline();
        assert_eq!(
            t.summary_line(31, 48_000, 6_000),
            "SUBMIT latency staleness=48ms oldest=48ms bf=2ms eval=9ms sem=0ms jup=0ms build=1ms \
             jito_accept=31ms (ams) total=91ms tip=48000L ratio=8.0x"
        );
    }

    #[test]
    fn pool_stamps_ignore_zero_and_pick_extremes() {
        let mut t = LatencyTimeline::default();
        t.set_pool_stamps(&[0, 0]);
        assert_eq!(t.freshest_pool_update_ns, None);
        assert_eq!(t.oldest_pool_update_ns, None);
        t.set_pool_stamps(&[5_000_000, 0, 3_000_000]);
        assert_eq!(t.freshest_pool_update_ns, Some(5_000_000));
        assert_eq!(t.oldest_pool_update_ns, Some(3_000_000));
    }

    #[test]
    fn clock_reexport_works() {
        let a = now_ns();
        assert!(a >= 1);
    }
}
