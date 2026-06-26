//! Offline backtest / parameter-search engine for the momentum trader.
//!
//! Replays recorded price history (`assets/price_history.jsonl`) through the
//! **production** decision functions (`momentum::rank_candidates` and the pure
//! entry/exit gates) under different parameter sets, so the live behavior is
//! reproduced rather than re-implemented. Used by the `momentum-sim` binary to
//! grid-search the rank metric + the high-leverage `MOMENTUM_*` knobs and report
//! the combination with the best held-out net P&L.
//!
//! Fill model is deliberately **conservative** (see `exit_fill_price` and the
//! next-snapshot stop fill in `replay_with_stream`): it never flatters the
//! trailing stop, so simulated P&L is a floor, not a hope.

use std::collections::{HashMap, VecDeque};

use super::history::PriceSnapshot;
use super::momentum::{
    build_trade_record, est_gas_bps, est_gas_usdc, fade_take_profit, is_stale_ts, rank_candidates,
    dynamic_trade_usdc, profit_protected_stop_triggered, rotation_net_green, rotation_target,
    vol_stop_triggered, Candidate, VolStopMode,
};
use super::momentum_state::{summarize, Position, TradeRecord};
use super::momentum_universe::WatchedToken;
use super::suggestions::{atr_proxy, return_sigma, RankMetric};

/// SOL price key in a snapshot (used to price gas in USD).
const SOL_KEY: &str = "SOL";

/// Trailing window (in snapshots) handed to `rank_candidates`, so each call is
/// O(window) not O(i). Sized generously off the lookback so even a token that
/// ticks only once every few snapshots still accumulates `lookback + lag`
/// observations — a sparser token is under-observed in live trading too.
const WINDOW_SAFETY: usize = 3;
const WINDOW_PAD: usize = 50;

fn trailing_window_snaps(params: &ParamSet) -> usize {
    (params.lookback_obs + params.confirm_lag_obs) * WINDOW_SAFETY + WINDOW_PAD
}

/// Market-regime mask: `mask[i] = true` when SOL is above its moving average over
/// the prior `ma_obs` observations (risk-on). `ma_obs == 0` → all-true (filter off).
/// When SOL is missing/warming up, the prior regime persists (defaults to on).
pub fn regime_mask(snapshots: &[PriceSnapshot], ma_obs: usize) -> Vec<bool> {
    let n = snapshots.len();
    let mut mask = vec![true; n];
    if ma_obs == 0 {
        return mask;
    }
    let mut win: VecDeque<f64> = VecDeque::with_capacity(ma_obs + 1);
    let mut last = true;
    for (i, s) in snapshots.iter().enumerate() {
        match s.prices.get(SOL_KEY).copied().filter(|p| *p > 0.0) {
            Some(p) => {
                if win.len() >= 2 {
                    let m = win.iter().sum::<f64>() / win.len() as f64;
                    last = p > m;
                }
                mask[i] = last;
                win.push_back(p);
                while win.len() > ma_obs {
                    win.pop_front();
                }
            }
            None => mask[i] = last, // no fresh SOL price → regime persists
        }
    }
    mask
}

/// z-score of a mint's price over its last `dip_obs` observations at snapshot `i`
/// (for the mean-reversion entry confirmation). `None` below the obs floor or on a
/// flat series. Cheap (computed only at the entry check) so it stays a knob, not a
/// stream recompute.
fn token_dip_z(snapshots: &[PriceSnapshot], i: usize, mint: &str, dip_obs: usize) -> Option<f64> {
    let lo = (i + 1).saturating_sub(dip_obs);
    let prices: Vec<f64> = snapshots[lo..=i]
        .iter()
        .filter_map(|s| s.prices.get(mint).copied())
        .filter(|p| *p > 0.0)
        .collect();
    zscore_last(&prices)
}

/// Reversal confirmation: has the mint's price turned UP over its last `obs`
/// observations at snapshot `i` (current > the price `obs` obs ago)? `obs == 0` ⇒
/// always true (no confirmation). Too little history ⇒ false (don't enter unconfirmed).
fn token_rising(snapshots: &[PriceSnapshot], i: usize, mint: &str, obs: usize) -> bool {
    if obs == 0 {
        return true;
    }
    let lo = i.saturating_sub(obs.saturating_mul(4) + 5);
    let prices: Vec<f64> = snapshots[lo..=i]
        .iter()
        .filter_map(|s| s.prices.get(mint).copied())
        .filter(|p| *p > 0.0)
        .collect();
    if prices.len() <= obs {
        return false;
    }
    prices[prices.len() - 1] > prices[prices.len() - 1 - obs]
}

/// Average true range proxy for a mint at snapshot `i`: the mean absolute
/// price step over its last `n` observations (close-only data, so the "true range"
/// is just |Δprice|). Powers the Chandelier (volatility-scaled) trailing stop.
/// `None` with < 2 observations.
fn token_atr(snapshots: &[PriceSnapshot], i: usize, mint: &str, n: usize) -> Option<f64> {
    atr_proxy(&window_prices(snapshots, i, mint, n))
}

/// Per-observation return volatility (σ of log-returns) for a mint at snapshot `i`
/// over its last `n` observations. Powers the σ-scaled trailing stop. Shares the
/// exact math the live trader uses via `suggestions::return_sigma`.
fn token_return_sigma(snapshots: &[PriceSnapshot], i: usize, mint: &str, n: usize) -> Option<f64> {
    return_sigma(&window_prices(snapshots, i, mint, n))
}

/// The mint's last `n+1` positive prices up to and including snapshot `i` (so the
/// window yields `n` steps/returns). Shared by both volatility proxies above.
fn window_prices(snapshots: &[PriceSnapshot], i: usize, mint: &str, n: usize) -> Vec<f64> {
    let lo = (i + 1).saturating_sub(n + 1);
    snapshots[lo..=i]
        .iter()
        .filter_map(|s| s.prices.get(mint).copied())
        .filter(|p| *p > 0.0)
        .collect()
}

/// Recent `(ts, price)` series for a mint over a generous trailing window — only
/// used by the equity-market staleness check (wall-clock based, small window).
fn recent_series(snapshots: &[PriceSnapshot], i: usize, mint: &str) -> Vec<(u64, f64)> {
    let lo = i.saturating_sub(2_000);
    snapshots[lo..=i]
        .iter()
        .filter_map(|s| s.prices.get(mint).map(|p| (s.ts, *p)))
        .filter(|(_, p)| *p > 0.0)
        .collect()
}

/// One point in the parameter grid: the 5 swept knobs plus the frozen knobs the
/// state machine still needs. Built from `PortfolioConfig` (frozen) crossed with
/// the swept ranges (see `momentum_sim.rs`).
#[derive(Debug, Clone)]
pub struct ParamSet {
    // ----- swept -----
    pub metric: RankMetric,
    pub min_metric: f64,
    pub trail_pct: f64,
    pub lookback_obs: usize,
    pub max_run_pct: f64,
    /// While holding, rotate into a stronger token only if its score beats the held
    /// token's by at least this much (active-metric units). `0` disables rotation
    /// (the default and the production default).
    pub rotate_margin: f64,
    /// Market-regime filter: block NEW entries unless SOL is above its moving average
    /// over this many trailing observations (risk-on). Exits are never blocked. `0`
    /// disables — the strategy ignores the broad market.
    pub regime_filter_obs: usize,
    // ----- frozen (from .env) -----
    pub decel_lookback_min: usize,
    pub confirm_lag_obs: usize,
    pub stale_minutes: usize,
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    pub trade_usdc: f64,
    pub slippage_bps: u32,
    pub max_cost_bps: u32,
    pub exit_on_fade: bool,
    /// Which volatility measure scales the trailing stop (`Off` = fixed-% `trail_pct`).
    /// `Atr` and `Sigma` are active only when `chandelier_k > 0`; both fall back to the
    /// fixed-% stop while their `vol_obs` window is warming up.
    pub vol_stop_mode: VolStopMode,
    /// Volatility-scaled trailing-stop multiplier `k`. For `Atr`: exit when price ≤
    /// peak − `k`·ATR(vol_obs). For `Sigma`: effective trail % = `k`·σ·100. The two `k`
    /// scales are NOT interchangeable (price-units vs %-multiplier). `0` → fixed-% stop.
    pub chandelier_k: f64,
    /// Window (observations) for the ATR / σ / overbought-z volatility measures.
    pub vol_obs: usize,
    /// Overbought take-profit: while green, exit when the token's z-score over `vol_obs`
    /// ≥ `overbought_z` (sell into the spike). `0` disables.
    pub overbought_z: f64,
    /// Mean-reversion entry confirmation ("both must be true"): require the token to
    /// ALSO be oversold — its z-score over the last `entry_dip_obs` observations
    /// ≤ −`entry_dip_z` — before a momentum entry fires. Buys the *pullback* within a
    /// strong token instead of the top. `entry_dip_obs == 0` disables it (pure momentum).
    pub entry_dip_obs: usize,
    pub entry_dip_z: f64,
    /// Reversal confirmation for the dip entry: also require the price to have turned
    /// UP over the last `dip_confirm_obs` observations (buy the bounce, not the falling
    /// knife). `0` = no confirmation (enter on oversold alone). Only used when
    /// `entry_dip_obs > 0`.
    pub dip_confirm_obs: usize,
    /// Fill realism for the trailing stop. `false` (default, conservative): a tripped
    /// stop fills at the NEXT snapshot's price (~3 min later — models reacting after
    /// the move on coarse history). `true` (optimistic): fills same-bar at the price
    /// that tripped the stop (closer to live, where the 1 s poll exits immediately).
    /// Brackets the truth; the real live fill sits between the two.
    pub optimistic_fill: bool,
    /// Hard time stop: exit a position `max_hold_min` minutes after entry regardless of
    /// price (the move didn't pay off in time). `0` disables.
    pub max_hold_min: u32,
    /// Breakeven stop: once a position has gone green (price rose above entry), exit if it
    /// falls back to/through the entry price — don't let a winner round-trip into a loser.
    pub breakeven_exit: bool,
    /// Profit-protected ("max-trail") give-back cap, percent. Once a position is green
    /// (peak above the cost-adjusted breakeven), exit at `max(floor, peak·(1 − max_trail_pct/100))`
    /// instead of the tight `trail_pct` — let a winner give back gains while never closing
    /// red. `0` = disabled (the tight trail/vol stop governs throughout). Generalizes
    /// `breakeven_exit` (large value ⇒ ride to the breakeven floor).
    pub max_trail_pct: f64,
    /// Equity-compounding sizing: grow the entry notional by `reinvest_frac` of banked
    /// realized PnL, clamped to `[trade_usdc, size_ceiling_usdc]`. `0` = fixed `trade_usdc`.
    pub reinvest_frac: f64,
    /// Hard ceiling (USDC) on the compounded entry size. Below `trade_usdc` ⇒ no growth.
    pub size_ceiling_usdc: f64,
}

/// The result of replaying one `ParamSet` over one slice of history.
#[derive(Debug, Clone, Default)]
pub struct SimRun {
    pub trades: Vec<TradeRecord>,
    /// `(ts, realized_equity_usdc)` — cumulative realized P&L after each closed
    /// trade, seeded at 0.0. Used for the drawdown metric.
    pub equity_curve: Vec<(u64, f64)>,
}

impl SimRun {
    /// Σ(usdc_out − usdc_in) across all closed trades — the optimization objective.
    pub fn net_pnl(&self) -> f64 {
        self.trades.iter().map(|t| t.usdc_out - t.usdc_in).sum()
    }

    pub fn n_trades(&self) -> usize {
        self.trades.len()
    }

    /// Fraction of closed trades that were net-positive, as a percentage.
    pub fn win_rate(&self) -> f64 {
        summarize(&self.trades).win_rate_pct
    }

    /// Largest peak-to-trough decline of the realized-equity curve, as a percent
    /// of the running peak. `0.0` when the curve never falls below a prior peak.
    /// Peaks are measured against the cumulative realized P&L (seeded at 0), so a
    /// run that only ever loses money still reports a meaningful drawdown.
    pub fn max_drawdown_pct(&self) -> f64 {
        let mut peak = f64::NEG_INFINITY;
        let mut max_dd = 0.0_f64;
        for &(_, equity) in &self.equity_curve {
            peak = peak.max(equity);
            if peak > 0.0 {
                max_dd = max_dd.max((peak - equity) / peak * 100.0);
            }
        }
        max_dd
    }
}

/// Entry fill price: a buy crosses the spread + pays slippage, so it fills
/// *above* the observed mark.
fn entry_fill_price(snapshot_price: f64, slippage_bps: u32) -> f64 {
    snapshot_price * (1.0 + slippage_bps as f64 / 10_000.0)
}

/// Exit fill price: a sell fills *below* the observed mark by the slippage cushion.
fn exit_fill_price(snapshot_price: f64, slippage_bps: u32) -> f64 {
    snapshot_price * (1.0 - slippage_bps as f64 / 10_000.0)
}

/// Per-snapshot ranked-candidate stream for one ranking-knob tuple. Index `i`
/// holds the candidates `rank_candidates` would return at snapshot `i`, using a
/// bounded trailing window so each call is O(window) rather than O(i).
pub fn ranked_stream(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    params: &ParamSet,
) -> Vec<Vec<Candidate>> {
    let win = trailing_window_snaps(params);
    let mut out = Vec::with_capacity(snapshots.len());
    // One growing deque: each snapshot is cloned in once and dropped once, so the
    // whole pass is O(N) clones rather than O(N·window).
    let mut deque: VecDeque<PriceSnapshot> = VecDeque::with_capacity(win + 1);
    for snap in snapshots {
        deque.push_back(snap.clone());
        while deque.len() > win {
            deque.pop_front();
        }
        out.push(rank_candidates(
            watched,
            &snap.prices,
            &deque,
            params.lookback_obs,
            params.stale_minutes,
            params.metric,
            params.max_run_pct,
            params.decel_lookback_min,
            params.confirm_lag_obs,
        ));
    }
    out
}

/// Run the FLAT→HOLDING state machine over a precomputed ranked stream. Mirrors
/// the gate ordering of `maybe_enter` / `maybe_exit` / `maybe_take_profit_on_fade`,
/// minus all network I/O. Rotation is intentionally omitted: `MOMENTUM_ROTATE_MARGIN`
/// is frozen at its `.env` default (0 = disabled) and is not a swept knob, so
/// `try_rotate` never fires in the core grid.
pub fn replay_with_stream(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<Candidate>],
    params: &ParamSet,
) -> SimRun {
    let n = snapshots.len();
    let regime = regime_mask(snapshots, params.regime_filter_obs);
    let mut trades: Vec<TradeRecord> = Vec::new();
    let mut equity_curve: Vec<(u64, f64)> = Vec::new();
    if let Some(first) = snapshots.first() {
        equity_curve.push((first.ts, 0.0));
    }
    let mut realized = 0.0_f64;
    let mut position: Option<Position> = None;
    let mut last_exit_ts: HashMap<String, i64> = HashMap::new();
    let mut entry_tss: Vec<i64> = Vec::new(); // every entry, for the rolling daily cap

    let mut i = 0;
    while i < n {
        let snap = &snapshots[i];
        let ts = snap.ts as i64;
        let sol_price = snap.prices.get(SOL_KEY).copied().unwrap_or(0.0);

        if let Some(mut pos) = position.take() {
            // ── HOLDING ──────────────────────────────────────────────────────
            let Some(px) = snap.prices.get(&pos.mint).copied().filter(|p| *p > 0.0) else {
                position = Some(pos); // no fresh price — never trip the stop on a gap
                i += 1;
                continue;
            };
            if px > pos.peak_price_usd {
                pos.peak_price_usd = px;
            }
            // Trailing stop: fixed-% (Off) or volatility-scaled (Atr/Sigma when
            // chandelier_k>0). Both vol modes fall back to the fixed-% stop while their
            // window is still warming up. Shares `vol_stop_triggered` with the live trader.
            let fallback_stop = vol_stop_triggered(
                px,
                pos.peak_price_usd,
                params.trail_pct,
                params.vol_stop_mode,
                params.chandelier_k,
                token_atr(snapshots, i, &pos.mint, params.vol_obs),
                token_return_sigma(snapshots, i, &pos.mint, params.vol_obs),
            );
            // Profit-protected (max-trail) override: once green, ride a pullback down to
            // max(cost-breakeven floor, peak−max_trail%) instead of the tight stop; while
            // not yet green (or disabled) the fallback stop governs. Shared with live.
            let gas_bps = est_gas_bps(params.trade_usdc, sol_price);
            let round_trip_cost_frac =
                (2 * params.slippage_bps + 2 * gas_bps) as f64 / 10_000.0;
            let stop = profit_protected_stop_triggered(
                px,
                pos.peak_price_usd,
                pos.entry_price_usd,
                round_trip_cost_frac,
                params.max_trail_pct,
                fallback_stop,
            );
            // Overbought take-profit: while green, sell into a z-spike (≥ overbought_z).
            let overbought = params.overbought_z > 0.0
                && px > pos.entry_price_usd
                && token_dip_z(snapshots, i, &pos.mint, params.vol_obs)
                    .is_some_and(|z| z >= params.overbought_z);
            let is_equity = watched.iter().any(|w| w.mint == pos.mint && w.is_equity());
            let market_closed = is_equity
                && params.stale_minutes > 0
                && is_stale_ts(&recent_series(snapshots, i, &pos.mint), params.stale_minutes);

            // Hard time stop: the move didn't pay off within `max_hold_min` minutes.
            let max_hold_hit = params.max_hold_min > 0
                && (ts - pos.entry_ts) >= params.max_hold_min as i64 * 60;
            // Breakeven stop: went green (peak above entry), now back to/under entry → exit flat.
            let breakeven_hit = params.breakeven_exit
                && pos.peak_price_usd > pos.entry_price_usd
                && px <= pos.entry_price_usd;

            if stop || market_closed || overbought || max_hold_hit || breakeven_hit {
                // Conservative: stop *detected* at `i`, *fills* at the next snapshot
                // (~3 min later). Optimistic: fills same-bar at the tripping price.
                let (fill_idx, exit_mark, exit_ts, exit_sol) = if params.optimistic_fill {
                    (i, px, snap.ts, sol_price)
                } else {
                    let fi = (i + 1).min(n - 1);
                    let fs = &snapshots[fi];
                    let mark = fs.prices.get(&pos.mint).copied().filter(|p| *p > 0.0).unwrap_or(px);
                    (fi, mark, fs.ts, fs.prices.get(SOL_KEY).copied().unwrap_or(sol_price))
                };
                let proceeds = pos.token_amount * exit_fill_price(exit_mark, params.slippage_bps);
                let usdc_out = (proceeds - est_gas_usdc(exit_sol)).max(0.0);
                let rec = build_trade_record(&pos, exit_ts as i64, exit_mark, usdc_out, "sim".into());
                realized += rec.usdc_out - rec.usdc_in;
                last_exit_ts.insert(pos.mint.clone(), exit_ts as i64);
                equity_curve.push((exit_ts, realized));
                trades.push(rec);
                i = fill_idx + 1; // never re-enter on the bar we sold into
                continue;
            }

            // Rotation: keep capital in a stronger token (one A→B swap). Mirrors
            // try_rotate — checked before fade, after the protective stop. Disabled
            // when rotate_margin == 0.
            if params.rotate_margin > 0.0 {
                let used = entry_tss.iter().filter(|&&e| e >= ts - 86_400).count();
                let held = stream[i].iter().find(|c| c.mint == pos.mint);
                let held_ok = held.map(|c| (!c.stale, c.score));
                if used < params.max_trades_per_day as usize
                    && px > pos.entry_price_usd // gross-green pre-filter
                    && matches!(held_ok, Some((true, _)))
                {
                    let held_score = held_ok.unwrap().1;
                    if let Some(target) = rotation_target(
                        &stream[i],
                        &pos.mint,
                        held_score,
                        params.min_metric,
                        params.rotate_margin,
                        params.reentry_cooldown_secs,
                        ts,
                        &last_exit_ts,
                    ) {
                        let notional = pos.token_amount * px;
                        let gas_bps = est_gas_bps(notional, sol_price);
                        let cost_bps = params.slippage_bps + gas_bps;
                        if cost_bps <= params.max_cost_bps
                            && rotation_net_green(px, pos.entry_price_usd, cost_bps)
                        {
                            // One A→B swap nets slippage; gas hits the A-leg's realized
                            // P&L, B's basis stays at the gross post-slippage value.
                            let b_value = pos.token_amount * exit_fill_price(px, params.slippage_bps);
                            let realized_a = (b_value - est_gas_usdc(sol_price)).max(0.0);
                            let rec = build_trade_record(&pos, ts, px, realized_a, "sim-rotate".into());
                            realized += rec.usdc_out - rec.usdc_in;
                            last_exit_ts.insert(pos.mint.clone(), ts);
                            equity_curve.push((snap.ts, realized));
                            trades.push(rec);
                            // Open B with the carry-forward basis (no new entry slippage).
                            position = Some(Position {
                                mint: target.mint.clone(),
                                symbol: target.symbol.clone(),
                                entry_ts: ts,
                                entry_price_usd: target.price_usd,
                                token_amount: b_value / target.price_usd,
                                usdc_spent: b_value,
                                peak_price_usd: target.price_usd,
                                entry_sig: "sim-rotate".into(),
                                dry_run: true,
                            });
                            entry_tss.push(ts); // rotation counts against the daily cap
                            i += 1;
                            continue;
                        }
                    }
                }
            }

            // Fade exit: a slow-tick decision, so it fills at the current mark.
            if params.exit_on_fade {
                if let Some(c) = stream[i].iter().find(|c| c.mint == pos.mint) {
                    if !c.stale && fade_take_profit(c.score, params.min_metric, px, pos.entry_price_usd)
                    {
                        let proceeds = pos.token_amount * exit_fill_price(px, params.slippage_bps);
                        let usdc_out = (proceeds - est_gas_usdc(sol_price)).max(0.0);
                        let rec = build_trade_record(&pos, ts, px, usdc_out, "sim".into());
                        realized += rec.usdc_out - rec.usdc_in;
                        last_exit_ts.insert(pos.mint.clone(), ts);
                        equity_curve.push((snap.ts, realized));
                        trades.push(rec);
                        i += 1;
                        continue;
                    }
                }
            }
            position = Some(pos); // still riding
            i += 1;
            continue;
        }

        // ── FLAT — entry gates (mirror maybe_enter ordering) ─────────────────
        // Market-regime gate: stay in cash while the broad market is risk-off.
        if !regime[i] {
            i += 1;
            continue;
        }
        let cutoff = ts - 86_400;
        let used = entry_tss.iter().filter(|&&e| e >= cutoff).count();
        if used >= params.max_trades_per_day as usize {
            i += 1;
            continue;
        }
        let best = stream[i].iter().find(|c| {
            !c.stale
                && !c.overextended
                && !c.falling
                && !c.metric_fading
                && last_exit_ts
                    .get(&c.mint)
                    .is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
        });
        let Some(best) = best else {
            i += 1;
            continue;
        };
        if best.score <= params.min_metric {
            i += 1;
            continue;
        }
        // Mean-reversion entry confirmation ("both true"): the strong token must ALSO
        // be currently oversold (a pullback), else we'd buy the top. Skip the tick if
        // the leader isn't dipping. `entry_dip_obs == 0` disables.
        if params.entry_dip_obs > 0 {
            let oversold = token_dip_z(snapshots, i, &best.mint, params.entry_dip_obs)
                .is_some_and(|z| z <= -params.entry_dip_z);
            // Reversal confirmation: also require the bounce to have started (buy the
            // recovery, not the falling knife). `dip_confirm_obs == 0` ⇒ no confirmation.
            let bouncing = token_rising(snapshots, i, &best.mint, params.dip_confirm_obs);
            if !oversold || !bouncing {
                i += 1;
                continue;
            }
        }
        // Equity-compounding size: grow the notional with banked realized profit
        // (reinvest_frac=0 ⇒ fixed `trade_usdc`). Shared with the live trader.
        let size = dynamic_trade_usdc(
            params.trade_usdc,
            params.reinvest_frac,
            params.size_ceiling_usdc,
            realized,
        );
        let gas_bps = est_gas_bps(size, sol_price);
        if params.slippage_bps + gas_bps > params.max_cost_bps {
            i += 1;
            continue;
        }
        let entry_mark = best.price_usd;
        let token_amount = size / entry_fill_price(entry_mark, params.slippage_bps);
        position = Some(Position {
            mint: best.mint.clone(),
            symbol: best.symbol.clone(),
            entry_ts: ts,
            entry_price_usd: entry_mark,
            token_amount,
            usdc_spent: size + est_gas_usdc(sol_price),
            peak_price_usd: entry_mark,
            entry_sig: "sim".into(),
            dry_run: true,
        });
        entry_tss.push(ts);
        i += 1;
    }

    SimRun { trades, equity_curve }
}

/// Convenience: build the stream then replay it. Used by tests; the grid search
/// calls `ranked_stream` once and sweeps `replay_with_stream` for the factoring win.
pub fn replay(snapshots: &[PriceSnapshot], watched: &[WatchedToken], params: &ParamSet) -> SimRun {
    let stream = ranked_stream(snapshots, watched, params);
    replay_with_stream(snapshots, watched, stream.as_slice(), params)
}

/// Remove isolated glitch prints before replay. A recorded price is dropped when
/// it differs from BOTH its time-neighbors (the previous and next *present* price
/// for that token) by more than a factor `max_step` — the signature of a one-tick
/// data spike. A genuine sustained move keeps one neighbor close, so it survives
/// even when the step itself exceeds `max_step`. `max_step <= 1.0` disables the
/// filter (returns the input unchanged). Only the offending token's price is
/// dropped from that snapshot; the snapshot and all other tokens are preserved.
pub fn sanitize_history(snapshots: &[PriceSnapshot], max_step: f64) -> Vec<PriceSnapshot> {
    let mut out = snapshots.to_vec();
    if max_step <= 1.0 {
        return out; // disabled
    }
    // Per token, the list of (snapshot_index, price) over present, positive ticks.
    let mut by_token: HashMap<&str, Vec<(usize, f64)>> = HashMap::new();
    for (i, s) in snapshots.iter().enumerate() {
        for (m, &p) in &s.prices {
            if p > 0.0 {
                by_token.entry(m.as_str()).or_default().push((i, p));
            }
        }
    }
    let is_jump = |a: f64, b: f64| (a / b).max(b / a) > max_step;
    for (mint, series) in &by_token {
        // Carry the last *trusted* price so a glitch never poisons the comparison
        // for the points around it. A point is a spike when it jumps from the last
        // trusted value AND the next present value reverts toward that value (the
        // series came back) — a genuine new level instead keeps the next point near
        // the jump, so it's trusted and becomes the new baseline.
        let mut last_good = series[0].1;
        for k in 1..series.len() {
            let (idx, p) = series[k];
            let reverts = series.get(k + 1).is_some_and(|&(_, nx)| !is_jump(nx, last_good));
            if is_jump(p, last_good) && reverts {
                out[idx].prices.remove(*mint); // isolated spike — drop just this print
            } else {
                last_good = p;
            }
        }
    }
    out
}

// ───────────────────────────── grid search ─────────────────────────────────

/// The core grid's swept ranges. Edit to widen/narrow the search; the `--quick`
/// flag in the binary trims these to a smoke-test subset.
pub const GRID_METRICS: [RankMetric; 4] = [
    RankMetric::Sortino,
    RankMetric::Sharpe,
    RankMetric::SlopeR2,
    RankMetric::Return,
];
pub const GRID_LOOKBACKS: [usize; 4] = [121, 240, 480, 720];
pub const GRID_MAX_RUNS: [f64; 4] = [0.0, 6.0, 10.0, 15.0];
pub const GRID_TRAILS: [f64; 5] = [4.0, 6.0, 8.0, 10.0, 12.0];
/// Volatility-scaled trailing-stop sweep. ATR `k` is in price-units (stop = peak −
/// k·ATR); σ `k` is a %-multiplier (eff trail% = k·σ·100). The two scales are
/// independent and NOT interchangeable. `GRID_VOL_OBS` is the shared window.
pub const GRID_ATR_KS: [f64; 3] = [2.0, 3.0, 4.0];
pub const GRID_SIGMA_KS: [f64; 3] = [3.0, 5.0, 8.0];
pub const GRID_VOL_OBS: [usize; 2] = [60, 120];
/// Profit-protected ("max-trail") give-back sweep, percent. `0` is the fixed-trail
/// baseline (already covered by `GRID_TRAILS`); the rest let a green position give back
/// up to that much from its peak before exiting (floored at cost-breakeven).
pub const GRID_MAX_TRAILS: [f64; 3] = [15.0, 25.0, 40.0];
/// Equity-compounding sizing sweep: reinvest fractions of banked profit, and size
/// ceilings as multiples of the base `trade_usdc`. Off by default (the binary only
/// activates it when `--reinvest-fracs` is passed); `0.0` is the fixed-size baseline.
pub const GRID_REINVEST_FRACS: [f64; 4] = [0.0, 0.25, 0.5, 1.0];
pub const GRID_SIZE_CEILING_MULTS: [f64; 3] = [2.0, 3.0, 5.0];
/// Probabilities at which per-metric `min_metric` thresholds are sampled from the
/// train-slice score distribution (p50 = enter often … p95 = strongest signals only).
pub const GRID_MIN_QUANTILES: [f64; 4] = [0.50, 0.70, 0.85, 0.95];

/// One trailing-stop configuration in the grid sweep. `Off` enumerates the fixed
/// trail widths; `Atr`/`Sigma` carry their multiplier `k` + window, with `trail_pct`
/// as the warmup fallback.
#[derive(Debug, Clone, Copy)]
pub struct StopVariant {
    pub mode: VolStopMode,
    pub k: f64,
    pub vol_obs: usize,
    pub trail_pct: f64,
    /// Profit-protected give-back cap (percent); `0` = off (plain trailing stop).
    pub max_trail_pct: f64,
}

/// Build the trailing-stop sweep dimension: one `Off` variant per fixed trail width,
/// plus each `Atr`/`Sigma` × `k` × `vol_obs` combo at a single representative fallback
/// trail. Deliberately ADDITIVE (≈ `|trails| + (|atr_ks|+|sigma_ks|)·|vol_obs|`), not a
/// cross-product with the trail loop — so enabling vol stops grows the grid by a
/// constant, not a multiple. Pass empty `atr_ks`+`sigma_ks` to sweep fixed stops only.
pub fn stop_variants(
    trails: &[f64],
    atr_ks: &[f64],
    sigma_ks: &[f64],
    vol_obs_set: &[usize],
    max_trails: &[f64],
) -> Vec<StopVariant> {
    // Active vol-stop / max-trail variants fall back to this trail only while not yet
    // green (or warming up), so a single representative width suffices (median trail).
    let fallback = trails.get(trails.len() / 2).copied().unwrap_or(8.0);
    let mut out: Vec<StopVariant> = trails
        .iter()
        .map(|&t| StopVariant {
            mode: VolStopMode::Off,
            k: 0.0,
            vol_obs: 0,
            trail_pct: t,
            max_trail_pct: 0.0,
        })
        .collect();
    for &obs in vol_obs_set {
        for &k in atr_ks {
            out.push(StopVariant {
                mode: VolStopMode::Atr,
                k,
                vol_obs: obs,
                trail_pct: fallback,
                max_trail_pct: 0.0,
            });
        }
        for &k in sigma_ks {
            out.push(StopVariant {
                mode: VolStopMode::Sigma,
                k,
                vol_obs: obs,
                trail_pct: fallback,
                max_trail_pct: 0.0,
            });
        }
    }
    // Profit-protected give-back variants: fixed not-green stop at the fallback trail,
    // green positions ride down to max(cost-breakeven, peak−max_trail%). Additive.
    for &mt in max_trails {
        if mt > 0.0 {
            out.push(StopVariant {
                mode: VolStopMode::Off,
                k: 0.0,
                vol_obs: 0,
                trail_pct: fallback,
                max_trail_pct: mt,
            });
        }
    }
    out
}

/// Build the position-sizing sweep as `(reinvest_frac, ceiling_usdc)` pairs. A
/// non-positive fraction collapses to the single fixed-size baseline `(0, base)`;
/// each positive fraction yields one pair per ceiling multiple (× base, floored at
/// base). Empty input ⇒ just the fixed baseline (sizing off). Off-by-default: the grid
/// sizes dynamically only when positive fractions are requested.
pub fn sizing_variants(base: f64, reinvest_fracs: &[f64], ceiling_mults: &[f64]) -> Vec<(f64, f64)> {
    let mut out: Vec<(f64, f64)> = Vec::new();
    let mut has_fixed = false;
    for &f in reinvest_fracs {
        if f <= 0.0 {
            if !has_fixed {
                out.push((0.0, base));
                has_fixed = true;
            }
        } else {
            for &m in ceiling_mults {
                out.push((f, (m * base).max(base)));
            }
        }
    }
    if out.is_empty() {
        out.push((0.0, base));
    }
    out
}

/// One scored point in the grid: the params plus train/test performance.
#[derive(Debug, Clone)]
pub struct SimResult {
    pub params: ParamSet,
    pub net_pnl_train: f64,
    pub n_trades_train: usize,
    pub net_pnl_test: f64,
    pub n_trades_test: usize,
    pub win_rate_test: f64,
    pub max_dd_test: f64,
}

/// A config is *robust* only if it makes money in BOTH the train and the held-out
/// test slice AND trades at least `min_trades` times in each — so a lone lucky
/// trade (e.g. 1 trade / 100% win) can never masquerade as an edge. Shared by the
/// momentum and mean-reversion result types.
pub fn config_is_robust(
    net_pnl_train: f64,
    net_pnl_test: f64,
    n_trades_train: usize,
    n_trades_test: usize,
    min_trades: usize,
) -> bool {
    net_pnl_train > 0.0
        && net_pnl_test > 0.0
        && n_trades_train >= min_trades
        && n_trades_test >= min_trades
}

impl SimResult {
    pub fn is_robust(&self, min_trades: usize) -> bool {
        config_is_robust(
            self.net_pnl_train,
            self.net_pnl_test,
            self.n_trades_train,
            self.n_trades_test,
            min_trades,
        )
    }
}

/// Candidate `min_metric` entry thresholds for ONE metric, derived from the
/// distribution of that metric's best-candidate scores over the **train** slice.
///
/// ⚠️ LEARNING-MODE HANDOFF — you implement this body. It's the one place where a
/// genuine modeling decision (not boilerplate) shapes the whole search:
///
///   • The four metrics live on wildly different scales (sortino≈0.5, slope_r2≈1e3,
///     return≈0.01). A single shared threshold grid is meaningless — thresholds must
///     be read off each metric's OWN observed distribution. That's why this takes
///     `scores` and returns data-derived levels rather than fixed constants.
///   • Using QUANTILES makes the grid adaptive: a low quantile (p50) enters often
///     (more trades, more noise); a high one (p95) only on the strongest signals
///     (fewer, higher-conviction trades). Sweeping several lets the search find the
///     selectivity that actually pays.
///
/// Decisions that are yours to make (and that change results):
///   1. Which scores feed the distribution? Momentum is often absent, so `scores`
///      contains ≤0 values. Including them drags thresholds down (more entries);
///      filtering to positive-only makes the grid pickier. Pick one and own it.
///   2. How to turn a probability into a level — nearest-rank vs interpolation.
///   3. Degenerate inputs: empty `scores`, or all-equal scores. Return something the
///      grid can sweep (the contract test below pins the edges).
///
/// Contract (see `min_metric_candidates_*` tests): one level per `prob` (deduped is
/// fine), non-decreasing, each within the observed score range; empty `scores` ⇒
/// empty vec.
pub fn min_metric_candidates(scores: &[f64], probs: &[f64]) -> Vec<f64> {
    // Default stance: thresholds are read off the POSITIVE scores only. A non-positive
    // best-score means momentum was absent that snapshot; including those would anchor
    // the distribution near/below zero and the grid would always be tempted to enter.
    // Gating off "what a real signal looks like" keeps every swept threshold meaningful.
    let mut positive: Vec<f64> = scores.iter().copied().filter(|s| s.is_finite() && *s > 0.0).collect();
    if positive.is_empty() {
        return Vec::new();
    }
    positive.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // Nearest-rank quantile: one level per probability (duplicates are fine — the grid
    // just evaluates the same threshold twice, which is cheap and keeps the mapping
    // 1:1 with `probs` for reporting).
    let last = positive.len() - 1;
    probs
        .iter()
        .map(|&p| {
            let idx = (p.clamp(0.0, 1.0) * last as f64).round() as usize;
            positive[idx.min(last)]
        })
        .collect()
}

/// Walk-forward grid search. Computes the expensive ranked stream once per
/// `(metric, lookback, max_run)` tuple, derives per-metric thresholds from the
/// train slice, then sweeps `trail × min_metric` cheaply over both slices.
/// Results are returned sorted by held-out (`net_pnl_test`) P&L, best first.
#[allow(clippy::too_many_arguments)]
pub fn run_grid(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    watched: &[WatchedToken],
    base: &ParamSet,
    metrics: &[RankMetric],
    lookbacks: &[usize],
    max_runs: &[f64],
    trails: &[f64],
    quantile_probs: &[f64],
    rotate_factors: &[f64],
    regime_obs_set: &[usize],
    atr_ks: &[f64],
    sigma_ks: &[f64],
    vol_obs_set: &[usize],
    max_trails: &[f64],
    reinvest_fracs: &[f64],
    size_ceiling_mults: &[f64],
) -> Vec<SimResult> {
    let variants = stop_variants(trails, atr_ks, sigma_ks, vol_obs_set, max_trails);
    let sizing = sizing_variants(base.trade_usdc, reinvest_fracs, size_ceiling_mults);
    let mut results = Vec::new();
    for &metric in metrics {
        for &lookback in lookbacks {
            for &max_run in max_runs {
                let mut rp = base.clone();
                rp.metric = metric;
                rp.lookback_obs = lookback;
                rp.max_run_pct = max_run;

                // Expensive part — once per ranking tuple.
                let train_stream = ranked_stream(train, watched, &rp);
                let test_stream = ranked_stream(test, watched, &rp);

                // Per-metric thresholds from the TRAIN distribution only (no peeking).
                let train_best_scores: Vec<f64> =
                    train_stream.iter().filter_map(|r| r.first().map(|c| c.score)).collect();
                let mins = min_metric_candidates(&train_best_scores, quantile_probs);

                for v in &variants {
                    for &min_metric in &mins {
                        for &rf in rotate_factors {
                            for &regime in regime_obs_set {
                                for &(reinvest, ceil) in &sizing {
                                    let mut p = rp.clone();
                                    p.trail_pct = v.trail_pct;
                                    p.vol_stop_mode = v.mode;
                                    p.chandelier_k = v.k;
                                    p.vol_obs = v.vol_obs;
                                    p.max_trail_pct = v.max_trail_pct;
                                    p.min_metric = min_metric;
                                    // rotate_margin is in the active metric's units, so scale it
                                    // off the (same-units) entry threshold; 0 disables rotation.
                                    p.rotate_margin = if rf > 0.0 { rf * min_metric } else { 0.0 };
                                    p.regime_filter_obs = regime;
                                    p.reinvest_frac = reinvest;
                                    p.size_ceiling_usdc = ceil;
                                    let tr = replay_with_stream(train, watched, &train_stream, &p);
                                    let te = replay_with_stream(test, watched, &test_stream, &p);
                                    results.push(SimResult {
                                        params: p,
                                        net_pnl_train: tr.net_pnl(),
                                        n_trades_train: tr.n_trades(),
                                        net_pnl_test: te.net_pnl(),
                                        n_trades_test: te.n_trades(),
                                        win_rate_test: te.win_rate(),
                                        max_dd_test: te.max_drawdown_pct(),
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    results.sort_by(|a, b| {
        b.net_pnl_test
            .partial_cmp(&a.net_pnl_test)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

// ─────────────────────── mean-reversion strategy ───────────────────────────
//
// The inverse of momentum: buy a token when it is statistically *oversold* versus
// its own recent mean (z-score ≤ −z_entry) and sell when it reverts toward the
// mean (z ≥ z_exit), with a hard stop if it keeps falling (z ≤ −z_stop). Long-only
// and spot-executable. Reuses the same `SimRun`, fill model, walk-forward split,
// and robustness verdict as the momentum path — only the candidate ranking and the
// entry/exit predicates change.

/// Minimum observations in the lookback window for a stable mean/σ z-score.
pub const MEANREV_MIN_OBS: usize = 30;

#[derive(Debug, Clone)]
pub struct MeanRevParams {
    pub lookback_obs: usize,
    /// Enter when z ≤ −z_entry (oversold). `z_entry > 0`.
    pub z_entry: f64,
    /// Exit (take profit) when z ≥ z_exit (reverted toward/through the mean).
    pub z_exit: f64,
    /// Stop out when z ≤ −z_stop (kept falling — a broken level, not a dip). `z_stop > z_entry`.
    pub z_stop: f64,
    /// Trend filter ("buy the pullback in an uptrend"): only take an oversold entry
    /// when the token's price is above the mean of its last `trend_filter_obs`
    /// observations (a confirmed uptrend). `0` disables — buy any dip.
    pub trend_filter_obs: usize,
    // ----- shared frozen knobs -----
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    pub trade_usdc: f64,
    pub slippage_bps: u32,
    pub max_cost_bps: u32,
}

/// One oversold/overbought candidate: a token with its current z-score.
#[derive(Debug, Clone)]
pub struct ZCandidate {
    pub symbol: String,
    pub mint: String,
    pub price_usd: f64,
    pub z: f64,
}

/// Mean-reversion grid result (mirrors `SimResult` but carries `MeanRevParams`).
#[derive(Debug, Clone)]
pub struct MeanRevResult {
    pub params: MeanRevParams,
    pub net_pnl_train: f64,
    pub n_trades_train: usize,
    pub net_pnl_test: f64,
    pub n_trades_test: usize,
    pub win_rate_test: f64,
    pub max_dd_test: f64,
}

impl MeanRevResult {
    pub fn is_robust(&self, min_trades: usize) -> bool {
        config_is_robust(
            self.net_pnl_train,
            self.net_pnl_test,
            self.n_trades_train,
            self.n_trades_test,
            min_trades,
        )
    }
}

/// Price z-score over a window: `(last − mean) / σ`. `None` below the obs floor or
/// when σ is ~0 (a flat series has no meaningful deviation).
fn zscore(window: &[(u64, f64)]) -> Option<f64> {
    let prices: Vec<f64> = window.iter().map(|&(_, p)| p).filter(|p| *p > 0.0).collect();
    if prices.len() < MEANREV_MIN_OBS {
        return None;
    }
    let n = prices.len() as f64;
    let m = prices.iter().sum::<f64>() / n;
    let var = prices.iter().map(|p| (p - m).powi(2)).sum::<f64>() / n;
    let sd = var.sqrt();
    if sd < 1e-12 {
        return None;
    }
    Some((prices.last().unwrap() - m) / sd)
}

/// Per-snapshot z-score candidates, most-oversold (lowest z) first.
pub fn meanrev_stream(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    params: &MeanRevParams,
) -> Vec<Vec<ZCandidate>> {
    let win = params.lookback_obs.max(MEANREV_MIN_OBS) + WINDOW_PAD;
    let mut out = Vec::with_capacity(snapshots.len());
    let mut deque: VecDeque<PriceSnapshot> = VecDeque::with_capacity(win + 1);
    for snap in snapshots {
        deque.push_back(snap.clone());
        while deque.len() > win {
            deque.pop_front();
        }
        let mut cands: Vec<ZCandidate> = Vec::new();
        for w in watched {
            let series: Vec<(u64, f64)> = deque
                .iter()
                .filter_map(|s| s.prices.get(&w.mint).map(|p| (s.ts, *p)))
                .filter(|(_, p)| *p > 0.0)
                .collect();
            // Use the trailing `lookback_obs` observations for the z-score.
            let window: &[(u64, f64)] = if series.len() > params.lookback_obs {
                &series[series.len() - params.lookback_obs..]
            } else {
                &series
            };
            let Some(price) = snap.prices.get(&w.mint).copied().filter(|p| *p > 0.0) else {
                continue;
            };
            if let Some(z) = zscore(window) {
                cands.push(ZCandidate {
                    symbol: w.symbol.clone(),
                    mint: w.mint.clone(),
                    price_usd: price,
                    z,
                });
            }
        }
        // Most oversold first.
        cands.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));
        out.push(cands);
    }
    out
}

/// Trend filter: is `mint` in a confirmed uptrend at snapshot `i` — its current
/// price above the mean of its last `trend_obs` observations? `trend_obs == 0`, or
/// too little history, ⇒ `true` (don't filter). Cheap (computed per entry-check),
/// so it stays a state-machine knob and never forces a z-stream recompute.
fn token_uptrend(snapshots: &[PriceSnapshot], i: usize, mint: &str, trend_obs: usize) -> bool {
    if trend_obs == 0 {
        return true;
    }
    let lo = i.saturating_sub(trend_obs);
    let prices: Vec<f64> = snapshots[lo..=i]
        .iter()
        .filter_map(|s| s.prices.get(mint).copied())
        .filter(|p| *p > 0.0)
        .collect();
    if prices.len() < 2 {
        return true;
    }
    let cur = *prices.last().unwrap();
    let mean = prices.iter().sum::<f64>() / prices.len() as f64;
    cur > mean
}

/// FLAT→HOLDING mean-reversion state machine over a precomputed z stream.
pub fn replay_meanrev(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<ZCandidate>],
    params: &MeanRevParams,
) -> SimRun {
    let _ = watched;
    let mut trades: Vec<TradeRecord> = Vec::new();
    let mut equity_curve: Vec<(u64, f64)> = Vec::new();
    if let Some(first) = snapshots.first() {
        equity_curve.push((first.ts, 0.0));
    }
    let mut realized = 0.0_f64;
    let mut position: Option<Position> = None;
    let mut last_exit_ts: HashMap<String, i64> = HashMap::new();
    let mut entry_tss: Vec<i64> = Vec::new();

    for (i, snap) in snapshots.iter().enumerate() {
        let ts = snap.ts as i64;
        let sol_price = snap.prices.get(SOL_KEY).copied().unwrap_or(0.0);

        if let Some(pos) = position.take() {
            // ── HOLDING: exit on reversion (z ≥ z_exit) or stop (z ≤ −z_stop) ──
            let z = stream[i].iter().find(|c| c.mint == pos.mint).map(|c| c.z);
            let px = snap.prices.get(&pos.mint).copied().filter(|p| *p > 0.0);
            match (z, px) {
                (Some(z), Some(px)) if z >= params.z_exit || z <= -params.z_stop => {
                    let proceeds = pos.token_amount * exit_fill_price(px, params.slippage_bps);
                    let usdc_out = (proceeds - est_gas_usdc(sol_price)).max(0.0);
                    let rec = build_trade_record(&pos, ts, px, usdc_out, "sim-meanrev".into());
                    realized += rec.usdc_out - rec.usdc_in;
                    last_exit_ts.insert(pos.mint.clone(), ts);
                    equity_curve.push((snap.ts, realized));
                    trades.push(rec);
                }
                _ => position = Some(pos), // keep holding (no price, or not yet reverted/stopped)
            }
            continue;
        }

        // ── FLAT: enter the most-oversold token past the z_entry threshold ──
        let used = entry_tss.iter().filter(|&&e| e >= ts - 86_400).count();
        if used >= params.max_trades_per_day as usize {
            continue;
        }
        let best = stream[i].iter().find(|c| {
            c.z <= -params.z_entry
                && token_uptrend(snapshots, i, &c.mint, params.trend_filter_obs)
                && last_exit_ts
                    .get(&c.mint)
                    .is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
        });
        let Some(best) = best else { continue };
        let gas_bps = est_gas_bps(params.trade_usdc, sol_price);
        if params.slippage_bps + gas_bps > params.max_cost_bps {
            continue;
        }
        let entry_mark = best.price_usd;
        position = Some(Position {
            mint: best.mint.clone(),
            symbol: best.symbol.clone(),
            entry_ts: ts,
            entry_price_usd: entry_mark,
            token_amount: params.trade_usdc / entry_fill_price(entry_mark, params.slippage_bps),
            usdc_spent: params.trade_usdc + est_gas_usdc(sol_price),
            peak_price_usd: entry_mark,
            entry_sig: "sim-meanrev".into(),
            dry_run: true,
        });
        entry_tss.push(ts);
    }

    SimRun { trades, equity_curve }
}

/// Mean-reversion grid ranges (edit to widen/narrow). `--quick` trims them.
pub const MR_LOOKBACKS: [usize; 4] = [60, 120, 240, 480];
pub const MR_Z_ENTRY: [f64; 4] = [1.5, 2.0, 2.5, 3.0];
pub const MR_Z_EXIT: [f64; 3] = [-0.5, 0.0, 0.5];
pub const MR_Z_STOP: [f64; 3] = [3.0, 4.0, 5.0];

/// Walk-forward grid search for the mean-reversion strategy. The z stream depends
/// only on the lookback, so it's computed once per lookback and the z-threshold
/// triple swept cheaply over it (the same factoring as the momentum grid).
#[allow(clippy::too_many_arguments)]
pub fn run_grid_meanrev(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    watched: &[WatchedToken],
    base: &MeanRevParams,
    lookbacks: &[usize],
    z_entries: &[f64],
    z_exits: &[f64],
    z_stops: &[f64],
    trend_filter_set: &[usize],
) -> Vec<MeanRevResult> {
    let mut results = Vec::new();
    for &lb in lookbacks {
        let mut bp = base.clone();
        bp.lookback_obs = lb;
        let train_stream = meanrev_stream(train, watched, &bp);
        let test_stream = meanrev_stream(test, watched, &bp);
        for &ze in z_entries {
            for &zx in z_exits {
                for &zs in z_stops {
                    if zs <= ze {
                        continue; // a stop must sit deeper than the entry
                    }
                    // trend_filter_obs is state-machine only → sweep it over the same
                    // cached z-streams (no recompute).
                    for &tf in trend_filter_set {
                        let mut p = bp.clone();
                        p.z_entry = ze;
                        p.z_exit = zx;
                        p.z_stop = zs;
                        p.trend_filter_obs = tf;
                        let tr = replay_meanrev(train, watched, &train_stream, &p);
                        let te = replay_meanrev(test, watched, &test_stream, &p);
                        results.push(MeanRevResult {
                            params: p,
                            net_pnl_train: tr.net_pnl(),
                            n_trades_train: tr.n_trades(),
                            net_pnl_test: te.net_pnl(),
                            n_trades_test: te.n_trades(),
                            win_rate_test: te.win_rate(),
                            max_dd_test: te.max_drawdown_pct(),
                        });
                    }
                }
            }
        }
    }
    results.sort_by(|a, b| {
        b.net_pnl_test
            .partial_cmp(&a.net_pnl_test)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// Convenience: build the z stream then replay it (used by tests).
pub fn replay_meanrev_full(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    params: &MeanRevParams,
) -> SimRun {
    let stream = meanrev_stream(snapshots, watched, params);
    replay_meanrev(snapshots, watched, stream.as_slice(), params)
}

// ──────────────────── market-neutral pairs (Phase 0) ───────────────────────
//
// Edge-validation only: simulate the *dollar-neutral two-leg* P&L of trading a
// spread `ln(A/B)` from recorded prices, to answer "does the spread converge
// profitably?" before building any perps execution. Long the laggard + short the
// leader, sized equal; profit = relative move, market direction cancels. Shorting
// on-chain is not modeled here — this is purely whether the edge exists.

pub const PAIRS_MIN_OBS: usize = 30;

#[derive(Debug, Clone)]
pub struct PairParams {
    pub lookback_obs: usize,
    /// Enter when |z| ≥ z_entry (spread stretched).
    pub z_entry: f64,
    /// Exit when |z| ≤ z_exit (reverted toward the mean).
    pub z_exit: f64,
    /// Stop when |z| ≥ z_stop (spread blew out further — broken relationship).
    pub z_stop: f64,
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    /// Notional per leg (gross exposure ≈ 2× this; capital ≈ margin).
    pub notional_usdc: f64,
    /// Round-trip cost charged per leg-trade (slippage + fee), in bps of notional.
    /// A pair round-trip pays this 4× (open A, open B, close A, close B).
    pub cost_bps: u32,
    /// Funding/borrow drag per day held on the position, in bps of notional.
    pub funding_bps_per_day: f64,
    /// Reversal-confirmation entry filter: only enter once |z| is *shrinking* vs this many
    /// observations ago (the spread has turned back toward the mean), instead of the instant
    /// |z| ≥ z_entry. Avoids entering into a still-diverging spread (a "knife"). 0 = off.
    pub entry_confirm_obs: usize,
}

#[derive(Debug, Clone, Default)]
pub struct PairRun {
    pub pnls: Vec<f64>,
    pub equity_curve: Vec<(u64, f64)>,
}

impl PairRun {
    pub fn net_pnl(&self) -> f64 {
        self.pnls.iter().sum()
    }
    pub fn n_trades(&self) -> usize {
        self.pnls.len()
    }
    pub fn win_rate(&self) -> f64 {
        if self.pnls.is_empty() {
            return 0.0;
        }
        self.pnls.iter().filter(|&&p| p > 0.0).count() as f64 / self.pnls.len() as f64 * 100.0
    }
    pub fn max_drawdown_pct(&self) -> f64 {
        let mut peak = f64::NEG_INFINITY;
        let mut dd = 0.0_f64;
        for &(_, e) in &self.equity_curve {
            peak = peak.max(e);
            if peak > 0.0 {
                dd = dd.max((peak - e) / peak * 100.0);
            }
        }
        dd
    }
}

#[derive(Debug, Clone)]
pub struct PairResult {
    pub symbol_a: String,
    pub symbol_b: String,
    pub params: PairParams,
    pub net_pnl_train: f64,
    pub n_trades_train: usize,
    pub net_pnl_test: f64,
    pub n_trades_test: usize,
    pub win_rate_test: f64,
    pub max_dd_test: f64,
}

impl PairResult {
    pub fn is_robust(&self, min_trades: usize) -> bool {
        config_is_robust(
            self.net_pnl_train,
            self.net_pnl_test,
            self.n_trades_train,
            self.n_trades_test,
            min_trades,
        )
    }
}

/// Aligned `(ts, ln(a/b), a, b)` series over snapshots where BOTH legs are priced.
pub fn pair_series(
    snapshots: &[PriceSnapshot],
    mint_a: &str,
    mint_b: &str,
) -> Vec<(u64, f64, f64, f64)> {
    snapshots
        .iter()
        .filter_map(|s| {
            let a = s.prices.get(mint_a).copied().filter(|p| *p > 0.0)?;
            let b = s.prices.get(mint_b).copied().filter(|p| *p > 0.0)?;
            let spread = (a / b).ln();
            spread.is_finite().then_some((s.ts, spread, a, b))
        })
        .collect()
}

/// z-score of the last value of `xs` versus the slice (population σ). `None` below
/// the obs floor or when σ ≈ 0.
pub fn zscore_last(xs: &[f64]) -> Option<f64> {
    if xs.len() < PAIRS_MIN_OBS {
        return None;
    }
    let n = xs.len() as f64;
    let m = xs.iter().sum::<f64>() / n;
    let var = xs.iter().map(|x| (x - m).powi(2)).sum::<f64>() / n;
    let sd = var.sqrt();
    if sd < 1e-12 {
        return None;
    }
    Some((xs.last().unwrap() - m) / sd)
}

/// Replay the dollar-neutral spread strategy over one pair's aligned series.
///
/// State: FLAT until |z| ≥ z_entry, then hold the spread until it reverts
/// (|z| ≤ z_exit) or blows out (|z| ≥ z_stop). `z > 0` ⇒ A rich ⇒ short A / long B;
/// `z < 0` ⇒ long A / short B. P&L is the dollar-neutral two-leg return on
/// `notional_usdc` per leg, minus 4 leg-costs and the funding drag over the hold.
pub fn replay_pairs(series: &[(u64, f64, f64, f64)], params: &PairParams) -> PairRun {
    let mut run = PairRun::default();
    if let Some(&(ts0, ..)) = series.first() {
        run.equity_curve.push((ts0, 0.0));
    }
    let mut realized = 0.0_f64;
    // Open position: (entry_ts, a0, b0, long_a) — long_a = true ⇒ long A / short B.
    let mut open: Option<(u64, f64, f64, bool)> = None;
    let mut last_exit_ts: i64 = i64::MIN / 2;
    let mut entry_tss: Vec<i64> = Vec::new();
    // Recent z values for the reversal-confirmation entry filter (kept to confirm+1 long).
    let mut z_hist: VecDeque<f64> = VecDeque::new();

    let n = params.notional_usdc;
    let leg_cost = n * params.cost_bps as f64 / 10_000.0; // one leg-trade

    for i in 0..series.len() {
        let (ts, _, a, b) = series[i];
        let lo = (i + 1).saturating_sub(params.lookback_obs);
        let window: Vec<f64> = series[lo..=i].iter().map(|&(_, s, ..)| s).collect();
        let Some(z) = zscore_last(&window) else { continue };
        if params.entry_confirm_obs > 0 {
            z_hist.push_back(z);
            if z_hist.len() > params.entry_confirm_obs + 1 {
                z_hist.pop_front();
            }
        }

        if let Some((ts0, a0, b0, long_a)) = open {
            if z.abs() <= params.z_exit || z.abs() >= params.z_stop {
                // Two-leg dollar-neutral return.
                let (ra, rb) = (a / a0 - 1.0, b / b0 - 1.0);
                let gross = if long_a { ra - rb } else { rb - ra } * n;
                let hold_days = (ts.saturating_sub(ts0)) as f64 / 86_400.0;
                let funding = n * params.funding_bps_per_day / 10_000.0 * hold_days;
                let net = gross - 4.0 * leg_cost - funding;
                realized += net;
                run.pnls.push(net);
                run.equity_curve.push((ts, realized));
                last_exit_ts = ts as i64;
                open = None;
            }
            continue;
        }

        // FLAT — enter on a stretched-but-not-broken spread.
        if z.abs() >= params.z_entry && z.abs() < params.z_stop {
            // Reversal confirmation: only enter once |z| is shrinking vs entry_confirm_obs
            // ago (the spread has turned back toward the mean), not while it's still
            // diverging (catching a knife). Needs a full history window first.
            if params.entry_confirm_obs > 0
                && !(z_hist.len() == params.entry_confirm_obs + 1
                    && z.abs() < z_hist.front().copied().unwrap_or(f64::INFINITY).abs())
            {
                continue;
            }
            let cutoff = ts as i64 - 86_400;
            let used = entry_tss.iter().filter(|&&e| e >= cutoff).count();
            if used >= params.max_trades_per_day as usize {
                continue;
            }
            if (ts as i64) - last_exit_ts < params.reentry_cooldown_secs {
                continue;
            }
            open = Some((ts, a, b, z < 0.0)); // z<0 ⇒ A cheap ⇒ long A / short B
            entry_tss.push(ts as i64);
        }
    }
    run
}

/// Walk-forward grid over every pair × z-knob combination, ranked by held-out P&L.
#[allow(clippy::too_many_arguments)]
pub fn run_grid_pairs(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    pairs: &[(WatchedToken, WatchedToken)],
    base: &PairParams,
    lookbacks: &[usize],
    z_entries: &[f64],
    z_exits: &[f64],
    z_stops: &[f64],
    confirms: &[usize],
) -> Vec<PairResult> {
    let mut results = Vec::new();
    for (a, b) in pairs {
        let train_series = pair_series(train, &a.mint, &b.mint);
        let test_series = pair_series(test, &a.mint, &b.mint);
        // Skip pairs that can't even warm up in both slices.
        if train_series.len() < PAIRS_MIN_OBS || test_series.len() < PAIRS_MIN_OBS {
            continue;
        }
        for &lb in lookbacks {
            for &ze in z_entries {
                for &zx in z_exits {
                    for &zs in z_stops {
                        if zs <= ze {
                            continue;
                        }
                        for &cf in confirms {
                            let mut p = base.clone();
                            p.lookback_obs = lb;
                            p.z_entry = ze;
                            p.z_exit = zx;
                            p.z_stop = zs;
                            p.entry_confirm_obs = cf;
                            let tr = replay_pairs(&train_series, &p);
                            let te = replay_pairs(&test_series, &p);
                            results.push(PairResult {
                                symbol_a: a.symbol.clone(),
                                symbol_b: b.symbol.clone(),
                                params: p,
                                net_pnl_train: tr.net_pnl(),
                                n_trades_train: tr.n_trades(),
                                net_pnl_test: te.net_pnl(),
                                n_trades_test: te.n_trades(),
                                win_rate_test: te.win_rate(),
                                max_dd_test: te.max_drawdown_pct(),
                            });
                        }
                    }
                }
            }
        }
    }
    results.sort_by(|x, y| {
        y.net_pnl_test
            .partial_cmp(&x.net_pnl_test)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

// ───────────── long-only relative-value (on-chain executable) ───────────────
//
// The spot-tradeable capture of the pairs edge: when the spread says A is cheap
// relative to a correlated B (z ≤ −entry), go LONG A on spot (no short); when B is
// cheap (z ≥ +entry), go long B. Exit on reversion (|z| ≤ exit) or break (|z| ≥
// stop). You forfeit the market-neutral hedge, but you only ever buy the
// statistically-cheap leg of a proven-convergent pair — a far better long than
// naive momentum, and it requires nothing the spot bot can't already do.

#[derive(Debug, Clone)]
pub struct RelValParams {
    pub lookback_obs: usize,
    pub z_entry: f64,
    pub z_exit: f64,
    pub z_stop: f64,
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    pub trade_usdc: f64,
    pub slippage_bps: u32,
    pub max_cost_bps: u32,
}

#[derive(Debug, Clone)]
pub struct RelValResult {
    pub symbol_a: String,
    pub symbol_b: String,
    pub params: RelValParams,
    pub net_pnl_train: f64,
    pub n_trades_train: usize,
    pub net_pnl_test: f64,
    pub n_trades_test: usize,
    pub win_rate_test: f64,
    pub max_dd_test: f64,
}

impl RelValResult {
    pub fn is_robust(&self, min_trades: usize) -> bool {
        config_is_robust(
            self.net_pnl_train,
            self.net_pnl_test,
            self.n_trades_train,
            self.n_trades_test,
            min_trades,
        )
    }
}

/// Aligned `(ts, ln(a/b), a, b, sol)` series (sol carried for gas costing).
pub fn relval_series(
    snapshots: &[PriceSnapshot],
    mint_a: &str,
    mint_b: &str,
) -> Vec<(u64, f64, f64, f64, f64)> {
    snapshots
        .iter()
        .filter_map(|s| {
            let a = s.prices.get(mint_a).copied().filter(|p| *p > 0.0)?;
            let b = s.prices.get(mint_b).copied().filter(|p| *p > 0.0)?;
            let spread = (a / b).ln();
            let sol = s.prices.get(SOL_KEY).copied().unwrap_or(0.0);
            spread.is_finite().then_some((s.ts, spread, a, b, sol))
        })
        .collect()
}

/// Replay the long-only relative-value strategy over one pair's aligned series.
/// Holds exactly one leg long at a time; `(mint_a, sym_a)` / `(mint_b, sym_b)` label
/// the legs for the trade records.
#[allow(clippy::too_many_arguments)]
pub fn replay_relval(
    series: &[(u64, f64, f64, f64, f64)],
    mint_a: &str,
    sym_a: &str,
    mint_b: &str,
    sym_b: &str,
    params: &RelValParams,
) -> SimRun {
    let mut trades: Vec<TradeRecord> = Vec::new();
    let mut equity_curve: Vec<(u64, f64)> = Vec::new();
    if let Some(&(ts0, ..)) = series.first() {
        equity_curve.push((ts0, 0.0));
    }
    let mut realized = 0.0_f64;
    let mut position: Option<Position> = None;
    let mut last_exit_ts: i64 = i64::MIN / 2;
    let mut entry_tss: Vec<i64> = Vec::new();

    for i in 0..series.len() {
        let (ts, _, a, b, sol) = series[i];
        let lo = (i + 1).saturating_sub(params.lookback_obs);
        let window: Vec<f64> = series[lo..=i].iter().map(|&(_, s, ..)| s).collect();
        let Some(z) = zscore_last(&window) else { continue };

        if let Some(pos) = position.take() {
            // Held-leg current price.
            let px = if pos.mint == mint_a { a } else { b };
            if z.abs() <= params.z_exit || z.abs() >= params.z_stop {
                let proceeds = pos.token_amount * exit_fill_price(px, params.slippage_bps);
                let usdc_out = (proceeds - est_gas_usdc(sol)).max(0.0);
                let rec = build_trade_record(&pos, ts as i64, px, usdc_out, "sim-relval".into());
                realized += rec.usdc_out - rec.usdc_in;
                last_exit_ts = ts as i64;
                equity_curve.push((ts, realized));
                trades.push(rec);
            } else {
                position = Some(pos);
            }
            continue;
        }

        // FLAT — long the cheap leg of a stretched-but-not-broken spread.
        if z.abs() < params.z_entry || z.abs() >= params.z_stop {
            continue;
        }
        let cutoff = ts as i64 - 86_400;
        if entry_tss.iter().filter(|&&e| e >= cutoff).count() >= params.max_trades_per_day as usize {
            continue;
        }
        if (ts as i64) - last_exit_ts < params.reentry_cooldown_secs {
            continue;
        }
        let gas_bps = est_gas_bps(params.trade_usdc, sol);
        if params.slippage_bps + gas_bps > params.max_cost_bps {
            continue;
        }
        // z < 0 ⇒ A cheap ⇒ long A; z > 0 ⇒ B cheap ⇒ long B.
        let (mint, sym, px) = if z < 0.0 { (mint_a, sym_a, a) } else { (mint_b, sym_b, b) };
        position = Some(Position {
            mint: mint.to_string(),
            symbol: sym.to_string(),
            entry_ts: ts as i64,
            entry_price_usd: px,
            token_amount: params.trade_usdc / entry_fill_price(px, params.slippage_bps),
            usdc_spent: params.trade_usdc + est_gas_usdc(sol),
            peak_price_usd: px,
            entry_sig: "sim-relval".into(),
            dry_run: true,
        });
        entry_tss.push(ts as i64);
    }

    SimRun { trades, equity_curve }
}

/// Walk-forward grid over every pair × z-knob combo for long-only relative value.
#[allow(clippy::too_many_arguments)]
pub fn run_grid_relval(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    pairs: &[(WatchedToken, WatchedToken)],
    base: &RelValParams,
    lookbacks: &[usize],
    z_entries: &[f64],
    z_exits: &[f64],
    z_stops: &[f64],
) -> Vec<RelValResult> {
    let mut results = Vec::new();
    for (a, b) in pairs {
        let train_series = relval_series(train, &a.mint, &b.mint);
        let test_series = relval_series(test, &a.mint, &b.mint);
        if train_series.len() < PAIRS_MIN_OBS || test_series.len() < PAIRS_MIN_OBS {
            continue;
        }
        for &lb in lookbacks {
            for &ze in z_entries {
                for &zx in z_exits {
                    for &zs in z_stops {
                        if zs <= ze {
                            continue;
                        }
                        let mut p = base.clone();
                        p.lookback_obs = lb;
                        p.z_entry = ze;
                        p.z_exit = zx;
                        p.z_stop = zs;
                        let tr = replay_relval(&train_series, &a.mint, &a.symbol, &b.mint, &b.symbol, &p);
                        let te = replay_relval(&test_series, &a.mint, &a.symbol, &b.mint, &b.symbol, &p);
                        results.push(RelValResult {
                            symbol_a: a.symbol.clone(),
                            symbol_b: b.symbol.clone(),
                            params: p,
                            net_pnl_train: tr.net_pnl(),
                            n_trades_train: tr.n_trades(),
                            net_pnl_test: te.net_pnl(),
                            n_trades_test: te.n_trades(),
                            win_rate_test: te.win_rate(),
                            max_dd_test: te.max_drawdown_pct(),
                        });
                    }
                }
            }
        }
    }
    results.sort_by(|x, y| {
        y.net_pnl_test
            .partial_cmp(&x.net_pnl_test)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

// ───────────── relative-strength market-neutral momentum ────────────────────
//
// Long the momentum LEADER (top-ranked watched token) and short SOL in equal
// dollars — a market-hedged momentum bet. Profit = the leader's return minus
// SOL's return; the market move cancels. Tests "does the momentum leader beat the
// market?" SOL is the hedge (trivially shortable on-chain). Reuses `PairRun`.

#[derive(Debug, Clone)]
pub struct RelStrengthParams {
    pub metric: RankMetric,
    pub min_metric: f64,
    pub lookback_obs: usize,
    /// Trailing stop on the *relative* (long−short) P&L: exit when it falls this
    /// many percent from its peak-since-entry.
    pub trail_pct: f64,
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    pub notional_usdc: f64,
    /// Per-leg cost (slippage + fee), bps; charged 4× per round-trip (2 legs × in/out).
    pub cost_bps: u32,
}

#[derive(Debug, Clone)]
pub struct RelStrengthResult {
    pub params: RelStrengthParams,
    pub net_pnl_train: f64,
    pub n_trades_train: usize,
    pub net_pnl_test: f64,
    pub n_trades_test: usize,
    pub win_rate_test: f64,
    pub max_dd_test: f64,
}

impl RelStrengthResult {
    pub fn is_robust(&self, min_trades: usize) -> bool {
        config_is_robust(
            self.net_pnl_train,
            self.net_pnl_test,
            self.n_trades_train,
            self.n_trades_test,
            min_trades,
        )
    }
}

/// Replay long-leader/short-SOL over one slice. Single position at a time. Enter
/// when the top momentum score clears `min_metric`; exit when that score fades to
/// ≤ `min_metric` OR the relative P&L trails `trail_pct` off its peak.
pub fn replay_relstrength(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    params: &RelStrengthParams,
) -> PairRun {
    let stream = ranked_stream(snapshots, watched, &relstrength_rank_param(params.metric, params.lookback_obs));
    let mut run = PairRun::default();
    if let Some(&first) = snapshots.first().map(|s| &s.ts) {
        run.equity_curve.push((first, 0.0));
    }
    let mut realized = 0.0_f64;
    // Open position: (held_mint, entry_token_px, entry_sol_px, peak_rel).
    let mut open: Option<(String, f64, f64, f64)> = None;
    let mut last_exit_ts: i64 = i64::MIN / 2;
    let mut entry_tss: Vec<i64> = Vec::new();
    let n = params.notional_usdc;
    let leg_cost = n * params.cost_bps as f64 / 10_000.0;

    for (i, snap) in snapshots.iter().enumerate() {
        let ts = snap.ts as i64;
        let sol = snap.prices.get(SOL_KEY).copied().filter(|p| *p > 0.0);

        if let Some((mint, e_tok, e_sol, peak)) = open.clone() {
            let tok = snap.prices.get(&mint).copied().filter(|p| *p > 0.0);
            let (Some(tok), Some(sol)) = (tok, sol) else { continue };
            // Dollar-neutral relative return: long token + short SOL.
            let rel = (tok / e_tok - 1.0) - (sol / e_sol - 1.0);
            let peak = peak.max(rel);
            let held_score = stream[i].iter().find(|c| c.mint == mint).map(|c| c.score);
            let faded = held_score.is_none_or(|s| s <= params.min_metric);
            let stopped = rel <= peak - params.trail_pct / 100.0;
            if faded || stopped {
                let net = n * rel - 4.0 * leg_cost;
                realized += net;
                run.pnls.push(net);
                run.equity_curve.push((snap.ts, realized));
                last_exit_ts = ts;
                open = None;
            } else {
                open = Some((mint, e_tok, e_sol, peak));
            }
            continue;
        }

        // FLAT — open long the momentum leader + short SOL.
        let Some(sol) = sol else { continue };
        let cutoff = ts - 86_400;
        if entry_tss.iter().filter(|&&e| e >= cutoff).count() >= params.max_trades_per_day as usize {
            continue;
        }
        if ts - last_exit_ts < params.reentry_cooldown_secs {
            continue;
        }
        if let Some(top) = stream[i].first() {
            if top.score > params.min_metric && top.price_usd > 0.0 {
                open = Some((top.mint.clone(), top.price_usd, sol, 0.0));
                entry_tss.push(ts);
            }
        }
    }
    run
}

/// Walk-forward grid for relative-strength momentum (metric × lookback × trail ×
/// min-threshold quantile).
#[allow(clippy::too_many_arguments)]
pub fn run_grid_relstrength(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    watched: &[WatchedToken],
    metrics: &[RankMetric],
    lookbacks: &[usize],
    trails: &[f64],
    quantile_probs: &[f64],
    base: &RelStrengthParams,
) -> Vec<RelStrengthResult> {
    let mut results = Vec::new();
    for &metric in metrics {
        for &lb in lookbacks {
            // Score distribution for this (metric, lookback) on the TRAIN slice, to
            // derive per-metric entry thresholds (same approach as the momentum grid).
            let rank_p = relstrength_rank_param(metric, lb);
            let train_stream = ranked_stream(train, watched, &rank_p);
            let scores: Vec<f64> = train_stream
                .iter()
                .filter_map(|c| c.first().map(|t| t.score))
                .collect();
            let mins = min_metric_candidates(&scores, quantile_probs);
            for &trail in trails {
                for &min_metric in &mins {
                    let mut p = base.clone();
                    p.metric = metric;
                    p.lookback_obs = lb;
                    p.trail_pct = trail;
                    p.min_metric = min_metric;
                    let tr = replay_relstrength(train, watched, &p);
                    let te = replay_relstrength(test, watched, &p);
                    results.push(RelStrengthResult {
                        params: p,
                        net_pnl_train: tr.net_pnl(),
                        n_trades_train: tr.n_trades(),
                        net_pnl_test: te.net_pnl(),
                        n_trades_test: te.n_trades(),
                        win_rate_test: te.win_rate(),
                        max_dd_test: te.max_drawdown_pct(),
                    });
                }
            }
        }
    }
    results.sort_by(|x, y| {
        y.net_pnl_test
            .partial_cmp(&x.net_pnl_test)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    results
}

/// Neutral momentum-ranking ParamSet for relative-strength (all gates off; we only
/// want each token's raw metric score so we can pick the leader).
fn relstrength_rank_param(metric: RankMetric, lookback_obs: usize) -> ParamSet {
    ParamSet {
        metric,
        min_metric: 0.0,
        trail_pct: 0.0,
        lookback_obs,
        max_run_pct: 0.0,
        rotate_margin: 0.0,
        regime_filter_obs: 0,
        decel_lookback_min: 0,
        confirm_lag_obs: 0,
        stale_minutes: 0,
        reentry_cooldown_secs: 0,
        max_trades_per_day: 0,
        trade_usdc: 0.0,
        slippage_bps: 0,
        max_cost_bps: 0,
        exit_on_fade: false,
        vol_stop_mode: VolStopMode::Off,
        chandelier_k: 0.0,
        vol_obs: 0,
        overbought_z: 0.0,
        entry_dip_obs: 0,
        entry_dip_z: 0.0,
        dip_confirm_obs: 0,
        optimistic_fill: false,
        max_hold_min: 0,
        breakeven_exit: false,
        max_trail_pct: 0.0,
        reinvest_frac: 0.0,
        size_ceiling_usdc: 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// One snapshot carrying a crypto token "AAA" and a constant SOL price.
    fn snap(ts: u64, aaa: f64, sol: f64) -> PriceSnapshot {
        let mut prices = HashMap::new();
        prices.insert("AAA".to_string(), aaa);
        prices.insert(SOL_KEY.to_string(), sol);
        PriceSnapshot { ts, prices }
    }

    fn aaa() -> Vec<WatchedToken> {
        vec![WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None }]
    }

    /// A param set with every shape-guard disabled, so entry fires as soon as a
    /// token is rankable and exit is purely the trailing stop.
    fn bare_params() -> ParamSet {
        ParamSet {
            metric: RankMetric::Return,
            min_metric: 0.0,
            trail_pct: 8.0,
            lookback_obs: 121,
            max_run_pct: 0.0,        // over-extension off
            rotate_margin: 0.0,      // rotation off by default
            regime_filter_obs: 0,    // market-regime filter off by default
            decel_lookback_min: 0,   // recent-slope off → `falling` off
            confirm_lag_obs: 0,      // metric-fading off
            stale_minutes: 0,        // staleness off
            reentry_cooldown_secs: 0,
            max_trades_per_day: 100,
            trade_usdc: 100.0,
            slippage_bps: 50,
            max_cost_bps: 1000,
            exit_on_fade: false,
            vol_stop_mode: VolStopMode::Off,
            chandelier_k: 0.0,
            vol_obs: 0,
            overbought_z: 0.0,
            entry_dip_obs: 0,
            entry_dip_z: 0.0,
            dip_confirm_obs: 0,
            optimistic_fill: false,
            max_hold_min: 0,
            breakeven_exit: false,
            max_trail_pct: 0.0,
            reinvest_frac: 0.0,
            size_ceiling_usdc: 100.0,
        }
    }

    #[test]
    fn stop_variants_is_additive_not_multiplicative() {
        let trails = [4.0, 6.0, 8.0, 10.0, 12.0];
        let atr_ks = [2.0, 3.0, 4.0];
        let sigma_ks = [3.0, 5.0, 8.0];
        let vol_obs = [60usize, 120];
        let max_trails = [15.0, 25.0, 40.0];
        let v = stop_variants(&trails, &atr_ks, &sigma_ks, &vol_obs, &max_trails);
        // 5 fixed (Off) + (3 ATR + 3 σ) × 2 windows + 3 max-trail = 5 + 12 + 3 = 20. Additive.
        assert_eq!(
            v.len(),
            trails.len() + (atr_ks.len() + sigma_ks.len()) * vol_obs.len() + max_trails.len()
        );
        // Off variants WITHOUT a give-back cap carry the swept trail widths.
        assert_eq!(
            v.iter().filter(|s| s.mode == VolStopMode::Off && s.max_trail_pct == 0.0).count(),
            trails.len()
        );
        // Vol-stop variants share one fallback trail and carry no give-back cap.
        let fallback = trails[trails.len() / 2];
        for s in v.iter().filter(|s| s.mode != VolStopMode::Off) {
            assert_eq!(s.trail_pct, fallback);
            assert!(s.k > 0.0 && s.vol_obs > 0 && s.max_trail_pct == 0.0);
        }
        // Max-trail variants: Off not-green stop at the fallback trail, give-back cap set.
        let mt: Vec<f64> = v.iter().filter(|s| s.max_trail_pct > 0.0).map(|s| s.max_trail_pct).collect();
        assert_eq!(mt, max_trails);
        // Nothing requested ⇒ only the fixed sweep.
        assert_eq!(stop_variants(&trails, &[], &[], &vol_obs, &[]).len(), trails.len());
    }

    #[test]
    fn sizing_variants_off_by_default_and_expands_on_request() {
        let base = 100.0;
        // Empty / zero-only ⇒ single fixed baseline (sizing off).
        assert_eq!(sizing_variants(base, &[], &[2.0, 3.0]), vec![(0.0, base)]);
        assert_eq!(sizing_variants(base, &[0.0], &[2.0, 3.0]), vec![(0.0, base)]);
        // Positive fractions expand to one pair per ceiling multiple; `0` stays a single
        // baseline. {0, 0.5, 1.0} × {2,3} ⇒ 1 + 2 + 2 = 5 configs.
        let v = sizing_variants(base, &[0.0, 0.5, 1.0], &[2.0, 3.0]);
        assert_eq!(v.len(), 5);
        assert_eq!(v.iter().filter(|(f, _)| *f == 0.0).count(), 1);
        assert!(v.contains(&(0.5, 200.0)) && v.contains(&(1.0, 300.0)));
        // Ceiling floored at base (a <1× multiple can't shrink the cap below base).
        assert_eq!(sizing_variants(base, &[1.0], &[0.5]), vec![(1.0, base)]);
    }

    #[test]
    fn max_hold_forces_exit_on_a_monotonic_rise() {
        // A monotone rise never trips the trailing stop (px always == peak), so the
        // position would ride to the end — unless the hard time stop fires.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..200u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        assert_eq!(
            replay(&snaps, &aaa(), &bare_params()).n_trades(),
            0,
            "monotone rise never trips the trailing stop → no exit"
        );
        let mut params = bare_params();
        params.max_hold_min = 60; // 3600s ≈ 20 obs after entry
        // The time stop forces exits; the still-rising token is re-entered after each, so
        // several round-trips occur over the series (vs zero without the guard).
        assert!(
            replay(&snaps, &aaa(), &params).n_trades() >= 1,
            "the time stop forces at least one exit"
        );
    }

    #[test]
    fn reinvest_frac_grows_size_as_profit_banks() {
        // A monotone rise + a hard time stop forces repeated *profitable* round-trips.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..200u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        let mut fixed = bare_params();
        fixed.max_hold_min = 60; // ~20-obs holds → several round-trips on the rise

        let fr = replay(&snaps, &aaa(), &fixed);
        assert!(fr.trades.len() >= 2, "the rise should yield multiple round-trips");
        // reinvest=0 ⇒ every entry deploys the same notional.
        let base_in = fr.trades[0].usdc_in;
        assert!(
            fr.trades.iter().all(|t| (t.usdc_in - base_in).abs() < 1e-6),
            "fixed sizing keeps usdc_in constant"
        );

        let mut dynamic = fixed.clone();
        dynamic.reinvest_frac = 1.0;
        dynamic.size_ceiling_usdc = 1_000_000.0; // effectively uncapped
        let dr = replay(&snaps, &aaa(), &dynamic);
        // Sizing changes neither entry timing nor count (it's price/time-driven).
        assert_eq!(dr.trades.len(), fr.trades.len());
        // First entry starts at base (realized = 0); a later entry deploys strictly more.
        assert!((dr.trades[0].usdc_in - base_in).abs() < 1e-6, "starts small at base");
        assert!(
            dr.trades.last().unwrap().usdc_in > dr.trades[0].usdc_in,
            "size compounds upward as banked profit accumulates"
        );
    }

    #[test]
    fn breakeven_exits_a_green_position_back_at_entry() {
        // Enter on the warm-up rise, spike green, then fall back below entry. With a wide
        // trailing stop (won't trip), only the breakeven guard can close the position.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        snaps.push(snap(1000 + 131 * 180, 2.00, sol)); // green peak (well above entry ~1.8)
        snaps.push(snap(1000 + 132 * 180, 1.50, sol)); // back below entry → breakeven trips
        snaps.push(snap(1000 + 133 * 180, 1.49, sol)); // conservative next-snapshot fill
        let mut params = bare_params();
        params.trail_pct = 50.0; // disable the trailing stop so only breakeven can exit
        assert_eq!(
            replay(&snaps, &aaa(), &params).n_trades(),
            0,
            "wide trail + no breakeven → position rides through the dip"
        );
        params.breakeven_exit = true;
        assert_eq!(
            replay(&snaps, &aaa(), &params).n_trades(),
            1,
            "breakeven closes the green round-trip"
        );
    }

    #[test]
    fn max_trail_rides_a_pullback_the_tight_trail_would_exit() {
        // Warm-up rise to entry (~1.8), peak 3.0, then a partial dip to 2.5. The tight
        // 8% trail would bail at 2.5 (stop ≈ 2.76); a 30% max-trail (give-back floor ≈
        // peak·0.70 = 2.10) lets the green position ride straight past it.
        let sol = 150.0;
        let mut partial = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            partial.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        partial.push(snap(1000 + 131 * 180, 3.00, sol)); // green peak
        partial.push(snap(1000 + 132 * 180, 2.50, sol)); // partial dip
        partial.push(snap(1000 + 133 * 180, 2.49, sol)); // next-snapshot fill mark

        let mut tight = bare_params(); // trail_pct = 8.0 by default in bare_params? set explicitly
        tight.trail_pct = 8.0;
        tight.max_trail_pct = 0.0;
        assert_eq!(
            replay(&partial, &aaa(), &tight).n_trades(),
            1,
            "tight 8% trail exits on the dip to 2.5"
        );

        let mut wide = bare_params();
        wide.trail_pct = 8.0; // same tight trail — but max-trail overrides once green
        wide.max_trail_pct = 30.0;
        assert_eq!(
            replay(&partial, &aaa(), &wide).n_trades(),
            0,
            "30% max-trail rides the pullback (2.5 is above the 2.10 give-back floor)"
        );

        // But a deeper drop, through the give-back floor, DOES cut the position.
        let mut deep = partial.clone();
        deep[132] = snap(1000 + 132 * 180, 2.00, sol); // 2.00 < give-back 2.10 → exit
        deep[133] = snap(1000 + 133 * 180, 1.99, sol);
        assert_eq!(
            replay(&deep, &aaa(), &wide).n_trades(),
            1,
            "max-trail still exits once the give-back floor is breached"
        );
    }

    #[test]
    fn replay_enters_then_trailing_stops_with_next_snapshot_fill() {
        // 0..=130: monotone rise (×1.005/step) — entry becomes possible at the
        // 121st price (index 120). 131: peak 2.0. 132: drop to 1.80 (−10% from
        // peak → stop trips). 133: the conservative next-snapshot fill mark.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        snaps.push(snap(1000 + 131 * 180, 2.00, sol)); // peak
        snaps.push(snap(1000 + 132 * 180, 1.80, sol)); // stop detected here
        snaps.push(snap(1000 + 133 * 180, 1.78, sol)); // fills here (next snapshot)

        let params = bare_params();
        let run = replay(&snaps, &aaa(), &params);

        assert_eq!(run.n_trades(), 1, "exactly one round-trip");
        let t = &run.trades[0];

        let entry_mark = 1.005_f64.powi(120);
        assert!((t.entry_price_usd - entry_mark).abs() < 1e-9, "entry at p[120]; got {}", t.entry_price_usd);
        assert_eq!(t.entry_ts, (1000 + 120 * 180) as i64, "enters at the first rankable snapshot");

        // Conservative fill: detected at 132, executed at 133's mark.
        assert!((t.exit_price_usd - 1.78).abs() < 1e-9, "exit mark is the next snapshot; got {}", t.exit_price_usd);
        assert_eq!(t.exit_ts, (1000 + 133 * 180) as i64);

        let usdc_in = 100.0 + est_gas_usdc(sol);
        let token_amount = 100.0 / entry_fill_price(entry_mark, 50);
        let usdc_out = (token_amount * exit_fill_price(1.78, 50) - est_gas_usdc(sol)).max(0.0);
        assert!((t.usdc_in - usdc_in).abs() < 1e-6, "basis nets entry gas; got {}", t.usdc_in);
        assert!((t.usdc_out - usdc_out).abs() < 1e-6, "proceeds net slippage + exit gas; got {}", t.usdc_out);

        // Equity curve ends at cumulative realized P&L.
        assert!((run.equity_curve.last().unwrap().1 - run.net_pnl()).abs() < 1e-9);
    }

    #[test]
    fn replay_rotates_into_a_stronger_token() {
        // AAA rises steadily (never stops). BBB starts weaker, then accelerates and
        // overtakes AAA's score by the margin. With rotation OFF, AAA never closes
        // (it only rises) → 0 trades. With rotation ON, B overtaking forces the A-leg
        // to close → ≥1 trade, the first being AAA. That gap isolates rotation.
        let sol = 150.0;
        let snap2 = |ts: u64, a: f64, b: f64| {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), a);
            m.insert("BBB".to_string(), b);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None },
            WatchedToken { symbol: "BBB".into(), mint: "BBB".into(), name: None, equity: None },
        ];
        let mut snaps = Vec::new();
        let (mut a, mut b) = (1.0_f64, 1.0_f64);
        for i in 0..128u64 {
            snaps.push(snap2(1000 + i * 180, a, b));
            a *= 1.004; // steady, higher early score → AAA entered first
            b *= 1.001; // weaker early
        }
        // BBB accelerates hard so its windowed Return overtakes AAA past the margin.
        for i in 128..220u64 {
            snaps.push(snap2(1000 + i * 180, a, b));
            a *= 1.004; // AAA keeps rising → never trailing-stops
            b *= 1.03; // BBB rockets
        }

        let mut off = bare_params();
        off.metric = RankMetric::Return;
        off.min_metric = 0.0;
        off.trail_pct = 8.0;
        off.rotate_margin = 0.0;
        assert_eq!(replay(&snaps, &watched, &off).n_trades(), 0, "no rotation, rising A never closes");

        let mut on = off.clone();
        on.rotate_margin = 0.10; // Return-units margin BBB clears once it rockets
        let run = replay(&snaps, &watched, &on);
        assert!(run.n_trades() >= 1, "rotation should force the A-leg to close");
        assert_eq!(run.trades[0].mint, "AAA", "first close is the rotated-out A leg");
    }

    #[test]
    fn regime_filter_blocks_entries_while_market_is_risk_off() {
        // AAA rises→peak→drops (1 trade with the filter off). SOL declines the whole
        // time → SOL below its MA → risk-off → entries blocked → 0 trades.
        let mut snaps = Vec::new();
        let mut a = 1.0_f64;
        let mut sol = 300.0_f64;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, a, sol));
            a *= 1.005;
            sol *= 0.999; // SOL drifting down → risk-off regime
        }
        snaps.push(snap(1000 + 131 * 180, 2.00, sol));
        snaps.push(snap(1000 + 132 * 180, 1.80, sol));
        snaps.push(snap(1000 + 133 * 180, 1.78, sol));

        let mut off = bare_params();
        off.metric = RankMetric::Return;
        off.min_metric = 0.0;
        assert_eq!(replay(&snaps, &aaa(), &off).n_trades(), 1, "no filter → enters");

        let mut on = off.clone();
        on.regime_filter_obs = 50;
        assert_eq!(replay(&snaps, &aaa(), &on).n_trades(), 0, "risk-off SOL blocks the entry");
    }

    #[test]
    fn token_rising_confirms_uptick() {
        let up: Vec<PriceSnapshot> = (0..10u64).map(|i| snap(i, 100.0 + i as f64, 150.0)).collect();
        assert!(token_rising(&up, 9, "AAA", 3), "rising → bounce confirmed");
        let down: Vec<PriceSnapshot> = (0..10u64).map(|i| snap(i, 100.0 - i as f64, 150.0)).collect();
        assert!(!token_rising(&down, 9, "AAA", 3), "falling → not confirmed");
        assert!(token_rising(&up, 9, "AAA", 0), "obs=0 → always true (no confirmation)");
    }

    #[test]
    fn token_atr_is_mean_abs_step() {
        let snaps: Vec<PriceSnapshot> =
            [100.0, 101.0, 103.0, 102.0].iter().enumerate().map(|(i, &p)| snap(i as u64, p, 150.0)).collect();
        // |Δ| = 1, 2, 1 → mean 4/3.
        let atr = token_atr(&snaps, 3, "AAA", 10).unwrap();
        assert!((atr - 4.0 / 3.0).abs() < 1e-9, "got {atr}");
    }

    #[test]
    fn chandelier_stop_exits_on_vol_scaled_drop() {
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        snaps.push(snap(1000 + 131 * 180, 2.0, sol));
        snaps.push(snap(1000 + 132 * 180, 1.8, sol));
        snaps.push(snap(1000 + 133 * 180, 1.78, sol));
        let mut pr = bare_params();
        pr.metric = RankMetric::Return;
        pr.min_metric = 0.0;
        pr.chandelier_k = 3.0;
        pr.vol_obs = 60;
        assert_eq!(replay(&snaps, &aaa(), &pr).n_trades(), 1, "chandelier stop exits the drop");
    }

    #[test]
    fn overbought_takeprofit_exits_into_a_spike() {
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..200u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.01; // steady strong rise → z stretches overbought
        }
        let mut pr = bare_params();
        pr.metric = RankMetric::Return;
        pr.min_metric = 0.0;
        pr.trail_pct = 99.0; // disable the fixed stop so only overbought can exit
        pr.overbought_z = 1.5;
        pr.vol_obs = 60;
        let run = replay(&snaps, &aaa(), &pr);
        assert!(run.n_trades() >= 1, "overbought take-profit exits the green position");
        assert!(run.trades[0].usdc_out > run.trades[0].usdc_in, "sold into strength → green");
    }

    #[test]
    fn momentum_dip_gate_blocks_entry_at_highs() {
        // Rising series → momentum fires, but the token is at its highs (not oversold),
        // so the mean-reversion entry confirmation ("both true") must block it.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        snaps.push(snap(1000 + 131 * 180, 2.0, sol));
        snaps.push(snap(1000 + 132 * 180, 1.8, sol));
        snaps.push(snap(1000 + 133 * 180, 1.78, sol));
        let mut off = bare_params();
        off.metric = RankMetric::Return;
        off.min_metric = 0.0;
        let mut on = off.clone();
        on.entry_dip_obs = 60;
        on.entry_dip_z = 1.0;
        assert_eq!(replay(&snaps, &aaa(), &off).n_trades(), 1, "pure momentum enters at the high");
        assert_eq!(replay(&snaps, &aaa(), &on).n_trades(), 0, "dip gate blocks buying the high");
    }

    #[test]
    fn replay_optimistic_fill_exits_same_bar_at_tripping_price() {
        // Same series as the conservative test; optimistic mode must fill at the
        // snapshot that tripped the stop (index 132, price 1.80), not the next.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.005;
        }
        snaps.push(snap(1000 + 131 * 180, 2.00, sol));
        snaps.push(snap(1000 + 132 * 180, 1.80, sol)); // trips here AND fills here
        snaps.push(snap(1000 + 133 * 180, 1.78, sol));

        let mut params = bare_params();
        params.optimistic_fill = true;
        let run = replay(&snaps, &aaa(), &params);

        assert_eq!(run.n_trades(), 1);
        let t = &run.trades[0];
        assert!((t.exit_price_usd - 1.80).abs() < 1e-9, "same-bar fill at the trip price; got {}", t.exit_price_usd);
        assert_eq!(t.exit_ts, (1000 + 132 * 180) as i64);
    }

    #[test]
    fn replay_fade_exits_a_green_position_when_metric_decays() {
        // Steep rise (enter high-score) then a long flat plateau: the trailing stop
        // can never fire (price never drops), but the windowed Return metric decays
        // toward 0 as the flat region fills the lookback — so the only possible exit
        // is the fade-take-profit, and it fires while green.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut p = 1.0;
        for i in 0..131u64 {
            snaps.push(snap(1000 + i * 180, p, sol));
            p *= 1.01; // steep
        }
        let plateau = p; // hold flat well past the lookback so Return decays to ~0
        for i in 131..360u64 {
            snaps.push(snap(1000 + i * 180, plateau, sol));
        }

        let mut params = bare_params();
        params.trail_pct = 50.0; // stop can't fire on flat data anyway; make doubly sure
        params.min_metric = 0.05; // entry score (~1.2) clears it; decayed Return won't
        params.exit_on_fade = true;
        let run = replay(&snaps, &aaa(), &params);

        assert_eq!(run.n_trades(), 1, "one fade-driven round-trip");
        let t = &run.trades[0];
        assert!((t.exit_price_usd - plateau).abs() < 1e-9, "fade fills at the flat mark");
        assert!(t.exit_ts < snaps.last().unwrap().ts as i64, "fade fires mid-plateau, not at the end");
        assert!(t.usdc_out > t.usdc_in, "sold the plateau above entry → green");
    }

    fn mr_params(lookback: usize, z_entry: f64, z_exit: f64, z_stop: f64) -> MeanRevParams {
        MeanRevParams {
            lookback_obs: lookback,
            z_entry,
            z_exit,
            z_stop,
            trend_filter_obs: 0,
            reentry_cooldown_secs: 0,
            max_trades_per_day: 100,
            trade_usdc: 100.0,
            slippage_bps: 50,
            max_cost_bps: 1000,
        }
    }

    #[test]
    fn meanrev_trend_filter_blocks_dip_below_trend() {
        // Flat baseline at 100 → dip to 90 → recovery (the proven round-trip scenario).
        // The dip sits BELOW the trailing trend (≈100), so the trend filter must block
        // the entry that the no-filter run takes.
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut t = 1000u64;
        let mut push = |s: &mut Vec<PriceSnapshot>, p: f64| { s.push(snap(t, p, sol)); t += 180; };
        for _ in 0..40 { push(&mut snaps, 100.0); }
        for &p in &[100.0, 98.0, 95.0, 92.0, 90.0] { push(&mut snaps, p); }
        for &p in &[92.0, 95.0, 98.0, 100.0, 102.0, 103.0] { push(&mut snaps, p); }

        let mut off = mr_params(45, 1.5, 0.0, 50.0);
        off.trend_filter_obs = 0;
        let mut on = mr_params(45, 1.5, 0.0, 50.0);
        on.trend_filter_obs = 30;
        assert!(replay_meanrev_full(&snaps, &aaa(), &off).n_trades() >= 1, "no filter buys the dip");
        assert_eq!(
            replay_meanrev_full(&snaps, &aaa(), &on).n_trades(),
            0,
            "trend filter must block a dip that's below the trend"
        );
    }

    #[test]
    fn meanrev_buys_dip_and_sells_the_reversion_for_a_profit() {
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut t = 1000u64;
        let mut push = |snaps: &mut Vec<PriceSnapshot>, p: f64| {
            snaps.push(snap(t, p, sol));
            t += 180;
        };
        for _ in 0..40 {
            push(&mut snaps, 100.0); // flat warmup → tight mean/σ
        }
        for &p in &[100.0, 98.0, 95.0, 92.0, 90.0] {
            push(&mut snaps, p); // dip → z goes deeply negative → buy
        }
        for &p in &[92.0, 95.0, 98.0, 100.0, 102.0, 103.0] {
            push(&mut snaps, p); // recovery → z ≥ 0 → take profit
        }
        // Loose stop so the shallow dip can't trip it — only the reversion exit fires.
        let params = mr_params(45, 1.5, 0.0, 50.0);
        let run = replay_meanrev_full(&snaps, &aaa(), &params);
        assert_eq!(run.n_trades(), 1, "one reversion round-trip");
        assert!(
            run.trades[0].usdc_out > run.trades[0].usdc_in,
            "bought the dip, sold the reversion → profit; got in {} out {}",
            run.trades[0].usdc_in, run.trades[0].usdc_out
        );
    }

    #[test]
    fn meanrev_stops_out_when_the_dip_keeps_falling() {
        let sol = 150.0;
        let mut snaps = Vec::new();
        let mut t = 1000u64;
        let mut push = |snaps: &mut Vec<PriceSnapshot>, p: f64| {
            snaps.push(snap(t, p, sol));
            t += 180;
        };
        for _ in 0..40 {
            push(&mut snaps, 100.0);
        }
        // Sharp continued crash while the window is still mostly 100 → z very negative.
        for &p in &[97.0, 90.0, 80.0, 70.0, 65.0] {
            push(&mut snaps, p);
        }
        let mut params = mr_params(45, 1.5, 5.0, 3.0); // z_exit unreachable; z_stop=3 trips
        params.reentry_cooldown_secs = 10_000_000; // no re-entry into the falling knife
        let run = replay_meanrev_full(&snaps, &aaa(), &params);
        assert_eq!(run.n_trades(), 1, "one stopped-out trade");
        assert!(
            run.trades[0].usdc_out < run.trades[0].usdc_in,
            "stop fired on a falling knife → loss; got in {} out {}",
            run.trades[0].usdc_in, run.trades[0].usdc_out
        );
    }

    fn pair_params(lookback: usize, z_entry: f64, z_exit: f64, z_stop: f64) -> PairParams {
        PairParams {
            lookback_obs: lookback,
            z_entry,
            z_exit,
            z_stop,
            reentry_cooldown_secs: 0,
            max_trades_per_day: 100,
            notional_usdc: 100.0,
            cost_bps: 10,
            funding_bps_per_day: 0.0,
            entry_confirm_obs: 0,
        }
    }

    /// Build a `(ts, ln(a/b), a, b)` series from an `a` path against constant `b`.
    fn pseries(a_path: &[f64], b: f64) -> Vec<(u64, f64, f64, f64)> {
        a_path
            .iter()
            .enumerate()
            .map(|(i, &a)| (1000 + i as u64 * 180, (a / b).ln(), a, b))
            .collect()
    }

    /// 40-point noisy baseline (±1% around 100) so σ is well-defined, not degenerate.
    fn noisy_baseline() -> Vec<f64> {
        (0..40).map(|i| if i % 2 == 0 { 99.0 } else { 101.0 }).collect()
    }

    #[test]
    fn pairs_profits_when_a_stretched_spread_converges() {
        // b flat; a dislocates up (short A) then snaps back → profit.
        let mut a = noisy_baseline();
        a.push(110.0); // dislocation → z high → short A / long B
        a.extend_from_slice(&[100.0, 100.0, 100.0]); // revert → exit in profit
        let series = pseries(&a, 100.0);
        let run = replay_pairs(&series, &pair_params(45, 2.0, 0.5, 50.0));
        assert!(run.n_trades() >= 1, "should open a spread trade");
        assert!(run.net_pnl() > 0.0, "converging spread → profit; got {}", run.net_pnl());
    }

    #[test]
    fn pairs_stops_out_when_spread_keeps_diverging() {
        // Gradual widening so entry-z clears z_entry but is below z_stop, then blows past.
        let mut a = noisy_baseline();
        a.extend_from_slice(&[103.0, 108.0, 114.0, 121.0, 128.0]); // keeps widening → stop
        let series = pseries(&a, 100.0);
        let mut p = pair_params(45, 2.0, 0.5, 4.0);
        p.reentry_cooldown_secs = 10_000_000; // one trade only
        let run = replay_pairs(&series, &p);
        assert_eq!(run.n_trades(), 1, "one stopped-out trade");
        assert!(run.net_pnl() < 0.0, "diverging spread → loss; got {}", run.net_pnl());
    }

    #[test]
    fn reversal_confirm_skips_a_still_diverging_entry() {
        // Same monotonic divergence as the stop-out test: |z| only ever grows, never pulls
        // back — exactly the "knife" the live trader kept catching.
        let mut a = noisy_baseline();
        a.extend_from_slice(&[103.0, 108.0, 114.0, 121.0, 128.0]);
        let series = pseries(&a, 100.0);
        // Without confirm: enters at the first |z| ≥ 2 (and stops out).
        let base = pair_params(45, 2.0, 0.5, 4.0);
        assert!(
            replay_pairs(&series, &base).n_trades() >= 1,
            "no confirm → enters the diverging spread"
        );
        // With reversal confirmation: |z| never shrinks before the stop, so entry is never
        // confirmed → the knife-catch is skipped entirely.
        let mut p = base.clone();
        p.entry_confirm_obs = 3;
        assert_eq!(
            replay_pairs(&series, &p).n_trades(),
            0,
            "reversal-confirm skips the still-diverging entry"
        );
    }

    #[test]
    fn relval_longs_the_cheap_leg_and_profits_on_reversion() {
        // b flat; a dips below b (z negative ⇒ A cheap ⇒ long A) then recovers → profit.
        let mut a = noisy_baseline();
        a.push(85.0); // sharp dislocation down → A cheap → long A
        a.extend_from_slice(&[100.0, 100.0, 100.0]); // snaps back → exit well above entry
        let series: Vec<(u64, f64, f64, f64, f64)> = a
            .iter()
            .enumerate()
            .map(|(i, &av)| (1000 + i as u64 * 180, (av / 100.0).ln(), av, 100.0, 150.0))
            .collect();
        let params = RelValParams {
            lookback_obs: 45,
            z_entry: 1.5,
            z_exit: 0.5,
            z_stop: 50.0,
            reentry_cooldown_secs: 0,
            max_trades_per_day: 100,
            trade_usdc: 100.0,
            slippage_bps: 50,
            max_cost_bps: 1000,
        };
        let run = replay_relval(&series, "AAA", "AAA", "BBB", "BBB", &params);
        assert!(run.n_trades() >= 1, "should long the cheap leg");
        assert_eq!(run.trades[0].mint, "AAA", "longs A (the cheap leg)");
        assert!(run.trades[0].usdc_out > run.trades[0].usdc_in, "reversion → profit");
    }

    fn rs_snap(ts: u64, aaa: f64, sol: f64) -> PriceSnapshot {
        let mut p = HashMap::new();
        p.insert("AAA".to_string(), aaa);
        p.insert(SOL_KEY.to_string(), sol);
        PriceSnapshot { ts, prices: p }
    }
    fn rs_watched() -> Vec<WatchedToken> {
        vec![WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None }]
    }
    fn rs_params() -> RelStrengthParams {
        RelStrengthParams {
            metric: RankMetric::Return,
            min_metric: 0.05,
            lookback_obs: 121,
            trail_pct: 6.0,
            reentry_cooldown_secs: 0,
            max_trades_per_day: 100,
            notional_usdc: 1000.0,
            cost_bps: 10,
        }
    }

    #[test]
    fn relstrength_profits_when_leader_beats_flat_sol() {
        let mut snaps = Vec::new();
        for i in 0..=120u64 {
            snaps.push(rs_snap(i, 1.0 + 0.001 * i as f64, 150.0)); // gentle rise → enter at i=120
        }
        // sharp rise (peak ~16% rel) then a dip that trails >6% off peak but stays above entry.
        for (k, &a) in [1.18, 1.24, 1.30, 1.28, 1.26, 1.24, 1.22].iter().enumerate() {
            snaps.push(rs_snap(121 + k as u64, a, 150.0)); // SOL flat → rel = AAA gain
        }
        let run = replay_relstrength(&snaps, &rs_watched(), &rs_params());
        assert!(run.n_trades() >= 1, "should open a long-leader/short-SOL position");
        assert!(run.net_pnl() > 0.0, "leader beat flat SOL → profit; got {}", run.net_pnl());
    }

    #[test]
    fn relstrength_loses_when_leader_reverts_below_entry() {
        let mut snaps = Vec::new();
        for i in 0..=120u64 {
            snaps.push(rs_snap(i, 1.0 + 0.001 * i as f64, 150.0));
        }
        snaps.push(rs_snap(121, 1.05, 150.0)); // crashes below entry (1.12) → stop/fade → loss
        snaps.push(rs_snap(122, 1.04, 150.0));
        let run = replay_relstrength(&snaps, &rs_watched(), &rs_params());
        assert_eq!(run.n_trades(), 1, "one stopped-out trade");
        assert!(run.net_pnl() < 0.0, "leader reverted below entry → loss; got {}", run.net_pnl());
    }

    #[test]
    fn min_metric_candidates_are_monotonic_within_range() {
        // Mixed scores incl. non-positive (momentum absent on some snapshots).
        let scores = vec![-0.2, 0.0, 0.1, 0.2, 0.3, 0.4, 0.5];
        let got = min_metric_candidates(&scores, &[0.50, 0.70, 0.85, 0.95]);
        assert_eq!(got.len(), 4, "one level per probability");
        // Non-decreasing.
        for w in got.windows(2) {
            assert!(w[1] >= w[0], "levels must be non-decreasing: {got:?}");
        }
        // Each level lies within the observed score range.
        let (lo, hi) = (-0.2, 0.5);
        for &g in &got {
            assert!(g >= lo && g <= hi, "{g} out of observed range [{lo},{hi}]");
        }
        // A high quantile must be more selective than a low one on a spread sample.
        assert!(got[3] > got[0], "p95 should exceed p50 on a non-degenerate sample");
    }

    #[test]
    fn min_metric_candidates_empty_scores_yields_empty() {
        assert!(min_metric_candidates(&[], &[0.5, 0.9]).is_empty());
    }

    #[test]
    fn sanitize_drops_isolated_spike_keeps_sustained_move() {
        let mk = |ts: u64, p: f64| {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), p);
            PriceSnapshot { ts, prices: m }
        };
        let snaps = vec![
            mk(0, 1.0),
            mk(1, 1.0),
            mk(2, 5000.0), // isolated glitch: 5000× off both neighbors → drop
            mk(3, 1.0),
            mk(4, 20.0), // 20× off prev but sustained next → real move, keep
            mk(5, 20.0),
        ];
        let clean = sim_sanitize(&snaps, 8.0);
        assert_eq!(clean.len(), snaps.len(), "snapshots preserved; only bad prices dropped");
        assert!(!clean[2].prices.contains_key("AAA"), "isolated spike removed");
        assert!((clean[3].prices["AAA"] - 1.0).abs() < 1e-9, "neighbor untouched");
        assert!((clean[4].prices["AAA"] - 20.0).abs() < 1e-9, "sustained 20× move kept");
    }

    #[test]
    fn is_robust_requires_both_slices_positive_and_min_trades() {
        let mk = |test: f64, train: f64, nte: usize, ntr: usize| SimResult {
            params: bare_params(),
            net_pnl_test: test,
            net_pnl_train: train,
            n_trades_test: nte,
            n_trades_train: ntr,
            win_rate_test: 0.0,
            max_dd_test: 0.0,
        };
        assert!(mk(5.0, 3.0, 4, 4).is_robust(3));
        assert!(!mk(5.0, -1.0, 4, 4).is_robust(3), "train must be positive");
        assert!(!mk(-1.0, 3.0, 4, 4).is_robust(3), "test must be positive");
        assert!(!mk(5.0, 3.0, 1, 4).is_robust(3), "needs ≥min trades in test");
        assert!(!mk(5.0, 3.0, 4, 2).is_robust(3), "needs ≥min trades in train");
    }

    #[test]
    fn sanitize_disabled_is_noop() {
        let mut m = HashMap::new();
        m.insert("AAA".to_string(), 5000.0);
        let snaps = vec![PriceSnapshot { ts: 0, prices: m }];
        assert_eq!(sim_sanitize(&snaps, 0.0).len(), 1);
        assert!(sim_sanitize(&snaps, 0.0)[0].prices.contains_key("AAA"));
    }

    // alias so the test reads clearly regardless of the public name
    use super::sanitize_history as sim_sanitize;

    #[test]
    fn entry_fill_pays_slippage_above_mark() {
        // 50 bps = 0.50% above the mark.
        let f = entry_fill_price(100.0, 50);
        assert!((f - 100.5).abs() < 1e-9, "got {f}");
    }

    #[test]
    fn exit_fill_pays_slippage_below_mark() {
        let f = exit_fill_price(100.0, 50);
        assert!((f - 99.5).abs() < 1e-9, "got {f}");
    }

    #[test]
    fn drawdown_zero_on_monotone_rising_equity() {
        let run = SimRun {
            trades: vec![],
            equity_curve: vec![(0, 0.0), (1, 5.0), (2, 12.0)],
        };
        assert!(run.max_drawdown_pct().abs() < 1e-9);
    }

    #[test]
    fn drawdown_measures_peak_to_trough_against_running_peak() {
        // Peak 20 at ts=2, trough 14 at ts=3 → (20-14)/20 = 30%.
        let run = SimRun {
            trades: vec![],
            equity_curve: vec![(0, 0.0), (1, 10.0), (2, 20.0), (3, 14.0), (4, 18.0)],
        };
        assert!((run.max_drawdown_pct() - 30.0).abs() < 1e-9, "got {}", run.max_drawdown_pct());
    }
}
