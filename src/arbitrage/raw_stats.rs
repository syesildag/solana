//! Outcome accounting for the raw-RPC submission path (`ENABLE_RAW_RPC`).
//!
//! Kept deliberately separate from the LATENCY ring: `LatencyStats::verdict()` reasons
//! about Jito tip auctions (staleness-vs-tip), while raw sends have no auction — a raw
//! "Expired" means the blockhash died un-included, not "outbid", and tip_ratio=0 rows
//! would corrupt the operator verdicts. This module answers the raw path's own three
//! questions: does anything land, what do the failures say (a `ProgramAccountNotFound`
//! here falsifies the no-ALT-immunity hypothesis), and what do reverts burn in fees —
//! a failed Jito bundle costs $0, but a reverted raw tx pays base + priority fee.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use solana_sdk::transaction::TransactionError;

pub use crate::dex::types::monotonic_now_ns as now_ns;

/// Landed-confirmation ring capacity (p50 over the most recent landings).
pub const RAW_RING_CAP: usize = 256;
/// Minimum interval between `RAW summary` report lines.
pub const REPORT_INTERVAL_NS: u64 = 600 * 1_000_000_000; // 10 min

/// Terminal state of one raw send, as classified from signature-status polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawOutcome {
    /// Status visible with no error: the tx executed (profit locked by min_out guards).
    Landed,
    /// Status visible WITH an error: the tx was included and reverted — fees were paid.
    /// The error string is the experiment's core observable.
    FailedOnChain,
    /// No status and the blockhash is no longer valid: the tx died un-included — no fee.
    Expired,
}

/// Pure classifier for the polling monitor. `status` is the `err` field of a visible
/// signature status (`Some(Some(e))` = included-and-failed, `Some(None)` = included-ok),
/// `None` = not (yet) visible. Returns `None` while the outcome is still open.
pub fn classify_raw_status(
    status: Option<Option<TransactionError>>,
    blockhash_valid: bool,
) -> Option<RawOutcome> {
    match status {
        Some(Some(_)) => Some(RawOutcome::FailedOnChain),
        Some(None) => Some(RawOutcome::Landed),
        None if !blockhash_valid => Some(RawOutcome::Expired),
        None => None,
    }
}

pub struct RawStats {
    pub sent: AtomicU64,
    pub landed: AtomicU64,
    pub failed_onchain: AtomicU64,
    pub expired: AtomicU64,
    pub dry_run_skips: AtomicU64,
    confirm_ms: Mutex<VecDeque<u32>>,
    last_report_ns: AtomicU64,
}

impl RawStats {
    pub fn new() -> Arc<RawStats> {
        Arc::new(RawStats {
            sent: AtomicU64::new(0),
            landed: AtomicU64::new(0),
            failed_onchain: AtomicU64::new(0),
            expired: AtomicU64::new(0),
            dry_run_skips: AtomicU64::new(0),
            confirm_ms: Mutex::new(VecDeque::with_capacity(RAW_RING_CAP)),
            last_report_ns: AtomicU64::new(0),
        })
    }

    /// Record a landed raw tx and its submit→confirm latency.
    pub fn record_landed(&self, confirm_ms: u32) {
        self.landed.fetch_add(1, Ordering::Relaxed);
        let mut ring = self.confirm_ms.lock().unwrap_or_else(|e| e.into_inner());
        if ring.len() == RAW_RING_CAP {
            ring.pop_front();
        }
        ring.push_back(confirm_ms);
    }

    pub fn maybe_report(&self, cu_limit: u64, cu_price_micro: u64) -> Option<String> {
        self.maybe_report_at(now_ns(), cu_limit, cu_price_micro)
    }

    /// Emit the summary at most every `REPORT_INTERVAL_NS`, and only once something
    /// happened. Fee burn counts ONLY included-and-reverted txs (expired ones never
    /// paid); the estimate assumes the full CU limit was charged — an upper bound.
    pub fn maybe_report_at(&self, now: u64, cu_limit: u64, cu_price_micro: u64) -> Option<String> {
        let sent = self.sent.load(Ordering::Relaxed);
        let dry = self.dry_run_skips.load(Ordering::Relaxed);
        if sent == 0 && dry == 0 {
            return None;
        }
        let last = self.last_report_ns.load(Ordering::Relaxed);
        if last != 0 && now.saturating_sub(last) < REPORT_INTERVAL_NS {
            return None;
        }
        if self
            .last_report_ns
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return None; // another thread just reported
        }
        let landed = self.landed.load(Ordering::Relaxed);
        let failed = self.failed_onchain.load(Ordering::Relaxed);
        let expired = self.expired.load(Ordering::Relaxed);
        let pct = |n: u64| if sent > 0 { n as f64 * 100.0 / sent as f64 } else { 0.0 };
        let p50 = {
            let ring = self.confirm_ms.lock().unwrap_or_else(|e| e.into_inner());
            let mut v: Vec<u32> = ring.iter().copied().collect();
            if v.is_empty() {
                None
            } else {
                v.sort_unstable();
                Some(v[v.len() / 2])
            }
        };
        let fee_burn = failed * (5_000 + cu_limit * cu_price_micro / 1_000_000);
        Some(format!(
            "RAW summary sent={sent} landed={landed}({:.0}%) failed={failed}({:.0}%) expired={expired}({:.0}%) p50_confirm={} est_fee_burn={fee_burn}L dry_run_skips={dry}",
            pct(landed),
            pct(failed),
            pct(expired),
            p50.map(|m| format!("{m}ms")).unwrap_or_else(|| "n/a".into()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::transaction::TransactionError;

    #[test]
    fn classifier_truth_table() {
        // Included with an error → FailedOnChain (fees paid), regardless of blockhash.
        assert_eq!(
            classify_raw_status(Some(Some(TransactionError::AccountNotFound)), true),
            Some(RawOutcome::FailedOnChain)
        );
        assert_eq!(
            classify_raw_status(Some(Some(TransactionError::AccountNotFound)), false),
            Some(RawOutcome::FailedOnChain)
        );
        // Included cleanly → Landed.
        assert_eq!(classify_raw_status(Some(None), true), Some(RawOutcome::Landed));
        // Not visible + blockhash dead → Expired (never included, no fee).
        assert_eq!(classify_raw_status(None, false), Some(RawOutcome::Expired));
        // Not visible + blockhash still valid → keep polling.
        assert_eq!(classify_raw_status(None, true), None);
    }

    #[test]
    fn report_counts_percentages_and_fee_burn() {
        let s = RawStats::new();
        s.sent.store(4, Ordering::Relaxed);
        s.record_landed(800);
        s.record_landed(1_200);
        s.failed_onchain.store(1, Ordering::Relaxed);
        s.expired.store(1, Ordering::Relaxed);
        // 600k CU × 1000 µlam/CU = 600_000_000 µlam = 600 lamports priority + 5000 base.
        let line = s.maybe_report_at(1, 600_000, 1_000).expect("first report fires");
        assert!(line.contains("sent=4"), "{line}");
        assert!(line.contains("landed=2(50%)"), "{line}");
        assert!(line.contains("failed=1(25%)"), "{line}");
        assert!(line.contains("expired=1(25%)"), "{line}");
        assert!(line.contains("p50_confirm=1200ms"), "{line}");
        assert!(line.contains("est_fee_burn=5600L"), "{line}");
    }

    #[test]
    fn report_cadence_gates_and_requires_activity() {
        let s = RawStats::new();
        // Nothing recorded → never reports.
        assert!(s.maybe_report_at(1, 600_000, 1_000).is_none());
        s.dry_run_skips.store(1, Ordering::Relaxed);
        assert!(s.maybe_report_at(10, 600_000, 1_000).is_some(), "dry-run activity reports");
        // Within the interval → silent; after it → reports again.
        assert!(s.maybe_report_at(10 + REPORT_INTERVAL_NS - 1, 600_000, 1_000).is_none());
        assert!(s.maybe_report_at(10 + REPORT_INTERVAL_NS + 1, 600_000, 1_000).is_some());
    }

    #[test]
    fn confirm_ring_caps_at_capacity() {
        let s = RawStats::new();
        for i in 0..(RAW_RING_CAP + 10) {
            s.record_landed(i as u32);
        }
        let ring = s.confirm_ms.lock().unwrap();
        assert_eq!(ring.len(), RAW_RING_CAP);
        assert_eq!(*ring.front().unwrap(), 10, "oldest entries evicted");
    }
}
