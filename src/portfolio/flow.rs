//! Order-flow entry gate: volume floor + sell/buy divergence guard.
//!
//! The momentum trader ranks on price alone. Price cannot distinguish "rising because
//! demand is real" from "rising while every holder distributes into it" — and a pump.fun
//! token spends its whole life somewhere on that spectrum. This module fetches per-pool
//! trade counts and volume from DexScreener (the same REST source `pricer.rs` already
//! uses) and turns them into an entry veto.
//!
//! **Why counts and not volume-weighted flow.** DexScreener publishes `txns.{buys,sells}`
//! and a single `volume` total per window — there is no buy-volume/sell-volume split. So a
//! raw sell:buy *count* ratio cannot tell 12,000 dust sells apart from genuine
//! distribution. Two things make it usable anyway:
//!
//! 1. `min_txns_h1` — a denominator guard. Measured 2026-08-01: JitoSOL showed **67:1
//!    sells on 68 transactions** (one buy) while the price *rose* — the most extreme ratio
//!    in the book, on the healthiest token. Any bare ratio threshold fires there first. The
//!    count floor removes that entire class of false positive.
//! 2. The ratio is only acted on when the price is **rising**. Sells dominating while price
//!    falls is ordinary and already handled — the momentum metric's slope drops on its own.
//!    The pathology worth vetoing is the divergence: price up, flow overwhelmingly out.
//!
//! Volume is the cleaner half: monotone, no denominator, and already trusted at curation
//! time (`SCAN_MIN_VOLUME` in `scan_tokens.js`). It is expressed as *decay* against the
//! token's own 24h average rather than an absolute floor, so a natively quiet deep pool
//! (JitoSOL, $12k/h) is not punished for being quiet.
//!
//! **Not backtestable.** `price_history.jsonl` holds prices only — no trade counts, no
//! volume. `momentum-sim` can never score this gate. That is why every rejection is
//! audited and every poll is logged: the JSONL record is the only evidence that will ever
//! accumulate. Being entry-side, a false positive costs an opportunity rather than a
//! position — the cheaper direction to be wrong in.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// One pool's order-flow reading over the 1h window (plus the 24h volume the decay ratio
/// is measured against). Mirrors the DexScreener fields verbatim — no derived state is
/// stored, so the policy layer stays pure and testable.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct FlowSnapshot {
    pub vol_h1: f64,
    pub vol_h24: f64,
    pub buys_h1: u64,
    pub sells_h1: u64,
    /// Percent price change over the 1h window, as DexScreener reports it (e.g. `-7.73`).
    pub price_chg_h1: f64,
}

impl FlowSnapshot {
    pub fn txns_h1(&self) -> u64 {
        self.buys_h1.saturating_add(self.sells_h1)
    }
    /// Sell:buy count ratio. `None` when there are no buys — the caller must reject on the
    /// transaction-count guard before ever reaching this, so an infinite ratio can never
    /// become a veto by itself.
    pub fn sell_buy_ratio(&self) -> Option<f64> {
        (self.buys_h1 > 0).then(|| self.sells_h1 as f64 / self.buys_h1 as f64)
    }
    /// 1h volume as a multiple of the token's own hourly 24h average. `None` when the 24h
    /// figure is missing or zero (a brand-new pool), which reads as "no opinion".
    pub fn vol_decay(&self) -> Option<f64> {
        let base = self.vol_h24 / 24.0;
        (base.is_finite() && base > 0.0).then(|| self.vol_h1 / base)
    }
}

/// Resolved per-token thresholds (per-token override ?? global default). Every gate is
/// off at `0`; `min_txns_h1` is a guard rather than a gate, so it defaults ON.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlowParams {
    pub min_vol_h1_usd: f64,
    pub min_vol_decay: f64,
    pub max_sell_buy_ratio: f64,
    pub min_txns_h1: u64,
}

impl Default for FlowParams {
    fn default() -> Self {
        FlowParams { min_vol_h1_usd: 0.0, min_vol_decay: 0.0, max_sell_buy_ratio: 0.0, min_txns_h1: 200 }
    }
}

/// Outcome of the gate. Carries the numbers that produced it so the log line and the audit
/// record can state *why* without recomputing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FlowVerdict {
    Pass,
    LowVolume { vol_h1: f64, floor: f64 },
    VolumeDecay { decay: f64, floor: f64 },
    Distribution { ratio: f64, cap: f64, txns: u64, chg: f64 },
}

impl FlowVerdict {
    pub fn is_block(&self) -> bool {
        !matches!(self, FlowVerdict::Pass)
    }
    pub fn reason(&self) -> String {
        match *self {
            FlowVerdict::Pass => "pass".into(),
            FlowVerdict::LowVolume { vol_h1, floor } =>
                format!("1h volume ${vol_h1:.0} below floor ${floor:.0}"),
            FlowVerdict::VolumeDecay { decay, floor } =>
                format!("1h volume {decay:.2}x its 24h average, below {floor:.2}x floor"),
            FlowVerdict::Distribution { ratio, cap, txns, chg } =>
                format!("distribution into strength: {ratio:.2} sells per buy (cap {cap:.2}) \
                         over {txns} txns while price {chg:+.2}%"),
        }
    }
}

/// Pure entry gate. Order matters: the volume checks are unambiguous and run first; the
/// divergence check runs last and requires BOTH a meaningful sample (`min_txns_h1`) and a
/// *rising* price, because sells outnumbering buys on a falling price is the ordinary case
/// the momentum metric already prices in.
pub fn flow_gate(f: &FlowSnapshot, p: &FlowParams) -> FlowVerdict {
    if p.min_vol_h1_usd > 0.0 && f.vol_h1 < p.min_vol_h1_usd {
        return FlowVerdict::LowVolume { vol_h1: f.vol_h1, floor: p.min_vol_h1_usd };
    }
    if p.min_vol_decay > 0.0 {
        if let Some(d) = f.vol_decay() {
            if d < p.min_vol_decay {
                return FlowVerdict::VolumeDecay { decay: d, floor: p.min_vol_decay };
            }
        }
    }
    if p.max_sell_buy_ratio > 0.0 && f.txns_h1() >= p.min_txns_h1 && f.price_chg_h1 > 0.0 {
        if let Some(r) = f.sell_buy_ratio() {
            if r > p.max_sell_buy_ratio {
                return FlowVerdict::Distribution {
                    ratio: r,
                    cap: p.max_sell_buy_ratio,
                    txns: f.txns_h1(),
                    chg: f.price_chg_h1,
                };
            }
        }
    }
    FlowVerdict::Pass
}

/// Freshness-stamped per-mint flow cache, written by the background poller and read
/// lock-free on the entry path.
#[derive(Clone, Default)]
pub struct FlowCache {
    inner: Arc<DashMap<String, (FlowSnapshot, Instant)>>,
}

impl FlowCache {
    pub fn new() -> Self {
        FlowCache { inner: Arc::new(DashMap::new()) }
    }
    pub fn publish(&self, mint: &str, snap: FlowSnapshot) {
        self.inner.insert(mint.to_string(), (snap, Instant::now()));
    }
    /// Latest reading for `mint` if present and fresher than `max_age`. `None` — absent,
    /// stale, or DexScreener unreachable — means the gate must FAIL OPEN.
    pub fn get(&self, mint: &str, max_age: Duration) -> Option<FlowSnapshot> {
        self.inner.get(mint).and_then(|e| {
            let (snap, ts) = *e.value();
            (ts.elapsed() <= max_age).then_some(snap)
        })
    }
    /// Every fresh reading, for the periodic log line.
    pub fn snapshot_all(&self, max_age: Duration) -> HashMap<String, FlowSnapshot> {
        self.inner
            .iter()
            .filter(|e| e.value().1.elapsed() <= max_age)
            .map(|e| (e.key().clone(), e.value().0))
            .collect()
    }
}

/// Parse DexScreener's `/latest/dex/pairs/solana/{pool}` payload into a snapshot. Returns
/// `None` on any missing field rather than substituting zeros — a zero volume would read
/// as "collapsed" and veto an entry on absent data.
pub fn parse_pair(v: &serde_json::Value) -> Option<FlowSnapshot> {
    let p = v.get("pair").or_else(|| v.get("pairs")?.get(0))?;
    Some(FlowSnapshot {
        vol_h1: p.pointer("/volume/h1")?.as_f64()?,
        vol_h24: p.pointer("/volume/h24")?.as_f64()?,
        buys_h1: p.pointer("/txns/h1/buys")?.as_u64()?,
        sells_h1: p.pointer("/txns/h1/sells")?.as_u64()?,
        price_chg_h1: p.pointer("/priceChange/h1").and_then(|x| x.as_f64()).unwrap_or(0.0),
    })
}

/// Background poller: refresh every watched token's flow reading every `every_secs`.
///
/// One HTTP request per pool, paced 250 ms apart — DexScreener's documented limit is
/// 300 req/min and a 5-token book uses ~5. Tokens without a wired `pool` are skipped (the
/// pairs endpoint is keyed by pool address). A failed or unparseable response simply leaves
/// the previous entry to age out, so a DexScreener outage degrades to "no opinion" rather
/// than to a veto.
pub fn spawn_poller(
    cache: FlowCache,
    watched: Vec<crate::portfolio::momentum_universe::WatchedToken>,
    every_secs: u64,
) {
    let pools: Vec<(String, String, String)> = watched
        .iter()
        .filter_map(|w| w.pool.clone().map(|p| (w.symbol.clone(), w.mint.clone(), p)))
        .collect();
    if pools.is_empty() {
        tracing::info!("momentum flow: no watched token has a wired pool — poller not started");
        return;
    }
    tracing::info!(
        "momentum flow: polling {} pool(s) every {}s (DexScreener)",
        pools.len(),
        every_secs
    );
    tokio::spawn(async move {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        let mut tick = tokio::time::interval(Duration::from_secs(every_secs));
        loop {
            tick.tick().await;
            for (sym, mint, pool) in &pools {
                let url = format!("https://api.dexscreener.com/latest/dex/pairs/solana/{pool}");
                match http.get(&url).send().await {
                    Ok(r) => match r.json::<serde_json::Value>().await {
                        Ok(v) => match parse_pair(&v) {
                            Some(snap) => cache.publish(mint, snap),
                            None => tracing::debug!("momentum flow: {sym} payload missing fields"),
                        },
                        Err(e) => tracing::debug!("momentum flow: {sym} decode failed: {e}"),
                    },
                    Err(e) => tracing::debug!("momentum flow: {sym} fetch failed: {e}"),
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    });
}

/// Render the per-tick console line and the matching audit rows. Returns `None` when no
/// fresh reading exists, so the caller writes nothing rather than an empty snapshot.
pub fn render_log(
    watched: &[crate::portfolio::momentum_universe::WatchedToken],
    cache: &FlowCache,
    max_age: Duration,
) -> Option<(String, Vec<crate::portfolio::momentum_actions::TokenFlow>)> {
    use crate::portfolio::momentum_actions::TokenFlow;
    let fresh = cache.snapshot_all(max_age);
    if fresh.is_empty() {
        return None;
    }
    let (mut parts, mut rows) = (Vec::new(), Vec::new());
    for w in watched {
        let Some(f) = fresh.get(&w.mint) else { continue };
        let ratio = f.sell_buy_ratio();
        let decay = f.vol_decay();
        parts.push(format!(
            "{} s:b {} vol1h ${} ({}) {:+.2}%",
            w.symbol,
            ratio.map(|r| format!("{r:.2}")).unwrap_or_else(|| "—".into()),
            fmt_usd(f.vol_h1),
            decay.map(|d| format!("{d:.2}x")).unwrap_or_else(|| "—".into()),
            f.price_chg_h1,
        ));
        rows.push(TokenFlow {
            symbol: w.symbol.clone(),
            vol_h1: f.vol_h1,
            vol_h24: f.vol_h24,
            buys_h1: f.buys_h1,
            sells_h1: f.sells_h1,
            sell_buy_ratio: ratio,
            vol_decay: decay,
            price_chg_h1: f.price_chg_h1,
        });
    }
    (!rows.is_empty()).then(|| (format!("momentum flow: {}", parts.join(" | ")), rows))
}

fn fmt_usd(v: f64) -> String {
    if v >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if v >= 1e3 {
        format!("{:.0}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The measured book on 2026-08-01. Every one of these must PASS at the suggested
    /// thresholds — this is the false-positive regression lock.
    fn book() -> Vec<(&'static str, FlowSnapshot)> {
        vec![
            // 67:1 sells on 68 txns while rising — the case that breaks a bare ratio gate
            ("JitoSOL", FlowSnapshot { vol_h1: 12_311.0, vol_h24: 261_840.0, buys_h1: 1, sells_h1: 67, price_chg_h1: 0.29 }),
            ("WETH",    FlowSnapshot { vol_h1: 137_686.0, vol_h24: 2_592_528.0, buys_h1: 132, sells_h1: 173, price_chg_h1: 0.14 }),
            ("HYPE",    FlowSnapshot { vol_h1: 15_281.0, vol_h24: 662_448.0, buys_h1: 18, sells_h1: 5, price_chg_h1: 0.20 }),
            ("ZEC",     FlowSnapshot { vol_h1: 132_294.0, vol_h24: 2_399_856.0, buys_h1: 58, sells_h1: 186, price_chg_h1: -0.77 }),
            // 8.53:1 sells but price FALLING — ordinary, the metric handles it
            ("CATE",    FlowSnapshot { vol_h1: 227_276.0, vol_h24: 7_526_712.0, buys_h1: 1528, sells_h1: 13038, price_chg_h1: -7.73 }),
        ]
    }

    fn suggested() -> FlowParams {
        FlowParams { min_vol_h1_usd: 0.0, min_vol_decay: 0.3, max_sell_buy_ratio: 5.0, min_txns_h1: 200 }
    }

    #[test]
    fn suggested_thresholds_pass_the_whole_measured_book() {
        for (sym, f) in book() {
            assert_eq!(flow_gate(&f, &suggested()), FlowVerdict::Pass, "{sym} must not be vetoed");
        }
    }

    /// The denominator guard is the whole reason a ratio gate is usable: without it,
    /// JitoSOL's one-buy hour reads as 67:1 distribution.
    #[test]
    fn txn_floor_is_what_saves_jitosol() {
        let jito = book()[0].1;
        let no_guard = FlowParams { min_txns_h1: 0, ..suggested() };
        assert!(matches!(flow_gate(&jito, &no_guard), FlowVerdict::Distribution { .. }));
        assert_eq!(flow_gate(&jito, &suggested()), FlowVerdict::Pass);
    }

    /// Sells dominating on a FALLING price is ordinary; only the rising-price divergence
    /// is vetoed.
    #[test]
    fn distribution_requires_a_rising_price() {
        let falling = book()[4].1; // CATE, 8.53:1, -7.73%
        assert_eq!(flow_gate(&falling, &suggested()), FlowVerdict::Pass);
        let rising = FlowSnapshot { price_chg_h1: 3.0, ..falling };
        match flow_gate(&rising, &suggested()) {
            FlowVerdict::Distribution { ratio, txns, .. } => {
                assert!((ratio - 8.53).abs() < 0.01);
                assert_eq!(txns, 14_566);
            }
            v => panic!("expected Distribution, got {v:?}"),
        }
    }

    #[test]
    fn volume_gates_fire_independently() {
        let f = FlowSnapshot { vol_h1: 1_000.0, vol_h24: 240_000.0, buys_h1: 500, sells_h1: 500, price_chg_h1: 1.0 };
        // decay = 1000 / (240000/24) = 0.10
        assert!(matches!(
            flow_gate(&f, &FlowParams { min_vol_decay: 0.3, ..Default::default() }),
            FlowVerdict::VolumeDecay { .. }
        ));
        assert!(matches!(
            flow_gate(&f, &FlowParams { min_vol_h1_usd: 5_000.0, ..Default::default() }),
            FlowVerdict::LowVolume { .. }
        ));
        // all-zero thresholds ⇒ inert
        assert_eq!(flow_gate(&f, &FlowParams::default()), FlowVerdict::Pass);
    }

    /// A brand-new pool has no 24h base; "no opinion" must not become a veto.
    #[test]
    fn missing_24h_base_does_not_veto() {
        let f = FlowSnapshot { vol_h1: 5_000.0, vol_h24: 0.0, buys_h1: 300, sells_h1: 300, price_chg_h1: 1.0 };
        assert_eq!(f.vol_decay(), None);
        assert_eq!(flow_gate(&f, &suggested()), FlowVerdict::Pass);
    }

    #[test]
    fn cache_serves_fresh_and_hides_stale() {
        let c = FlowCache::new();
        let f = book()[0].1;
        c.publish("M", f);
        assert_eq!(c.get("M", Duration::from_secs(300)), Some(f));
        assert_eq!(c.get("MISSING", Duration::from_secs(300)), None);
        assert_eq!(c.get("M", Duration::from_nanos(1)), None, "stale must read as None");
    }

    #[test]
    fn parse_pair_reads_dexscreener_shape() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"pair":{"volume":{"h1":227276.0,"h24":7526712.0},
                 "txns":{"h1":{"buys":1528,"sells":13038}},
                 "priceChange":{"h1":-7.73}}}"#,
        ).unwrap();
        let f = parse_pair(&v).expect("parses");
        assert_eq!(f.buys_h1, 1528);
        assert_eq!(f.sells_h1, 13038);
        assert!((f.price_chg_h1 + 7.73).abs() < 1e-9);
        // a payload missing volume yields None, never a zero-volume "collapsed" reading
        let bad: serde_json::Value = serde_json::from_str(r#"{"pair":{"txns":{"h1":{"buys":1,"sells":1}}}}"#).unwrap();
        assert!(parse_pair(&bad).is_none());
    }
}
