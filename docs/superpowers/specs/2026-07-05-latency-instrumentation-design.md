# Detect-to-Submit Latency Instrumentation — Design

**Date:** 2026-07-05
**Status:** Approved
**Goal:** Answer one operational question: *are Jito bundle races lost on latency
(milliseconds) or on tip sizing?*

## Problem

The bot submits Jito bundles but has **never won a contested opportunity**. The pipeline
(gRPC update → Bellman-Ford → `optimize_input_and_tip` → semaphore → Jupiter-hop
resolve → bundle build → `submit_bundle`) has **no stage timing**, and although
`log_bundle_outcome` already classifies every submission as `Landed` /
`FailedOnChain` / `Dropped`, nothing joins outcome to how *stale* the opportunity
was at submit time. Without that join, "too slow" and "outbid" are
indistinguishable.

The diagnostic is conditioning, not raw timing: bucket submissions by outcome and
compare latency distributions.

- Dropped ≈ Landed latency profile → losing the **tip auction**.
- FailedOnChain / Dropped skew stale → losing the **clock**.

Because no contested bundle has ever landed, the `Landed` bucket is expected to
be empty at first — possibly for the whole measurement period. The design must
therefore also work against **absolute anchors** (slot time, tip floor), not
only against a Landed baseline. See §5–6.

## Scope

**In:** per-submission stage-latency log line; outcome-bucketed percentile summary
(p50/p95/n) printed periodically; first-accepting Jito region + accept RTT; a
small user-written verdict heuristic appended to the summary.

**Out (deliberately, all addable later without rework):** slot-level race
forensics (update slot vs landed slot), per-region Jito RTT probes,
Prometheus/Grafana, any change to submission behaviour. This feature is
**measurement only** — zero effect on routing, tips, or cooldowns.

## Design

### 1. Monotonic clock + per-pool update stamp

New module `src/arbitrage/latency.rs` owns a process-wide epoch
(`OnceLock<Instant>`) and `now_ns() -> u64` = nanos since epoch. Monotonic
`Instant` (not `SystemTime`) so NTP steps can never produce negative latencies;
nanos-since-epoch as `u64` makes the value atomic-friendly (overflow ≈ 584 years).

`Pool` gains `last_update_ns: AtomicU64` (0 = never updated). Stamped with
`Ordering::Relaxed` everywhere pool state changes and `graph.update_pool()` is
called:

- gRPC callback in `main.rs` — lp-account, vault, and state-account branches;
- the Jupiter REST poller (`dex::jupiter::spawn_poller`), for consistency.

Cost: one atomic store per account update. The whale-hint path only signals the
existing BF loop, so it needs no separate instrumentation.

### 2. `LatencyTimeline`

Plain struct in `latency.rs`, all fields `Option<u64>` (ns since epoch):

| Stamp | Taken |
|---|---|
| `bf_start` | top of BF iteration, after `update_rx.changed()` |
| `bf_done` | after `find_negative_cycles_with_diag` returns |
| `eval_done` | after the chosen opportunity is selected |
| `spawned` | first line of the submission task |
| `sem_acquired` | after `sem.acquire()` |
| `jup_resolved` | after `resolve_jupiter_hops` |
| `built` | after `JitoBundle::build` |
| `submit_started` | before `jito.submit_bundle` |
| `accepted` | from `SubmitReceipt` (first region accept) |
| `freshest_pool_update_ns` / `oldest_pool_update_ns` | min/max age over the chosen cycle's pools, read via the registry when the cycle is selected |

Created per BF iteration in the loop, moved into the submission task alongside
`opportunity` — **no new field on `ArbOpportunity`**, no global registry.

Derived durations use `saturating_sub`; a missing stamp renders as `?` and never
panics. Key derived value: **`staleness` = `submit_started` −
`freshest_pool_update_ns`** — time from the market event that created the edge to
the bundle leaving the process. `oldest` is also logged (quote-validity bound).

### 3. `SubmitReceipt`

`JitoClient::submit_bundle` return type changes from `Result<String>` to
`Result<SubmitReceipt>`:

```rust
pub struct SubmitReceipt {
    pub bundle_id: String,
    pub region: &'static str, // first region to accept
    pub accept_ms: u32,       // time to that first accept
}
```

Single call site (`main.rs`) updates; dry-run returns
`{ "dry-run-no-id", "dry", 0 }`. The per-region race already exists — this only
records who won it.

### 4. Per-submission log line

Emitted at `info!` immediately after accept (the existing red
`Bundle submitted` eprintln stays as-is):

```
SUBMIT latency staleness=48ms oldest=310ms bf=2ms eval=9ms sem=0ms jup=0ms build=1ms jito_accept=31ms (ams) total=91ms tip=48000L ratio=8x
```

`total` = `accepted` − `freshest_pool_update_ns`. `ratio` = tip / current floor
(as already computed for the `Bundle submitted` line).

### 5. Outcome join + rolling aggregate

`latency.rs` adds:

```rust
pub struct LatencyRecord {
    pub staleness_ms: u32,
    pub total_ms: u32,
    pub accept_ms: u32,
    pub tip_ratio_x10: u32,       // tip/floor × 10, saturating
    pub outcome: RecordOutcome,   // Landed | FailedOnChain | Dropped
}
pub struct LatencyStats { /* Mutex<VecDeque<LatencyRecord>> cap 512 + last_report */ }
```

The existing outcome-monitor task (the `log_bundle_outcome` match in `main.rs`)
receives a compact pre-computed record (moved into its spawn) and pushes it with
the resolved outcome. Every submission resolves within ≤ 20 s (timeout ⇒
`Dropped`), so records are only inserted with a final outcome — no pending state.
Dry-run resolves `Landed` immediately; records are still collected (accept/RTT
fields are meaningless there, which is acceptable).

Reporting: the BF loop's existing 10 s stats tick calls
`stats.maybe_report()` — prints at most every 10 min, only when new records
arrived since the last print, aggregating over the ring's ≤ 512 entries:

```
LATENCY summary (n=47, ring≤512, printed ≤ every 10m)
          p50_stale  p95_stale  p50_total  p50_accept  p50_ratio  n
Landed          —          —          —           —          —    0
Dropped       58ms      102ms       88ms        30ms       1.0×  41
Failed       310ms      720ms      350ms        29ms       1.2×   6
anchors: slot=400ms  tip_floor_ema=6000L
⇒ p50 staleness 58ms ≪ 1 slot but tips pinned at floor — tips, not latency
```

**Zero-landed reality:** rows with n = 0 print `—`; the `anchors:` line and the
per-bucket `p50_ratio` (tip / floor) make the table diagnostic even when nothing
has ever landed. The two cold-start readings:

- p50 staleness ≥ ~400 ms (1 slot) → **latency-dominated** — the opportunity is
  gone before the auction is decided, no tip can save it.
- p50 staleness well under a slot **and** tips ≈ 1× floor on Dropped bundles →
  **tip-dominated** — fast enough, outbid. (The `BYPASS_JITO_BUNDLE` floor-tip
  path bids ~6 000 lamports; contested cycles routinely clear orders of
  magnitude higher.)

Percentiles: sort a copy of ≤ 512 `u32`s at report time (trivial cost).
Concurrency: the `Mutex` is touched per-submission-resolution and per-report only
— **never** in the gRPC callback or the BF hot loop.

### 6. Verdict heuristic (user-written)

`fn verdict(landed: &BucketSummary, dropped: &BucketSummary, failed: &BucketSummary, anchors: &Anchors) -> Option<String>`
appended to the report when it returns `Some`. `Anchors` carries the slot-time
constant (400 ms) and the current tip-floor EMA. The function **must handle
`landed.n == 0` as the primary case** (it is the expected state at rollout),
falling back to the absolute-anchor readings described in §5. The thresholds
(how similar is "same speed", how stale is "too stale", minimum n) are operator
judgment — signature and computed inputs will be prepared with a `TODO` body for
the user to fill (~8 lines) during implementation.

### 7. Error handling

- Missing stamps → `?` in logs, `None` skipped in aggregates; no unwrap/panic.
- `saturating_sub` everywhere; `u32::MAX` cap on ms conversions.
- Tip floor = 0 (EMA not yet warmed) → `tip_ratio_x10` recorded as 0, rendered
  `ratio=?` — never a division by zero.
- Failed submissions (Jupiter resolve error, build error, all regions reject)
  produce no record — they never entered the race, and polluting the buckets
  would blur the answer.
- Instrumentation failure must never abort a submission: stats recording is
  best-effort.

### 8. Testing

Repo convention — `#[cfg(test)]` at the bottom of `latency.rs`:

- derived-duration math, including missing stamps and out-of-order stamps;
- ring-buffer cap eviction at 512;
- percentile edge cases (n = 0, 1, 2);
- `maybe_report` gating (10-min interval AND new-data requirement);
- outcome bucketing;
- report rendering with an empty `Landed` bucket (n = 0 → `—`, anchors line
  present), and the verdict cold-start path once the user-provided body lands.

`cargo test --bin solana-mev` stays green. No whole-file `rustfmt` (repo is not
rustfmt-clean).

## Files touched

| File | Change |
|---|---|
| `src/arbitrage/latency.rs` | **new** — epoch, timeline, records, stats, report, verdict |
| `src/arbitrage/mod.rs` | register module |
| `src/dex/types.rs` | `Pool.last_update_ns: AtomicU64` (+ constructors) |
| `src/main.rs` | stamp callback branches, timeline through BF loop + submission task, record push in outcome monitor, `maybe_report` on stats tick |
| `src/jito/client.rs` | `SubmitReceipt` return type |
| `src/dex/jupiter.rs` | stamp `last_update_ns` in poller |

## Alternatives considered

- **`tracing` spans + offline analysis** — least code, but the outcome join
  becomes manual log archaeology by bundle_id, and there is no live verdict.
- **Prometheus/Grafana** — proper histograms, wrong weight class for a
  single-operator bot (new dep, HTTP endpoint, dashboard upkeep).
