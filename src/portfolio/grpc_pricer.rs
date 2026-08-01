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
    /// off; `note_spike` is then a no-op and the entry-side signal never fires.
    pub spike_cfg: Option<SpikeCfg>,
    /// Per-mint rolling price window `(sample_instant, usd)`, oldest-first, feeding the
    /// upward-spike detector. Only populated when `spike_cfg` is `Some`.
    spike_win: Arc<DashMap<String, VecDeque<(Instant, f64)>>>,
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
            spike_win: Arc::new(DashMap::new()),
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

    /// Record a fresh price in the mint's rolling window and, if it is an upward spike
    /// (≥ `threshold_bps` over the lowest in-window baseline) on a NON-held mint, signal
    /// the entry path with the mint. No-op when spike detection is disabled — keeps the
    /// sub-second hot path free when the feature is off. Held mints are skipped: they are
    /// the exit path's job and the entry gate dedups them anyway.
    pub fn note_spike(&self, mint: &str, usd: f64) {
        let (Some(cfg), Some(tx)) = (self.spike_cfg, self.spike_tx.as_ref()) else { return };
        if !usd.is_finite() || usd <= 0.0 {
            return;
        }
        if self.held.read().map(|h| h.contains(mint)).unwrap_or(false) {
            return;
        }
        let now = Instant::now();
        let window_ms = cfg.window.as_millis() as u64;
        // Brief per-shard DashMap write lock (the only lock on this path): evict stale
        // front samples, snapshot the in-window prior samples onto a (ms, price) timeline
        // where "now" sits at `window_ms`, run the pure detector, then append this sample.
        let spike = {
            let mut win = self.spike_win.entry(mint.to_string()).or_default();
            while let Some(&(si, _)) = win.front() {
                if now.saturating_duration_since(si) > cfg.window {
                    win.pop_front();
                } else {
                    break;
                }
            }
            let prev: Vec<(u64, f64)> = win
                .iter()
                .map(|(si, p)| {
                    let age_ms = now.saturating_duration_since(*si).as_millis() as u64;
                    (window_ms.saturating_sub(age_ms), *p)
                })
                .collect();
            let s = detect_spike_bps(&prev, window_ms, usd, window_ms, cfg.threshold_bps);
            win.push_back((now, usd));
            s
        };
        if let Some(bps) = spike {
            // Unbounded, non-blocking: a dropped signal under a storm is harmless (spike
            // re-eval is idempotent; the 60s tick and later spikes still cover the token).
            let _ = tx.send(mint.to_string());
            info!("gRPC: SPIKE {} +{:.0}bps/{}s", mint, bps, cfg.window.as_secs());
        }
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
    pub fn set_held(&self, mints: impl IntoIterator<Item = String>) {
        if let Ok(mut h) = self.held.write() { *h = mints.into_iter().collect(); }
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
}
