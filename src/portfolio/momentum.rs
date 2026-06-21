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
use super::suggestions::{compute_metrics, Metrics, RankMetric, SORTINO_MIN_OBS};
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
    /// Rotated directly from one held token into another (A→B swap, no USDC leg).
    Rotated { from_mint: String, to_mint: String, to_symbol: String, to_amount: f64, dry_run: bool },
}

impl TradeOutcome {
    pub fn dry_run(&self) -> bool {
        match self {
            TradeOutcome::Entered { dry_run, .. }
            | TradeOutcome::Exited { dry_run, .. }
            | TradeOutcome::Rotated { dry_run, .. } => *dry_run,
        }
    }
}

/// A ranked entry candidate.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub symbol: String,
    pub mint: String,
    /// Value of the *selected* `RankMetric` — what ranking + the gates compare.
    pub score: f64,
    /// All four metrics, for the side-by-side visibility log.
    pub metrics: Metrics,
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

/// Estimated network cost of one momentum swap in USD (two base fees + a priority
/// buffer). Subtracted from realized P&L on every swap so the loss breaker sees the
/// true net: the Jupiter quote already nets price impact + swap fee, but gas is paid in
/// SOL *outside* the swap, so it has to be charged explicitly. Modeled in dry-run too,
/// so paper P&L predicts live P&L.
pub fn est_gas_usdc(sol_price_usd: f64) -> f64 {
    if sol_price_usd <= 0.0 {
        return 0.0;
    }
    let gas_lamports = BASE_FEE_LAMPORTS * 2 + 5_000;
    gas_lamports as f64 / 1_000_000_000.0 * sol_price_usd
}

/// Gas cost (two base fees + a buffer) expressed in bps of the trade notional.
pub fn est_gas_bps(trade_usdc: f64, sol_price_usd: f64) -> u32 {
    if trade_usdc <= 0.0 || sol_price_usd <= 0.0 {
        return 0;
    }
    (est_gas_usdc(sol_price_usd) / trade_usdc * 10_000.0) as u32
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

/// Rank watched tokens by the chosen `metric` over the lookback window. Only tokens
/// with computable metrics (≥120 returns) AND a positive current price appear, sorted
/// best-first by the selected metric's `score`. Each carries all four `metrics` (for
/// the side-by-side log) and a `stale` flag (price frozen over `stale_window` minutes
/// → market closed); the entry path skips stale ones.
pub fn rank_candidates(
    watched: &[WatchedToken],
    prices: &HashMap<String, f64>,
    history: &VecDeque<PriceSnapshot>,
    lookback: usize,
    stale_window: usize,
    metric: RankMetric,
) -> Vec<Candidate> {
    let mut cands: Vec<Candidate> = Vec::new();
    for w in watched {
        // Source the (ts, price) series so `slope_r2` has its time axis; same `p>0`
        // filter + oldest-first ordering as the price-only path.
        let series_ts = price_series_with_ts(history, &w.mint);
        let window: &[(u64, f64)] = if series_ts.len() > lookback {
            &series_ts[series_ts.len() - lookback..]
        } else {
            &series_ts
        };
        let Some(price) = prices.get(&w.mint).copied().filter(|p| *p > 0.0) else {
            continue;
        };
        if let Some(metrics) = compute_metrics(window) {
            cands.push(Candidate {
                symbol: w.symbol.clone(),
                mint: w.mint.clone(),
                score: metrics.select(metric),
                metrics,
                price_usd: price,
                obs: window.len().saturating_sub(1), // returns count (= old rets.len())
                // Closed-market guard applies only to equities (xStocks/ETFs);
                // 24/7 crypto is never flagged stale, even when low-volatility.
                stale: w.is_equity() && is_stale_ts(&series_ts, stale_window),
            });
        }
    }
    cands.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    cands
}

/// Pick the token to rotate the held position into, or `None`. `ranked` is
/// best-score-first, so this returns the strongest eligible B: not the held token,
/// not stale (market closed), not in re-entry cooldown, clears `min_score`, and beats
/// the held token's score by at least `rotate_margin` (which must exceed the swap
/// cost). Scores are in the active metric's units. `rotate_margin == 0` disables.
#[allow(clippy::too_many_arguments)]
pub fn rotation_target(
    ranked: &[Candidate],
    held_mint: &str,
    held_score: f64,
    min_score: f64,
    rotate_margin: f64,
    reentry_cooldown_secs: i64,
    now: i64,
    cooldowns: &HashMap<String, i64>,
) -> Option<Candidate> {
    if rotate_margin <= 0.0 {
        return None; // rotation disabled
    }
    ranked
        .iter()
        .find(|c| {
            c.mint != held_mint
                && !c.stale
                && c.score > min_score
                && c.score - held_score >= rotate_margin
                && cooldowns
                    .get(&c.mint)
                    .is_none_or(|&last| now - last >= reentry_cooldown_secs)
        })
        .cloned()
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

/// Per-tick visibility: log every watched token's metrics, one per line, best-first by
/// the active metric, so the operator can A/B which separates trend from noise. Each
/// token shows `so`=sortino `sh`=sharpe `sl`=slope_r2 `rt`=return, with `*` on the
/// active metric. Frozen markets show `closed`; tokens still warming show `warming`.
fn log_rank_line(cfg: &PortfolioConfig, watched: &[WatchedToken], ranked: &[Candidate], metric: RankMetric) {
    let mark = |m: RankMetric, tag: &str| if m == metric { format!("*{tag}") } else { tag.to_string() };
    let scored: std::collections::HashSet<&str> = ranked.iter().map(|c| c.mint.as_str()).collect();
    // Symbols padded to a fixed width so the metric columns line up across rows.
    let mut parts: Vec<String> = ranked
        .iter()
        .map(|c| {
            if c.stale {
                return format!("  {:<9} closed", c.symbol);
            }
            let m = &c.metrics;
            format!(
                "  {:<9} {}={:.2} {}={:.2} {}={:.2} {}={:+.4}",
                c.symbol,
                mark(RankMetric::Sortino, "so"), m.sortino,
                mark(RankMetric::Sharpe, "sh"), m.sharpe,
                mark(RankMetric::SlopeR2, "sl"), m.slope_r2,
                mark(RankMetric::Return, "rt"), m.ret,
            )
        })
        .collect();
    for w in watched {
        if !scored.contains(w.mint.as_str()) {
            parts.push(format!("  {:<9} warming", w.symbol));
        }
    }
    info!(
        "momentum: rank[{metric}] (min {:.2}) —\n{}",
        cfg.momentum_min_score,
        parts.join("\n")
    );
}

/// After a close leg has been pushed to `state.trades`: recompute the realized-PnL
/// summary, write the sidecar, and trip the loss circuit-breaker if cumulative
/// realized P&L has hit the configured limit. Returns the summary. Shared by the
/// trailing-stop exit and rotation (both close a leg).
async fn finalize_pnl_and_halt(
    cfg: &PortfolioConfig,
    state: &momentum_state::TraderState,
    ts: i64,
) -> momentum_state::PnlSummary {
    let pnl = momentum_state::summarize(&state.trades);
    if let Ok(json) = serde_json::to_string_pretty(&pnl) {
        if let Err(e) = std::fs::write(&cfg.momentum_pnl_path, json) {
            warn!("momentum: PnL sidecar write failed: {e}");
        }
    }
    if cfg.momentum_max_loss_usdc > 0.0 && pnl.realized_usdc <= -cfg.momentum_max_loss_usdc {
        let reason = format!(
            "cumulative realized P&L {:+.2} USDC hit the -{:.2} USDC loss limit over {} trades",
            pnl.realized_usdc, cfg.momentum_max_loss_usdc, pnl.closed_trades
        );
        error!(
            "momentum: LOSS HALT — {reason}. New entries/rotations stopped; delete {} to re-arm.",
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
    pnl
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
    // Mode mismatch: the persisted position belongs to the OTHER mode and can't be
    // managed here — paper mode would never sell the real tokens a live position holds,
    // and live mode would try to sell paper tokens that were never bought. Ignore it:
    // reset to FLAT (persisted, since every tick reloads state from disk) so the bot
    // starts clean in the current mode rather than stranding the position and erroring
    // on every tick. The real holding (if any) is left untouched in the wallet.
    if pos.dry_run != cfg.momentum_dry_run {
        warn!(
            "momentum: ignoring persisted {} position {} (entry ${:.6}) — opened with dry_run={} but \
             DRY_RUN_MOMENTUM_TRADER={}; resetting to FLAT for this mode",
            if pos.dry_run { "PAPER" } else { "LIVE" },
            pos.symbol, pos.entry_price_usd, pos.dry_run, cfg.momentum_dry_run
        );
        state.position = None;
        if let Err(e) = momentum_state::save(path, &state) {
            warn!("momentum: failed to persist FLAT reset after mode mismatch: {e}");
        }
        return;
    }
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

/// Mid-run reconciliation, called after a wallet re-scan detects a change. A **live**
/// position must stay backed by an on-chain balance; if the wallet no longer holds the
/// token (sold or moved externally), the recorded position is stale → invalidate it
/// (clear to FLAT + bench the mint) so the bot doesn't manage a phantom. **Paper**
/// positions are simulated and wallet-independent, so they're never invalidated by a
/// wallet change. Quiet (no-op) unless it actually clears something; returns `true` if
/// it did. Mode mismatch is handled once at startup, so a paper position here matches
/// the current (paper) mode and is correctly left alone.
pub fn invalidate_unbacked_position(cfg: &PortfolioConfig, portfolio: &Portfolio) -> bool {
    if !cfg.enable_momentum_trader {
        return false;
    }
    let path = Path::new(&cfg.momentum_state_path);
    let mut state = match momentum_state::load(path) {
        Ok(s) => s,
        Err(e) => {
            warn!("momentum: could not load state for re-scan reconcile: {e}");
            return false;
        }
    };
    let Some(pos) = state.position.clone() else {
        return false; // FLAT — nothing to invalidate
    };
    if pos.dry_run {
        return false; // paper position — simulated, not backed by (or affected by) the wallet
    }
    let held = portfolio
        .tokens
        .iter()
        .find(|t| t.mint == pos.mint)
        .map(|t| t.amount)
        .unwrap_or(0.0);
    if held > 0.0 {
        return false; // still wallet-backed — valid
    }
    warn!(
        "momentum: wallet no longer holds {} (sold/moved externally) — invalidating stale position → FLAT",
        pos.symbol
    );
    state.position = None;
    state.last_exit_ts_per_mint.insert(pos.mint.clone(), now_ts());
    if let Err(e) = momentum_state::save(path, &state) {
        warn!("momentum: failed to persist invalidated state: {e}");
    }
    true
}

// ───────────────────────────── ENTRY (FLAT, 60s) ─────────────────────────────

pub async fn maybe_enter(ctx: &MomentumContext<'_>) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
    if !cfg.enable_momentum_trader || halted(cfg) {
        return Ok(None);
    }
    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    let ts = now_ts();

    // Rank all watched tokens and log the per-tick metric panel (FLAT or HOLDING).
    let ranked = rank_candidates(
        ctx.watched,
        ctx.prices_usd,
        ctx.history,
        cfg.momentum_lookback_obs,
        cfg.momentum_stale_minutes,
        cfg.momentum_rank_metric,
    );
    log_rank_line(cfg, ctx.watched, &ranked, cfg.momentum_rank_metric);

    // HOLDING — the trailing-stop / market-close exit runs on the fast loop. Here
    // (the 60s tick) we log unrealized PnL and consider rotating into a stronger token.
    if let Some(pos) = state.position.clone() {
        if let Some(px) = ctx.prices_usd.get(&pos.mint).copied().filter(|p| *p > 0.0) {
            let unreal = (px - pos.entry_price_usd) / pos.entry_price_usd * 100.0;
            info!(
                "momentum: HOLDING {} — entry ${:.6} now ${:.6} peak ${:.6} unrealized {:+.2}%",
                pos.symbol, pos.entry_price_usd, px, pos.peak_price_usd, unreal
            );
        }
        return try_rotate(ctx, &mut state, state_path, pos, &ranked, ts).await;
    }

    // FLAT — consider opening a new position.
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

    if best.score <= cfg.momentum_min_score {
        info!(
            "momentum: best candidate {} {}={:.2} ≤ MIN {:.2} — staying FLAT",
            best.symbol, cfg.momentum_rank_metric, best.score, cfg.momentum_min_score
        );
        audit(cfg, ts, ActionKind::SkipBelowThreshold {
            best_symbol: best.symbol,
            best_sortino: best.score,
            min_sortino: cfg.momentum_min_score,
            metric: cfg.momentum_rank_metric.to_string(),
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

    // P&L cost basis includes the entry swap's gas, so realized P&L nets it at the
    // eventual close (the basis is subtracted exactly once → can't cancel like a
    // mid-chain charge would). The PORTFOLIO USDC delta (TradeOutcome below) stays at
    // the real notional — gas is paid in SOL, not USDC.
    let entry_basis = cfg.momentum_trade_usdc + est_gas_usdc(sol_price);
    state.position = Some(Position {
        mint: best.mint.clone(),
        symbol: best.symbol.clone(),
        entry_ts: ts,
        entry_price_usd: best.price_usd,
        token_amount: expected_token,
        usdc_spent: entry_basis,
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
    info!("momentum: {tag} {label} — {:.6} tokens for {} USDC @ ${:.6} ({}={:.2}, cost={total_cost_bps}bps) tx={sig}",
        expected_token, cfg.momentum_trade_usdc, best.price_usd, cfg.momentum_rank_metric, best.score);
    // Emails are live-only (see email_trade), so the subject is always "ENTER".
    email_trade(cfg, &format!("[Momentum] ENTER {label}"),
        &format!("Bought {:.6} {} for {} USDC @ ${:.6}\n{}={:.2}  cost={total_cost_bps}bps\ntx={sig}",
            expected_token, label, cfg.momentum_trade_usdc, best.price_usd, cfg.momentum_rank_metric, best.score)).await;

    Ok(Some(TradeOutcome::Entered {
        symbol: best.symbol,
        mint: best.mint,
        token_amount: expected_token,
        usdc_spent: cfg.momentum_trade_usdc,
        dry_run: cfg.momentum_dry_run,
    }))
}

// ─────────────────────────── ROTATION (HOLDING, 60s) ───────────────────────────

/// While holding A, rotate directly into a stronger token B (one atomic A→B swap)
/// when B clears the margin and all entry gates. Runs on the 60s monitor tick
/// (Sortino is slow-moving); the fast-loop trailing-stop / market-close exit is
/// unaffected. P&L is netted of the swap cost via the received-B value.
async fn try_rotate(
    ctx: &MomentumContext<'_>,
    state: &mut momentum_state::TraderState,
    state_path: &Path,
    pos: Position,
    ranked: &[Candidate],
    ts: i64,
) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
    if cfg.momentum_rotate_margin <= 0.0 {
        return Ok(None); // rotation disabled
    }
    // Mode-mismatch guard (same as exit): never act on a position opened in the other mode.
    if pos.dry_run != cfg.momentum_dry_run {
        audit(cfg, ts, ActionKind::ModeMismatch {
            position_dry_run: pos.dry_run,
            config_dry_run: cfg.momentum_dry_run,
        });
        return Ok(None);
    }
    // A rotation opens a new position → it counts against the daily cap.
    if momentum_state::entries_last_24h(state, ts) >= cfg.momentum_max_trades_per_day as usize {
        return Ok(None);
    }
    // The held token must be rankable (priced, warm, open) to compare; if it's
    // closed/stale the fast exit flattens it — don't rotate.
    let held_score = match ranked.iter().find(|c| c.mint == pos.mint) {
        Some(c) if !c.stale => c.score,
        _ => return Ok(None),
    };
    let Some(target) = rotation_target(
        ranked,
        &pos.mint,
        held_score,
        cfg.momentum_min_score,
        cfg.momentum_rotate_margin,
        cfg.momentum_reentry_cooldown_secs,
        ts,
        &state.last_exit_ts_per_mint,
    ) else {
        return Ok(None); // nothing beats the held token by the margin
    };

    let Some(&from_decimals) = ctx.decimals.get(&pos.mint) else {
        warn!("momentum: cannot rotate {} — missing decimals", pos.symbol);
        return Ok(None);
    };
    let Some(&to_decimals) = ctx.decimals.get(&target.mint) else {
        audit(cfg, ts, ActionKind::QuoteFailed { symbol: target.symbol, reason: "missing decimals".into() });
        return Ok(None);
    };

    // Sell amount of the held token: actual on-chain balance (live) or recorded (dry-run).
    let sell_amount = if cfg.momentum_dry_run {
        pos.token_amount
    } else {
        let owner = scanner::load_keypair(&cfg.wallet_keypair_path)
            .context("could not load wallet keypair for rotation")?
            .pubkey()
            .to_string();
        match scanner::fetch_token_balance(&cfg.rpc_url, &owner, &pos.mint).await {
            Ok(bal) if bal > 0.0 => bal,
            Ok(_) => {
                warn!("momentum: on-chain balance of {} is zero — clearing stale position", pos.symbol);
                state.position = None;
                state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
                momentum_state::save(state_path, state)?;
                return Ok(None);
            }
            Err(e) => {
                warn!("momentum: balance fetch for {} failed ({e}); using recorded amount", pos.symbol);
                pos.token_amount
            }
        }
    };

    // Quote the direct A→B swap.
    let token_raw = jupiter::to_raw_amount(sell_amount, from_decimals);
    let quote = match jupiter::quote(
        ctx.http,
        &cfg.momentum_jupiter_api_url,
        &pos.mint,
        &target.mint,
        token_raw,
        cfg.momentum_slippage_bps,
    )
    .await
    {
        Ok(q) => q,
        Err(e) => {
            warn!("momentum: rotate /quote {}→{} failed — {e}", pos.symbol, target.symbol);
            audit(cfg, ts, ActionKind::QuoteFailed { symbol: target.symbol, reason: e.to_string() });
            return Ok(None);
        }
    };

    // Cost gate — the margin should already clear cost; this is the hard backstop.
    let a_price = ranked.iter().find(|c| c.mint == pos.mint).map(|c| c.price_usd).unwrap_or(pos.entry_price_usd);
    let notional = sell_amount * a_price;
    let slip_bps = jupiter::price_impact_bps(&quote);
    let sol_price = ctx.prices_usd.get(SOL_KEY).copied().unwrap_or(0.0);
    let gas_bps = est_gas_bps(notional, sol_price);
    let total_cost_bps = slip_bps + gas_bps;
    if total_cost_bps > cfg.momentum_max_cost_bps {
        audit(cfg, ts, ActionKind::SkipCostGate {
            symbol: target.symbol,
            total_cost_bps,
            gas_bps,
            slip_bps,
            budget_bps: cfg.momentum_max_cost_bps,
        });
        return Ok(None);
    }

    let expected_b = jupiter::from_raw_amount(quote.out_amount.parse::<u64>().unwrap_or(0), to_decimals);
    if expected_b <= 0.0 {
        audit(cfg, ts, ActionKind::QuoteFailed { symbol: target.symbol, reason: "zero out amount".into() });
        return Ok(None);
    }
    // Post-slippage USDC value of the B actually received — the quote already nets the
    // A→B price impact + swap fee. This is B's carry-forward cost basis.
    let b_value = expected_b * target.price_usd;
    // A-leg realized P&L = that value minus this swap's network gas, charging the
    // rotation's gas to the closing (A) leg. B's BASIS stays at the gross `b_value`:
    // subtracting gas from the basis too would cancel it out across the telescoping
    // chain (B's lower basis would exactly offset A's lower proceeds), so the gas must
    // hit only the realized side.
    let gas_usdc = est_gas_usdc(sol_price);
    let realized = (b_value - gas_usdc).max(0.0);

    let sig = if cfg.momentum_dry_run {
        "dry-run".to_string()
    } else {
        let (s, confirmed) = submit_and_confirm(cfg, ctx.http, &quote).await?;
        if !confirmed {
            warn!("momentum: ROTATE {}→{} submitted but not confirmed in {}s (tx={s})",
                pos.symbol, target.symbol, CONFIRM_TIMEOUT.as_secs());
        }
        s.to_string()
    };

    // Record the A leg (closed, net of swap cost), then open B with the carry-forward basis.
    let rec = build_trade_record(&pos, ts, a_price, realized, sig.clone());
    state.trades.push(rec.clone());
    state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
    state.position = Some(Position {
        mint: target.mint.clone(),
        symbol: target.symbol.clone(),
        entry_ts: ts,
        entry_price_usd: target.price_usd,
        token_amount: expected_b,
        usdc_spent: b_value,
        peak_price_usd: target.price_usd,
        entry_sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });
    momentum_state::save(state_path, state)?;

    let pnl = finalize_pnl_and_halt(cfg, state, ts).await;

    audit(cfg, ts, ActionKind::Rotated {
        from_symbol: pos.symbol.clone(),
        from_mint: pos.mint.clone(),
        from_sortino: held_score,
        to_symbol: target.symbol.clone(),
        to_mint: target.mint.clone(),
        to_sortino: target.score,
        to_amount: expected_b,
        realized_usdc: realized,
        cost_bps: total_cost_bps,
        sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
        metric: cfg.momentum_rank_metric.to_string(),
    });
    let tag = if cfg.momentum_dry_run { "DRY-RUN ROTATE" } else { "ROTATE" };
    let from_label = token_label(ctx.watched, &pos.mint, &pos.symbol);
    let to_label = token_label(ctx.watched, &target.mint, &target.symbol);
    let metric = cfg.momentum_rank_metric;
    info!(
        "momentum: {tag} {from_label} ({metric} {:.2}) → {to_label} ({metric} {:.2}) — {:.6} {} for ~{:.4} USDC (A-leg pnl {:+.2}%, cost {total_cost_bps}bps) | realized {:+.4} USDC over {} trade(s) {}W/{}L tx={sig}",
        held_score, target.score, expected_b, target.symbol, realized, rec.pnl_pct,
        pnl.realized_usdc, pnl.closed_trades, pnl.wins, pnl.losses
    );
    email_trade(
        cfg,
        &format!("[Momentum] ROTATE {} → {} (A-leg {:+.2}%)", pos.symbol, target.symbol, rec.pnl_pct),
        &format!(
            "Rotated {from_label} → {to_label}\nsold {:.6} {} ({metric} {:.2}) → bought {:.6} {} ({metric} {:.2})\nA-leg pnl {:+.2}%  cost {total_cost_bps}bps  tx={sig}\n\n\
             ── Cumulative realized P&L ──\n\
             {:+.4} USDC ({:+.2}%) over {} trade(s)\n\
             {}W / {}L  ({:.0}% win)   best {:+.2}%   worst {:+.2}%",
            sell_amount, pos.symbol, held_score, expected_b, target.symbol, target.score, rec.pnl_pct,
            pnl.realized_usdc, pnl.realized_pct, pnl.closed_trades, pnl.wins, pnl.losses,
            pnl.win_rate_pct, pnl.best_trade_pct, pnl.worst_trade_pct
        ),
    )
    .await;

    Ok(Some(TradeOutcome::Rotated {
        from_mint: pos.mint,
        to_mint: target.mint,
        to_symbol: target.symbol,
        to_amount: expected_b,
        dry_run: cfg.momentum_dry_run,
    }))
}

// ─────────────────────────── EXIT (HOLDING, fast) ───────────────────────────

pub async fn maybe_exit(ctx: &MomentumContext<'_>) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
    // Deliberately NOT gated on halted(): a halted bot must still be able to EXIT
    // its open position (the loss breaker / manual halt blocks only new entries and
    // rotations, in maybe_enter) — otherwise a position would be stranded.
    if !cfg.enable_momentum_trader {
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
    // The quote's `out_amount` already nets price impact + swap fee; gas is paid in SOL
    // outside the swap, so subtract it here to make realized P&L net of ALL costs.
    let sol_price = ctx.prices_usd.get(SOL_KEY).copied().unwrap_or(0.0);
    let gas_usdc = est_gas_usdc(sol_price);
    let net_usdc = (expected_usdc - gas_usdc).max(0.0);

    let sig = if cfg.momentum_dry_run {
        "dry-run".to_string()
    } else {
        let (s, confirmed) = submit_and_confirm(cfg, ctx.http, &quote).await?;
        if !confirmed {
            warn!("momentum: EXIT {} submitted but not confirmed in {}s (tx={s})", pos.symbol, CONFIRM_TIMEOUT.as_secs());
        }
        s.to_string()
    };

    let rec = build_trade_record(&pos, ts, price, net_usdc, sig.clone());
    state.trades.push(rec.clone());
    state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
    state.position = None;
    momentum_state::save(state_path, &state)?;

    // Recompute the realized-PnL summary, write the sidecar, and trip the loss
    // circuit-breaker if the cumulative realized P&L hit the limit (shared helper).
    let pnl = finalize_pnl_and_halt(cfg, &state, ts).await;

    audit(cfg, ts, ActionKind::Exited {
        symbol: pos.symbol.clone(),
        mint: pos.mint.clone(),
        usdc_out: net_usdc,
        exit_price_usd: price,
        peak_price_usd: pos.peak_price_usd,
        pnl_pct: rec.pnl_pct,
        sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });
    let tag = if cfg.momentum_dry_run { "DRY-RUN EXIT" } else { "EXIT" };
    let label = token_label(ctx.watched, &pos.mint, &pos.symbol);
    info!(
        "momentum: {tag} {label} ({exit_reason}) — sold for {:.4} USDC (net of ~{:.4} gas) @ ${:.6} (peak ${:.6}, trade {:+.2}%) | \
         realized {:+.4} USDC ({:+.2}%) over {} trade(s), {}W/{}L ({:.0}% win) tx={sig}",
        net_usdc, gas_usdc, price, pos.peak_price_usd, rec.pnl_pct,
        pnl.realized_usdc, pnl.realized_pct, pnl.closed_trades, pnl.wins, pnl.losses, pnl.win_rate_pct
    );
    // Emails are live-only (see email_trade), so the subject is always "EXIT".
    email_trade(
        cfg,
        &format!("[Momentum] EXIT {label} ({:+.2}%) — total {:+.2} USDC", rec.pnl_pct, pnl.realized_usdc),
        &format!(
            "Sold {} for {:.4} USDC (net of ~{:.4} gas) @ ${:.6}  ({exit_reason})\nentry ${:.6}  peak ${:.6}  trade pnl {:+.2}%\ntx={sig}\n\n\
             ── Cumulative realized P&L ──\n\
             {:+.4} USDC ({:+.2}%) over {} trade(s)\n\
             {}W / {}L  ({:.0}% win)   best {:+.2}%   worst {:+.2}%",
            label, net_usdc, gas_usdc, price, pos.entry_price_usd, pos.peak_price_usd, rec.pnl_pct,
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
    fn est_gas_usdc_and_bps_agree() {
        // 15_000 lamports (2 base fees + 5_000 buffer) × $200 SOL / 1e9 = $0.003.
        let g = est_gas_usdc(200.0);
        assert!((g - 0.003).abs() < 1e-9, "gas usd was {g}");
        // No SOL price ⇒ no estimate (don't fabricate a cost).
        assert_eq!(est_gas_usdc(0.0), 0.0);
        // bps is just the USD cost over the notional; on a $100 trade, $0.003 = 0.3 bps → 0.
        assert_eq!(est_gas_bps(100.0, 200.0), (0.003 / 100.0 * 10_000.0) as u32);
        // The charge is real on a small trade: $0.003 on a $5 notional = 6 bps.
        assert_eq!(est_gas_bps(5.0, 200.0), 6);
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
        let ranked = rank_candidates(&watched, &prices, &h, 1440, 0, RankMetric::Sortino);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].mint, "A", "rising token ranks first");
        assert!(ranked[0].score > ranked[1].score);
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
        assert!(rank_candidates(&watched, &prices, &h, 1440, 0, RankMetric::Sortino).is_empty());
    }

    #[test]
    fn rotation_target_respects_margin_and_gates() {
        let cand = |sym: &str, score: f64, stale: bool| Candidate {
            symbol: sym.into(),
            mint: sym.into(),
            score,
            // rotation_target reads only `score`; the panel metrics are irrelevant here.
            metrics: Metrics { sortino: score, sharpe: 0.0, slope_r2: 0.0, ret: 0.0 },
            price_usd: 1.0,
            obs: 200,
            stale,
        };
        // best-first: B=1.0, held A=0.5, C=0.3
        let ranked = vec![cand("B", 1.0, false), cand("A", 0.5, false), cand("C", 0.3, false)];
        let no_cd = HashMap::new();
        let pick = |min, margin, cd: &HashMap<String, i64>| {
            rotation_target(&ranked, "A", 0.5, min, margin, 3600, 1000, cd).map(|c| c.mint)
        };
        assert_eq!(pick(0.0, 0.3, &no_cd), Some("B".into()), "B beats A by 0.5 ≥ 0.3");
        assert_eq!(pick(0.0, 0.6, &no_cd), None, "0.5 edge < 0.6 margin");
        assert_eq!(pick(0.0, 0.0, &no_cd), None, "margin 0 disables rotation");
        assert_eq!(pick(1.5, 0.3, &no_cd), None, "B=1.0 below MIN_SORTINO 1.5");
        // B benched by cooldown (exited at 900, now 1000, cooldown 3600) → no target
        let mut cd = HashMap::new();
        cd.insert("B".to_string(), 900);
        assert_eq!(pick(0.0, 0.3, &cd), None, "B in cooldown, C too weak");
        // stale B excluded
        let stale_ranked = vec![cand("B", 1.0, true), cand("A", 0.5, false)];
        assert!(rotation_target(&stale_ranked, "A", 0.5, 0.0, 0.3, 3600, 1000, &no_cd).is_none());
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
