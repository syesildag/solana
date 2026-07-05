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
    /// Pool that owns the oldest stamp — names the stale leg in the SUBMIT line.
    pub oldest_pool: Option<solana_sdk::pubkey::Pubkey>,
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
    /// Record the freshest/oldest pool-update stamps of the chosen cycle,
    /// keeping the oldest pool's id so the SUBMIT line can name the stale leg.
    /// Zero stamps ("never updated") are ignored.
    pub fn set_pool_stamps(&mut self, stamps: &[(solana_sdk::pubkey::Pubkey, u64)]) {
        let nonzero = stamps.iter().filter(|&&(_, s)| s != 0);
        self.freshest_pool_update_ns = nonzero.clone().map(|&(_, s)| s).max();
        match nonzero.min_by_key(|&&(_, s)| s) {
            Some(&(id, s)) => {
                self.oldest_pool_update_ns = Some(s);
                self.oldest_pool = Some(id);
            }
            None => {
                self.oldest_pool_update_ns = None;
                self.oldest_pool = None;
            }
        }
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
        // Oldest cell names the stale leg when known: `3799ms(HTvjzsfX)`.
        let oldest_cell = match (self.oldest_ms(), self.oldest_pool) {
            (Some(ms), Some(id)) => format!("{}ms({})", ms, &id.to_string()[..8]),
            (v, _) => fmt_ms(v),
        };
        format!(
            "SUBMIT latency staleness={} oldest={} bf={} eval={} sem={} jup={} build={} \
             jito_accept={}ms ({}) total={} tip={}L ratio={}",
            fmt_ms(self.staleness_ms()),
            oldest_cell,
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

// ─── Outcome-joined records ─────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordOutcome {
    Landed,
    FailedOnChain,
    Dropped,
}

#[derive(Debug, Clone, Copy)]
pub struct LatencyRecord {
    pub staleness_ms: u32,
    pub total_ms: u32,
    pub accept_ms: u32,
    /// tip / tip_floor × 10, saturating. 0 = floor unknown at submit time.
    pub tip_ratio_x10: u32,
    pub outcome: RecordOutcome,
}

impl LatencyRecord {
    /// Build a record from a completed timeline. `None` when the timeline
    /// lacks the stamps needed for staleness/total — a record with fabricated
    /// numbers would blur the buckets (spec: "missing → skipped").
    pub fn from_timeline(
        t: &LatencyTimeline,
        accept_ms: u32,
        tip_lamports: u64,
        tip_floor: u64,
        outcome: RecordOutcome,
    ) -> Option<LatencyRecord> {
        let clamp = |v: u64| v.min(u32::MAX as u64) as u32;
        let tip_ratio_x10 = if tip_floor > 0 {
            clamp(tip_lamports.saturating_mul(10) / tip_floor)
        } else {
            0
        };
        Some(LatencyRecord {
            staleness_ms: clamp(t.staleness_ms()?),
            total_ms: clamp(t.total_ms()?),
            accept_ms,
            tip_ratio_x10,
            outcome,
        })
    }
}

// ─── Rolling stats + report ─────────────────────────────────────────────────

pub const RING_CAP: usize = 512;
/// Minimum spacing between LATENCY summary prints (first print fires on the
/// first record).
pub const REPORT_INTERVAL_NS: u64 = 600 * 1_000_000_000; // 10 min
/// Solana slot time — the absolute anchor for staleness readings.
pub const SLOT_MS: u32 = 400;

#[derive(Debug, Default, Clone, Copy)]
pub struct BucketSummary {
    pub n: usize,
    pub p50_stale_ms: u32,
    pub p95_stale_ms: u32,
    pub p50_total_ms: u32,
    pub p50_accept_ms: u32,
    pub p50_ratio_x10: u32,
}

pub struct Anchors {
    pub slot_ms: u32,
    pub tip_floor_lamports: u64,
}

struct StatsInner {
    ring: std::collections::VecDeque<LatencyRecord>,
    new_since_report: usize,
    last_report_ns: u64,
}

/// Outcome-joined rolling window. The `Mutex` is touched once per resolved
/// submission and once per report tick — never in the gRPC callback or the
/// BF hot loop.
pub struct LatencyStats {
    inner: std::sync::Mutex<StatsInner>,
}

impl LatencyStats {
    pub fn new() -> std::sync::Arc<LatencyStats> {
        std::sync::Arc::new(LatencyStats {
            inner: std::sync::Mutex::new(StatsInner {
                ring: std::collections::VecDeque::with_capacity(RING_CAP),
                new_since_report: 0,
                last_report_ns: 0,
            }),
        })
    }

    /// Push an outcome-resolved record (from the bundle-outcome monitor task).
    pub fn record(&self, r: LatencyRecord) {
        let Ok(mut inner) = self.inner.lock() else { return }; // poisoned → drop sample
        if inner.ring.len() == RING_CAP {
            inner.ring.pop_front();
        }
        inner.ring.push_back(r);
        inner.new_since_report += 1;
    }

    /// Render the percentile table when due: ≥1 new record since the last
    /// print AND ≥10 min since it (first print fires immediately).
    pub fn maybe_report(&self, tip_floor: u64) -> Option<String> {
        self.maybe_report_at(now_ns(), tip_floor)
    }

    fn maybe_report_at(&self, now: u64, tip_floor: u64) -> Option<String> {
        let records: Vec<LatencyRecord> = {
            let Ok(mut inner) = self.inner.lock() else { return None };
            if inner.new_since_report == 0 {
                return None;
            }
            if inner.last_report_ns != 0
                && now.saturating_sub(inner.last_report_ns) < REPORT_INTERVAL_NS
            {
                return None;
            }
            inner.last_report_ns = now;
            inner.new_since_report = 0;
            inner.ring.iter().copied().collect()
        };
        Some(render_report(
            &records,
            &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: tip_floor },
        ))
    }
}

/// Nearest-rank percentile on a sorted slice. Empty → 0.
fn percentile(sorted: &[u32], p: f64) -> u32 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p * sorted.len() as f64).ceil() as usize).max(1);
    sorted[rank.min(sorted.len()) - 1]
}

fn summarize(records: &[LatencyRecord], outcome: RecordOutcome) -> BucketSummary {
    let mut stale: Vec<u32> = Vec::new();
    let mut total: Vec<u32> = Vec::new();
    let mut accept: Vec<u32> = Vec::new();
    let mut ratio: Vec<u32> = Vec::new();
    for r in records.iter().filter(|r| r.outcome == outcome) {
        stale.push(r.staleness_ms);
        total.push(r.total_ms);
        accept.push(r.accept_ms);
        ratio.push(r.tip_ratio_x10);
    }
    stale.sort_unstable();
    total.sort_unstable();
    accept.sort_unstable();
    ratio.sort_unstable();
    BucketSummary {
        n: stale.len(),
        p50_stale_ms: percentile(&stale, 0.50),
        p95_stale_ms: percentile(&stale, 0.95),
        p50_total_ms: percentile(&total, 0.50),
        p50_accept_ms: percentile(&accept, 0.50),
        p50_ratio_x10: percentile(&ratio, 0.50),
    }
}

fn fmt_cell_ms(n: usize, v: u32) -> String {
    if n == 0 { "—".to_string() } else { format!("{v}ms") }
}

fn fmt_cell_ratio(n: usize, x10: u32) -> String {
    if n == 0 || x10 == 0 { "—".to_string() } else { format!("{:.1}x", x10 as f64 / 10.0) }
}

fn fmt_row(label: &str, b: &BucketSummary) -> String {
    format!(
        "{:<9}{:>10}{:>11}{:>11}{:>12}{:>11}{:>5}",
        label,
        fmt_cell_ms(b.n, b.p50_stale_ms),
        fmt_cell_ms(b.n, b.p95_stale_ms),
        fmt_cell_ms(b.n, b.p50_total_ms),
        fmt_cell_ms(b.n, b.p50_accept_ms),
        fmt_cell_ratio(b.n, b.p50_ratio_x10),
        b.n,
    )
}

fn render_report(records: &[LatencyRecord], anchors: &Anchors) -> String {
    let landed = summarize(records, RecordOutcome::Landed);
    let dropped = summarize(records, RecordOutcome::Dropped);
    let failed = summarize(records, RecordOutcome::FailedOnChain);
    let header = format!(
        "{:<9}{:>10}{:>11}{:>11}{:>12}{:>11}{:>5}",
        "", "p50_stale", "p95_stale", "p50_total", "p50_accept", "p50_ratio", "n"
    );
    let floor_str = if anchors.tip_floor_lamports > 0 {
        format!("{}L", anchors.tip_floor_lamports)
    } else {
        "n/a".to_string()
    };
    let mut out = format!(
        "LATENCY summary (n={}, ring≤{})\n{header}\n{}\n{}\n{}\nanchors: slot={}ms  tip_floor_ema={}",
        records.len(),
        RING_CAP,
        fmt_row("Landed", &landed),
        fmt_row("Dropped", &dropped),
        fmt_row("Failed", &failed),
        anchors.slot_ms,
        floor_str,
    );
    if let Some(v) = verdict(&landed, &dropped, &failed, anchors) {
        out.push_str(&format!("\n⇒ {v}"));
    }
    out
}

/// Operator-tuned one-line reading of the table, appended to the report.
///
/// `landed.n == 0` is the PRIMARY case (the bot has never won a contested
/// bundle): fall back to absolute readings — dropped p50 staleness vs
/// `anchors.slot_ms` (≥ ~1 slot → latency-dominated: the opportunity is
/// consumed before the auction is decided), dropped p50 tip ratio vs floor
/// (≈ 1.0× floor while fast → tip-dominated), and fast + ≫floor bids that
/// still drop (→ arb-specific competition or phantom edges via stale legs —
/// Jito's Dropped status conflates "outbid" with "failed Block Engine
/// simulation"). Return `None` when the data is too thin to call.
pub fn verdict(
    landed: &BucketSummary,
    dropped: &BucketSummary,
    failed: &BucketSummary,
    anchors: &Anchors,
) -> Option<String> {
    // Thresholds (2026-07-05): speak only with ≥10 dropped samples. Latency is
    // checked BEFORE tips — a stale bot bidding floor is still latency-bound,
    // and raising its tip just pays more to lose. The competition branch needs
    // drops to DOMINATE landings (≥10×) so a healthy bot's normal drop tail
    // stays silent. failed stays unused for now.
    let _ = failed;
    if dropped.n < 10 {
        return None;
    }
    if dropped.p50_stale_ms >= anchors.slot_ms {
        return Some(format!(
            "dropped p50 staleness {}ms ≥ {}ms slot — latency-bound: the opportunity is \
             consumed before the auction is decided; colocate/shorten the pipeline before \
             raising tips",
            dropped.p50_stale_ms, anchors.slot_ms
        ));
    }
    if dropped.p50_stale_ms < anchors.slot_ms / 2
        && dropped.p50_ratio_x10 > 0
        && dropped.p50_ratio_x10 <= 15
    {
        return Some(format!(
            "dropped bundles are fast (p50 staleness {}ms ≪ {}ms slot) but bid ~{:.1}× floor — \
             losing the tip auction, not the race",
            dropped.p50_stale_ms, anchors.slot_ms, dropped.p50_ratio_x10 as f64 / 10.0
        ));
    }
    if dropped.p50_stale_ms < anchors.slot_ms / 2
        && dropped.p50_ratio_x10 >= 100
        && dropped.n >= landed.n.saturating_mul(10).max(10)
    {
        return Some(format!(
            "dropped bundles are fast (p50 staleness {}ms) and bid ~{:.0}× floor yet still drop — \
             arb-specific competition or phantom edges: check oldest= on SUBMIT lines (a stale \
             leg fails Block Engine simulation and reads as a drop)",
            dropped.p50_stale_ms, dropped.p50_ratio_x10 as f64 / 10.0
        ));
    }
    None
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
            oldest_pool: None,
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
        use solana_sdk::pubkey::Pubkey;
        let (a, b, c) = (Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique());
        let mut t = LatencyTimeline::default();
        t.set_pool_stamps(&[(a, 0), (b, 0)]);
        assert_eq!(t.freshest_pool_update_ns, None);
        assert_eq!(t.oldest_pool_update_ns, None);
        assert_eq!(t.oldest_pool, None);
        t.set_pool_stamps(&[(a, 5_000_000), (b, 0), (c, 3_000_000)]);
        assert_eq!(t.freshest_pool_update_ns, Some(5_000_000));
        assert_eq!(t.oldest_pool_update_ns, Some(3_000_000));
        assert_eq!(t.oldest_pool, Some(c), "oldest stamp belongs to pool c");
    }

    #[test]
    fn summary_line_names_the_oldest_pool() {
        let pk = solana_sdk::pubkey::Pubkey::new_unique();
        let mut t = full_timeline();
        t.oldest_pool = Some(pk);
        let tag: String = pk.to_string().chars().take(8).collect();
        let line = t.summary_line(31, 48_000, 6_000);
        assert!(
            line.contains(&format!("oldest=48ms({tag})")),
            "oldest cell must carry the stale pool's short id: {line}"
        );
    }

    #[test]
    fn clock_reexport_works() {
        let a = now_ns();
        assert!(a >= 1);
    }

    fn rec(outcome: RecordOutcome, stale: u32) -> LatencyRecord {
        LatencyRecord {
            staleness_ms: stale,
            total_ms: stale + 30,
            accept_ms: 30,
            tip_ratio_x10: 10,
            outcome,
        }
    }

    #[test]
    fn from_timeline_full_and_missing() {
        let r = LatencyRecord::from_timeline(&full_timeline(), 31, 48_000, 6_000, RecordOutcome::Dropped)
            .expect("full timeline must produce a record");
        assert_eq!(r.staleness_ms, 48);
        assert_eq!(r.total_ms, 91);
        assert_eq!(r.accept_ms, 31);
        assert_eq!(r.tip_ratio_x10, 80); // 48000/6000 = 8.0×
        assert_eq!(r.outcome, RecordOutcome::Dropped);
        // Missing stamps → no record (skipped, not fabricated).
        assert!(LatencyRecord::from_timeline(&LatencyTimeline::default(), 5, 100, 6_000, RecordOutcome::Landed).is_none());
        // Unknown floor → ratio 0 (rendered as —/?)
        let r0 = LatencyRecord::from_timeline(&full_timeline(), 31, 48_000, 0, RecordOutcome::Dropped).unwrap();
        assert_eq!(r0.tip_ratio_x10, 0);
    }

    #[test]
    fn percentile_edge_cases() {
        assert_eq!(percentile(&[], 0.50), 0);
        assert_eq!(percentile(&[7], 0.50), 7);
        assert_eq!(percentile(&[7], 0.95), 7);
        assert_eq!(percentile(&[1, 2], 0.50), 1);
        assert_eq!(percentile(&[1, 2], 0.95), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 0.95), 4);
    }

    #[test]
    fn ring_evicts_oldest_at_cap() {
        let stats = LatencyStats::new();
        for i in 0..600u32 {
            stats.record(rec(RecordOutcome::Dropped, i));
        }
        let inner = stats.inner.lock().unwrap();
        assert_eq!(inner.ring.len(), RING_CAP);
        assert_eq!(inner.ring.front().unwrap().staleness_ms, 600 - RING_CAP as u32);
    }

    #[test]
    fn maybe_report_gating() {
        let stats = LatencyStats::new();
        // No data → never fires.
        assert!(stats.maybe_report_at(1_000, 6_000).is_none());
        // First record → fires immediately (last_report_ns == 0).
        stats.record(rec(RecordOutcome::Dropped, 50));
        assert!(stats.maybe_report_at(1_000, 6_000).is_some());
        // Nothing new since → silent.
        assert!(stats.maybe_report_at(u64::MAX, 6_000).is_none());
        // New record but < 10 min since last print → silent.
        stats.record(rec(RecordOutcome::Dropped, 70));
        assert!(stats.maybe_report_at(1_000 + REPORT_INTERVAL_NS - 1, 6_000).is_none());
        // ≥ 10 min → fires.
        assert!(stats.maybe_report_at(1_000 + REPORT_INTERVAL_NS, 6_000).is_some());
    }

    #[test]
    fn report_cold_start_renders_dashes_and_anchors() {
        let records = vec![rec(RecordOutcome::Dropped, 50), rec(RecordOutcome::Dropped, 70)];
        let report = render_report(&records, &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: 6_000 });
        assert!(report.contains("LATENCY summary (n=2"), "got:\n{report}");
        // Landed bucket is empty → dashes, not zeros.
        let landed_row = report.lines().find(|l| l.starts_with("Landed")).unwrap();
        assert!(landed_row.contains('—'), "got: {landed_row}");
        assert!(landed_row.trim_end().ends_with('0'), "n column should be 0: {landed_row}");
        // Dropped bucket has real numbers.
        let dropped_row = report.lines().find(|l| l.starts_with("Dropped")).unwrap();
        assert!(dropped_row.contains("50ms"), "p50 stale: {dropped_row}");
        assert!(dropped_row.contains("1.0x"), "p50 ratio: {dropped_row}");
        // Absolute anchors always present.
        assert!(report.contains("anchors: slot=400ms  tip_floor_ema=6000L"), "got:\n{report}");
    }

    #[test]
    fn verdict_cold_start_fast_but_floor_tips_says_tips() {
        // Never landed; dropped bundles are fast (60ms ≪ 400ms slot) but bid ~1.0× floor.
        let landed = BucketSummary::default();
        let dropped = BucketSummary {
            n: 40, p50_stale_ms: 60, p95_stale_ms: 110,
            p50_total_ms: 90, p50_accept_ms: 30, p50_ratio_x10: 10,
        };
        let failed = BucketSummary::default();
        let v = verdict(&landed, &dropped, &failed,
            &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: 6_000 })
            .expect("clear cold-start tip signal must produce a verdict");
        assert!(v.to_lowercase().contains("tip"), "got: {v}");
    }

    #[test]
    fn verdict_cold_start_stale_says_latency() {
        // Never landed; dropped bundles are already ~2 slots stale at submit.
        let landed = BucketSummary::default();
        let dropped = BucketSummary {
            n: 40, p50_stale_ms: 700, p95_stale_ms: 1_500,
            p50_total_ms: 750, p50_accept_ms: 30, p50_ratio_x10: 50,
        };
        let failed = BucketSummary { n: 6, p50_stale_ms: 800, ..Default::default() };
        let v = verdict(&landed, &dropped, &failed,
            &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: 6_000 })
            .expect("clear staleness signal must produce a verdict");
        assert!(v.to_lowercase().contains("latency"), "got: {v}");
    }

    #[test]
    fn verdict_thin_data_stays_silent() {
        let landed = BucketSummary::default();
        let dropped = BucketSummary { n: 2, p50_stale_ms: 60, p50_ratio_x10: 10, ..Default::default() };
        assert!(verdict(&landed, &dropped, &BucketSummary::default(),
            &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: 6_000 }).is_none(),
            "n=2 is too thin to call");
    }

    #[test]
    fn verdict_fast_high_ratio_drops_point_at_oldest() {
        // Third regime (observed live 2026-07-05): fast (60ms ≪ slot), bidding
        // ~5995× floor, never landing. Neither latency- nor floor-tip-bound —
        // arb-specific competition or phantom edges via stale legs. The message
        // must point the operator at the SUBMIT line's oldest= field.
        let landed = BucketSummary::default();
        let dropped = BucketSummary {
            n: 40, p50_stale_ms: 60, p95_stale_ms: 110,
            p50_total_ms: 90, p50_accept_ms: 30, p50_ratio_x10: 59_950,
        };
        let v = verdict(&landed, &dropped, &BucketSummary::default(),
            &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: 1_908 })
            .expect("fast + far-above-floor + all-dropped must produce a verdict");
        assert!(v.contains("oldest"), "must point at oldest= as the check: {v}");
        assert!(v.to_lowercase().contains("competition"), "must name the regime: {v}");
    }

    #[test]
    fn verdict_high_ratio_but_landing_fine_stays_silent() {
        // Same fast/high-ratio drops, but the bot lands plenty — drops are the
        // normal tail of a healthy bot, not a regime; stay silent.
        let landed = BucketSummary { n: 40, p50_stale_ms: 55, ..Default::default() };
        let dropped = BucketSummary {
            n: 40, p50_stale_ms: 60, p50_ratio_x10: 59_950, ..Default::default()
        };
        assert!(verdict(&landed, &dropped, &BucketSummary::default(),
            &Anchors { slot_ms: SLOT_MS, tip_floor_lamports: 1_908 }).is_none(),
            "dropped.n must dominate landed.n for the competition verdict");
    }
}
