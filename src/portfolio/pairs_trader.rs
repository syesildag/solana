use std::collections::{HashMap, VecDeque};
use anyhow::Result;
use tracing::info;

use super::history::PriceSnapshot;
use super::jupiter;
use super::kamino::ReserveInfo;
use super::pairs_config::{PairSpec, PairsConfig};
use super::pairs_signal::{borrow_apy_ok, estimate_health_factor, pair_decision, PairDecision};
use super::pairs_state::{self, PairPosition};
use super::sim;
use super::momentum::est_gas_usdc;

/// z-score of the live `ln(A/B)` spread over the last `lookback` aligned observations.
pub fn live_spread_z(history: &VecDeque<PriceSnapshot>, spec: &PairSpec, lookback: usize) -> Option<f64> {
    let spreads: Vec<f64> = history.iter().filter_map(|s| {
        let a = s.prices.get(&spec.mint_a).copied().filter(|p| *p > 0.0)?;
        let b = s.prices.get(&spec.mint_b).copied().filter(|p| *p > 0.0)?;
        let z = (a / b).ln();
        z.is_finite().then_some(z)
    }).collect();
    if spreads.is_empty() { return None; }
    let lo = spreads.len().saturating_sub(lookback);
    sim::zscore_last(&spreads[lo..])
}

/// Paper P&L for a dollar-neutral pair: sell the long leg, buy back the short leg,
/// both net of slippage, minus two gas legs. Pure in stored entry marks + current prices.
pub fn simulate_pair_pnl(pos: &PairPosition, long_px: f64, short_px: f64, slippage_bps: u32, sol_px: f64) -> f64 {
    // NB: this is the *gross* spread P&L (slippage + 2 gas legs). The short-leg borrow
    // funding cost is applied separately by `close_pair` via `funding_cost_usdc`, using the
    // borrow APY captured at entry — keeping this fn a pure function of prices.
    let slip = slippage_bps as f64 / 10_000.0;
    let long_pl = pos.long_amount * (long_px * (1.0 - slip) - pos.entry_long_px);
    let short_pl = pos.short_amount * (pos.entry_short_px - short_px * (1.0 + slip));
    long_pl + short_pl - 2.0 * est_gas_usdc(sol_px)
}

// ─── Phase 2c: leg sizing · slippage-capped swap · preflight gate · rollback planner ──

/// Raw base-unit size of one leg: how many tokens `trade_usdc` buys at price `px`.
pub fn leg_size(trade_usdc: f64, px: f64, decimals: u8) -> u64 {
    if px <= 0.0 || !px.is_finite() {
        return 0;
    }
    jupiter::to_raw_amount(trade_usdc / px, decimals)
}

/// Result of a single DEX leg quote (read-only — no `/swap`, no submission).
#[derive(Debug, Clone)]
pub struct SwapResult {
    pub out_raw: u64,
    pub out_amount: f64,
    pub impact_bps: u32,
}

/// Quote one swap leg via Jupiter and enforce the slippage cap. Read-only: it calls
/// `/quote` only, never `/swap`, so it is safe in dry-run. Errors (so the caller aborts
/// the leg) if price impact exceeds `max_slippage_bps`.
pub async fn swap_leg(
    http: &reqwest::Client,
    api_url: &str,
    from_mint: &str,
    to_mint: &str,
    amount_raw: u64,
    out_decimals: u8,
    max_slippage_bps: u32,
) -> Result<SwapResult> {
    let q = jupiter::quote(http, api_url, from_mint, to_mint, amount_raw, max_slippage_bps).await?;
    let impact_bps = jupiter::price_impact_bps(&q);
    if impact_bps > max_slippage_bps {
        anyhow::bail!("swap {from_mint}->{to_mint} impact {impact_bps}bps exceeds cap {max_slippage_bps}bps");
    }
    let out_raw = q.out_amount.parse::<u64>().unwrap_or(0);
    Ok(SwapResult { out_raw, out_amount: jupiter::from_raw_amount(out_raw, out_decimals), impact_bps })
}

/// Outcome of the pre-open risk gate.
#[derive(Debug, Clone, PartialEq)]
pub enum Preflight {
    Ok { borrow_apy_pct: f64, health_factor: f64 },
    MissingReserve(String),
    ShortNotBorrowable(String),
    BorrowApyTooHigh { sym: String, apy_pct: f64 },
    HealthTooLow { health_factor: f64 },
}

/// Pre-open risk gate (pure). Given the chosen decision + live klend reserves, decide
/// whether the cross-margin open is allowed:
/// 1. the short leg must be **borrowable** with liquidity (GOOGLx in the xStocks market
///    has a 0 borrow cap → never shortable, even though it has collateral liquidity),
/// 2. its borrow APY must be under `PAIRS_MAX_BORROW_APY_PCT`,
/// 3. the estimated post-open health factor must be ≥ `PAIRS_MIN_HEALTH_FACTOR`.
///
/// Dollar-neutral sizing is assumed: USDC collateral = long value = short debt =
/// `trade_usdc`, so health = (usdc_liq_thr + long_liq_thr). A non-`Open` decision passes
/// trivially.
///
/// Decision (2026-06-23): **no slippage haircut on the health estimate.** Kamino computes
/// health from *oracle* prices, not our execution price, so swap slippage is an entry cost
/// (already in P&L) — not a health input. The safety margin is the configurable
/// `min_health_factor` floor (1.5), not a fudge factor here. Borrow-factor weighting of the
/// debt (Kamino weights e.g. NVDAx debt ×2.25) is deferred — at the 1.5 floor it only makes
/// the gate stricter, and the floor already dominates; revisit if sizing grows.
pub fn preflight_open(
    decision: &PairDecision,
    reserves: &HashMap<String, ReserveInfo>,
    trade_usdc: f64,
    cfg: &PairsConfig,
) -> Preflight {
    let (long_sym, short_sym) = match decision {
        PairDecision::Open { long_sym, short_sym, .. } => (long_sym.as_str(), short_sym.as_str()),
        _ => return Preflight::Ok { borrow_apy_pct: 0.0, health_factor: f64::INFINITY },
    };
    let Some(short) = reserves.get(short_sym) else {
        return Preflight::MissingReserve(short_sym.to_string());
    };
    let Some(long) = reserves.get(long_sym) else {
        return Preflight::MissingReserve(long_sym.to_string());
    };
    let Some(usdc) = reserves.get("USDC") else {
        return Preflight::MissingReserve("USDC".to_string());
    };

    if !short.borrowable || short.available_liquidity <= 0.0 {
        return Preflight::ShortNotBorrowable(short_sym.to_string());
    }
    if !borrow_apy_ok(short.borrow_apy_pct, cfg) {
        return Preflight::BorrowApyTooHigh { sym: short_sym.to_string(), apy_pct: short.borrow_apy_pct };
    }
    // Cross-margin health: collateral = USDC + long leg (each `trade_usdc`), debt = short.
    let collateral_weighted = trade_usdc * usdc.liq_threshold + trade_usdc * long.liq_threshold;
    let hf = estimate_health_factor(collateral_weighted, trade_usdc, 1.0);
    if hf < cfg.min_health_factor {
        return Preflight::HealthTooLow { health_factor: hf };
    }
    Preflight::Ok { borrow_apy_pct: short.borrow_apy_pct, health_factor: hf }
}

/// How far `open_pair` progressed before failing (the last completed step).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenProgress {
    Nothing,
    BoughtLong,
    Deposited,
    Borrowed,
    Opened,
}

/// An undo step for a partially-completed open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackAction {
    RepayShort,
    WithdrawCollateral,
    SellLongToUsdc,
}

/// Plan the unwind for a partially-completed open (pure). The open sequence is
/// buy long → deposit(USDC+long) → borrow short → sell short; rollback reverses whatever
/// completed. At `Borrowed` we still hold the just-borrowed short tokens, so they repay
/// the loan directly (no buy-back needed). `Opened` (success) and `Nothing` need no unwind.
pub fn rollback_plan(progress: OpenProgress) -> Vec<RollbackAction> {
    use RollbackAction::*;
    match progress {
        OpenProgress::Nothing | OpenProgress::Opened => vec![],
        OpenProgress::BoughtLong => vec![SellLongToUsdc],
        OpenProgress::Deposited => vec![WithdrawCollateral, SellLongToUsdc],
        OpenProgress::Borrowed => vec![RepayShort, WithdrawCollateral, SellLongToUsdc],
    }
}

/// Borrow funding cost (USDC) for holding a short of `notional_usdc` at `borrow_apy_pct`
/// for `hold_secs`. Pure; mirrors the backtest's funding model (notional × apy × time).
pub fn funding_cost_usdc(notional_usdc: f64, borrow_apy_pct: f64, hold_secs: i64) -> f64 {
    if hold_secs <= 0 || borrow_apy_pct <= 0.0 || notional_usdc <= 0.0 {
        return 0.0;
    }
    const SECONDS_PER_YEAR: f64 = 365.0 * 86_400.0;
    notional_usdc * (borrow_apy_pct / 100.0) * (hold_secs as f64 / SECONDS_PER_YEAR)
}

/// Open a pair position. The live sequence (executed in Phase 2d), in order:
/// 1. buy the long leg on the DEX,
/// 2. deposit USDC + the long leg to Kamino as collateral,
/// 3. borrow the short leg,
/// 4. sell the borrowed short leg → USDC.
/// A mid-sequence failure unwinds via [`rollback_plan`]. Here (Phase 2c, paper) each step
/// is logged and the position is priced from `prices`; nothing is submitted. Live
/// (`!cfg.dry_run`) returns an error — that path is Phase 2d.
#[allow(clippy::too_many_arguments)]
pub async fn open_pair(
    cfg: &PairsConfig,
    pair_key: &str,
    decision: &PairDecision,
    z: f64,
    prices: &HashMap<String, f64>,
    now: i64,
    borrow_apy_pct: f64,
) -> Result<PairPosition> {
    let PairDecision::Open { long_mint, long_sym, short_mint, short_sym } = decision else {
        anyhow::bail!("open_pair requires an Open decision");
    };
    if !cfg.dry_run {
        anyhow::bail!("pairs live execution is Phase 2d — keep DRY_RUN_PAIRS_TRADER=true");
    }
    let lpx = prices.get(long_mint.as_str()).copied().unwrap_or(0.0);
    let spx = prices.get(short_mint.as_str()).copied().unwrap_or(0.0);
    if lpx <= 0.0 || spx <= 0.0 {
        anyhow::bail!("open_pair {pair_key}: missing price (long={lpx}, short={spx})");
    }
    let long_amount = cfg.trade_usdc / lpx;
    let short_amount = cfg.trade_usdc / spx;
    info!("pairs(paper): OPEN {pair_key} z={z:.2} long {long_sym} short {short_sym}");
    info!("  [paper] 1/4 buy {long_amount:.4} {long_sym} (~{:.2} USDC)", cfg.trade_usdc);
    info!("  [paper] 2/4 deposit {:.2} USDC + {long_amount:.4} {long_sym} as collateral", cfg.trade_usdc);
    info!("  [paper] 3/4 borrow {short_amount:.4} {short_sym} (apy {borrow_apy_pct:.2}%)");
    info!("  [paper] 4/4 sell {short_amount:.4} {short_sym} → USDC");
    Ok(PairPosition {
        pair_key: pair_key.to_string(),
        long_mint: long_mint.clone(),
        long_sym: long_sym.clone(),
        long_amount,
        short_mint: short_mint.clone(),
        short_sym: short_sym.clone(),
        short_amount,
        usdc_collateral: cfg.trade_usdc,
        entry_ts: now,
        entry_z: z,
        entry_long_px: lpx,
        entry_short_px: spx,
        borrow_apy_pct,
        dry_run: cfg.dry_run,
    })
}

/// Close a pair position; returns realized USDC P&L net of slippage, gas, and the
/// short-leg borrow funding accrued over the hold. Live sequence (Phase 2d), in order:
/// 1. buy back the short leg → repay the Kamino borrow,
/// 2. withdraw collateral (USDC + long leg),
/// 3. sell the long leg → USDC.
/// Closing carries no cost gate in 2d — a position must always be closable; slippage
/// self-escalates there. Here (paper) each step is logged and P&L is priced from
/// `prices`; nothing is submitted. Live (`!cfg.dry_run`) is Phase 2d.
pub async fn close_pair(
    cfg: &PairsConfig,
    pos: &PairPosition,
    z: f64,
    prices: &HashMap<String, f64>,
    now: i64,
) -> Result<f64> {
    if !cfg.dry_run {
        anyhow::bail!("pairs live execution is Phase 2d — keep DRY_RUN_PAIRS_TRADER=true");
    }
    let lpx = prices.get(pos.long_mint.as_str()).copied().unwrap_or(0.0);
    let spx = prices.get(pos.short_mint.as_str()).copied().unwrap_or(0.0);
    if lpx <= 0.0 || spx <= 0.0 {
        anyhow::bail!("close_pair {}: missing price (long={lpx}, short={spx})", pos.pair_key);
    }
    let sol = prices.get("SOL").copied().unwrap_or(0.0);
    let gross = simulate_pair_pnl(pos, lpx, spx, cfg.slippage_bps, sol);
    let funding = funding_cost_usdc(pos.usdc_collateral, pos.borrow_apy_pct, now - pos.entry_ts);
    let pnl = gross - funding;
    info!("pairs(paper): CLOSE {} z={z:.2}", pos.pair_key);
    info!("  [paper] 1/3 buy back {:.4} {} → repay borrow", pos.short_amount, pos.short_sym);
    info!("  [paper] 2/3 withdraw collateral (USDC + {})", pos.long_sym);
    info!("  [paper] 3/3 sell {:.4} {} → USDC", pos.long_amount, pos.long_sym);
    info!("  net pnl {pnl:+.4} USDC (gross {gross:+.4} − funding {funding:.4})");
    Ok(pnl)
}

// ─── Phase 2d (drafted): portfolio risk layer — gates · loss breaker · health monitor ──

/// Portfolio-level pre-open risk verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RiskVerdict {
    Ok,
    Halted,
    DailyCapReached,
}

/// Pre-open risk gate (pure). Blocks NEW opens when the halt file is present (manual kill
/// switch or a tripped loss breaker) or the daily trade cap is hit. Per-pair borrow/health
/// checks live in [`preflight_open`]; the held-position health monitor is [`should_derisk`].
/// Closing is never gated — a position must always stay exitable.
pub fn risk_ok(halted: bool, trades_24h: usize, cfg: &PairsConfig) -> RiskVerdict {
    if halted {
        return RiskVerdict::Halted;
    }
    if trades_24h >= cfg.max_trades_per_day as usize {
        return RiskVerdict::DailyCapReached;
    }
    RiskVerdict::Ok
}

/// Cumulative realized USDC P&L across all recorded closed trades (pure).
pub fn cumulative_realized_pnl(state: &pairs_state::PairsTraderState) -> f64 {
    state.trades.iter().map(|t| t.pnl_usdc).sum()
}

/// Aggregate realized-P&L stats over the closed-trade log (pure).
#[derive(Debug, Clone)]
pub struct PnlStats {
    pub n: usize,
    pub net: f64,
    pub wins: usize,
    pub win_rate: f64,
    pub best: f64,
    pub worst: f64,
}

pub fn pnl_stats(state: &pairs_state::PairsTraderState) -> PnlStats {
    let t = &state.trades;
    let n = t.len();
    let wins = t.iter().filter(|x| x.pnl_usdc > 0.0).count();
    PnlStats {
        n,
        net: cumulative_realized_pnl(state),
        wins,
        win_rate: if n > 0 { wins as f64 / n as f64 * 100.0 } else { 0.0 },
        best: t.iter().map(|x| x.pnl_usdc).fold(f64::NEG_INFINITY, f64::max),
        worst: t.iter().map(|x| x.pnl_usdc).fold(f64::INFINITY, f64::min),
    }
}

/// Log the cumulative realized-P&L summary (no-op when there are no closed trades).
pub fn log_pnl_summary(state: &pairs_state::PairsTraderState) {
    let s = pnl_stats(state);
    if s.n == 0 {
        return;
    }
    info!(
        "pairs: realized P&L — {} trade(s), net {:+.4} USDC, win {:.0}% ({}W/{}L), best {:+.2}, worst {:+.2}",
        s.n, s.net, s.win_rate, s.wins, s.n - s.wins, s.best, s.worst,
    );
}

/// Has cumulative realized P&L breached the loss floor? `max_loss_usdc ≤ 0` disables it.
pub fn loss_breaker_tripped(realized_usdc: f64, cfg: &PairsConfig) -> bool {
    cfg.max_loss_usdc > 0.0 && realized_usdc <= -cfg.max_loss_usdc
}

/// Held-position health monitor (pure): force a de-risking close when the live obligation
/// health drops below the floor, regardless of the z-score. ∞/NaN never trips. Wired into
/// the HOLDING path in Phase 2d.2 (needs live `read_obligation_health`).
pub fn should_derisk(current_health: f64, cfg: &PairsConfig) -> bool {
    current_health.is_finite() && current_health < cfg.min_health_factor
}

/// Is the pairs trader halted? (halt file present — manual or breaker-written.) Reuses the
/// momentum halt-file format/IO so the two traders share one halt convention.
pub fn is_halted(halt_path: &str) -> bool {
    matches!(
        crate::portfolio::momentum_state::read_halt(std::path::Path::new(halt_path)),
        Ok(Some(_))
    )
}

/// LIVE-only loss circuit breaker. If cumulative realized P&L has breached the floor, write
/// the halt file (stopping further opens until the operator deletes it) and return true.
/// Paper losses never halt — they aren't real (mirrors the momentum breaker).
pub fn maybe_halt_on_loss(state: &pairs_state::PairsTraderState, cfg: &PairsConfig, now: i64) -> bool {
    if cfg.dry_run {
        return false;
    }
    let realized = cumulative_realized_pnl(state);
    if !loss_breaker_tripped(realized, cfg) {
        return false;
    }
    let reason = format!(
        "pairs cumulative realized P&L {realized:+.2} USDC hit the -{:.2} USDC loss limit",
        cfg.max_loss_usdc
    );
    tracing::error!("pairs: LOSS HALT — {reason}. New opens stopped; delete {} to re-arm.", cfg.halt_path);
    if let Err(e) = crate::portfolio::momentum_state::write_halt(
        std::path::Path::new(&cfg.halt_path),
        &crate::portfolio::momentum_state::HaltRecord { ts: now, reason },
    ) {
        tracing::warn!("pairs: failed to write halt file: {e}");
    }
    true
}

/// Reconstruct the PairSpec for a "SYMA/SYMB" key from the configured pairs.
fn spec_for(cfg: &PairsConfig, key: &str) -> Option<PairSpec> {
    cfg.pairs.iter().find(|s| format!("{}/{}", s.symbol_a, s.symbol_b) == key).cloned()
}

/// One paper tick: evaluate close if holding, else scan pairs and open the first whose
/// signal fires + cooldown/daily-cap pass. DRY-RUN only — no on-chain calls.
pub async fn tick(cfg: &PairsConfig, history: &VecDeque<PriceSnapshot>, prices: &HashMap<String, f64>) -> Result<()> {
    if !cfg.enable { return Ok(()); }
    let state_path = std::path::Path::new(&cfg.state_path);
    let mut state = pairs_state::load(state_path)?;
    let now = chrono::Utc::now().timestamp();

    // Per-tick heartbeat: log each configured pair's live spread z + the signal it implies,
    // so the trader's state is visible even on a no-action tick (analogue of momentum's
    // rank[...] log). This reflects the raw z-signal; the actual open/close/skip is logged
    // by the action paths below.
    for spec in &cfg.pairs {
        let key = format!("{}/{}", spec.symbol_a, spec.symbol_b);
        let holding = state.position.as_ref().is_some_and(|p| p.pair_key == key);
        match live_spread_z(history, spec, cfg.lookback_obs) {
            Some(z) => {
                let signal = match pair_decision(z, holding, spec, cfg) {
                    PairDecision::Hold => "hold".to_string(),
                    PairDecision::Open { long_sym, short_sym, .. } => format!("signal: long {long_sym} / short {short_sym}"),
                    PairDecision::Close => "signal: close".to_string(),
                };
                info!(
                    "pairs: {key} z={z:+.2} (enter ±{:.1}, exit ±{:.1}, stop ±{:.1}){} — {signal}",
                    cfg.z_entry, cfg.z_exit, cfg.z_stop,
                    if holding { " [in position]" } else { "" },
                );
            }
            None => info!("pairs: {key} z=n/a — not enough aligned price history yet"),
        }
    }

    // HOLDING: evaluate close.
    if let Some(pos) = state.position.clone() {
        let Some(spec) = spec_for(cfg, &pos.pair_key) else {
            tracing::warn!("pairs: held pair {} no longer in config — leaving position open, add it back or close manually", pos.pair_key);
            return Ok(());
        };
        if let Some(z) = live_spread_z(history, &spec, cfg.lookback_obs) {
            if matches!(pair_decision(z, true, &spec, cfg), PairDecision::Close) {
                match close_pair(cfg, &pos, z, prices, now).await {
                    Ok(pnl) => {
                        state.trades.push(pairs_state::PairTradeRecord { pair_key: pos.pair_key.clone(),
                            entry_ts: pos.entry_ts, exit_ts: now, entry_z: pos.entry_z, exit_z: z, pnl_usdc: pnl, dry_run: pos.dry_run });
                        state.last_close_ts_per_pair.insert(pos.pair_key.clone(), now);
                        state.position = None;
                        pairs_state::save(state_path, &state)?;
                        // LIVE-only loss circuit breaker (no-op in paper).
                        maybe_halt_on_loss(&state, cfg, now);
                        // Running realized-P&L summary so the aggregate is visible in the log.
                        log_pnl_summary(&state);
                    }
                    Err(e) => tracing::warn!("pairs: close {} deferred — {e}", pos.pair_key),
                }
            }
        }
        return Ok(());
    }

    // FLAT: scan pairs, open the first whose signal fires + gates pass (paper).
    // Portfolio risk gate: halt file (manual kill switch / tripped loss breaker) or daily cap.
    match risk_ok(is_halted(&cfg.halt_path), pairs_state::trades_last_24h(&state, now), cfg) {
        RiskVerdict::Ok => {}
        v => { tracing::info!("pairs: no opens — {v:?}"); return Ok(()); }
    }
    // klend preflight gate (borrowability / APY / health). Enabled only when a sidecar URL
    // is configured; reserves are fetched once per tick (read-only — paper makes no
    // submission). If the gate is on but the sidecar is unreachable, fail safe: no opens.
    let reserves = if cfg.klend_sidecar_url.is_empty() {
        None
    } else {
        match crate::portfolio::kamino::KlendClient::new(cfg.klend_sidecar_url.as_str()).market().await {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!("pairs: klend gate on but /market failed ({e}); no opens this tick");
                return Ok(());
            }
        }
    };
    for spec in &cfg.pairs {
        let Some(z) = live_spread_z(history, spec, cfg.lookback_obs) else { continue };
        let decision = pair_decision(z, false, spec, cfg);
        if !matches!(decision, PairDecision::Open { .. }) { continue; }
        let key = format!("{}/{}", spec.symbol_a, spec.symbol_b);
        if state.last_close_ts_per_pair.get(&key).is_some_and(|&t| now - t < cfg.reentry_cooldown_secs) { continue; }
        // Borrowability / APY / health gate (e.g. never short GOOGLx) when enabled; capture
        // the entry borrow APY so close can charge funding.
        let mut entry_borrow_apy = 0.0;
        if let Some(res) = &reserves {
            match preflight_open(&decision, res, cfg.trade_usdc, cfg) {
                Preflight::Ok { borrow_apy_pct, health_factor } => {
                    info!("pairs: {key} preflight OK (borrow apy {borrow_apy_pct:.2}%, hf {health_factor:.2})");
                    entry_borrow_apy = borrow_apy_pct;
                }
                reason => { info!("pairs: skip {key} — preflight {reason:?}"); continue; }
            }
        }
        match open_pair(cfg, &key, &decision, z, prices, now, entry_borrow_apy).await {
            Ok(pos) => {
                state.position = Some(pos);
                pairs_state::save(state_path, &state)?;
                break;
            }
            Err(e) => { tracing::warn!("pairs: open {key} skipped — {e}"); continue; }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::history::PriceSnapshot;
    use std::collections::{HashMap, VecDeque};
    use crate::portfolio::pairs_config::PairSpec;
    use crate::portfolio::pairs_state::PairPosition;

    fn snap(ts: u64, a: f64, b: f64) -> PriceSnapshot {
        let mut p = HashMap::new();
        p.insert("MA".to_string(), a);
        p.insert("MB".to_string(), b);
        PriceSnapshot { ts, prices: p }
    }
    fn spec() -> PairSpec { PairSpec{symbol_a:"A".into(),mint_a:"MA".into(),symbol_b:"B".into(),mint_b:"MB".into()} }

    #[test]
    fn live_spread_z_matches_window() {
        let mut h: VecDeque<PriceSnapshot> = VecDeque::new();
        for i in 0..40u64 { h.push_back(snap(i, if i%2==0 {99.0} else {101.0}, 100.0)); }
        h.push_back(snap(40, 110.0, 100.0)); // A spikes up vs B → ln(A/B) high → z >> 0
        let z = live_spread_z(&h, &spec(), 45).expect("z computable");
        assert!(z > 2.0, "stretched spread → high z, got {z}");
    }

    #[test]
    fn simulate_pnl_profits_on_convergence() {
        // Opened with both legs at 100; long A rises to 110, short B flat at 100.
        let pos = PairPosition { pair_key:"A/B".into(), long_mint:"MA".into(), long_sym:"A".into(),
            long_amount: 1.0, short_mint:"MB".into(), short_sym:"B".into(), short_amount: 1.0,
            usdc_collateral: 50.0, entry_ts: 0, entry_z: -2.5,
            entry_long_px: 100.0, entry_short_px: 100.0, borrow_apy_pct: 0.0, dry_run: true };
        // long leg +~9.45 (110×0.995−100), short leg −0.5 (100−100×1.005) → net positive.
        let pnl = simulate_pair_pnl(&pos, 110.0, 100.0, 50, 150.0);
        assert!(pnl > 0.0, "convergence in our favor → profit, got {pnl}");
    }

    // ── Phase 2c: sizing · preflight gate · rollback planner ──
    use crate::portfolio::kamino::ReserveInfo;
    use solana_sdk::pubkey::Pubkey;

    fn reserve(borrowable: bool, apy_pct: f64, liq_thr: f64) -> ReserveInfo {
        ReserveInfo {
            reserve: Pubkey::new_unique(),
            liquidity_mint: Pubkey::new_unique(),
            liq_threshold: liq_thr,
            borrow_apy_pct: apy_pct,
            available_liquidity: 1000.0,
            borrow_cap: if borrowable { 1000.0 } else { 0.0 },
            borrowable,
        }
    }

    fn open_decision(long: &str, short: &str) -> PairDecision {
        PairDecision::Open {
            long_mint: format!("m{long}"),
            long_sym: long.into(),
            short_mint: format!("m{short}"),
            short_sym: short.into(),
        }
    }

    fn xstock_reserves() -> HashMap<String, ReserveInfo> {
        let mut m = HashMap::new();
        m.insert("USDC".into(), reserve(true, 5.0, 0.90));
        m.insert("NVDAx".into(), reserve(true, 3.4, 0.65));
        m.insert("SPYx".into(), reserve(true, 3.5, 0.75));
        m.insert("GOOGLx".into(), reserve(false, 3.4, 0.70)); // borrow cap 0 → collateral-only
        m
    }

    #[test]
    fn leg_size_converts_usdc_to_token_base_units() {
        assert_eq!(leg_size(50.0, 100.0, 6), 500_000); // $50 / $100 = 0.5 tok @ 6dp
        assert_eq!(leg_size(50.0, 0.0, 6), 0, "bad price → 0");
    }

    #[test]
    fn preflight_blocks_shorting_googlx() {
        // signal wants long NVDAx / short GOOGLx — GOOGLx is not borrowable.
        let d = open_decision("NVDAx", "GOOGLx");
        let r = preflight_open(&d, &xstock_reserves(), 50.0, &PairsConfig::test_default());
        assert!(matches!(r, Preflight::ShortNotBorrowable(ref s) if s == "GOOGLx"), "got {r:?}");
    }

    #[test]
    fn preflight_ok_for_borrowable_short_and_healthy_collateral() {
        // long SPYx / short NVDAx — hf = usdc_lt + long_lt = 0.90 + 0.75 = 1.65 ≥ 1.5.
        let d = open_decision("SPYx", "NVDAx");
        match preflight_open(&d, &xstock_reserves(), 50.0, &PairsConfig::test_default()) {
            Preflight::Ok { health_factor, .. } => {
                assert!((health_factor - 1.65).abs() < 1e-9, "hf={health_factor}")
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn preflight_rejects_high_borrow_apy() {
        let mut res = xstock_reserves();
        res.get_mut("NVDAx").unwrap().borrow_apy_pct = 35.0; // over the 30% cap
        let d = open_decision("SPYx", "NVDAx");
        assert!(matches!(
            preflight_open(&d, &res, 50.0, &PairsConfig::test_default()),
            Preflight::BorrowApyTooHigh { .. }
        ));
    }

    #[test]
    fn preflight_rejects_thin_health() {
        let cfg = PairsConfig { min_health_factor: 2.0, ..PairsConfig::test_default() };
        let d = open_decision("SPYx", "NVDAx"); // hf 1.65 < 2.0
        assert!(matches!(
            preflight_open(&d, &xstock_reserves(), 50.0, &cfg),
            Preflight::HealthTooLow { .. }
        ));
    }

    #[test]
    fn rollback_plan_reverses_completed_steps() {
        use RollbackAction::*;
        assert_eq!(rollback_plan(OpenProgress::Nothing), vec![]);
        assert_eq!(rollback_plan(OpenProgress::BoughtLong), vec![SellLongToUsdc]);
        assert_eq!(rollback_plan(OpenProgress::Deposited), vec![WithdrawCollateral, SellLongToUsdc]);
        assert_eq!(
            rollback_plan(OpenProgress::Borrowed),
            vec![RepayShort, WithdrawCollateral, SellLongToUsdc]
        );
        assert_eq!(rollback_plan(OpenProgress::Opened), vec![]);
    }

    #[test]
    fn funding_cost_scales_with_time_and_apy() {
        assert_eq!(funding_cost_usdc(50.0, 0.0, 86_400), 0.0, "no apy → no funding");
        assert_eq!(funding_cost_usdc(50.0, 30.0, 0), 0.0, "no time → no funding");
        let one_year = funding_cost_usdc(100.0, 30.0, 365 * 86_400);
        assert!((one_year - 30.0).abs() < 1e-6, "100 @ 30%/yr for 1y = 30, got {one_year}");
    }

    #[tokio::test]
    async fn open_pair_dry_run_builds_priced_position() {
        let cfg = PairsConfig::test_default();
        let d = open_decision("SPYx", "NVDAx"); // long SPYx / short NVDAx; mints "mSPYx"/"mNVDAx"
        let mut prices = HashMap::new();
        prices.insert("mSPYx".to_string(), 250.0);
        prices.insert("mNVDAx".to_string(), 100.0);
        let pos = open_pair(&cfg, "NVDAx/SPYx", &d, -2.5, &prices, 1_000, 3.4).await.unwrap();
        assert_eq!((pos.long_sym.as_str(), pos.short_sym.as_str()), ("SPYx", "NVDAx"));
        assert!((pos.long_amount - 50.0 / 250.0).abs() < 1e-9);
        assert!((pos.short_amount - 50.0 / 100.0).abs() < 1e-9);
        assert_eq!(pos.borrow_apy_pct, 3.4);
        assert_eq!(pos.pair_key, "NVDAx/SPYx");
    }

    #[tokio::test]
    async fn open_pair_live_is_phase_2d() {
        let cfg = PairsConfig { dry_run: false, ..PairsConfig::test_default() };
        let d = open_decision("SPYx", "NVDAx");
        assert!(open_pair(&cfg, "NVDAx/SPYx", &d, -2.5, &HashMap::new(), 0, 0.0).await.is_err());
    }

    #[tokio::test]
    async fn close_pair_charges_funding_over_gross() {
        let cfg = PairsConfig::test_default();
        let pos = PairPosition {
            pair_key: "A/B".into(), long_mint: "MA".into(), long_sym: "A".into(), long_amount: 0.5,
            short_mint: "MB".into(), short_sym: "B".into(), short_amount: 0.5,
            usdc_collateral: 50.0, entry_ts: 0, entry_z: -2.5, entry_long_px: 100.0,
            entry_short_px: 100.0, borrow_apy_pct: 36.5, dry_run: true,
        };
        let mut prices = HashMap::new();
        prices.insert("MA".to_string(), 100.0);
        prices.insert("MB".to_string(), 100.0);
        prices.insert("SOL".to_string(), 150.0);
        let hold = 10 * 86_400; // 10 days @ 36.5% on 50 USDC notional = 0.5 USDC funding
        let net = close_pair(&cfg, &pos, 0.1, &prices, hold).await.unwrap();
        let gross = simulate_pair_pnl(&pos, 100.0, 100.0, cfg.slippage_bps, 150.0);
        assert!((gross - net - 0.5).abs() < 1e-6, "funding drag ≈ 0.5 USDC (gross {gross}, net {net})");
    }

    // ── Phase 2d: risk layer (gates · loss breaker · health monitor) ──
    fn state_with_pnls(pnls: &[f64]) -> pairs_state::PairsTraderState {
        let mut s = pairs_state::PairsTraderState::default();
        for (i, &p) in pnls.iter().enumerate() {
            s.trades.push(pairs_state::PairTradeRecord {
                pair_key: "A/B".into(), entry_ts: i as i64, exit_ts: i as i64,
                entry_z: 0.0, exit_z: 0.0, pnl_usdc: p, dry_run: false,
            });
        }
        s
    }

    #[test]
    fn risk_ok_blocks_on_halt_and_daily_cap() {
        let cfg = PairsConfig { max_trades_per_day: 3, ..PairsConfig::test_default() };
        assert_eq!(risk_ok(false, 0, &cfg), RiskVerdict::Ok);
        assert_eq!(risk_ok(true, 0, &cfg), RiskVerdict::Halted);
        assert_eq!(risk_ok(false, 3, &cfg), RiskVerdict::DailyCapReached);
        assert_eq!(risk_ok(false, 2, &cfg), RiskVerdict::Ok);
    }

    #[test]
    fn loss_breaker_and_cumulative_pnl() {
        let cfg = PairsConfig { max_loss_usdc: 10.0, ..PairsConfig::test_default() };
        let s = state_with_pnls(&[-4.0, -7.0]); // cumulative -11
        assert!((cumulative_realized_pnl(&s) + 11.0).abs() < 1e-9);
        assert!(loss_breaker_tripped(cumulative_realized_pnl(&s), &cfg), "-11 ≤ -10 trips");
        assert!(!loss_breaker_tripped(-9.0, &cfg), "-9 within limit");
        let disabled = PairsConfig { max_loss_usdc: 0.0, ..PairsConfig::test_default() };
        assert!(!loss_breaker_tripped(-1000.0, &disabled), "0 disables breaker");
    }

    #[test]
    fn pnl_stats_aggregates_the_trade_log() {
        let s = state_with_pnls(&[1.5, -2.0, 0.5]);
        let st = pnl_stats(&s);
        assert_eq!(st.n, 3);
        assert!(st.net.abs() < 1e-9, "1.5 − 2.0 + 0.5 = 0");
        assert_eq!(st.wins, 2);
        assert!((st.win_rate - 200.0 / 3.0).abs() < 0.1, "2/3 wins");
        assert!((st.best - 1.5).abs() < 1e-9);
        assert!((st.worst + 2.0).abs() < 1e-9);
    }

    #[test]
    fn should_derisk_below_floor_only() {
        let cfg = PairsConfig { min_health_factor: 1.5, ..PairsConfig::test_default() };
        assert!(should_derisk(1.2, &cfg), "below floor → derisk");
        assert!(!should_derisk(1.8, &cfg), "above floor → hold");
        assert!(!should_derisk(f64::INFINITY, &cfg), "no debt → never derisk");
    }

    #[test]
    fn maybe_halt_on_loss_is_live_only_and_writes_halt() {
        let halt = std::env::temp_dir().join(format!("pairs_halt_{}.json", rand::random::<u32>()));
        let halt_path = halt.to_string_lossy().to_string();
        let losing = state_with_pnls(&[-12.0]);
        // paper: a breached loss must NOT halt (paper losses aren't real)
        let paper = PairsConfig { dry_run: true, max_loss_usdc: 10.0, halt_path: halt_path.clone(), ..PairsConfig::test_default() };
        assert!(!maybe_halt_on_loss(&losing, &paper, 1));
        assert!(!is_halted(&halt_path), "paper loss must not write the halt file");
        // live + breached: writes the halt file
        let live = PairsConfig { dry_run: false, max_loss_usdc: 10.0, halt_path: halt_path.clone(), ..PairsConfig::test_default() };
        assert!(maybe_halt_on_loss(&losing, &live, 1));
        assert!(is_halted(&halt_path), "live breach must write the halt file");
        std::fs::remove_file(&halt).ok();
    }
}
