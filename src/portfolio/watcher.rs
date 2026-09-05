use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use reqwest::Client;
use tracing::{error, info, warn};

use super::analyzer::{self, Alert, AnalysisConfig, RiskReport, SwapSuggestion};
use super::momentum::{self, MomentumContext, TradeOutcome};
use super::momentum_state;
use super::momentum_universe::{self, PoolRef, WatchedToken};
use super::scanner;
use super::suggestions::{self, Suggestion, SORTINO_MIN_OBS};
use super::{Portfolio, PortfolioConfig, TokenEntry};
use super::emailer;
use super::history::{self, PriceSnapshot};
use super::pricer;
use super::rest_prices::{self, RestPriceCache};
use super::grpc_pricer::{self, GrpcFeed};
use super::momentum_actions::ActionKind;
use super::tick_timing::{self, TickTimer};

const PRICE_STALE_THRESHOLD: Duration = Duration::from_secs(300);
/// Consecutive carried-forward ticks (no fresh price) for a watched momentum token
/// before we warn its price is frozen. At the ~60 s momentum tick this is ~3 min — long
/// enough to ignore a single missed fetch, short enough to catch a real feed freeze.
const CARRY_FORWARD_WARN_STREAK: u32 = 3;
/// After the first warning, re-warn every this-many additional carried-forward ticks so a
/// persistent freeze stays visible without spamming a line every tick.
const CARRY_FORWARD_REWARN_EVERY: u32 = 10;
/// Consecutive failed decodes after which a pool is benched from the dynamic-wire set.
/// A pool that never decodes (e.g. a Meteora DAMM venue routed to the DLMM fetcher, which
/// cannot read it) would otherwise keep `want != wired_dynamic` true forever, re-spawning
/// the whole gRPC feed — and warm-starting an empty price map for EVERY token — once per
/// retry window, indefinitely. Benching lets `want` converge on the decodable subset so
/// `wired_dynamic` catches up and the churn stops.
const WIRE_FAIL_STRIKES: u32 = 3;
/// How long a benched pool stays out, as a multiple of the wire-retry window (≈6 h at the
/// default hourly cadence). Benching is a COOL-DOWN, never permanent: `run_pool_decode`
/// reports failure per script BATCH and its Err covers transient infra modes too (30 s
/// subprocess timeout, spawn failure, an RPC hiccup behind a non-zero exit), so a pool can
/// be benched for another pool's fault or for a blip. On expiry its strikes are cleared and
/// it gets a full fresh attempt cycle. A genuinely undecodable pool therefore costs one
/// short burst of decode attempts per cool-down instead of one per retry window — bounded —
/// while a transiently-failed pool always heals itself.
const WIRE_FAIL_COOLDOWN_MULT: u32 = 6;

/// Decode-failure bookkeeping for one dynamically-wired pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WireFail {
    /// Consecutive failed decode attempts (reset by any successful decode).
    strikes: u32,
    /// When the pool was benched, i.e. when `strikes` first reached `WIRE_FAIL_STRIKES`.
    /// `None` = currently eligible for a decode attempt.
    benched_at: Option<Instant>,
}

pub async fn run(
    cfg: PortfolioConfig,
    http: Client,
    grpc_feed: Option<(GrpcFeed, tokio::task::JoinHandle<()>)>,
) {
    let (mut grpc_feed, mut feed_task): (Option<GrpcFeed>, Option<tokio::task::JoinHandle<()>>) =
        match grpc_feed {
            Some((f, h)) => (Some(f), Some(h)),
            None => (None, None),
        };

    // Event-driven exit dwell map: mint -> when a stop breach began (wick-confirm
    // arming). Owned here (not inside MomentumContext) so it persists across ticks
    // for both the fast-ticker exit arm and the gRPC-notify exit arm below. Inert
    // when MOMENTUM_GRPC_EXIT is off — `maybe_exit` only reads it under that flag.
    let stop_armed: std::sync::Arc<dashmap::DashMap<String, std::time::Instant>> =
        std::sync::Arc::new(dashmap::DashMap::new());

    let mut portfolio = match super::load_portfolio(&cfg.portfolio_path) {
        Ok(p) => p,
        Err(e) => {
            error!("portfolio watcher: failed to load portfolio: {e}");
            return;
        }
    };

    let history_path = Path::new(&cfg.history_path).to_path_buf();

    let mut history: VecDeque<PriceSnapshot> = match history::load_history(&history_path) {
        Ok(h) => {
            info!("portfolio: loaded {} historical snapshots", h.len());
            h
        }
        Err(e) => {
            warn!("portfolio: could not load history file: {e}");
            VecDeque::new()
        }
    };

    // Momentum trader: load the watched universe up front so its mints can be
    // warmed up (backfilled) and unioned into the price/history set. Empty when
    // the trader is disabled.
    let watched: Vec<WatchedToken> = if cfg.enable_momentum_trader {
        match momentum_universe::load(Path::new(&cfg.momentum_tokens_path)) {
            Ok(w) => {
                info!(
                    "momentum: watching {} tokens (DRY_RUN_MOMENTUM_TRADER={}, poll={}s, trail={}%, rank={})",
                    w.len(), cfg.momentum_dry_run, cfg.momentum_poll_secs, cfg.momentum_trail_pct,
                    cfg.momentum_rank_metric
                );
                if cfg.momentum_rank_metric != super::RankMetric::Sortino {
                    warn!(
                        "momentum: rank metric is '{}' — MOMENTUM_MIN_METRIC ({:.3}) and \
                         MOMENTUM_ROTATE_MARGIN ({:.3}) are in THIS metric's units; recalibrate \
                         them (a sortino-scaled 0.5 mis-gates other metrics). See the per-tick \
                         rank[...] log for live ranges.",
                        cfg.momentum_rank_metric, cfg.momentum_min_score, cfg.momentum_rotate_margin
                    );
                }
                w
            }
            Err(e) => {
                error!("momentum: failed to load {} ({e}); trader idle this run", cfg.momentum_tokens_path);
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    // Order-flow poller (DexScreener): one background task refreshing per-pool trade counts
    // and volume for every watched token, feeding the entry gates in `portfolio::flow`.
    // `MOMENTUM_FLOW_POLL_SECS=0` disables it entirely — no task, no HTTP — and every flow
    // gate then reads `None` and fails open.
    let flow_cache: Option<crate::portfolio::flow::FlowCache> =
        if cfg.enable_momentum_trader && cfg.momentum_flow_poll_secs > 0 {
            let cache = crate::portfolio::flow::FlowCache::new();
            crate::portfolio::flow::spawn_poller(
                cache.clone(),
                watched.clone(),
                cfg.momentum_flow_poll_secs,
            );
            Some(cache)
        } else {
            None
        };

    // Background REST price cache (MOMENTUM_REST_BG): one task refreshes Kraken SOL + a
    // DexScreener price per priced mint; the slow tick and the 1 s exit tick then read it
    // instead of fetching inline on the loop that evaluates the trailing stop.
    let rest_cache: Option<RestPriceCache> = if cfg.momentum_rest_bg {
        let cache = RestPriceCache::new();
        rest_prices::spawn_poller(cache.clone(), cfg.momentum_rest_poll_secs);
        Some(cache)
    } else {
        None
    };

    // Pairs trader config, loaded early so its legs join the price/backfill set below —
    // decoupling pairs pricing from the momentum watch list (a pairs leg need not be a
    // momentum token, nor momentum even enabled, to be priced).
    let pairs_cfg: Option<crate::portfolio::pairs_config::PairsConfig> =
        match crate::portfolio::pairs_config::PairsConfig::from_env() {
            Ok(c) => Some(c),
            Err(e) => { tracing::warn!("pairs trader disabled — config error: {e}"); None }
        };
    // Liquidation detection bot config (Phase A — paper). Loaded early so the klend sidecar
    // auto-launch below fires if either pairs OR liquidation needs it.
    let liq_cfg: Option<crate::portfolio::liquidation_config::LiquidationConfig> =
        match crate::portfolio::liquidation_config::LiquidationConfig::from_env() {
            Ok(c) => Some(c),
            Err(e) => { tracing::warn!("liquidation bot disabled — config error: {e}"); None }
        };
    // The pairs trader's own legs as watch entries (deduped by mint).
    let pairs_mints: Vec<WatchedToken> = match pairs_cfg.as_ref().filter(|c| c.enable) {
        Some(c) => {
            let mut v: Vec<WatchedToken> = Vec::new();
            for s in &c.pairs {
                for (sym, mint) in [(&s.symbol_a, &s.mint_a), (&s.symbol_b, &s.mint_b)] {
                    if !v.iter().any(|w| &w.mint == mint) {
                        v.push(WatchedToken { symbol: sym.clone(), mint: mint.clone(), name: None, equity: None, params: None, pool: None, quote: None, pools: None });
                    }
                }
            }
            v
        }
        None => Vec::new(),
    };

    // Backfill from Birdeye when the oldest snapshot is less than 7 days old.
    // backfill_birdeye now persists each mint's data incrementally so a crash
    // mid-backfill doesn't lose work already fetched.
    let now_ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let needs_backfill = history
        .front()
        .is_none_or(|oldest| oldest.ts > now_ts.saturating_sub(7 * 24 * 3600));
    if needs_backfill {
        if let Some(api_key) = &cfg.birdeye_api_key {
            backfill_birdeye(&http, api_key, &portfolio, &mut history, &history_path).await;
            // Let Birdeye's rate limiter recover before the first price fetch.
            tokio::time::sleep(Duration::from_secs(5)).await;
        } else {
            info!("portfolio: no BIRDEYE_API_KEY set, skipping history backfill");
        }
    }

    // Warm up any cold watched momentum mints — independent of the held-token
    // backfill above (a freshly-added token can be cold even when overall
    // history is deep). No-ops for already-warm mints and when there's no key,
    // so a new token is tradeable at boot instead of after a ~2h live warm-up.
    if cfg.enable_momentum_trader {
        if let Some(api_key) = &cfg.birdeye_api_key {
            backfill_watched_cold(&http, api_key, &watched, cfg.momentum_lookback_obs, &mut history, &history_path).await;
        }
    }

    // Warm up any cold *held* momentum positions read from state. A position whose
    // token is neither curated nor currently in the discovered top-N (most acutely a
    // PAPER position, which has no wallet backing to keep it in `portfolio.tokens`)
    // would otherwise fall out of the priced set and never re-warm — stranding it in
    // `warming` forever, where the score-based exits/rotation can't act on it while it
    // still consumes a position slot. Backfilling here (+ folding held mints into
    // `token_mints` below) restores its history so it becomes rankable again.
    let held_at_start: Vec<WatchedToken> =
        if cfg.enable_momentum_trader { held_mints_from_state(&cfg) } else { Vec::new() };
    if !held_at_start.is_empty() {
        if let Some(api_key) = &cfg.birdeye_api_key {
            backfill_watched_cold(&http, api_key, &held_at_start, cfg.momentum_lookback_obs, &mut history, &history_path).await;
        }
    }

    // Warm pairs legs independently — a leg that isn't a momentum token still gets its
    // history backfilled (no-op for mints already warm or held).
    if !pairs_mints.is_empty() {
        if let Some(api_key) = &cfg.birdeye_api_key {
            let lb = pairs_cfg.as_ref().map(|c| c.lookback_obs).unwrap_or(240);
            backfill_watched_cold(&http, api_key, &pairs_mints, lb, &mut history, &history_path).await;
        }
    }

    let mut token_mints: Vec<String> = portfolio.tokens.iter().map(|t| t.mint.clone()).collect();
    for w in &watched {
        if !token_mints.contains(&w.mint) {
            token_mints.push(w.mint.clone());
        }
    }
    // Pairs legs join the priced set so the pairs trader prices its own legs even when
    // they aren't momentum tokens (or momentum is disabled).
    for w in &pairs_mints {
        if !token_mints.contains(&w.mint) {
            token_mints.push(w.mint.clone());
        }
    }
    // Held positions join the priced set so an open position whose token isn't curated
    // and has rolled off the discovered top-N keeps getting a live price (and thus keeps
    // accumulating history / staying rankable). Without this a paper position strands in
    // `warming` and blocks its slot. Deduped — a live position already present via the
    // wallet scan is a no-op.
    for w in &held_at_start {
        if !token_mints.contains(&w.mint) {
            token_mints.push(w.mint.clone());
        }
    }

    let mut known_price_keys = build_known_price_keys(&token_mints);

    // Decimals for watched + held + USDC, used for raw↔human conversions in the
    // trader. Seeded once here; SELF-HEALED every slow tick for mints that join the
    // priced set later (adopted unwatched holdings, entered discoveries) — the seed
    // alone left such mints permanently unsized: every exit/rotation for them failed
    // with "missing decimals" and the position wedged in a stop-retry loop (the KIO
    // incident, 2026-08-10). Missing decimals → that candidate/exit is skipped until
    // the next tick's refresh fills it.
    let mut decimals: HashMap<String, u8> = if cfg.enable_momentum_trader {
        let mut mints = token_mints.clone();
        mints.push(momentum_universe::USDC_MINT.to_string());
        match scanner::fetch_decimals_for_mints(&cfg.rpc_url, mints).await {
            Ok(m) => {
                info!("momentum: cached decimals for {} mints", m.len());
                m
            }
            Err(e) => {
                warn!("momentum: decimals fetch failed ({e}); entries will be skipped");
                HashMap::new()
            }
        }
    } else {
        HashMap::new()
    };

    // Seed last_prices from the most recent history snapshot so the first tick
    // never shows €0 if fetch_prices fails before any data is collected. Seeded HERE
    // (ahead of the startup reconcile) rather than after it, because the reconcile now
    // needs a price map: an invalidated position is booked at its last known price, and
    // an empty map would mark every startup drop at the `close_mark` fallback instead.
    // Nothing between the history load above and this point touches `history`, so the
    // snapshot read is the same one it was.
    let mut last_prices: HashMap<String, f64> = history
        .back()
        .map(|snap| snap.prices.clone())
        .unwrap_or_default();
    // WHEN that seed was captured — the reconcile books invalidations at these prices, so
    // it must be able to refuse a pre-outage snapshot rather than write a phantom realized
    // loss into the never-resetting loss breaker. A u64 too large for i64 is not a real
    // timestamp; mapping it to i64::MAX reads as "stamped in the future" ⇒ distrusted,
    // which is the same fail-closed direction.
    let seed_mark_ts: Option<i64> =
        history.back().map(|snap| i64::try_from(snap.ts).unwrap_or(i64::MAX));

    // Reconcile any recorded position against the freshly-scanned wallet so the
    // trader never resumes managing a phantom (stale live position). The wallet
    // snapshot only NOMINATES: each unbacked mint is re-read on-chain and dropped only
    // on a confirmed zero (same `confirm_and_close` core as the mid-run tick), so a
    // partial scan at boot can no longer disarm a live position's trailing stop.
    // Ordered BEFORE both adoption passes below so a mint dropped here cannot be
    // re-adopted on the same boot — and `stop_armed` is passed so a real drop releases
    // its dwell-arm entry.
    momentum::reconcile_startup_position(
        &cfg,
        &portfolio,
        &last_prices,
        seed_mark_ts,
        Some(&stop_armed),
    )
    .await;

    let analysis_cfg = AnalysisConfig {
        alert_pct_5m: cfg.alert_pct_5m,
        alert_pct_1h: cfg.alert_pct_1h,
        zscore_lambda: cfg.zscore_lambda,
        zscore_threshold: cfg.zscore_threshold,
        zscore_min_obs: cfg.zscore_min_obs,
        price_thresholds: cfg.price_thresholds.clone(),
        price_ceilings: cfg.price_ceilings.clone(),
    };
    let cooldown = Duration::from_secs(cfg.alert_cooldown_min * 60);

    // Per-asset cooldown map: each asset tracks its own last-email time independently.
    let mut last_alert_per_asset: HashMap<String, Instant> = HashMap::new();

    let mut last_price_update: HashMap<String, Instant> = HashMap::new();
    // Per-watched-mint count of consecutive ticks whose price was carried forward
    // (absent from this tick's fresh fetch). A frozen price silently corrupts the
    // momentum ranking, so we warn when the streak crosses a threshold and log the
    // recovery. Distinct from the wall-clock staleness in log_values, which only
    // covers held portfolio tokens — a watched-but-unheld candidate (we're FLAT) is
    // invisible to that check.
    let mut carry_forward_streak: HashMap<String, u32> = HashMap::new();

    // If FLAT, optionally adopt a manually-acquired wallet holding (gated by
    // MOMENTUM_ADOPT_WALLET_POSITION) so the trader manages it. Uses the seeded
    // last_prices for the current price; no-op unless exactly one watched holding
    // worth ≥ half the trade size is present. This startup call gives an immediate
    // adoption on boot; the loop also re-checks every slow tick (see "Step 0" below),
    // so a holding bought AFTER startup is adopted without a restart.
    if cfg.enable_momentum_trader {
        momentum::adopt_wallet_position(&cfg, &portfolio, &last_prices, &watched).await;
        momentum::adopt_unwatched_holdings(&cfg, &portfolio, &last_prices, &watched, &http).await;
    }

    // EUR/USD rate — fetched once at startup, refreshed every 10 ticks.
    let mut eur_rate = match pricer::fetch_eur_rate(&http).await {
        Ok(r) => { info!("portfolio: EUR rate = {r:.4}"); r }
        Err(e) => { warn!("portfolio: EUR rate fetch failed: {e}, defaulting to 0.92"); 0.92 }
    };
    let mut ticks_since_eur_refresh = 0u32;

    // SMA — computed from local history (free, no API calls).
    // Falls back to Birdeye only when local history is too thin (bot just started).
    let local_sma = pricer::compute_sma_from_history(&history, &portfolio);
    let mut monthly_sma = if local_sma.len() >= 2 {
        info!("portfolio: SMA computed from local history for {} assets", local_sma.len() / 2);
        local_sma
    } else if let Some(api_key) = &cfg.birdeye_api_key {
        info!("portfolio: local history too thin — trying Birdeye for initial SMA");
        let sma = pricer::fetch_monthly_sma(&http, api_key, &portfolio).await;
        info!("portfolio: SMA fetched from Birdeye for {} assets", sma.len() / 2);
        sma
    } else {
        info!("portfolio: no history and no BIRDEYE_API_KEY — swap suggestions disabled");
        HashMap::new()
    };
    let mut ticks_since_sma_refresh = 0u32;
    let mut ticks_since_history_rewrite = 0u32;

    // Periodic on-chain wallet re-scan: external funding / swaps are reflected
    // without a restart (the momentum entry gate reads the scanned USDC balance).
    let mut ticks_since_rescan = 0u32;

    // Live token discovery overlay (momentum only; opt-in). `discovered` is the
    // rolling top-N from scan_tokens.js; `effective` = curated ∪ discovered ∪ held,
    // recomputed each monitor tick and shared by the entry + fast-exit paths. When
    // scanning is off, `discovered` stays empty — but `effective` is still NOT just
    // `watched`: held mints join it unconditionally (c415ea0), which is what keeps an
    // adopted-UNWATCHED position rankable (and therefore evictable) at all.
    let mut discovered: Vec<WatchedToken> = Vec::new();
    // pool → DexScreener dexId for the current discovered set, kept in lockstep with
    // `discovered` so dynamic-wiring can dispatch each pool to the right decoder fetcher.
    let mut pool_dex: HashMap<String, String> = HashMap::new();
    // Pool ids currently wired into the gRPC feed (dynamic wiring, spec 2026-07-22) —
    // compared against `dynamic_pool_set(&discovered)` ∪ the adopted-holding venues every
    // slow tick, so an unchanged set (the common case) never triggers a feed re-spawn.
    let mut wired_dynamic: HashSet<String> = HashSet::new();
    // Adopted UNWATCHED holdings (MOMENTUM_ADOPT_ALL_TOKENS, spec 2026-08-09) → the venue
    // resolved for each from DexScreener. Such a position is absent from the curated file,
    // so it carries no pool/quote and would otherwise be REST-priced for its whole life.
    // Resolved once per mint, dropped when the position closes.
    let mut adopted_pools: HashMap<String, pricer::ResolvedPool> = HashMap::new();
    // Adopted mints with no wireable venue → when that lookup last failed. Holds off both
    // the LOG and the RE-QUERY: this runs inline on the slow-tick arm that also drives
    // exits, and a serial 15 s-timeout DexScreener GET per unresolvable mint every 60 s
    // would push minutes of latency in front of `fetch_prices` during an outage. A mint's
    // FIRST attempt is always immediate; only retries wait. Cleared on success/close.
    let mut adopted_venue_failed: HashMap<String, Instant> = HashMap::new();
    // Last dynamic-wire attempt that did NOT fully succeed, with its `want` set. Because
    // the wiring block now runs every slow tick (not only on a scan tick), a pool that
    // fails to decode would otherwise re-decode AND re-spawn the feed every 60 s — throwing
    // away the live price map each time. A repeat of the SAME failed set is held off for
    // `wire_retry` below; a changed set always attempts immediately.
    let mut last_wire_failure: Option<(HashSet<String>, Instant)> = None;
    // Decode-failure bookkeeping per pool, for the `WIRE_FAIL_STRIKES` bench + its cool-down.
    let mut wire_fail_strikes: HashMap<String, WireFail> = HashMap::new();
    // Hold-off between repeated attempts at something that already failed — both the
    // dynamic-wire retry above and an adopted mint's venue lookup. Keyed to the discovery
    // scan interval so the failure cadence is exactly what it was before the wiring block
    // was hoisted out of the scan tick; floored at one monitor tick.
    let wire_retry = Duration::from_secs(cfg.momentum_scan_interval_secs.max(60));
    let mut effective: Vec<WatchedToken> = watched.clone();
    // Scan cadence in 60s monitor ticks (floored to 1 so a tiny interval can't div to 0).
    let scan_every_ticks = (cfg.momentum_scan_interval_secs / 60).max(1);
    // Pre-armed so the first eligible monitor tick scans (warm start), then hourly.
    let mut ticks_since_scan = scan_every_ticks;
    // Monitor-tick health (tick_timing): start-to-start gap + per-phase durations. The
    // trailing stop is evaluated by this same loop, so a long phase here IS a blind stop.
    let mut last_tick_start: Option<Instant> = None;
    let mut last_gap_alert: Option<Instant> = None;
    // Background discovery (MOMENTUM_SCAN_BG): the scan child and the cold warm-up run on
    // their own tasks and post results here; the tick only `try_recv`s.
    let (scan_tx, mut scan_rx) = tokio::sync::mpsc::channel::<ScanMsg>(8);
    let mut scan_inflight: Option<(tokio::task::JoinHandle<()>, Instant)> = None;
    // Background wallet re-scan (MOMENTUM_WALLET_BG): snapshots over a watch channel.
    let rescan_every: u32 = if cfg.momentum_adopt_all_tokens { 1 } else { 5 };
    let wallet_rx = if cfg.momentum_wallet_bg {
        Some(spawn_wallet_poller(cfg.clone(), http.clone(), Duration::from_secs(60 * u64::from(rescan_every))))
    } else {
        None
    };
    let mut last_wallet_seq: u64 = 0;
    // When the last LIVE fill mutated the in-memory portfolio; a wallet snapshot taken
    // before it would roll that mutation back and is skipped.
    let mut last_fill_at: Option<Instant> = None;
    let mut wallet_stale_logged = false;

    // Auto-launch the klend-builder sidecar (mirrors dex::jupiter::spawn_metis) when
    // PAIRS_KLEND_BUILDER_DIR is set, so the borrowability/APY/health gate has a backend
    // without a second process to babysit. Stopped explicitly at shutdown (below).
    // Shared klend sidecar: either the pairs gate OR the liquidation scanner can need it, so
    // launch a single instance if either is enabled. The builder dir comes from the pairs
    // config (or PAIRS_KLEND_BUILDER_DIR directly); the URL/market from whichever is set.
    let mut klend_sidecar: Option<tokio::process::Child> = None;
    let pairs_wants = pairs_cfg.as_ref().is_some_and(|c| c.enable);
    let liq_wants = liq_cfg.as_ref().is_some_and(|c| c.enable);
    if pairs_wants || liq_wants {
        let builder_dir = pairs_cfg.as_ref().and_then(|c| c.klend_builder_dir.clone())
            .or_else(|| std::env::var("PAIRS_KLEND_BUILDER_DIR").ok().filter(|s| !s.is_empty()));
        let sidecar_url = pairs_cfg.as_ref().filter(|c| c.enable).map(|c| c.klend_sidecar_url.clone())
            .or_else(|| liq_cfg.as_ref().map(|c| c.klend_sidecar_url.clone()))
            .unwrap_or_else(|| "http://127.0.0.1:8181".to_string());
        if let Some(dir) = builder_dir {
            let rpc = std::env::var("RPC_URL").unwrap_or_default();
            let market = std::env::var("KLEND_MARKET")
                .unwrap_or_else(|_| crate::portfolio::kamino::XSTOCKS_MARKET.to_string());
            let port = crate::portfolio::kamino::sidecar_port(&sidecar_url).unwrap_or(8181);
            // Idempotent: if a healthy sidecar is already on this port (another watcher
            // instance, or a manually-run one), reuse it rather than spawning a duplicate
            // that crashes with EADDRINUSE. Leaving klend_sidecar=None means shutdown won't
            // kill a sidecar this process didn't start.
            if crate::portfolio::kamino::wait_until_ready(&sidecar_url, 1).await {
                info!("klend-builder already running on :{port} — reusing it (not spawning a duplicate)");
            } else {
                match crate::portfolio::kamino::spawn_klend_sidecar(&dir, &rpc, &market, port) {
                    Ok(child) => {
                        info!("Launched klend-builder sidecar from {dir} on :{port} (market {market})");
                        klend_sidecar = Some(child);
                        if crate::portfolio::kamino::wait_until_ready(&sidecar_url, 30).await {
                            info!("klend-builder ready");
                        } else {
                            warn!("klend-builder not ready after 30s — pairs/liquidation fail-safe until it is");
                        }
                    }
                    Err(e) => warn!("klend-builder auto-launch skipped: {e}"),
                }
            }
        } else if liq_wants {
            info!("liquidation: no builder dir set — expecting an externally-run klend sidecar at {sidecar_url}");
        }
    }

    // One-time pairs realized-P&L summary at startup, so the running total is visible on
    // boot (it otherwise only prints when a trade closes).
    if let Some(pcfg) = pairs_cfg.as_ref().filter(|c| c.enable) {
        if let Ok(st) = crate::portfolio::pairs_state::load(Path::new(&pcfg.state_path)) {
            crate::portfolio::pairs_trader::log_pnl_summary(&st);
        }
    }

    // interval_at delays the first tick by the full period so it doesn't
    // fire immediately on top of the backfill requests.
    let start = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut ticker = tokio::time::interval_at(start, Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Fast cadence for the momentum trailing-stop EXIT check. Decoupled from the
    // 60s monitoring tick so it never rescales history/alert windows. When the
    // trader is disabled we arm a slow heartbeat just to keep the select! shape.
    let fast_secs = if cfg.enable_momentum_trader { cfg.momentum_poll_secs.max(1) } else { 3600 };
    let mut fast_ticker = tokio::time::interval(Duration::from_secs(fast_secs));
    fast_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // SIGTERM stream (systemctl stop / supervisors), created once. SIGINT (Ctrl-C) is
    // handled inline in the shutdown arm below; both break the loop into graceful cleanup.
    #[cfg(unix)]
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| warn!("SIGTERM handler unavailable, Ctrl-C only: {e}"))
        .ok();

    // Last logged gRPC/REST pricing split (watched symbols). Logged only when it
    // changes, so a 1s poll doesn't spam — you see one line at first coverage and
    // again whenever a wired token drops to REST (stream stale) or recovers.
    let mut last_pricing_sig: Option<String> = None;

    // Last logged loss-breaker halt reason. Logged only on transition (mirrors
    // last_pricing_sig) so a halted trader is never *silently* inert: one banner
    // when the halt is first seen — including at startup, when the sticky halt file
    // already exists from a prior run and `halted()` would otherwise short-circuit
    // every tick with no output — and one line when the halt clears. The breaker's
    // own `error!` fires only at the instant it trips, so without this a restart
    // into a pre-existing halt shows nothing at all.
    let mut last_halt_reason: Option<String> = None;

    // Take sole ownership of the spike-signal receiver (Approach B fast-entry). The
    // ingestion task holds the Sender via the shared GrpcFeed clone; here we drain the
    // mint-signals in a dedicated select! arm. `None` when the feed is absent or spikes
    // are off → the arm's future is pending() and never fires.
    let mut spike_rx = grpc_feed
        .as_ref()
        .and_then(|f| f.spike_rx.lock().ok().and_then(|mut g| g.take()));

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = fast_ticker.tick() => {
                // EXIT-first fast path; only acts when HOLDING. The mctx borrows
                // are released before we mutate `portfolio` on a live fill. This is
                // the backstop exit path — always armed, unaffected by MOMENTUM_GRPC_EXIT.
                if cfg.enable_momentum_trader {
                    let outcomes = {
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &effective, prices_usd: &last_prices,
                            history: &history, decimals: &decimals, http: &http,
                            usdc_balance: usdc_balance(&portfolio),
                            grpc_feed: grpc_feed.as_ref(), stop_armed: Some(&stop_armed), flow: flow_cache.as_ref(), rest_prices: rest_cache.as_ref(),
                        };
                        momentum::maybe_exit(&mctx).await
                    };
                    if apply_exit_outcomes(&mut portfolio, outcomes, "exit tick") { last_fill_at = Some(Instant::now()); }
                    // Fast-tick ENTRY retry (MOMENTUM_ENTRY_RETRY_SECS): once a
                    // reverted entry's deadline passes, re-attempt it here at the
                    // escalated tolerance instead of waiting for the next slow
                    // tick. Cheap no-op when the feature is off or nothing is due.
                    if cfg.momentum_entry_retry_secs > 0 {
                        let retry_outcomes = {
                            let mctx = MomentumContext {
                                cfg: &cfg, watched: &effective, prices_usd: &last_prices,
                                history: &history, decimals: &decimals, http: &http,
                                usdc_balance: usdc_balance(&portfolio),
                                grpc_feed: None, stop_armed: None, flow: flow_cache.as_ref(), rest_prices: rest_cache.as_ref(),
                            };
                            momentum::maybe_retry_entry(&mctx).await
                        };
                        match retry_outcomes {
                            Ok(os) => for o in os { if !o.dry_run() { apply_outcome(&mut portfolio, &o); last_fill_at = Some(Instant::now()); } },
                            Err(e) => error!("momentum: entry-retry tick error: {e:#}"),
                        }
                    }
                }
                continue;
            }
            // Event-driven EXIT re-eval, woken by the gRPC ingestion task when a HELD
            // token's on-chain price updates (GrpcFeed::note_update -> notify_one()).
            // Guarded on the flag AND feed presence so with MOMENTUM_GRPC_EXIT off (or
            // no gRPC feed configured) this branch's future is `pending()` and never
            // fires — the fast ticker above remains the sole exit path, byte-identical
            // to pre-Task-4 behavior. Mutually exclusive with every other arm (including
            // the fast ticker) via select!, so there is no race on `portfolio`/`stop_armed`.
            _ = async {
                match &grpc_feed {
                    Some(f) => f.notify.notified().await,
                    None => std::future::pending().await,
                }
            }, if cfg.momentum_grpc_exit && grpc_feed.is_some() => {
                if cfg.enable_momentum_trader {
                    let outcomes = {
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &effective, prices_usd: &last_prices,
                            history: &history, decimals: &decimals, http: &http,
                            usdc_balance: usdc_balance(&portfolio),
                            grpc_feed: grpc_feed.as_ref(), stop_armed: Some(&stop_armed), flow: flow_cache.as_ref(), rest_prices: rest_cache.as_ref(),
                        };
                        momentum::maybe_exit(&mctx).await
                    };
                    if apply_exit_outcomes(&mut portfolio, outcomes, "grpc-notify exit") { last_fill_at = Some(Instant::now()); }
                }
                continue;
            }
            // Event-driven spike ENTRY (Approach B), woken by the ingestion task when a
            // watched token's gRPC price jumps up past MOMENTUM_SPIKE_BPS within
            // MOMENTUM_SPIKE_WINDOW_SECS. Runs the *normal validated* entry decision for
            // that one mint NOW instead of waiting up to 60s for the slow tick. Guarded on
            // the flag + trader + feed; with spikes off the future is pending() and this
            // never fires (byte-identical to today). Mutually exclusive with every other
            // arm via select!, so there is no race on `portfolio`/state.
            mint = async {
                match &mut spike_rx {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if cfg.momentum_spike_entry && cfg.enable_momentum_trader && grpc_feed.is_some() => {
                if let Some(mint) = mint {
                    // Overlay the freshest gRPC price for the spiking mint onto the last
                    // 60s snapshot so the Candidate's CURRENT price reflects the spike; the
                    // rank window still comes from `history` (correct — the spike must not
                    // manufacture a passing metric, only accelerate one that already passes).
                    let mut spike_prices = last_prices.clone();
                    if let Some(f) = grpc_feed.as_ref() {
                        if let Some(e) = f.map.get(&mint) {
                            let (usd, _) = *e.value();
                            if usd > 0.0 {
                                spike_prices.insert(mint.clone(), usd);
                            }
                        }
                    }
                    let outcomes = {
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &effective, prices_usd: &spike_prices,
                            history: &history, decimals: &decimals, http: &http,
                            usdc_balance: usdc_balance(&portfolio),
                            grpc_feed: grpc_feed.as_ref(), stop_armed: None, flow: flow_cache.as_ref(), rest_prices: rest_cache.as_ref(),
                        };
                        momentum::maybe_enter_spike(&mctx, &mint, cfg.momentum_spike_shadow).await
                    };
                    match outcomes {
                        Ok(os) => for o in os { if !o.dry_run() { apply_outcome(&mut portfolio, &o); last_fill_at = Some(Instant::now()); } },
                        Err(e) => error!("momentum: spike entry error: {e:#}"),
                    }
                }
                continue;
            }
            _ = async {
                #[cfg(unix)]
                {
                    if let Some(term) = sigterm.as_mut() {
                        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
                    } else {
                        let _ = tokio::signal::ctrl_c().await;
                    }
                }
                #[cfg(not(unix))]
                { let _ = tokio::signal::ctrl_c().await; }
            } => {
                info!("portfolio: shutting down (SIGINT/SIGTERM) — persisting final history");
                if let Err(e) = history::rewrite_history(&history_path, &history) {
                    warn!("portfolio: final history flush failed: {e}");
                }
                break;
            }
        }

        // ── Tick health: measure this tick, alert on the gap since the previous one ──
        let tick_start = Instant::now();
        let tick_gap = tick_timing::gap_secs(last_tick_start, tick_start);
        last_tick_start = Some(tick_start);
        let mut timer = TickTimer::start_at(tick_start);
        if tick_timing::gap_alert_due(
            tick_gap, cfg.momentum_max_tick_gap_secs, last_gap_alert, tick_start, GAP_ALERT_COOLDOWN,
        ) {
            warn!(
                "portfolio: monitor loop did not tick for {tick_gap}s (limit {}s) — the trailing stop was blind meanwhile",
                cfg.momentum_max_tick_gap_secs
            );
            let subject = format!("[portfolio-watcher] monitor tick gap {tick_gap}s — trailing stop was blind");
            let body = format!(
                "The watcher's monitor loop did not tick for {tick_gap}s (limit {}s).\nLook at the TickTiming records in {} for the phase that blocked.",
                cfg.momentum_max_tick_gap_secs, cfg.momentum_actions_path
            );
            match emailer::send_alert(&cfg, &subject, &body).await {
                Ok(_) => last_gap_alert = Some(tick_start),
                Err(e) => warn!("portfolio: tick-gap alert email failed: {e:#}"),
            }
        }

        // Re-scan the wallet on-chain every 5 ticks (~5 min) so external funding /
        // swaps are picked up without a restart. `scan_and_save` rewrites
        // portfolio.json and its merge() drops sold tokens + refreshes balances from
        // chain — so the momentum entry gate sees the true current USDC. The RPC runs
        // on a blocking thread inside scan_wallet, so this `.await` never stalls the
        // select! loop; the invalidation confirm reads below (Step 0) are batched via
        // join_all and hard-capped at 8 s each, so the worst-case loop stall is one
        // timeout window, not N×30 s. With MOMENTUM_ADOPT_ALL_TOKENS the scan runs EVERY tick
        // (~60 s): the whole point of that mode is adopting a manual buy promptly,
        // and a 5-tick cadence adds up to 4 minutes of invisible-wallet latency
        // for the price of one extra RPC call per minute.
        // Source of the fresh wallet state: the background poller's latest snapshot (flag
        // on — nothing awaited here) or the inline bounded scan (flag off).
        let wallet_update: Option<Portfolio> = if let Some(rx) = &wallet_rx {
            let latest = rx.borrow().clone();
            match latest {
                Some(snap) if snap.seq != last_wallet_seq => {
                    last_wallet_seq = snap.seq;
                    if snapshot_predates_fill(snap.taken, last_fill_at) {
                        info!("portfolio: wallet snapshot predates a live fill — waiting for the next one");
                        None
                    } else {
                        Some(snap.portfolio)
                    }
                }
                _ => None,
            }
        } else {
            ticks_since_rescan += 1;
            if ticks_since_rescan >= rescan_every {
                ticks_since_rescan = 0;
                match bounded(cfg.momentum_wallet_scan_timeout_secs, "wallet re-scan", scanner::scan_and_save(&cfg, &http)).await {
                    Ok(p) => Some(p),
                    Err(e) => {
                        warn!("portfolio: periodic wallet re-scan failed: {e}");
                        None
                    }
                }
            } else {
                None
            }
        };
        if let Some(new_p) = wallet_update {
            {
                {
                    let changed = holdings_changed(&portfolio, &new_p);
                    let usdc = usdc_balance(&new_p);
                    portfolio = new_p;
                    token_mints = portfolio.tokens.iter().map(|t| t.mint.clone()).collect();
                    // Re-union watched + pairs + discovered mints so a re-scan doesn't drop them from pricing.
                    for w in watched.iter().chain(pairs_mints.iter()).chain(discovered.iter()) {
                        if !token_mints.contains(&w.mint) {
                            token_mints.push(w.mint.clone());
                        }
                    }
                    // Held positions (from state) re-join too, so a paper position that
                    // has rolled off the discovered top-N keeps getting priced (and stays
                    // rankable) instead of stranding in `warming` and blocking its slot.
                    for w in &held_mints_from_state(&cfg) {
                        if !token_mints.contains(&w.mint) {
                            token_mints.push(w.mint.clone());
                        }
                    }
                    known_price_keys = build_known_price_keys(&token_mints);
                    if changed {
                        info!("portfolio: wallet re-scanned — holdings CHANGED ({} tokens, {:.2} USDC available)",
                            portfolio.tokens.len(), usdc);
                    } else {
                        info!("portfolio: wallet re-scanned — unchanged ({:.2} USDC available)", usdc);
                    }
                }
            }
        }
        timer.lap("wallet_scan");

        // Periodic generic token scan → rolling in-memory top-N discovery overlay
        // (momentum only; opt-in). One-shot `node scan_tokens.js --json`; best-effort —
        // a failed/slow scan logs and keeps the prior `discovered`. Curated file untouched.
        // Scan results to apply this tick: from the inline bounded run (flag off) or from
        // the background task's channel (MOMENTUM_SCAN_BG — nothing awaited on this tick).
        let mut scan_results: Vec<ScanMsg> = Vec::new();
        if cfg.enable_momentum_trader && cfg.momentum_scan_enable {
            ticks_since_scan += 1;
            if ticks_since_scan >= scan_every_ticks {
                ticks_since_scan = 0;
                if cfg.momentum_scan_bg {
                    let running = match &scan_inflight {
                        Some((h, started)) if !h.is_finished() => {
                            if scan_overdue(*started, Instant::now(), cfg.momentum_scan_interval_secs) {
                                warn!(
                                    "momentum: background token scan running for {}s — aborting it",
                                    started.elapsed().as_secs()
                                );
                                h.abort();
                                false
                            } else {
                                true
                            }
                        }
                        _ => false,
                    };
                    if running {
                        info!("momentum: token scan still running — not starting another");
                    } else {
                        let tx = scan_tx.clone();
                        let script = cfg.momentum_scan_script.clone();
                        let top_n = cfg.momentum_scan_top_n;
                        // Off the loop, a slow scan costs nothing but its own lateness, so let it
                        // run up to its whole interval (the smoke run measured > 120 s); the
                        // `scan_overdue` abort at 2× the interval is the backstop.
                        let cap = cfg.momentum_scan_timeout_secs.max(cfg.momentum_scan_interval_secs);
                        let handle = tokio::spawn(async move {
                            match bounded(cap, "token scan", run_token_scan(&script, top_n)).await {
                                Ok((found, found_dex)) => {
                                    let _ = tx.send(ScanMsg::Discovered { found, found_dex }).await;
                                }
                                Err(e) => warn!("momentum: background token scan failed ({e}); keeping prior discoveries"),
                            }
                        });
                        scan_inflight = Some((handle, Instant::now()));
                    }
                } else {
                    match bounded(cfg.momentum_scan_timeout_secs, "token scan", run_token_scan(&cfg.momentum_scan_script, cfg.momentum_scan_top_n)).await {
                        Ok((found, found_dex)) => scan_results.push(ScanMsg::Discovered { found, found_dex }),
                        Err(e) => warn!("momentum: token scan failed ({e}); keeping {} discovered", discovered.len()),
                    }
                }
            }
            while let Ok(msg) = scan_rx.try_recv() {
                scan_results.push(msg);
            }
        }
        for msg in scan_results {
            match msg {
                ScanMsg::Discovered { found, found_dex } => {
                    if discovered_changed(&discovered, &found) {
                        discovered = found;
                        pool_dex = found_dex;
                        let syms: Vec<&str> = discovered.iter().map(|w| w.symbol.as_str()).collect();
                        info!("momentum: scan → discovered {:?}", syms);
                        // Warm cold new entrants so they are rankable immediately (no-op for
                        // mints already warm/held). The candle fetch is the network half; the
                        // merge into `history` always happens here, on this task.
                        if let Some(api_key) = &cfg.birdeye_api_key {
                            let cold = cold_watched(&discovered, &history);
                            if !cold.is_empty() {
                                info!("momentum: warming up {} cold watched token(s) via Birdeye", cold.len());
                                if cfg.momentum_scan_bg {
                                    let tx = scan_tx.clone();
                                    let http = http.clone();
                                    let api_key = api_key.clone();
                                    let lookback = cfg.momentum_lookback_obs;
                                    tokio::spawn(async move {
                                        let snaps = fetch_cold_candles(&http, &api_key, &cold, lookback).await;
                                        let _ = tx.send(ScanMsg::Backfill { snaps }).await;
                                    });
                                } else {
                                    // Bounded: the Birdeye pager sleeps ~1.1 s per page and this
                                    // runs on the exit loop. A timeout leaves `history` untouched;
                                    // the tokens simply warm from live ticks instead.
                                    match tokio::time::timeout(
                                        BACKFILL_TIMEOUT,
                                        fetch_cold_candles(&http, api_key, &cold, cfg.momentum_lookback_obs),
                                    )
                                    .await
                                    {
                                        Ok(snaps) => apply_backfill(&mut history, snaps, &history_path),
                                        Err(_) => warn!(
                                            "momentum: cold backfill of discovered tokens timed out after {}s — they warm from live ticks",
                                            BACKFILL_TIMEOUT.as_secs()
                                        ),
                                    }
                                }
                            }
                        }
                        // Fold discovered mints into the priced set for this tick onward.
                        for w in &discovered {
                            if !token_mints.contains(&w.mint) {
                                token_mints.push(w.mint.clone());
                            }
                        }
                        known_price_keys = build_known_price_keys(&token_mints);
                    } else {
                        info!("momentum: scan → no change ({} discovered)", discovered.len());
                    }
                }
                ScanMsg::Backfill { snaps } => apply_backfill(&mut history, snaps, &history_path),
            }
        }
        timer.lap("scan");

        // Resolve a gRPC venue for each adopted UNWATCHED holding (spec 2026-08-09).
        // Such a position exists only in the trader's state file — it is in no curated
        // entry and in no scan discovery — so nothing else can give it a pool/quote, and
        // `spawn_grpc_feed` wires strictly through `pool_refs()`. One DexScreener lookup
        // per mint, cached for the position's life; failures fail open (REST pricing, which
        // is what the position already has) and are logged once per streak.
        if cfg.enable_momentum_trader && cfg.momentum_grpc_pricing {
            let adopted_mints: HashSet<String> =
                momentum_state::load(Path::new(&cfg.momentum_state_path))
                    .map(|s| {
                        s.positions
                            .into_iter()
                            .filter(|p| p.adopted_unwatched)
                            .map(|p| p.mint)
                            .collect()
                    })
                    .unwrap_or_default();
            // Closed positions release their venue (and their failure-log suppression, so a
            // re-adoption logs again rather than being silently mute).
            adopted_pools.retain(|m, _| adopted_mints.contains(m));
            adopted_venue_failed.retain(|m, _| adopted_mints.contains(m));
            let now = Instant::now();
            for mint in &adopted_mints {
                if adopted_pools.contains_key(mint) {
                    continue;
                }
                // Already wired by the curated file (the operator added the mint after it
                // was adopted): that wiring is authoritative and `overlay_adopted_pools`
                // would skip the entry anyway — resolving here would only subscribe a
                // second, unused pool. No lookup, no log.
                if watched.iter().any(|w| w.mint == *mint && !w.pool_refs().is_empty()) {
                    continue;
                }
                // A mint that just failed is not re-queried until `wire_retry` has passed —
                // this loop is a SERIAL, blocking HTTP walk sitting in front of the price
                // fetch and the exit logic on the same tick.
                if within_holdoff(adopted_venue_failed.get(mint).copied(), now, wire_retry) {
                    continue;
                }
                match tokio::time::timeout(VENUE_RESOLVE_TIMEOUT, pricer::resolve_best_pool(&http, mint)).await.ok().flatten() {
                    Some(r) => {
                        info!(
                            "momentum: adopted {} → gRPC pool {} ({}, quote {})",
                            mint, r.pool, r.dex, r.quote
                        );
                        adopted_venue_failed.remove(mint);
                        adopted_pools.insert(mint.clone(), r);
                    }
                    None => {
                        if adopted_venue_failed.insert(mint.clone(), now).is_none() {
                            info!(
                                "momentum: adopted {mint} — no wireable DexScreener venue; staying REST-priced"
                            );
                        }
                    }
                }
            }
        }

        timer.lap("venues");

        // Dynamic gRPC wiring (spec 2026-07-22, extended 2026-08-09): pools that are not in
        // pools.json get vault subscriptions by re-spawning the feed with their ad-hoc
        // decoded PoolConfigs merged in. Two sources feed it: scan discoveries carrying a
        // pool, and the venues resolved above for adopted unwatched holdings. pools.json is
        // never written; an unchanged pool set → no rebuild (the common case: the same
        // top-N rediscovered hourly, and a stable set of adopted positions). Runs every slow
        // tick rather than only on a scan tick, so an adoption is wired even with
        // MOMENTUM_SCAN_ENABLE=false — with no discoveries and no adoptions `want` is empty
        // and matches `wired_dynamic`, so the block does nothing at all.
        if cfg.momentum_grpc_pricing {
            let now = Instant::now();
            let mut want = dynamic_pool_set(&discovered);
            want.extend(adopted_pools.values().map(|r| r.pool.clone()));
            // A pool that no longer appears anywhere forgets its decode failures.
            wire_fail_strikes.retain(|p, _| want.contains(p));
            // …one that has failed `WIRE_FAIL_STRIKES` times in a row is benched, so `want`
            // converges on the decodable subset instead of holding the change-gate open
            // (and re-spawning the feed) forever — and one whose bench has expired comes
            // back for a fresh attempt cycle, so no exclusion is ever permanent.
            bench_failed_pools(
                &mut want,
                &mut wire_fail_strikes,
                now,
                wire_retry * WIRE_FAIL_COOLDOWN_MULT,
            );
            // Hold off only a *repeat* of a set that already failed to wire — see
            // `last_wire_failure`. A changed `want` always attempts immediately.
            let backoff = last_wire_failure
                .as_ref()
                .is_some_and(|(failed, at)| *failed == want && at.elapsed() < wire_retry);
            if want != wired_dynamic && !backoff {
                // Group each wanted pool under its DEX's decoder script, then
                // decode per group. A pool with unknown/absent dex falls to the
                // pumpswap script (fails cleanly → REST). Partial failure keeps
                // the decoded pools on gRPC and leaves the rest on REST, retried
                // next tick (wired_dynamic only advances on a fully-clean decode).
                let mut by_script: std::collections::BTreeMap<&'static str, Vec<String>> =
                    std::collections::BTreeMap::new();
                for pool in &want {
                    let dex = pool_dex
                        .get(pool)
                        .map(|d| d.as_str())
                        .or_else(|| {
                            adopted_pools
                                .values()
                                .find(|r| r.pool == *pool)
                                .map(|r| r.dex.as_str())
                        });
                    let script = dex.map(dex_to_decode_script).unwrap_or(POOL_DECODE_SCRIPT);
                    by_script.entry(script).or_default().push(pool.clone());
                }
                let mut extra: Vec<crate::dex::types::PoolConfig> = Vec::new();
                let mut any_fail = false;
                for (script, ids) in &by_script {
                    match run_pool_decode(script, ids).await {
                        Ok(mut cfgs) => extra.append(&mut cfgs),
                        Err(e) => {
                            any_fail = true;
                            warn!("scan pool decode via {script} failed ({e:#}) — those discoveries stay REST");
                            // A group Err means nothing was salvageable, so every pool in it
                            // failed *this attempt* — the decoder reports per batch, not per
                            // pool. Strike each; at the limit bench it (the next pass drops
                            // it from `want` and the retry churn ends) until the cool-down
                            // expires and it gets a fresh cycle.
                            for id in ids {
                                let f = wire_fail_strikes.entry(id.clone()).or_default();
                                f.strikes += 1;
                                if f.strikes >= WIRE_FAIL_STRIKES && f.benched_at.is_none() {
                                    f.benched_at = Some(now);
                                    warn!(
                                        "pool {id} failed to decode {}× via {script} — benching it from dynamic wiring for {}s (stays REST; retried after that)",
                                        f.strikes,
                                        (wire_retry * WIRE_FAIL_COOLDOWN_MULT).as_secs()
                                    );
                                }
                            }
                        }
                    }
                }
                // Anything that DID decode clears its failure history outright.
                for c in &extra {
                    wire_fail_strikes.remove(&c.id);
                }
                if !extra.is_empty() || want.is_empty() {
                    let mut universe = effective_universe(
                        &watched, &discovered, &held_mints_from_state(&cfg),
                    );
                    // Held-from-state entries carry no pool/quote; give the adopted ones
                    // theirs, or spawn_grpc_feed silently leaves them REST-priced.
                    overlay_adopted_pools(&mut universe, &adopted_pools);
                    match bounded(
                        FEED_RESPAWN_TIMEOUT_SECS,
                        "gRPC feed re-spawn",
                        crate::portfolio::feed_setup::spawn_grpc_feed(&cfg, &universe, &extra),
                    )
                    .await
                    {
                        Ok(Some((new_feed, new_task))) => {
                            if let Some(old) = feed_task.take() {
                                old.abort();
                            }
                            spike_rx = new_feed
                                .spike_rx
                                .lock()
                                .ok()
                                .and_then(|mut g| g.take());
                            grpc_feed = Some(new_feed);
                            feed_task = Some(new_task);
                            // Only mark the full set wired if every group decoded;
                            // a partial failure keeps `want` so the failed pools
                            // retry on the next tick.
                            if any_fail {
                                last_wire_failure = Some((want, Instant::now()));
                            } else {
                                wired_dynamic = want;
                                last_wire_failure = None;
                            }
                            info!(
                                "gRPC feed re-spawned with {} dynamic pool(s){}",
                                extra.len(),
                                if any_fail { " (partial — some REST, will retry)" } else { "" }
                            );
                        }
                        Ok(None) => {
                            last_wire_failure = Some((want, Instant::now()));
                            warn!("gRPC feed re-spawn produced no feed — keeping previous");
                        }
                        Err(e) => {
                            last_wire_failure = Some((want, Instant::now()));
                            warn!("gRPC feed re-spawn failed ({e}) — keeping previous");
                        }
                    }
                } else {
                    warn!(
                        "scan pool decode: all {} discovered pool(s) failed — staying REST, retrying next tick",
                        want.len()
                    );
                    last_wire_failure = Some((want, Instant::now()));
                }
            }
        }

        timer.lap("wiring");

        // gRPC-preferred pricing (opt-in): take fresh on-chain prices from the gRPC feed,
        // REST-fetch only the mints it didn't cover (missing/stale/distrusted). Falls back
        // to REST for everything when the feed is absent (flag off) — today's behavior.
        let distrusted = grpc_feed.as_ref().map(|f| f.distrusted_snapshot()).unwrap_or_default();
        let (mut grpc_prices, mut rest_mints) = match &grpc_feed {
            Some(feed) => grpc_pricer::select_prices(
                &feed.map,
                &token_mints,
                Duration::from_secs(cfg.momentum_grpc_stale_secs),
                Instant::now(),
                &distrusted,
            ),
            None => (HashMap::new(), token_mints.clone()),
        };
        // Trust-until-changed mode (MOMENTUM_GRPC_STALE_SECS=0): periodically REST-fetch
        // gRPC-priced mints anyway and compare. Divergence beyond the bps budget distrusts
        // the mint back to REST until it re-agrees or a fresh on-chain write arrives —
        // covers a dead stream or a price that migrated venues.
        let mut xcheck_mints: Vec<String> = Vec::new();
        if cfg.momentum_grpc_stale_secs == 0 && cfg.momentum_grpc_xcheck_secs > 0 {
            if let Some(feed) = &grpc_feed {
                let every = Duration::from_secs(cfg.momentum_grpc_xcheck_secs);
                let now = Instant::now();
                for m in grpc_prices.keys() {
                    if feed.xcheck_due(m, every, now) { xcheck_mints.push(m.clone()); }
                }
                rest_mints.extend(xcheck_mints.iter().cloned());
                // Also re-enroll currently-distrusted mints that still carry a live gRPC
                // price: select_prices always routes a distrusted mint to REST, so it never
                // re-appears in grpc_prices.keys() above — without this, distrust could only
                // ever clear via a fresh on-chain write (note_update), never via a later
                // cross-check re-agreeing (the recovery path the spec promises). These mints
                // are already in rest_mints (routed there by select_prices as to_rest), so
                // only xcheck_mints needs the addition here.
                for m in &distrusted {
                    if feed.map.contains_key(m) && feed.xcheck_due(m, every, now) {
                        xcheck_mints.push(m.clone());
                    }
                }
            }
        }
        // Observability: which watched (curated) tokens are on-chain-priced vs REST
        // this tick. Only wired tokens (pool+quote in momentum_tokens.json) can be
        // gRPC-priced; a wired token in REST=[…] means its stream is stale/down.
        if grpc_feed.is_some() {
            let mut via_grpc: Vec<&str> = Vec::new();
            let mut via_rest: Vec<&str> = Vec::new();
            for w in &watched {
                if grpc_prices.contains_key(&w.mint) {
                    via_grpc.push(&w.symbol);
                } else if !w.pool_refs().is_empty() {
                    via_rest.push(&w.symbol);
                }
            }
            let sig = format!("gRPC=[{}] REST(wired)=[{}]", via_grpc.join(","), via_rest.join(","));
            if last_pricing_sig.as_deref() != Some(sig.as_str()) {
                info!("momentum: pricing {sig}");
                last_pricing_sig = Some(sig);
            }
        }
        // Fetch current prices; merge with last known prices so tokens that
        // hit a transient error still show their previous value rather than $0.
        // Deadline-bounded: the serial REST walk stops at the deadline and keeps what it
        // has (mints not reached carry forward, as any transient miss already does).
        let price_deadline = Instant::now() + Duration::from_secs(cfg.momentum_prices_timeout_secs);
        let rest_result: anyhow::Result<HashMap<String, f64>> = if let Some(cache) = &rest_cache {
            // Background cache (MOMENTUM_REST_BG): read fresh REST prices without awaiting
            // the network. Cross-check mints are gRPC-priced and the check exists to catch a
            // dead stream, so THEY are still read live (bounded by the same deadline) — a
            // cached sample would compare two different moments.
            cache.set_want(rest_prices::poll_set(&token_mints));
            let max_age = Duration::from_secs(cfg.momentum_rest_max_age_secs);
            let mut keys = rest_mints.clone();
            keys.push("SOL".to_string());
            keys.push(pricer::SOL_MINT.to_string());
            let mut p = cache.snapshot(&keys, max_age);
            if !xcheck_mints.is_empty() {
                p.extend(pricer::fetch_token_prices_until(&http, &xcheck_mints, price_deadline).await);
            }
            if p.is_empty() && !rest_mints.is_empty() {
                Err(anyhow::anyhow!(
                    "REST price cache has no fresh price for any of {} mint(s) (poller stalled or upstream down)",
                    rest_mints.len()
                ))
            } else {
                Ok(p)
            }
        } else {
            pricer::fetch_prices_until(&http, &rest_mints, price_deadline).await
        };
        let fresh = match rest_result {
            Ok(mut p) => {
                // Resolve any due cross-checks: compare the gRPC price this tick is about
                // to trust against the REST price just fetched for the same mint.
                if let Some(feed) = &grpc_feed {
                    let now = Instant::now();
                    for m in &xcheck_mints {
                        // A distrusted mint is absent from grpc_prices (select_prices
                        // routed it to REST); fall back to its last-known gRPC value.
                        let g_opt = grpc_prices.get(m).copied().or_else(|| feed.map.get(m).map(|e| e.value().0));
                        if let (Some(g), Some(&r)) = (g_opt, p.get(m)) {
                            if !(r.is_finite() && r > 0.0) {
                                // Degenerate REST read (zero, negative, NaN, or inf):
                                // skip without recording, so the mint stays (or becomes)
                                // due and is retried next tick instead of being
                                // trusted/distrusted off a garbage price.
                                continue;
                            }
                            let dev_bps = ((g - r).abs() / r * 10_000.0) as u32;
                            let ok = dev_bps <= cfg.momentum_grpc_xcheck_bps;
                            feed.record_xcheck(m, ok, now);
                            if !ok {
                                warn!("momentum: xcheck DIVERGED {m}: grpc=${g:.6} rest=${r:.6} ({dev_bps}bps > {}bps) — distrusting", cfg.momentum_grpc_xcheck_bps);
                                grpc_prices.remove(m); // REST value already in p wins this tick (no-op if m was already distrusted)
                            }
                            // ok=true — including a previously-distrusted mint re-agreeing —
                            // clears distrust via record_xcheck; the mint still rides REST
                            // this tick (already in p) and returns to gRPC next tick via select_prices.
                        }
                    }
                }
                p.extend(grpc_prices); // gRPC-fresh wins (disjoint from rest by construction)
                p
            }
            Err(e) => {
                warn!("portfolio: price fetch failed: {e}");
                if grpc_prices.is_empty() {
                    timer.lap("prices");
                    emit_tick_timing(&cfg, tick_gap, &timer);
                    continue;
                }
                grpc_prices // still use on-chain prices this tick even if REST failed
            }
        };
        timer.lap("prices");
        let fetch_time = Instant::now();
        for key in fresh.keys() {
            last_price_update.insert(key.clone(), fetch_time);
        }

        // Detect frozen watched-token prices: a watched mint absent from this tick's
        // fresh fetch is about to be carried forward. A short streak is a transient miss;
        // a long one means the feed is stuck and the momentum ranking is scoring a flat
        // price (the JitoSOL REST(wired) freeze). Warn on crossing the threshold, re-warn
        // periodically, and log recovery when a fresh price returns.
        for w in &watched {
            if fresh.contains_key(&w.mint) {
                if let Some(prev) = carry_forward_streak.remove(&w.mint) {
                    if prev >= CARRY_FORWARD_WARN_STREAK {
                        let price = fresh.get(&w.mint).copied().unwrap_or(0.0);
                        info!(
                            "momentum: {} price recovered after {} stale tick(s) → ${:.6}",
                            w.symbol, prev, price
                        );
                    }
                }
            } else if last_prices.contains_key(&w.mint) {
                // Only count as frozen once there IS a prior price to carry (a
                // never-yet-priced cold mint is a different, already-handled case).
                let streak = carry_forward_streak.entry(w.mint.clone()).or_insert(0);
                *streak += 1;
                let n = *streak;
                if n == CARRY_FORWARD_WARN_STREAK
                    || (n > CARRY_FORWARD_WARN_STREAK
                        && (n - CARRY_FORWARD_WARN_STREAK) % CARRY_FORWARD_REWARN_EVERY == 0)
                {
                    let frozen = last_prices.get(&w.mint).copied().unwrap_or(0.0);
                    warn!(
                        "momentum: {} price FROZEN — carried forward {} ticks at ${:.6} \
                         (no fresh {} fetch); ranking on stale data",
                        w.symbol,
                        n,
                        frozen,
                        if w.pool_refs().is_empty() { "REST" } else { "gRPC/REST" },
                    );
                }
            }
        }

        // Carry forward last known prices for any mint missing from this tick.
        let mut prices = last_prices.clone();
        prices.extend(fresh);
        last_prices = prices.clone();
        last_prices.retain(|k, _| known_price_keys.contains(k.as_str()));

        // Publish the latest SOL/USD so the gRPC ingestion task can convert SOL-quoted
        // pools to USD (no-op when the feed is absent).
        if let Some(feed) = &grpc_feed {
            if let Some(sol) = prices.get("SOL") {
                feed.publish_sol_usd(*sol);
            }
        }

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let snap = PriceSnapshot { ts, prices: prices.clone() };

        // Refresh EUR rate every 10 ticks (~10 minutes).
        ticks_since_eur_refresh += 1;
        if ticks_since_eur_refresh >= 10 {
            if let Ok(r) = bounded(EUR_RATE_TIMEOUT_SECS, "EUR rate", pricer::fetch_eur_rate(&http)).await {
                eur_rate = r;
            }
            ticks_since_eur_refresh = 0;
        }

        // Refresh SMA daily from local history — no API calls needed.
        ticks_since_sma_refresh += 1;
        if ticks_since_sma_refresh >= 1440 {
            monthly_sma = pricer::compute_sma_from_history(&history, &portfolio);
            info!("portfolio: SMA refreshed from local history for {} assets", monthly_sma.len() / 2);
            ticks_since_sma_refresh = 0;
        }

        // Log asset values with staleness warnings.
        log_values(&portfolio, &prices, eur_rate, &last_price_update);

        // Persist to disk.
        if let Err(e) = history::append_snapshot(&history_path, &snap) {
            warn!("portfolio: failed to append snapshot: {e}");
        }

        // Update in-memory deque.
        if history.len() == history::MAX_HISTORY {
            history.pop_front();
        }
        history.push_back(snap);

        // Rewrite history file every 12 hours so the on-disk file stays
        // close to MAX_HISTORY entries between daily rewrites.
        ticks_since_history_rewrite += 1;
        if ticks_since_history_rewrite >= 720 {
            if let Err(e) = history::rewrite_history(&history_path, &history) {
                warn!("portfolio: history trim failed: {e}");
            }
            ticks_since_history_rewrite = 0;
        }

        timer.lap("history");

        // Compute risk metrics, log them, and write a JSON sidecar for external tooling.
        let risk_report = analyzer::compute_risk(&history, &portfolio, eur_rate, &analysis_cfg);
        log_risk_report(&risk_report, analysis_cfg.zscore_min_obs);
        if let Ok(json) = serde_json::to_string_pretty(&risk_report) {
            if let Err(e) = std::fs::write(&cfg.status_path, json) {
                warn!("portfolio: failed to write status sidecar: {e}");
            }
        }

        timer.lap("risk");

        // Momentum slow-tick: eviction then entries. Runs every monitor tick,
        // before the alert path, so it isn't skipped on ticks without alerts.
        // Per-tick order: adopt → exits (fast arm) → eviction → entries.
        if cfg.enable_momentum_trader {
            // Surface the loss-breaker state so a halted trader is never silently
            // inert. When halted, maybe_evict/maybe_enter/maybe_exit all early-return
            // with no log, and the per-token rank/metric panel (log_rank_line, inside
            // maybe_enter after the halt gate) never prints — so the only signal the
            // operator gets is the trader going quiet. Log the halt once on transition
            // (incl. startup, since the halt file is sticky across restarts) and once
            // when it clears.
            match momentum_state::read_halt(Path::new(&cfg.momentum_halt_path)) {
                Ok(Some(h)) => {
                    if last_halt_reason.as_deref() != Some(h.reason.as_str()) {
                        warn!(
                            "momentum: HALTED by loss breaker — {} (no entries/rotations/metrics; delete {} to re-arm)",
                            h.reason, cfg.momentum_halt_path
                        );
                        last_halt_reason = Some(h.reason);
                    }
                }
                Ok(None) => {
                    if last_halt_reason.take().is_some() {
                        info!("momentum: loss-breaker halt cleared — trading re-armed");
                    }
                }
                Err(e) => warn!("momentum: failed to read halt file: {e}"),
            }

            // Step 0: adopt a manually-acquired wallet holding mid-run, so the operator
            // is never forced to restart. Symmetric to invalidate_unbacked_position (which
            // reacts when a watched token LEAVES the wallet): when one ARRIVES — e.g. bought
            // via a mobile app — adopt it into a free slot so the trailing stop / fade exit
            // start managing it from here. Runs every slow tick (not just startup): the
            // wallet is re-scanned every 5 ticks so a fresh purchase becomes visible within
            // ~5 min, and this also recovers a startup adoption that was skipped (e.g. no
            // live price then). State is disk-backed and reloaded by every momentum call,
            // so the write is visible to the eviction/entry steps below and the next exit
            // tick immediately. Fully gated inside the fn (flag MOMENTUM_ADOPT_WALLET_POSITION,
            // FLAT/free-slot, exactly one watched holding worth ≥ half the trade size,
            // non-paper) — a cheap no-op otherwise.
            // Decimals self-heal: fetch any priced-set mint still missing from the
            // startup-seeded map BEFORE the adoption/eviction/entry steps (and the
            // fast exit arm until the next tick) need it. token_mints already
            // carries every wallet/held/discovered mint by this point in the tick
            // (wallet re-scan + scan blocks run above), so a mint adopted this tick
            // is sized this tick. No-op when nothing is missing.
            let missing = missing_decimal_mints(&token_mints, &decimals);
            if !missing.is_empty() {
                let n = missing.len();
                match bounded(DECIMALS_TIMEOUT_SECS, "decimals refresh", scanner::fetch_decimals_for_mints(&cfg.rpc_url, missing)).await {
                    Ok(m) => {
                        info!("momentum: cached decimals for {} new mint(s)", m.len());
                        decimals.extend(m);
                    }
                    Err(e) => warn!(
                        "momentum: decimals refresh failed ({e}); {n} mint(s) still missing — retrying next tick"
                    ),
                }
            }

            timer.lap("decimals");

            // Reconcile FIRST, adopt after. Invalidation runs every slow tick (not only
            // when the re-scan reports a change, as it did before): nomination is a cheap
            // set-diff that is empty in the common case, so the RPC cost is zero unless a
            // live position is actually missing from the wallet — and a candidate kept on a
            // failed/ambiguous read is then retried a minute later instead of waiting for
            // the next unrelated holdings change. Ordering it ahead of adoption frees a
            // slot the same tick it is confirmed dead. Same-tick RE-adoption of the mint
            // just written off is impossible by construction, not by cooldown: dropping it
            // required balance ≤ 0 in `portfolio` — the very snapshot both adoption passes
            // read below — so neither pass has a holding to adopt. The bench
            // (`last_exit_ts_per_mint`, written by the drop itself) is the guard for LATER
            // ticks, once the wallet re-scan sees the balance again: BOTH passes now honor
            // it through the shared `within_adopt_bench` predicate.
            // With the background wallet poller, both passes trust the wallet snapshot only
            // while it is younger than MOMENTUM_WALLET_MAX_AGE_SECS; a stalled poller means
            // "no opinion", never a drop or an adoption off a stale picture.
            let wallet_fresh = match &wallet_rx {
                Some(rx) => rx.borrow().as_ref().is_some_and(|snap| {
                    wallet_snapshot_usable(
                        snap.taken,
                        Instant::now(),
                        Duration::from_secs(cfg.momentum_wallet_max_age_secs),
                    )
                }),
                None => true,
            };
            if wallet_fresh {
                if wallet_stale_logged {
                    info!("portfolio: wallet snapshot fresh again — reconcile/adoption resumed");
                    wallet_stale_logged = false;
                }
                momentum::invalidate_unbacked_position(&cfg, &portfolio, &prices, Some(&stop_armed)).await;
                momentum::adopt_wallet_position(&cfg, &portfolio, &prices, &watched).await;
                momentum::adopt_unwatched_holdings(&cfg, &portfolio, &prices, &watched, &http).await;
            } else if !wallet_stale_logged {
                warn!(
                    "portfolio: wallet snapshot older than {}s (background re-scan stalled?) — skipping reconcile/adoption until it refreshes",
                    cfg.momentum_wallet_max_age_secs
                );
                wallet_stale_logged = true;
            }
            timer.lap("reconcile_adopt");

            // Refresh the effective universe (curated ∪ discovered ∪ held_set) so
            // this tick's ranking — and the fast exit arm until the next tick — see
            // the full overlay. Uses ALL held mints so no open position is orphaned.
            // Unconditional (mirrors the Task-8 dynamic-wiring block's own
            // `effective_universe` call below): an adopted-UNWATCHED holding is held but
            // NOT in `watched`, so when scanning is off the old `held is already in watched
            // by construction` invariant no longer holds — skipping this left such a
            // position out of `ranked` forever, and `weakest_stalled`/`weakest_green` both
            // require the mint to appear in `ranked`, so stagnation eviction (and rotation)
            // could never touch it. `discovered` stays empty when scanning is off, so this
            // call is a no-op beyond the held-mint overlay in that case — behavior with
            // scanning on is unchanged.
            effective = effective_universe(&watched, &discovered, &held_mints_from_state(&cfg));

            // Refresh the gRPC feed's held-mint set from current positions each slow
            // tick, so the ingestion task (GrpcFeed::note_update) knows which on-chain
            // price updates should wake the event-driven exit arm. No-op when the feed
            // is absent (flag off / gRPC disabled) — inert w.r.t. today's behavior.
            if let Some(feed) = &grpc_feed {
                feed.set_held(held_mints_from_state(&cfg).into_iter().map(|w| w.mint));
            }

            // Step 1: eviction (weakest-green rotation when all slots are full).
            let evict_outcomes = {
                let mctx = MomentumContext {
                    cfg: &cfg, watched: &effective, prices_usd: &prices,
                    history: &history, decimals: &decimals, http: &http,
                    usdc_balance: usdc_balance(&portfolio),
                    grpc_feed: None, stop_armed: Some(&stop_armed), flow: flow_cache.as_ref(), rest_prices: rest_cache.as_ref(),
                };
                momentum::maybe_evict(&mctx).await
            };
            match evict_outcomes {
                Ok(os) => for o in os { if !o.dry_run() { apply_outcome(&mut portfolio, &o); last_fill_at = Some(Instant::now()); } },
                Err(e) => error!("momentum: eviction tick error: {e:#}"),
            }
            timer.lap("evict");

            // Step 2: entries (fill free slots; maybe_enter self-limits via capacity).
            let enter_outcomes = {
                let mctx = MomentumContext {
                    cfg: &cfg, watched: &effective, prices_usd: &prices,
                    history: &history, decimals: &decimals, http: &http,
                    usdc_balance: usdc_balance(&portfolio),
                    grpc_feed: None, stop_armed: Some(&stop_armed), flow: flow_cache.as_ref(), rest_prices: rest_cache.as_ref(),
                };
                momentum::maybe_enter(&mctx).await
            };
            match enter_outcomes {
                Ok(os) => for o in os { if !o.dry_run() { apply_outcome(&mut portfolio, &o); last_fill_at = Some(Instant::now()); } },
                Err(e) => error!("momentum: entry tick error: {e:#}"),
            }
            timer.lap("enter");
        }

        // Market-neutral pairs trader (paper by default).
        if let Some(pcfg) = &pairs_cfg {
            if pcfg.enable {
                if let Err(e) = crate::portfolio::pairs_trader::tick(pcfg, &cfg, &history, &prices).await {
                    tracing::warn!("pairs tick failed: {e}");
                }
            }
        }

        // Kamino liquidation detection bot (Phase A — paper; self-paces to its scan cadence).
        if let Some(lcfg) = &liq_cfg {
            if lcfg.enable {
                if let Err(e) = crate::portfolio::liquidation::tick(lcfg, &cfg, &prices, &http).await {
                    tracing::warn!("liquidation tick failed: {e}");
                }
            }
        }

        timer.lap("pairs_liq");

        // Generate alerts using pre-computed risk data.
        let alerts = analyzer::analyze(&history, &portfolio, &risk_report, &analysis_cfg);
        if alerts.is_empty() {
            emit_tick_timing(&cfg, tick_gap, &timer);
            continue;
        }

        // Always log all alert details to console regardless of email cooldown.
        for alert in &alerts {
            info!(
                "portfolio: ⚠  {} — {} (€{:.2})",
                alert.symbol,
                alert.kind,
                alert.current_price * eur_rate,
            );
        }

        // Per-asset cooldown: filter to alerts whose asset has cleared its own timer.
        let total_alerts = alerts.len();
        let eligible: Vec<Alert> = alerts.into_iter()
            .filter(|a| last_alert_per_asset.get(&a.symbol)
                .is_none_or(|t| t.elapsed() >= cooldown))
            .collect();

        if eligible.is_empty() {
            info!("portfolio: email suppressed — all {} alert(s) in per-asset cooldown", total_alerts);
            emit_tick_timing(&cfg, tick_gap, &timer);
            continue;
        }

        let suppressed = total_alerts - eligible.len();
        if suppressed > 0 {
            info!("portfolio: {} alert(s) suppressed by per-asset cooldown", suppressed);
        }

        // Generate swap suggestions and trading insights for eligible alerts only.
        let swaps = analyzer::generate_swap_suggestions(&eligible, &monthly_sma, &risk_report);
        let insights = suggestions::generate_all_suggestions(&history, &portfolio, &risk_report, &monthly_sma);

        // Build and send email.
        let (subject, body) = build_email(&portfolio, &prices, &eligible, &swaps, &insights, &risk_report, eur_rate, analysis_cfg.zscore_lambda);
        match emailer::send_alert(&cfg, &subject, &body).await {
            Ok(true) => {
                info!("portfolio: alert email sent ({} alert(s))", eligible.len());
                let now = Instant::now();
                for alert in &eligible {
                    last_alert_per_asset.insert(alert.symbol.clone(), now);
                }
            }
            Ok(false) => {}
            Err(e) => error!("portfolio: failed to send alert email: {e:#}"),
        }
        timer.lap("alerts");
        emit_tick_timing(&cfg, tick_gap, &timer);
    }

    // ── Graceful shutdown (reached when the shutdown branch breaks the loop) ──
    // Halt the pairs trader on LIVE exit only (fail-closed: a restart won't auto-resume real
    // opening until the operator deletes the halt file). Paper auto-resumes — paper losses
    // aren't real, so requiring a manual re-arm each restart would just be friction. The
    // sidecar is stopped regardless of mode (below).
    if let Some(pcfg) = pairs_cfg.as_ref().filter(|c| c.enable && !c.dry_run) {
        let now = chrono::Utc::now().timestamp();
        match crate::portfolio::momentum_state::write_halt(
            Path::new(&pcfg.halt_path),
            &crate::portfolio::momentum_state::HaltRecord {
                ts: now,
                reason: "portfolio-watcher exit — delete this file to re-arm pairs opens".into(),
            },
        ) {
            Ok(()) => info!("pairs trader halted on exit — delete {} to re-arm", pcfg.halt_path),
            Err(e) => warn!("pairs: failed to write halt file on exit: {e}"),
        }
    }
    if let Some(mut child) = klend_sidecar.take() {
        let _ = child.kill().await;
        info!("klend-builder sidecar stopped");
    }
}

/// Shared outcome-application for the two momentum EXIT arms (fast-ticker backstop
/// and gRPC-notify event-driven), so their handling of `maybe_exit`'s result can
/// never diverge. `label` only tags the error log so the two call sites stay
/// distinguishable in `journalctl`/logs.
/// Returns `true` when at least one LIVE fill was applied to the in-memory portfolio.
fn apply_exit_outcomes(
    portfolio: &mut Portfolio,
    outcomes: anyhow::Result<Vec<TradeOutcome>>,
    label: &str,
) -> bool {
    let mut applied = false;
    match outcomes {
        Ok(os) => {
            for o in os {
                if !o.dry_run() {
                    apply_outcome(portfolio, &o);
                    applied = true;
                }
            }
        }
        Err(e) => error!("momentum: {label} error: {e:#}"),
    }
    applied
}

/// Apply a LIVE momentum fill to the in-memory portfolio so value logging stays
/// truthful within the running process (startup `scan_and_save` re-syncs from
/// chain anyway). Dry-run fills never reach here.
fn apply_outcome(portfolio: &mut Portfolio, outcome: &TradeOutcome) {
    match outcome {
        TradeOutcome::Entered { mint, symbol, token_amount, usdc_spent, .. } => {
            if let Some(u) = portfolio.tokens.iter_mut().find(|t| t.mint == momentum_universe::USDC_MINT) {
                u.amount = (u.amount - usdc_spent).max(0.0);
            }
            match portfolio.tokens.iter_mut().find(|t| &t.mint == mint) {
                Some(t) => t.amount += token_amount,
                None => portfolio.tokens.push(TokenEntry {
                    mint: mint.clone(),
                    symbol: symbol.clone(),
                    amount: *token_amount,
                }),
            }
        }
        TradeOutcome::Exited { mint, usdc_out, .. } => {
            if let Some(t) = portfolio.tokens.iter_mut().find(|t| &t.mint == mint) {
                t.amount = 0.0;
            }
            match portfolio.tokens.iter_mut().find(|t| t.mint == momentum_universe::USDC_MINT) {
                Some(u) => u.amount += usdc_out,
                None => portfolio.tokens.push(TokenEntry {
                    mint: momentum_universe::USDC_MINT.to_string(),
                    symbol: "USDC".to_string(),
                    amount: *usdc_out,
                }),
            }
        }
        TradeOutcome::Rotated { from_mint, to_mint, to_symbol, to_amount, .. } => {
            // Direct A→B swap: zero the old holding, add the new one. No USDC leg.
            if let Some(t) = portfolio.tokens.iter_mut().find(|t| &t.mint == from_mint) {
                t.amount = 0.0;
            }
            match portfolio.tokens.iter_mut().find(|t| &t.mint == to_mint) {
                Some(t) => t.amount += to_amount,
                None => portfolio.tokens.push(TokenEntry {
                    mint: to_mint.clone(),
                    symbol: to_symbol.clone(),
                    amount: *to_amount,
                }),
            }
        }
    }
}

/// Current USDC holdings from the in-memory portfolio (the trader's cash leg).
fn usdc_balance(portfolio: &Portfolio) -> f64 {
    portfolio
        .tokens
        .iter()
        .find(|t| t.mint == momentum_universe::USDC_MINT)
        .map(|t| t.amount)
        .unwrap_or(0.0)
}

/// True if token holdings differ between two wallet scans — a token appeared,
/// disappeared, or any balance moved beyond a tiny epsilon. Drives re-scan
/// reconciliation. SOL is ignored on purpose: a pure gas-spend change can't unback a
/// token position and shouldn't trip the "holdings changed" path.
fn holdings_changed(old: &Portfolio, new: &Portfolio) -> bool {
    if old.tokens.len() != new.tokens.len() {
        return true;
    }
    let new_map: HashMap<&str, f64> =
        new.tokens.iter().map(|t| (t.mint.as_str(), t.amount)).collect();
    old.tokens.iter().any(|t| match new_map.get(t.mint.as_str()) {
        Some(&amt) => (amt - t.amount).abs() > (t.amount.abs() * 1e-6).max(1e-9),
        None => true,
    })
}

fn build_known_price_keys(token_mints: &[String]) -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::from([
        "SOL".to_string(),
        "So11111111111111111111111111111111111111112".to_string(),
    ]);
    s.extend(token_mints.iter().cloned());
    s
}

/// One wireable venue from scan_tokens.js `pools` enrichment (best-first order).
#[derive(Debug, serde::Deserialize)]
struct ScanPool {
    pool: String,
    quote: String,
    /// DexScreener dexId (pumpswap/raydium/orca/meteora) — selects the decoder fetcher.
    #[serde(default)]
    dex: Option<String>,
}

/// One row of `scan_tokens.js --json`. Extra fields (vol24, liq) are ignored —
/// the script already volume-sorted, so the watcher only needs identity.
#[derive(Debug, serde::Deserialize)]
struct ScanCandidate {
    symbol: String,
    mint: String,
    #[serde(default)]
    name: Option<String>,
    /// Top gRPC-priceable venues (best first) from scan_tokens.js pool enrichment — present
    /// only when the token has a dynamically-wireable venue. The watcher decodes each via its
    /// `dex`'s fetcher and wires those that decode; the per-pool `dex` rides `pool_dex`.
    #[serde(default)]
    pools: Option<Vec<ScanPool>>,
    /// Legacy single-venue shorthand (pre-top-N scanner output) — still honoured when `pools`
    /// is absent, so an older scan_tokens still wires its one pool.
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    quote: Option<String>,
    #[serde(default)]
    dex: Option<String>,
}

/// Pure mapping half of `run_token_scan` (unit-tested): top-`top_n` scan rows →
/// watch entries, carrying the wireable pool/quote when the scanner emitted one.
fn candidates_to_watched(cands: Vec<ScanCandidate>, top_n: usize) -> Vec<WatchedToken> {
    cands
        .into_iter()
        .take(top_n)
        .map(|c| {
            // Prefer the top-N `pools` list; fall back to the legacy single shorthand. When
            // `pools` is present it wins outright (WatchedToken::pool_refs), so null the
            // shorthand to avoid the load()-time "both set" warning.
            let pools = c
                .pools
                .map(|ps| ps.into_iter().map(|p| PoolRef { pool: p.pool, quote: p.quote }).collect::<Vec<_>>());
            let (pool, quote) = if pools.is_some() { (None, None) } else { (c.pool, c.quote) };
            WatchedToken {
                symbol: c.symbol,
                mint: c.mint,
                name: c.name,
                equity: None,
                params: None,
                pool,
                quote,
                pools,
            }
        })
        .collect()
}

/// Spawn `node <script> --json`, parse stdout, and return the top-`top_n` rows as
/// watch entries. Best-effort: the caller logs any Err and keeps the prior set.
async fn run_token_scan(
    script: &str,
    top_n: usize,
) -> anyhow::Result<(Vec<WatchedToken>, HashMap<String, String>)> {
    let out = tokio::process::Command::new("node")
        .arg(script)
        .arg("--json")
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `node {script} --json`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "scan exited {}: {}",
            out.status.code().map_or_else(|| "signal".to_string(), |c| c.to_string()),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // The scanner writes its filter-funnel diagnostics (per-stage counts, per-token
    // drop reasons) to stderr; stdout is the JSON payload. Surface them on success
    // too — otherwise "discovered [X]" is unexplainable when the funnel ran dry.
    // Capped so a runaway script can't flood the log.
    for line in String::from_utf8_lossy(&out.stderr).lines().filter(|l| !l.trim().is_empty()).take(60) {
        info!("momentum {}", line.trim());
    }
    let cands: Vec<ScanCandidate> = serde_json::from_slice(&out.stdout)
        .context("scan stdout was not a JSON array of {symbol,mint,name,...}")?;
    // pool → DexScreener dexId for every wireable venue (same top-N slice candidates_to_watched
    // keeps), so the dynamic-wiring decode can dispatch each pool to the matching fetcher.
    // Built from the top-N `pools` list, falling back to the legacy single shorthand.
    let pool_dex: HashMap<String, String> = cands
        .iter()
        .take(top_n)
        .flat_map(|c| match &c.pools {
            Some(ps) => ps
                .iter()
                .filter_map(|p| p.dex.as_ref().map(|d| (p.pool.clone(), d.clone())))
                .collect::<Vec<_>>(),
            None => c
                .pool
                .as_ref()
                .zip(c.dex.as_ref())
                .map(|(p, d)| (p.clone(), d.clone()))
                .into_iter()
                .collect::<Vec<_>>(),
        })
        .collect();
    Ok((candidates_to_watched(cands, top_n), pool_dex))
}

/// The existing PumpSwap decoder script (ad-hoc `--pools` mode). Relative to the
/// bot's working directory, like `MOMENTUM_SCAN_SCRIPT`'s default.
const POOL_DECODE_SCRIPT: &str = "scripts/fetch_pumpswap_pools.js";

/// Map a DexScreener dexId to the fetcher that decodes that venue in `--pools` mode. Unknown
/// venues fall back to the pumpswap decoder, whose vault↔mint cross-check fails cleanly →
/// REST (safe by construction). The raydium/meteora ids are coarse (AMM-vs-CLMM, DLMM-vs-DAMM)
/// but the matching fetcher auto-detects; a genuine mismatch just drops that token to REST.
fn dex_to_decode_script(dex: &str) -> &'static str {
    match dex {
        "raydium" => "scripts/fetch_raydium_pools.js",
        "orca" => "scripts/fetch_orca_pools.js",
        "meteora" => "scripts/fetch_meteora_dlmm.js",
        _ => POOL_DECODE_SCRIPT, // pumpswap + anything unknown
    }
}

/// Pure parse half of `run_pool_decode` (unit-tested): the decoder writes a JSON
/// array in the PoolConfig schema.
fn parse_pool_configs(raw: &str) -> anyhow::Result<Vec<crate::dex::types::PoolConfig>> {
    serde_json::from_str(raw).context("pool decoder output was not a PoolConfig array")
}

/// Decode PumpSwap pool accounts for dynamically discovered tokens by spawning the
/// existing JS decoder (on-chain layout decode + mandatory vault↔mint cross-check).
/// Any failure is an Err — the caller keeps the previous feed (REST fallback), and
/// the next scan tick retries naturally.
async fn run_pool_decode(
    script: &str,
    pools: &[String],
) -> anyhow::Result<Vec<crate::dex::types::PoolConfig>> {
    // Fresh private per-call dir: create_dir fails if the path already exists, so a
    // pre-placed symlink can't be followed and concurrent calls can't collide on the
    // output path (security review 2026-07-22).
    let dir = std::env::temp_dir().join(format!(
        "scan_pools_{}_{:08x}",
        std::process::id(),
        rand::random::<u32>()
    ));
    std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join("pools.json");
    let out = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new("node")
            .arg(script)
            .arg("--pools")
            .arg(pools.join(","))
            .arg("--output")
            .arg(&tmp)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|e| {
        let _ = std::fs::remove_dir_all(&dir);
        e
    })
    .context("pool decode timed out after 30s")?
    .map_err(|e| {
        let _ = std::fs::remove_dir_all(&dir);
        e
    })
    .with_context(|| format!("failed to spawn `node {script} --pools …`"))?;
    if !out.status.success() {
        // The decoder (scripts/fetch_pumpswap_pools.js) sets a non-zero exit code on a
        // per-pool cross-check failure but still writes every successfully-decoded pool
        // to `--output`. Salvage those rather than discarding the whole batch — one bad
        // pool must fall back to REST, not the entire scan tick (spec: "one pool fails
        // cross-check → that token REST, rest proceed").
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let code = out.status.code().map_or_else(|| "signal".to_string(), |c| c.to_string());
        if let Ok(raw) = std::fs::read_to_string(&tmp) {
            if let Ok(configs) = parse_pool_configs(&raw) {
                if !configs.is_empty() {
                    warn!(
                        "pool decode exited {} but salvaged {} pool(s) from partial output: {}",
                        code,
                        configs.len(),
                        stderr
                    );
                    let _ = std::fs::remove_dir_all(&dir);
                    return Ok(configs);
                }
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        anyhow::bail!("pool decode exited {}: {}", code, stderr);
    }
    let raw = std::fs::read_to_string(&tmp)
        .map_err(|e| {
            let _ = std::fs::remove_dir_all(&dir);
            e
        })
        .with_context(|| format!("reading {}", tmp.display()))?;
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir(&dir);
    parse_pool_configs(&raw)
}

/// Effective momentum universe = curated ∪ discovered ∪ held_set, deduped by mint
/// (curated wins, then discovered, then held tokens in slot order). The held clause
/// keeps every open position rankable after it rolls off the discovered top-N.
fn effective_universe(
    curated: &[WatchedToken],
    discovered: &[WatchedToken],
    held: &[WatchedToken],
) -> Vec<WatchedToken> {
    let mut out: Vec<WatchedToken> = Vec::with_capacity(curated.len() + discovered.len() + held.len());
    let mut seen: HashSet<&str> = HashSet::new();
    for w in curated.iter().chain(discovered.iter()).chain(held.iter()) {
        if seen.insert(w.mint.as_str()) {
            out.push(w.clone());
        }
    }
    out
}

/// The momentum trader's currently-held tokens (if any), read from its state file,
/// as watch entries — so the rolling overlay never orphans open positions.
/// `name`/`equity`/`params` are unknown here (`None`); the exit path doesn't need them.
fn held_mints_from_state(cfg: &PortfolioConfig) -> Vec<WatchedToken> {
    super::momentum_state::load(Path::new(&cfg.momentum_state_path))
        .ok()
        .map(|s| {
            s.positions
                .into_iter()
                .map(|p| WatchedToken { symbol: p.symbol, mint: p.mint, name: None, equity: None, params: None, pool: None, quote: None, pools: None })
                .collect()
        })
        .unwrap_or_default()
}

/// True if two discovered sets differ as mint sets (order-independent) — gates the
/// warm/log work so an unchanged hourly scan is a no-op.
fn discovered_changed(old: &[WatchedToken], new: &[WatchedToken]) -> bool {
    if old.len() != new.len() {
        return true;
    }
    let olds: HashSet<&str> = old.iter().map(|w| w.mint.as_str()).collect();
    new.iter().any(|w| !olds.contains(w.mint.as_str()))
}

/// Is a previous failure recorded at `last` still inside its hold-off window? `None`
/// (never failed) is never held off, so a first attempt is always immediate. Pure so the
/// retry policy shared by the venue lookup and the dynamic-wire retry is unit-tested.
/// Minimum spacing between two tick-gap alert emails.
const GAP_ALERT_COOLDOWN: Duration = Duration::from_secs(1800);
/// Caps on slow-tick awaits that have no knob of their own (all on the exit loop).
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(90);
const VENUE_RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);
const FEED_RESPAWN_TIMEOUT_SECS: u64 = 30;
const EUR_RATE_TIMEOUT_SECS: u64 = 10;
const DECIMALS_TIMEOUT_SECS: u64 = 15;

/// Run `fut` under a hard cap, folding a timeout into the future's own error type so
/// call sites keep their existing `Err` arms (log + keep previous state).
async fn bounded<T>(
    secs: u64,
    what: &str,
    fut: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!("{what} timed out after {secs}s")),
    }
}

/// Persist one monitor tick's phase timings (`TickTiming`) and warn, naming the slowest
/// phases, when the tick blew its budget. Called on EVERY exit path of the tick body so
/// the record is never lost to an early `continue`.
fn emit_tick_timing(cfg: &PortfolioConfig, gap_secs: u64, timer: &TickTimer) {
    let (total_ms, steps) = timer.finish();
    if tick_timing::over_budget(total_ms, cfg.momentum_tick_warn_ms) {
        warn!(
            "portfolio: monitor tick took {total_ms}ms (budget {}ms) — slowest: {}",
            cfg.momentum_tick_warn_ms,
            tick_timing::top_steps(&steps, 3)
        );
    }
    if cfg.enable_momentum_trader {
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
        momentum::audit(cfg, ts, ActionKind::TickTiming { gap_secs, total_ms, steps });
    }
}

fn within_holdoff(last: Option<Instant>, now: Instant, window: Duration) -> bool {
    last.is_some_and(|t| now.duration_since(t) < window)
}

/// Remove pools that are currently benched (see `WIRE_FAIL_STRIKES`) from a wanted set, and
/// un-bench any whose cool-down has expired. Without the benching a permanently-undecodable
/// pool holds the `want != wired_dynamic` change-gate open forever, re-spawning the gRPC
/// feed (and resetting every token's price to REST during warm-up) on every retry; without
/// the expiry a pool benched by a transient failure — or by another pool's fault, since
/// `run_pool_decode` fails per script batch — would be stuck REST-priced for the whole life
/// of the position holding it. Expiry clears the strike count too, so the pool returns to a
/// completely fresh attempt cycle. Pure (no clock of its own) so both halves are testable.
fn bench_failed_pools(
    want: &mut HashSet<String>,
    fails: &mut HashMap<String, WireFail>,
    now: Instant,
    cooldown: Duration,
) {
    // Cool-down served → clean slate (equivalent to never having failed).
    fails.retain(|_, f| f.benched_at.is_none_or(|t| now.duration_since(t) < cooldown));
    // Still benched → not attempted this pass.
    want.retain(|p| fails.get(p).is_none_or(|f| f.benched_at.is_none()));
}

/// Overlay resolved venues for adopted-unwatched holdings onto the effective universe.
///
/// `spawn_grpc_feed` wires a token ONLY through `WatchedToken::pool_refs()`, and a held
/// token materialised by `held_mints_from_state` carries `pool: None` — so an adopted
/// unwatched position would never be gRPC-priced no matter what PoolConfigs are passed as
/// `extra_pools`. This fills in the `pool`/`quote` shorthand for exactly those entries.
///
/// Entries that already resolve a venue (curated `pool`+`quote`, or a `pools` list) are
/// left untouched: curated wiring is authoritative, mirroring `merge_pool_configs`.
fn overlay_adopted_pools(
    universe: &mut [WatchedToken],
    adopted: &HashMap<String, crate::portfolio::pricer::ResolvedPool>,
) {
    for w in universe.iter_mut() {
        if !w.pool_refs().is_empty() {
            continue;
        }
        if let Some(r) = adopted.get(&w.mint) {
            w.pool = Some(r.pool.clone());
            w.quote = Some(r.quote.clone());
        }
    }
}

/// Mints in the priced set that have no cached decimals yet — the per-tick
/// self-heal's work list (pure; unit-tested). A mint acquired mid-run (adopted
/// unwatched holding, entered discovery) is absent from the startup-seeded map,
/// and without decimals every exit/rotation sizing for it fails.
fn missing_decimal_mints(
    token_mints: &[String],
    decimals: &HashMap<String, u8>,
) -> Vec<String> {
    token_mints.iter().filter(|m| !decimals.contains_key(*m)).cloned().collect()
}

/// Pool ids of discoveries that carry a dynamically wireable pool — the change
/// signal for the feed re-spawn (set-compared against what is currently wired).
fn dynamic_pool_set(discovered: &[WatchedToken]) -> HashSet<String> {
    // All wireable venues across every discovery (a token can carry several via `pools`).
    discovered.iter().flat_map(|w| w.pool_refs()).map(|r| r.pool).collect()
}

fn log_values(
    portfolio: &Portfolio,
    prices: &HashMap<String, f64>,
    eur: f64,
    last_updated: &HashMap<String, Instant>,
) {
    let sol_usd = prices.get("SOL").copied().unwrap_or(0.0);
    let sol_eur = sol_usd * eur;
    let sol_value = sol_eur * portfolio.sol_amount;

    if last_updated.get("SOL").is_some_and(|t| t.elapsed() > PRICE_STALE_THRESHOLD) {
        warn!("portfolio: SOL price is stale (>{:.0}s old)", PRICE_STALE_THRESHOLD.as_secs_f64());
    }
    info!(
        "portfolio: SOL {:.4} × €{:.2} = €{:.2}",
        portfolio.sol_amount, sol_eur, sol_value
    );

    let mut total = sol_value;
    for token in &portfolio.tokens {
        let key = if prices.contains_key(&token.mint) { &token.mint } else { &token.symbol };
        let price_eur = prices.get(key).copied().unwrap_or(0.0) * eur;
        let value = price_eur * token.amount;
        total += value;

        if last_updated.get(key).is_some_and(|t| t.elapsed() > PRICE_STALE_THRESHOLD) {
            warn!("portfolio: {} price is stale (>{:.0}s old)", token.symbol, PRICE_STALE_THRESHOLD.as_secs_f64());
        }
        info!(
            "portfolio: {} {:.4} × €{:.4} = €{:.2}",
            token.symbol, token.amount, price_eur, value
        );
    }
    eprintln!("\x1b[31mportfolio: total value = €{:.2}\x1b[0m", total);
}

fn log_risk_report(report: &RiskReport, min_obs: usize) {
    info!("portfolio: ----- Risk Report -----");
    for a in &report.assets {
        if a.is_warm {
            let z_str = a.z_score.map_or("--".to_string(), |z| format!("{:+.2}", z));
            let vol_str = a.sigma_ann.map_or("--".to_string(), |v| format!("{:.1}%", v));
            info!(
                "portfolio:   {:<8} z={:<6} sigma_ann={:<8} dd={:.1}%  (EUR -{:.2})",
                a.symbol, z_str, vol_str, a.current_drawdown_pct.abs(), a.drawdown_eur
            );
        } else {
            info!(
                "portfolio:   {:<8} (warming {}/{})",
                a.symbol, a.n_obs, min_obs
            );
        }
    }
    info!(
        "portfolio:   Portfolio drawdown from combined peak: EUR -{:.2} ({:.1}%)",
        report.portfolio_drawdown_eur, report.portfolio_drawdown_pct.abs()
    );
    info!(
        "portfolio:   Sum of per-asset drawdowns:            EUR -{:.2}",
        report.total_drawdown_eur
    );
}

fn build_email(
    portfolio: &Portfolio,
    prices: &HashMap<String, f64>,
    alerts: &[Alert],
    swaps: &[SwapSuggestion],
    insights: &[Suggestion],
    risk: &RiskReport,
    eur: f64,
    lambda: f64,
) -> (String, String) {
    let subject = format!("[Portfolio Alert] {} signal(s) detected", alerts.len());

    let mut body = String::from("Portfolio Alerts\n");
    body.push_str(&"=".repeat(30));
    body.push('\n');

    for alert in alerts {
        body.push_str(&format!(
            "!  {} -- {} (price: EUR {:.4}, value: EUR {:.2})\n",
            alert.symbol,
            alert.kind,
            alert.current_price * eur,
            alert.current_value_usd * eur,
        ));
    }

    body.push('\n');
    body.push_str("Current Holdings\n");
    body.push_str(&"-".repeat(40));
    body.push('\n');

    let sol_eur = prices.get("SOL").copied().unwrap_or(0.0) * eur;
    body.push_str(&format!(
        "SOL   {:.4} x EUR {:.2} = EUR {:.2}\n",
        portfolio.sol_amount,
        sol_eur,
        sol_eur * portfolio.sol_amount
    ));
    let mut total = sol_eur * portfolio.sol_amount;

    for token in &portfolio.tokens {
        let key = if prices.contains_key(&token.mint) { &token.mint } else { &token.symbol };
        let price_eur = prices.get(key).copied().unwrap_or(0.0) * eur;
        let value = price_eur * token.amount;
        total += value;
        body.push_str(&format!(
            "{:<8} {:.4} x EUR {:.4} = EUR {:.2}\n",
            token.symbol, token.amount, price_eur, value
        ));
    }
    body.push_str(&format!("\nTotal: EUR {:.2}\n", total));

    // Risk summary section
    body.push('\n');
    body.push_str(&format!("Risk Summary (EWMA lambda={lambda:.2})\n"));
    body.push_str(&"-".repeat(40));
    body.push('\n');
    for a in &risk.assets {
        if a.is_warm {
            let z_str = a.z_score.map_or("--".to_string(), |z| format!("{:+.2}", z));
            let vol_str = a.sigma_ann.map_or("--".to_string(), |v| format!("{:.1}%", v));
            body.push_str(&format!(
                "{:<8} z={:<6} sigma_ann={:<8} dd={:.1}%  (EUR -{:.2} from peak)\n",
                a.symbol, z_str, vol_str, a.current_drawdown_pct.abs(), a.drawdown_eur
            ));
        } else {
            body.push_str(&format!("{:<8} (warming up)\n", a.symbol));
        }
    }
    body.push_str(&format!(
        "Portfolio drawdown from combined peak: EUR -{:.2} ({:.1}%)\n",
        risk.portfolio_drawdown_eur, risk.portfolio_drawdown_pct.abs()
    ));

    // Swap suggestions section (omitted when empty)
    if !swaps.is_empty() {
        body.push('\n');
        body.push_str("Swap Suggestions\n");
        body.push_str(&"-".repeat(40));
        body.push('\n');
        for s in swaps {
            let sell_dev = (s.sell_price - s.sell_sma) / s.sell_sma * 100.0;
            let buy_dev  = (s.buy_price  - s.buy_sma)  / s.buy_sma  * 100.0;
            body.push_str(&format!("→ SWAP {} FOR {}\n", s.sell_symbol, s.buy_symbol));
            body.push_str(&format!(
                "  {}: 7-day HIGH  price=€{:.2}  30d avg=€{:.2}  ({:+.1}% above avg)\n",
                s.sell_symbol, s.sell_price, s.sell_sma, sell_dev
            ));
            body.push_str(&format!(
                "  {}:  7-day LOW  price=€{:.2}  30d avg=€{:.2}  ({:+.1}% below avg)\n",
                s.buy_symbol, s.buy_price, s.buy_sma, buy_dev
            ));
            body.push_str(&format!(
                "  Positions: {} €{:.0}  →  {} €{:.0}\n\n",
                s.sell_symbol, s.sell_value_eur, s.buy_symbol, s.buy_value_eur
            ));
        }
    }

    // Trading Insights section (omitted when empty)
    if !insights.is_empty() {
        body.push('\n');
        body.push_str("Trading Insights\n");
        body.push_str(&"-".repeat(40));
        body.push('\n');
        for insight in insights {
            body.push_str(&format!("[{}]\n", insight.signal_name));
            body.push_str(&format!("{}\n", insight.action));
            for line in &insight.rationale {
                body.push_str(&format!("  • {}\n", line));
            }
            body.push('\n');
        }
    }

    (subject, body)
}

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

/// Count how many in-memory snapshots carry a price for `mint`.
fn obs_count(history: &VecDeque<PriceSnapshot>, mint: &str) -> usize {
    history.iter().filter(|s| s.prices.contains_key(mint)).count()
}

/// Backfill held tokens (+ SOL) when overall history is too shallow.
async fn backfill_birdeye(
    http: &Client,
    api_key: &str,
    portfolio: &Portfolio,
    history: &mut VecDeque<PriceSnapshot>,
    history_path: &Path,
) {
    let mut items: Vec<(String, String)> = portfolio
        .tokens
        .iter()
        .map(|t| (t.mint.clone(), t.symbol.clone()))
        .collect();
    items.push((SOL_MINT.to_string(), "SOL".to_string()));
    backfill_pass(http, api_key, &items, history, history_path).await;
}

/// Warm up watched momentum mints that are short of the minimum
/// (≤ `SORTINO_MIN_OBS` observations). For each, fetch ~`lookback`(+margin) of
/// 1-min candles from Birdeye and merge them into the grid by timestamp
/// (`history::merge_backfill_grid`), so a just-added token gets a full Sortino
/// window even on a sparse grid — and the result is **persisted**, so it's a
/// one-time cost: on later restarts the token is already warm and skipped.
///
/// Fetches are SERIAL and paced on purpose: Birdeye's public tier returns 429 on
/// concurrent paginated pulls, and a failed fetch leaves the token cold — so
/// reliability beats raw speed. Fetching only the lookback window (not a full
/// 7 days) keeps it to a few requests per token. No-ops when nothing is cold.
/// Watched tokens with too few observations to rank, as `(mint, label)`.
fn cold_watched(watched: &[WatchedToken], history: &VecDeque<PriceSnapshot>) -> Vec<(String, String)> {
    watched
        .iter()
        .filter(|w| obs_count(history, &w.mint) <= SORTINO_MIN_OBS)
        .map(|w| (w.mint.clone(), w.name.clone().unwrap_or_else(|| w.symbol.clone())))
        .collect()
}

/// Network half of the cold warm-up: the lookback window (+4h margin, ≤ 7 days) of 1-min
/// candles per cold token, paced for Birdeye's rate limit. Touches no shared state.
async fn fetch_cold_candles(
    http: &Client,
    api_key: &str,
    cold: &[(String, String)],
    lookback_obs: usize,
) -> Vec<PriceSnapshot> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let window_min = (lookback_obs as u64).saturating_add(240).min(7 * 24 * 60);
    let from = now.saturating_sub(window_min * 60);
    let mut all_snaps: Vec<PriceSnapshot> = Vec::new();
    for (i, (mint, label)) in cold.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // pace for rate limits
        }
        match pricer::fetch_history_birdeye(http, api_key, mint, from, now).await {
            Ok(snaps) => {
                let n = snaps.iter().filter(|s| s.prices.contains_key(mint)).count();
                info!("momentum: fetched {n} candles for {label}");
                all_snaps.extend(snaps);
            }
            Err(e) => warn!("momentum: Birdeye warm-up failed for {label}: {e}"),
        }
    }
    all_snaps
}

/// Sync half of the warm-up: merge the fetched grid into `history` and persist. Always
/// runs on the monitor task — `history` is never touched from a background task.
fn apply_backfill(history: &mut VecDeque<PriceSnapshot>, snaps: Vec<PriceSnapshot>, history_path: &Path) {
    if snaps.is_empty() {
        return;
    }
    history::merge_backfill_grid(history, snaps);
    if let Err(e) = history::rewrite_history(history_path, history) {
        warn!("momentum: backfill persist failed: {e}");
    }
}

async fn backfill_watched_cold(
    http: &Client,
    api_key: &str,
    watched: &[WatchedToken],
    lookback_obs: usize,
    history: &mut VecDeque<PriceSnapshot>,
    history_path: &Path,
) {
    let cold = cold_watched(watched, history);
    if cold.is_empty() {
        return;
    }
    info!("momentum: warming up {} cold watched token(s) via Birdeye", cold.len());
    let snaps = fetch_cold_candles(http, api_key, &cold, lookback_obs).await;
    apply_backfill(history, snaps, history_path);
}

/// Results posted by the background discovery tasks (MOMENTUM_SCAN_BG).
enum ScanMsg {
    Discovered { found: Vec<WatchedToken>, found_dex: HashMap<String, String> },
    Backfill { snaps: Vec<PriceSnapshot> },
}

/// A wallet re-scan published by the background poller (MOMENTUM_WALLET_BG).
#[derive(Clone)]
struct WalletSnapshot {
    portfolio: Portfolio,
    taken: Instant,
    seq: u64,
}

/// `true` when the snapshot was taken before the last live fill mutated the in-memory
/// portfolio — applying it would roll that fill back until the next re-scan.
fn snapshot_predates_fill(taken: Instant, last_fill: Option<Instant>) -> bool {
    last_fill.is_some_and(|f| taken < f)
}

/// `true` while a wallet snapshot is young enough to base adoption/invalidation on.
fn wallet_snapshot_usable(taken: Instant, now: Instant, max_age: Duration) -> bool {
    now.saturating_duration_since(taken) <= max_age
}

/// A background scan that has run past twice its own interval is stuck; abort it.
fn scan_overdue(started: Instant, now: Instant, interval_secs: u64) -> bool {
    now.saturating_duration_since(started) > Duration::from_secs(interval_secs.saturating_mul(2))
}

/// Background wallet re-scan: `scan_and_save` every `every` (it keeps sole ownership of
/// portfolio.json), publishing each result with a sequence number and its timestamp.
fn spawn_wallet_poller(
    cfg: PortfolioConfig,
    http: Client,
    every: Duration,
) -> tokio::sync::watch::Receiver<Option<WalletSnapshot>> {
    let (tx, rx) = tokio::sync::watch::channel::<Option<WalletSnapshot>>(None);
    info!("portfolio: background wallet re-scan every {}s", every.as_secs());
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut seq: u64 = 0;
        loop {
            tick.tick().await;
            if tx.is_closed() {
                break;
            }
            match bounded(cfg.momentum_wallet_scan_timeout_secs, "wallet re-scan", scanner::scan_and_save(&cfg, &http)).await {
                Ok(portfolio) => {
                    seq += 1;
                    let _ = tx.send(Some(WalletSnapshot { portfolio, taken: Instant::now(), seq }));
                }
                Err(e) => warn!("portfolio: background wallet re-scan failed: {e}"),
            }
        }
    });
    rx
}

/// Shared per-mint backfill loop: fetch ~7 days of 1-min history for each
/// `(mint, label)`, merge (older-only, deduped), and persist incrementally.
async fn backfill_pass(
    http: &Client,
    api_key: &str,
    items: &[(String, String)],
    history: &mut VecDeque<PriceSnapshot>,
    history_path: &Path,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let from = now.saturating_sub(7 * 24 * 3600);

    for (i, (mint, label)) in items.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
        match pricer::fetch_history_birdeye(http, api_key, mint, from, now).await {
            Ok(mut snaps) => {
                if mint == SOL_MINT {
                    for snap in &mut snaps {
                        // Normalise SOL mint key → "SOL" symbol key so all snapshots
                        // use a consistent key regardless of source.
                        if let Some(price) = snap.prices.remove(SOL_MINT) {
                            snap.prices.insert("SOL".to_string(), price);
                        }
                    }
                }
                info!("portfolio: backfilled {} snapshots for {label}", snaps.len());
                history::merge_backfill(history, snaps);
                // Persist incrementally after each mint so a crash mid-backfill
                // doesn't lose data already fetched.
                if let Err(e) = history::rewrite_history(history_path, history) {
                    warn!("portfolio: backfill persist failed for {label}: {e}");
                }
            }
            Err(e) => warn!("portfolio: Birdeye backfill failed for {label}: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pf(sol: f64, toks: &[(&str, f64)]) -> Portfolio {
        Portfolio {
            sol_amount: sol,
            tokens: toks
                .iter()
                .map(|(m, a)| TokenEntry { mint: (*m).to_string(), symbol: (*m).to_string(), amount: *a })
                .collect(),
        }
    }

    #[test]
    fn missing_decimal_mints_finds_only_uncached() {
        let mints = vec!["KNOWN".to_string(), "NEW_A".to_string(), "NEW_B".to_string()];
        let mut decimals = HashMap::new();
        decimals.insert("KNOWN".to_string(), 6u8);
        assert_eq!(missing_decimal_mints(&mints, &decimals), vec!["NEW_A", "NEW_B"]);
        decimals.insert("NEW_A".to_string(), 9u8);
        decimals.insert("NEW_B".to_string(), 5u8);
        assert!(missing_decimal_mints(&mints, &decimals).is_empty()); // no-op when complete
    }

    #[test]
    fn holdings_changed_detects_relevant_moves() {
        let base = pf(1.0, &[("USDC", 1000.0), ("MET", 50.0)]);
        // Identical token holdings → unchanged (SOL ignored).
        assert!(!holdings_changed(&base, &pf(1.0, &[("USDC", 1000.0), ("MET", 50.0)])));
        // SOL-only change (gas) → NOT a holdings change.
        assert!(!holdings_changed(&base, &pf(0.97, &[("USDC", 1000.0), ("MET", 50.0)])));
        // A balance moved → changed (e.g. swapped USDC for more MET).
        assert!(holdings_changed(&base, &pf(1.0, &[("USDC", 900.0), ("MET", 50.0)])));
        // A token disappeared (sold the whole MET position) → changed.
        assert!(holdings_changed(&base, &pf(1.0, &[("USDC", 1000.0)])));
        // A token appeared → changed.
        assert!(holdings_changed(&base, &pf(1.0, &[("USDC", 1000.0), ("MET", 50.0), ("JTO", 5.0)])));
        // Sub-epsilon float noise on an unchanged balance → NOT changed.
        assert!(!holdings_changed(&base, &pf(1.0, &[("USDC", 1000.0 + 1e-7), ("MET", 50.0)])));
    }

    fn wt(sym: &str, mint: &str) -> WatchedToken {
        WatchedToken { symbol: sym.into(), mint: mint.into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None }
    }

    #[test]
    fn within_holdoff_only_delays_a_recorded_failure() {
        let t0 = Instant::now();
        // Never failed → attempt immediately (a new adopted mint is looked up on sight).
        assert!(!within_holdoff(None, t0, Duration::from_secs(3600)));
        // Just failed → held off.
        assert!(within_holdoff(Some(t0), t0, Duration::from_secs(3600)));
        // A zero window disables the hold-off entirely.
        assert!(!within_holdoff(Some(t0), t0, Duration::ZERO));
        // Older than the window → attempt again. (Guarded: on a platform where the
        // monotonic clock starts at boot, `t0 - 1h` may not exist in a fresh process.)
        if let Some(old) = t0.checked_sub(Duration::from_secs(3600)) {
            assert!(!within_holdoff(Some(old), t0, Duration::from_secs(600)));
        }
    }

    fn pools(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bench_failed_pools_benches_only_at_the_strike_limit() {
        let t0 = Instant::now();
        let cooldown = Duration::from_secs(3600);
        let mut want = pools(&["OK", "FLAKY", "DEAD"]);
        let mut fails: HashMap<String, WireFail> = [
            // Below the limit → never benched, so it still gets attempts.
            ("FLAKY".to_string(), WireFail { strikes: WIRE_FAIL_STRIKES - 1, benched_at: None }),
            ("DEAD".to_string(), WireFail { strikes: WIRE_FAIL_STRIKES, benched_at: Some(t0) }),
        ]
        .into_iter()
        .collect();
        bench_failed_pools(&mut want, &mut fails, t0, cooldown);
        assert!(want.contains("OK"), "a never-failed pool is kept");
        assert!(want.contains("FLAKY"), "below the limit → still retried");
        assert!(!want.contains("DEAD"), "benched → dropped so `want` can converge");

        // Inert path: no recorded failures → nothing is touched at all.
        let mut untouched = pools(&["A", "B"]);
        let mut empty: HashMap<String, WireFail> = HashMap::new();
        bench_failed_pools(&mut untouched, &mut empty, t0, cooldown);
        assert_eq!(untouched.len(), 2);
        assert!(empty.is_empty());
    }

    #[test]
    fn bench_failed_pools_reinstates_a_pool_after_its_cooldown() {
        let t0 = Instant::now();
        let cooldown = Duration::from_secs(3600);
        // A pool benched an hour "ago", expressed by shrinking the cool-down instead of
        // moving the clock, so the test never does Instant arithmetic.
        let mut fails: HashMap<String, WireFail> = [(
            "DEAD".to_string(),
            WireFail { strikes: WIRE_FAIL_STRIKES, benched_at: Some(t0) },
        )]
        .into_iter()
        .collect();

        // Still inside the cool-down → excluded, bookkeeping preserved.
        let mut want = pools(&["DEAD"]);
        bench_failed_pools(&mut want, &mut fails, t0, cooldown);
        assert!(want.is_empty(), "still cooling down");
        assert_eq!(fails["DEAD"].strikes, WIRE_FAIL_STRIKES, "strikes survive the bench");

        // Cool-down elapsed (window of zero ⇒ any age qualifies) → the pool comes back for a
        // FRESH cycle: no longer excluded, and its strike count is wiped so a single later
        // transient failure cannot immediately re-bench it. This is the property that makes
        // a batch-attributed strike-out recoverable rather than permanent.
        let mut want = pools(&["DEAD"]);
        bench_failed_pools(&mut want, &mut fails, t0, Duration::ZERO);
        assert!(want.contains("DEAD"), "cool-down served → re-attempted");
        assert!(!fails.contains_key("DEAD"), "clean slate — strikes cleared on expiry");
    }

    #[test]
    fn overlay_adopted_pools_fills_only_empty_refs() {
        let mut universe = vec![
            WatchedToken { symbol: "A".into(), mint: "MA".into(), name: None, equity: None,
                           params: None, pool: None, quote: None, pools: None },
            WatchedToken { symbol: "B".into(), mint: "MB".into(), name: None, equity: None,
                           params: None, pool: Some("EXISTING".into()), quote: Some("SOL".into()), pools: None },
        ];
        let mut adopted = std::collections::HashMap::new();
        adopted.insert("MA".to_string(), crate::portfolio::pricer::ResolvedPool {
            pool: "PA".into(), dex: "pumpswap".into(), quote: "SOL".into() });
        adopted.insert("MB".to_string(), crate::portfolio::pricer::ResolvedPool {
            pool: "PB".into(), dex: "pumpswap".into(), quote: "SOL".into() });
        overlay_adopted_pools(&mut universe, &adopted);
        assert_eq!(universe[0].pool.as_deref(), Some("PA")); // filled
        assert_eq!(universe[0].quote.as_deref(), Some("SOL"));
        assert_eq!(universe[1].pool.as_deref(), Some("EXISTING")); // curated ref untouched
    }

    #[test]
    fn overlay_adopted_pools_leaves_unmapped_and_multi_venue_entries_alone() {
        let mut universe = vec![
            wt("C", "MC"), // no adopted entry → stays REST (pool_refs empty)
            WatchedToken { symbol: "D".into(), mint: "MD".into(), name: None, equity: None,
                           params: None, pool: None, quote: None,
                           pools: Some(vec![PoolRef { pool: "MULTI".into(), quote: "USDC".into() }]) },
        ];
        let mut adopted = std::collections::HashMap::new();
        adopted.insert("MD".to_string(), crate::portfolio::pricer::ResolvedPool {
            pool: "PD".into(), dex: "raydium".into(), quote: "SOL".into() });
        overlay_adopted_pools(&mut universe, &adopted);
        assert!(universe[0].pool.is_none(), "no venue resolved → left REST-priced");
        assert!(universe[1].pool.is_none(), "a `pools` list already wires MD — shorthand untouched");
        assert_eq!(universe[1].pool_refs()[0].pool, "MULTI");
    }

    #[test]
    fn effective_universe_dedups_curated_first() {
        let curated = vec![wt("RAY", "mRAY"), wt("JUP", "mJUP")];
        let discovered = vec![wt("RAY2", "mRAY"), wt("BONK", "mBONK")]; // mRAY is a dup
        let eff = effective_universe(&curated, &discovered, &[]);
        let mints: Vec<&str> = eff.iter().map(|w| w.mint.as_str()).collect();
        assert_eq!(mints, vec!["mRAY", "mJUP", "mBONK"]);
        assert_eq!(eff[0].symbol, "RAY", "curated entry wins the dup");
    }

    #[test]
    fn effective_universe_retains_and_dedups_held() {
        let curated = vec![wt("RAY", "mRAY")];
        let discovered = vec![wt("BONK", "mBONK")];
        // Single held token absent from both → retained.
        let held = vec![wt("WIF", "mWIF")];
        let eff = effective_universe(&curated, &discovered, &held);
        assert_eq!(eff.len(), 3);
        assert!(eff.iter().any(|w| w.mint == "mWIF"));
        // Held token already present → not duplicated.
        let held2 = vec![wt("RAY", "mRAY")];
        let eff2 = effective_universe(&curated, &discovered, &held2);
        assert_eq!(eff2.len(), 2);
    }

    #[test]
    fn effective_universe_multi_held_all_retained() {
        let curated = vec![wt("RAY", "mRAY")];
        let discovered = vec![wt("BONK", "mBONK")];
        // Two held tokens: one new, one already in curated (dedup).
        let held = vec![wt("WIF", "mWIF"), wt("RAY", "mRAY")];
        let eff = effective_universe(&curated, &discovered, &held);
        assert_eq!(eff.len(), 3, "mWIF added; mRAY already in curated — no dup");
        assert!(eff.iter().any(|w| w.mint == "mWIF"), "new held token included");
        assert_eq!(eff.iter().filter(|w| w.mint == "mRAY").count(), 1, "no duplicate for mRAY");
    }

    #[test]
    fn effective_universe_empty_discovered_equals_curated() {
        let curated = vec![wt("RAY", "mRAY"), wt("JUP", "mJUP")];
        let eff = effective_universe(&curated, &[], &[]);
        assert_eq!(eff.len(), 2);
    }

    #[test]
    fn dynamic_pool_set_collects_only_pooled_discoveries() {
        let mut a = wt("AAA", "mAAA");
        a.pool = Some("pAAA".into());
        a.quote = Some("SOL".into()); // pool_refs() needs pool+quote (the scanner emits both)
        let b = wt("BBB", "mBBB"); // pool-less — REST
        let set = dynamic_pool_set(&[a, b]);
        assert_eq!(set.len(), 1);
        assert!(set.contains("pAAA"));
        assert!(dynamic_pool_set(&[]).is_empty());
    }

    #[test]
    fn discovered_changed_is_mint_set_aware() {
        let a = vec![wt("RAY", "mRAY"), wt("BONK", "mBONK")];
        let b = vec![wt("BONK", "mBONK"), wt("RAY", "mRAY")]; // same set, reordered
        assert!(!discovered_changed(&a, &b));
        let c = vec![wt("RAY", "mRAY"), wt("WIF", "mWIF")];
        assert!(discovered_changed(&a, &c));
        assert!(discovered_changed(&a, &a[..1]), "different length");
    }

    #[test]
    fn scan_candidate_parses_and_take_n_maps_to_watched() {
        let json = r#"[
            {"symbol":"AAA","mint":"mAAA","name":"Alpha","vol24":9.0,"liq":1.0},
            {"symbol":"BBB","mint":"mBBB","vol24":8.0,"liq":1.0},
            {"symbol":"CCC","mint":"mCCC","vol24":7.0,"liq":1.0}
        ]"#;
        let cands: Vec<ScanCandidate> = serde_json::from_str(json).unwrap();
        let top: Vec<WatchedToken> = cands.into_iter().take(2)
            .map(|c| WatchedToken { symbol: c.symbol, mint: c.mint, name: c.name, equity: None, params: None, pool: None, quote: None, pools: None })
            .collect();
        assert_eq!(top.len(), 2);
        assert_eq!((top[0].symbol.as_str(), top[0].name.as_deref()), ("AAA", Some("Alpha")));
        assert_eq!(top[1].mint, "mBBB");
        assert!(top[1].name.is_none(), "missing name → None, extra fields ignored");
    }

    #[test]
    fn scan_candidate_carries_pool_and_quote_into_watched() {
        let json = r#"[
            {"symbol":"AAA","mint":"mAAA","name":"Alpha","pool":"pAAA","quote":"SOL","vol24":9.0},
            {"symbol":"BBB","mint":"mBBB"}
        ]"#;
        let cands: Vec<ScanCandidate> = serde_json::from_str(json).unwrap();
        let w = candidates_to_watched(cands, 5);
        assert_eq!(w[0].pool.as_deref(), Some("pAAA"));
        assert_eq!(w[0].quote.as_deref(), Some("SOL"));
        assert_eq!(w[1].pool, None, "pool-less rows stay REST-priced");
        assert_eq!(w[1].quote, None);
    }

    #[test]
    fn scan_candidate_pools_list_maps_to_multi_venue() {
        let json = r#"[
            {"symbol":"AAA","mint":"mAAA","pools":[
                {"pool":"pRAY","quote":"SOL","dex":"raydium"},
                {"pool":"pORCA","quote":"USDC","dex":"orca"}
            ]},
            {"symbol":"BBB","mint":"mBBB","pool":"pLEGACY","quote":"SOL","dex":"pumpswap"}
        ]"#;
        let cands: Vec<ScanCandidate> = serde_json::from_str(json).unwrap();
        let w = candidates_to_watched(cands, 5);
        // Top-N `pools` list → multi-venue; the single shorthand is nulled when pools present.
        assert_eq!(w[0].pool_refs().len(), 2);
        assert_eq!(w[0].pool_refs()[0].pool, "pRAY");
        assert_eq!(w[0].pool_refs()[1].pool, "pORCA");
        assert_eq!(w[0].pool, None, "shorthand nulled when pools present");
        // Legacy single-pool shorthand still honoured when `pools` is absent.
        assert_eq!(w[1].pool_refs().len(), 1);
        assert_eq!(w[1].pool_refs()[0].pool, "pLEGACY");
        // dynamic_pool_set unions every venue across discoveries (2 + 1 = 3).
        assert_eq!(dynamic_pool_set(&w).len(), 3);
    }

    #[test]
    fn parse_pool_configs_reads_decoder_output() {
        let raw = r#"[{
            "id": "BkPool111111111111111111111111111111111111",
            "dex": "pump_swap",
            "token_a": "So11111111111111111111111111111111111111112",
            "token_b": "mTOK",
            "vault_a": "va", "vault_b": "vb",
            "fee_bps": 25
        }]"#;
        let configs = parse_pool_configs(raw).expect("decoder-shaped JSON parses");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id, "BkPool111111111111111111111111111111111111");
        assert!(parse_pool_configs("not json").is_err());
    }

    /// A decoder that exits non-zero (per-pool cross-check failure) but still writes a
    /// partial batch to `--output` must have that batch salvaged, not discarded — see
    /// `scripts/fetch_pumpswap_pools.js` lines ~214-220.
    #[tokio::test]
    async fn run_pool_decode_salvages_partial_output_on_nonzero_exit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake_decoder.js");
        std::fs::write(
            &script,
            r#"
            const fs = require("fs");
            const args = process.argv.slice(2);
            const outIdx = args.indexOf("--output");
            const outPath = args[outIdx + 1];
            const pools = [{
                id: "BkPool111111111111111111111111111111111111",
                dex: "pump_swap",
                token_a: "So11111111111111111111111111111111111111112",
                token_b: "mTOK",
                vault_a: "va",
                vault_b: "vb",
                fee_bps: 25
            }];
            fs.writeFileSync(outPath, JSON.stringify(pools, null, 2));
            console.error("one pool failed cross-check");
            process.exitCode = 1;
            "#,
        )
        .expect("write fake decoder");

        let script_path = script.to_str().expect("utf8 path").to_string();
        let result = run_pool_decode(&script_path, &["p1".to_string(), "p2".to_string()]).await;
        let configs = result.expect("partial output should be salvaged, not bailed on");
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].id, "BkPool111111111111111111111111111111111111");
    }

    /// A decoder that exits non-zero AND writes nothing usable (empty array, or no
    /// file at all) must still bail — there's nothing to salvage.
    #[tokio::test]
    async fn run_pool_decode_bails_when_nothing_salvageable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("fake_decoder_empty.js");
        std::fs::write(
            &script,
            r#"
            const fs = require("fs");
            const args = process.argv.slice(2);
            const outIdx = args.indexOf("--output");
            const outPath = args[outIdx + 1];
            fs.writeFileSync(outPath, JSON.stringify([], null, 2));
            console.error("all pools failed cross-check");
            process.exitCode = 1;
            "#,
        )
        .expect("write fake decoder");

        let script_path = script.to_str().expect("utf8 path").to_string();
        let result = run_pool_decode(&script_path, &["p1".to_string()]).await;
        assert!(result.is_err(), "empty salvage must still bail");
    }

    #[test]
    fn cold_watched_selects_tokens_with_too_few_observations() {
        let mut history: VecDeque<PriceSnapshot> = VecDeque::new();
        for i in 0..(SORTINO_MIN_OBS as u64 + 5) {
            let mut prices = HashMap::new();
            prices.insert("warm".to_string(), 1.0 + i as f64);
            if i < 3 {
                prices.insert("cold".to_string(), 2.0);
            }
            history.push_back(PriceSnapshot { ts: i, prices });
        }
        let mut named = wt("COLD", "cold");
        named.name = Some("Cold Token".into());
        let watched = vec![wt("WARM", "warm"), named, wt("NEW", "never_priced")];
        assert_eq!(
            cold_watched(&watched, &history),
            vec![
                ("cold".to_string(), "Cold Token".to_string()),
                ("never_priced".to_string(), "NEW".to_string()),
            ]
        );
    }

    #[test]
    fn snapshot_predates_fill_only_when_a_later_fill_exists() {
        let t0 = Instant::now();
        assert!(!snapshot_predates_fill(t0, None));
        assert!(snapshot_predates_fill(t0, Some(t0 + Duration::from_secs(1))));
        assert!(!snapshot_predates_fill(t0 + Duration::from_secs(1), Some(t0)));
    }

    #[test]
    fn wallet_snapshot_usable_within_max_age_only() {
        let t0 = Instant::now();
        let max = Duration::from_secs(300);
        assert!(wallet_snapshot_usable(t0, t0 + Duration::from_secs(299), max));
        assert!(!wallet_snapshot_usable(t0, t0 + Duration::from_secs(301), max));
    }

    #[test]
    fn scan_overdue_after_twice_the_interval() {
        let t0 = Instant::now();
        assert!(!scan_overdue(t0, t0 + Duration::from_secs(1199), 600));
        assert!(scan_overdue(t0, t0 + Duration::from_secs(1201), 600));
    }
}
