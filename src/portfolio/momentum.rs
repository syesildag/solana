//! Momentum trader engine.
//!
//! A single-position, Sortino-ranked, trailing-stop strategy living in the
//! `portfolio-watcher` binary. It holds USDC when FLAT, rotates into the
//! strongest-momentum watched token, rides it, and trails out back to USDC.
//!
//! Two entry points, driven by the watcher's dual cadence:
//!   - [`maybe_enter`] — the 60s monitoring tick (only when FLAT). Ranks the
//!     watched universe by Sortino over `MOMENTUM_LOOKBACK_OBS` of 1-min
//!     history, gates, and buys a fixed USDC notional of the best.
//!   - [`maybe_exit`]  — the fast `MOMENTUM_POLL_SECS` loop (only when HOLDING).
//!     Fetches the held token's fresh price, updates the peak, and sells the
//!     whole position back to USDC the moment the trailing stop trips.
//!
//! `DRY_RUN_MOMENTUM_TRADER` (default true) paper-trades: real `/quote`, never
//! `/swap`. The execution/state/safety plumbing is lifted from the (removed)
//! auto-rebalancer; only the decision logic here is new.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::VersionedTransaction;
use tracing::{error, info, warn};

use super::history::PriceSnapshot;
use super::momentum_actions::{self, Action, ActionKind};
use super::momentum_state::{self, Position, TradeRecord};
use super::momentum_universe::{WatchedToken, USDC_DECIMALS, USDC_MINT};
use super::suggestions::{compute_sortino, log_returns, SORTINO_MIN_OBS};
use super::{emailer, jupiter, pricer, scanner, Portfolio, PortfolioConfig};

const BASE_FEE_LAMPORTS: u64 = 5_000;
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(45);
/// Price key the pricer uses for native SOL (tokens are keyed by mint).
const SOL_KEY: &str = "SOL";

/// Everything the engine needs each tick. Prices/history come from the 60s
/// monitoring loop; the exit path re-fetches the held token's price itself.
pub struct MomentumContext<'a> {
    pub cfg: &'a PortfolioConfig,
    pub watched: &'a [WatchedToken],
    pub prices_usd: &'a HashMap<String, f64>,
    pub history: &'a VecDeque<PriceSnapshot>,
    pub decimals: &'a HashMap<String, u8>,
    pub http: &'a Client,
    /// Current USDC holdings (the cash leg) — entry is skipped below the trade size.
    pub usdc_balance: f64,
}

/// What a tick did — the watcher uses this to mutate the in-memory portfolio on
/// live fills (dry-run fills are ignored, they don't touch real holdings).
#[derive(Debug, Clone)]
pub enum TradeOutcome {
    Entered { symbol: String, mint: String, token_amount: f64, usdc_spent: f64, dry_run: bool },
    Exited { symbol: String, mint: String, usdc_out: f64, dry_run: bool },
}

impl TradeOutcome {
    pub fn dry_run(&self) -> bool {
        match self {
            TradeOutcome::Entered { dry_run, .. } | TradeOutcome::Exited { dry_run, .. } => *dry_run,
        }
    }
}

/// A ranked entry candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub symbol: String,
    pub mint: String,
    pub sortino: f64,
    pub price_usd: f64,
    pub obs: usize,
    /// Price hasn't moved over the staleness window → market closed/halted; the
    /// entry path skips these.
    pub stale: bool,
}

// ───────────────────────── pure helpers (unit-tested) ─────────────────────────

/// Extract the positive price series for one mint from history, oldest first.
pub fn price_series_for_mint(history: &VecDeque<PriceSnapshot>, mint: &str) -> Vec<f64> {
    history
        .iter()
        .filter_map(|s| s.prices.get(mint).copied())
        .filter(|p| *p > 0.0)
        .collect()
}

/// Trailing-stop predicate: true when the price has fallen `trail_pct` below the
/// peak since entry. A non-positive peak never triggers (no valid high yet).
pub fn trailing_stop_triggered(price: f64, peak: f64, trail_pct: f64) -> bool {
    if peak <= 0.0 {
        return false;
    }
    price <= peak * (1.0 - trail_pct / 100.0)
}

/// Gas cost (two base fees + a buffer) expressed in bps of the trade notional.
pub fn est_gas_bps(trade_usdc: f64, sol_price_usd: f64) -> u32 {
    if trade_usdc <= 0.0 || sol_price_usd <= 0.0 {
        return 0;
    }
    let gas_lamports = BASE_FEE_LAMPORTS * 2 + 5_000;
    let gas_usd = gas_lamports as f64 / 1_000_000_000.0 * sol_price_usd;
    (gas_usd / trade_usdc * 10_000.0) as u32
}

/// Fractional price move below which two prices count as "unchanged".
const STALE_EPS_FRAC: f64 = 0.001; // 0.1%

/// `(timestamp, price)` series for a mint, oldest-first, positive prices only.
pub fn price_series_with_ts(history: &VecDeque<PriceSnapshot>, mint: &str) -> Vec<(u64, f64)> {
    history
        .iter()
        .filter_map(|s| s.prices.get(mint).map(|p| (s.ts, *p)))
        .filter(|(_, p)| *p > 0.0)
        .collect()
}

/// True if the price hasn't moved (>`STALE_EPS_FRAC`) in the last `stale_minutes`
/// of **wall-clock** time — i.e. the market is closed/halted. Timestamp-based on
/// purpose: a frozen price reads as "last changed N minutes ago" immediately, so
/// it's detected right after a restart instead of needing N fresh frozen samples
/// to accumulate (which is how a just-backfilled token slipped a count-based
/// check and got bought into a closed market). `stale_minutes == 0` disables it.
pub fn is_stale_ts(series: &[(u64, f64)], stale_minutes: usize) -> bool {
    if stale_minutes == 0 || series.len() < 2 {
        return false;
    }
    let (latest_ts, latest_px) = *series.last().unwrap();
    if latest_px <= 0.0 {
        return false;
    }
    let threshold = stale_minutes as f64;
    // Most recent point whose price differs from the latest = the last real move.
    for &(ts, px) in series.iter().rev() {
        if (px - latest_px).abs() / latest_px > STALE_EPS_FRAC {
            return latest_ts.saturating_sub(ts) as f64 / 60.0 >= threshold;
        }
    }
    // Never moved across the whole series → flat for its entire span.
    latest_ts.saturating_sub(series.first().unwrap().0) as f64 / 60.0 >= threshold
}

/// Rank watched tokens by Sortino over the lookback window. Only tokens with a
/// computable Sortino (≥120 returns) AND a positive current price appear, sorted
/// best-first. Each carries a `stale` flag (price frozen over `stale_window`
/// minutes → market closed); the entry path skips those.
pub fn rank_candidates(
    watched: &[WatchedToken],
    prices: &HashMap<String, f64>,
    history: &VecDeque<PriceSnapshot>,
    lookback: usize,
    stale_window: usize,
) -> Vec<Candidate> {
    let mut cands: Vec<Candidate> = Vec::new();
    for w in watched {
        let series = price_series_for_mint(history, &w.mint);
        let window: &[f64] = if series.len() > lookback {
            &series[series.len() - lookback..]
        } else {
            &series
        };
        let rets = log_returns(window);
        let Some(price) = prices.get(&w.mint).copied().filter(|p| *p > 0.0) else {
            continue;
        };
        if let Some(sortino) = compute_sortino(&rets) {
            cands.push(Candidate {
                symbol: w.symbol.clone(),
                mint: w.mint.clone(),
                sortino,
                price_usd: price,
                obs: rets.len(),
                // Closed-market guard applies only to equities (xStocks/ETFs);
                // 24/7 crypto is never flagged stale, even when low-volatility.
                stale: w.is_equity()
                    && is_stale_ts(&price_series_with_ts(history, &w.mint), stale_window),
            });
        }
    }
    cands.sort_by(|a, b| {
        b.sortino
            .partial_cmp(&a.sortino)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cands
}

/// Build the closed-trade record, computing realized PnL% off USDC committed.
pub fn build_trade_record(
    pos: &Position,
    exit_ts: i64,
    exit_price_usd: f64,
    usdc_out: f64,
    exit_sig: String,
) -> TradeRecord {
    let pnl_pct = if pos.usdc_spent > 0.0 {
        (usdc_out - pos.usdc_spent) / pos.usdc_spent * 100.0
    } else {
        0.0
    };
    TradeRecord {
        entry_ts: pos.entry_ts,
        exit_ts,
        mint: pos.mint.clone(),
        symbol: pos.symbol.clone(),
        entry_price_usd: pos.entry_price_usd,
        exit_price_usd,
        peak_price_usd: pos.peak_price_usd,
        usdc_in: pos.usdc_spent,
        usdc_out,
        pnl_pct,
        entry_sig: pos.entry_sig.clone(),
        exit_sig,
        dry_run: pos.dry_run,
    }
}

// ───────────────────────────── small utilities ─────────────────────────────

fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn audit(cfg: &PortfolioConfig, ts: i64, kind: ActionKind) {
    if let Err(e) = momentum_actions::append(Path::new(&cfg.momentum_actions_path), &Action { ts, kind })
    {
        warn!("momentum: audit append failed: {e}");
    }
}

fn halted(cfg: &PortfolioConfig) -> bool {
    matches!(
        momentum_state::read_halt(Path::new(&cfg.momentum_halt_path)),
        Ok(Some(_))
    )
}

async fn email_trade(cfg: &PortfolioConfig, subject: &str, body: &str) {
    // Paper trades stay silent — only real fills notify. (Price alerts are a
    // separate path and are unaffected by DRY_RUN_MOMENTUM_TRADER.)
    if cfg.momentum_dry_run {
        return;
    }
    if let Err(e) = emailer::send_alert(cfg, subject, body).await {
        warn!("momentum: trade email failed: {e}");
    }
}

/// "SYMBOL — Name" when the watch list carries a name for the mint, else "SYMBOL".
fn token_label(watched: &[WatchedToken], mint: &str, symbol: &str) -> String {
    match watched.iter().find(|w| w.mint == mint).and_then(|w| w.name.as_deref()) {
        Some(name) if !name.is_empty() => format!("{symbol} — {name}"),
        _ => symbol.to_string(),
    }
}

// ─────────────────────────── startup reconciliation ───────────────────────────

/// At startup, ground the recorded position in reality. A **live** position must
/// be backed by the wallet — if `portfolio` (freshly scanned on-chain) doesn't
/// hold that mint, the record is stale (sold manually, never filled, or the
/// wallet changed) → clear it so the bot doesn't manage a phantom. **Paper**
/// (dry-run) positions are simulated, not wallet-backed, so they're kept as-is.
/// Call once before the loop.
pub fn reconcile_startup_position(cfg: &PortfolioConfig, portfolio: &Portfolio) {
    if !cfg.enable_momentum_trader {
        return;
    }
    let path = Path::new(&cfg.momentum_state_path);
    let mut state = match momentum_state::load(path) {
        Ok(s) => s,
        Err(e) => {
            warn!("momentum: could not load state at startup: {e}");
            return;
        }
    };
    let Some(pos) = state.position.clone() else {
        return; // FLAT — nothing to reconcile
    };
    if pos.dry_run {
        info!(
            "momentum: resuming PAPER position {} (entry ${:.6}, peak ${:.6}) — simulated, not wallet-backed",
            pos.symbol, pos.entry_price_usd, pos.peak_price_usd
        );
        return;
    }
    // Live: the wallet must actually hold the token.
    let held = portfolio
        .tokens
        .iter()
        .find(|t| t.mint == pos.mint)
        .map(|t| t.amount)
        .unwrap_or(0.0);
    if held <= 0.0 {
        warn!(
            "momentum: state says HOLDING {} but the wallet holds none — clearing stale position → FLAT",
            pos.symbol
        );
        state.position = None;
        state.last_exit_ts_per_mint.insert(pos.mint.clone(), now_ts());
        if let Err(e) = momentum_state::save(path, &state) {
            warn!("momentum: failed to persist reconciled state: {e}");
        }
    } else {
        info!(
            "momentum: resuming LIVE position {} — wallet holds {:.6} (entry ${:.6}, peak ${:.6})",
            pos.symbol, held, pos.entry_price_usd, pos.peak_price_usd
        );
    }
}

// ───────────────────────────── ENTRY (FLAT, 60s) ─────────────────────────────

pub async fn maybe_enter(ctx: &MomentumContext<'_>) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
    if !cfg.enable_momentum_trader || halted(cfg) {
        return Ok(None);
    }
    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    if let Some(pos) = state.position.as_ref() {
        // HOLDING — entry is a no-op (exit runs on the fast loop). Emit a
        // once-per-monitor-tick unrealized-PnL line so the open position is
        // trackable from the console, not just on exit.
        if let Some(px) = ctx.prices_usd.get(&pos.mint).copied().filter(|p| *p > 0.0) {
            let unreal = (px - pos.entry_price_usd) / pos.entry_price_usd * 100.0;
            info!(
                "momentum: HOLDING {} — entry ${:.6} now ${:.6} peak ${:.6} unrealized {:+.2}%",
                pos.symbol, pos.entry_price_usd, px, pos.peak_price_usd, unreal
            );
        }
        return Ok(None);
    }

    let ts = now_ts();
    let used = momentum_state::entries_last_24h(&state, ts);
    if used >= cfg.momentum_max_trades_per_day as usize {
        audit(cfg, ts, ActionKind::SkipDailyCap { used, cap: cfg.momentum_max_trades_per_day });
        return Ok(None);
    }

    // No capital, no trade. Guards an unfunded wallet (live: avoids submit
    // failures every tick; dry-run: avoids paper-trading USDC you don't hold).
    if ctx.usdc_balance < cfg.momentum_trade_usdc {
        info!(
            "momentum: USDC balance {:.2} < trade size {:.2} — staying FLAT (fund the wallet to trade)",
            ctx.usdc_balance, cfg.momentum_trade_usdc
        );
        audit(cfg, ts, ActionKind::SkipInsufficientUsdc {
            have: ctx.usdc_balance,
            need: cfg.momentum_trade_usdc,
        });
        return Ok(None);
    }

    // Rank, then take the best candidate not benched by the re-entry cooldown.
    let ranked = rank_candidates(
        ctx.watched,
        ctx.prices_usd,
        ctx.history,
        cfg.momentum_lookback_obs,
        cfg.momentum_stale_minutes,
    );

    // Visibility: log every watched token's Sortino each tick (best-first); tokens
    // whose market is frozen show "closed", and those still accumulating history
    // (or unpriced) show "warming".
    {
        let scored: HashMap<&str, f64> = ranked.iter().map(|c| (c.mint.as_str(), c.sortino)).collect();
        let mut parts: Vec<String> = ranked
            .iter()
            .map(|c| {
                if c.stale {
                    format!("{}=closed", c.symbol)
                } else {
                    format!("{}={:.2}", c.symbol, c.sortino)
                }
            })
            .collect();
        for w in ctx.watched {
            if !scored.contains_key(w.mint.as_str()) {
                parts.push(format!("{}=warming", w.symbol));
            }
        }
        info!("momentum: sortino — {}  (min {:.2})", parts.join("  "), cfg.momentum_min_sortino);
    }

    if ranked.is_empty() {
        // Observability: never silently inert. A token qualifies only with both a
        // live price AND ≥ (SORTINO_MIN_OBS+1) prices in the lookback window
        // (the window of N prices yields N-1 returns).
        info!(
            "momentum: no entry candidate yet — {} watched token(s) each need a live price + ≥{} obs in the lookback window (lookback={}); warming up",
            ctx.watched.len(), SORTINO_MIN_OBS + 1, cfg.momentum_lookback_obs
        );
        return Ok(None);
    }
    let mut best: Option<Candidate> = None;
    for c in ranked {
        if c.stale {
            audit(cfg, ts, ActionKind::SkipMarketClosed { symbol: c.symbol.clone() });
            continue; // market closed/frozen — never enter on a stale price
        }
        if let Some(&last) = state.last_exit_ts_per_mint.get(&c.mint) {
            let since = ts - last;
            if since < cfg.momentum_reentry_cooldown_secs {
                audit(cfg, ts, ActionKind::SkipReentryCooldown {
                    symbol: c.symbol.clone(),
                    secs_remaining: cfg.momentum_reentry_cooldown_secs - since,
                });
                continue;
            }
        }
        best = Some(c);
        break;
    }
    let Some(best) = best else {
        info!("momentum: all ranked candidates are in re-entry cooldown — staying FLAT");
        return Ok(None);
    };

    if best.sortino <= cfg.momentum_min_sortino {
        info!(
            "momentum: best candidate {} sortino={:.2} ≤ MIN_SORTINO {:.2} — staying FLAT",
            best.symbol, best.sortino, cfg.momentum_min_sortino
        );
        audit(cfg, ts, ActionKind::SkipBelowThreshold {
            best_symbol: best.symbol,
            best_sortino: best.sortino,
            min_sortino: cfg.momentum_min_sortino,
        });
        return Ok(None);
    }

    let Some(&token_decimals) = ctx.decimals.get(&best.mint) else {
        audit(cfg, ts, ActionKind::QuoteFailed { symbol: best.symbol, reason: "missing decimals".into() });
        return Ok(None);
    };

    // Quote USDC → token for the fixed notional.
    let usdc_raw = jupiter::to_raw_amount(cfg.momentum_trade_usdc, USDC_DECIMALS);
    let quote = match jupiter::quote(
        ctx.http,
        &cfg.momentum_jupiter_api_url,
        USDC_MINT,
        &best.mint,
        usdc_raw,
        cfg.momentum_slippage_bps,
    )
    .await
    {
        Ok(q) => q,
        Err(e) => {
            warn!("momentum: /quote failed for {} via {} — {e}", best.symbol, cfg.momentum_jupiter_api_url);
            audit(cfg, ts, ActionKind::QuoteFailed { symbol: best.symbol, reason: e.to_string() });
            return Ok(None);
        }
    };

    let slip_bps = jupiter::price_impact_bps(&quote);
    let sol_price = ctx.prices_usd.get(SOL_KEY).copied().unwrap_or(0.0);
    let gas_bps = est_gas_bps(cfg.momentum_trade_usdc, sol_price);
    let total_cost_bps = slip_bps + gas_bps;
    if total_cost_bps > cfg.momentum_max_cost_bps {
        audit(cfg, ts, ActionKind::SkipCostGate {
            symbol: best.symbol,
            total_cost_bps,
            gas_bps,
            slip_bps,
            budget_bps: cfg.momentum_max_cost_bps,
        });
        return Ok(None);
    }

    let expected_token = jupiter::from_raw_amount(quote.out_amount.parse::<u64>().unwrap_or(0), token_decimals);
    if expected_token <= 0.0 {
        audit(cfg, ts, ActionKind::QuoteFailed { symbol: best.symbol, reason: "zero out amount".into() });
        return Ok(None);
    }

    let sig = if cfg.momentum_dry_run {
        "dry-run".to_string()
    } else {
        let (s, confirmed) = submit_and_confirm(cfg, ctx.http, &quote).await?;
        if !confirmed {
            warn!("momentum: ENTER {} submitted but not confirmed in {}s (tx={s}); exit uses on-chain balance",
                best.symbol, CONFIRM_TIMEOUT.as_secs());
        }
        s.to_string()
    };

    state.position = Some(Position {
        mint: best.mint.clone(),
        symbol: best.symbol.clone(),
        entry_ts: ts,
        entry_price_usd: best.price_usd,
        token_amount: expected_token,
        usdc_spent: cfg.momentum_trade_usdc,
        peak_price_usd: best.price_usd,
        entry_sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });
    momentum_state::save(state_path, &state)?;

    audit(cfg, ts, ActionKind::Entered {
        symbol: best.symbol.clone(),
        mint: best.mint.clone(),
        usdc_in: cfg.momentum_trade_usdc,
        token_amount: expected_token,
        entry_price_usd: best.price_usd,
        cost_bps: total_cost_bps,
        sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });
    let tag = if cfg.momentum_dry_run { "DRY-RUN ENTER" } else { "ENTER" };
    let label = token_label(ctx.watched, &best.mint, &best.symbol);
    info!("momentum: {tag} {label} — {:.6} tokens for {} USDC @ ${:.6} (sortino={:.2}, cost={total_cost_bps}bps) tx={sig}",
        expected_token, cfg.momentum_trade_usdc, best.price_usd, best.sortino);
    // Emails are live-only (see email_trade), so the subject is always "ENTER".
    email_trade(cfg, &format!("[Momentum] ENTER {label}"),
        &format!("Bought {:.6} {} for {} USDC @ ${:.6}\nsortino={:.2}  cost={total_cost_bps}bps\ntx={sig}",
            expected_token, label, cfg.momentum_trade_usdc, best.price_usd, best.sortino)).await;

    Ok(Some(TradeOutcome::Entered {
        symbol: best.symbol,
        mint: best.mint,
        token_amount: expected_token,
        usdc_spent: cfg.momentum_trade_usdc,
        dry_run: cfg.momentum_dry_run,
    }))
}

// ─────────────────────────── EXIT (HOLDING, fast) ───────────────────────────

pub async fn maybe_exit(ctx: &MomentumContext<'_>) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
    if !cfg.enable_momentum_trader || halted(cfg) {
        return Ok(None);
    }
    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    let Some(mut pos) = state.position.clone() else {
        return Ok(None); // FLAT — nothing to exit
    };

    let ts = now_ts();

    // Mode-mismatch guard: a paper position must never be acted on in live mode
    // (it would try to sell tokens never bought) and vice-versa.
    if pos.dry_run != cfg.momentum_dry_run {
        audit(cfg, ts, ActionKind::ModeMismatch {
            position_dry_run: pos.dry_run,
            config_dry_run: cfg.momentum_dry_run,
        });
        error!("momentum: open position dry_run={} but DRY_RUN_MOMENTUM_TRADER={} — refusing to trade. \
            Be FLAT (or delete {}) before switching modes.",
            pos.dry_run, cfg.momentum_dry_run, cfg.momentum_state_path);
        return Ok(None);
    }

    // Fresh price for the held token — this is the fast-poll source (the 60s
    // `prices` cache is too stale for a tight stop).
    let price = pricer::fetch_prices(
        ctx.http,
        std::slice::from_ref(&pos.mint),
        cfg.birdeye_api_key.as_deref(),
    )
    .await
    .ok()
    .and_then(|m| m.get(&pos.mint).copied())
    .filter(|p| *p > 0.0);
    let Some(price) = price else {
        // Never trip the stop on missing/zero price data.
        return Ok(None);
    };

    // Update the high-water mark (persist on each rise so a restart keeps it).
    if price > pos.peak_price_usd {
        pos.peak_price_usd = price;
        state.position = Some(pos.clone());
        momentum_state::save(state_path, &state)?;
    }

    // Exit when the trailing stop trips OR the market closes (price frozen over
    // the staleness window) — flatten to USDC rather than hold a frozen position
    // across the close. The entry guard then keeps us FLAT until it reopens, so
    // this fires once per close, not in a churn.
    let stop_hit = trailing_stop_triggered(price, pos.peak_price_usd, cfg.momentum_trail_pct);
    // Only equities can be "market closed"; 24/7 crypto never flattens on staleness.
    let is_equity = ctx.watched.iter().any(|w| w.mint == pos.mint && w.is_equity());
    let market_closed = is_equity
        && cfg.momentum_stale_minutes > 0
        && is_stale_ts(&price_series_with_ts(ctx.history, &pos.mint), cfg.momentum_stale_minutes);
    if !stop_hit && !market_closed {
        return Ok(None); // still riding the gain, market open
    }
    let exit_reason = if stop_hit { "trailing stop" } else { "market closed" };

    // Sell the whole position back to USDC (unconditionally; no cost gate on exit
    // — never stay stuck holding because slippage is high).
    let Some(&token_decimals) = ctx.decimals.get(&pos.mint) else {
        warn!("momentum: cannot exit {} — missing decimals", pos.symbol);
        return Ok(None);
    };

    // Sell the actual on-chain balance (live) so a worse-than-expected entry fill
    // can't oversize the sell quote and revert. Dry-run uses the recorded amount.
    let sell_amount = if cfg.momentum_dry_run {
        pos.token_amount
    } else {
        let owner = scanner::load_keypair(&cfg.wallet_keypair_path)
            .context("could not load wallet keypair for exit")?
            .pubkey()
            .to_string();
        match scanner::fetch_token_balance(&cfg.rpc_url, &owner, &pos.mint).await {
            Ok(bal) if bal > 0.0 => bal,
            Ok(_) => {
                warn!("momentum: on-chain balance of {} is zero — clearing stale position", pos.symbol);
                state.position = None;
                state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
                momentum_state::save(state_path, &state)?;
                return Ok(None);
            }
            Err(e) => {
                warn!("momentum: balance fetch for {} failed ({e}); using recorded amount", pos.symbol);
                pos.token_amount
            }
        }
    };

    let token_raw = jupiter::to_raw_amount(sell_amount, token_decimals);
    let quote = match jupiter::quote(
        ctx.http,
        &cfg.momentum_jupiter_api_url,
        &pos.mint,
        USDC_MINT,
        token_raw,
        cfg.momentum_slippage_bps,
    )
    .await
    {
        Ok(q) => q,
        Err(e) => {
            warn!("momentum: EXIT /quote failed for {} — {e}; stop stays armed, retrying", pos.symbol);
            audit(cfg, ts, ActionKind::QuoteFailed { symbol: pos.symbol.clone(), reason: e.to_string() });
            return Ok(None); // retry next poll; stop stays armed
        }
    };
    let expected_usdc = jupiter::from_raw_amount(quote.out_amount.parse::<u64>().unwrap_or(0), USDC_DECIMALS);

    let sig = if cfg.momentum_dry_run {
        "dry-run".to_string()
    } else {
        let (s, confirmed) = submit_and_confirm(cfg, ctx.http, &quote).await?;
        if !confirmed {
            warn!("momentum: EXIT {} submitted but not confirmed in {}s (tx={s})", pos.symbol, CONFIRM_TIMEOUT.as_secs());
        }
        s.to_string()
    };

    let rec = build_trade_record(&pos, ts, price, expected_usdc, sig.clone());
    state.trades.push(rec.clone());
    state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
    state.position = None;
    momentum_state::save(state_path, &state)?;

    // PnL tracking: recompute cumulative realized performance from the full ledger
    // and write a sidecar (so it can never drift from the trades that produced it).
    let pnl = momentum_state::summarize(&state.trades);
    if let Ok(json) = serde_json::to_string_pretty(&pnl) {
        if let Err(e) = std::fs::write(&cfg.momentum_pnl_path, json) {
            warn!("momentum: PnL sidecar write failed: {e}");
        }
    }

    // Loss circuit breaker: once the cumulative realized P&L (sum of all closed
    // trades) hits the configured max loss, write the halt file so every future
    // tick short-circuits until the operator investigates and deletes it.
    if cfg.momentum_max_loss_usdc > 0.0 && pnl.realized_usdc <= -cfg.momentum_max_loss_usdc {
        let reason = format!(
            "cumulative realized P&L {:+.2} USDC hit the -{:.2} USDC loss limit over {} trades",
            pnl.realized_usdc, cfg.momentum_max_loss_usdc, pnl.closed_trades
        );
        error!(
            "momentum: LOSS HALT — {reason}. Trading stopped; delete {} to re-arm.",
            cfg.momentum_halt_path
        );
        if let Err(e) = momentum_state::write_halt(
            Path::new(&cfg.momentum_halt_path),
            &momentum_state::HaltRecord { ts, reason: reason.clone() },
        ) {
            warn!("momentum: failed to write halt file: {e}");
        }
        email_trade(cfg, "[Momentum] LOSS HALT — trading stopped", &reason).await;
    }

    audit(cfg, ts, ActionKind::Exited {
        symbol: pos.symbol.clone(),
        mint: pos.mint.clone(),
        usdc_out: expected_usdc,
        exit_price_usd: price,
        peak_price_usd: pos.peak_price_usd,
        pnl_pct: rec.pnl_pct,
        sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });
    let tag = if cfg.momentum_dry_run { "DRY-RUN EXIT" } else { "EXIT" };
    let label = token_label(ctx.watched, &pos.mint, &pos.symbol);
    info!(
        "momentum: {tag} {label} ({exit_reason}) — sold for {:.4} USDC @ ${:.6} (peak ${:.6}, trade {:+.2}%) | \
         realized {:+.4} USDC ({:+.2}%) over {} trade(s), {}W/{}L ({:.0}% win) tx={sig}",
        expected_usdc, price, pos.peak_price_usd, rec.pnl_pct,
        pnl.realized_usdc, pnl.realized_pct, pnl.closed_trades, pnl.wins, pnl.losses, pnl.win_rate_pct
    );
    // Emails are live-only (see email_trade), so the subject is always "EXIT".
    email_trade(
        cfg,
        &format!("[Momentum] EXIT {label} ({:+.2}%) — total {:+.2} USDC", rec.pnl_pct, pnl.realized_usdc),
        &format!(
            "Sold {} for {:.4} USDC @ ${:.6}  ({exit_reason})\nentry ${:.6}  peak ${:.6}  trade pnl {:+.2}%\ntx={sig}\n\n\
             ── Cumulative realized P&L ──\n\
             {:+.4} USDC ({:+.2}%) over {} trade(s)\n\
             {}W / {}L  ({:.0}% win)   best {:+.2}%   worst {:+.2}%",
            label, expected_usdc, price, pos.entry_price_usd, pos.peak_price_usd, rec.pnl_pct,
            pnl.realized_usdc, pnl.realized_pct, pnl.closed_trades, pnl.wins, pnl.losses,
            pnl.win_rate_pct, pnl.best_trade_pct, pnl.worst_trade_pct
        ),
    )
    .await;

    Ok(Some(TradeOutcome::Exited {
        symbol: pos.symbol,
        mint: pos.mint,
        usdc_out: expected_usdc,
        dry_run: cfg.momentum_dry_run,
    }))
}

// ───────────────────────── execution (lifted, adapted) ─────────────────────────

/// Sign + submit + confirm a Jupiter swap. Lifted from the removed rebalancer:
/// load keypair → `/swap` → base64 decode → bincode → sign slot 0 →
/// `send_transaction` → poll `get_signature_statuses` (800ms) up to 45s.
async fn submit_and_confirm(
    cfg: &PortfolioConfig,
    http: &Client,
    quote: &jupiter::QuoteResponse,
) -> Result<(Signature, bool)> {
    let keypair = scanner::load_keypair(&cfg.wallet_keypair_path)
        .context("could not load wallet keypair")?;
    let user_pubkey = keypair.pubkey().to_string();
    let swap_resp = jupiter::swap(http, &cfg.momentum_jupiter_api_url, quote, &user_pubkey)
        .await
        .context("jupiter /swap failed")?;

    let tx_b64 = swap_resp.swap_transaction.clone();
    let rpc_url_submit = cfg.rpc_url.clone();
    let sig: Signature = tokio::task::spawn_blocking(move || -> Result<Signature> {
        let raw = STANDARD.decode(tx_b64).context("base64 decode of swap tx failed")?;
        let mut tx: VersionedTransaction =
            bincode::deserialize(&raw).context("bincode decode of swap tx failed")?;
        tx = sign_versioned(tx, &keypair)?;
        let rpc = RpcClient::new_with_commitment(rpc_url_submit, CommitmentConfig::confirmed());
        rpc.send_transaction(&tx).context("send_transaction failed")
    })
    .await
    .context("swap submit join failed")??;

    let rpc_url_confirm = cfg.rpc_url.clone();
    let confirmed: bool = tokio::task::spawn_blocking(move || -> Result<bool> {
        let rpc = RpcClient::new_with_commitment(rpc_url_confirm, CommitmentConfig::confirmed());
        let started = Instant::now();
        while started.elapsed() < CONFIRM_TIMEOUT {
            let statuses = rpc.get_signature_statuses(&[sig]).ok();
            if let Some(st) = statuses.and_then(|r| r.value.into_iter().next()).flatten() {
                if st.err.is_some() {
                    anyhow::bail!("transaction reverted on chain: {:?}", st.err);
                }
                if st.confirmation_status.is_some() {
                    return Ok(true);
                }
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        Ok(false)
    })
    .await
    .context("confirm join failed")??;

    Ok((sig, confirmed))
}

/// Jupiter returns the tx with an empty fee-payer signature slot; sign the
/// message and overwrite slot 0.
fn sign_versioned(mut tx: VersionedTransaction, keypair: &Keypair) -> Result<VersionedTransaction> {
    let msg = tx.message.serialize();
    let sig = keypair.sign_message(&msg);
    if tx.signatures.is_empty() {
        tx.signatures.push(sig);
    } else {
        tx.signatures[0] = sig;
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(ts: u64, mint: &str, price: f64) -> PriceSnapshot {
        let mut prices = HashMap::new();
        prices.insert(mint.to_string(), price);
        PriceSnapshot { ts, prices }
    }

    #[test]
    fn trailing_stop_boundary() {
        // peak 100, trail 8% → stop at exactly 92.0
        assert!(trailing_stop_triggered(92.0, 100.0, 8.0), "at the boundary fires");
        assert!(!trailing_stop_triggered(92.01, 100.0, 8.0), "just above holds");
        assert!(trailing_stop_triggered(80.0, 100.0, 8.0), "well below fires");
        assert!(!trailing_stop_triggered(50.0, 0.0, 8.0), "no valid peak never fires");
    }

    #[test]
    fn is_stale_ts_detects_closed_market() {
        // Rises 100→110 over ts 0..=600 (10 pts/min), then frozen at 110 to ts 2400.
        let mut s: Vec<(u64, f64)> = Vec::new();
        for t in (0..=600).step_by(60) {
            s.push((t, 100.0 + (t as f64 / 600.0) * 10.0));
        }
        for t in (660..=2400).step_by(60) {
            s.push((t, 110.0));
        }
        // Last real move was ~ts 540–600; "now" is 2400 ⇒ ~30 min frozen.
        assert!(is_stale_ts(&s, 20), "30 min frozen ≥ 20 min ⇒ closed");
        assert!(!is_stale_ts(&s, 45), "30 min frozen < 45 min ⇒ not yet");
        assert!(!is_stale_ts(&s, 0), "0 disables");
        // A continuously-moving series is never stale.
        let moving: Vec<(u64, f64)> = (0..30).map(|i| (i * 60, 100.0 + i as f64)).collect();
        assert!(!is_stale_ts(&moving, 20));
        // Frozen-since-restart even with only 2 samples spanning the window.
        assert!(is_stale_ts(&[(0, 110.0), (1500, 110.0)], 20), "flat 25 min ⇒ closed");
    }

    #[test]
    fn gas_bps_scales_inversely_with_trade_size() {
        // 15_000 lamports @ $150/SOL = $0.00225; over a $1 trade = 22 bps.
        assert_eq!(est_gas_bps(1.0, 150.0), 22);
        // Over a $100 trade the same gas rounds to 0 bps.
        assert_eq!(est_gas_bps(100.0, 150.0), 0);
        assert_eq!(est_gas_bps(0.0, 150.0), 0);
        assert_eq!(est_gas_bps(100.0, 0.0), 0);
    }

    #[test]
    fn price_series_filters_nonpositive() {
        let mut h = VecDeque::new();
        h.push_back(snap(1, "M", 10.0));
        h.push_back(snap(2, "M", 0.0)); // dropped
        h.push_back(snap(3, "M", 12.0));
        assert_eq!(price_series_for_mint(&h, "M"), vec![10.0, 12.0]);
        assert!(price_series_for_mint(&h, "OTHER").is_empty());
    }

    #[test]
    fn rank_picks_highest_sortino() {
        // A: steadily rising (positive returns, ~zero downside → high Sortino).
        // B: steadily falling (negative drift, downside → low/negative Sortino).
        let mut h = VecDeque::new();
        let mut a = 100.0;
        let mut b = 100.0;
        for i in 0..200u64 {
            a *= 1.001; // +0.1%/step
            b *= 0.999; // -0.1%/step
            let mut prices = HashMap::new();
            prices.insert("A".to_string(), a);
            prices.insert("B".to_string(), b);
            h.push_back(PriceSnapshot { ts: i, prices });
        }
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "A".into(), name: None, equity: None },
            WatchedToken { symbol: "BBB".into(), mint: "B".into(), name: None, equity: None },
        ];
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), a);
        prices.insert("B".to_string(), b);
        let ranked = rank_candidates(&watched, &prices, &h, 1440, 0);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].mint, "A", "rising token ranks first");
        assert!(ranked[0].sortino > ranked[1].sortino);
    }

    #[test]
    fn rank_skips_warmup_tokens() {
        // Only 50 snapshots → < 120 returns → no Sortino → excluded.
        let mut h = VecDeque::new();
        for i in 0..50u64 {
            h.push_back(snap(i, "A", 100.0 + i as f64));
        }
        let watched = vec![WatchedToken { symbol: "AAA".into(), mint: "A".into(), name: None, equity: None }];
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 150.0);
        assert!(rank_candidates(&watched, &prices, &h, 1440, 0).is_empty());
    }

    #[test]
    fn trade_record_pnl() {
        let pos = Position {
            mint: "M".into(), symbol: "S".into(), entry_ts: 1,
            entry_price_usd: 1.0, token_amount: 50.0, usdc_spent: 50.0,
            peak_price_usd: 1.2, entry_sig: "e".into(), dry_run: true,
        };
        let rec = build_trade_record(&pos, 2, 1.1, 55.0, "x".into());
        assert!((rec.pnl_pct - 10.0).abs() < 1e-9);
        assert_eq!(rec.exit_sig, "x");
        assert_eq!(rec.entry_sig, "e");
    }
}
