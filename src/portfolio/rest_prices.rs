//! Background REST price cache for the momentum trader.
//!
//! The slow tick and the 1 s exit tick used to fetch DexScreener/Kraken prices INLINE, on the
//! same task that evaluates the trailing stop, with no bound — one slow host blinded every
//! stop. This cache is filled by its own task (`spawn_poller`, mirroring `flow::spawn_poller`)
//! and read without awaiting: a price is served only while it is younger than the caller's
//! `max_age`, and a missing/stale entry reads as *absent* (the caller carries forward or skips),
//! never as a fake move.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use dashmap::DashMap;

use super::pricer;

/// Shared, clone-cheap cache: `key → (price_usd, published_at)`. Keys are mints plus the
/// two SOL aliases the recorder uses (`"SOL"` and the WSOL mint).
#[derive(Clone, Default)]
pub struct RestPriceCache {
    inner: Arc<DashMap<String, (f64, Instant)>>,
    want: Arc<RwLock<Vec<String>>>,
}

impl RestPriceCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publish_at(&self, key: &str, price: f64, now: Instant) {
        self.inner.insert(key.to_string(), (price, now));
    }

    pub fn publish(&self, key: &str, price: f64) {
        self.publish_at(key, price, Instant::now());
    }

    /// The cached price if it is younger than `max_age`; `None` when missing or stale.
    pub fn get_at(&self, key: &str, max_age: Duration, now: Instant) -> Option<f64> {
        self.inner.get(key).and_then(|e| {
            let (px, at) = *e;
            (now.saturating_duration_since(at) < max_age).then_some(px)
        })
    }

    pub fn get(&self, key: &str, max_age: Duration) -> Option<f64> {
        self.get_at(key, max_age, Instant::now())
    }

    /// The cached price and its age, regardless of staleness (caller decides).
    pub fn get_with_age_at(&self, key: &str, now: Instant) -> Option<(f64, Duration)> {
        self.inner.get(key).map(|e| {
            let (px, at) = *e;
            (px, now.saturating_duration_since(at))
        })
    }

    pub fn get_with_age(&self, key: &str) -> Option<(f64, Duration)> {
        self.get_with_age_at(key, Instant::now())
    }

    /// Fresh prices for the requested keys only — stale and missing keys are simply
    /// absent, so the caller's carry-forward rules apply unchanged.
    pub fn snapshot_at(&self, keys: &[String], max_age: Duration, now: Instant) -> HashMap<String, f64> {
        keys.iter()
            .filter_map(|k| self.get_at(k, max_age, now).map(|px| (k.clone(), px)))
            .collect()
    }

    pub fn snapshot(&self, keys: &[String], max_age: Duration) -> HashMap<String, f64> {
        self.snapshot_at(keys, max_age, Instant::now())
    }

    /// Replace the set of mints the poller refreshes each cycle.
    pub fn set_want(&self, keys: Vec<String>) {
        if let Ok(mut w) = self.want.write() {
            *w = keys;
        }
    }

    pub fn want(&self) -> Vec<String> {
        self.want.read().map(|w| w.clone()).unwrap_or_default()
    }
}

/// The mints the poller should walk: the priced set minus the SOL aliases (SOL comes from
/// Kraken in the same cycle), de-duplicated in first-seen order.
pub fn poll_set(mints: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(mints.len());
    for m in mints {
        if m == "SOL" || m == pricer::SOL_MINT || out.contains(m) {
            continue;
        }
        out.push(m.clone());
    }
    out
}

/// Pacing between DexScreener requests inside one poll cycle (documented limit 300/min).
const POLL_PACING: Duration = Duration::from_millis(250);
/// Per-request cap: a healthy answer takes ~65 ms; past this the host is throttling.
const POLL_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Background poller: every `every_secs`, publish SOL (Kraken) and then one DexScreener
/// price per wanted mint. Only successful reads are published — a failure leaves the
/// previous entry to age out, so an outage degrades to "absent" (carry-forward / no stop
/// evaluation for that mint), never to a fake move. Mirrors `flow::spawn_poller`.
pub fn spawn_poller(cache: RestPriceCache, every_secs: u64) {
    let every = Duration::from_secs(every_secs.max(1));
    tracing::info!(
        "momentum rest-prices: background poller every {}s (Kraken SOL + DexScreener per wanted mint)",
        every.as_secs()
    );
    tokio::spawn(async move {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut sol_failing = false;
        loop {
            tick.tick().await;
            match pricer::fetch_sol_kraken_with_timeout(&http, Some(POLL_REQUEST_TIMEOUT)).await {
                Ok(sol) => {
                    if sol_failing {
                        tracing::info!("momentum rest-prices: Kraken SOL recovered");
                        sol_failing = false;
                    }
                    for (k, px) in sol {
                        cache.publish(&k, px);
                    }
                }
                Err(e) => {
                    if !sol_failing {
                        tracing::warn!("momentum rest-prices: Kraken SOL fetch failed ({e}); SOL ages out until it recovers");
                        sol_failing = true;
                    }
                }
            }
            for mint in cache.want() {
                match pricer::best_base_pair_price_with_timeout(&http, &mint, Some(POLL_REQUEST_TIMEOUT)).await {
                    Ok(Some(px)) => cache.publish(&mint, px),
                    Ok(None) => {}
                    Err(e) => tracing::debug!("momentum rest-prices: {mint} fetch failed: {e}"),
                }
                tokio::time::sleep(POLL_PACING).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn get_serves_only_prices_younger_than_max_age() {
        let c = RestPriceCache::new();
        let t0 = Instant::now();
        c.publish_at("m", 1.5, t0);
        assert_eq!(c.get_at("m", Duration::from_secs(60), t0 + Duration::from_secs(59)), Some(1.5));
        assert_eq!(c.get_at("m", Duration::from_secs(60), t0 + Duration::from_secs(61)), None);
        assert_eq!(c.get_at("missing", Duration::from_secs(60), t0), None);
    }

    #[test]
    fn snapshot_returns_fresh_requested_keys_only() {
        let c = RestPriceCache::new();
        let t0 = Instant::now();
        c.publish_at("fresh", 2.0, t0);
        c.publish_at("stale", 3.0, t0 - Duration::from_secs(120));
        c.publish_at("unrequested", 4.0, t0);
        let snap = c.snapshot_at(
            &["fresh".to_string(), "stale".to_string(), "absent".to_string()],
            Duration::from_secs(60),
            t0,
        );
        assert_eq!(snap.len(), 1);
        assert_eq!(snap.get("fresh"), Some(&2.0));
    }

    #[test]
    fn get_with_age_reports_how_old_the_sample_is() {
        let c = RestPriceCache::new();
        let t0 = Instant::now();
        c.publish_at("m", 9.0, t0);
        let (px, age) = c.get_with_age_at("m", t0 + Duration::from_secs(7)).unwrap();
        assert_eq!(px, 9.0);
        assert_eq!(age, Duration::from_secs(7));
        assert!(c.get_with_age_at("nope", t0).is_none());
    }

    #[test]
    fn set_want_replaces_the_polled_set() {
        let c = RestPriceCache::new();
        assert!(c.want().is_empty());
        c.set_want(vec!["a".into(), "b".into()]);
        assert_eq!(c.want(), vec!["a".to_string(), "b".to_string()]);
        c.set_want(vec!["c".into()]);
        assert_eq!(c.want(), vec!["c".to_string()]);
    }

    #[test]
    fn republishing_overwrites_value_and_timestamp() {
        let c = RestPriceCache::new();
        let t0 = Instant::now();
        c.publish_at("m", 1.0, t0 - Duration::from_secs(500));
        c.publish_at("m", 2.0, t0);
        assert_eq!(c.get_at("m", Duration::from_secs(60), t0), Some(2.0));
    }

    #[test]
    fn poll_set_drops_the_sol_keys_the_poller_gets_from_kraken() {
        let mints = vec![
            "SOL".to_string(),
            "So11111111111111111111111111111111111111112".to_string(),
            "MintA".to_string(),
            "MintA".to_string(),
        ];
        assert_eq!(poll_set(&mints), vec!["MintA".to_string()]);
    }
}
