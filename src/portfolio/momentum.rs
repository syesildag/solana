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
use super::momentum_actions::{self, Action, ActionKind, TokenRank, TokenState};
use super::momentum_state::{self, Position, TradeRecord};
use super::momentum_universe::{WatchedToken, USDC_DECIMALS, USDC_MINT};
use super::suggestions::{compute_metrics, compute_slope_r2, Metrics, RankMetric, SORTINO_MIN_OBS};
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
    /// Live on-chain price feed for event-driven exits (Some only when MOMENTUM_GRPC_EXIT).
    pub grpc_feed: Option<&'a crate::portfolio::grpc_pricer::GrpcFeed>,
    /// In-memory wick-confirm arm state: mint -> when the stop breach began.
    pub stop_armed: Option<&'a dashmap::DashMap<String, std::time::Instant>>,
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

/// Outcome of one wick-confirmed stop evaluation for a held position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ExitDecision { Sell, Arm, StayArmed, Disarm, Hold }

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
    /// Lookback window already up > `MOMENTUM_MAX_RUN_PCT` and decelerating → momentum
    /// likely spent; the entry AND rotation-target paths skip these as buy targets (it
    /// still ranks and still serves as the held-token score reference).
    pub overextended: bool,
    /// Price is actively falling over the recent window (recent slope < 0) → never buy
    /// into a drop, regardless of run size. Independent of `overextended` (which only
    /// fires above the run cap). Skipped by the entry and rotation-target paths.
    pub falling: bool,
    /// The ranking metric itself is *descending* vs `MOMENTUM_CONFIRM_LAG_OBS`
    /// observations ago — its trend quality is rolling over even if price still
    /// ticks up. Confirmation guard: skipped by the entry and rotation-target paths
    /// so we never buy a fading signal. Still ranks (serves as held-token reference).
    pub metric_fading: bool,
    /// Recent-window ln-price slope (`recent_slope`) and whole-window ln-price slope
    /// (`ln_price_slope`) — the two inputs `is_overextended` consumes. Stored so a
    /// consumer can re-evaluate over-extension with a different `max_run_pct` without
    /// rebuilding the price window. `None` when the window was too short to fit a slope.
    pub slope_recent: Option<f64>,
    pub slope_full: Option<f64>,
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

/// Which volatility measure (if any) scales the trailing stop. `Off` ⇒ the fixed-%
/// stop (`trail_pct`). `Atr`/`Sigma` are active only when `chandelier_k > 0`; both
/// fall back to the fixed-% stop while their window is still warming up.
///
/// Defined here (the production module) so the live trader and the backtest decide
/// the stop with one shared definition; `sim` and the config import it from here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VolStopMode {
    /// Fixed-% trailing stop: `price ≤ peak·(1 − trail_pct/100)`.
    #[default]
    Off,
    /// Chandelier (price-units): `price ≤ peak − k·ATR(vol_obs)`.
    Atr,
    /// Return-σ (percent): `eff% = k·σ·100`, then the fixed-% predicate at `eff%`.
    Sigma,
}

impl VolStopMode {
    /// Lower-case wire form for CLI args, env vars, and CSV columns.
    pub fn as_str(self) -> &'static str {
        match self {
            VolStopMode::Off => "off",
            VolStopMode::Atr => "atr",
            VolStopMode::Sigma => "sigma",
        }
    }

    /// Parse the wire form (case-insensitive). `None` for an unknown token.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "" | "none" | "fixed" => Some(VolStopMode::Off),
            "atr" | "chandelier" => Some(VolStopMode::Atr),
            "sigma" | "stddev" | "vol" => Some(VolStopMode::Sigma),
            _ => None,
        }
    }
}

/// Trailing-stop predicate generalised over the volatility mode. The caller passes
/// the pre-computed `atr`/`sigma` for the mint's `vol_obs` window (either may be
/// `None` while warming up); this keeps the function pure and identical between the
/// live trader and the backtest. Warmup or `k == 0` ⇒ the fixed-% stop at `trail_pct`.
///
/// - `Off`   → `price ≤ peak·(1 − trail_pct/100)`
/// - `Atr`   → `price ≤ peak − k·ATR`
/// - `Sigma` → fixed-% predicate at `eff% = k·σ·100`
#[allow(clippy::too_many_arguments)]
pub fn vol_stop_triggered(
    price: f64,
    peak: f64,
    trail_pct: f64,
    mode: VolStopMode,
    k: f64,
    atr: Option<f64>,
    sigma: Option<f64>,
) -> bool {
    if peak <= 0.0 {
        return false;
    }
    match mode {
        VolStopMode::Atr if k > 0.0 => match atr {
            Some(atr) => price <= peak - k * atr,
            None => trailing_stop_triggered(price, peak, trail_pct),
        },
        VolStopMode::Sigma if k > 0.0 => match sigma {
            Some(sigma) => trailing_stop_triggered(price, peak, k * sigma * 100.0),
            None => trailing_stop_triggered(price, peak, trail_pct),
        },
        _ => trailing_stop_triggered(price, peak, trail_pct),
    }
}

/// Profit-protected ("max-trail") exit predicate. Once a position is *green* — its
/// peak has cleared the cost-adjusted breakeven `entry·(1 + round_trip_cost_frac)` —
/// it is allowed to give back gains down to `max(floor, peak·(1 − max_trail_pct/100))`,
/// so a winner can breathe (ride a pullback) yet never closes red. While not yet green,
/// or when disabled, the existing stop (`fallback_stop_hit`) governs the stop-loss.
///
/// Pure and shared by the backtest and the live trader so they cannot drift.
/// `max_trail_pct <= 0` ⇒ disabled: returns `fallback_stop_hit` unchanged (today's
/// behavior). A large `max_trail_pct` ⇒ "ride all the way to the cost-breakeven floor".
pub fn profit_protected_stop_triggered(
    price: f64,
    peak: f64,
    entry: f64,
    round_trip_cost_frac: f64,
    max_trail_pct: f64,
    fallback_stop_hit: bool,
) -> bool {
    if max_trail_pct <= 0.0 || peak <= 0.0 || entry <= 0.0 {
        return fallback_stop_hit;
    }
    let floor = entry * (1.0 + round_trip_cost_frac);
    if peak <= floor {
        // Not yet green (never cleared cost-breakeven) → normal stop-loss governs.
        return fallback_stop_hit;
    }
    let give_back = peak * (1.0 - max_trail_pct / 100.0);
    price <= floor.max(give_back)
}

/// Equity-compounding per-entry trade size: grow the notional with *banked* profit.
/// `size = clamp(base + reinvest_frac·max(0, realized_pnl), base, ceiling)`. Only
/// realized profit compounds, floored at `base`; `reinvest_frac <= 0` ⇒ `base`
/// (today's fixed size). `ceiling` below `base` is treated as `base` (fail-safe: a
/// misconfigured cap can never shrink the trade below base).
///
/// Pure and shared by the backtest and the live trader so they can't drift. The
/// caller is responsible for clamping the result to the available wallet balance.
pub fn dynamic_trade_usdc(base: f64, reinvest_frac: f64, ceiling: f64, realized_pnl: f64) -> f64 {
    if reinvest_frac <= 0.0 {
        return base;
    }
    let grown = base + reinvest_frac * realized_pnl.max(0.0);
    grown.clamp(base, ceiling.max(base))
}

/// z-score of a token's price over its last `dip_obs` observations — the
/// mean-reversion entry confirmation. `None` below ~30 obs or on a flat series.
/// Negative ⇒ oversold (a pullback). Mirrors `sim::token_dip_z` so live matches the
/// backtest.
pub fn entry_dip_z(history: &VecDeque<PriceSnapshot>, mint: &str, dip_obs: usize) -> Option<f64> {
    let series = price_series_for_mint(history, mint);
    let lo = series.len().saturating_sub(dip_obs);
    let w = &series[lo..];
    if w.len() < 30 {
        return None;
    }
    let n = w.len() as f64;
    let m = w.iter().sum::<f64>() / n;
    let sd = (w.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n).sqrt();
    if sd < 1e-12 {
        return None;
    }
    Some((w.last().unwrap() - m) / sd)
}

/// Overbought entry gate (mean-reversion filter): `true` ⇒ BLOCK a new entry because
/// the token is extended above its own mean — its z-score over the last `obs`
/// observations exceeds `max_z`. `obs == 0` disables (never blocks). Mirrors the
/// backtest's `entry_max_z_obs`/`entry_max_z` so live matches the simulator. A warming
/// series (`entry_dip_z` → `None`) never blocks.
pub fn entry_overbought(history: &VecDeque<PriceSnapshot>, mint: &str, obs: usize, max_z: f64) -> bool {
    obs > 0 && entry_dip_z(history, mint, obs).is_some_and(|z| z > max_z)
}

/// A scheduled macro release (CPI/PPI/FOMC decision) from the
/// `MOMENTUM_MACRO_CALENDAR_PATH` JSON — `ts` is the release moment in epoch seconds
/// (extra fields like a human-readable `utc` string are ignored).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct MacroEvent {
    pub name: String,
    pub ts: i64,
}

/// Load the macro calendar once per process. A missing or unparseable file logs a
/// warning and yields an empty list — the blackout gate then never fires (fail-open:
/// a bad calendar must not halt trading, it just loses the protection). A calendar
/// whose last event is near (or in the past) also warns: a stale file silently
/// degrades to no protection, so the operator is told to refresh it.
pub fn macro_calendar(path: &str) -> &'static [MacroEvent] {
    static CAL: std::sync::OnceLock<Vec<MacroEvent>> = std::sync::OnceLock::new();
    CAL.get_or_init(|| match std::fs::read_to_string(path) {
        Ok(s) => match serde_json::from_str::<Vec<MacroEvent>>(&s) {
            Ok(mut v) => {
                v.sort_by_key(|e| e.ts);
                if let (Some(last), Ok(now)) = (v.last(), SystemTime::now().duration_since(UNIX_EPOCH)) {
                    let days_left = (last.ts - now.as_secs() as i64) / 86_400;
                    if days_left < 45 {
                        warn!(
                            "macro calendar {path} ends in {days_left}d ({}) — refresh with: node scripts/fetch_macro_calendar.js",
                            last.name
                        );
                    }
                }
                v
            }
            Err(e) => {
                warn!("macro calendar {path} unparseable ({e}) — blackout gate inert");
                Vec::new()
            }
        },
        Err(e) => {
            warn!("macro calendar {path} unreadable ({e}) — blackout gate inert");
            Vec::new()
        }
    })
}

/// Macro-calendar blackout (pure): `Some(event)` when `now` falls within
/// `before_hours` BEFORE or `after_hours` AFTER a scheduled release. The before-window
/// covers the print itself dumping the market out of a fresh entry (2026-05-12 CPI:
/// −8% SOL); the after-window covers the digestion period — the entry signal re-fires
/// as soon as a naive pre-only gate lifts and walks into the continuing slide (the
/// May 2026 dump ran 4 days past the print). Entries only; exits are never gated.
/// Both windows `<= 0` ⇒ disabled.
pub fn macro_blackout<'a>(
    events: &'a [MacroEvent],
    now: i64,
    before_hours: f64,
    after_hours: f64,
) -> Option<&'a MacroEvent> {
    if before_hours <= 0.0 && after_hours <= 0.0 {
        return None;
    }
    let before = (before_hours.max(0.0) * 3600.0) as i64;
    let after = (after_hours.max(0.0) * 3600.0) as i64;
    events.iter().find(|e| e.ts - before <= now && now <= e.ts + after)
}

/// Market-regime gate (pure): is SOL "risk-on" — its latest price above the mean of
/// the prior up-to-`ma_obs` SOL observations? Used to keep the momentum trader in
/// cash while the broad market is risk-off. Mirrors the backtest's
/// `sim::regime_mask` final-point semantics so live behavior matches the simulator.
/// `ma_obs == 0`, or fewer than 2 prior observations, ⇒ `true` (never block).
pub fn sol_risk_on(history: &VecDeque<PriceSnapshot>, ma_obs: usize) -> bool {
    // No values to compare (gate disabled or warming up) ⇒ never block.
    sol_regime_values(history, ma_obs).map_or(true, |(current, mean)| current > mean)
}

/// Diagnostic companion to [`sol_risk_on`]: the `(current, mean)` SOL prices the gate
/// compares — latest observation vs the mean of the prior up-to-`ma_obs` window.
/// `None` when the gate can't fire (`ma_obs == 0`, no SOL prices, or < 2 prior obs),
/// which both callers treat as risk-on. Lets the caller log the evidence behind the
/// decision without recomputing the window.
pub fn sol_regime_values(history: &VecDeque<PriceSnapshot>, ma_obs: usize) -> Option<(f64, f64)> {
    if ma_obs == 0 {
        return None;
    }
    let sols: Vec<f64> = history
        .iter()
        .filter_map(|s| s.prices.get(SOL_KEY).copied())
        .filter(|p| *p > 0.0)
        .collect();
    let (current, prior) = sols.split_last()?;
    let window = &prior[prior.len().saturating_sub(ma_obs)..];
    if window.len() < 2 {
        return None; // warming up — don't gate
    }
    let mean = window.iter().sum::<f64>() / window.len() as f64;
    Some((*current, mean))
}

/// Which market-regime gate the entry uses. `level` = SOL above its MA (the original
/// gate); `trend` = SOL in a clean uptrend by slope_r2 (regime *momentum* — backtests
/// favor it: fewer trades, higher per-trade P&L); `off` = no gate. Env
/// `MOMENTUM_REGIME_MODE`. Default `level` for backward compatibility with existing
/// `MOMENTUM_REGIME_OBS` configs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegimeMode {
    Off,
    #[default]
    Level,
    Trend,
}

impl std::str::FromStr for RegimeMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "" => Ok(RegimeMode::Off),
            "level" | "ma" => Ok(RegimeMode::Level),
            "trend" | "slope" | "slope_r2" => Ok(RegimeMode::Trend),
            other => Err(format!("unknown MOMENTUM_REGIME_MODE '{other}' (want off|level|trend)")),
        }
    }
}

impl std::fmt::Display for RegimeMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RegimeMode::Off => "off",
            RegimeMode::Level => "level",
            RegimeMode::Trend => "trend",
        })
    }
}

/// Trend-strength regime (pure): SOL's `slope_r2` over the last up-to-`obs` SOL
/// observations, paired with the `min_slope_r2` it's compared against. `Some((slope_r2,
/// min))` once warm; `None` when `obs == 0` or the window is too short to compute
/// (warming ⇒ never block). Mirrors `sim::regime_mask_trend` final-point semantics so
/// live behavior matches the backtest. Because `compute_slope_r2` is slope×R² (signed,
/// cleanliness-weighted), a positive `min` demands a *clean uptrend*, not just price
/// drifting above an average.
pub fn sol_regime_trend(
    history: &VecDeque<PriceSnapshot>,
    obs: usize,
    min_slope_r2: f64,
) -> Option<(f64, f64)> {
    if obs == 0 {
        return None;
    }
    let sols: Vec<(u64, f64)> = history
        .iter()
        .filter_map(|s| s.prices.get(SOL_KEY).copied().filter(|p| *p > 0.0).map(|p| (s.ts, p)))
        .collect();
    let window = &sols[sols.len().saturating_sub(obs)..];
    let sr2 = compute_slope_r2(window)?; // None below the slope_r2 obs floor ⇒ warming
    Some((sr2, min_slope_r2))
}

/// Entry regime gate dispatched on `mode` (pure). Returns `(risk_on, diagnostic)`:
/// `diagnostic` is `Some` only when the gate actually decided, so the caller logs the
/// evidence on active ticks and stays quiet while off/warming. `risk_on` is `true`
/// whenever the gate can't fire (off / warming) — the gate never blocks on no signal.
pub fn regime_risk_on(
    history: &VecDeque<PriceSnapshot>,
    mode: RegimeMode,
    obs: usize,
    trend_min: f64,
) -> (bool, Option<String>) {
    match mode {
        RegimeMode::Off => (true, None),
        RegimeMode::Level => match sol_regime_values(history, obs) {
            Some((cur, mean)) => {
                (cur > mean, Some(format!("level: SOL ${cur:.4} vs {obs}-obs MA ${mean:.4}")))
            }
            None => (true, None),
        },
        RegimeMode::Trend => match sol_regime_trend(history, obs, trend_min) {
            Some((sr2, min)) => {
                (sr2 >= min, Some(format!("trend: SOL slope_r2 {sr2:.1} vs min {min:.1} over {obs} obs")))
            }
            None => (true, None),
        },
    }
}

/// Take-profit-on-fade predicate (pure): momentum has faded (active-metric score ≤
/// `min_score`) AND the position is green (`price > entry_price`). Both must hold to
/// flatten a held winner whose trend died before the trailing stop tripped; an
/// underwater position is left to the trailing stop.
pub fn fade_take_profit(held_score: f64, min_score: f64, price: f64, entry_price: f64) -> bool {
    held_score <= min_score && price > entry_price
}

/// Rotation "green" predicate (pure): true only when the held position is profitable
/// enough to cover the rotation swap's cost — `price` must exceed `entry_price` by
/// more than `cost_bps` (slippage + gas). I.e. still green AFTER paying to rotate, so
/// the A leg never books at or below its basis.
pub fn rotation_net_green(price: f64, entry_price: f64, cost_bps: u32) -> bool {
    price > entry_price * (1.0 + cost_bps as f64 / 10_000.0)
}

/// Divergence (bps) between a Jupiter quote's implied fill price and a reference
/// (live gRPC) price: `|implied − reference| / reference × 10_000`. `None` if either
/// input isn't finite and positive — a degenerate price can't be compared honestly.
/// Used by the entry/rotation price-freshness guard: the rank/quote signal was
/// computed moments ago, and this catches a fill price that has since moved away
/// from what's live on-chain.
pub fn quote_divergence_bps(implied_price: f64, reference_price: f64) -> Option<u32> {
    if !implied_price.is_finite() || implied_price <= 0.0
        || !reference_price.is_finite() || reference_price <= 0.0
    {
        return None;
    }
    Some(((implied_price - reference_price).abs() / reference_price * 10_000.0) as u32)
}

/// Trusted live gRPC price for `mint`, for the entry/rotation divergence guard.
/// Mirrors `grpc_pricer::select_prices`'s single-mint case: present in the feed,
/// positive, not currently distrusted by the REST cross-check, and — unless
/// `stale_secs` is `0` (trust-until-changed: an AMM price cannot move without an
/// account write, so age alone never demotes it) — updated within that window.
/// `None` (feed absent, mint unpriced, or any check fails) means "nothing to compare
/// against", so callers must skip the guard rather than block the trade on missing data.
fn trusted_grpc_price(
    feed: Option<&crate::portfolio::grpc_pricer::GrpcFeed>,
    mint: &str,
    stale_secs: u64,
) -> Option<f64> {
    let feed = feed?;
    let (price, updated_at) = feed.map.get(mint).map(|e| *e.value())?;
    let stale = Duration::from_secs(stale_secs);
    let fresh = stale.is_zero() || Instant::now().duration_since(updated_at) <= stale;
    if price > 0.0 && fresh && !feed.distrusted_snapshot().contains(mint) {
        Some(price)
    } else {
        None
    }
}

/// Raw OLS slope of ln(price) vs elapsed seconds over `window` (oldest-first) — the
/// ln-price-per-second trend. `None` if < 2 points, any non-positive price, or a
/// degenerate time axis. Unlike `compute_slope_r2` there's no R² scaling and no
/// min-obs floor: it's a cheap *direction* probe used to tell an accelerating trend
/// from a decelerating (topping) one, and must work on a short recent sub-window.
fn ln_price_slope(window: &[(u64, f64)]) -> Option<f64> {
    if window.len() < 2 {
        return None;
    }
    let t0 = window.first()?.0;
    let n = window.len() as f64;
    let (mut xs, mut ys) = (Vec::with_capacity(window.len()), Vec::with_capacity(window.len()));
    for &(t, p) in window {
        if p <= 0.0 {
            return None;
        }
        xs.push(t.saturating_sub(t0) as f64);
        ys.push(p.ln());
    }
    let (mx, my) = (xs.iter().sum::<f64>() / n, ys.iter().sum::<f64>() / n);
    let (mut sxx, mut sxy) = (0.0_f64, 0.0_f64);
    for (x, y) in xs.iter().zip(ys.iter()) {
        let dx = x - mx;
        sxx += dx * dx;
        sxy += dx * (y - my);
    }
    if sxx <= 1e-12 {
        return None;
    }
    Some(sxy / sxx)
}

/// Slope over just the last `decel_min` minutes of `window` — the "is it still
/// accelerating?" probe. `None` when the check is disabled (`decel_min == 0`) or
/// there are too few recent points, which makes the over-extension guard fall back to
/// a pure run cap.
fn recent_slope(window: &[(u64, f64)], decel_min: usize) -> Option<f64> {
    if decel_min == 0 {
        return None;
    }
    let latest = window.last()?.0;
    let cutoff = latest.saturating_sub(decel_min as u64 * 60);
    let recent: Vec<(u64, f64)> = window.iter().copied().filter(|&(t, _)| t >= cutoff).collect();
    ln_price_slope(&recent)
}

/// Over-extension guard (pure): block *buying* a token whose lookback window has run
/// more than `max_run_pct` percent — BUT only when the trend is also decelerating, so
/// a still-accelerating runner (e.g. a startup mid-breakout) isn't vetoed. `window_ret`
/// is the `Return` metric (Σ log-returns), so the run is `e^ret − 1`. `slope_recent` /
/// `slope_full` are raw ln-price slopes over the recent sub-window and the whole
/// window; `recent < full` ⇒ decelerating. When slope info is absent (deceleration
/// check disabled, or too little data) it falls back to a pure run cap.
/// `max_run_pct <= 0` disables entirely.
pub fn is_overextended(
    window_ret: f64,
    max_run_pct: f64,
    slope_recent: Option<f64>,
    slope_full: Option<f64>,
) -> bool {
    if max_run_pct <= 0.0 || (window_ret.exp() - 1.0) * 100.0 <= max_run_pct {
        return false; // disabled, or not a big enough run to worry about
    }
    match (slope_recent, slope_full) {
        // Recent trend flatter than the overall trend → exhausted/topping → skip.
        // Recent still steeper → accelerating → let it run.
        (Some(recent), Some(full)) => recent < full,
        // No trend info (check off or too little data) → conservative pure run cap.
        _ => true,
    }
}

/// Whether the active ranking metric is *descending* versus `lag` observations
/// ago — the entry "confirmation" guard. Compares the metric over the current
/// `lookback` window against the same-length window ending `lag` obs earlier
/// (`score_now` is the already-computed current value, passed in to avoid a second
/// regression). A rising or flat metric is fine; a falling one means we'd be
/// buying a fading signal — the JUP case, where `slope_r2` slid 7508→5774 while
/// price still ticked up. `lag == 0` disables the guard. Too little history to
/// form the lagged window counts as fading: a confirmation guard only admits a
/// trend it can positively see is not rolling over.
fn metric_is_fading(
    series_ts: &[(u64, f64)],
    lookback: usize,
    lag: usize,
    metric: RankMetric,
    score_now: f64,
) -> bool {
    if lag == 0 {
        return false; // guard disabled
    }
    let n = series_ts.len();
    if n < lookback + lag {
        return true; // can't form the lagged window → unconfirmed → treat as fading
    }
    let prev_window = &series_ts[n - lookback - lag..n - lag];
    match compute_metrics(prev_window).map(|m| m.select(metric)) {
        Some(prev) => score_now < prev, // strictly weaker than `lag` obs ago → fading
        None => true,                    // degenerate lagged window → unconfirmed
    }
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

/// Factor by which a swap's slippage tolerance widens on each consecutive
/// revert. ×2 per attempt widens gently — paired with a tight
/// `MOMENTUM_ENTRY_SLIPPAGE_CAP_BPS` it gives ladders like 10→20 (base 10, cap 20)
/// without over-paying on the first (tight) try; exits take one extra retry to
/// reach their wide cap vs the old ×3 (e.g. 50→100→200→400→800). Shared by both
/// exit escalation (unconditional, wide cap) and entry escalation (optional,
/// tight cap).
const SLIPPAGE_ESCALATION_FACTOR: u32 = 2;

/// Slippage tolerance (bps) for retry `attempt` (0-indexed). A revert (typically
/// Jupiter `0x1771` SlippageToleranceExceeded on a high-volatility / fast-moving
/// token) widens the next attempt's min-out cushion. Geometric ×2 escalation off
/// `base_bps`, capped at `cap_bps`; `attempt == 0` returns `base_bps` unchanged
/// so the first try stays tight. Saturating, so large attempt counts never
/// overflow. Exits cap wide (must get out); entries cap tight (chasing a fill is
/// optional — don't buy a blowoff top).
fn escalated_slippage_bps(base_bps: u32, attempt: u32, cap_bps: u32) -> u32 {
    let mut bps = base_bps;
    for _ in 0..attempt {
        bps = bps.saturating_mul(SLIPPAGE_ESCALATION_FACTOR);
        if bps >= cap_bps {
            return cap_bps;
        }
    }
    bps.min(cap_bps)
}

/// The attempt index to size the next entry into `best_mint`, given the
/// persisted entry-attempt record. Carrying a chase only makes sense for the
/// *same* candidate: if the best token has changed since the last failure, the
/// escalation resets to 0 so we never inherit a wide tolerance meant for a
/// different token.
fn entry_attempt_for(prior: &Option<momentum_state::EntryAttempt>, best_mint: &str) -> u32 {
    match prior {
        Some(ea) if ea.mint == best_mint => ea.count,
        _ => 0,
    }
}

/// Whether the watcher's fast tick should re-attempt a reverted entry now:
/// requires the feature on (`retry_secs > 0`), a pending revert record with a
/// stamped deadline (`next_retry_ts > 0` — records stamped `0` predate the
/// feature or were written with it off, and wait for the slow tick as before),
/// and the deadline reached. Between slow ticks the ranking inputs are static,
/// so the re-attempt deterministically chases the same candidate.
fn entry_retry_due(
    rec: &Option<momentum_state::EntryAttempt>,
    retry_secs: u64,
    now: i64,
) -> bool {
    if retry_secs == 0 {
        return false;
    }
    matches!(rec, Some(ea) if ea.count > 0 && ea.next_retry_ts > 0 && now >= ea.next_retry_ts)
}

/// Hard ceiling on staged-entry tranches: each live tranche is a full quote +
/// submit + confirm (up to 45s) awaited inline on the watcher's single task, so
/// exit checks stall for the whole ladder. 10 bounds that worst case while
/// covering any sane TWAP split.
const MAX_ENTRY_STEPS: u32 = 10;

/// Effective tranche count for a staged (TWAP) entry. `None` (env unset), `0`
/// and `1` all mean the original single-swap entry; larger values are clamped
/// to `MAX_ENTRY_STEPS`.
fn effective_entry_steps(cfg_steps: Option<u32>) -> u32 {
    match cfg_steps {
        None | Some(0) | Some(1) => 1,
        Some(n) => n.min(MAX_ENTRY_STEPS),
    }
}

/// Split a raw USDC notional into per-tranche amounts for a staged entry.
/// The first N−1 tranches get `total_raw / steps`; the last absorbs the
/// remainder, so the sum is always exactly `total_raw`. Degenerate inputs
/// (steps ≤ 1, or a notional too small to give every tranche ≥1 raw unit)
/// collapse to a single full-size tranche — never zero-amount tranches.
fn entry_step_amounts(total_raw: u64, steps: u32) -> Vec<u64> {
    if steps <= 1 || total_raw < steps as u64 {
        return vec![total_raw];
    }
    let steps = steps as u64;
    let base = total_raw / steps;
    let mut amounts = vec![base; steps as usize];
    *amounts.last_mut().unwrap() = total_raw - base * (steps - 1);
    amounts
}

/// Fast-tick entry retry: when a reverted entry's retry deadline has passed,
/// re-run the normal entry path (full gates, fresh Jupiter quote at the
/// escalated tolerance). No-op — without touching the state file's mtime or
/// doing any pricing work — unless a retry is actually due.
pub async fn maybe_retry_entry(ctx: &MomentumContext<'_>) -> Result<Vec<TradeOutcome>> {
    let cfg = ctx.cfg;
    if cfg.momentum_entry_retry_secs == 0 {
        return Ok(Vec::new());
    }
    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    let now = now_ts();
    if !entry_retry_due(&state.entry_attempt, cfg.momentum_entry_retry_secs, now) {
        return Ok(Vec::new());
    }
    // Push the deadline forward BEFORE attempting: if the attempt changes no
    // state (an entry gate fails — no revert, no fill), the next re-check waits
    // a full retry window instead of re-ranking on every fast tick until the
    // slow tick. A revert inside maybe_enter re-stamps its own fresh deadline.
    if let Some(ea) = state.entry_attempt.as_mut() {
        ea.next_retry_ts = now + cfg.momentum_entry_retry_secs as i64;
    }
    momentum_state::save(state_path, &state)?;
    maybe_enter(ctx).await
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
/// the side-by-side log), a `stale` flag (price frozen over `stale_window` minutes
/// → market closed), and an `overextended` flag (window already up > `max_run_pct`);
/// the entry/rotation-target paths skip stale and over-extended candidates as buys.
#[allow(clippy::too_many_arguments)]
pub fn rank_candidates(
    watched: &[WatchedToken],
    prices: &HashMap<String, f64>,
    history: &VecDeque<PriceSnapshot>,
    lookback: usize,
    stale_window: usize,
    metric: RankMetric,
    max_run_pct: f64,
    decel_lookback_min: usize,
    confirm_lag_obs: usize,
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
            // Slopes for the trend-shape guards (compute once). `slope_recent` is the
            // last-N-min ln-price slope; `ln_price_slope(window)` the whole-window slope.
            let slope_recent = recent_slope(window, decel_lookback_min);
            let slope_full = ln_price_slope(window);
            // Over-extension: only vetoes a *decelerating* run above the cap — a
            // still-accelerating breakout is left alone. Read `ret` before the move below.
            let overextended = is_overextended(metrics.ret, max_run_pct, slope_recent, slope_full);
            // Falling: actively dropping right now (recent slope < 0) → never buy into
            // it, regardless of run size. Catches a decelerating mid-run entry (e.g. BP
            // at +12.7%, under the cap, but already rolling over) the run cap misses.
            let falling = slope_recent.is_some_and(|s| s < 0.0);
            // Confirmation: is the ranking metric itself rolling over vs `lag` obs ago?
            // Uses the full (un-truncated) series so the lagged window can reach back
            // past the current lookback slice.
            let metric_fading =
                metric_is_fading(&series_ts, lookback, confirm_lag_obs, metric, metrics.select(metric));
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
                overextended,
                falling,
                metric_fading,
                slope_recent,
                slope_full,
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
/// not stale (market closed), not over-extended (window already up > MAX_RUN_PCT —
/// the worst loss, MET#2, was a rotation INTO a token at the top of a +9.7% run), not
/// falling (recent slope < 0 — never rotate into a dropping token), not in re-entry
/// cooldown, clears `min_score`, and beats the held token's score by at least
/// `rotate_margin` (which must exceed the swap cost). Scores are in the active metric's
/// units. `rotate_margin == 0` disables.
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
                && !c.overextended
                && !c.falling
                && !c.metric_fading
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
    // Both live and paper fills notify (ENTER / EXIT / ROTATE). Paper fills are
    // labeled [PAPER] so a dry-run fill is never mistaken for a real one. (Price
    // alerts are a separate path, unaffected by DRY_RUN_MOMENTUM_TRADER.)
    let subject = if cfg.momentum_dry_run {
        format!("[PAPER] {subject}")
    } else {
        subject.to_string()
    };
    if let Err(e) = emailer::send_alert(cfg, &subject, body).await {
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

/// Convert the ranked candidates + the watched universe into the per-token records
/// persisted in an [`ActionKind::RankSnapshot`]. This is the JSONL twin of
/// `log_rank_line` and must reproduce the same panel content, in the same order:
///
///   - `ranked` is already best-first by the active metric. Each candidate becomes a
///     [`TokenRank`]: a `stale` candidate → [`TokenState::Closed`]; otherwise
///     [`TokenState::Scored`] carrying all four metrics (`metrics.sortino`,
///     `metrics.sharpe`, `metrics.slope_r2`, `metrics.ret`).
///   - Every watched token NOT present in `ranked` is still warming up (no metrics
///     yet) → [`TokenState::Warming`], appended after the scored rows — exactly how
///     `log_rank_line` lists them last.
///
/// Mirror the membership test `log_rank_line` uses (it builds a `HashSet` of ranked
/// mints and pushes any watched mint missing from it).
fn snapshot_tokens(watched: &[WatchedToken], ranked: &[Candidate]) -> Vec<TokenRank> {
    let scored: std::collections::HashSet<&str> = ranked.iter().map(|c| c.mint.as_str()).collect();
    // Ranked rows first (best-first; a stale candidate is "closed", else "scored").
    let mut out: Vec<TokenRank> = ranked
        .iter()
        .map(|c| TokenRank {
            symbol: c.symbol.clone(),
            state: if c.stale {
                TokenState::Closed
            } else {
                TokenState::Scored {
                    sortino: c.metrics.sortino,
                    sharpe: c.metrics.sharpe,
                    slope_r2: c.metrics.slope_r2,
                    ret: c.metrics.ret,
                }
            },
        })
        .collect();
    // Then any watched token not yet rankable → warming, appended last.
    for w in watched {
        if !scored.contains(w.mint.as_str()) {
            out.push(TokenRank { symbol: w.symbol.clone(), state: TokenState::Warming });
        }
    }
    out
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
    // Loss circuit breaker — LIVE only. Paper losses aren't real, so the breaker must
    // not halt a dry-run observation run (the PnL sidecar above still records them).
    if !cfg.momentum_dry_run
        && cfg.momentum_max_loss_usdc > 0.0
        && pnl.realized_usdc <= -cfg.momentum_max_loss_usdc
    {
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

/// At startup, ground all recorded positions in reality. For each **live** position,
/// the wallet must actually hold that mint — if not, the record is stale (sold
/// manually, never filled, or the wallet changed) → remove it so the bot doesn't
/// manage a phantom. **Paper** (dry-run) positions are simulated, not wallet-backed,
/// so they are kept as-is. Any position whose `dry_run` flag mismatches the current
/// `DRY_RUN_MOMENTUM_TRADER` setting is cleared (mode mismatch — the bot cannot
/// manage a position from the other mode). Dedup by mint (keeps the first of any
/// duplicate). Caps the live position list at `max_positions`; excess are dropped.
/// Call once before the loop.
///
/// At `MOMENTUM_MAX_POSITIONS=1` (default) this behaves identically to the original
/// single-slot reconciliation.
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
    if state.positions.is_empty() {
        return; // FLAT — nothing to reconcile
    }

    let mut changed = false;

    // --- Step 1: mode-mismatch purge ---
    // If ANY position was opened in the other mode we can't safely manage any of
    // them in the current mode — clear all and start fresh (same semantics as the
    // single-slot version, extended to cover heterogeneous mode vectors).
    let mode_mismatch = state.positions.iter().any(|p| p.dry_run != cfg.momentum_dry_run);
    if mode_mismatch {
        let mismatched: Vec<_> = state.positions
            .iter()
            .filter(|p| p.dry_run != cfg.momentum_dry_run)
            .map(|p| format!("{} (dry_run={})", p.symbol, p.dry_run))
            .collect();
        warn!(
            "momentum: {} persisted position(s) opened with dry_run≠{} ({}); resetting all to FLAT — \
             DRY_RUN_MOMENTUM_TRADER={}",
            mismatched.len(),
            cfg.momentum_dry_run,
            mismatched.join(", "),
            cfg.momentum_dry_run,
        );
        state.positions.clear();
        if let Err(e) = momentum_state::save(path, &state) {
            warn!("momentum: failed to persist FLAT reset after mode mismatch: {e}");
        }
        return;
    }

    // --- Step 2: paper positions are always valid (not wallet-backed) ---
    if cfg.momentum_dry_run {
        for pos in &state.positions {
            info!(
                "momentum: resuming PAPER position {} (entry ${:.6}, peak ${:.6}) — simulated, not wallet-backed",
                pos.symbol, pos.entry_price_usd, pos.peak_price_usd
            );
        }
        return;
    }

    // --- Step 3: live positions — verify wallet backing; remove unbacked ones ---
    let mut stale_mints: Vec<String> = Vec::new();
    for pos in &state.positions {
        let held = portfolio
            .tokens
            .iter()
            .find(|t| t.mint == pos.mint)
            .map(|t| t.amount)
            .unwrap_or(0.0);
        if held <= 0.0 {
            warn!(
                "momentum: state says HOLDING {} but the wallet holds none — removing stale position",
                pos.symbol
            );
            stale_mints.push(pos.mint.clone());
            changed = true;
        } else {
            info!(
                "momentum: resuming LIVE position {} — wallet holds {:.6} (entry ${:.6}, peak ${:.6})",
                pos.symbol, held, pos.entry_price_usd, pos.peak_price_usd
            );
        }
    }
    for mint in &stale_mints {
        state.positions.retain(|p| &p.mint != mint);
        state.last_exit_ts_per_mint.insert(mint.clone(), now_ts());
    }

    // --- Step 4: dedup by mint (keep first occurrence) ---
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let before = state.positions.len();
    state.positions.retain(|p| seen.insert(p.mint.clone()));
    if state.positions.len() < before {
        warn!("momentum: removed {} duplicate position(s) by mint at startup", before - state.positions.len());
        changed = true;
    }

    // --- Step 5: cap at max_positions ---
    if state.positions.len() > cfg.momentum_max_positions {
        let excess = state.positions.len() - cfg.momentum_max_positions;
        warn!(
            "momentum: {} positions exceed MAX_POSITIONS={}; dropping {} oldest",
            state.positions.len(), cfg.momentum_max_positions, excess
        );
        state.positions.truncate(cfg.momentum_max_positions);
        changed = true;
    }

    if changed {
        if let Err(e) = momentum_state::save(path, &state) {
            warn!("momentum: failed to persist reconciled state: {e}");
        }
    }
}

/// One adoptable wallet holding: a watched token the wallet holds with a live price.
#[derive(Debug, Clone, PartialEq)]
pub struct AdoptCandidate {
    pub mint: String,
    pub symbol: String,
    pub amount: f64,
    pub price_usd: f64,
}

/// Outcome of scanning the wallet for an adoptable position.
#[derive(Debug, PartialEq)]
pub enum Adoption {
    /// No watched holding worth ≥ `min_usd`.
    None,
    /// Exactly one — safe to adopt (single-slot mode or only one qualifies).
    One(AdoptCandidate),
    /// Multiple qualify and `cap == 1` → can't tell which the operator meant; refuse and warn.
    Ambiguous(usize),
    /// Multiple qualify and `cap > 1` → adopt up to `cap`, sorted by value descending.
    Many(Vec<AdoptCandidate>),
}

/// Pure adoption selection: from the candidate holdings (each already filtered to a
/// watched mint with a positive live price), keep those worth ≥ `min_usd` and decide
/// based on available capacity.
///
/// - `cap == 1` (single-slot / default): adopt if exactly one qualifies; refuse and
///   warn if two or more qualify (original behavior — backward-compatible).
/// - `cap > 1` (multi-slot): adopt up to `cap` qualifiers, sorted by USD value
///   descending (highest-value holdings first). No ambiguity error.
///
/// Kept I/O-free so the decision is unit-tested.
fn choose_adoption(cands: Vec<AdoptCandidate>, min_usd: f64, cap: usize) -> Adoption {
    let mut big: Vec<AdoptCandidate> =
        cands.into_iter().filter(|c| c.amount * c.price_usd >= min_usd).collect();
    // Sort by USD value descending so the highest-value holdings are adopted first
    // when capped below the number of qualifiers.
    big.sort_by(|a, b| {
        let va = a.amount * a.price_usd;
        let vb = b.amount * b.price_usd;
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    match (big.len(), cap) {
        (0, _) => Adoption::None,
        (1, _) => Adoption::One(big.pop().unwrap()),
        (n, 1) => Adoption::Ambiguous(n), // single-slot: warn and refuse
        (_, c) => {
            big.truncate(c);
            Adoption::Many(big)
        }
    }
}

/// Adopt manually-acquired wallet holdings into the trader so it manages each position
/// (trailing stop / fade exit). Fires only when: the feature is enabled, live mode, and
/// there is free capacity (`max_positions - held` slots available).
///
/// Adopts up to `cap` (= `max_positions − currently_held`) watched tokens worth ≥ half
/// the trade size, deduped by mint, sorted by USD value descending. Entry/peak are set
/// to the **current** price (real cost basis unknown), so PnL and the trailing stop are
/// measured from adoption.
///
/// At `MOMENTUM_MAX_POSITIONS=1` (default, cap≤1): adopts only if exactly one wallet
/// holding qualifies; refuses with a warning if two or more qualify (original behavior).
/// At `MOMENTUM_MAX_POSITIONS>1`: adopts up to `cap` qualifiers without ambiguity.
///
/// Returns `true` if at least one position was adopted. `prices` is keyed by mint.
/// Called at startup AND every slow tick (state is disk-backed and reloaded by every
/// momentum call, so a mid-run adoption is picked up without a restart); it is a cheap
/// no-op whenever the gates aren't met (occupied slots, no qualifying holding, etc.).
pub fn adopt_wallet_position(
    cfg: &PortfolioConfig,
    portfolio: &Portfolio,
    prices: &HashMap<String, f64>,
    watched: &[WatchedToken],
) -> bool {
    if !cfg.enable_momentum_trader || !cfg.momentum_adopt_wallet_position || cfg.momentum_dry_run {
        return false; // disabled, or paper mode (nothing wallet-backed to adopt)
    }
    let path = Path::new(&cfg.momentum_state_path);
    let mut state = match momentum_state::load(path) {
        Ok(s) => s,
        Err(e) => {
            warn!("momentum: could not load state for adoption: {e}");
            return false;
        }
    };
    // Only adopt into free slots; already-held mints are excluded below.
    let cap = state.capacity(cfg.momentum_max_positions);
    if cap == 0 {
        return false; // all slots occupied
    }
    let held_mints = state.held_mints();
    // Join the watched universe with wallet balances + live prices; skip already-held mints.
    // Observability ("never silently inert"): a watched token IS in the wallet but fails a
    // join — log which lookup broke, else a skipped adoption is undiagnosable from logs.
    let mut cands: Vec<AdoptCandidate> = Vec::new();
    for w in watched.iter().filter(|w| !held_mints.contains(&w.mint)) {
        let Some(amount) =
            portfolio.tokens.iter().find(|t| t.mint == w.mint).map(|t| t.amount)
        else {
            continue; // not in the wallet — the normal case, stay quiet
        };
        if amount <= 0.0 {
            continue;
        }
        let Some(price) = prices.get(&w.mint).copied().filter(|p| *p > 0.0) else {
            info!(
                "momentum: adoption skip {} — wallet holds {:.6} but no live price yet (key {})",
                w.symbol, amount, w.mint
            );
            continue;
        };
        cands.push(AdoptCandidate {
            mint: w.mint.clone(),
            symbol: w.symbol.clone(),
            amount,
            price_usd: price,
        });
    }
    let min_usd = cfg.momentum_trade_usdc * 0.5;
    for c in &cands {
        let usd = c.amount * c.price_usd;
        if usd < min_usd {
            info!(
                "momentum: adoption skip {} — wallet value ${:.2} < ${:.2} floor (0.5 × MOMENTUM_TRADE_USDC)",
                c.symbol, usd, min_usd
            );
        }
    }
    let to_adopt: Vec<AdoptCandidate> = match choose_adoption(cands, min_usd, cap) {
        Adoption::None => return false,
        Adoption::Ambiguous(n) => {
            warn!(
                "momentum: {n} watched holdings worth ≥ ${:.0} in the wallet — ambiguous which to adopt; \
                 staying FLAT. Leave only one or raise MOMENTUM_MAX_POSITIONS (or disable \
                 MOMENTUM_ADOPT_WALLET_POSITION).",
                min_usd
            );
            return false;
        }
        Adoption::One(c) => vec![c],
        Adoption::Many(cs) => cs,
    };
    let ts = now_ts();
    let mut adopted_any = false;
    for c in to_adopt {
        // Dedup: skip if somehow already present (race-safe).
        if state.positions.iter().any(|p| p.mint == c.mint) {
            continue;
        }
        let usdc_basis = c.amount * c.price_usd;
        state.positions.push(Position {
            mint: c.mint.clone(),
            symbol: c.symbol.clone(),
            entry_ts: ts,
            entry_price_usd: c.price_usd,
            token_amount: c.amount,
            usdc_spent: usdc_basis,
            peak_price_usd: c.price_usd,
            entry_sig: "adopted".to_string(),
            dry_run: false,
        });
        audit(cfg, ts, ActionKind::Adopted {
            symbol: c.symbol.clone(),
            mint: c.mint.clone(),
            token_amount: c.amount,
            entry_price_usd: c.price_usd,
        });
        info!(
            "momentum: ADOPTED wallet position {} — {:.6} tokens @ ${:.6} (basis ${:.2}); managing from here \
             (trailing stop / fade exit). Real cost basis unknown — PnL measured from adoption.",
            c.symbol, c.amount, c.price_usd, usdc_basis
        );
        adopted_any = true;
    }
    if adopted_any {
        if let Err(e) = momentum_state::save(path, &state) {
            warn!("momentum: failed to persist adopted position(s): {e}");
            return false;
        }
    }
    adopted_any
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
    // FLAT — nothing to invalidate
    if state.positions.is_empty() {
        return false;
    }
    // Find all live (non-dry-run) positions that are no longer backed by a wallet balance.
    let unbacked: Vec<String> = state
        .positions
        .iter()
        .filter(|p| !p.dry_run)
        .filter(|p| {
            let held = portfolio
                .tokens
                .iter()
                .find(|t| t.mint == p.mint)
                .map(|t| t.amount)
                .unwrap_or(0.0);
            held <= 0.0
        })
        .map(|p| p.mint.clone())
        .collect();
    if unbacked.is_empty() {
        return false; // all live positions still wallet-backed — valid
    }
    let ts = now_ts();
    // Log and bench each unbacked mint before removing it.
    for mint in &unbacked {
        let symbol = state
            .positions
            .iter()
            .find(|p| p.mint == *mint)
            .map(|p| p.symbol.as_str())
            .unwrap_or(mint.as_str());
        warn!(
            "momentum: wallet no longer holds {} (sold/moved externally) — invalidating stale position",
            symbol
        );
        state.last_exit_ts_per_mint.insert(mint.clone(), ts);
    }
    // Drop only the unbacked live positions; dry-run positions and backed live ones survive.
    state.positions.retain(|p| p.dry_run || !unbacked.contains(&p.mint));
    if state.positions.is_empty() {
        // Log the → FLAT transition only when the last position was cleared.
        warn!("momentum: no remaining positions — now FLAT");
    }
    if let Err(e) = momentum_state::save(path, &state) {
        warn!("momentum: failed to persist invalidated state: {e}");
    }
    true
}

// ───────────────────────────── ENTRY (FLAT, 60s) ─────────────────────────────

/// Select eligible candidates from a ranked list, up to `cap` slots, skipping
/// mints already held. Applies per-token over-extension and min-metric gates.
/// Pure (no I/O) so it can be unit-tested in isolation.
///
/// Returns indices into `ranked` for candidates that passed every static gate
/// (stale / falling / fading / overextended / cooldown / score / regime) *and* are not
/// already held.  Callers that need the actual swap work through the indices.
///
/// `risk_on`: the global SOL regime signal. When `false`, a candidate is still
/// eligible iff it is individually regime-exempt (`regime_filter: false`).
fn select_entries<'a>(
    ranked: &'a [Candidate],
    held_mints: &[String],
    cap: usize,
    watched: &[WatchedToken],
    global_min_score: f64,
    global_max_run_pct: f64,
    last_exit_ts: &HashMap<String, i64>,
    cooldown_secs: i64,
    ts: i64,
    risk_on: bool,
) -> Vec<&'a Candidate> {
    if cap == 0 {
        return vec![];
    }
    let mut selected: Vec<&Candidate> = Vec::with_capacity(cap);
    for c in ranked {
        if selected.len() >= cap {
            break;
        }
        // Skip already-held mints (dedup guard).
        if held_mints.iter().any(|m| m == &c.mint) {
            continue;
        }
        if c.stale || c.falling || c.metric_fading {
            continue;
        }
        // Per-candidate regime gate: non-exempt tokens require global risk-on.
        if !risk_on && !regime_exempt_for(watched, &c.mint) {
            continue;
        }
        // Per-token over-extension: recompute with this token's own max_run override.
        let token_max_run = max_run_for(watched, &c.mint, global_max_run_pct);
        if is_overextended(c.metrics.ret, token_max_run, c.slope_recent, c.slope_full) {
            continue;
        }
        // Per-token score threshold.
        let min_score = min_metric_for(watched, &c.mint, global_min_score);
        if c.score <= min_score {
            continue;
        }
        // Re-entry cooldown (per-token override ?? global).
        if let Some(&last) = last_exit_ts.get(&c.mint) {
            if ts - last < reentry_cooldown_for(watched, &c.mint, cooldown_secs) {
                continue;
            }
        }
        selected.push(c);
    }
    selected
}

pub async fn maybe_enter(ctx: &MomentumContext<'_>) -> Result<Vec<TradeOutcome>> {
    let cfg = ctx.cfg;
    if !cfg.enable_momentum_trader || halted(cfg) {
        return Ok(vec![]);
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
        cfg.momentum_max_run_pct,
        cfg.momentum_decel_lookback_min,
        cfg.momentum_confirm_lag_obs,
    );
    log_rank_line(cfg, ctx.watched, &ranked, cfg.momentum_rank_metric);
    audit(cfg, ts, ActionKind::RankSnapshot {
        metric: cfg.momentum_rank_metric.to_string(),
        min_score: cfg.momentum_min_score,
        tokens: snapshot_tokens(ctx.watched, &ranked),
    });

    // HOLDING — log unrealized PnL for every held position and check each for a
    // soft fade-exit. Rotation is now `maybe_evict`'s job (called by the watcher
    // between maybe_exit and maybe_enter); it is NOT called here to prevent
    // double-rotation. After collecting fade outcomes we fall through to the FLAT
    // section so free slots are filled in the same tick.
    let held_positions: Vec<_> = state.positions.clone();
    let mut slow_tick_outcomes: Vec<TradeOutcome> = Vec::new();
    for pos in held_positions {
        if let Some(px) = ctx.prices_usd.get(&pos.mint).copied().filter(|p| *p > 0.0) {
            let unreal = (px - pos.entry_price_usd) / pos.entry_price_usd * 100.0;
            info!(
                "momentum: HOLDING {} — entry ${:.6} now ${:.6} peak ${:.6} unrealized {:+.2}%",
                pos.symbol, pos.entry_price_usd, px, pos.peak_price_usd, unreal
            );
        }
        // Fade-take-profit (slow tick only); rotation is handled by maybe_evict.
        if let Some(outcome) =
            maybe_take_profit_on_fade(ctx, &mut state, state_path, pos, &ranked, ts).await?
        {
            slow_tick_outcomes.push(outcome);
        }
    }

    // Fade exits already happened — don't chain an entry in the same slow tick.
    // This preserves N=1 equivalence (old code returned early after fade) and is
    // safe for N>1 (the freed slot will be filled on the next 60s tick instead).
    if !slow_tick_outcomes.is_empty() {
        return Ok(slow_tick_outcomes);
    }

    // FLAT / partial — consider opening a new position in free slots.
    // Market-regime gate (entry-only; exits unaffected): stay in cash unless the broad
    // market is risk-on. Mode picks the signal — `level` (SOL>MA), `trend` (SOL slope_r2
    // clean-uptrend, the backtest-preferred regime momentum), or `off`.
    let (risk_on, diag) = regime_risk_on(
        ctx.history,
        cfg.momentum_regime_mode,
        cfg.momentum_regime_obs,
        cfg.momentum_regime_trend_min,
    );
    if let Some(d) = diag {
        info!(
            "momentum: SOL regime [{}] {} → {}",
            cfg.momentum_regime_mode,
            d,
            if risk_on { "risk-on" } else { "risk-off — staying FLAT" }
        );
    }
    // Fast-path: when risk-off AND no watched token is regime-exempt, preserve today's
    // behavior exactly — early-return without reaching the slot-fill loop.
    if !risk_on && !ctx.watched.iter().any(|w| regime_exempt_for(ctx.watched, &w.mint)) {
        return Ok(slow_tick_outcomes);
    }
    // When risk-off but at least one token is exempt, fall through: select_entries
    // applies a per-candidate `(risk_on || regime_exempt_for)` gate so only exempt
    // tokens can enter. Non-exempt tokens are filtered there, not here.
    let used = momentum_state::entries_last_24h(&state, ts);
    if used >= cfg.momentum_max_trades_per_day as usize {
        audit(cfg, ts, ActionKind::SkipDailyCap { used, cap: cfg.momentum_max_trades_per_day });
        return Ok(slow_tick_outcomes);
    }

    // No capital, no trade — LIVE only. A real entry would fail to submit without
    // USDC, but paper mode spends nothing, so dry-run trades regardless of balance.
    // Pre-screen against the SMALLEST per-token size any watched token could need (a
    // per-token trade_usdc override may be below the global), so we don't over-block an
    // affordable smaller entry; the per-candidate gate below re-checks the exact size.
    let min_entry_size = ctx
        .watched
        .iter()
        .map(|w| trade_usdc_for(ctx.watched, &w.mint, cfg.momentum_trade_usdc))
        .fold(cfg.momentum_trade_usdc, f64::min);
    if !cfg.momentum_dry_run && ctx.usdc_balance < min_entry_size {
        info!(
            "momentum: USDC balance {:.2} < smallest trade size {:.2} — staying FLAT (fund the wallet to trade)",
            ctx.usdc_balance, min_entry_size
        );
        audit(cfg, ts, ActionKind::SkipInsufficientUsdc {
            have: ctx.usdc_balance,
            need: min_entry_size,
        });
        return Ok(slow_tick_outcomes);
    }

    if ranked.is_empty() {
        // Observability: never silently inert. A token qualifies only with both a
        // live price AND ≥ (SORTINO_MIN_OBS+1) prices in the lookback window
        // (the window of N prices yields N-1 returns).
        info!(
            "momentum: no entry candidate yet — {} watched token(s) each need a live price + ≥{} obs in the lookback window (lookback={}); warming up",
            ctx.watched.len(), SORTINO_MIN_OBS + 1, cfg.momentum_lookback_obs
        );
        return Ok(slow_tick_outcomes);
    }

    // How many free slots remain?
    let cap = state.capacity(cfg.momentum_max_positions);
    if cap == 0 {
        // Full — eviction is handled by maybe_evict (called by the watcher before
        // maybe_enter). Nothing for maybe_enter to do.
        return Ok(slow_tick_outcomes);
    }

    // Emit per-candidate audit skips for the stale/overextended/falling/fading/cooldown
    // gates before handing off to the pure selector (which doesn't audit).
    for c in &ranked {
        if c.stale {
            audit(cfg, ts, ActionKind::SkipMarketClosed { symbol: c.symbol.clone() });
        } else if is_overextended(
            c.metrics.ret,
            max_run_for(ctx.watched, &c.mint, cfg.momentum_max_run_pct),
            c.slope_recent,
            c.slope_full,
        ) {
            audit(cfg, ts, ActionKind::SkipOverextended {
                symbol: c.symbol.clone(),
                run_pct: (c.metrics.ret.exp() - 1.0) * 100.0,
                max_run_pct: cfg.momentum_max_run_pct,
            });
        } else if c.falling {
            audit(cfg, ts, ActionKind::SkipFalling { symbol: c.symbol.clone() });
        } else if c.metric_fading {
            audit(cfg, ts, ActionKind::SkipMetricFading {
                symbol: c.symbol.clone(),
                metric: cfg.momentum_rank_metric.to_string(),
                lag_obs: cfg.momentum_confirm_lag_obs,
            });
        } else if let Some(&last) = state.last_exit_ts_per_mint.get(&c.mint) {
            let since = ts - last;
            let cooldown = reentry_cooldown_for(ctx.watched, &c.mint, cfg.momentum_reentry_cooldown_secs);
            if since < cooldown {
                audit(cfg, ts, ActionKind::SkipReentryCooldown {
                    symbol: c.symbol.clone(),
                    secs_remaining: cooldown - since,
                });
            }
        }
    }

    let eligible = select_entries(
        &ranked,
        &state.held_mints(),
        cap,
        ctx.watched,
        cfg.momentum_min_score,
        cfg.momentum_max_run_pct,
        &state.last_exit_ts_per_mint,
        cfg.momentum_reentry_cooldown_secs,
        ts,
        risk_on,
    );

    if eligible.is_empty() {
        // Emit SkipBelowThreshold for the top ranked candidate that didn't qualify
        // (mirrors old single-best behaviour for observability).
        if let Some(top) = ranked.first() {
            let min_score = min_metric_for(ctx.watched, &top.mint, cfg.momentum_min_score);
            if top.score <= min_score {
                info!(
                    "momentum: best candidate {} {}={:.2} ≤ MIN {:.2} — staying FLAT",
                    top.symbol, cfg.momentum_rank_metric, top.score, min_score
                );
                audit(cfg, ts, ActionKind::SkipBelowThreshold {
                    best_symbol: top.symbol.clone(),
                    best_sortino: top.score,
                    min_sortino: min_score,
                    metric: cfg.momentum_rank_metric.to_string(),
                });
            } else {
                info!("momentum: all ranked candidates are in re-entry cooldown — staying FLAT");
            }
        }
        return Ok(slow_tick_outcomes);
    }

    let mut outcomes: Vec<TradeOutcome> = slow_tick_outcomes;
    let mut remaining_cap = cap;

    for best in eligible {
        if remaining_cap == 0 {
            break;
        }
        // Re-check daily cap (it decrements as we open positions this tick).
        let used_now = momentum_state::entries_last_24h(&state, ts);
        if used_now >= cfg.momentum_max_trades_per_day as usize {
            audit(cfg, ts, ActionKind::SkipDailyCap { used: used_now, cap: cfg.momentum_max_trades_per_day });
            break;
        }

        // A fill returns Some(outcome); a per-candidate skip or a benign revert (which
        // already audited + persisted its own escalated attempt inside) returns None — in
        // which case we just move on to the next eligible candidate.
        if let Some(outcome) = try_open_position(ctx, &mut state, state_path, best, ts).await? {
            outcomes.push(outcome);
            remaining_cap -= 1;
        }
    }

    // Save state once after all entries (or after a revert mid-loop that already saved).
    if !outcomes.is_empty() {
        momentum_state::save(state_path, &state)?;
    }

    Ok(outcomes)
}

/// Attempt to open ONE position for `best`: the mean-reversion entry gates, decimals,
/// slippage escalation, per-token size + balance recheck, local-impact pre-gate, Jupiter
/// quote, cost gate, entry-divergence guard, submit/dry-run, and the `Position` record.
/// Returns `Ok(Some(outcome))` on a fill and `Ok(None)` on any per-candidate skip or a
/// benign revert (the revert persists its own escalated attempt before returning).
/// Extracted verbatim from `maybe_enter`'s per-candidate loop so `maybe_enter_spike`
/// reuses the exact same buy path — no duplicated quote/cost/submit logic. The caller
/// owns capacity + daily-cap accounting and the post-loop state save.
///
/// Staged (TWAP) entry: with `MOMENTUM_ENTRY_STEPS` ≥ 2 the notional is split into N
/// sequential tranches (`MOMENTUM_ENTRY_STEP_SLEEP_SECS` apart, pure TWAP — no gate
/// re-checks). Gates run once, on tranche 1; a tranche-1 failure is the classic revert
/// path above, while a later tranche's failure KEEPS the partial fill as the position
/// and stops buying. Always ONE `Position`/`Entered` per call (one slot, one daily-cap
/// count) with summed amounts. Applies uniformly to slow-tick and spike entries.
async fn try_open_position(
    ctx: &MomentumContext<'_>,
    state: &mut momentum_state::TraderState,
    state_path: &Path,
    best: &Candidate,
    ts: i64,
) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;

    // Mean-reversion entry confirmation ("both true"): require the strong token to
    // ALSO be oversold right now (z over MOMENTUM_ENTRY_DIP_OBS ≤ −MOMENTUM_ENTRY_DIP_Z)
    // — buy the pullback within a strong token, not the exhaustion top. `0` disables.
    // Backtest-promising but UNVALIDATED on the current sample; default off.
    if cfg.momentum_entry_dip_obs > 0 {
        let oversold = entry_dip_z(ctx.history, &best.mint, cfg.momentum_entry_dip_obs)
            .is_some_and(|z| z <= -cfg.momentum_entry_dip_z);
        if !oversold {
            info!(
                "momentum: {} clears {} but isn't oversold (dip gate {}obs/{:.1}σ) — staying FLAT",
                best.symbol, cfg.momentum_rank_metric, cfg.momentum_entry_dip_obs, cfg.momentum_entry_dip_z
            );
            return Ok(None);
        }
    }

    // Overbought entry gate (mean-reversion filter): skip when the candidate is
    // extended above its own mean (z over MOMENTUM_ENTRY_MAX_Z_OBS > MOMENTUM_ENTRY_MAX_Z)
    // — don't chase the top; only buy at/below the recent average. `0` obs disables.
    // Independent of the dip gate; both on ⇒ a band −dip_z ≤ z ≤ max_z.
    if entry_overbought(ctx.history, &best.mint, cfg.momentum_entry_max_z_obs, cfg.momentum_entry_max_z) {
        info!(
            "momentum: {} clears {} but is overbought (>{:.1}σ over {}obs) — staying FLAT",
            best.symbol, cfg.momentum_rank_metric, cfg.momentum_entry_max_z, cfg.momentum_entry_max_z_obs
        );
        return Ok(None);
    }

    // Macro-calendar blackout: no NEW positions within MOMENTUM_MACRO_BLACKOUT_HOURS
    // before (or _AFTER_HOURS after) a scheduled CPI/PPI/FOMC release — hot prints
    // dump the whole market (2026-05-12 CPI: −8% SOL) and the entry signal cannot see
    // them coming. Exits and held positions are untouched. The guard keeps the
    // calendar unloaded (and its staleness warning silent) while the gate is off.
    if cfg.momentum_macro_blackout_hours > 0.0 || cfg.momentum_macro_blackout_after_hours > 0.0 {
        if let Some(ev) = macro_blackout(
            macro_calendar(&cfg.momentum_macro_calendar_path),
            ts,
            cfg.momentum_macro_blackout_hours,
            cfg.momentum_macro_blackout_after_hours,
        ) {
            info!(
                "momentum: {} clears {} but {} prints {:+.1}h from now — macro blackout, staying FLAT",
                best.symbol,
                cfg.momentum_rank_metric,
                ev.name,
                (ev.ts - ts) as f64 / 3600.0
            );
            return Ok(None);
        }
    }

    let Some(&token_decimals) = ctx.decimals.get(&best.mint) else {
        audit(cfg, ts, ActionKind::QuoteFailed { symbol: best.symbol.clone(), reason: "missing decimals".into() });
        return Ok(None);
    };

    // Entries escalate slippage on consecutive reverts, but capped *tight*: the
    // rank deliberately picks fast movers, which can run past a 50bps min-out
    // before the tx lands. Resets to base whenever the best candidate changes.
    let entry_attempt = entry_attempt_for(&state.entry_attempt, &best.mint);
    let entry_slippage_bps = escalated_slippage_bps(
        cfg.momentum_slippage_bps,
        entry_attempt,
        cfg.momentum_entry_slippage_cap_bps,
    );

    // Per-token trade size: override from momentum_tokens.json, else global config.
    // Compute once here and use consistently for all sizing/cost/log sites below.
    // Balance pre-screen at the top of maybe_enter uses the global config value as a
    // conservative guard; the per-token size is the real gate for this candidate.
    let size = trade_usdc_for(ctx.watched, &best.mint, cfg.momentum_trade_usdc);
    if !cfg.momentum_dry_run && ctx.usdc_balance < size {
        info!(
            "momentum: USDC balance {:.2} < per-token trade size {:.2} for {} — skipping",
            ctx.usdc_balance, size, best.symbol
        );
        audit(cfg, ts, ActionKind::SkipInsufficientUsdc {
            have: ctx.usdc_balance,
            need: size,
        });
        return Ok(None);
    }

    // Local impact pre-gate (opt-in, MOMENTUM_LOCAL_IMPACT): the gRPC ingestion
    // task continuously estimates the price impact of a MOMENTUM_TRADE_USDC-sized
    // buy from live pool state (CP + Whirlpool pools only). The local model
    // ignores routing, so only an obviously-doomed estimate (> 2x the cost budget)
    // is acted on here — anything closer is left to the authoritative Jupiter quote
    // and its SkipCostGate below. Off by default (no gRPC lookup even happens);
    // only fires with a fresh (<120s) estimate available.
    if cfg.momentum_local_impact {
        if let Some(feed) = ctx.grpc_feed {
            if let Some(est) = feed.est_impact_bps(&best.mint, Duration::from_secs(120)) {
                if est > 2 * cfg.momentum_max_cost_bps {
                    warn!(
                        "momentum: {} entry skipped — local impact estimate {}bps exceeds 2x cost budget (budget {}bps)",
                        best.symbol, est, cfg.momentum_max_cost_bps
                    );
                    audit(cfg, ts, ActionKind::SkipLocalImpact {
                        symbol: best.symbol.clone(),
                        est_bps: est,
                        budget_bps: cfg.momentum_max_cost_bps,
                    });
                    return Ok(None);
                }
            }
        }
    }

    // Quote USDC → token for the fixed notional.
    let usdc_raw = jupiter::to_raw_amount(size, USDC_DECIMALS);
    // Staged (TWAP) entry: split the notional into tranches (1 = the original
    // single-swap path). All gates below run once, on tranche 1's quote.
    // `step1_human` reuses `size` verbatim in the single-tranche case so the
    // unstaged path stays bit-identical (`to_raw_amount` truncates — a raw
    // round-trip would perturb every downstream f64).
    let steps = effective_entry_steps(cfg.momentum_entry_steps);
    if cfg.momentum_entry_steps.is_some_and(|n| n > MAX_ENTRY_STEPS) {
        warn!("momentum: MOMENTUM_ENTRY_STEPS={} clamped to {MAX_ENTRY_STEPS}", cfg.momentum_entry_steps.unwrap_or(0));
    }
    let step_amounts = entry_step_amounts(usdc_raw, steps);
    let step1_human = if step_amounts.len() == 1 {
        size
    } else {
        jupiter::from_raw_amount(step_amounts[0], USDC_DECIMALS)
    };
    let quote = match jupiter::quote(
        ctx.http,
        &cfg.momentum_jupiter_api_url,
        USDC_MINT,
        &best.mint,
        step_amounts[0],
        entry_slippage_bps,
    )
    .await
    {
        Ok(q) => q,
        Err(e) => {
            warn!("momentum: /quote failed for {} via {} — {e}", best.symbol, cfg.momentum_jupiter_api_url);
            audit(cfg, ts, ActionKind::QuoteFailed { symbol: best.symbol.clone(), reason: e.to_string() });
            return Ok(None);
        }
    };

    let slip_bps = jupiter::price_impact_bps(&quote);
    let sol_price = ctx.prices_usd.get(SOL_KEY).copied().unwrap_or(0.0);
    // Gas in bps of the TRANCHE notional — one tx fee per tranche, so this equals
    // steps × gas over the full notional, with a single u32 truncation at the end
    // (multiplying the truncated full-size bps by steps would floor to 0 at
    // typical sizes). Both cost-gate terms are per-tranche: slip_bps comes from a
    // tranche-sized quote, keeping the gate "execution cost per unit notional".
    let gas_bps = est_gas_bps(step1_human, sol_price);
    let total_cost_bps = slip_bps + gas_bps;
    if total_cost_bps > cfg.momentum_max_cost_bps {
        audit(cfg, ts, ActionKind::SkipCostGate {
            symbol: best.symbol.clone(),
            total_cost_bps,
            gas_bps,
            slip_bps,
            budget_bps: cfg.momentum_max_cost_bps,
        });
        return Ok(None);
    }

    let expected_token = jupiter::from_raw_amount(quote.out_amount.parse::<u64>().unwrap_or(0), token_decimals);
    if expected_token <= 0.0 {
        audit(cfg, ts, ActionKind::QuoteFailed { symbol: best.symbol.clone(), reason: "zero out amount".into() });
        return Ok(None);
    }

    // Entry price-freshness guard: the rank/quote signal above was computed moments
    // ago — if the live gRPC price has since diverged from what Jupiter will
    // actually fill at, the signal is stale. Off by default (0, guard skipped
    // entirely — no gRPC lookup even happens); only fires with a trusted gRPC price
    // available (nothing to compare against otherwise ⇒ skip the guard, not the trade).
    if cfg.momentum_entry_divergence_bps > 0 {
        if let Some(g) = trusted_grpc_price(ctx.grpc_feed, &best.mint, cfg.momentum_grpc_stale_secs) {
            let implied = step1_human / expected_token;
            if let Some(dev_bps) = quote_divergence_bps(implied, g) {
                if dev_bps > cfg.momentum_entry_divergence_bps {
                    warn!(
                        "momentum: {} entry skipped — Jupiter implied fill ${:.6} diverges {}bps from live gRPC ${:.6} (budget {}bps)",
                        best.symbol, implied, dev_bps, g, cfg.momentum_entry_divergence_bps
                    );
                    audit(cfg, ts, ActionKind::SkipDivergence {
                        symbol: best.symbol.clone(),
                        implied,
                        grpc: g,
                        dev_bps,
                        budget_bps: cfg.momentum_entry_divergence_bps,
                    });
                    return Ok(None);
                }
            }
        }
    }

    let sig = if cfg.momentum_dry_run {
        "dry-run".to_string()
    } else {
        match submit_and_confirm(cfg, ctx.http, &quote).await {
            Ok((s, confirmed)) => {
                if !confirmed {
                    warn!("momentum: ENTER {} submitted but not confirmed in {}s (tx={s}); exit uses on-chain balance",
                        best.symbol, CONFIRM_TIMEOUT.as_secs());
                }
                s.to_string()
            }
            Err(e) => {
                // Entry is optional: a revert (typically 0x1771 — the mover ran past
                // our min-out) is benign, NOT a hard error. Bump this candidate's
                // attempt count (widens the next quote, capped tight), stay FLAT, retry.
                let count = entry_attempt + 1;
                // Stamp the fast-tick retry deadline (0 = feature off → the
                // record waits for the next slow tick, pre-feature behavior).
                let next_retry_ts = if cfg.momentum_entry_retry_secs > 0 {
                    ts + cfg.momentum_entry_retry_secs as i64
                } else {
                    0
                };
                state.entry_attempt = Some(momentum_state::EntryAttempt {
                    mint: best.mint.clone(),
                    count,
                    next_retry_ts,
                });
                let next_bps = escalated_slippage_bps(
                    cfg.momentum_slippage_bps, count, cfg.momentum_entry_slippage_cap_bps,
                );
                warn!(
                    "momentum: ENTER {} reverted at {} bps (attempt {}) — {e:#}; staying FLAT, re-quoting at {} bps next tick",
                    best.symbol, entry_slippage_bps, count, next_bps,
                );
                audit(cfg, ts, ActionKind::EntryReverted {
                    symbol: best.symbol.clone(),
                    attempt: count,
                    slippage_bps: entry_slippage_bps,
                    next_slippage_bps: next_bps,
                    reason: format!("{e:#}"),
                });
                momentum_state::save(state_path, &*state)?;
                return Ok(None); // benign — no position opened, capital intact, retry next tick
            }
        }
    };

    // Tranche accumulators, seeded from tranche 1 (with steps=1 the loop below is
    // empty and every value passes through unchanged). Later tranches append; a
    // mid-ladder failure KEEPS what already filled as the position and stops
    // buying — no unwind, no retry. Pure TWAP: no gate re-checks between tranches;
    // each tranche's own quote + slippage min-out is its protection.
    // INVARIANT: a failure past tranche 1 must never write `state.entry_attempt`
    // (that record means "stayed FLAT, retry with escalation" — a position now
    // exists for this mint, and a stale record would leak escalated slippage into
    // a future entry after exit + cooldown).
    let mut total_token = expected_token;
    let mut spent = step1_human;
    let mut sigs = vec![sig];
    for (i, &amt_raw) in step_amounts.iter().enumerate().skip(1) {
        let step_no = (i + 1) as u32; // 1-based, for logs/audit
        tokio::time::sleep(Duration::from_secs(cfg.momentum_entry_step_sleep_secs)).await;
        let step_quote = match jupiter::quote(
            ctx.http,
            &cfg.momentum_jupiter_api_url,
            USDC_MINT,
            &best.mint,
            amt_raw,
            entry_slippage_bps,
        )
        .await
        {
            Ok(q) => q,
            Err(e) => {
                warn!(
                    "momentum: ENTER {} tranche {step_no}/{steps} /quote failed — keeping partial fill ({:.2} of {:.2} USDC): {e}",
                    best.symbol, spent, size
                );
                audit(cfg, ts, ActionKind::EntryStepFailed {
                    symbol: best.symbol.clone(),
                    step: step_no,
                    steps,
                    reason: e.to_string(),
                });
                break;
            }
        };
        let step_token =
            jupiter::from_raw_amount(step_quote.out_amount.parse::<u64>().unwrap_or(0), token_decimals);
        if step_token <= 0.0 {
            warn!(
                "momentum: ENTER {} tranche {step_no}/{steps} quoted zero out — keeping partial fill ({:.2} of {:.2} USDC)",
                best.symbol, spent, size
            );
            audit(cfg, ts, ActionKind::EntryStepFailed {
                symbol: best.symbol.clone(),
                step: step_no,
                steps,
                reason: "zero out amount".into(),
            });
            break;
        }
        let step_sig = if cfg.momentum_dry_run {
            "dry-run".to_string()
        } else {
            match submit_and_confirm(cfg, ctx.http, &step_quote).await {
                Ok((s, confirmed)) => {
                    if !confirmed {
                        warn!(
                            "momentum: ENTER {} tranche {step_no}/{steps} submitted but not confirmed in {}s (tx={s}); exit uses on-chain balance",
                            best.symbol, CONFIRM_TIMEOUT.as_secs()
                        );
                    }
                    s.to_string()
                }
                Err(e) => {
                    warn!(
                        "momentum: ENTER {} tranche {step_no}/{steps} reverted — keeping partial fill ({:.2} of {:.2} USDC): {e:#}",
                        best.symbol, spent, size
                    );
                    audit(cfg, ts, ActionKind::EntryStepFailed {
                        symbol: best.symbol.clone(),
                        step: step_no,
                        steps,
                        reason: format!("{e:#}"),
                    });
                    break;
                }
            }
        };
        total_token += step_token;
        spent += jupiter::from_raw_amount(amt_raw, USDC_DECIMALS);
        sigs.push(step_sig);
    }
    let filled = sigs.len() as u32;
    let steps_note = if steps > 1 { format!(" steps={filled}/{steps}") } else { String::new() };
    let sig = sigs.join(",");

    // P&L cost basis includes the entry swap's gas (one tx fee per FILLED tranche),
    // so realized P&L nets it at the eventual close (the basis is subtracted exactly
    // once → can't cancel like a mid-chain charge would). The PORTFOLIO USDC delta
    // (TradeOutcome below) stays at the real notional — gas is paid in SOL, not USDC.
    let entry_basis = spent + est_gas_usdc(sol_price) * filled as f64;
    state.entry_attempt = None; // entry filled — clear escalation
    state.positions.push(Position {
        mint: best.mint.clone(),
        symbol: best.symbol.clone(),
        entry_ts: ts,
        entry_price_usd: best.price_usd,
        token_amount: total_token,
        usdc_spent: entry_basis,
        peak_price_usd: best.price_usd,
        entry_sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });

    audit(cfg, ts, ActionKind::Entered {
        symbol: best.symbol.clone(),
        mint: best.mint.clone(),
        usdc_in: spent,
        token_amount: total_token,
        entry_price_usd: best.price_usd,
        cost_bps: total_cost_bps,
        sig: sig.clone(),
        dry_run: cfg.momentum_dry_run,
    });
    let tag = if cfg.momentum_dry_run { "DRY-RUN ENTER" } else { "ENTER" };
    let label = token_label(ctx.watched, &best.mint, &best.symbol);
    info!("momentum: {tag} {label} — {:.6} tokens for {} USDC @ ${:.6} ({}={:.2}, cost={total_cost_bps}bps{steps_note}) tx={sig}",
        total_token, spent, best.price_usd, cfg.momentum_rank_metric, best.score);
    // Emails are live-only (see email_trade), so the subject is always "ENTER".
    email_trade(cfg, &format!("[Momentum] ENTER {label}"),
        &format!("Bought {:.6} {} for {} USDC @ ${:.6}\n{}={:.2}  cost={total_cost_bps}bps{steps_note}\ntx={sig}",
            total_token, label, spent, best.price_usd, cfg.momentum_rank_metric, best.score)).await;

    Ok(Some(TradeOutcome::Entered {
        symbol: best.symbol.clone(),
        mint: best.mint.clone(),
        token_amount: total_token,
        usdc_spent: spent,
        dry_run: cfg.momentum_dry_run,
    }))
}

/// Spike-triggered fast entry (Approach B — latency accelerant): re-run the *normal
/// validated* entry decision for a single `mint` the moment its gRPC price spikes up,
/// instead of waiting for the 60s slow tick. The spike wins latency, not the decision —
/// every gate (rank metric / MIN_METRIC / regime / over-extension / cost / divergence /
/// capacity / cooldown / daily cap) still applies, reusing `rank_candidates`,
/// `select_entries`, and `try_open_position` (no duplicated buy logic). In `shadow` mode
/// it logs the would-be entry (local gates only — the cost/divergence gates need a live
/// quote) and never quotes or buys.
///
/// The watcher overlays the freshest spike price into `ctx.prices_usd`, but the rank
/// metric still reads the 60s `history` (the sub-second spike isn't in it yet) — which is
/// exactly why a spike cannot *manufacture* a passing metric; it only accelerates a token
/// that already qualifies.
pub async fn maybe_enter_spike(
    ctx: &MomentumContext<'_>,
    mint: &str,
    shadow: bool,
) -> Result<Vec<TradeOutcome>> {
    let cfg = ctx.cfg;
    if !cfg.enable_momentum_trader || halted(cfg) {
        return Ok(vec![]);
    }
    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    let ts = now_ts();

    // Rank the full watched set exactly as the slow tick does, then keep only the spiking
    // mint. Empty ⇒ the mint isn't watched, has no live price, or is still under the
    // SORTINO_MIN_OBS warm-up floor — nothing to fast-enter (no spike-only fallback: that
    // would be Approach A, buying a metric the token hasn't earned).
    let ranked = rank_candidates(
        ctx.watched,
        ctx.prices_usd,
        ctx.history,
        cfg.momentum_lookback_obs,
        cfg.momentum_stale_minutes,
        cfg.momentum_rank_metric,
        cfg.momentum_max_run_pct,
        cfg.momentum_decel_lookback_min,
        cfg.momentum_confirm_lag_obs,
    );
    let spike_cand: Vec<Candidate> = ranked.into_iter().filter(|c| c.mint == mint).collect();
    if spike_cand.is_empty() {
        let obs = price_series_with_ts(ctx.history, mint).len();
        info!(
            "momentum: SPIKE {mint} but not rankable yet ({obs} obs, need ≥{}; or unwatched/no live price) — skipping",
            SORTINO_MIN_OBS + 1
        );
        return Ok(vec![]);
    }

    // Capacity + daily cap: identical gates to the slow tick.
    let cap = state.capacity(cfg.momentum_max_positions);
    if cap == 0 {
        info!("momentum: SPIKE {mint} but all {} slot(s) full — skipping", cfg.momentum_max_positions);
        return Ok(vec![]);
    }
    let used = momentum_state::entries_last_24h(&state, ts);
    if used >= cfg.momentum_max_trades_per_day as usize {
        audit(cfg, ts, ActionKind::SkipDailyCap { used, cap: cfg.momentum_max_trades_per_day });
        return Ok(vec![]);
    }

    // Regime: respect the SOL regime gate unless MOMENTUM_SPIKE_REGIME_GATE is off, in
    // which case a spike may enter regardless of regime (still subject to every other gate).
    let risk_on = if cfg.momentum_spike_regime_gate {
        regime_risk_on(
            ctx.history,
            cfg.momentum_regime_mode,
            cfg.momentum_regime_obs,
            cfg.momentum_regime_trend_min,
        )
        .0
    } else {
        true
    };

    // Reuse the exact validated selector: held dedup, stale/falling/fading, per-candidate
    // regime, over-extension, MIN_METRIC, and re-entry cooldown all apply here.
    let eligible = select_entries(
        &spike_cand,
        &state.held_mints(),
        cap,
        ctx.watched,
        cfg.momentum_min_score,
        cfg.momentum_max_run_pct,
        &state.last_exit_ts_per_mint,
        cfg.momentum_reentry_cooldown_secs,
        ts,
        risk_on,
    );
    let Some(best) = eligible.first().copied() else {
        info!("momentum: SPIKE {mint} did not clear entry gates (MIN_METRIC/regime/cooldown/over-extension/held) — skipping");
        return Ok(vec![]);
    };

    if shadow {
        let min_score = min_metric_for(ctx.watched, &best.mint, cfg.momentum_min_score);
        info!(
            "momentum: SPIKE would-enter {} ({}={:.2} > MIN {:.2}, regime={}) — SHADOW, no order (cost/divergence gates need a live quote)",
            best.symbol, cfg.momentum_rank_metric, best.score, min_score, risk_on
        );
        return Ok(vec![]);
    }

    match try_open_position(ctx, &mut state, state_path, best, ts).await? {
        Some(outcome) => {
            momentum_state::save(state_path, &state)?;
            Ok(vec![outcome])
        }
        None => Ok(vec![]),
    }
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
    // Cheap pre-filter: never rotate out of an underwater position (current price ≤
    // entry) — that's the trailing stop's job. The precise "green net of the rotation
    // cost" test runs after the quote (once slippage + gas are known); this gross
    // check just avoids a quote round-trip when clearly red.
    let held_px = ctx.prices_usd.get(&pos.mint).copied().unwrap_or(0.0);
    if held_px <= pos.entry_price_usd {
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
                state.positions.retain(|p| p.mint != pos.mint);
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

    // Entry price-freshness guard (rotation buy leg): mirrors the maybe_enter guard —
    // skip if this quote's implied fill price for the target token has diverged from
    // the live gRPC price. Off by default (0, guard skipped entirely); only fires with
    // a trusted gRPC price available.
    if cfg.momentum_entry_divergence_bps > 0 {
        if let Some(g) = trusted_grpc_price(ctx.grpc_feed, &target.mint, cfg.momentum_grpc_stale_secs) {
            let out_amount = jupiter::from_raw_amount(quote.out_amount.parse::<u64>().unwrap_or(0), to_decimals);
            if out_amount > 0.0 {
                let implied = notional / out_amount;
                if let Some(dev_bps) = quote_divergence_bps(implied, g) {
                    if dev_bps > cfg.momentum_entry_divergence_bps {
                        warn!(
                            "momentum: rotate {}→{} skipped — Jupiter implied fill ${:.6} diverges {}bps from live gRPC ${:.6} (budget {}bps)",
                            pos.symbol, target.symbol, implied, dev_bps, g, cfg.momentum_entry_divergence_bps
                        );
                        audit(cfg, ts, ActionKind::SkipDivergence {
                            symbol: target.symbol,
                            implied,
                            grpc: g,
                            dev_bps,
                            budget_bps: cfg.momentum_entry_divergence_bps,
                        });
                        return Ok(None);
                    }
                }
            }
        }
    }

    // "Green" for rotation means green AFTER paying this swap's slippage + gas: only
    // rotate a winner whose unrealized gain still clears the rotation cost. Below it,
    // the A leg would close at/under its basis — hold instead.
    if !rotation_net_green(a_price, pos.entry_price_usd, total_cost_bps) {
        info!(
            "momentum: not rotating {}→{} — gain at ${:.6} vs entry ${:.6} doesn't clear the {}bps rotation cost",
            pos.symbol, target.symbol, a_price, pos.entry_price_usd, total_cost_bps
        );
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
    // Replace the A position with B in-place (not clobber all positions — multi-slot
    // safety: only the rotated slot is replaced, other slots are untouched).
    state.positions.retain(|p| p.mint != pos.mint);
    state.positions.push(Position {
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

// ─────────────────────────── EVICTION (HOLDING, 60s) ───────────────────────────

/// Find the index of the weakest-scoring GREEN held position (price > entry, non-stale).
/// Returns `None` when no green, rankable, non-stale position exists.
///
/// "Weakest" = lowest current ranked score. Red positions (price ≤ entry) are excluded
/// because their exit is the trailing stop's job, not eviction. Stale positions are
/// excluded because the fast-exit loop already flattens them (market-closed gate).
///
/// This is a pure fn — no I/O, no state mutation — so it is directly unit-testable.
pub fn weakest_green(
    positions: &[Position],
    ranked: &[Candidate],
    prices_usd: &HashMap<String, f64>,
) -> Option<usize> {
    let mut weakest: Option<(usize, f64)> = None;
    for (idx, pos) in positions.iter().enumerate() {
        let px = prices_usd.get(&pos.mint).copied().unwrap_or(0.0);
        if px <= pos.entry_price_usd {
            continue; // gross-green pre-filter (mirror sim's eviction + try_rotate)
        }
        let Some(c) = ranked.iter().find(|c| c.mint == pos.mint) else {
            continue; // not rankable — fast exit handles stale/missing-price cases
        };
        if c.stale {
            continue; // stale positions are flattened by the fast exit, not evicted
        }
        if weakest.map_or(true, |(_, s)| c.score < s) {
            weakest = Some((idx, c.score));
        }
    }
    weakest.map(|(idx, _)| idx)
}

/// Weakest-green eviction for multi-slot: when all N slots are full and
/// `MOMENTUM_ROTATE_MARGIN > 0`, rotate the weakest-scoring gross-green held position
/// into a stronger candidate (A→B swap, same execution path as `try_rotate`).
///
/// **Call site:** the watcher calls this between `maybe_exit` and `maybe_enter`
/// (slow 60s tick) to preserve the exits → eviction → entries ordering.
/// Task 5 wires the watcher call. For Task 4, the fn compiles and is unit-tested.
///
/// **N=1 reduction:** with exactly one held position, "weakest of one" is that
/// position — this path is identical to the existing `try_rotate` call in
/// `maybe_enter`. Correctness at N=1 follows by construction: the pure helpers
/// (`select_entries`, `weakest_green`, resolvers) are unit-tested; the async
/// paths are verified by reasoning and operator dry-run smoke (no async N=1
/// equivalence test exists).
pub async fn maybe_evict(ctx: &MomentumContext<'_>) -> Result<Vec<TradeOutcome>> {
    let cfg = ctx.cfg;
    if cfg.momentum_rotate_margin <= 0.0 {
        return Ok(vec![]); // rotation/eviction disabled
    }
    if !cfg.enable_momentum_trader {
        return Ok(vec![]);
    }
    // Eviction opens a new position → blocked when halted (same as entries).
    if halted(cfg) {
        return Ok(vec![]);
    }

    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    let ts = now_ts();

    // Guard: only run when all slots are full (at capacity).
    if state.positions.len() < cfg.momentum_max_positions {
        return Ok(vec![]); // free slots exist — let maybe_enter fill them
    }
    if state.positions.is_empty() {
        return Ok(vec![]); // FLAT — nothing to evict
    }

    // Daily cap check: a rotation counts against the cap (it opens a new position).
    if momentum_state::entries_last_24h(&state, ts) >= cfg.momentum_max_trades_per_day as usize {
        return Ok(vec![]);
    }

    // Rank all watched tokens to score the held positions and find a stronger candidate.
    let ranked = rank_candidates(
        ctx.watched,
        ctx.prices_usd,
        ctx.history,
        cfg.momentum_lookback_obs,
        cfg.momentum_stale_minutes,
        cfg.momentum_rank_metric,
        cfg.momentum_max_run_pct,
        cfg.momentum_decel_lookback_min,
        cfg.momentum_confirm_lag_obs,
    );

    // Find the weakest-scoring gross-green, non-stale held position.
    let Some(weakest_idx) = weakest_green(&state.positions, &ranked, ctx.prices_usd) else {
        return Ok(vec![]); // no green position to evict
    };

    let pos = state.positions[weakest_idx].clone();

    // Delegate to try_rotate for the actual A→B swap + state mutation.
    // try_rotate handles: mode-mismatch guard, daily-cap re-check, cost/net-green gate,
    // balance fetch, quote, submit_and_confirm, state update, audit, email.
    // At N=1: weakest_idx==0 == the single held position → identical to today's path.
    if let Some(outcome) = try_rotate(ctx, &mut state, state_path, pos, &ranked, ts).await? {
        return Ok(vec![outcome]);
    }

    Ok(vec![])
}

// ──────────────────────── TAKE-PROFIT ON FADE (HOLDING, 60s) ────────────────────────

/// Take profit when momentum dies but the trailing stop hasn't tripped. Runs on the
/// 60s tick after `try_rotate` declines (rotation takes precedence — keep capital
/// deployed if a stronger token exists). Flattens to USDC only when **all** hold:
///   - `MOMENTUM_EXIT_ON_FADE` is on,
///   - the held token is rankable (priced, warm, not market-closed — a closed market
///     is the fast exit's job),
///   - its active-metric score has faded to ≤ `momentum_min_score` (momentum gone),
///   - the position is **green** (current price > entry) — losses are left to the
///     trailing stop; never realize a loss on this soft signal.
async fn maybe_take_profit_on_fade(
    ctx: &MomentumContext<'_>,
    state: &mut momentum_state::TraderState,
    state_path: &Path,
    pos: Position,
    ranked: &[Candidate],
    ts: i64,
) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
    if !exit_on_fade_for(ctx.watched, &pos.mint, cfg.momentum_exit_on_fade) {
        return Ok(None);
    }
    // Mode-mismatch guard (same as try_rotate / maybe_exit): never act on a position
    // opened in the other mode.
    if pos.dry_run != cfg.momentum_dry_run {
        audit(cfg, ts, ActionKind::ModeMismatch {
            position_dry_run: pos.dry_run,
            config_dry_run: cfg.momentum_dry_run,
        });
        return Ok(None);
    }
    // Held token must be rankable to read its score; if it's stale/closed, leave the
    // flatten to the fast market-closed exit rather than acting on a frozen price.
    let held_score = match ranked.iter().find(|c| c.mint == pos.mint) {
        Some(c) if !c.stale => c.score,
        _ => return Ok(None),
    };
    let Some(px) = ctx.prices_usd.get(&pos.mint).copied().filter(|p| *p > 0.0) else {
        return Ok(None);
    };
    // Fire only on faded momentum AND a green position; otherwise keep riding (still
    // strong) or let the trailing stop own the exit (underwater). The fade threshold is the
    // held token's OWN per-token min_metric (falls back to global) — so a token tuned with
    // its own entry bar exits on that same bar, at any MOMENTUM_MAX_POSITIONS including 1.
    let min_score = min_metric_for(ctx.watched, &pos.mint, cfg.momentum_min_score);
    if !fade_take_profit(held_score, min_score, px, pos.entry_price_usd) {
        return Ok(None);
    }
    info!(
        "momentum: {} momentum faded ({}={:.2} ≤ MIN {:.2}) while green (${:.6} > entry ${:.6}) — taking profit",
        pos.symbol, cfg.momentum_rank_metric, held_score, min_score, px, pos.entry_price_usd
    );
    flatten_position(ctx, state, state_path, pos, px, "momentum faded", ts).await
}

// ────────────────────────── PER-TOKEN RESOLVERS ─────────────────────────────

/// Return the `TokenParams` override for `mint`, if any.
fn token_params_for<'a>(
    watched: &'a [WatchedToken],
    mint: &str,
) -> Option<&'a crate::portfolio::momentum_universe::TokenParams> {
    watched.iter().find(|w| w.mint == mint).and_then(|w| w.params.as_ref())
}

/// Per-token `min_metric` override, falling back to the global config value.
fn min_metric_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint)
        .and_then(|p| p.min_metric)
        .unwrap_or(global)
}

/// Per-token trailing-stop percentage override, falling back to the global config value.
fn trail_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint)
        .and_then(|p| p.trail_pct)
        .unwrap_or(global)
}

/// Per-token max-run percentage override, falling back to the global config value.
fn max_run_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint)
        .and_then(|p| p.max_run_pct)
        .unwrap_or(global)
}

/// A token is regime-exempt (ignores the global SOL gate) iff `params.regime_filter == false`.
/// Absent params or `regime_filter: true` → not exempt (obeys the gate, default behavior).
fn regime_exempt_for(watched: &[WatchedToken], mint: &str) -> bool {
    token_params_for(watched, mint).and_then(|p| p.regime_filter) == Some(false)
}

/// Per-token USDC trade size override, falling back to the global config value.
fn trade_usdc_for(watched: &[WatchedToken], mint: &str, global: f64) -> f64 {
    token_params_for(watched, mint)
        .and_then(|p| p.trade_usdc)
        .unwrap_or(global)
}

/// Per-token fade-exit toggle, falling back to the global config value.
fn exit_on_fade_for(watched: &[WatchedToken], mint: &str, global: bool) -> bool {
    token_params_for(watched, mint)
        .and_then(|p| p.exit_on_fade)
        .unwrap_or(global)
}

/// Per-token re-entry cooldown (seconds), falling back to the global config value.
fn reentry_cooldown_for(watched: &[WatchedToken], mint: &str, global: i64) -> i64 {
    token_params_for(watched, mint)
        .and_then(|p| p.reentry_cooldown_secs)
        .unwrap_or(global)
}

// ─────────────────────────── EXIT (HOLDING, fast) ───────────────────────────

pub async fn maybe_exit(ctx: &MomentumContext<'_>) -> Result<Vec<TradeOutcome>> {
    let cfg = ctx.cfg;
    // Deliberately NOT gated on halted(): a halted bot must still be able to EXIT
    // its open positions (the loss breaker / manual halt blocks only new entries and
    // rotations, in maybe_enter) — otherwise positions would be stranded.
    if !cfg.enable_momentum_trader {
        return Ok(vec![]);
    }
    let state_path = Path::new(&cfg.momentum_state_path);
    let mut state = momentum_state::load(state_path)?;
    if state.positions.is_empty() {
        return Ok(vec![]); // FLAT — nothing to exit
    }

    let ts = now_ts();

    // Price source: when MOMENTUM_GRPC_EXIT, prefer the live on-chain price for held
    // mints (fresh within the feed's stale window), REST-fetch only the rest. Flag off ⇒
    // REST for all (today's path).
    let held_mints: Vec<String> = state.positions.iter().map(|p| p.mint.clone()).collect();
    let mut prices_map: HashMap<String, f64> = HashMap::new();
    let mut rest_mints: Vec<String> = Vec::new();
    if cfg.momentum_grpc_exit {
        if let Some(feed) = ctx.grpc_feed {
            let stale = Duration::from_secs(cfg.momentum_grpc_stale_secs);
            let now = Instant::now();
            for m in &held_mints {
                match feed.map.get(m) {
                    Some(e) if now.duration_since(e.value().1) <= stale && e.value().0 > 0.0 => {
                        prices_map.insert(m.clone(), e.value().0);
                    }
                    _ => rest_mints.push(m.clone()),
                }
            }
        } else {
            rest_mints = held_mints.clone();
        }
    } else {
        rest_mints = held_mints.clone();
    }
    if !rest_mints.is_empty() {
        let rest = pricer::fetch_prices(ctx.http, &rest_mints, cfg.birdeye_api_key.as_deref())
            .await
            .unwrap_or_default();
        prices_map.extend(rest);
    }

    // Evaluate each position independently against its per-token trailing stop.
    // Collect positions that trip their stop; update peak-water marks for those
    // that don't. State is saved once at the end.
    let positions_snapshot = state.positions.clone();
    let mut outcomes: Vec<TradeOutcome> = Vec::new();
    let mut peak_updates: Vec<(String, f64)> = Vec::new(); // (mint, new_peak)
    let mut to_exit: Vec<(usize, String)> = Vec::new(); // (index, exit_reason)

    for (idx, pos) in positions_snapshot.iter().enumerate() {
        // Mode-mismatch guard: a paper position must never be acted on in live mode
        // (it would try to sell tokens never bought) and vice-versa.
        if pos.dry_run != cfg.momentum_dry_run {
            audit(cfg, ts, ActionKind::ModeMismatch {
                position_dry_run: pos.dry_run,
                config_dry_run: cfg.momentum_dry_run,
            });
            error!(
                "momentum: open position {} dry_run={} but DRY_RUN_MOMENTUM_TRADER={} — refusing to trade. \
                 Be FLAT (or delete {}) before switching modes.",
                pos.symbol, pos.dry_run, cfg.momentum_dry_run, cfg.momentum_state_path
            );
            continue;
        }

        let price = prices_map.get(&pos.mint).copied().filter(|p| *p > 0.0);
        let Some(price) = price else {
            // Never trip the stop on missing/zero price data.
            continue;
        };

        // Update the high-water mark (persisted below in one save).
        if price > pos.peak_price_usd {
            peak_updates.push((pos.mint.clone(), price));
        }

        // Use the per-token trailing stop pct (falls back to global).
        let trail_pct = trail_for(ctx.watched, &pos.mint, cfg.momentum_trail_pct);
        let stop_hit = trailing_stop_triggered(price, pos.peak_price_usd.max(price), trail_pct);
        // Only equities can be "market closed"; 24/7 crypto never flattens on staleness.
        let is_equity = ctx.watched.iter().any(|w| w.mint == pos.mint && w.is_equity());
        let market_closed = is_equity
            && cfg.momentum_stale_minutes > 0
            && is_stale_ts(&price_series_with_ts(ctx.history, &pos.mint), cfg.momentum_stale_minutes);

        // Dwell-confirm the trailing-stop leg only (flag-gated); "market closed" always
        // exits immediately — it's not a wick-prone stop breach. Flag off (or no armed
        // map) ⇒ sell = stop_hit, today's behavior, byte-identical.
        let armed_map = ctx.stop_armed.filter(|_| cfg.momentum_grpc_exit);
        let stop_sell = match armed_map {
            Some(armed) => {
                let now = Instant::now();
                let armed_since = armed.get(&pos.mint).map(|e| *e.value());
                match stop_decision(stop_hit, armed_since, now, cfg.momentum_stop_confirm_secs) {
                    ExitDecision::Arm | ExitDecision::StayArmed => {
                        armed.entry(pos.mint.clone()).or_insert(now);
                        false
                    }
                    ExitDecision::Disarm => {
                        armed.remove(&pos.mint);
                        false
                    }
                    ExitDecision::Sell => {
                        armed.remove(&pos.mint);
                        true
                    }
                    ExitDecision::Hold => false,
                }
            }
            None => stop_hit, // flag off ⇒ immediate, today's behavior
        };

        if stop_sell || market_closed {
            let exit_reason = if stop_sell { "trailing stop" } else { "market closed" };
            to_exit.push((idx, exit_reason.to_string()));
        }
    }

    // Apply peak-water-mark updates (before exits so flattened positions keep the right peak).
    for (mint, new_peak) in &peak_updates {
        if let Some(p) = state.positions.iter_mut().find(|p| &p.mint == mint) {
            p.peak_price_usd = *new_peak;
        }
    }

    // Process exits: flatten each tripped position, accumulate outcomes.
    // We iterate by index descending so removal doesn't shift earlier indices.
    // Collect exit data first (pos clone + price) then execute.
    let mut exit_jobs: Vec<(Position, f64, String)> = Vec::new();
    for (idx, reason) in &to_exit {
        if let Some(pos) = positions_snapshot.get(*idx) {
            let price = prices_map.get(&pos.mint).copied().unwrap_or(0.0);
            exit_jobs.push((pos.clone(), price, reason.clone()));
        }
    }

    for (pos, price, exit_reason) in exit_jobs {
        // flatten_position mutates state internally (removes position, records trade, saves).
        // We call it one-at-a-time; each call re-reads the current state's positions list.
        // After flatten_position we accumulate the outcome if Some.
        match flatten_position(ctx, &mut state, state_path, pos, price, &exit_reason, ts).await? {
            Some(outcome) => outcomes.push(outcome),
            None => {} // flatten returned None (e.g. missing decimals, balance zero, revert) — stop stays armed
        }
    }

    // If no exits happened but we had peak updates, persist the updated peaks.
    if to_exit.is_empty() && !peak_updates.is_empty() {
        momentum_state::save(state_path, &state)?;
    }

    // Eviction (rotate out the lowest-scoring green held position when at capacity and
    // rotate_margin > 0) runs on the slow 60s tick via `maybe_evict`, called by the
    // watcher between `maybe_exit` and `maybe_enter` (Task 5 wires the call).
    // It is NOT called here: `maybe_exit` is the fast-tick trailing-stop loop and does
    // not have ranked candidates, preserving the exits → eviction → entries ordering.

    Ok(outcomes)
}

/// Sell the whole held position back to USDC and record it: on-chain balance fetch
/// (live), `/quote`, submit+confirm, trade record, realized-PnL summary + loss
/// breaker, audit, and email. Shared by every exit path — `exit_reason`
/// (`trailing stop` / `market closed` / `momentum faded`) is logged and audited so
/// the close is attributable. `price` is the exit mark used for the trade record.
/// Unconditional: no cost gate on exit (never stay stuck holding because slippage is
/// high).
async fn flatten_position(
    ctx: &MomentumContext<'_>,
    state: &mut momentum_state::TraderState,
    state_path: &Path,
    pos: Position,
    price: f64,
    exit_reason: &str,
    ts: i64,
) -> Result<Option<TradeOutcome>> {
    let cfg = ctx.cfg;
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
                state.positions.retain(|p| p.mint != pos.mint);
                state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
                state.exit_attempts_per_mint.remove(&pos.mint); // position gone — reset escalation
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
    // The exit is unconditional, so the min-out cushion self-escalates off the
    // consecutive-failure count: a revert on a volatile token (0x1771) widens the
    // next attempt rather than wedging the position. First try stays at base.
    let exit_attempt = state.exit_attempts_per_mint.get(&pos.mint).copied().unwrap_or(0);
    let exit_slippage_bps = escalated_slippage_bps(
        cfg.momentum_slippage_bps,
        exit_attempt,
        cfg.momentum_exit_slippage_cap_bps,
    );
    let quote = match jupiter::quote(
        ctx.http,
        &cfg.momentum_jupiter_api_url,
        &pos.mint,
        USDC_MINT,
        token_raw,
        exit_slippage_bps,
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
        match submit_and_confirm(cfg, ctx.http, &quote).await {
            Ok((s, confirmed)) => {
                if !confirmed {
                    warn!("momentum: EXIT {} submitted but not confirmed in {}s (tx={s})", pos.symbol, CONFIRM_TIMEOUT.as_secs());
                }
                s.to_string()
            }
            Err(e) => {
                // Unconditional exit: a revert (typically 0x1771 slippage) must not
                // wedge the position. Bump the consecutive-failure count — which
                // widens the next attempt's tolerance — persist it, and stay armed.
                let attempt = state.exit_attempts_per_mint.entry(pos.mint.clone()).or_insert(0);
                *attempt += 1;
                let next_bps = escalated_slippage_bps(
                    cfg.momentum_slippage_bps, *attempt, cfg.momentum_exit_slippage_cap_bps,
                );
                warn!(
                    "momentum: EXIT {} reverted at {} bps (attempt {}) — {e:#}; staying armed, re-quoting at {} bps next tick",
                    pos.symbol, exit_slippage_bps, *attempt, next_bps,
                );
                audit(cfg, ts, ActionKind::ExitReverted {
                    symbol: pos.symbol.clone(),
                    attempt: *attempt,
                    slippage_bps: exit_slippage_bps,
                    next_slippage_bps: next_bps,
                    reason: format!("{e:#}"),
                });
                momentum_state::save(state_path, &state)?;
                return Ok(None); // retry next tick at the wider tolerance; stop stays armed
            }
        }
    };

    let rec = build_trade_record(&pos, ts, price, net_usdc, sig.clone());
    state.trades.push(rec.clone());
    state.last_exit_ts_per_mint.insert(pos.mint.clone(), ts);
    state.exit_attempts_per_mint.remove(&pos.mint); // exit landed — reset escalation
    state.positions.retain(|p| p.mint != pos.mint);
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
        reason: exit_reason.to_string(),
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

/// Dwell-based wick-confirmation: a stop must stay breached for `confirm_secs`
/// before selling, so a single-block on-chain price wick that reverts doesn't
/// whipsaw the position out. `confirm_secs == 0` ⇒ sell immediately on breach
/// (dwell disabled — today's behavior). `armed_since` is when the breach began.
pub fn stop_decision(
    stop_hit: bool,
    armed_since: Option<std::time::Instant>,
    now: std::time::Instant,
    confirm_secs: u64,
) -> ExitDecision {
    match (stop_hit, armed_since) {
        (true, None) => {
            if confirm_secs == 0 { ExitDecision::Sell } else { ExitDecision::Arm }
        }
        (true, Some(since)) => {
            if now.duration_since(since).as_secs() >= confirm_secs {
                ExitDecision::Sell
            } else {
                ExitDecision::StayArmed
            }
        }
        (false, Some(_)) => ExitDecision::Disarm,
        (false, None) => ExitDecision::Hold,
    }
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
    fn regime_mode_parses_and_defaults_to_level() {
        use std::str::FromStr;
        assert_eq!(RegimeMode::from_str("off").unwrap(), RegimeMode::Off);
        assert_eq!(RegimeMode::from_str("Level").unwrap(), RegimeMode::Level);
        assert_eq!(RegimeMode::from_str("TREND").unwrap(), RegimeMode::Trend);
        assert_eq!(RegimeMode::from_str("slope_r2").unwrap(), RegimeMode::Trend);
        assert!(RegimeMode::from_str("bogus").is_err());
        assert_eq!(RegimeMode::default(), RegimeMode::Level, "back-compat with existing configs");
    }

    #[test]
    fn regime_risk_on_dispatches_by_mode() {
        let up: VecDeque<PriceSnapshot> = (0..200u64)
            .map(|i| snap(1000 + i * 180, "SOL", 100.0 * 1.002_f64.powi(i as i32)))
            .collect();
        // off → always risk-on, no diagnostic logged.
        assert_eq!(regime_risk_on(&up, RegimeMode::Off, 150, 0.0), (true, None));
        // level → latest SOL above its MA → risk-on, with a diagnostic.
        let (ok, diag) = regime_risk_on(&up, RegimeMode::Level, 150, 0.0);
        assert!(ok && diag.is_some());
        // trend → clean uptrend clears a 0 threshold → risk-on.
        assert!(regime_risk_on(&up, RegimeMode::Trend, 150, 0.0).0);
        // trend → unreachable threshold → risk-off (still logs the evidence).
        let (ok, diag) = regime_risk_on(&up, RegimeMode::Trend, 150, 1e9);
        assert!(!ok && diag.is_some());
        // SOL downtrend → trend gate risk-off at a 0 threshold.
        let down: VecDeque<PriceSnapshot> = (0..200u64)
            .map(|i| snap(1000 + i * 180, "SOL", 100.0 * 0.998_f64.powi(i as i32)))
            .collect();
        assert!(!regime_risk_on(&down, RegimeMode::Trend, 150, 0.0).0, "downtrend → risk-off");
        // obs = 0 → gate off (risk-on, no diagnostic) regardless of mode.
        assert_eq!(regime_risk_on(&up, RegimeMode::Trend, 0, 0.0), (true, None));
    }

    #[test]
    fn macro_blackout_gates_the_windows_around_an_event() {
        let events = vec![MacroEvent { name: "CPI May".into(), ts: 1_000_000 }];
        // Inside the 24h window before the event → blocked (boundary inclusive).
        assert!(macro_blackout(&events, 1_000_000 - 3_600, 24.0, 0.0).is_some());
        assert!(macro_blackout(&events, 1_000_000 - 24 * 3_600, 24.0, 0.0).is_some());
        assert!(macro_blackout(&events, 1_000_000, 24.0, 0.0).is_some());
        // No after-window → free right after the release; earlier than before-window → free.
        assert!(macro_blackout(&events, 1_000_001, 24.0, 0.0).is_none());
        assert!(macro_blackout(&events, 1_000_000 - 24 * 3_600 - 1, 24.0, 0.0).is_none());
        // After-window: blocked through +48h, free past it; works with before = 0 too.
        assert!(macro_blackout(&events, 1_000_000 + 47 * 3_600, 24.0, 48.0).is_some());
        assert!(macro_blackout(&events, 1_000_000 + 48 * 3_600 + 1, 24.0, 48.0).is_none());
        assert!(macro_blackout(&events, 1_000_000 + 3_600, 0.0, 2.0).is_some());
        // Both windows <= 0 disable the gate entirely.
        assert!(macro_blackout(&events, 1_000_000 - 3_600, 0.0, 0.0).is_none());
    }

    #[test]
    fn entry_dip_z_detects_oversold() {
        let rising: VecDeque<PriceSnapshot> =
            (0..40u64).map(|i| snap(i, "AAA", 100.0 + i as f64)).collect();
        assert!(entry_dip_z(&rising, "AAA", 40).unwrap() > 0.0, "rising → at highs, not oversold");
        let mut dip: VecDeque<PriceSnapshot> = (0..39u64).map(|i| snap(i, "AAA", 100.0)).collect();
        dip.push_back(snap(39, "AAA", 90.0));
        assert!(entry_dip_z(&dip, "AAA", 40).unwrap() < 0.0, "sharp dip → oversold (negative z)");
        assert!(entry_dip_z(&dip, "AAA", 10).is_none(), "too few obs → None");
    }

    #[test]
    fn entry_overbought_blocks_only_extended_tokens() {
        // Rising to the highs → z well above the mean → overbought gate blocks.
        let rising: VecDeque<PriceSnapshot> =
            (0..40u64).map(|i| snap(i, "AAA", 100.0 + i as f64)).collect();
        assert!(entry_overbought(&rising, "AAA", 40, 1.0), "at highs (z>1σ) → blocked");
        assert!(!entry_overbought(&rising, "AAA", 40, 5.0), "5σ ceiling → admitted (threshold-directional)");
        assert!(!entry_overbought(&rising, "AAA", 0, 1.0), "obs=0 → gate disabled, never blocks");
        // A dip below the mean is the opposite of overbought → never blocked.
        let mut dip: VecDeque<PriceSnapshot> = (0..39u64).map(|i| snap(i, "AAA", 100.0)).collect();
        dip.push_back(snap(39, "AAA", 90.0));
        assert!(!entry_overbought(&dip, "AAA", 40, 1.0), "oversold dip → admitted, not blocked");
        // Warming series (too few obs → entry_dip_z None) never blocks.
        assert!(!entry_overbought(&dip, "AAA", 10, 1.0), "warming (None) → never blocks");
    }

    #[test]
    fn sol_risk_on_gates_on_sol_trend() {
        let mk = |sol: f64| snap(0, SOL_KEY, sol);
        let rising: VecDeque<PriceSnapshot> = (0..10).map(|i| mk(100.0 + i as f64)).collect();
        assert!(sol_risk_on(&rising, 0), "ma_obs=0 disables → always risk-on");
        assert!(sol_risk_on(&rising, 5), "rising SOL → above prior mean → risk-on");
        let falling: VecDeque<PriceSnapshot> = (0..10).map(|i| mk(100.0 - i as f64)).collect();
        assert!(!sol_risk_on(&falling, 5), "falling SOL → risk-off");
        let short: VecDeque<PriceSnapshot> = vec![mk(100.0), mk(101.0)].into();
        assert!(sol_risk_on(&short, 5), "too little history → not gated");
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
    fn vol_stop_off_matches_fixed_trail() {
        // Off mode must be byte-for-byte the fixed-% stop, regardless of k / atr / sigma.
        for &(px, peak, trail) in &[(92.0, 100.0, 8.0), (92.01, 100.0, 8.0), (80.0, 100.0, 8.0)] {
            assert_eq!(
                vol_stop_triggered(
                    px,
                    peak,
                    trail,
                    VolStopMode::Off,
                    3.0,
                    Some(1.0),
                    Some(0.02)
                ),
                trailing_stop_triggered(px, peak, trail),
                "Off must equal the fixed-% stop at px={px}"
            );
        }
    }

    #[test]
    fn vol_stop_atr_and_sigma_widths() {
        // ATR: stop = peak − k·ATR = 100 − 3·2 = 94.0.
        assert!(vol_stop_triggered(
            94.0,
            100.0,
            8.0,
            VolStopMode::Atr,
            3.0,
            Some(2.0),
            None
        ));
        assert!(!vol_stop_triggered(
            94.01,
            100.0,
            8.0,
            VolStopMode::Atr,
            3.0,
            Some(2.0),
            None
        ));
        // Sigma: eff% = k·σ·100 = 5·0.02·100 = 10% → stop at 90.0.
        assert!(vol_stop_triggered(
            90.0,
            100.0,
            8.0,
            VolStopMode::Sigma,
            5.0,
            None,
            Some(0.02)
        ));
        assert!(!vol_stop_triggered(
            90.01,
            100.0,
            8.0,
            VolStopMode::Sigma,
            5.0,
            None,
            Some(0.02)
        ));
        // Warmup (vol = None) or k == 0 → fall back to the fixed 8% stop (92.0).
        assert!(vol_stop_triggered(
            92.0,
            100.0,
            8.0,
            VolStopMode::Atr,
            3.0,
            None,
            None
        ));
        assert!(vol_stop_triggered(
            92.0,
            100.0,
            8.0,
            VolStopMode::Sigma,
            0.0,
            None,
            Some(0.02)
        ));
        // A non-positive peak never fires, whatever the mode.
        assert!(!vol_stop_triggered(
            50.0,
            0.0,
            8.0,
            VolStopMode::Atr,
            3.0,
            Some(2.0),
            None
        ));
    }

    #[test]
    fn profit_protected_stop_disabled_matches_fallback() {
        // max_trail_pct <= 0 → returns the fallback verbatim (today's behavior).
        for &fb in &[true, false] {
            assert_eq!(
                profit_protected_stop_triggered(120.0, 150.0, 100.0, 0.01, 0.0, fb),
                fb
            );
        }
    }

    #[test]
    fn profit_protected_stop_caps_giveback_and_floors_at_breakeven() {
        // entry 100, round-trip cost 1% → floor = 101.
        let c = 0.01;
        // Big winner (peak 150), max_trail 20% → give_back 120 > floor → exit at 120.
        assert!(!profit_protected_stop_triggered(121.0, 150.0, 100.0, c, 20.0, false));
        assert!(profit_protected_stop_triggered(120.0, 150.0, 100.0, c, 20.0, false));
        // Same peak, wide max_trail 40% → give_back 90 < floor → floored at breakeven 101.
        assert!(!profit_protected_stop_triggered(101.5, 150.0, 100.0, c, 40.0, false));
        assert!(profit_protected_stop_triggered(101.0, 150.0, 100.0, c, 40.0, false));
        // While green, a tight fallback stop is IGNORED — the position rides the pullback.
        assert!(
            !profit_protected_stop_triggered(130.0, 150.0, 100.0, c, 20.0, true),
            "green position rides past the fallback stop until the capped give-back level"
        );
        // Not yet green (peak below the cost floor) → defer to the fallback stop-loss.
        assert!(profit_protected_stop_triggered(100.0, 100.5, 100.0, c, 20.0, true));
        assert!(!profit_protected_stop_triggered(100.0, 100.5, 100.0, c, 20.0, false));
    }

    #[test]
    fn dynamic_trade_usdc_compounds_banked_profit() {
        let base = 100.0;
        // Disabled (frac 0) → always base, regardless of profit.
        assert_eq!(dynamic_trade_usdc(base, 0.0, 500.0, 800.0), base);
        // No/negative realized profit → floored at base.
        assert_eq!(dynamic_trade_usdc(base, 0.5, 500.0, 0.0), base);
        assert_eq!(dynamic_trade_usdc(base, 0.5, 500.0, -250.0), base);
        // Compounds: 100 + 0.5·300 = 250.
        assert_eq!(dynamic_trade_usdc(base, 0.5, 500.0, 300.0), 250.0);
        // Clamped at the ceiling: 100 + 0.5·800 = 500 cap.
        assert_eq!(dynamic_trade_usdc(base, 0.5, 500.0, 800.0), 500.0);
        // Ceiling below base is a no-op fail-safe (never shrinks below base).
        assert_eq!(dynamic_trade_usdc(base, 1.0, 50.0, 1000.0), base);
    }

    #[test]
    fn fade_take_profit_needs_faded_and_green() {
        // min score 0.5, entry $10.
        assert!(fade_take_profit(0.4, 0.5, 11.0, 10.0), "faded (≤min) + green ⇒ take profit");
        assert!(fade_take_profit(0.5, 0.5, 11.0, 10.0), "score exactly at min counts as faded");
        assert!(!fade_take_profit(0.6, 0.5, 11.0, 10.0), "momentum still alive ⇒ hold");
        assert!(!fade_take_profit(0.4, 0.5, 10.0, 10.0), "flat (not green) ⇒ trailing stop owns it");
        assert!(!fade_take_profit(0.4, 0.5, 9.0, 10.0), "underwater ⇒ hold for trailing stop");
    }

    #[test]
    fn rotation_net_green_clears_cost() {
        // entry $10; cost 100 bps (1%) ⇒ breakeven $10.10.
        assert!(rotation_net_green(10.20, 10.0, 100), "2% gain clears 1% cost");
        assert!(!rotation_net_green(10.10, 10.0, 100), "gain exactly = cost does not clear");
        assert!(!rotation_net_green(10.05, 10.0, 100), "0.5% gain < 1% cost ⇒ hold");
        assert!(!rotation_net_green(9.0, 10.0, 100), "underwater ⇒ hold");
        // Zero cost reduces to a plain gross-green check.
        assert!(rotation_net_green(10.01, 10.0, 0), "any gain passes at zero cost");
        assert!(!rotation_net_green(10.0, 10.0, 0), "flat fails even at zero cost");
    }

    #[test]
    fn quote_divergence_bps_math_and_guards() {
        assert_eq!(quote_divergence_bps(101.0, 100.0), Some(100));
        assert_eq!(quote_divergence_bps(99.0, 100.0), Some(100));
        assert_eq!(quote_divergence_bps(100.0, 100.0), Some(0));
        assert_eq!(quote_divergence_bps(0.0, 100.0), None);
        assert_eq!(quote_divergence_bps(100.0, f64::NAN), None);
    }

    #[test]
    fn trusted_grpc_price_gates_on_presence_freshness_and_trust() {
        use crate::portfolio::grpc_pricer::GrpcFeed;
        let feed = GrpcFeed::new();
        let now = Instant::now();
        feed.map.insert("FRESH".to_string(), (1.5, now));
        feed.map.insert("STALE".to_string(), (2.0, now - Duration::from_secs(120)));
        feed.map.insert("ZERO".to_string(), (0.0, now));
        feed.map.insert("DISTRUSTED".to_string(), (3.0, now));
        feed.record_xcheck("DISTRUSTED", false, now);

        // No feed at all (MOMENTUM_GRPC_PRICING off / MOMENTUM_GRPC_EXIT off) → None.
        assert_eq!(trusted_grpc_price(None, "FRESH", 30), None);
        // Present, positive, fresh (age 0 ≤ 30s TTL) → trusted.
        assert_eq!(trusted_grpc_price(Some(&feed), "FRESH", 30), Some(1.5));
        // Older than the TTL → None.
        assert_eq!(trusted_grpc_price(Some(&feed), "STALE", 30), None);
        // stale_secs=0 (trust-until-changed) → age never demotes it.
        assert_eq!(trusted_grpc_price(Some(&feed), "STALE", 0), Some(2.0));
        // Non-positive price → None.
        assert_eq!(trusted_grpc_price(Some(&feed), "ZERO", 30), None);
        // Distrusted by the REST cross-check → None even though fresh/positive.
        assert_eq!(trusted_grpc_price(Some(&feed), "DISTRUSTED", 30), None);
        // Mint absent from the feed → None.
        assert_eq!(trusted_grpc_price(Some(&feed), "MISSING", 30), None);
    }

    #[test]
    fn entry_retry_due_requires_feature_record_and_deadline() {
        let rec = |count: u32, next_retry_ts: i64| Some(momentum_state::EntryAttempt {
            mint: "X".into(),
            count,
            next_retry_ts,
        });
        // Feature off (retry_secs = 0) → never due, even with a stamped deadline.
        assert!(!entry_retry_due(&rec(1, 100), 0, 200));
        // No pending record → nothing to retry.
        assert!(!entry_retry_due(&None, 10, 200));
        // Legacy/unstamped record (deadline 0, e.g. reverted while the feature was
        // off) → not due; it retries on the slow tick as before.
        assert!(!entry_retry_due(&rec(1, 0), 10, 200));
        // Deadline in the future → not due yet.
        assert!(!entry_retry_due(&rec(1, 300), 10, 200));
        // Deadline reached or passed → due.
        assert!(entry_retry_due(&rec(1, 200), 10, 200));
        assert!(entry_retry_due(&rec(3, 150), 10, 200));
    }

    #[test]
    fn exit_slippage_escalates_geometrically_and_caps() {
        // First attempt stays tight at the configured base.
        assert_eq!(escalated_slippage_bps(50, 0, 800), 50);
        // Each consecutive revert doubles the tolerance …
        assert_eq!(escalated_slippage_bps(50, 1, 800), 100);
        assert_eq!(escalated_slippage_bps(50, 2, 800), 200);
        assert_eq!(escalated_slippage_bps(50, 3, 800), 400);
        // … until it would exceed the cap, then it pins to the cap.
        assert_eq!(escalated_slippage_bps(50, 5, 800), 800, "1600 clamps to cap");
        // Large attempt counts saturate at the cap (no overflow, never wider).
        assert_eq!(escalated_slippage_bps(50, 30, 800), 800);
        // The 10→20 entry ladder: base 10, entry cap 20 → 10, then 20, then stays 20.
        assert_eq!(escalated_slippage_bps(10, 0, 20), 10);
        assert_eq!(escalated_slippage_bps(10, 1, 20), 20);
        assert_eq!(escalated_slippage_bps(10, 4, 20), 20);
        // Tolerance is monotonic non-decreasing and never drops below base.
        let mut prev = 0;
        for n in 0..12 {
            let s = escalated_slippage_bps(50, n, 800);
            assert!(s >= prev && s >= 50, "attempt {n}: {s} >= {prev} and >= base");
            prev = s;
        }
    }

    #[test]
    fn entry_step_amounts_splits_exactly_with_remainder_in_last() {
        // steps 0/1 → the original single-swap path, full notional untouched.
        assert_eq!(entry_step_amounts(50_000_000, 0), vec![50_000_000]);
        assert_eq!(entry_step_amounts(50_000_000, 1), vec![50_000_000]);
        // Exact division → equal tranches.
        assert_eq!(entry_step_amounts(50_000_000, 5), vec![10_000_000; 5]);
        // Remainder folds into the LAST tranche; the sum is exactly the total.
        assert_eq!(
            entry_step_amounts(50_000_000, 3),
            vec![16_666_666, 16_666_666, 16_666_668]
        );
        // Notional too small for one raw unit per tranche → single full tranche,
        // never zero-amount tranches.
        assert_eq!(entry_step_amounts(3, 5), vec![3]);
        // Degenerate zero notional (guarded upstream by the size/balance checks).
        assert_eq!(entry_step_amounts(0, 4), vec![0]);
        // Property sweep: sums are exact, tranche counts honor `steps` when the
        // notional is big enough, and tranches never differ by more than the
        // largest possible remainder (steps − 1).
        for &total in &[1u64, 7, 99, 100_000_000, 123_456_789] {
            for steps in 2..=MAX_ENTRY_STEPS {
                let v = entry_step_amounts(total, steps);
                assert_eq!(v.iter().sum::<u64>(), total, "Σ == total for {total}/{steps}");
                if total >= steps as u64 {
                    assert_eq!(v.len(), steps as usize, "len == steps for {total}/{steps}");
                    let (min, max) = (*v.iter().min().unwrap(), *v.iter().max().unwrap());
                    assert!(min > 0, "no zero tranches for {total}/{steps}");
                    assert!(max - min <= steps as u64 - 1, "near-equal split for {total}/{steps}");
                } else {
                    assert_eq!(v, vec![total]);
                }
            }
        }
        // No overflow at the extreme: the tranches reconstruct the total exactly.
        assert_eq!(entry_step_amounts(u64::MAX, 3).iter().sum::<u64>(), u64::MAX);
    }

    #[test]
    fn effective_entry_steps_defaults_and_clamps() {
        // Unset / 0 / 1 all mean the original single-swap entry.
        assert_eq!(effective_entry_steps(None), 1);
        assert_eq!(effective_entry_steps(Some(0)), 1);
        assert_eq!(effective_entry_steps(Some(1)), 1);
        // Real staging passes through …
        assert_eq!(effective_entry_steps(Some(2)), 2);
        assert_eq!(effective_entry_steps(Some(MAX_ENTRY_STEPS)), MAX_ENTRY_STEPS);
        // … and anything wilder clamps to the ceiling (bounds the exit-stall window).
        assert_eq!(effective_entry_steps(Some(MAX_ENTRY_STEPS + 1)), MAX_ENTRY_STEPS);
        assert_eq!(effective_entry_steps(Some(u32::MAX)), MAX_ENTRY_STEPS);
    }

    #[test]
    fn staged_entry_gas_bps_uses_tranche_denominator() {
        // Why the staged cost gate charges gas against the TRANCHE notional:
        // est_gas_bps truncates to u32, so the full-size form floors to 0 at
        // typical sizes and `0 × steps` would erase the gas term entirely. The
        // tranche denominator is algebraically steps × gas / total notional,
        // truncated ONCE at the end. $50 across 5 × $10 tranches at SOL=$150:
        // 0.45 bps full-size (→ 0) vs 2.25 bps per tranche (→ 2).
        assert_eq!(est_gas_bps(50.0, 150.0), 0, "full-size bps truncates to zero");
        assert_eq!(est_gas_bps(50.0 / 5.0, 150.0), 2, "per-tranche bps survives truncation");
    }

    #[test]
    fn metric_fading_detects_a_rolling_over_trend() {
        // lookback=121, lag=10 → a 131-point (ts, price) series. The bulk rises at
        // 1%/step; the final `lag` steps either keep that pace or flatten to 0.1%.
        let lookback = 121usize;
        let lag = 10usize;
        let mk = |fast_tail: bool| -> Vec<(u64, f64)> {
            let n = lookback + lag;
            let mut v = Vec::with_capacity(n);
            let mut p = 100.0f64;
            for i in 0..n {
                if i > 0 {
                    let rate = if i < lookback || fast_tail { 1.01 } else { 1.001 };
                    p *= rate;
                }
                v.push((i as u64 * 60, p));
            }
            v
        };
        let score = |s: &[(u64, f64)]| {
            compute_metrics(&s[s.len() - lookback..]).unwrap().select(RankMetric::Return)
        };

        // Tail flattens → the recent window is weaker than 10 obs ago → fading.
        let decel = mk(false);
        assert!(metric_is_fading(&decel, lookback, lag, RankMetric::Return, score(&decel)),
            "metric weaker than `lag` obs ago → fading");

        // Constant rate → metric flat across the lag → not fading.
        let steady = mk(true);
        assert!(!metric_is_fading(&steady, lookback, lag, RankMetric::Return, score(&steady)),
            "constant-rate trend → not fading");

        // lag = 0 disables the guard, even on the decelerating series.
        assert!(!metric_is_fading(&decel, lookback, 0, RankMetric::Return, score(&decel)),
            "lag 0 disables the guard");

        // Not enough history to form the lagged window → unconfirmed → treated as fading.
        let short = &decel[..lookback + 5];
        assert!(metric_is_fading(short, lookback, lag, RankMetric::Return, score(short)),
            "insufficient history to confirm a trajectory → fading");
    }

    #[test]
    fn entry_attempt_resets_when_candidate_changes() {
        use momentum_state::EntryAttempt;
        // No prior record → first attempt.
        assert_eq!(entry_attempt_for(&None, "JUP"), 0);
        // Same candidate as the prior failure → carry the count (escalate).
        let prior = Some(EntryAttempt { mint: "JUP".into(), count: 2, next_retry_ts: 0 });
        assert_eq!(entry_attempt_for(&prior, "JUP"), 2);
        // Best candidate changed → reset; don't inherit JUP's wide tolerance for BP.
        assert_eq!(entry_attempt_for(&prior, "BP"), 0);
    }

    #[test]
    fn ln_price_slope_signs_track_direction() {
        // Rising ln-price → positive slope; falling → negative; flat → ~0.
        let up: Vec<(u64, f64)> = (0..10).map(|i| (i * 60, 100.0 + i as f64)).collect();
        let down: Vec<(u64, f64)> = (0..10).map(|i| (i * 60, 100.0 - i as f64)).collect();
        assert!(ln_price_slope(&up).unwrap() > 0.0);
        assert!(ln_price_slope(&down).unwrap() < 0.0);
        assert!(ln_price_slope(&[(0, 100.0), (60, 100.0)]).unwrap().abs() < 1e-12);
        assert!(ln_price_slope(&[(0, 100.0)]).is_none(), "needs ≥2 points");
    }

    #[test]
    fn overextension_skips_only_decelerating_big_runs() {
        let big = 1.10_f64.ln(); // +10% window run (Σ log-returns)
        let small = 1.02_f64.ln(); // +2% run
        let max_run = 6.0;
        // Big run + decelerating (recent slope < full, here recent gone negative):
        // the ZINC#9 −4.15% top — skip. Slopes are the real values from price_history.
        assert!(is_overextended(big, max_run, Some(-1.6e-5), Some(8.6e-6)), "topping big run → skip");
        // Big run + still accelerating (recent steeper than full): ZINC mid-breakout
        // (run +15.6% but recent slope 6× the full) — keep, don't veto a runner.
        assert!(!is_overextended(big, max_run, Some(1.27e-4), Some(2.0e-5)), "accelerating big run → keep");
        // Small run → never over-extended, whatever the slope.
        assert!(!is_overextended(small, max_run, Some(-1.0), Some(1.0)), "small run is fine");
        // No slope info (decel check disabled / too little data) → conservative run cap.
        assert!(is_overextended(big, max_run, None, None), "no trend info → pure run cap");
        // Threshold 0 disables entirely.
        assert!(!is_overextended(big, 0.0, Some(-1.0), Some(1.0)), "max_run 0 disables");
        // Around the run threshold (decelerating so the slope branch is live):
        assert!(!is_overextended(1.059_f64.ln(), 6.0, Some(0.0), Some(1.0)), "+5.9% under cap");
        assert!(is_overextended(1.061_f64.ln(), 6.0, Some(0.0), Some(1.0)), "+6.1% over cap + decel");
    }

    #[test]
    fn snapshot_tokens_mirrors_panel_states() {
        let mk = |sym: &str, mint: &str, stale: bool| Candidate {
            symbol: sym.into(),
            mint: mint.into(),
            score: 1.0,
            metrics: Metrics { sortino: 0.1, sharpe: 0.2, slope_r2: 3.0, ret: 0.05 },
            price_usd: 1.0,
            obs: 200,
            stale,
            overextended: false,
            falling: false,
            metric_fading: false,
            slope_recent: None,
            slope_full: None,
        };
        // A scored token, a stale (closed) token, and a watched-but-unranked (warming) one.
        let ranked = vec![mk("AAA", "A", false), mk("BBB", "B", true)];
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "A".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
            WatchedToken { symbol: "BBB".into(), mint: "B".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
            WatchedToken { symbol: "CCC".into(), mint: "C".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
        ];
        let snap = snapshot_tokens(&watched, &ranked);
        assert_eq!(snap.len(), 3);
        // best-first ranked rows first, warming appended last — same order as log_rank_line.
        assert_eq!(snap[0].symbol, "AAA");
        assert!(matches!(snap[0].state, TokenState::Scored { .. }));
        assert_eq!(snap[1].symbol, "BBB");
        assert!(matches!(snap[1].state, TokenState::Closed));
        assert_eq!(snap[2].symbol, "CCC");
        assert!(matches!(snap[2].state, TokenState::Warming));
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
            WatchedToken { symbol: "AAA".into(), mint: "A".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
            WatchedToken { symbol: "BBB".into(), mint: "B".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
        ];
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), a);
        prices.insert("B".to_string(), b);
        let ranked = rank_candidates(&watched, &prices, &h, 1440, 0, RankMetric::Sortino, 0.0, 0, 0);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].mint, "A", "rising token ranks first");
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn rank_flags_falling_token() {
        // F: rises for most of the window, then drops over the last ~10 min.
        // R: rises throughout. ts spaced 60s so "last 10 min" ≈ last 10 points.
        let mut h = VecDeque::new();
        let n = 130u64;
        let (mut f, mut r) = (0.0, 0.0);
        for i in 0..n {
            f = if i < n - 10 { 100.0 + i as f64 } else { 100.0 + (n - 10) as f64 - (i - (n - 10)) as f64 * 3.0 };
            r = 100.0 + i as f64;
            let mut prices = HashMap::new();
            prices.insert("F".to_string(), f);
            prices.insert("R".to_string(), r);
            h.push_back(PriceSnapshot { ts: i * 60, prices });
        }
        let watched = vec![
            WatchedToken { symbol: "FFF".into(), mint: "F".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
            WatchedToken { symbol: "RRR".into(), mint: "R".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None },
        ];
        let mut prices = HashMap::new();
        prices.insert("F".to_string(), f);
        prices.insert("R".to_string(), r);
        // decel window = 10 min → the falling gate is live.
        let ranked = rank_candidates(&watched, &prices, &h, 1440, 0, RankMetric::Sortino, 0.0, 10, 0);
        let cf = ranked.iter().find(|c| c.mint == "F").expect("F ranked");
        let cr = ranked.iter().find(|c| c.mint == "R").expect("R ranked");
        assert!(cf.falling, "F dropped over the last 10 min → falling");
        assert!(!cr.falling, "R rose throughout → not falling");
        // decel window 0 disables the gate (recent slope unknown → never flagged).
        let ranked0 = rank_candidates(&watched, &prices, &h, 1440, 0, RankMetric::Sortino, 0.0, 0, 0);
        assert!(ranked0.iter().all(|c| !c.falling), "decel window 0 disables the falling gate");
    }

    #[test]
    fn rank_skips_warmup_tokens() {
        // Only 50 snapshots → < 120 returns → no Sortino → excluded.
        let mut h = VecDeque::new();
        for i in 0..50u64 {
            h.push_back(snap(i, "A", 100.0 + i as f64));
        }
        let watched = vec![WatchedToken { symbol: "AAA".into(), mint: "A".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None }];
        let mut prices = HashMap::new();
        prices.insert("A".to_string(), 150.0);
        assert!(rank_candidates(&watched, &prices, &h, 1440, 0, RankMetric::Sortino, 0.0, 0, 0).is_empty());
    }

    #[test]
    fn rotation_target_respects_margin_and_gates() {
        let cand = |sym: &str, score: f64, stale: bool, overextended: bool, falling: bool| Candidate {
            symbol: sym.into(),
            mint: sym.into(),
            score,
            // rotation_target reads only `score`; the panel metrics are irrelevant here.
            metrics: Metrics { sortino: score, sharpe: 0.0, slope_r2: 0.0, ret: 0.0 },
            price_usd: 1.0,
            obs: 200,
            stale,
            overextended,
            falling,
            metric_fading: false,
            slope_recent: None,
            slope_full: None,
        };
        // best-first: B=1.0, held A=0.5, C=0.3
        let ranked = vec![cand("B", 1.0, false, false, false), cand("A", 0.5, false, false, false), cand("C", 0.3, false, false, false)];
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
        let stale_ranked = vec![cand("B", 1.0, true, false, false), cand("A", 0.5, false, false, false)];
        assert!(rotation_target(&stale_ranked, "A", 0.5, 0.0, 0.3, 3600, 1000, &no_cd).is_none());
        // over-extended B excluded as a rotation target (this is what blocks MET#2)
        let ox_ranked = vec![cand("B", 1.0, false, true, false), cand("A", 0.5, false, false, false)];
        assert!(rotation_target(&ox_ranked, "A", 0.5, 0.0, 0.3, 3600, 1000, &no_cd).is_none());
        // falling B excluded as a rotation target (never rotate into a dropping token)
        let fall_ranked = vec![cand("B", 1.0, false, false, true), cand("A", 0.5, false, false, false)];
        assert!(rotation_target(&fall_ranked, "A", 0.5, 0.0, 0.3, 3600, 1000, &no_cd).is_none());
        // metric-fading B excluded as a rotation target (never rotate into a rolling-over signal)
        let mut fading_b = cand("B", 1.0, false, false, false);
        fading_b.metric_fading = true;
        let fading_ranked = vec![fading_b, cand("A", 0.5, false, false, false)];
        assert!(rotation_target(&fading_ranked, "A", 0.5, 0.0, 0.3, 3600, 1000, &no_cd).is_none());
    }

    #[test]
    fn choose_adoption_handles_none_one_ambiguous() {
        let c = |sym: &str, amount: f64, price: f64| AdoptCandidate {
            mint: sym.into(), symbol: sym.into(), amount, price_usd: price,
        };
        let min = 500.0;
        // All dust below the floor → adopt nothing.
        assert_eq!(choose_adoption(vec![c("A", 10.0, 1.0), c("B", 100.0, 1.0)], min, 1), Adoption::None);
        // Exactly one big holding → adopt it (dust ignored).
        match choose_adoption(vec![c("BIG", 1000.0, 1.0), c("dust", 5.0, 1.0)], min, 1) {
            Adoption::One(a) => assert_eq!(a.symbol, "BIG"),
            other => panic!("expected One, got {other:?}"),
        }
        // Two big holdings at cap=1 → ambiguous, never guess.
        assert_eq!(choose_adoption(vec![c("X", 600.0, 1.0), c("Y", 30.0, 25.0)], min, 1), Adoption::Ambiguous(2));
        // Empty wallet → None.
        assert_eq!(choose_adoption(vec![], min, 1), Adoption::None);
    }

    #[test]
    fn choose_adoption_multi_slot_adopts_up_to_cap() {
        let c = |sym: &str, amount: f64, price: f64| AdoptCandidate {
            mint: sym.into(), symbol: sym.into(), amount, price_usd: price,
        };
        let min = 500.0;
        // cap=2: two big holdings → Many with both, sorted by value desc (Y=$750 > X=$600).
        match choose_adoption(vec![c("X", 600.0, 1.0), c("Y", 30.0, 25.0)], min, 2) {
            Adoption::Many(cs) => {
                assert_eq!(cs.len(), 2);
                assert_eq!(cs[0].symbol, "Y", "Y is bigger ($750) and should come first");
                assert_eq!(cs[1].symbol, "X");
            }
            other => panic!("expected Many, got {other:?}"),
        }
        // cap=2 but only one qualifies → still One.
        match choose_adoption(vec![c("BIG", 1000.0, 1.0), c("dust", 5.0, 1.0)], min, 2) {
            Adoption::One(a) => assert_eq!(a.symbol, "BIG"),
            other => panic!("expected One, got {other:?}"),
        }
        // cap=1 with 2 qualifiers → Ambiguous (backward-compat).
        assert_eq!(choose_adoption(vec![c("X", 600.0, 1.0), c("Y", 30.0, 25.0)], min, 1), Adoption::Ambiguous(2));
        // cap=2 but 3 qualify → Many capped at 2 (top-2 by value).
        match choose_adoption(vec![c("A", 600.0, 1.0), c("B", 700.0, 1.0), c("C", 800.0, 1.0)], min, 2) {
            Adoption::Many(cs) => {
                assert_eq!(cs.len(), 2, "capped at 2");
                // C=$800 then B=$700 (A=$600 dropped)
                assert_eq!(cs[0].symbol, "C");
                assert_eq!(cs[1].symbol, "B");
            }
            other => panic!("expected Many, got {other:?}"),
        }
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

    #[test]
    fn rank_candidates_exposes_slopes_for_overextension_recompute() {
        // A steadily-rising token has a positive whole-window slope; the stored
        // slope_full must reproduce is_overextended when fed back with the same max_run.
        let mut hist: std::collections::VecDeque<PriceSnapshot> = std::collections::VecDeque::new();
        let mut p = 1.0_f64;
        for i in 0..130u64 {
            let mut m = std::collections::HashMap::new();
            m.insert("AAA".to_string(), p);
            m.insert(SOL_KEY.to_string(), 150.0);
            hist.push_back(PriceSnapshot { ts: 1000 + i * 180, prices: m });
            p *= 1.01;
        }
        let watched = vec![WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None }];
        let prices: std::collections::HashMap<String, f64> =
            [("AAA".to_string(), p)].into_iter().collect();
        let cands = rank_candidates(&watched, &prices, &hist, 121, 0, RankMetric::Return, 6.0, 0, 0);
        let c = cands.iter().find(|c| c.mint == "AAA").expect("AAA ranked");
        // Re-evaluating is_overextended with the stored slopes + same max_run must equal
        // the candidate's precomputed flag.
        let recomputed = is_overextended(c.metrics.ret, 6.0, c.slope_recent, c.slope_full);
        assert_eq!(recomputed, c.overextended, "stored slopes reproduce is_overextended");
        // whole-window slope of a monotone rise is positive
        assert!(c.slope_full.is_some_and(|s| s > 0.0));
    }

    #[test]
    fn per_token_resolvers_override_then_global() {
        let g_min = 0.04_f64;
        let g_trail = 20.0_f64;
        let g_run = 6.0_f64;
        let w_over = WatchedToken {
            symbol: "A".into(),
            mint: "A".into(),
            name: None,
            equity: None,
            params: Some(crate::portfolio::momentum_universe::TokenParams {
                min_metric: Some(0.09),
                trail_pct: Some(30.0),
                ..Default::default()
            }),
            pool: None,
            quote: None,
            pools: None,
        };
        let w_none = WatchedToken {
            symbol: "B".into(),
            mint: "B".into(),
            name: None,
            equity: None,
            params: None,
            pool: None,
            quote: None,
            pools: None,
        };
        let watched = vec![w_over, w_none];
        assert_eq!(min_metric_for(&watched, "A", g_min), 0.09);   // override
        assert_eq!(trail_for(&watched, "A", g_trail), 30.0);       // override
        assert_eq!(max_run_for(&watched, "A", g_run), 6.0);        // field None → global
        assert_eq!(min_metric_for(&watched, "B", g_min), 0.04);    // no params → global
        assert_eq!(trail_for(&watched, "Z", g_trail), 20.0);       // unknown mint → global
    }

    #[test]
    fn extended_resolvers_override_then_global() {
        let w_over = WatchedToken {
            symbol: "A".into(), mint: "A".into(), name: None, equity: None,
            params: Some(crate::portfolio::momentum_universe::TokenParams {
                trade_usdc: Some(250.0),
                exit_on_fade: Some(false),
                reentry_cooldown_secs: Some(1800),
                ..Default::default()
            }),
            pool: None, quote: None, pools: None,
        };
        let w_none = WatchedToken {
            symbol: "B".into(), mint: "B".into(), name: None, equity: None, params: None,
            pool: None, quote: None, pools: None,
        };
        let watched = vec![w_over, w_none];
        // override wins
        assert_eq!(trade_usdc_for(&watched, "A", 100.0), 250.0);
        assert_eq!(exit_on_fade_for(&watched, "A", true), false);
        assert_eq!(reentry_cooldown_for(&watched, "A", 360), 1800);
        // no params → global
        assert_eq!(trade_usdc_for(&watched, "B", 100.0), 100.0);
        assert_eq!(exit_on_fade_for(&watched, "B", true), true);
        assert_eq!(reentry_cooldown_for(&watched, "B", 360), 360);
        // unknown mint → global
        assert_eq!(trade_usdc_for(&watched, "Z", 100.0), 100.0);
        assert_eq!(exit_on_fade_for(&watched, "Z", true), true);
        assert_eq!(reentry_cooldown_for(&watched, "Z", 360), 360);
    }

    fn make_candidate(mint: &str, score: f64) -> Candidate {
        Candidate {
            symbol: mint.into(),
            mint: mint.into(),
            score,
            metrics: Metrics { sortino: score, sharpe: 0.0, slope_r2: 0.0, ret: 0.05 },
            price_usd: 1.0,
            obs: 200,
            stale: false,
            overextended: false,
            falling: false,
            metric_fading: false,
            slope_recent: Some(1e-5),
            slope_full: Some(5e-6),
        }
    }

    #[test]
    fn select_entries_respects_capacity() {
        let watched: Vec<WatchedToken> = vec![];
        let ranked = vec![
            make_candidate("A", 2.0),
            make_candidate("B", 1.5),
            make_candidate("C", 1.0),
        ];
        let no_cd: HashMap<String, i64> = HashMap::new();
        // cap=1 → only one candidate returned (the best)
        let out = select_entries(&ranked, &[], 1, &watched, 0.0, 0.0, &no_cd, 0, 0, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mint, "A");
        // cap=2 → two candidates
        let out2 = select_entries(&ranked, &[], 2, &watched, 0.0, 0.0, &no_cd, 0, 0, true);
        assert_eq!(out2.len(), 2);
        // cap=0 → empty
        let out0 = select_entries(&ranked, &[], 0, &watched, 0.0, 0.0, &no_cd, 0, 0, true);
        assert!(out0.is_empty());
    }

    #[test]
    fn select_entries_skips_held_mints() {
        let watched: Vec<WatchedToken> = vec![];
        let ranked = vec![
            make_candidate("A", 2.0),
            make_candidate("B", 1.5),
            make_candidate("C", 1.0),
        ];
        let no_cd: HashMap<String, i64> = HashMap::new();
        let held = vec!["A".to_string(), "B".to_string()];
        // A and B are held → only C eligible
        let out = select_entries(&ranked, &held, 3, &watched, 0.0, 0.0, &no_cd, 0, 0, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mint, "C");
    }

    #[test]
    fn select_entries_respects_per_token_min_score() {
        // Token A has a per-token override of min_metric=1.8; score=2.0 passes.
        // Token B has no override; global min=1.6; score=1.5 fails.
        let w_a = WatchedToken {
            symbol: "A".into(), mint: "A".into(), name: None, equity: None,
            params: Some(crate::portfolio::momentum_universe::TokenParams {
                min_metric: Some(1.8), ..Default::default()
            }),
            pool: None, quote: None, pools: None,
        };
        let w_b = WatchedToken {
            symbol: "B".into(), mint: "B".into(), name: None, equity: None, params: None,
            pool: None, quote: None, pools: None,
        };
        let watched = vec![w_a, w_b];
        let ranked = vec![make_candidate("A", 2.0), make_candidate("B", 1.5)];
        let no_cd: HashMap<String, i64> = HashMap::new();
        let out = select_entries(&ranked, &[], 2, &watched, 1.6, 0.0, &no_cd, 0, 0, true);
        // A clears its own 1.8 threshold (2.0 > 1.8); B fails global 1.6 (1.5 ≤ 1.6)
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mint, "A");
    }

    #[test]
    fn select_entries_skips_cooldown() {
        let watched: Vec<WatchedToken> = vec![];
        let ranked = vec![make_candidate("A", 2.0), make_candidate("B", 1.5)];
        let mut cd: HashMap<String, i64> = HashMap::new();
        cd.insert("A".to_string(), 500); // exited at ts=500
        // ts=1000, cooldown=3600 → A still in cooldown; B passes
        let out = select_entries(&ranked, &[], 2, &watched, 0.0, 0.0, &cd, 3600, 1000, true);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].mint, "B");
    }

    // ─── Task 4: weakest_green unit tests ───────────────────────────────────

    fn make_position(mint: &str, entry_price: f64) -> crate::portfolio::momentum_state::Position {
        crate::portfolio::momentum_state::Position {
            mint: mint.to_string(),
            symbol: mint.to_string(),
            entry_ts: 1_700_000_000,
            entry_price_usd: entry_price,
            token_amount: 100.0,
            usdc_spent: 100.0 * entry_price,
            peak_price_usd: entry_price,
            entry_sig: "dry-run".to_string(),
            dry_run: true,
        }
    }

    #[test]
    fn weakest_green_picks_lowest_score_green_position() {
        // Three positions; two gross-green (price > entry), one red.
        // A: entry=1.0 → price=2.0 (green), score=1.0 (lowest green)
        // B: entry=1.0 → price=2.0 (green), score=2.0
        // C: entry=3.0 → price=2.0 (red — price < entry)
        let positions = vec![
            make_position("A", 1.0),
            make_position("B", 1.0),
            make_position("C", 3.0),
        ];
        let mut prices: HashMap<String, f64> = HashMap::new();
        prices.insert("A".to_string(), 2.0);
        prices.insert("B".to_string(), 2.0);
        prices.insert("C".to_string(), 2.0);
        let ranked = vec![
            make_candidate("A", 1.0), // green, weakest score
            make_candidate("B", 2.0), // green, stronger
            make_candidate("C", 3.0), // red (price < entry) — ignored
        ];
        let idx = weakest_green(&positions, &ranked, &prices);
        assert_eq!(idx, Some(0), "A is the weakest-scoring green held position");
    }

    #[test]
    fn weakest_green_ignores_red_positions() {
        // Only one position, and it's red (price ≤ entry) — should return None.
        let positions = vec![make_position("A", 5.0)];
        let mut prices: HashMap<String, f64> = HashMap::new();
        prices.insert("A".to_string(), 3.0); // below entry
        let ranked = vec![make_candidate("A", 2.0)];
        let idx = weakest_green(&positions, &ranked, &prices);
        assert_eq!(idx, None, "no green positions → None");
    }

    #[test]
    fn weakest_green_ignores_stale_positions() {
        // One position gross-green but stale in the ranked list — should be excluded.
        let positions = vec![make_position("A", 1.0)];
        let mut prices: HashMap<String, f64> = HashMap::new();
        prices.insert("A".to_string(), 2.0); // gross-green
        let mut ranked = vec![make_candidate("A", 1.5)];
        ranked[0].stale = true; // mark stale
        let idx = weakest_green(&positions, &ranked, &prices);
        assert_eq!(idx, None, "stale position must not be evicted");
    }

    #[test]
    fn weakest_green_none_when_empty() {
        let positions: Vec<crate::portfolio::momentum_state::Position> = vec![];
        let prices: HashMap<String, f64> = HashMap::new();
        let ranked: Vec<Candidate> = vec![];
        assert_eq!(weakest_green(&positions, &ranked, &prices), None);
    }

    // ─── invalidate_unbacked_position: retain semantics (Critical #1) ───────

    /// Verify the retain-by-mint logic introduced in `invalidate_unbacked_position`:
    /// when position A is unbacked and B is backed, only A is dropped and B survives.
    /// A's mint is recorded in `last_exit_ts_per_mint` (benched); B's is not.
    ///
    /// `invalidate_unbacked_position` itself performs disk I/O, so we test the
    /// pure retain/bench logic here — the same pattern used in
    /// `exit_removes_only_the_closed_position` in `momentum_state.rs`.
    #[test]
    fn invalidate_keeps_backed_coheld_positions() {
        use crate::portfolio::momentum_state::{Position, TraderState};
        use std::collections::HashMap;

        let mut state = TraderState::default();
        // Position A — live, UNBACKED (wallet holds 0).
        state.positions.push(Position {
            mint: "MINT_A".into(),
            symbol: "AAA".into(),
            entry_ts: 1_700_000_000,
            entry_price_usd: 1.0,
            token_amount: 100.0,
            usdc_spent: 100.0,
            peak_price_usd: 1.1,
            entry_sig: "sig_a".into(),
            dry_run: false, // live position
        });
        // Position B — live, BACKED (wallet still holds it).
        state.positions.push(Position {
            mint: "MINT_B".into(),
            symbol: "BBB".into(),
            entry_ts: 1_700_000_000,
            entry_price_usd: 2.0,
            token_amount: 50.0,
            usdc_spent: 100.0,
            peak_price_usd: 2.2,
            entry_sig: "sig_b".into(),
            dry_run: false, // live position
        });

        // Replicate the invalidation logic: collect unbacked mints, bench, retain.
        // Wallet holds MINT_B but not MINT_A.
        let wallet_balances: HashMap<String, f64> =
            [("MINT_B".to_string(), 50.0)].into_iter().collect();
        let unbacked: Vec<String> = state
            .positions
            .iter()
            .filter(|p| !p.dry_run)
            .filter(|p| wallet_balances.get(&p.mint).copied().unwrap_or(0.0) <= 0.0)
            .map(|p| p.mint.clone())
            .collect();

        let ts = 1_700_001_000_i64;
        for mint in &unbacked {
            state.last_exit_ts_per_mint.insert(mint.clone(), ts);
        }
        state.positions.retain(|p| p.dry_run || !unbacked.contains(&p.mint));

        // A was unbacked → removed; B was backed → survives.
        assert_eq!(state.positions.len(), 1, "exactly one position should remain");
        assert_eq!(state.positions[0].mint, "MINT_B", "MINT_B must survive invalidation of MINT_A");
        // A is benched.
        assert!(
            state.last_exit_ts_per_mint.contains_key("MINT_A"),
            "MINT_A must be benched in last_exit_ts_per_mint"
        );
        // B is NOT benched.
        assert!(
            !state.last_exit_ts_per_mint.contains_key("MINT_B"),
            "MINT_B must NOT be benched — it is still backed"
        );
    }

    #[test]
    fn regime_exempt_for_only_on_explicit_false() {
        let mk = |rf: Option<bool>| WatchedToken {
            symbol: "A".into(), mint: "A".into(), name: None, equity: None,
            params: Some(crate::portfolio::momentum_universe::TokenParams {
                regime_filter: rf, ..Default::default() }),
            pool: None, quote: None, pools: None,
        };
        assert!(regime_exempt_for(&[mk(Some(false))], "A"));   // explicit false → exempt
        assert!(!regime_exempt_for(&[mk(Some(true))], "A"));   // explicit true → obey gate
        assert!(!regime_exempt_for(&[mk(None)], "A"));         // absent field → obey gate
        assert!(!regime_exempt_for(&[mk(Some(false))], "Z"));  // unknown mint → obey gate
        let none = WatchedToken { symbol: "B".into(), mint: "B".into(), name: None, equity: None, params: None, pool: None, quote: None, pools: None };
        assert!(!regime_exempt_for(&[none], "B"));             // no params at all → obey gate
    }

    #[test]
    fn stop_decision_dwell_lifecycle() {
        use std::time::{Duration, Instant};
        let t0 = Instant::now();
        // first breach → Arm, do not sell
        assert!(matches!(stop_decision(true, None, t0, 3), ExitDecision::Arm));
        // still breached, dwell not elapsed → StayArmed
        assert!(matches!(stop_decision(true, Some(t0), t0 + Duration::from_secs(1), 3), ExitDecision::StayArmed));
        // still breached, dwell elapsed → Sell
        assert!(matches!(stop_decision(true, Some(t0), t0 + Duration::from_secs(3), 3), ExitDecision::Sell));
        // recovered while armed → Disarm
        assert!(matches!(stop_decision(false, Some(t0), t0 + Duration::from_secs(1), 3), ExitDecision::Disarm));
        // not breached, not armed → Hold
        assert!(matches!(stop_decision(false, None, t0, 3), ExitDecision::Hold));
        // confirm_secs=0 → immediate Sell on first breach (dwell disabled)
        assert!(matches!(stop_decision(true, None, t0, 0), ExitDecision::Sell));
    }
}
