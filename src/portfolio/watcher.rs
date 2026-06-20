use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
                    "momentum: watching {} tokens (DRY_RUN_MOMENTUM_TRADER={}, poll={}s, trail={}%)",
                    w.len(), cfg.momentum_dry_run, cfg.momentum_poll_secs, cfg.momentum_trail_pct
                );
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
            backfill_watched_cold(&http, api_key, &watched, &mut history, &history_path).await;
        }
    }

    let mut token_mints: Vec<String> = portfolio.tokens.iter().map(|t| t.mint.clone()).collect();
    for w in &watched {
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

    // Portfolio hot-reload: track mtime and re-read when the file changes.
    let mut portfolio_mtime = std::fs::metadata(&cfg.portfolio_path)
        .and_then(|m| m.modified())
        .ok();
    let mut ticks_since_reload_check = 0u32;

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

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = fast_ticker.tick() => {
                // EXIT-only fast path; only acts when HOLDING. The mctx borrows
                // are released before we mutate `portfolio` on a live fill.
                if cfg.enable_momentum_trader {
                    let outcome = {
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &watched, prices_usd: &last_prices,
                            history: &history, decimals: &decimals, http: &http,
                        };
                        momentum::maybe_exit(&mctx).await
                    };
                    match outcome {
                        Ok(Some(o)) => if !o.dry_run() { apply_outcome(&mut portfolio, &o); },
                        Ok(None) => {}
                        Err(e) => error!("momentum: exit tick error: {e:#}"),
                    }
                }
                continue;
            }
            _ = tokio::signal::ctrl_c() => {
                info!("portfolio: shutting down — persisting final history");
                if let Err(e) = history::rewrite_history(&history_path, &history) {
                    warn!("portfolio: final history flush failed: {e}");
                }
                break;
            }
        }

        // Portfolio hot-reload: check mtime every 5 ticks (~5 minutes).
        ticks_since_reload_check += 1;
        if ticks_since_reload_check >= 5 {
            ticks_since_reload_check = 0;
            let new_mtime = std::fs::metadata(&cfg.portfolio_path)
                .and_then(|m| m.modified())
                .ok();
            if new_mtime.is_some() && new_mtime != portfolio_mtime {
                match super::load_portfolio(&cfg.portfolio_path) {
                    Ok(new_p) => {
                        info!("portfolio: reloaded from disk ({} tokens)", new_p.tokens.len());
                        portfolio = new_p;
                        token_mints = portfolio.tokens.iter().map(|t| t.mint.clone()).collect();
                        // Re-union the watched mints so a reload doesn't drop them from pricing.
                        for w in &watched {
                            if !token_mints.contains(&w.mint) {
                                token_mints.push(w.mint.clone());
                            }
                        }
                        known_price_keys = build_known_price_keys(&token_mints);
                        portfolio_mtime = new_mtime;
                    }
                    Err(e) => warn!("portfolio: reload failed: {e}"),
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
            let outcome = {
                let mctx = MomentumContext {
                    cfg: &cfg, watched: &watched, prices_usd: &prices,
                    history: &history, decimals: &decimals, http: &http,
                };
                momentum::maybe_enter(&mctx).await
            };
            match outcome {
                Ok(Some(o)) => if !o.dry_run() { apply_outcome(&mut portfolio, &o); },
                Ok(None) => {}
                Err(e) => error!("momentum: entry tick error: {e:#}"),
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
    }
}

fn build_known_price_keys(token_mints: &[String]) -> std::collections::HashSet<String> {
    let mut s = std::collections::HashSet::from([
        "SOL".to_string(),
        "So11111111111111111111111111111111111111112".to_string(),
    ]);
    s.extend(token_mints.iter().cloned());
    s
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

/// Warm up watched momentum mints that are short of the warm-up minimum
/// (≤ `SORTINO_MIN_OBS` observations). For each, fetch ~7 days of 1-min candles
/// from Birdeye and **graft** them onto the existing snapshot grid (forward-fill,
/// no new snapshots — see `history::graft_mint_backfill`), so a just-added token
/// becomes rankable at boot instead of after a ~2h live warm-up. Already-warm
/// mints are skipped, so this no-ops in the common case.
async fn backfill_watched_cold(
    http: &Client,
    api_key: &str,
    watched: &[WatchedToken],
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
    info!(
        "momentum: warming up {} cold watched token(s) via Birdeye (≤{} obs)",
        cold.len(),
        SORTINO_MIN_OBS
    );
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let from = now.saturating_sub(7 * 24 * 3600);
    // Tolerate up to a 5-min gap when matching a candle to a live snapshot.
    const GRAFT_MAX_GAP_SECS: u64 = 300;
    for (i, (mint, label)) in cold.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
        match pricer::fetch_history_birdeye(http, api_key, mint, from, now).await {
            Ok(snaps) => {
                let candles: Vec<(u64, f64)> = snaps
                    .iter()
                    .filter_map(|s| s.prices.get(mint).map(|p| (s.ts, *p)))
                    .collect();
                let filled = history::graft_mint_backfill(history, mint, candles, GRAFT_MAX_GAP_SECS);
                info!("momentum: grafted {filled} backfilled prices onto the grid for {label}");
                if let Err(e) = history::rewrite_history(history_path, history) {
                    warn!("momentum: backfill persist failed for {label}: {e}");
                }
            }
            Err(e) => warn!("momentum: Birdeye warm-up failed for {label}: {e}"),
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
