use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use reqwest::Client;
use tracing::{error, info, warn};

use super::analyzer::{self, Alert, AnalysisConfig};
use super::PortfolioConfig;
use super::emailer;
use super::history::{self, PriceSnapshot};
use super::pricer;
use super::Portfolio;

pub async fn run(cfg: PortfolioConfig, http: Client) {
    let portfolio = match super::load_portfolio(&cfg.portfolio_path) {
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

    // Backfill from Birdeye when history is sparse (< 60 entries = < 1 hour).
    // After backfill, persist the new snapshots to disk so the next startup
    // loads them and skips this step entirely.
    if history.len() < 60 {
        if let Some(api_key) = &cfg.birdeye_api_key {
            backfill_birdeye(&http, api_key, &portfolio, &mut history).await;

            // Write all in-memory snapshots to the JSONL file so future
            // restarts don't need to hit Birdeye again.
            info!("portfolio: persisting backfill to disk...");
            let mut written = 0usize;
            for snap in &history {
                if history::append_snapshot(&history_path, snap).is_ok() {
                    written += 1;
                }
            }
            info!("portfolio: wrote {written} snapshots to {}", history_path.display());

            // Let Birdeye's rate limiter recover before the first price fetch.
            tokio::time::sleep(Duration::from_secs(5)).await;
        } else {
            info!("portfolio: no BIRDEYE_API_KEY set, skipping history backfill");
        }
    }

    let token_mints: Vec<String> = portfolio.tokens.iter().map(|t| t.mint.clone()).collect();
    let analysis_cfg = AnalysisConfig {
        alert_pct_5m: cfg.alert_pct_5m,
        alert_pct_1h: cfg.alert_pct_1h,
    };
    let cooldown = Duration::from_secs(cfg.alert_cooldown_min * 60);
    let mut last_alert: Option<Instant> = None;
    let mut last_prices: std::collections::HashMap<String, f64> = std::collections::HashMap::new();

    // EUR/USD rate — fetched once at startup, refreshed every 10 ticks.
    let mut eur_rate = match pricer::fetch_eur_rate(&http).await {
        Ok(r) => { info!("portfolio: EUR rate = {r:.4}"); r }
        Err(e) => { warn!("portfolio: EUR rate fetch failed: {e}, defaulting to 0.92"); 0.92 }
    };
    let mut ticks_since_eur_refresh = 0u32;

    // interval_at delays the first tick by the full period so it doesn't
    // fire immediately on top of the backfill requests.
    let start = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut ticker = tokio::time::interval_at(start, Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        // Fetch current prices; merge with last known prices so tokens that
        // hit a transient error still show their previous value rather than $0.
        let fresh = match pricer::fetch_prices(&http, &token_mints, cfg.birdeye_api_key.as_deref()).await {
            Ok(p) => p,
            Err(e) => {
                warn!("portfolio: price fetch failed: {e}");
                continue;
            }
        };
        // Carry forward last known prices for any mint missing from this tick.
        let mut prices = last_prices.clone();
        prices.extend(fresh);
        last_prices = prices.clone();

        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let snap = PriceSnapshot {
            ts,
            prices: prices.clone(),
        };

        // Refresh EUR rate every 10 ticks (~10 minutes)
        ticks_since_eur_refresh += 1;
        if ticks_since_eur_refresh >= 10 {
            if let Ok(r) = pricer::fetch_eur_rate(&http).await {
                eur_rate = r;
            }
            ticks_since_eur_refresh = 0;
        }

        // Log asset values
        log_values(&portfolio, &prices, eur_rate);

        // Persist to disk
        if let Err(e) = history::append_snapshot(&history_path, &snap) {
            warn!("portfolio: failed to append snapshot: {e}");
        }

        // Update in-memory deque
        if history.len() == history::MAX_HISTORY {
            history.pop_front();
        }
        history.push_back(snap);

        // Analyse trends
        let alerts = analyzer::analyze(&history, &portfolio, &analysis_cfg);
        if alerts.is_empty() {
            continue;
        }

        // Always log alert details to console regardless of cooldown.
        for alert in &alerts {
            info!(
                "portfolio: ⚠  {} — {} (€{:.2})",
                alert.symbol,
                alert.kind,
                alert.current_value_usd * eur_rate,
            );
        }

        // Respect cooldown before sending email.
        if let Some(last) = last_alert {
            if last.elapsed() < cooldown {
                let remaining = cooldown - last.elapsed();
                info!(
                    "portfolio: email suppressed — cooldown {:.0}m remaining",
                    remaining.as_secs_f64() / 60.0
                );
                continue;
            }
        }

        // Build and send email
        let (subject, body) = build_email(&portfolio, &prices, &alerts, eur_rate);
        match emailer::send_alert(&cfg, &subject, &body).await {
            Ok(()) => {
                info!("portfolio: alert email sent ({} alert(s))", alerts.len());
                last_alert = Some(Instant::now());
            }
            Err(e) => error!("portfolio: failed to send alert email: {e}"),
        }
    }
}

fn log_values(portfolio: &Portfolio, prices: &std::collections::HashMap<String, f64>, eur: f64) {
    let sol_usd = prices.get("SOL").copied().unwrap_or(0.0);
    let sol_eur = sol_usd * eur;
    let sol_value = sol_eur * portfolio.sol_amount;
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
        info!(
            "portfolio: {} {:.4} × €{:.4} = €{:.2}",
            token.symbol, token.amount, price_eur, value
        );
    }
    info!("portfolio: total value = €{:.2}", total);
}

fn build_email(
    portfolio: &Portfolio,
    prices: &std::collections::HashMap<String, f64>,
    alerts: &[Alert],
    eur: f64,
) -> (String, String) {
    let subject = format!("[Portfolio Alert] {} signal(s) detected", alerts.len());

    let mut body = String::from("Portfolio Alerts\n");
    body.push_str(&"=".repeat(30));
    body.push('\n');

    for alert in alerts {
        body.push_str(&format!(
            "⚠  {} — {} (price: €{:.4}, value: €{:.2})\n",
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
        "SOL   {:.4} × €{:.2} = €{:.2}\n",
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
            "{:<8} {:.4} × €{:.4} = €{:.2}\n",
            token.symbol, token.amount, price_eur, value
        ));
    }
    body.push_str(&format!("\nTotal: €{:.2}\n", total));

    (subject, body)
}

async fn backfill_birdeye(
    http: &Client,
    api_key: &str,
    portfolio: &Portfolio,
    history: &mut VecDeque<PriceSnapshot>,
) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let from = now.saturating_sub(24 * 3600);

    const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

    // Build mint → symbol map for readable log messages
    let mut symbol_map: std::collections::HashMap<String, String> = portfolio
        .tokens
        .iter()
        .map(|t| (t.mint.clone(), t.symbol.clone()))
        .collect();
    symbol_map.insert(SOL_MINT.to_string(), "SOL".to_string());

    let mut mints: Vec<String> = portfolio.tokens.iter().map(|t| t.mint.clone()).collect();
    mints.push(SOL_MINT.to_string());

    for (i, mint) in mints.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
        let label = symbol_map.get(mint).map(String::as_str).unwrap_or(&mint[..8]);
        match pricer::fetch_history_birdeye(http, api_key, mint, from, now).await {
            Ok(mut snaps) => {
                if mint == SOL_MINT {
                    for snap in &mut snaps {
                        if let Some(price) = snap.prices.get(SOL_MINT).copied() {
                            snap.prices.insert("SOL".to_string(), price);
                        }
                    }
                }
                info!("portfolio: backfilled {} snapshots for {}", snaps.len(), label);
                history::merge_backfill(history, snaps);
            }
            Err(e) => warn!("portfolio: Birdeye backfill failed for {label}: {e}"),
        }
    }
}
