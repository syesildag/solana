//! gRPC price feed for the momentum trader.
//!
//! This module provides an opt-in Yellowstone gRPC-based price source for the momentum
//! trader, preferred over the existing REST price API (DexScreener/Kraken) when fresh.
//! Configuration is via `PortfolioConfig` fields: `momentum_grpc_pricing` (master switch),
//! `grpc_endpoint`, `grpc_token`, `pools_path` (pool metadata), and
//! `momentum_grpc_stale_secs` (staleness threshold).
//!
//! `WatchedToken` entries optionally carry `pool` (the on-chain pool pubkey, which must
//! also exist in pools.json) and `quote` ("USDC" or "SOL"). CP (raydium_amm_v4/saber)
//! pools are priced from vault reserves; CL pools (Orca Whirlpool, Raydium CLMM, Meteora
//! DLMM, Invariant) from their state account. Other DEX kinds fall back to REST.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use dashmap::DashMap;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Notify;
use tracing::info;

/// Shared live price map: token mint -> (USD price, last-update time). Written by the
/// gRPC ingestion task, read by the watcher each poll.
pub type GrpcPriceMap = Arc<DashMap<String, (f64, Instant)>>;

/// Per-mint REST divergence cross-check bookkeeping (trust-until-changed mode only,
/// i.e. `momentum_grpc_stale_secs == 0`). `last` is when this mint was last
/// REST-cross-checked; `distrusted` means the last check diverged beyond the
/// configured bps budget, so `select_prices` forces it back to REST until a fresh
/// on-chain write (`note_update`) or a later re-agreeing check clears it.
#[derive(Debug, Clone, Default)]
pub struct XcheckState {
    last: Option<Instant>,
    distrusted: bool,
}

/// Upward-spike detector parameters (Approach B latency accelerant). `threshold_bps` is
/// the minimum rise over the trailing `window` that fires an entry re-evaluation signal
/// for the spiking mint. Carried on `GrpcFeed` so the ingestion task can detect spikes
/// inline; `None` there means the feature is off.
#[derive(Debug, Clone, Copy)]
pub struct SpikeCfg {
    pub threshold_bps: f64,
    pub window: Duration,
}

/// Downward-crash exit detector parameters (`MOMENTUM_SPIKE_EXIT`). Fire on a HELD mint when
/// the price is `threshold_bps` below the confirmed high of the trailing `window`, once
/// `confirm_prints` breaching prints have arrived at least `confirm_gap` apart (prints closer
/// than that are one swap's burst — a CP pool emits two per swap, a Whirlpool three).
#[derive(Debug, Clone, Copy)]
pub struct CrashCfg {
    pub threshold_bps: f64,
    pub window: Duration,
    pub confirm_prints: u32,
    pub confirm_gap: Duration,
}

impl CrashCfg {
    /// The `.env` global detector config: `None` when the master switch is off (nothing runs for
    /// any token), else the four knobs as one struct. Shared by the feed boot and the exit leg so
    /// both sides start from the SAME global; `momentum::spike_exit_cfg_for` layers the per-token
    /// `momentum_tokens.json` overrides on top.
    pub fn global(
        enabled: bool,
        threshold_bps: f64,
        window_secs: u64,
        confirm_prints: u32,
        confirm_gap_ms: u64,
    ) -> Option<CrashCfg> {
        enabled.then(|| CrashCfg {
            threshold_bps,
            window: Duration::from_secs(window_secs),
            confirm_prints: confirm_prints.max(1),
            confirm_gap: Duration::from_millis(confirm_gap_ms),
        })
    }
}

/// Where a mint's crash bar came from — carried into the shadow audit so a would-exit can be
/// scored against the bar that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashBarSource {
    /// The `.env` `MOMENTUM_SPIKE_EXIT_BPS`.
    Global,
    /// A `momentum_tokens.json` `params.spike_exit_bps` / `spike_exit_window_secs` override.
    Static,
    /// The watcher's volatility-scaled bar (`MOMENTUM_SPIKE_EXIT_DYN_K`), layered over the base.
    Dynamic,
}

impl CrashBarSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CrashBarSource::Global => "global",
            CrashBarSource::Static => "static",
            CrashBarSource::Dynamic => "dynamic",
        }
    }
}

/// A standing, confirmed crash on a held mint. `at` is the first confirmation (the shadow
/// audit's once-per-signal key); `last` is the latest breaching print (the staleness clock —
/// a dead stream lets the signal expire). Removed the moment a print recovers above the line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CrashSignal {
    pub drop_bps: f64,
    pub window_high: f64,
    pub price: f64,
    pub prints: u32,
    pub at: Instant,
    pub last: Instant,
}

/// Shared handle bundle between the (binary-side) gRPC ingestion task and the
/// (lib-side) watcher loop. `map` carries live on-chain USD prices; `sol_usd` is the
/// latest SOL/USD (as `f64` bits) that the watcher publishes each poll so the ingestion
/// task can convert SOL-quoted pools to USD. `notify` and `held` wire the ingestion task
/// to wake the exit path when a held token's price updates. `xcheck` tracks per-mint
/// REST divergence cross-check state used by trust-until-changed mode. `impact` carries
/// the ingestion task's estimated price impact (bps) of a `MOMENTUM_TRADE_USDC`-sized
/// buy per mint, consumed by the entry path's local pre-gate (`MOMENTUM_LOCAL_IMPACT`).
#[derive(Clone)]
pub struct GrpcFeed {
    pub map: GrpcPriceMap,
    pub sol_usd: Arc<std::sync::atomic::AtomicU64>,
    pub notify: Arc<Notify>,
    pub held: Arc<RwLock<HashSet<String>>>,
    pub xcheck: Arc<RwLock<HashMap<String, XcheckState>>>,
    pub impact: Arc<DashMap<String, (u32, Instant)>>,
    /// Latest quote-side pool depth in USD per mint, published by the gRPC ingestion task
    /// from live CP vault reserves. Consumed by the liquidity-drain guard
    /// (`MOMENTUM_MAX_EXIT_IMPACT_BPS`) and the entry size cap
    /// (`MOMENTUM_MAX_ENTRY_IMPACT_BPS`). Unlike `impact` — which is a fixed
    /// `MOMENTUM_TRADE_USDC`-sized *buy* estimate — this is raw state, so the consumer can
    /// size the impact to the position it actually holds (a winner that doubled costs ~2x
    /// as much to exit). CP pools only; see `feed_setup::publish_depth`.
    pub depth: Arc<DashMap<String, (f64, Instant)>>,
    /// Upward-spike detector config (threshold bps + window). `None` = spike detection
    /// off; the entry-side signal never fires.
    pub spike_cfg: Option<SpikeCfg>,
    /// Downward-crash exit detector config. `None` = off; `crash_signal` always reads `None`.
    pub crash_cfg: Option<CrashCfg>,
    /// Per-mint crash-detector overrides from `momentum_tokens.json` `params` (`Some(cfg)` =
    /// detect this mint with that config, `None` = the mint is EXEMPT). Mints absent here use
    /// `crash_cfg`. Installed at setup (`set_crash_overrides`); shared, so a later push from the
    /// watcher reaches the ingestion task's clone too. Inert while `crash_cfg` is `None`.
    crash_overrides: Arc<DashMap<String, Option<CrashCfg>>>,
    /// Longest crash window in use (the global or any override), in ms — the crash detector's
    /// share of the rolling window's span. Recomputed by the setters; zero when the master is off.
    crash_window_max_ms: Arc<std::sync::atomic::AtomicU64>,
    /// Volatility-scaled per-mint bars (bps) pushed by the watcher each slow tick
    /// (`set_crash_bars`, replace semantics). Layered over the static/global base: the bar
    /// replaces the threshold, the base keeps its window. Never revives an exempt mint.
    crash_dynamic: Arc<DashMap<String, f64>>,
    /// Per-mint rolling price window `(sample_instant, usd)`, oldest-first, SHARED by the
    /// up-spike detector (non-held mints) and the crash detector (held mints); sized to the
    /// longer of the two windows (`window_span`). Populated only when either is enabled.
    spike_win: Arc<DashMap<String, VecDeque<(Instant, f64)>>>,
    /// Per-mint crash breach streak `(last_counted_print, count)` — see `advance_streak`.
    crash_streak: Arc<DashMap<String, (Instant, u32)>>,
    /// Standing confirmed crash signals per held mint; read by `maybe_exit` via `crash_signal`.
    crash: Arc<DashMap<String, CrashSignal>>,
    /// Sender half of the spiking-mint signal (ingestion task → watcher `select!` arm).
    /// A cloneable `UnboundedSender` so it rides along in `GrpcFeed`'s `Clone`. `None`
    /// when spike detection is off.
    spike_tx: Option<UnboundedSender<String>>,
    /// Receiver half, `.take()`n once by the watcher before its loop. Behind
    /// `Arc<Mutex<..>>` so the shared (cloned) `GrpcFeed` hands single ownership to the
    /// one consumer. The lock is held only for the one-shot `take()` — never across
    /// `.await` and never on the hot path.
    pub spike_rx: Arc<Mutex<Option<UnboundedReceiver<String>>>>,
}

impl GrpcFeed {
    pub fn new() -> Self {
        GrpcFeed {
            map: Arc::new(DashMap::new()),
            sol_usd: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notify: Arc::new(Notify::new()),
            held: Arc::new(RwLock::new(HashSet::new())),
            xcheck: Arc::new(RwLock::new(HashMap::new())),
            impact: Arc::new(DashMap::new()),
            depth: Arc::new(DashMap::new()),
            spike_cfg: None,
            crash_cfg: None,
            crash_overrides: Arc::new(DashMap::new()),
            crash_window_max_ms: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            crash_dynamic: Arc::new(DashMap::new()),
            spike_win: Arc::new(DashMap::new()),
            crash_streak: Arc::new(DashMap::new()),
            crash: Arc::new(DashMap::new()),
            spike_tx: None,
            spike_rx: Arc::new(Mutex::new(None)),
        }
    }

    /// Enable upward-spike detection: create the mint-signal channel and store the
    /// threshold/window. Called once by the ingestion setup when `MOMENTUM_SPIKE_ENTRY`
    /// is on, before the feed is cloned into the ingestion task — so the `Sender` (via
    /// `Clone`) reaches the ingestion side and the `Receiver` stays reachable through the
    /// shared `spike_rx` for the watcher to `.take()`.
    pub fn enable_spike(&mut self, threshold_bps: f64, window: Duration) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.spike_tx = Some(tx);
        if let Ok(mut slot) = self.spike_rx.lock() {
            *slot = Some(rx);
        }
        self.spike_cfg = Some(SpikeCfg { threshold_bps, window });
    }

    /// Enable the downward-crash exit detector. Called once at feed setup when
    /// `MOMENTUM_SPIKE_EXIT` is on (shadow included — detection runs, the exit leg decides).
    pub fn enable_crash_exit(&mut self, cfg: CrashCfg) {
        self.crash_cfg = Some(cfg);
        self.recompute_crash_span();
    }

    /// Install the per-mint overrides (see `crash_overrides`; built by
    /// `momentum::crash_overrides_for` from the watched tokens' `params`). REPLACES the previous
    /// map, so a token whose override was removed falls back to the global. Shared state, so it
    /// may also be called after the feed was cloned into the ingestion task.
    pub fn set_crash_overrides(&self, overrides: HashMap<String, Option<CrashCfg>>) {
        self.crash_overrides.clear();
        for (mint, o) in overrides {
            self.crash_overrides.insert(mint, o);
        }
        self.recompute_crash_span();
    }

    /// Replace the volatility-scaled bars (bps per mint) — the watcher pushes the full held set
    /// each slow tick, so a mint it no longer sizes (exited, or its history shrank below the
    /// warm-up) falls back to its static/global bar on the next push. Non-finite or non-positive
    /// values are dropped, never installed.
    pub fn set_crash_bars(&self, bars: HashMap<String, f64>) {
        self.crash_dynamic.clear();
        for (mint, bps) in bars {
            if bps.is_finite() && bps > 0.0 {
                self.crash_dynamic.insert(mint, bps);
            }
        }
    }

    fn recompute_crash_span(&self) {
        let ms = match self.crash_cfg {
            Some(g) => self
                .crash_overrides
                .iter()
                .filter_map(|e| e.value().map(|c| c.window))
                .fold(g.window, Duration::max)
                .as_millis() as u64,
            None => 0,
        };
        self.crash_window_max_ms.store(ms, std::sync::atomic::Ordering::Relaxed);
    }

    /// The crash config in force for `mint` and where its bar came from: `None` when the master
    /// is off or the mint is exempt; otherwise the params override (`Static`) or the global
    /// (`Global`) as the base, with a pushed volatility-scaled bar replacing the base's threshold
    /// (`Dynamic`) — the base keeps its window. Read by the detector on every print and by the
    /// exit leg, so both judge a token at the same number.
    pub fn crash_resolution(&self, mint: &str) -> Option<(CrashCfg, CrashBarSource)> {
        let global = self.crash_cfg?;
        let (base, src) = match self.crash_overrides.get(mint).map(|e| *e.value()) {
            Some(None) => return None, // exempt
            Some(Some(c)) => (c, CrashBarSource::Static),
            None => (global, CrashBarSource::Global),
        };
        match self.crash_dynamic.get(mint).map(|e| *e.value()) {
            Some(bps) => Some((CrashCfg { threshold_bps: bps, ..base }, CrashBarSource::Dynamic)),
            None => Some((base, src)),
        }
    }

    /// `crash_resolution` without the source.
    pub fn crash_cfg_for(&self, mint: &str) -> Option<CrashCfg> {
        self.crash_resolution(mint).map(|(c, _)| c)
    }

    /// Length of the shared rolling window: the longer of the enabled detectors' windows (the
    /// crash side counts its longest per-mint override), `None` when both are off (no per-print
    /// bookkeeping at all).
    pub fn window_span(&self) -> Option<Duration> {
        let crash = self.crash_cfg.map(|_| {
            Duration::from_millis(self.crash_window_max_ms.load(std::sync::atomic::Ordering::Relaxed))
        });
        match (self.spike_cfg.map(|c| c.window), crash) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    /// Record a fresh on-chain price for `mint` and run both detectors on the shared window:
    /// an UPWARD spike on a NON-held mint signals the entry path (`spike_tx`); a DOWNWARD crash
    /// on a HELD mint advances that mint's breach streak and, once confirmed, publishes a
    /// `CrashSignal` for `maybe_exit`. A recovering print clears streak and signal. No-op when
    /// neither detector is enabled — the sub-second hot path stays free when the features are
    /// off. Only live stream writes should reach here (see `feed_setup::apply_update`).
    pub fn note_print(&self, mint: &str, usd: f64) {
        self.note_print_at(mint, usd, Instant::now());
    }

    /// Alias kept for the entry-side name; identical to `note_print`.
    pub fn note_spike(&self, mint: &str, usd: f64) {
        self.note_print(mint, usd);
    }

    /// `note_print` with an explicit clock (unit tests drive the burst/gap timing).
    pub(crate) fn note_print_at(&self, mint: &str, usd: f64, now: Instant) {
        let Some(span) = self.window_span() else { return };
        if !usd.is_finite() || usd <= 0.0 {
            return;
        }
        // Held-set read is a temporary taken BEFORE the window guard; never nested with it.
        let held = self.held.read().map(|h| h.contains(mint)).unwrap_or(false);
        let up = if held { None } else { self.spike_cfg.zip(self.spike_tx.as_ref()) };
        let down = if held { self.crash_cfg_for(mint) } else { None };
        if up.is_none() && down.is_none() {
            // Nothing to detect for this mint, but keep its window warm so a later transition
            // (entry ⇒ held, or exit ⇒ un-held) does not start from an empty history.
            let mut win = self.spike_win.entry(mint.to_string()).or_default();
            Self::evict(&mut win, now, span);
            win.push_back((now, usd));
            return;
        }
        let span_ms = span.as_millis() as u64;
        // Brief per-shard write lock (the only lock on this path): evict stale front samples,
        // snapshot the prior samples onto a (ms, price) timeline where "now" sits at
        // `span_ms`, run the pure detectors, then append this sample. One touch of
        // `spike_win` per call — re-entering the same shard would deadlock.
        let (spike, fall) = {
            let mut win = self.spike_win.entry(mint.to_string()).or_default();
            Self::evict(&mut win, now, span);
            let prev: Vec<(u64, f64)> = win
                .iter()
                .map(|(si, p)| {
                    let age_ms = now.saturating_duration_since(*si).as_millis() as u64;
                    (span_ms.saturating_sub(age_ms), *p)
                })
                .collect();
            let spike = up.and_then(|(cfg, _)| {
                detect_spike_bps(&prev, span_ms, usd, cfg.window.as_millis() as u64, cfg.threshold_bps)
            });
            let fall = down.and_then(|cfg| {
                detect_drop_bps(
                    &prev,
                    span_ms,
                    usd,
                    cfg.window.as_millis() as u64,
                    cfg.threshold_bps,
                    cfg.confirm_gap.as_millis() as u64,
                )
            });
            win.push_back((now, usd));
            (spike, fall)
        };
        if let (Some(bps), Some((cfg, tx))) = (spike, up) {
            // Unbounded, non-blocking: a dropped signal under a storm is harmless (spike
            // re-eval is idempotent; the 60s tick and later spikes still cover the token).
            let _ = tx.send(mint.to_string());
            info!("gRPC: SPIKE {} +{:.0}bps/{}s", mint, bps, cfg.window.as_secs());
        }
        if let Some(cfg) = down {
            let prev = self.crash_streak.get(mint).map(|e| *e.value());
            match advance_streak(prev, fall.is_some(), now, cfg.confirm_gap) {
                None => {
                    // Recovered above the line (or never breached): streak and signal both go.
                    self.crash_streak.remove(mint);
                    self.crash.remove(mint);
                }
                Some(st) => {
                    self.crash_streak.insert(mint.to_string(), st);
                    if let (true, Some((bps, high))) = (st.1 >= cfg.confirm_prints, fall) {
                        let first = !self.crash.contains_key(mint);
                        let mut e = self.crash.entry(mint.to_string()).or_insert(CrashSignal {
                            drop_bps: bps,
                            window_high: high,
                            price: usd,
                            prints: st.1,
                            at: now,
                            last: now,
                        });
                        e.drop_bps = bps;
                        e.window_high = high;
                        e.price = usd;
                        e.prints = st.1;
                        e.last = now; // `at` keeps the first confirmation
                        drop(e);
                        if first {
                            info!(
                                "gRPC: CRASH {} -{:.0}bps/{}s (high {:.6} → {:.6}, {} spaced prints)",
                                mint, bps, cfg.window.as_secs(), high, usd, st.1
                            );
                        }
                    }
                }
            }
        }
    }

    fn evict(win: &mut VecDeque<(Instant, f64)>, now: Instant, span: Duration) {
        while let Some(&(si, _)) = win.front() {
            if now.saturating_duration_since(si) > span {
                win.pop_front();
            } else {
                break;
            }
        }
    }

    /// The standing crash signal for `mint`, if its last breaching print is younger than
    /// `max_age`. Absent/stale ⇒ `None` ⇒ the exit leg fails OPEN.
    pub fn crash_signal(&self, mint: &str, max_age: Duration) -> Option<CrashSignal> {
        self.crash_signal_at(mint, max_age, Instant::now())
    }

    pub(crate) fn crash_signal_at(&self, mint: &str, max_age: Duration, now: Instant) -> Option<CrashSignal> {
        let sig = *self.crash.get(mint)?.value();
        (now.saturating_duration_since(sig.last) <= max_age).then_some(sig)
    }

    #[cfg(test)]
    pub(crate) fn window_len(&self, mint: &str) -> usize {
        self.spike_win.get(mint).map(|w| w.len()).unwrap_or(0)
    }

    /// Latest published SOL/USD (0.0 until the watcher publishes its first price).
    pub fn sol_usd(&self) -> f64 {
        f64::from_bits(self.sol_usd.load(std::sync::atomic::Ordering::Relaxed))
    }
    /// Publish the latest SOL/USD for the ingestion task's SOL-quote conversion.
    pub fn publish_sol_usd(&self, usd: f64) {
        self.sol_usd.store(usd.to_bits(), std::sync::atomic::Ordering::Relaxed);
    }
    /// Replace the held-mint set the ingestion task uses to decide when to wake the exit path.
    /// A mint that just became held (a fresh entry) gets a FRESH crash window: its pre-entry
    /// prints — and the fill's own — must not seed the high a crash is measured from. The
    /// write lock is released before the DashMaps are touched (never nested).
    pub fn set_held(&self, mints: impl IntoIterator<Item = String>) {
        let new: HashSet<String> = mints.into_iter().collect();
        let newly: Vec<String> = match self.held.write() {
            Ok(mut h) => {
                let newly = new.difference(&h).cloned().collect();
                *h = new;
                newly
            }
            Err(_) => Vec::new(),
        };
        for m in newly {
            self.spike_win.remove(&m);
            self.crash_streak.remove(&m);
            self.crash.remove(&m);
        }
    }
    /// Called by the ingestion task after storing a price: wake the exit path iff the
    /// updated mint is currently held (cheap read-lock; no-op otherwise), and clear any
    /// standing distrust for the mint — a fresh on-chain write re-earns trust.
    pub fn note_update(&self, mint: &str) {
        if self.held.read().map(|h| h.contains(mint)).unwrap_or(false) {
            self.notify.notify_one();
        }
        if let Ok(mut x) = self.xcheck.write() {
            if let Some(s) = x.get_mut(mint) {
                s.distrusted = false;
            }
        }
    }
    /// Snapshot of mints currently distrusted by the REST divergence cross-check —
    /// `select_prices` forces these to REST regardless of gRPC freshness.
    pub fn distrusted_snapshot(&self) -> HashSet<String> {
        self.xcheck
            .read()
            .map(|x| x.iter().filter(|(_, s)| s.distrusted).map(|(m, _)| m.clone()).collect())
            .unwrap_or_default()
    }
    /// Whether `mint` is due for a periodic REST cross-check: never checked before, or
    /// `every` has elapsed since the last one.
    pub fn xcheck_due(&self, mint: &str, every: Duration, now: Instant) -> bool {
        self.xcheck
            .read()
            .ok()
            .and_then(|x| x.get(mint).and_then(|s| s.last))
            .map(|last| now.duration_since(last) >= every)
            .unwrap_or(true)
    }
    /// Record the outcome of a REST cross-check for `mint`: `ok=false` distrusts it
    /// (forced to REST until a fresh on-chain write or a later re-agreeing check).
    pub fn record_xcheck(&self, mint: &str, ok: bool, now: Instant) {
        if let Ok(mut x) = self.xcheck.write() {
            let s = x.entry(mint.to_string()).or_default();
            s.last = Some(now);
            s.distrusted = !ok;
        }
    }
    /// Publish the estimated price impact (bps) of a `MOMENTUM_TRADE_USDC`-sized buy for
    /// `mint`, computed by the gRPC ingestion task from live pool state. Consumed by the
    /// entry path's local pre-gate (`MOMENTUM_LOCAL_IMPACT`) to skip an obviously-doomed
    /// candidate without spending a Jupiter REST quote.
    pub fn publish_impact(&self, mint: &str, bps: u32) {
        self.impact.insert(mint.to_string(), (bps, Instant::now()));
    }
    /// Latest published impact estimate (bps) for `mint`, if present and updated within
    /// `max_age`. `None` (absent or stale) means "no fresh estimate" — callers must skip
    /// the pre-gate rather than block the trade on missing data.
    pub fn est_impact_bps(&self, mint: &str, max_age: Duration) -> Option<u32> {
        self.impact.get(mint).and_then(|e| {
            let (bps, ts) = *e.value();
            (ts.elapsed() <= max_age).then_some(bps)
        })
    }
    /// Publish the quote-side pool depth (USD) for `mint`, computed by the gRPC ingestion
    /// task from live CP vault reserves. Non-finite or non-positive values are dropped
    /// rather than stored, so a consumer never reads a degenerate depth.
    pub fn publish_depth(&self, mint: &str, usd: f64) {
        if usd.is_finite() && usd > 0.0 {
            self.depth.insert(mint.to_string(), (usd, Instant::now()));
        }
    }
    /// Latest quote-side depth (USD) for `mint`, if present and updated within `max_age`.
    /// `None` (absent, stale, or never published for this pool kind) means "no fresh
    /// depth" — every caller must fail OPEN on `None`, never block or exit a trade on
    /// missing data.
    pub fn quote_depth_usd(&self, mint: &str, max_age: Duration) -> Option<f64> {
        self.depth.get(mint).and_then(|e| {
            let (usd, ts) = *e.value();
            (ts.elapsed() <= max_age).then_some(usd)
        })
    }
}

impl Default for GrpcFeed {
    fn default() -> Self { Self::new() }
}

/// Split the watched mints into (fresh gRPC prices to use, mints that still need REST).
/// A gRPC entry is used only if it is present, positive, not distrusted, and — unless
/// `stale` is `Duration::ZERO` (trust-until-changed: an AMM price cannot move without an
/// account write, so age alone never demotes it) — updated within `stale`. A mint in
/// `distrusted` always goes to `to_rest` regardless of freshness (REST divergence
/// cross-check covers a dead stream or a price that migrated venues).
pub fn select_prices(
    map: &GrpcPriceMap,
    watched_mints: &[String],
    stale: Duration,
    now: Instant,
    distrusted: &HashSet<String>,
) -> (HashMap<String, f64>, Vec<String>) {
    let mut use_grpc = HashMap::new();
    let mut to_rest = Vec::new();
    for m in watched_mints {
        match map.get(m) {
            Some(e) if !distrusted.contains(m)
                && (stale.is_zero() || now.duration_since(e.value().1) <= stale)
                && e.value().0 > 0.0 => { use_grpc.insert(m.clone(), e.value().0); }
            _ => to_rest.push(m.clone()),
        }
    }
    (use_grpc, to_rest)
}

/// Detect an UPWARD price spike: the bps rise of the current `price` over the lowest
/// baseline sample seen within the trailing `window_ms`. `prev` is the per-mint sample
/// history as `(unix_millis, price)`, oldest-first, EXCLUDING the current
/// `(now_ms, price)` sample. Returns `Some(bps)` when `price` is at least
/// `threshold_bps` above the minimum in-window baseline, else `None`.
///
/// Upward-only (a flat or falling move never fires). The baseline is the *minimum*
/// in-window price (not the oldest) so a fast rise off a recent trough is caught even
/// when the immediately-preceding tick was already elevated. Using `u64` millis + `f64`
/// (rather than `Instant`) keeps this pure and unit-testable with hand-built vectors;
/// the caller converts monotonic `Instant`s to millis before calling.
fn detect_spike_bps(
    prev: &[(u64, f64)],
    now_ms: u64,
    price: f64,
    window_ms: u64,
    threshold_bps: f64,
) -> Option<f64> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let cutoff = now_ms.saturating_sub(window_ms);
    // Spike baseline = the lowest positive price among prior samples still inside the
    // window. `fold(INFINITY, min)` yields INFINITY when nothing is in-window.
    let baseline = prev
        .iter()
        .filter(|(ts, p)| *ts >= cutoff && p.is_finite() && *p > 0.0)
        .map(|(_, p)| *p)
        .fold(f64::INFINITY, f64::min);
    if !baseline.is_finite() || baseline <= 0.0 {
        return None; // no in-window baseline to compare against
    }
    let bps = (price / baseline - 1.0) * 10_000.0;
    (bps >= threshold_bps).then_some(bps)
}

/// The highest price level held by TWO prior samples at least `gap_ms` apart inside the
/// window (samples at/after `cutoff_ms`): the level confirmed at sample `j` is
/// `min(p_j, max{p_i : t_i + gap ≤ t_j})`, and the result is the max over `j`. A single-burst
/// up-wick (the 2–3 prints one swap emits within a slot) can therefore never become the
/// baseline a crash is measured from. `gap_ms == 0` degenerates to the raw in-window maximum.
/// `prev` is oldest-first (the ingestion order). `None` without such a pair.
fn confirmed_high(prev: &[(u64, f64)], cutoff_ms: u64, gap_ms: u64) -> Option<f64> {
    let pts: Vec<(u64, f64)> = prev
        .iter()
        .copied()
        .filter(|(ts, p)| *ts >= cutoff_ms && p.is_finite() && *p > 0.0)
        .collect();
    let mut best = f64::NEG_INFINITY;
    let mut prefix_max = f64::NEG_INFINITY;
    let mut i = 0usize;
    for &(tj, pj) in &pts {
        while i < pts.len() && pts[i].0.saturating_add(gap_ms) <= tj {
            prefix_max = prefix_max.max(pts[i].1);
            i += 1;
        }
        if prefix_max.is_finite() {
            best = best.max(pj.min(prefix_max));
        }
    }
    best.is_finite().then_some(best)
}

/// Detect a DOWNWARD crash: the bps fall of the current `price` below the confirmed high of
/// the trailing `window_ms` (see `confirmed_high`). Same conventions as `detect_spike_bps`
/// (`prev` excludes the current sample; `u64` millis keep it pure). Returns
/// `Some((drop_bps, high))` when the fall is at least `threshold_bps`, else `None`.
fn detect_drop_bps(
    prev: &[(u64, f64)],
    now_ms: u64,
    price: f64,
    window_ms: u64,
    threshold_bps: f64,
    gap_ms: u64,
) -> Option<(f64, f64)> {
    if !price.is_finite() || price <= 0.0 {
        return None;
    }
    let cutoff = now_ms.saturating_sub(window_ms);
    let high = confirmed_high(prev, cutoff, gap_ms)?;
    let bps = (1.0 - price / high) * 10_000.0;
    (bps >= threshold_bps).then_some((bps, high))
}

/// Breach-streak step for the crash exit's "N consecutive prints" confirmation. A breaching
/// print counts only when it arrives at least `gap` after the last COUNTED one (prints inside
/// one burst are one observation); a non-breaching print clears the streak. The tuple is
/// `(last_counted_at, count)`.
pub(crate) fn advance_streak(
    prev: Option<(Instant, u32)>,
    breach: bool,
    now: Instant,
    gap: Duration,
) -> Option<(Instant, u32)> {
    if !breach {
        return None;
    }
    match prev {
        None => Some((now, 1)),
        Some((last, n)) => {
            if now.saturating_duration_since(last) >= gap {
                Some((now, n + 1))
            } else {
                Some((last, n))
            }
        }
    }
}

// PoolState from dex::types is available at the binary level (main.rs).
// For test context, we provide a mock that matches the real PoolState API.
#[cfg(test)]
mod pool_state_for_tests {
    /// Test double for dex::types::PoolState, matching its exact enum structure.
    #[derive(Debug, Clone)]
    pub enum PoolState {
        ConstantProduct {
            reserve_a: u64,
            reserve_b: u64,
            fee_bps: u64,
        },
        ConcentratedLiquidity {
            sqrt_price_x64: u128,
            _liquidity: u128,
            fee_bps: u64,
        },
    }

    impl PoolState {
        pub fn rate_a_to_b(&self) -> f64 {
            match self {
                Self::ConstantProduct { reserve_a, reserve_b, fee_bps } => {
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    (*reserve_b as f64 / *reserve_a as f64) * fee
                }
                Self::ConcentratedLiquidity { sqrt_price_x64, fee_bps, .. } => {
                    let sqrt_price = *sqrt_price_x64 as f64 / (1u128 << 64) as f64;
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    sqrt_price * sqrt_price * fee
                }
            }
        }

        pub fn rate_b_to_a(&self) -> f64 {
            match self {
                Self::ConstantProduct { reserve_a, reserve_b, fee_bps } => {
                    if *reserve_b == 0 { return 0.0; }
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    (*reserve_a as f64 / *reserve_b as f64) * fee
                }
                Self::ConcentratedLiquidity { sqrt_price_x64, fee_bps, .. } => {
                    let sqrt_price = *sqrt_price_x64 as f64 / (1u128 << 64) as f64;
                    if sqrt_price == 0.0 { return 0.0; }
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    fee / (sqrt_price * sqrt_price)
                }
            }
        }
    }
}

#[cfg(test)]
use pool_state_for_tests::PoolState;

/// Trait for types that provide pool exchange rates.
/// Implemented by PoolState in both test and production contexts.
pub trait PoolRates {
    fn rate_a_to_b(&self) -> f64;
    fn rate_b_to_a(&self) -> f64;
}

#[cfg(test)]
impl PoolRates for PoolState {
    fn rate_a_to_b(&self) -> f64 {
        PoolState::rate_a_to_b(self)
    }
    fn rate_b_to_a(&self) -> f64 {
        PoolState::rate_b_to_a(self)
    }
}

/// Convert an atomic quote-per-momentum rate to a USD price. Shared by the CP path
/// (rate from PoolState) and the CL path (rate from parse_cl_pool_state's price).
pub fn rate_to_usd(
    raw_rate: f64,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    sol_usd: f64,
) -> Option<f64> {
    if !raw_rate.is_finite() || raw_rate <= 0.0 {
        return None;
    }
    let price_in_quote = raw_rate * 10f64.powi(dec_momentum as i32 - dec_quote as i32);
    let usd = if quote_is_usdc { price_in_quote } else { price_in_quote * sol_usd };
    if usd.is_finite() && usd > 0.0 { Some(usd) } else { None }
}

/// USD price of the momentum token from current pool state.
/// `rate_a_to_b`/`rate_b_to_a` are atomic-unit rates (quote-atomic per momentum-atomic),
/// so we convert to human units with 10^(dec_momentum - dec_quote), then to USD
/// (quote=USDC → identity; quote=SOL → × sol_usd). Returns None on degenerate state.
pub fn price_usd(
    state: &dyn PoolRates,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    sol_usd: f64,
) -> Option<f64> {
    let raw = if momentum_is_token_a { state.rate_a_to_b() } else { state.rate_b_to_a() };
    rate_to_usd(raw, dec_momentum, dec_quote, quote_is_usdc, sol_usd)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Constant-product, momentum=token_a, quote=token_b=USDC, equal decimals (6/6).
    // reserve_b/reserve_a = 200/100 = 2.0 (fee 0 for simplicity via fee_bps=0).
    #[test]
    fn cp_usdc_quote_equal_decimals() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 6, 6, true, 0.0).unwrap();
        assert!((p - 2.0).abs() < 1e-9);
    }

    // SOL quote: price_in_sol × sol_usd. reserveB/reserveA=2.0 SOL per token, SOL=$150 → $300.
    #[test]
    fn cp_sol_quote_applies_sol_usd() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 9, 9, false, 150.0).unwrap();
        assert!((p - 300.0).abs() < 1e-6);
    }

    // Decimal adjustment: momentum has 6 dp, quote(USDC) 6 dp already covered;
    // here momentum=token_a 9dp, quote=token_b 6dp → ×10^(9-6)=1000.
    #[test]
    fn decimal_adjustment_scales_price() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 9, 6, true, 0.0).unwrap();
        assert!((p - 2000.0).abs() < 1e-6); // 2.0 × 10^3
    }

    // momentum=token_b path uses rate_b_to_a.
    #[test]
    fn momentum_is_token_b_uses_inverse_rate() {
        let s = PoolState::ConstantProduct { reserve_a: 200, reserve_b: 100, fee_bps: 0 };
        // momentum=token_b, quote=token_a=USDC, equal dp: rate_b_to_a = reserve_a/reserve_b = 2.0
        let p = price_usd(&s as &dyn PoolRates, false, 6, 6, true, 0.0).unwrap();
        assert!((p - 2.0).abs() < 1e-9);
    }

    // Degenerate input → None (not a panic, not a zero price).
    #[test]
    fn zero_reserves_returns_none() {
        let s = PoolState::ConstantProduct { reserve_a: 0, reserve_b: 200, fee_bps: 0 };
        assert!(price_usd(&s as &dyn PoolRates, true, 6, 6, true, 0.0).is_none());
    }

    #[test]
    fn cl_pool_uses_sqrt_price() {
        // sqrt_price_x64 = 2^64 → price = 1.0; equal dp, USDC quote → $1.0
        let s = PoolState::ConcentratedLiquidity { sqrt_price_x64: 1u128 << 64, _liquidity: 0, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 6, 6, true, 0.0).unwrap();
        assert!((p - 1.0).abs() < 1e-9);
    }

    #[test]
    fn select_prices_prefers_fresh_grpc_rest_fills_rest() {
        let map: GrpcPriceMap = Arc::new(DashMap::new());
        let now = Instant::now();
        map.insert("FRESH".into(), (1.23, now));                          // age 0
        map.insert("STALE".into(), (9.99, now - Duration::from_secs(120))); // too old
        // "MISS" absent from the map
        let watched = vec!["FRESH".to_string(), "STALE".to_string(), "MISS".to_string()];
        let (use_grpc, to_rest) = select_prices(&map, &watched, Duration::from_secs(30), now, &HashSet::new());
        assert_eq!(use_grpc.get("FRESH"), Some(&1.23));
        assert!(!use_grpc.contains_key("STALE") && !use_grpc.contains_key("MISS"));
        let mut rest = to_rest.clone();
        rest.sort();
        assert_eq!(rest, vec!["MISS".to_string(), "STALE".to_string()]);
    }

    #[test]
    fn select_prices_zero_stale_trusts_forever_but_respects_distrust() {
        let map: GrpcPriceMap = Arc::new(DashMap::new());
        let now = Instant::now();
        map.insert("OLD".into(), (1.0, now - Duration::from_secs(100_000)));
        map.insert("BAD".into(), (2.0, now));
        let watched = vec!["OLD".to_string(), "BAD".to_string()];
        let distrusted: HashSet<String> = ["BAD".to_string()].into();
        let (use_grpc, to_rest) = select_prices(&map, &watched, Duration::ZERO, now, &distrusted);
        assert_eq!(use_grpc.get("OLD"), Some(&1.0)); // age irrelevant in trust mode
        assert_eq!(to_rest, vec!["BAD".to_string()]); // distrust forces REST
    }

    #[test]
    fn xcheck_due_and_record_lifecycle() {
        let feed = GrpcFeed::new();
        let now = Instant::now();
        let every = Duration::from_secs(300);
        assert!(feed.xcheck_due("M", every, now));            // never checked → due
        feed.record_xcheck("M", true, now);
        assert!(!feed.xcheck_due("M", every, now));            // just checked → not due
        assert!(feed.xcheck_due("M", every, now + Duration::from_secs(301)));
        feed.record_xcheck("M", false, now);                   // diverged
        assert!(feed.distrusted_snapshot().contains("M"));
        feed.note_update("M");                                 // fresh write clears distrust
        assert!(!feed.distrusted_snapshot().contains("M"));
    }

    #[test]
    fn rate_to_usd_cl_style_both_orientations() {
        // raw a->b rate 2.0 (atomic b per atomic a), equal dp, USDC → $2.0
        assert!((rate_to_usd(2.0, 6, 6, true, 0.0).unwrap() - 2.0).abs() < 1e-9);
        // momentum=token_b uses 1/price at the call site; here just the inverse rate 0.5 → $0.5
        assert!((rate_to_usd(0.5, 6, 6, true, 0.0).unwrap() - 0.5).abs() < 1e-9);
        // SOL quote: 2.0 * sol_usd(150) = 300
        assert!((rate_to_usd(2.0, 9, 9, false, 150.0).unwrap() - 300.0).abs() < 1e-6);
        // decimal scale 10^(9-6)=1000
        assert!((rate_to_usd(2.0, 9, 6, true, 0.0).unwrap() - 2000.0).abs() < 1e-6);
        // degenerate
        assert!(rate_to_usd(0.0, 6, 6, true, 0.0).is_none());
        assert!(rate_to_usd(f64::INFINITY, 6, 6, true, 0.0).is_none());
    }

    // Task 5 (local impact pre-gate): GrpcFeed.impact accessors — insert → readable,
    // stale age → None, absent mint → None.
    #[test]
    fn impact_publish_then_read_respects_freshness() {
        let feed = GrpcFeed::new();
        feed.publish_impact("TOK", 480);
        // Fresh publish is readable within a generous max_age.
        assert_eq!(feed.est_impact_bps("TOK", Duration::from_secs(120)), Some(480));
        // Absent mint reads as None.
        assert_eq!(feed.est_impact_bps("MISSING", Duration::from_secs(120)), None);
    }

    #[test]
    fn est_impact_bps_stale_reads_as_none() {
        let feed = GrpcFeed::new();
        feed.impact.insert("OLD".to_string(), (999, Instant::now() - Duration::from_secs(200)));
        assert_eq!(feed.est_impact_bps("OLD", Duration::from_secs(120)), None);
    }

    // ---- quote-side depth (liquidity drain guard) -----------------------------------

    #[test]
    fn publish_and_read_quote_depth() {
        let feed = GrpcFeed::new();
        feed.publish_depth("TOK", 412_391.0);
        assert_eq!(feed.quote_depth_usd("TOK", Duration::from_secs(120)), Some(412_391.0));
        assert_eq!(feed.quote_depth_usd("MISSING", Duration::from_secs(120)), None);
    }

    #[test]
    fn quote_depth_stale_reads_as_none() {
        let feed = GrpcFeed::new();
        feed.depth.insert("OLD".to_string(), (1000.0, Instant::now() - Duration::from_secs(200)));
        assert_eq!(feed.quote_depth_usd("OLD", Duration::from_secs(120)), None);
    }

    /// A degenerate depth must never reach a consumer: publishing it is dropped, so the
    /// guard reads `None` and fails open rather than acting on a zero/NaN pool.
    #[test]
    fn publish_depth_rejects_degenerate_values() {
        let feed = GrpcFeed::new();
        feed.publish_depth("Z", 0.0);
        feed.publish_depth("N", f64::NAN);
        feed.publish_depth("I", f64::INFINITY);
        feed.publish_depth("M", -5.0);
        for m in ["Z", "N", "I", "M"] {
            assert_eq!(feed.quote_depth_usd(m, Duration::from_secs(120)), None, "mint {m}");
        }
    }

    // ---- detect_spike_bps (Task 1) --------------------------------------------------

    #[test]
    fn spike_single_jump_above_threshold_fires() {
        // +200bps in one tick, well above the 100bps threshold.
        let prev = [(0u64, 100.0)];
        let bps = detect_spike_bps(&prev, 1_000, 102.0, 5_000, 100.0).unwrap();
        assert!((bps - 200.0).abs() < 1e-6);
    }

    #[test]
    fn spike_below_threshold_is_none() {
        // +50bps < 100bps threshold.
        let prev = [(0u64, 100.0)];
        assert!(detect_spike_bps(&prev, 1_000, 100.5, 5_000, 100.0).is_none());
    }

    #[test]
    fn spike_cumulative_ticks_within_window_fire_off_min_baseline() {
        // Several small rises; baseline is the in-window MINIMUM (100), so +200bps fires.
        let prev = [(0u64, 100.0), (1_000, 100.5), (2_000, 101.0)];
        let bps = detect_spike_bps(&prev, 3_000, 102.0, 5_000, 100.0).unwrap();
        assert!((bps - 200.0).abs() < 1e-6);
    }

    #[test]
    fn spike_evicts_out_of_window_baseline() {
        // The low 100.0 print is OUTSIDE the 2s window (cutoff = 3000), so the baseline
        // is 101.5 and the rise (~98.5bps) falls short — proves windowing/eviction.
        let prev = [(0u64, 100.0), (4_000, 101.5), (4_500, 101.8)];
        assert!(detect_spike_bps(&prev, 5_000, 102.5, 2_000, 100.0).is_none());
        // Same samples with a wide-enough window DO see the 100.0 baseline → fires.
        assert!(detect_spike_bps(&prev, 5_000, 102.5, 6_000, 100.0).is_some());
    }

    #[test]
    fn spike_flat_and_descending_never_fire() {
        let flat = [(0u64, 100.0), (1_000, 100.0)];
        assert!(detect_spike_bps(&flat, 2_000, 100.0, 5_000, 100.0).is_none());
        let up_then_now_down = [(0u64, 105.0)];
        assert!(detect_spike_bps(&up_then_now_down, 1_000, 100.0, 5_000, 100.0).is_none());
    }

    #[test]
    fn spike_no_in_window_baseline_is_none() {
        // Only prior sample is older than the window → nothing to compare against.
        let prev = [(0u64, 100.0)];
        assert!(detect_spike_bps(&prev, 10_000, 200.0, 1_000, 100.0).is_none());
    }

    #[test]
    fn spike_ignores_nonpositive_and_nonfinite_baselines() {
        // Zero and NaN prior prices are filtered; baseline falls back to the valid 100.0.
        let prev = [(0u64, 0.0), (500, f64::NAN), (1_000, 100.0)];
        let bps = detect_spike_bps(&prev, 1_500, 110.0, 5_000, 100.0).unwrap();
        assert!((bps - 1_000.0).abs() < 1e-6);
    }

    #[test]
    fn spike_exact_boundary_fires() {
        // Exactly +100bps meets the >= threshold.
        let prev = [(0u64, 100.0)];
        let bps = detect_spike_bps(&prev, 1_000, 101.0, 5_000, 100.0).unwrap();
        assert!((bps - 100.0).abs() < 1e-6);
    }

    #[test]
    fn spike_nonpositive_current_price_is_none() {
        let prev = [(0u64, 100.0)];
        assert!(detect_spike_bps(&prev, 1_000, 0.0, 5_000, 100.0).is_none());
        assert!(detect_spike_bps(&prev, 1_000, f64::NAN, 5_000, 100.0).is_none());
    }

    // ---- note_spike plumbing (Task 2) -----------------------------------------------

    #[test]
    fn note_spike_signals_on_upward_jump_when_enabled() {
        let mut feed = GrpcFeed::new();
        feed.enable_spike(100.0, Duration::from_secs(5));
        feed.note_spike("TOK", 100.0); // baseline — no prior in-window sample, no fire
        feed.note_spike("TOK", 102.0); // +200bps over baseline → fires
        let mut rx = feed.spike_rx.lock().unwrap().take().unwrap();
        assert_eq!(rx.try_recv().ok(), Some("TOK".to_string()));
        assert!(rx.try_recv().is_err()); // exactly one signal
    }

    #[test]
    fn note_spike_is_inert_when_disabled() {
        let feed = GrpcFeed::new(); // spike_cfg / spike_tx both None
        feed.note_spike("TOK", 100.0);
        feed.note_spike("TOK", 200.0);
        assert!(feed.spike_rx.lock().unwrap().is_none()); // no channel ever created
    }

    #[test]
    fn note_spike_skips_held_mints() {
        let mut feed = GrpcFeed::new();
        feed.enable_spike(100.0, Duration::from_secs(5));
        feed.set_held(["TOK".to_string()]);
        feed.note_spike("TOK", 100.0);
        feed.note_spike("TOK", 500.0); // large jump, but held → managed by exit path
        let mut rx = feed.spike_rx.lock().unwrap().take().unwrap();
        assert!(rx.try_recv().is_err());
    }

    // ---- detect_drop_bps / confirmed_high / advance_streak (spike-crash exit) --------

    const GAP: u64 = 400; // ms — one slot; prints closer than this are one burst

    #[test]
    fn drop_single_print_above_threshold_fires() {
        // Two prints 500 ms apart confirm the 100.0 high; a 6% flush clears a 5% threshold.
        let prev = [(0u64, 100.0), (500, 100.0)];
        let (bps, high) = detect_drop_bps(&prev, 1_000, 94.0, 60_000, 500.0, GAP).unwrap();
        assert!((bps - 600.0).abs() < 1e-6);
        assert_eq!(high, 100.0);
    }

    #[test]
    fn drop_below_threshold_is_none() {
        let prev = [(0u64, 100.0), (500, 100.0)];
        assert!(detect_drop_bps(&prev, 1_000, 96.0, 60_000, 500.0, GAP).is_none());
    }

    #[test]
    fn drop_measured_from_confirmed_high_not_raw_max() {
        // The 110 print at 900 ms has no second print ≥ GAP later holding that level, so the
        // baseline stays at the confirmed 100 — a 5% drop from 100, not a 13.6% one from 110.
        let prev = [(0u64, 100.0), (500, 100.0), (900, 110.0)];
        let (bps, high) = detect_drop_bps(&prev, 1_200, 95.0, 60_000, 500.0, GAP).unwrap();
        assert_eq!(high, 100.0);
        assert!((bps - 500.0).abs() < 1e-6);
    }

    #[test]
    fn drop_single_burst_wick_high_is_ignored() {
        // Two wick prints 100 ms apart (one swap) never confirm 120; price back at 100 is no drop.
        let prev = [(0u64, 100.0), (500, 100.0), (900, 120.0), (1_000, 120.0)];
        assert!(detect_drop_bps(&prev, 1_500, 100.0, 60_000, 500.0, GAP).is_none());
        // gap 0 = raw max: the wick counts and the same tick reads as a 16.7% drop.
        assert!(detect_drop_bps(&prev, 1_500, 100.0, 60_000, 500.0, 0).is_some());
    }

    #[test]
    fn drop_two_prints_gap_apart_confirm_the_high() {
        let prev = [(0u64, 100.0), (500, 120.0), (900, 120.0)];
        let (bps, high) = detect_drop_bps(&prev, 1_500, 113.0, 60_000, 500.0, GAP).unwrap();
        assert_eq!(high, 120.0);
        assert!((bps - 583.333).abs() < 1e-2);
    }

    #[test]
    fn drop_evicts_out_of_window_high() {
        let prev = [(0u64, 120.0), (400, 120.0), (5_000, 100.0), (5_400, 100.0)];
        // 2 s window at t=6000: cutoff 4000 drops the 120s → high 100 → 4% is no trigger.
        assert!(detect_drop_bps(&prev, 6_000, 96.0, 2_000, 500.0, GAP).is_none());
        // 7 s window still sees the 120 high → 20% drop fires.
        let (_, high) = detect_drop_bps(&prev, 6_000, 96.0, 7_000, 500.0, GAP).unwrap();
        assert_eq!(high, 120.0);
    }

    #[test]
    fn drop_flat_and_rising_never_fire() {
        let prev = [(0u64, 100.0), (500, 100.0)];
        assert!(detect_drop_bps(&prev, 1_000, 100.0, 60_000, 500.0, GAP).is_none());
        assert!(detect_drop_bps(&prev, 1_000, 105.0, 60_000, 500.0, GAP).is_none());
    }

    #[test]
    fn drop_no_pair_in_window_is_none() {
        // A lone prior print cannot be a confirmed high (gap > 0), however far price fell.
        let prev = [(0u64, 100.0)];
        assert!(detect_drop_bps(&prev, 1_000, 50.0, 60_000, 500.0, GAP).is_none());
    }

    #[test]
    fn drop_ignores_nonpositive_and_nonfinite() {
        let prev = [(0u64, 0.0), (100, f64::NAN), (500, 100.0), (900, 100.0)];
        let (bps, _) = detect_drop_bps(&prev, 1_300, 94.0, 60_000, 500.0, GAP).unwrap();
        assert!((bps - 600.0).abs() < 1e-6);
        assert!(detect_drop_bps(&prev, 1_300, 0.0, 60_000, 500.0, GAP).is_none());
        assert!(detect_drop_bps(&prev, 1_300, f64::NAN, 60_000, 500.0, GAP).is_none());
    }

    #[test]
    fn drop_exact_boundary_fires() {
        let prev = [(0u64, 100.0), (500, 100.0)];
        let (bps, _) = detect_drop_bps(&prev, 1_000, 95.0, 60_000, 500.0, GAP).unwrap();
        assert!((bps - 500.0).abs() < 1e-6);
    }

    #[test]
    fn streak_counts_only_prints_gap_apart() {
        let t0 = Instant::now();
        let gap = Duration::from_millis(400);
        let s1 = advance_streak(None, true, t0, gap);
        assert_eq!(s1.map(|(_, n)| n), Some(1));
        let s2 = advance_streak(s1, true, t0 + Duration::from_millis(500), gap);
        assert_eq!(s2.map(|(_, n)| n), Some(2));
    }

    #[test]
    fn streak_same_burst_does_not_increment() {
        let t0 = Instant::now();
        let gap = Duration::from_millis(400);
        let s1 = advance_streak(None, true, t0, gap);
        let s2 = advance_streak(s1, true, t0 + Duration::from_millis(100), gap);
        assert_eq!(s2, Some((t0, 1)), "a second print inside the burst keeps count and timestamp");
    }

    #[test]
    fn streak_resets_on_recovering_print() {
        let t0 = Instant::now();
        let gap = Duration::from_millis(400);
        let s1 = advance_streak(None, true, t0, gap);
        assert_eq!(advance_streak(s1, false, t0 + Duration::from_millis(500), gap), None);
    }

    #[test]
    fn streak_reaches_n_after_n_spaced_breaches() {
        let t0 = Instant::now();
        let gap = Duration::from_millis(400);
        let mut s = None;
        for k in 0..3u64 {
            s = advance_streak(s, true, t0 + Duration::from_millis(450 * k), gap);
        }
        assert_eq!(s.map(|(_, n)| n), Some(3));
    }

    // ---- crash-exit plumbing ---------------------------------------------------------

    fn crash_cfg() -> CrashCfg {
        CrashCfg {
            threshold_bps: 500.0,
            window: Duration::from_secs(60),
            confirm_prints: 2,
            confirm_gap: Duration::from_millis(400),
        }
    }
    fn ms(t0: Instant, m: u64) -> Instant {
        t0 + Duration::from_millis(m)
    }

    #[test]
    fn note_print_fills_window_for_held_mints_when_crash_enabled() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        feed.note_print_at("TOK", 100.0, t0);
        feed.note_print_at("TOK", 100.0, ms(t0, 500));
        assert_eq!(feed.window_len("TOK"), 2, "held mints now keep a window (the up-detector used to skip them)");
    }

    #[test]
    fn note_print_up_detector_still_skips_held_mints() {
        let mut feed = GrpcFeed::new();
        feed.enable_spike(100.0, Duration::from_secs(5));
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        feed.note_print_at("TOK", 100.0, t0);
        feed.note_print_at("TOK", 500.0, ms(t0, 500));
        let mut rx = feed.spike_rx.lock().unwrap().take().unwrap();
        assert!(rx.try_recv().is_err(), "no entry signal for a held mint");
        assert_eq!(feed.window_len("TOK"), 2);
    }

    #[test]
    fn note_crash_needs_n_spaced_prints() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        feed.note_print_at("TOK", 100.0, t0);
        feed.note_print_at("TOK", 100.0, ms(t0, 500)); // confirms the 100 high
        feed.note_print_at("TOK", 94.0, ms(t0, 1_000)); // breach #1
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_000)).is_none());
        feed.note_print_at("TOK", 94.0, ms(t0, 1_500)); // breach #2, ≥ gap later
        let sig = feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).expect("confirmed");
        assert_eq!(sig.prints, 2);
        assert_eq!(sig.window_high, 100.0);
        assert!((sig.drop_bps - 600.0).abs() < 1e-6);
        assert_eq!(sig.at, ms(t0, 1_500));
    }

    #[test]
    fn note_crash_same_burst_does_not_confirm() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        feed.note_print_at("TOK", 100.0, t0);
        feed.note_print_at("TOK", 100.0, ms(t0, 500));
        feed.note_print_at("TOK", 94.0, ms(t0, 1_000)); // vault A
        feed.note_print_at("TOK", 94.0, ms(t0, 1_100)); // vault B of the same swap
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_100)).is_none());
    }

    #[test]
    fn note_crash_streak_resets_and_signal_removed_on_recovery() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        feed.note_print_at("TOK", 100.0, t0);
        feed.note_print_at("TOK", 100.0, ms(t0, 500));
        feed.note_print_at("TOK", 94.0, ms(t0, 1_000));
        feed.note_print_at("TOK", 94.0, ms(t0, 1_500));
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).is_some());
        feed.note_print_at("TOK", 99.0, ms(t0, 2_000)); // bounced above the line
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 2_000)).is_none());
        feed.note_print_at("TOK", 94.0, ms(t0, 2_500)); // a new breach starts from 1 again
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 2_500)).is_none());
    }

    #[test]
    fn note_crash_only_for_held_mints() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 90.0), (1_500, 90.0)] {
            feed.note_print_at("TOK", p, ms(t0, k));
        }
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).is_none());
    }

    #[test]
    fn note_crash_inert_when_disabled() {
        let mut feed = GrpcFeed::new();
        feed.enable_spike(100.0, Duration::from_secs(5)); // only the up-detector
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 90.0), (1_500, 90.0)] {
            feed.note_print_at("TOK", p, ms(t0, k));
        }
        assert!(feed.crash_cfg.is_none());
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).is_none());
    }

    #[test]
    fn crash_signal_stale_reads_none() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 94.0), (1_500, 94.0)] {
            feed.note_print_at("TOK", p, ms(t0, k));
        }
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 5_000)).is_some());
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 12_000)).is_none(), "a dead stream expires the signal");
    }

    #[test]
    fn crash_signal_keeps_first_confirmation_at_on_later_breaches() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 94.0), (1_500, 94.0), (2_000, 93.0)] {
            feed.note_print_at("TOK", p, ms(t0, k));
        }
        let sig = feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 2_000)).unwrap();
        assert_eq!(sig.at, ms(t0, 1_500), "shadow latch key = first confirmation");
        assert_eq!(sig.last, ms(t0, 2_000), "staleness clock = last breaching print");
        assert_eq!(sig.prints, 3);
        assert_eq!(sig.price, 93.0);
    }

    #[test]
    fn set_held_transition_resets_window_streak_and_signal() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 94.0), (1_500, 94.0)] {
            feed.note_print_at("TOK", p, ms(t0, k));
        }
        feed.set_held(["TOK".to_string()]); // already held: nothing changes
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).is_some());
        assert_eq!(feed.window_len("TOK"), 4);
        feed.set_held(Vec::<String>::new());
        feed.set_held(["TOK".to_string()]); // not-held → held: a fresh position sees a fresh window
        assert_eq!(feed.window_len("TOK"), 0);
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).is_none());
    }

    #[test]
    fn window_span_is_max_of_enabled_detectors() {
        let mut feed = GrpcFeed::new();
        assert_eq!(feed.window_span(), None);
        feed.enable_spike(100.0, Duration::from_secs(5));
        assert_eq!(feed.window_span(), Some(Duration::from_secs(5)));
        feed.enable_crash_exit(crash_cfg());
        assert_eq!(feed.window_span(), Some(Duration::from_secs(60)));
        let mut only_crash = GrpcFeed::new();
        only_crash.enable_crash_exit(crash_cfg());
        assert_eq!(only_crash.window_span(), Some(Duration::from_secs(60)));
    }

    // ---- per-mint overrides (momentum_tokens.json `params`) --------------------------

    #[test]
    fn crash_cfg_global_follows_the_master_switch() {
        assert!(CrashCfg::global(false, 500.0, 60, 2, 400).is_none(), "master off ⇒ no detector");
        let c = CrashCfg::global(true, 500.0, 60, 2, 400).expect("master on");
        assert_eq!(c.threshold_bps, 500.0);
        assert_eq!(c.window, Duration::from_secs(60));
        assert_eq!(c.confirm_prints, 2);
        assert_eq!(c.confirm_gap, Duration::from_millis(400));
    }

    fn overrides(entries: &[(&str, Option<CrashCfg>)]) -> HashMap<String, Option<CrashCfg>> {
        entries.iter().map(|(m, c)| (m.to_string(), *c)).collect()
    }

    #[test]
    fn crash_override_threshold_applies_to_that_mint_only() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_overrides(overrides(&[("LOW", Some(CrashCfg { threshold_bps: 200.0, ..crash_cfg() }))]));
        feed.set_held(["LOW".to_string(), "STD".to_string()]);
        let t0 = Instant::now();
        for m in ["LOW", "STD"] {
            feed.note_print_at(m, 100.0, t0);
            feed.note_print_at(m, 100.0, ms(t0, 500)); // confirmed high 100
            feed.note_print_at(m, 97.0, ms(t0, 1_000)); // −3%: a breach only at the 200 bps bar
            feed.note_print_at(m, 97.0, ms(t0, 1_500));
        }
        let low = feed.crash_signal_at("LOW", Duration::from_secs(10), ms(t0, 1_500)).expect("200 bps override fires");
        assert!((low.drop_bps - 300.0).abs() < 1e-6);
        assert!(feed.crash_signal_at("STD", Duration::from_secs(10), ms(t0, 1_500)).is_none(), "global 500 bps bar holds");
    }

    #[test]
    fn crash_override_window_applies_to_that_mint() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg()); // 60 s
        feed.set_crash_overrides(overrides(&[("SHORT", Some(CrashCfg { window: Duration::from_secs(10), ..crash_cfg() }))]));
        feed.set_held(["SHORT".to_string(), "STD".to_string()]);
        let t0 = Instant::now();
        for m in ["SHORT", "STD"] {
            feed.note_print_at(m, 100.0, t0);
            feed.note_print_at(m, 100.0, ms(t0, 500)); // the high, 30 s before the fall
            feed.note_print_at(m, 90.0, ms(t0, 30_000));
            feed.note_print_at(m, 90.0, ms(t0, 30_500));
            feed.note_print_at(m, 90.0, ms(t0, 31_000));
        }
        assert!(feed.crash_signal_at("STD", Duration::from_secs(10), ms(t0, 31_000)).is_some(), "high inside the 60 s window");
        assert!(
            feed.crash_signal_at("SHORT", Duration::from_secs(10), ms(t0, 31_000)).is_none(),
            "the 100 high is outside SHORT's 10 s window: only the 90s are in it, so nothing is a fall"
        );
    }

    #[test]
    fn crash_override_exempt_mint_never_signals() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_overrides(overrides(&[("EX", None)]));
        feed.set_held(["EX".to_string(), "STD".to_string()]);
        let t0 = Instant::now();
        for m in ["EX", "STD"] {
            for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 90.0), (1_500, 90.0)] {
                feed.note_print_at(m, p, ms(t0, k));
            }
        }
        assert!(feed.crash_signal_at("STD", Duration::from_secs(10), ms(t0, 1_500)).is_some(), "control: same prints fire on STD");
        assert!(feed.crash_signal_at("EX", Duration::from_secs(10), ms(t0, 1_500)).is_none(), "exempt mint is never detected");
    }

    #[test]
    fn crash_overrides_inert_without_master() {
        let feed = GrpcFeed::new();
        feed.set_crash_overrides(overrides(&[("LOW", Some(CrashCfg { threshold_bps: 200.0, ..crash_cfg() }))]));
        feed.set_held(["LOW".to_string()]);
        assert_eq!(feed.window_span(), None, "no master ⇒ no window bookkeeping at all");
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 90.0), (1_500, 90.0)] {
            feed.note_print_at("LOW", p, ms(t0, k));
        }
        assert!(feed.crash_signal_at("LOW", Duration::from_secs(10), ms(t0, 1_500)).is_none());
        assert_eq!(feed.window_len("LOW"), 0);
    }

    #[test]
    fn window_span_covers_override_windows() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg()); // 60 s
        feed.set_crash_overrides(overrides(&[
            ("A", Some(CrashCfg { window: Duration::from_secs(120), ..crash_cfg() })),
            ("B", Some(CrashCfg { window: Duration::from_secs(30), ..crash_cfg() })),
            ("C", None),
        ]));
        assert_eq!(feed.window_span(), Some(Duration::from_secs(120)), "the longest window in use");
        feed.set_crash_overrides(HashMap::new());
        assert_eq!(feed.window_span(), Some(Duration::from_secs(60)), "back to the global window");
    }

    #[test]
    fn set_crash_overrides_replaces_the_previous_map() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_overrides(overrides(&[("LOW", Some(CrashCfg { threshold_bps: 200.0, ..crash_cfg() }))]));
        assert_eq!(feed.crash_cfg_for("LOW").map(|c| c.threshold_bps), Some(200.0));
        feed.set_crash_overrides(overrides(&[("OTHER", None)]));
        assert_eq!(feed.crash_cfg_for("LOW").map(|c| c.threshold_bps), Some(500.0), "LOW is back on the global");
        assert!(feed.crash_cfg_for("OTHER").is_none());
        assert_eq!(feed.crash_cfg_for("ANY").map(|c| c.threshold_bps), Some(500.0));
    }

    // ---- dynamic per-mint bars (volatility-scaled, pushed by the watcher) --------------

    fn bars(entries: &[(&str, f64)]) -> HashMap<String, f64> {
        entries.iter().map(|(m, b)| (m.to_string(), *b)).collect()
    }

    #[test]
    fn crash_dynamic_bar_replaces_the_threshold_and_keeps_the_window() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg()); // 500 bps / 60 s
        feed.set_crash_bars(bars(&[("TOK", 250.0)]));
        let (c, src) = feed.crash_resolution("TOK").expect("resolved");
        assert_eq!(c.threshold_bps, 250.0);
        assert_eq!(c.window, Duration::from_secs(60));
        assert_eq!(src, CrashBarSource::Dynamic);
        feed.set_held(["TOK".to_string()]);
        let t0 = Instant::now();
        for (k, p) in [(0u64, 100.0), (500, 100.0), (1_000, 97.0), (1_500, 97.0)] {
            feed.note_print_at("TOK", p, ms(t0, k));
        }
        assert!(feed.crash_signal_at("TOK", Duration::from_secs(10), ms(t0, 1_500)).is_some(), "3% ≥ the 2.5% dynamic bar");
    }

    #[test]
    fn crash_dynamic_bar_layers_over_a_static_window_override() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_overrides(overrides(&[("TOK", Some(CrashCfg { window: Duration::from_secs(120), ..crash_cfg() }))]));
        feed.set_crash_bars(bars(&[("TOK", 250.0)]));
        let (c, src) = feed.crash_resolution("TOK").expect("resolved");
        assert_eq!((c.threshold_bps, c.window, src), (250.0, Duration::from_secs(120), CrashBarSource::Dynamic));
    }

    #[test]
    fn crash_dynamic_bar_never_revives_an_exempt_mint() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_overrides(overrides(&[("EX", None)]));
        feed.set_crash_bars(bars(&[("EX", 250.0)]));
        assert!(feed.crash_resolution("EX").is_none());
        assert!(feed.crash_cfg_for("EX").is_none());
    }

    #[test]
    fn set_crash_bars_replaces_the_set_and_ignores_garbage() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_bars(bars(&[("A", 250.0), ("B", f64::NAN), ("C", 0.0)]));
        let res = |m: &str| feed.crash_resolution(m).map(|(c, s)| (c.threshold_bps, s));
        assert_eq!(res("A"), Some((250.0, CrashBarSource::Dynamic)));
        assert_eq!(res("B"), Some((500.0, CrashBarSource::Global)), "NaN bar is dropped");
        assert_eq!(res("C"), Some((500.0, CrashBarSource::Global)), "0 bar is dropped");
        feed.set_crash_bars(HashMap::new());
        assert_eq!(res("A"), Some((500.0, CrashBarSource::Global)), "a bar not re-pushed falls back");
    }

    #[test]
    fn crash_resolution_reports_static_source_for_a_params_override() {
        let mut feed = GrpcFeed::new();
        feed.enable_crash_exit(crash_cfg());
        feed.set_crash_overrides(overrides(&[("PIN", Some(CrashCfg { threshold_bps: 800.0, ..crash_cfg() }))]));
        assert_eq!(feed.crash_resolution("PIN").map(|(c, s)| (c.threshold_bps, s)), Some((800.0, CrashBarSource::Static)));
        assert_eq!(feed.crash_resolution("ANY").map(|(_, s)| s), Some(CrashBarSource::Global));
        assert!(GrpcFeed::new().crash_resolution("ANY").is_none(), "master off ⇒ nothing");
    }
}
