use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use reqwest::Client;
use tracing::{error, info, warn};

use super::analyzer::{self, Alert, AnalysisConfig, RiskReport, SwapSuggestion};
use super::momentum::{self, MomentumContext, TradeOutcome};
use super::momentum_universe::{self, WatchedToken};
use super::scanner;
use super::suggestions::{self, Suggestion, SORTINO_MIN_OBS};
use super::{Portfolio, PortfolioConfig, TokenEntry};
use super::emailer;
use super::history::{self, PriceSnapshot};
use super::pricer;

const PRICE_STALE_THRESHOLD: Duration = Duration::from_secs(300);

pub async fn run(cfg: PortfolioConfig, http: Client) {
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
                        v.push(WatchedToken { symbol: sym.clone(), mint: mint.clone(), name: None, equity: None, params: None });
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

    let mut known_price_keys = build_known_price_keys(&token_mints);

    // One-time decimals for watched + held + USDC, used for raw↔human conversions
    // in the trader. Missing decimals → that candidate is simply skipped.
    let decimals: HashMap<String, u8> = if cfg.enable_momentum_trader {
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

    // Reconcile any recorded position against the freshly-scanned wallet so the
    // trader never resumes managing a phantom (stale live position).
    momentum::reconcile_startup_position(&cfg, &portfolio);

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

    // Seed last_prices from the most recent history snapshot so the first tick
    // never shows €0 if fetch_prices fails before any data is collected.
    let mut last_prices: HashMap<String, f64> = history
        .back()
        .map(|snap| snap.prices.clone())
        .unwrap_or_default();
    let mut last_price_update: HashMap<String, Instant> = HashMap::new();

    // If FLAT, optionally adopt a manually-acquired wallet holding (gated by
    // MOMENTUM_ADOPT_WALLET_POSITION) so the trader manages it. Uses the seeded
    // last_prices for the current price; no-op unless exactly one watched holding
    // worth ≥ half the trade size is present.
    if cfg.enable_momentum_trader {
        momentum::adopt_wallet_position(&cfg, &portfolio, &last_prices, &watched);
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
    // scanning is off, `effective` stays equal to `watched` (zero behavior change).
    let mut discovered: Vec<WatchedToken> = Vec::new();
    let mut effective: Vec<WatchedToken> = watched.clone();
    // Scan cadence in 60s monitor ticks (floored to 1 so a tiny interval can't div to 0).
    let scan_every_ticks = (cfg.momentum_scan_interval_secs / 60).max(1);
    // Pre-armed so the first eligible monitor tick scans (warm start), then hourly.
    let mut ticks_since_scan = scan_every_ticks;

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

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = fast_ticker.tick() => {
                // EXIT-only fast path; only acts when HOLDING. The mctx borrows
                // are released before we mutate `portfolio` on a live fill.
                if cfg.enable_momentum_trader {
                    let outcomes = {
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &effective, prices_usd: &last_prices,
                            history: &history, decimals: &decimals, http: &http,
                            usdc_balance: usdc_balance(&portfolio),
                        };
                        momentum::maybe_exit(&mctx).await
                    };
                    // Task 5 will do the full watcher rewrite; minimal adaptation here.
                    match outcomes {
                        Ok(os) => {
                            for o in os {
                                if !o.dry_run() { apply_outcome(&mut portfolio, &o); }
                            }
                        }
                        Err(e) => error!("momentum: exit tick error: {e:#}"),
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

        // Re-scan the wallet on-chain every 5 ticks (~5 min) so external funding /
        // swaps are picked up without a restart. `scan_and_save` rewrites
        // portfolio.json and its merge() drops sold tokens + refreshes balances from
        // chain — so the momentum entry gate sees the true current USDC. The RPC runs
        // on a blocking thread inside scan_wallet, so this `.await` never stalls the
        // select! loop.
        ticks_since_rescan += 1;
        if ticks_since_rescan >= 5 {
            ticks_since_rescan = 0;
            match scanner::scan_and_save(&cfg, &http).await {
                Ok(new_p) => {
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
                    known_price_keys = build_known_price_keys(&token_mints);
                    if changed {
                        info!("portfolio: wallet re-scanned — holdings CHANGED ({} tokens, {:.2} USDC available)",
                            portfolio.tokens.len(), usdc);
                        // The change may have sold/moved a live position's token out from
                        // under the bot — invalidate the recorded position if it's no longer
                        // wallet-backed (paper positions are left alone; see the fn doc).
                        momentum::invalidate_unbacked_position(&cfg, &portfolio);
                    } else {
                        info!("portfolio: wallet re-scanned — unchanged ({:.2} USDC available)", usdc);
                    }
                }
                Err(e) => warn!("portfolio: periodic wallet re-scan failed: {e}"),
            }
        }

        // Periodic generic token scan → rolling in-memory top-N discovery overlay
        // (momentum only; opt-in). One-shot `node scan_tokens.js --json`; best-effort —
        // a failed/slow scan logs and keeps the prior `discovered`. Curated file untouched.
        if cfg.enable_momentum_trader && cfg.momentum_scan_enable {
            ticks_since_scan += 1;
            if ticks_since_scan >= scan_every_ticks {
                ticks_since_scan = 0;
                match run_token_scan(&cfg.momentum_scan_script, cfg.momentum_scan_top_n).await {
                    Ok(found) => {
                        if discovered_changed(&discovered, &found) {
                            discovered = found;
                            let syms: Vec<&str> = discovered.iter().map(|w| w.symbol.as_str()).collect();
                            info!("momentum: scan → discovered {:?}", syms);
                            // Warm cold new entrants so they are rankable immediately
                            // (no-op for mints already warm/held).
                            if let Some(api_key) = &cfg.birdeye_api_key {
                                backfill_watched_cold(
                                    &http, api_key, &discovered,
                                    cfg.momentum_lookback_obs, &mut history, &history_path,
                                ).await;
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
                    Err(e) => warn!("momentum: token scan failed ({e}); keeping {} discovered", discovered.len()),
                }
            }
        }

        // Fetch current prices; merge with last known prices so tokens that
        // hit a transient error still show their previous value rather than $0.
        let fresh = match pricer::fetch_prices(&http, &token_mints, cfg.birdeye_api_key.as_deref()).await {
            Ok(p) => p,
            Err(e) => {
                warn!("portfolio: price fetch failed: {e}");
                continue;
            }
        };
        let fetch_time = Instant::now();
        for key in fresh.keys() {
            last_price_update.insert(key.clone(), fetch_time);
        }

        // Carry forward last known prices for any mint missing from this tick.
        let mut prices = last_prices.clone();
        prices.extend(fresh);
        last_prices = prices.clone();
        last_prices.retain(|k, _| known_price_keys.contains(k.as_str()));

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let snap = PriceSnapshot { ts, prices: prices.clone() };

        // Refresh EUR rate every 10 ticks (~10 minutes).
        ticks_since_eur_refresh += 1;
        if ticks_since_eur_refresh >= 10 {
            if let Ok(r) = pricer::fetch_eur_rate(&http).await {
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

        // Compute risk metrics, log them, and write a JSON sidecar for external tooling.
        let risk_report = analyzer::compute_risk(&history, &portfolio, eur_rate, &analysis_cfg);
        log_risk_report(&risk_report, analysis_cfg.zscore_min_obs);
        if let Ok(json) = serde_json::to_string_pretty(&risk_report) {
            if let Err(e) = std::fs::write(&cfg.status_path, json) {
                warn!("portfolio: failed to write status sidecar: {e}");
            }
        }

        // Momentum ENTRY check (only acts when FLAT). Runs every monitor tick,
        // before the alert path, so it isn't skipped on ticks without alerts.
        if cfg.enable_momentum_trader {
            // Refresh the effective universe (curated ∪ discovered ∪ held) so this
            // tick's ranking — and the fast exit arm until the next tick — see the
            // current overlay. Skipped when scanning is off (effective == watched).
            if cfg.momentum_scan_enable {
                effective = effective_universe(&watched, &discovered, held_token(&cfg).as_ref());
            }
            let outcome = {
                let mctx = MomentumContext {
                    cfg: &cfg, watched: &effective, prices_usd: &prices,
                    history: &history, decimals: &decimals, http: &http,
                    usdc_balance: usdc_balance(&portfolio),
                };
                momentum::maybe_enter(&mctx).await
            };
            match outcome {
                Ok(Some(o)) => if !o.dry_run() { apply_outcome(&mut portfolio, &o); },
                Ok(None) => {}
                Err(e) => error!("momentum: entry tick error: {e:#}"),
            }
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

        // Generate alerts using pre-computed risk data.
        let alerts = analyzer::analyze(&history, &portfolio, &risk_report, &analysis_cfg);
        if alerts.is_empty() {
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

/// One row of `scan_tokens.js --json`. Extra fields (vol24, liq) are ignored —
/// the script already volume-sorted, so the watcher only needs identity.
#[derive(Debug, serde::Deserialize)]
struct ScanCandidate {
    symbol: String,
    mint: String,
    #[serde(default)]
    name: Option<String>,
}

/// Spawn `node <script> --json`, parse stdout, and return the top-`top_n` rows as
/// watch entries. Best-effort: the caller logs any Err and keeps the prior set.
async fn run_token_scan(script: &str, top_n: usize) -> anyhow::Result<Vec<WatchedToken>> {
    let out = tokio::process::Command::new("node")
        .arg(script)
        .arg("--json")
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
    let cands: Vec<ScanCandidate> = serde_json::from_slice(&out.stdout)
        .context("scan stdout was not a JSON array of {symbol,mint,name,...}")?;
    Ok(cands
        .into_iter()
        .take(top_n)
        .map(|c| WatchedToken { symbol: c.symbol, mint: c.mint, name: c.name, equity: None, params: None })
        .collect())
}

/// Effective momentum universe = curated ∪ discovered ∪ {held}, deduped by mint
/// (curated wins, then discovered, then the held token). The held clause keeps a
/// position in a discovered name rankable after it rolls off the top-N.
fn effective_universe(
    curated: &[WatchedToken],
    discovered: &[WatchedToken],
    held: Option<&WatchedToken>,
) -> Vec<WatchedToken> {
    let mut out: Vec<WatchedToken> = Vec::with_capacity(curated.len() + discovered.len() + 1);
    let mut seen: HashSet<&str> = HashSet::new();
    for w in curated.iter().chain(discovered.iter()).chain(held) {
        if seen.insert(w.mint.as_str()) {
            out.push(w.clone());
        }
    }
    out
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

/// The momentum trader's currently-held token (if any), read from its state file,
/// as a watch entry — so the rolling overlay never orphans an open position.
/// `name`/`equity` are unknown here (`None`); the exit path doesn't need them.
fn held_token(cfg: &PortfolioConfig) -> Option<WatchedToken> {
    super::momentum_state::load(Path::new(&cfg.momentum_state_path))
        .ok()
        .and_then(|s| s.position().cloned())
        .map(|p| WatchedToken { symbol: p.symbol, mint: p.mint, name: None, equity: None, params: None })
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
async fn backfill_watched_cold(
    http: &Client,
    api_key: &str,
    watched: &[WatchedToken],
    lookback_obs: usize,
    history: &mut VecDeque<PriceSnapshot>,
    history_path: &Path,
) {
    let cold: Vec<(String, String)> = watched
        .iter()
        .filter(|w| obs_count(history, &w.mint) <= SORTINO_MIN_OBS)
        .map(|w| (w.mint.clone(), w.name.clone().unwrap_or_else(|| w.symbol.clone())))
        .collect();
    if cold.is_empty() {
        return;
    }
    info!("momentum: warming up {} cold watched token(s) via Birdeye", cold.len());
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    // Just the lookback window (+4h margin), capped at 7 days — enough for the
    // Sortino window with far fewer paginated requests than a full pull.
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
    if !all_snaps.is_empty() {
        history::merge_backfill_grid(history, all_snaps);
        if let Err(e) = history::rewrite_history(history_path, history) {
            warn!("momentum: backfill persist failed: {e}");
        }
    }
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
        WatchedToken { symbol: sym.into(), mint: mint.into(), name: None, equity: None, params: None }
    }

    #[test]
    fn effective_universe_dedups_curated_first() {
        let curated = vec![wt("RAY", "mRAY"), wt("JUP", "mJUP")];
        let discovered = vec![wt("RAY2", "mRAY"), wt("BONK", "mBONK")]; // mRAY is a dup
        let eff = effective_universe(&curated, &discovered, None);
        let mints: Vec<&str> = eff.iter().map(|w| w.mint.as_str()).collect();
        assert_eq!(mints, vec!["mRAY", "mJUP", "mBONK"]);
        assert_eq!(eff[0].symbol, "RAY", "curated entry wins the dup");
    }

    #[test]
    fn effective_universe_retains_and_dedups_held() {
        let curated = vec![wt("RAY", "mRAY")];
        let discovered = vec![wt("BONK", "mBONK")];
        // Held token absent from both → retained.
        let held = wt("WIF", "mWIF");
        let eff = effective_universe(&curated, &discovered, Some(&held));
        assert_eq!(eff.len(), 3);
        assert!(eff.iter().any(|w| w.mint == "mWIF"));
        // Held token already present → not duplicated.
        let held2 = wt("RAY", "mRAY");
        let eff2 = effective_universe(&curated, &discovered, Some(&held2));
        assert_eq!(eff2.len(), 2);
    }

    #[test]
    fn effective_universe_empty_discovered_equals_curated() {
        let curated = vec![wt("RAY", "mRAY"), wt("JUP", "mJUP")];
        let eff = effective_universe(&curated, &[], None);
        assert_eq!(eff.len(), 2);
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
            .map(|c| WatchedToken { symbol: c.symbol, mint: c.mint, name: c.name, equity: None, params: None })
            .collect();
        assert_eq!(top.len(), 2);
        assert_eq!((top[0].symbol.as_str(), top[0].name.as_deref()), ("AAA", Some("Alpha")));
        assert_eq!(top[1].mint, "mBBB");
        assert!(top[1].name.is_none(), "missing name → None, extra fields ignored");
    }
}
