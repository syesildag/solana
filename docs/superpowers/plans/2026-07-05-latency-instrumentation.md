# Detect-to-Submit Latency Instrumentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Instrument the arb bot's detect→submit pipeline and join per-submission stage latencies to Jito bundle outcomes, so the operator can tell whether races are lost on latency (milliseconds) or tip sizing.

**Architecture:** A monotonic ns clock + `last_update_ns` stamp live on `Pool` (in `dex/types.rs`, shared by both binaries). A `LatencyTimeline` of optional stage stamps is created per Bellman-Ford iteration and moved through the submission task; the existing bundle-outcome monitor converts it into a `LatencyRecord` and pushes it into a 512-entry ring (`LatencyStats`), which renders an outcome-bucketed percentile table at most every 10 minutes. Measurement only — no behavioural change.

**Tech Stack:** Rust (tokio, std atomics/Mutex/OnceLock only — no new dependencies).

**Spec:** `docs/superpowers/specs/2026-07-05-latency-instrumentation-design.md`

## Global Constraints

- **NEVER run `cargo fmt` or `rustfmt`** — the repo is not rustfmt-clean; formatting a whole file causes huge diff churn.
- Tests live in `#[cfg(test)] mod tests` at the **bottom of each source file**, run via `cargo test --bin solana-mev <filter>`.
- **Never reference `crate::arbitrage` from any file under `src/dex/`** — `src/bin/portfolio_watcher.rs` `#[path]`-includes `src/dex/mod.rs` into a crate that has no `arbitrage` module; such a reference breaks the `portfolio-watcher` build. This is why the clock lives in `dex/types.rs`.
- Measurement only: no change to routing, tips, cooldowns, or submission behaviour.
- The working tree contains unrelated modified files — `git add` **only** the files each task names. Never `git push`. Commit at the end of every task with trailer `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- `pools.json` is auto-generated — never edit it.
- Full-workspace check after each task: `cargo build --release 2>&1 | tail -3` must end in `Finished` (this compiles all four binaries, including `portfolio-watcher`).

---

### Task 1: Monotonic clock + `Pool.last_update_ns` + stamps at every pool-write site

**Files:**
- Modify: `src/dex/types.rs` (clock fns, field at ~line 290, `stamp_update` in `impl Pool`, 2 constructors, tests at bottom)
- Modify: `src/arbitrage/evaluator.rs:673` (test-helper struct literal)
- Modify: `src/graph/bellman_ford.rs:154` (test-helper struct literal)
- Modify: `src/graph/exchange_graph.rs:347` (test-helper struct literal)
- Modify: `src/main.rs` (~lines 806–836, three gRPC-callback branches)
- Modify: `src/dex/jupiter.rs` (~line 338, poller loop)

**Interfaces:**
- Consumes: nothing new.
- Produces (later tasks rely on these exact names):
  - `pub fn monotonic_now_ns() -> u64` in `src/dex/types.rs` — ns since process epoch, monotonic, `>= 1`, `0` reserved for "never".
  - `pub last_update_ns: AtomicU64` field on `Pool`.
  - `pub fn stamp_update(&self)` on `impl Pool` — stores `monotonic_now_ns()` with `Ordering::Relaxed`.

- [ ] **Step 1: Write the failing tests**

In `src/dex/types.rs`, inside the existing `#[cfg(test)] mod tests` block at the bottom of the file, add:

```rust
    #[test]
    fn monotonic_clock_nonzero_and_monotonic() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(a >= 1, "clock must never return 0 (reserved for 'never stamped')");
        assert!(b >= a, "clock must be monotonic");
    }

    #[test]
    fn pool_update_stamp_starts_zero_then_sets() {
        let pool = Pool::new_jupiter(Pubkey::new_unique(), Pubkey::new_unique());
        assert_eq!(pool.last_update_ns.load(Ordering::Relaxed), 0);
        pool.stamp_update();
        assert!(pool.last_update_ns.load(Ordering::Relaxed) >= 1);
    }
```

If the tests module lacks the imports, extend its `use` lines so `monotonic_now_ns`, `Pool`, `Pubkey`, and `Ordering` resolve (the module already tests `Pool`, so most are present).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev monotonic_clock 2>&1 | tail -5`
Expected: compile error — `cannot find function monotonic_now_ns` / `no field last_update_ns`.

- [ ] **Step 3: Implement clock + field + stamp**

In `src/dex/types.rs`, directly **above** `pub struct Pool {` (line ~254), add:

```rust
// ─── Monotonic process clock (latency instrumentation) ─────────────────────
// Lives here (not in arbitrage::latency) because portfolio_watcher #[path]-
// includes src/dex/ into a crate that has no `arbitrage` module.

static MONOTONIC_EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Nanoseconds since the process epoch. Monotonic (immune to NTP steps),
/// atomic-friendly, never 0 (0 = "never stamped"). Wraps after ~584 years.
pub fn monotonic_now_ns() -> u64 {
    (MONOTONIC_EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64)
        .max(1)
}
```

In `pub struct Pool`, immediately after the `pub b_lp_balance: AtomicU64,` field (line ~290), add:

```rust
    /// Latency instrumentation: `monotonic_now_ns()` of the last state write
    /// (gRPC vault/state/lp update or Jupiter poll). 0 = never updated.
    /// Read by the arb loop to compute opportunity staleness; never read on
    /// any hot path.
    pub last_update_ns: AtomicU64,
```

In `impl Pool` (e.g. right after `token_program_for`, line ~337), add:

```rust
    /// Stamp this pool as updated-now. Called wherever pool state is written
    /// and the graph edge refreshed (gRPC callback branches, Jupiter poller).
    pub fn stamp_update(&self) {
        self.last_update_ns
            .store(monotonic_now_ns(), Ordering::Relaxed);
    }
```

Add `last_update_ns: AtomicU64::new(0),` to **every** `Pool { ... }` struct literal. Sites (the compiler will confirm with E0063 — expected to be exactly these five):

1. `src/dex/types.rs` — `Pool::new_jupiter` (after `b_lp_balance: AtomicU64::new(0),` ~line 372)
2. `src/dex/types.rs` — `impl TryFrom<PoolConfig>` (after `b_lp_balance: AtomicU64::new(0),` ~line 568)
3. `src/arbitrage/evaluator.rs:673` — `zero_fee_pool` test helper (after `b_lp_balance: AtomicU64::new(0),`)
4. `src/graph/bellman_ford.rs:154` — `pool` test helper (after `reserve_b: AtomicU64::new(reserve_b),` — match its field order, just add the line before the closing brace region alongside the other atomics)
5. `src/graph/exchange_graph.rs:347` — `phoenix_pool_with_prices` test helper (same approach)

(`evaluator.rs:705`/`720` use struct-update `..` syntax and need no change.)

- [ ] **Step 4: Stamp at the four pool-write sites**

`src/main.rs` gRPC callback (three branches). Each gets one line **immediately before** its `graph_cb.update_pool(...)` call:

Branch 1 (lp accounts, ~line 813):
```rust
                    pool.stamp_update();
                    graph_cb.update_pool(&pool);
```
Branch 2 (vaults, ~line 824, inside the `for pool in &pools` loop):
```rust
                    pool.stamp_update();
                    graph_cb.update_pool(pool);
```
Branch 3 (CL state accounts, ~line 835):
```rust
                pool.stamp_update();
                graph_cb.update_pool(&pool);
```

`src/dex/jupiter.rs` poller (~line 338), same pattern:
```rust
                pool.a_lp_balance.store(probe_lamports, Ordering::Relaxed);
                pool.stamp_update();
                graph.update_pool(pool);
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --bin solana-mev types:: 2>&1 | tail -8`
Expected: PASS including `monotonic_clock_nonzero_and_monotonic` and `pool_update_stamp_starts_zero_then_sets`.

Run: `cargo build --release 2>&1 | tail -3`
Expected: `Finished` (all binaries, incl. portfolio-watcher, compile).

Run: `cargo test --bin solana-mev 2>&1 | tail -5`
Expected: full suite green (test helpers updated).

- [ ] **Step 6: Commit**

```bash
git add src/dex/types.rs src/dex/jupiter.rs src/main.rs src/arbitrage/evaluator.rs src/graph/bellman_ford.rs src/graph/exchange_graph.rs
git commit -m "feat(latency): monotonic clock + last_update_ns stamp on Pool

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: `arbitrage::latency` module — `LatencyTimeline` + summary line

**Files:**
- Create: `src/arbitrage/latency.rs`
- Modify: `src/arbitrage/mod.rs` (add `pub mod latency;`)

**Interfaces:**
- Consumes: `crate::dex::types::monotonic_now_ns` (Task 1).
- Produces:
  - `arbitrage::latency::now_ns() -> u64` (re-export of the dex clock)
  - `arbitrage::latency::init()`
  - `pub struct LatencyTimeline` (all fields `Option<u64>` ns + `region: Option<&'static str>`; `Default + Clone + Copy`) with `set_pool_stamps(&mut self, &[u64])`, `staleness_ms/oldest_ms/total_ms/bf_ms/eval_ms/sem_ms/jup_ms/build_ms(&self) -> Option<u64>`, and `summary_line(&self, accept_ms: u32, tip_lamports: u64, tip_floor: u64) -> String`.

- [ ] **Step 1: Register the module**

In `src/arbitrage/mod.rs` add (alphabetical position):

```rust
pub mod latency;
```

- [ ] **Step 2: Write the module skeleton with failing tests**

Create `src/arbitrage/latency.rs`:

```rust
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
```

Note the two multi-line string assertions use Rust's `\` line continuation — the expected strings contain **no** newline (identical to the `format!` template above them).

- [ ] **Step 3: Run tests**

Run: `cargo test --bin solana-mev latency:: 2>&1 | tail -10`
Expected: 6 tests PASS. (They were written together with the implementation in one file-creation step; treat any failure as a defect to fix now — do not adjust an expected string unless the template itself was mistyped.)

- [ ] **Step 4: Commit**

```bash
git add src/arbitrage/latency.rs src/arbitrage/mod.rs
git commit -m "feat(latency): LatencyTimeline with per-stage stamps and SUBMIT summary line

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `LatencyRecord`, `LatencyStats` ring, percentile report, verdict scaffold

**Files:**
- Modify: `src/arbitrage/latency.rs` (append below the `LatencyTimeline` impl, above the tests module; extend tests)

**Interfaces:**
- Consumes: `LatencyTimeline` (Task 2).
- Produces:
  - `pub enum RecordOutcome { Landed, FailedOnChain, Dropped }` (`Debug + Clone + Copy + PartialEq + Eq`)
  - `pub struct LatencyRecord { staleness_ms: u32, total_ms: u32, accept_ms: u32, tip_ratio_x10: u32, outcome: RecordOutcome }` with `pub fn from_timeline(t: &LatencyTimeline, accept_ms: u32, tip_lamports: u64, tip_floor: u64, outcome: RecordOutcome) -> Option<LatencyRecord>`
  - `pub struct LatencyStats` with `pub fn new() -> std::sync::Arc<LatencyStats>`, `pub fn record(&self, r: LatencyRecord)`, `pub fn maybe_report(&self, tip_floor: u64) -> Option<String>`
  - `pub struct BucketSummary { n, p50_stale_ms, p95_stale_ms, p50_total_ms, p50_accept_ms, p50_ratio_x10 }` (`Default + Clone + Copy`)
  - `pub struct Anchors { slot_ms: u32, tip_floor_lamports: u64 }`
  - `pub fn verdict(landed: &BucketSummary, dropped: &BucketSummary, failed: &BucketSummary, anchors: &Anchors) -> Option<String>` (scaffold returns `None`; Task 6 fills it)
  - `pub const SLOT_MS: u32 = 400;`, `pub const RING_CAP: usize = 512;`, `pub const REPORT_INTERVAL_NS: u64`

- [ ] **Step 1: Write the failing tests**

Append inside the existing `#[cfg(test)] mod tests` in `src/arbitrage/latency.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev latency:: 2>&1 | tail -5`
Expected: compile error — `cannot find struct LatencyRecord`, `percentile`, etc.

- [ ] **Step 3: Implement records, stats, report, verdict scaffold**

Append to `src/arbitrage/latency.rs` after the `impl LatencyTimeline` block (before `#[cfg(test)]`):

```rust
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
/// consumed before the auction is decided) and dropped p50 tip ratio vs floor
/// (≈ 1.0× floor while fast → tip-dominated). Return `None` when the data is
/// too thin to call.
pub fn verdict(
    landed: &BucketSummary,
    dropped: &BucketSummary,
    failed: &BucketSummary,
    anchors: &Anchors,
) -> Option<String> {
    // TODO(user): ~8 lines of operator judgment. The contract tests
    // (verdict_* in this file, Task 6) define the expected behaviour.
    let _ = (landed, dropped, failed, anchors);
    None
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin solana-mev latency:: 2>&1 | tail -10`
Expected: all latency tests PASS (11 total: 6 from Task 2 + 5 new).

Run: `cargo build --release 2>&1 | tail -3` — Expected: `Finished`. A `dead_code` warning on `maybe_report`/`verdict` is acceptable until Task 5 wires them.

- [ ] **Step 5: Commit**

```bash
git add src/arbitrage/latency.rs
git commit -m "feat(latency): outcome-joined LatencyStats ring with percentile report

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: `SubmitReceipt` from `submit_bundle`

**Files:**
- Modify: `src/jito/client.rs` (~lines 7–20 enum area, 146–230 `submit_bundle`)
- Modify: `src/main.rs:1223-1224` (call site — minimal adaptation only; full timeline wiring is Task 5)

**Interfaces:**
- Consumes: nothing from Tasks 1–3.
- Produces:
  - `pub struct SubmitReceipt { pub bundle_id: String, pub region: &'static str, pub accept_ms: u32 }` in `src/jito/client.rs`
  - `pub async fn submit_bundle(&self, bundle: &JitoBundle) -> Result<SubmitReceipt>`

- [ ] **Step 1: Add the receipt type and change the signature**

In `src/jito/client.rs`, immediately after the `BundleOutcome` enum block, add:

```rust
/// What `submit_bundle` learned from the first Block Engine to accept:
/// which region won the parallel race and how long the accept took.
/// Feeds the latency instrumentation (arbitrage::latency); `bundle_id`
/// is what callers previously received as a bare `String`.
#[derive(Debug, Clone)]
pub struct SubmitReceipt {
    pub bundle_id: String,
    pub region: &'static str,
    pub accept_ms: u32,
}
```

Change the `submit_bundle` signature and add the entry stopwatch:

```rust
    pub async fn submit_bundle(&self, bundle: &JitoBundle) -> Result<SubmitReceipt> {
        let t0 = std::time::Instant::now();
        let encoded = bundle.encode().context("Failed to encode bundle")?;
```

Change the dry-run early return (inside `if self.dry_run { ... }`):

```rust
            return Ok(SubmitReceipt {
                bundle_id: "dry-run-no-id".to_string(),
                region: "dry",
                accept_ms: 0,
            });
```

- [ ] **Step 2: Capture region + accept time at first accept**

Replace `let mut first_id: Option<String> = None;` with:

```rust
        let mut first: Option<SubmitReceipt> = None;
```

In the `Ok(id) =>` arm, replace the `if first_id.is_none() { first_id = Some(id.clone());` head with:

```rust
                    if first.is_none() {
                        first = Some(SubmitReceipt {
                            bundle_id: id.clone(),
                            region,
                            accept_ms: t0.elapsed().as_millis().min(u32::MAX as u128) as u32,
                        });
```

(The body that spawns the background drain logger is unchanged.)

Replace the final `match first_id { ... }` with:

```rust
        match first {
            Some(receipt) => {
                info!(
                    bundle_id = %receipt.bundle_id,
                    region = receipt.region,
                    accept_ms = receipt.accept_ms,
                    "Bundle accepted by first region"
                );
                Ok(receipt)
            }
            None => anyhow::bail!("All {} Block Engine regions rejected the bundle", REGIONS.len()),
        }
```

- [ ] **Step 3: Minimally adapt the call site**

In `src/main.rs` (~line 1223), change:

```rust
                    match jito.submit_bundle(&bundle).await {
                        Ok(id) => {
```

to:

```rust
                    match jito.submit_bundle(&bundle).await {
                        Ok(receipt) => {
                            let jito::client::SubmitReceipt { bundle_id: id, .. } = receipt;
```

Everything downstream keeps using `id` unchanged. (`region`/`accept_ms` are consumed in Task 5; the `..` pattern avoids unused-variable warnings now.)

- [ ] **Step 4: Verify build + tests**

Run: `cargo build --release 2>&1 | tail -3` — Expected: `Finished`.
Run: `cargo test --bin solana-mev 2>&1 | tail -5` — Expected: full suite green.

- [ ] **Step 5: Commit**

```bash
git add src/jito/client.rs src/main.rs
git commit -m "feat(latency): submit_bundle returns SubmitReceipt (region + accept ms)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Wire the timeline through main.rs and print the report

**Files:**
- Modify: `src/main.rs` (init at ~line 150; stats Arc at ~line 744; BF clone block ~line 892; stat window ~line 949; BF stamps ~line 963; eval stamps ~line 1138; submission-task clones ~line 1180; spawn body ~lines 1184–1230; outcome monitor ~line 1256)

**Interfaces:**
- Consumes (exact names from earlier tasks): `arbitrage::latency::{init, now_ns, LatencyTimeline, LatencyRecord, RecordOutcome, LatencyStats}`, `Pool.last_update_ns`, `jito::client::SubmitReceipt`.
- Produces: running-bot behaviour — `SUBMIT latency ...` info line per accepted bundle; `LATENCY summary` table ≤ every 10 min.

- [ ] **Step 1: Init the clock and create the stats handle**

In `main()`, immediately after the `tracing_subscriber::fmt()...` initialization statement (~line 150), add:

```rust
    arbitrage::latency::init();
```

Next to the update channel creation (line ~744, `let (update_tx, update_rx) = watch::channel(0u64);`), add:

```rust
    let latency_stats = arbitrage::latency::LatencyStats::new();
```

- [ ] **Step 2: Clone into the BF task**

In the BF clone block (~line 892, alongside `let tip_floor_bf = Arc::clone(&tip_floor_cache);`), add:

```rust
        let latency_stats_bf = Arc::clone(&latency_stats);
```

- [ ] **Step 3: Stamp the BF run**

Around line 963, change:

```rust
                stat_bf_runs += 1;
                let search = bellman_ford::find_negative_cycles_with_diag(&graph_bf, base_mint);
```

to:

```rust
                stat_bf_runs += 1;
                let mut timeline = arbitrage::latency::LatencyTimeline {
                    bf_start: Some(arbitrage::latency::now_ns()),
                    ..Default::default()
                };
                let search = bellman_ford::find_negative_cycles_with_diag(&graph_bf, base_mint);
                timeline.bf_done = Some(arbitrage::latency::now_ns());
```

- [ ] **Step 4: Stamp evaluation + pool staleness for the chosen cycle**

Immediately after the `let Some((opportunity, cycle_key)) = chosen else { ... };` block (~line 1138), add:

```rust
                timeline.eval_done = Some(arbitrage::latency::now_ns());
                let pool_stamps: Vec<u64> = opportunity.cycle.edges.iter()
                    .filter_map(|e| registry_bf.get_by_pool_id(&e.pool_id))
                    .map(|p| p.last_update_ns.load(Ordering::Relaxed))
                    .collect();
                timeline.set_pool_stamps(&pool_stamps);
```

- [ ] **Step 5: Report tick**

In the 10 s stat-window block, directly after the `info!("BF window — ...")` call (ends ~line 949, before `stat_bf_runs = 0;`), add:

```rust
                    if let Some(report) = latency_stats_bf.maybe_report(floor) {
                        info!("\n{report}");
                    }
```

(`floor` is already loaded from `tip_floor_bf` a few lines above at ~line 939.)

- [ ] **Step 6: Thread the timeline through the submission task**

In the submission-task clone list (~line 1180, alongside `let user_t = user;`), add:

```rust
                let latency_stats_t = Arc::clone(&latency_stats_bf);
```

Change the spawn head (~line 1184):

```rust
                tokio::spawn(async move {
                    let mut opportunity = opportunity;
                    let _permit = sem.acquire().await.expect("Semaphore closed");
```

to:

```rust
                tokio::spawn(async move {
                    let mut opportunity = opportunity;
                    let mut timeline = timeline;
                    timeline.spawned = Some(arbitrage::latency::now_ns());
                    let _permit = sem.acquire().await.expect("Semaphore closed");
                    timeline.sem_acquired = Some(arbitrage::latency::now_ns());
```

After the `resolve_jupiter_hops` match (the `let submit_alts = match ... };` statement), add:

```rust
                    timeline.jup_resolved = Some(arbitrage::latency::now_ns());
```

After the `let bundle = match JitoBundle::build(...) ... };` statement, add:

```rust
                    timeline.built = Some(arbitrage::latency::now_ns());
```

Immediately before `match jito.submit_bundle(&bundle).await {`, add:

```rust
                    timeline.submit_started = Some(arbitrage::latency::now_ns());
```

- [ ] **Step 7: Consume the receipt + log the summary line**

Change the `Ok` arm head (from Task 4's minimal version):

```rust
                        Ok(receipt) => {
                            let jito::client::SubmitReceipt { bundle_id: id, .. } = receipt;
```

to:

```rust
                        Ok(receipt) => {
                            timeline.accepted = Some(arbitrage::latency::now_ns());
                            timeline.region = Some(receipt.region);
                            let jito::client::SubmitReceipt { bundle_id: id, accept_ms, .. } = receipt;
```

Then, directly after the existing red `eprintln!("\x1b[31mBundle submitted ...")` statement, add:

```rust
                            info!("{}", timeline.summary_line(accept_ms, opportunity.jito_tip_lamports, floor_now));
```

(`floor_now` is already defined at the top of the `Ok` arm.)

- [ ] **Step 8: Push the outcome-joined record in the monitor task**

In the monitor-spawn capture list (~lines 1246–1255, alongside `let floor_dropped = Arc::clone(&tip_floor_t);`), add:

```rust
                            let stats_outcome      = Arc::clone(&latency_stats_t);
                            let timeline_outcome   = timeline; // Copy
                            let accept_ms_outcome  = accept_ms;
                            let floor_at_submit    = floor_now;
```

Change the monitor body head from:

```rust
                            tokio::spawn(async move {
                                use jito::client::BundleOutcome;
                                match jito_poll.log_bundle_outcome(&id).await {
```

to:

```rust
                            tokio::spawn(async move {
                                use jito::client::BundleOutcome;
                                let outcome = jito_poll.log_bundle_outcome(&id).await;
                                let rec_outcome = match &outcome {
                                    BundleOutcome::Landed        => arbitrage::latency::RecordOutcome::Landed,
                                    BundleOutcome::FailedOnChain => arbitrage::latency::RecordOutcome::FailedOnChain,
                                    BundleOutcome::Dropped       => arbitrage::latency::RecordOutcome::Dropped,
                                };
                                if let Some(rec) = arbitrage::latency::LatencyRecord::from_timeline(
                                    &timeline_outcome, accept_ms_outcome, tip_dropped, floor_at_submit, rec_outcome,
                                ) {
                                    stats_outcome.record(rec);
                                }
                                match outcome {
```

The existing match arms (`BundleOutcome::Landed => { ... }` etc.) stay byte-identical.

- [ ] **Step 9: Verify**

Run: `cargo build --release 2>&1 | tail -3` — Expected: `Finished`, no warnings mentioning `latency`.
Run: `cargo test --bin solana-mev 2>&1 | tail -5` — Expected: full suite green.
Run: `cargo clippy --bin solana-mev 2>&1 | grep -A3 latency` — Expected: no output (clippy may suggest `#[derive(Default)]`-style nits elsewhere; only fix ones in touched code).

Optional manual smoke (needs populated `.env` + gRPC): `DRY_RUN=true cargo run --release --bin solana-mev` and watch for a `SUBMIT latency staleness=...` line after the first dry-run submission, then a `LATENCY summary` table (dry-run outcomes resolve as `Landed` immediately).

- [ ] **Step 10: Commit**

```bash
git add src/main.rs
git commit -m "feat(latency): wire timeline detect→submit and outcome-joined report

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: Verdict heuristic — contract tests + USER-WRITTEN body

**Files:**
- Modify: `src/arbitrage/latency.rs` (tests + the `verdict` body)

**Interfaces:**
- Consumes: `verdict`, `BucketSummary`, `Anchors`, `SLOT_MS` (Task 3).
- Produces: a non-`None` verdict line on clear signals; report output otherwise unchanged.

**⚠️ CHECKPOINT TASK — requires the human operator.** The verdict thresholds are operator judgment (what counts as "same speed", "too stale", minimum sample size). Steps 1–2 are agent work; Step 3 is the user's ~8 lines; do not write the body for them.

- [ ] **Step 1: Write the failing contract tests**

Append inside `#[cfg(test)] mod tests` in `src/arbitrage/latency.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify the two positive contracts fail**

Run: `cargo test --bin solana-mev verdict 2>&1 | tail -8`
Expected: `verdict_thin_data_stays_silent` PASSES (scaffold returns `None`); the other two FAIL with `expect` panics.

- [ ] **Step 3: CHECKPOINT — hand `verdict()` to the user**

Ask the user to replace the `TODO(user)` body in `src/arbitrage/latency.rs::verdict` (~8 lines). Guidance to give them, verbatim:

> The contract tests define the behaviour: (1) require a minimum sample before speaking (the n=2 test must stay silent — pick your own floor, e.g. `dropped.n < 10`); (2) if dropped p50 staleness ≥ your multiple of `anchors.slot_ms`, return a message containing "latency"; (3) else if dropped p50 tip ratio is near floor (`p50_ratio_x10` close to 10) while staleness is well under a slot, return a message containing "tip"; (4) otherwise `None`. When `landed.n > 0` you can sharpen it — e.g. compare dropped vs landed p50 staleness — but the cold-start branches above are the ones that will fire first.

- [ ] **Step 4: Run tests to verify all pass**

Run: `cargo test --bin solana-mev latency:: 2>&1 | tail -8`
Expected: all latency tests PASS, including the three verdict contracts.

- [ ] **Step 5: Commit**

```bash
git add src/arbitrage/latency.rs
git commit -m "feat(latency): operator verdict heuristic for the LATENCY summary

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Done criteria (whole plan)

- `cargo build --release` compiles all four binaries.
- `cargo test --bin solana-mev` fully green; latency module has ≥14 tests.
- Running the bot (dry-run suffices) produces `SUBMIT latency ...` per submission and a `LATENCY summary` table with `anchors:` line; buckets with n=0 render `—`.
- No behavioural diff: tips, routing, cooldowns, and submission flow byte-identical except added log lines.
