//! `momentum-sim` — offline parameter search for the momentum trader.
//!
//! Replays `assets/price_history.jsonl` through the production decision functions
//! (via `portfolio::sim`) over a grid of rank metric + high-leverage `MOMENTUM_*`
//! knobs, using a walk-forward split, and reports the combination with the best
//! held-out net P&L — ready to paste into `.env`.
//!
//! ```bash
//! cargo run --release --bin momentum-sim -- run                 # full grid
//! cargo run --release --bin momentum-sim -- run --quick         # smoke subset
//! cargo run --release --bin momentum-sim -- run --train-frac 0.7 --top 30
//! ```

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use solana_mev::portfolio::momentum::VolStopMode;
use solana_mev::portfolio::sim::{
    self, MeanRevParams, MeanRevResult, PairParams, PairResult, ParamSet, RelStrengthParams,
    RelStrengthResult, RelValParams, RelValResult, SimResult, GRID_ATR_KS, GRID_LOOKBACKS,
    GRID_MAX_RUNS, GRID_MAX_TRAILS, GRID_METRICS, GRID_MIN_QUANTILES, GRID_SIGMA_KS,
    GRID_SIZE_CEILING_MULTS, GRID_TRAILS, GRID_VOL_OBS, MR_LOOKBACKS, MR_Z_ENTRY, MR_Z_EXIT,
    MR_Z_STOP,
};
use solana_mev::portfolio::momentum_universe::WatchedToken;

#[derive(Clone, Copy, ValueEnum)]
enum StrategyArg {
    /// Rank-by-metric trend following (the live trader's strategy).
    Momentum,
    /// Buy oversold (z ≤ −entry), sell on reversion to the mean — the inverse.
    Meanrev,
    /// Market-neutral pairs: trade the spread ln(A/B) dollar-neutral (Phase-0 edge check).
    Pairs,
    /// Long-only relative value: buy the statistically-cheap leg of a pair (spot, executable).
    Relval,
    /// Relative-strength market-neutral momentum: long the leader, short SOL.
    Relstrength,
}
use solana_mev::portfolio::{history, momentum_universe, PortfolioConfig, RankMetric, RegimeMode};

#[derive(Parser)]
#[command(name = "momentum-sim", about = "Backtest + grid-search the momentum trader")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the walk-forward grid search and print the ranked results.
    Run {
        /// Fraction of history used for optimization; the rest is held out for scoring.
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        /// Trim the grid to a fast smoke-test subset.
        #[arg(long, default_value_t = false)]
        quick: bool,
        /// How many top rows to print.
        #[arg(long, default_value_t = 20)]
        top: usize,
        /// Override the watched-token list path (defaults to MOMENTUM_TOKENS_PATH).
        #[arg(long)]
        tokens: Option<String>,
        /// Override the price-history path (defaults to HISTORY_PATH).
        #[arg(long)]
        history: Option<String>,
        /// Where to write the full results CSV.
        #[arg(long, default_value = "assets/momentum_sim_results.csv")]
        csv: String,
        /// Spike filter: drop a price that jumps more than this factor off its
        /// neighbors then reverts (isolated glitch print). ≤1.0 disables it.
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        /// Optimistic fills: a tripped trailing stop fills same-bar at the trip
        /// price instead of the next snapshot. Brackets the upper bound of P&L.
        #[arg(long, default_value_t = false)]
        optimistic_fill: bool,
        /// Comma-separated lookback_obs values to sweep (overrides the default grid),
        /// e.g. --lookbacks 720,1440,2880. Each must exceed 120.
        #[arg(long, value_delimiter = ',')]
        lookbacks: Option<Vec<usize>>,
        /// Comma-separated trailing-stop widths (percent) to sweep (momentum only),
        /// overriding the default grid. e.g. --trails 12,16,20,30,50
        #[arg(long, value_delimiter = ',')]
        trails: Option<Vec<f64>>,
        /// Comma-separated rotation factors to sweep, as a multiple of the entry
        /// threshold (rotate when B beats A by factor×min_metric). 0 disables
        /// rotation. e.g. --rotate-factors 0,0.5,1.0
        #[arg(long, value_delimiter = ',', default_value = "0")]
        rotate_factors: Vec<f64>,
        /// A config must trade at least this many times in BOTH slices to count as
        /// "robust" — filters out lucky 1-trade flukes from the verdict.
        #[arg(long, default_value_t = 3)]
        min_trades: usize,
        /// Which strategy to backtest: momentum (default) or meanrev (the inverse).
        #[arg(long, value_enum, default_value_t = StrategyArg::Momentum)]
        strategy: StrategyArg,
        /// Comma-separated market-regime MA windows to sweep (momentum only): block
        /// entries unless SOL is above its MA over N obs. 0 = filter off.
        /// e.g. --regime-obs 0,240,720
        #[arg(long, value_delimiter = ',', default_value = "0")]
        regime_obs: Vec<usize>,
        /// Comma-separated TREND-regime windows (SOL slope_r2 clean-uptrend gate) added to
        /// the sweep; thresholds auto-derived per window from train quantiles (p0/p50/p70).
        /// e.g. --regime-trend-obs 240,480,720. Absent = no trend variants (grid unchanged).
        #[arg(long, value_delimiter = ',')]
        regime_trend_obs: Vec<usize>,
        /// Pairs strategy: per-leg trading cost (slippage + perp/swap fee), bps.
        #[arg(long, default_value_t = 15)]
        pair_cost_bps: u32,
        /// Pairs strategy: borrow/funding drag on the short leg, bps PER DAY held.
        /// Plug in the live Kamino xStock borrow APY ÷ 365 to test on-chain viability.
        #[arg(long, default_value_t = 0.0)]
        pair_funding_bps_day: f64,
        /// Pairs strategy: reversal-confirmation entry filter — only enter once |z| has
        /// shrunk vs N obs ago (spread turning back). Comma-separated to sweep; 0 = off.
        /// e.g. --pair-entry-confirm-obs 0,5,10,20
        #[arg(long, value_delimiter = ',', default_value = "0")]
        pair_entry_confirm_obs: Vec<usize>,
        /// Momentum exit: hard time stop — exit a position this many minutes after entry
        /// regardless of price (0 = off). Applied to every config in the grid.
        #[arg(long, default_value_t = 0)]
        max_hold_min: u32,
        /// Momentum exit: breakeven stop — once a position goes green, exit if it falls
        /// back to/through the entry price (don't let a winner round-trip into a loser).
        #[arg(long, default_value_t = false)]
        breakeven: bool,
        /// Momentum exit: ATR (Chandelier) vol-stop multipliers k to sweep
        /// (stop = peak − k·ATR). Comma-separated; omit for the default grid.
        #[arg(long, value_delimiter = ',')]
        atr_ks: Option<Vec<f64>>,
        /// Momentum exit: σ-scaled vol-stop multipliers k to sweep
        /// (eff trail% = k·σ·100). Comma-separated; omit for the default grid.
        #[arg(long, value_delimiter = ',')]
        sigma_ks: Option<Vec<f64>>,
        /// Window(s) in observations for the ATR/σ vol-stop measure. Comma-separated;
        /// omit for the default grid. e.g. --vol-obs 60,120
        #[arg(long, value_delimiter = ',')]
        vol_obs: Option<Vec<usize>>,
        /// Sweep fixed trailing stops ONLY — disable the ATR/σ vol-stop variants.
        #[arg(long, default_value_t = false)]
        no_vol_stops: bool,
        /// Momentum exit: profit-protected give-back caps (percent) to sweep — a green
        /// position rides down to max(cost-breakeven, peak−max_trail%). Comma-separated;
        /// omit for the default grid, `0` for fixed-trail only. e.g. --max-trail-pcts 0,15,25,40
        #[arg(long, value_delimiter = ',')]
        max_trail_pcts: Option<Vec<f64>>,
        /// Position sizing: reinvest fractions of banked profit to sweep (equity
        /// compounding). Omit ⇒ fixed size only; `0` = fixed baseline. e.g.
        /// --reinvest-fracs 0,0.25,0.5,1.0
        #[arg(long, value_delimiter = ',')]
        reinvest_fracs: Option<Vec<f64>>,
        /// Size ceilings as multiples of the base trade_usdc, for the compounding sweep.
        /// Omit for the default grid. e.g. --size-ceilings 2,3,5
        #[arg(long, value_delimiter = ',')]
        size_ceilings: Option<Vec<f64>>,
    },
    /// Run ONE fixed momentum config on each token in isolation and report per-token P&L.
    PerToken {
        /// Rank metric: sortino｜sharpe｜slope_r2｜return.
        #[arg(long, default_value = "slope_r2")]
        metric: String,
        /// Entry threshold in the metric's units (slope_r2 ≈ thousands).
        #[arg(long, default_value_t = 100.0)]
        min_metric: f64,
        /// Trailing-stop width, percent.
        #[arg(long, default_value_t = 6.0)]
        trail: f64,
        /// Lookback observations for the metric window.
        #[arg(long, default_value_t = 1440)]
        lookback: usize,
        /// Over-extension run cap, percent (0 = off).
        #[arg(long, default_value_t = 0.0)]
        max_run: f64,
        /// Regime filter: only enter when SOL is above its N-obs MA (0 = off).
        #[arg(long, default_value_t = 1440)]
        regime_obs: usize,
        /// USDC notional per trade.
        #[arg(long, default_value_t = 1000.0)]
        trade_usdc: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        /// Strategy: momentum (default) or meanrev (trend-filtered mean-reversion).
        #[arg(long, default_value = "momentum")]
        strategy: String,
        /// meanrev: enter when z ≤ −z_entry.
        #[arg(long, default_value_t = 2.0)]
        z_entry: f64,
        /// meanrev: exit when z ≥ z_exit.
        #[arg(long, default_value_t = 0.0)]
        z_exit: f64,
        /// meanrev: stop when z ≤ −z_stop.
        #[arg(long, default_value_t = 4.0)]
        z_stop: f64,
        /// meanrev: trend filter — only buy a dip when price is above its N-obs MA (0=off).
        #[arg(long, default_value_t = 0)]
        trend_obs: usize,
        /// momentum: mean-reversion entry confirmation — also require the token oversold
        /// (z ≤ −entry_dip_z over the last N obs) before entering. 0 = off (pure momentum).
        #[arg(long, default_value_t = 0)]
        entry_dip_obs: usize,
        #[arg(long, default_value_t = 1.0)]
        entry_dip_z: f64,
        /// momentum exit: volatility-scaled trailing stop — `atr` exits at
        /// peak − k×ATR(vol_obs); `sigma` uses eff trail% = k×σ×100; `off` = fixed --trail %.
        #[arg(long, default_value = "atr")]
        vol_mode: String,
        /// momentum exit: volatility-scaled trailing-stop multiplier k (used by --vol-mode
        /// atr/sigma). 0 = off (use fixed --trail %).
        #[arg(long, default_value_t = 0.0)]
        chandelier_k: f64,
        /// window for ATR / σ / overbought-z volatility.
        #[arg(long, default_value_t = 120)]
        vol_obs: usize,
        /// momentum exit: profit-protected give-back cap (percent). Once green, ride down to
        /// max(cost-breakeven, peak−this%). 0 = off (fixed/vol stop governs throughout).
        #[arg(long, default_value_t = 0.0)]
        max_trail_pct: f64,
        /// momentum exit: overbought take-profit — while green, exit when z over vol_obs
        /// ≥ this. 0 = off.
        #[arg(long, default_value_t = 0.0)]
        overbought_z: f64,
        /// dip entry reversal confirmation: also require price up over last N obs (buy
        /// the bounce, not the falling knife). 0 = off. Only used with --entry-dip-obs.
        #[arg(long, default_value_t = 0)]
        dip_confirm_obs: usize,
    },
    /// Compare regime gating head-to-head — **none** vs **SOL>MA level** vs
    /// **SOL trend-strength** (regime momentum) — over one fixed config, isolating the
    /// regime effect on held-out P&L and trade count. The candidate stream is built once
    /// and replayed under each mask, so only the entry-timing gate changes.
    RegimeCompare {
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        /// Fixed ranking metric (regime is the only thing varied). Default slope_r2.
        #[arg(long, default_value = "slope_r2")]
        metric: String,
        #[arg(long, default_value_t = 240)]
        lookback: usize,
        #[arg(long, default_value_t = 12.0)]
        trail: f64,
        #[arg(long, default_value_t = 0.0)]
        max_run: f64,
        #[arg(long, default_value_t = 0.0)]
        min_metric: f64,
        #[arg(long, default_value_t = 100.0)]
        trade_usdc: f64,
        /// Level-gate MA windows to compare (SOL>MA over N obs). e.g. 240,480,720
        #[arg(long, value_delimiter = ',', default_value = "240,480,720")]
        level_obs: Vec<usize>,
        /// Trend-gate (regime-momentum) windows for SOL slope_r2. Thresholds are derived
        /// from each window's train-slice quantiles (no peeking). e.g. 240,480,720
        #[arg(long, value_delimiter = ',', default_value = "240,480,720")]
        trend_obs: Vec<usize>,
    },
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let cfg = PortfolioConfig::from_env()?;

    match cli.command {
        Command::Run {
            train_frac,
            quick,
            top,
            tokens,
            history,
            csv,
            max_step,
            optimistic_fill,
            lookbacks,
            trails,
            rotate_factors,
            min_trades,
            strategy,
            regime_obs,
            regime_trend_obs,
            pair_cost_bps,
            pair_funding_bps_day,
            max_hold_min,
            breakeven,
            pair_entry_confirm_obs,
            atr_ks,
            sigma_ks,
            vol_obs,
            no_vol_stops,
            max_trail_pcts,
            reinvest_fracs,
            size_ceilings,
        } => run(RunArgs {
            cfg: &cfg, train_frac, quick, top, tokens, history_override: history, csv_path: &csv,
            max_step, optimistic_fill, lookbacks_override: lookbacks, trails_override: trails,
            rotate_factors, min_trades,
            strategy, regime_obs, regime_trend_obs, pair_cost_bps, pair_funding_bps_day, max_hold_min, breakeven,
            pair_entry_confirm_obs,
            atr_ks,
            sigma_ks,
            vol_obs,
            no_vol_stops,
            max_trail_pcts,
            reinvest_fracs,
            size_ceilings,
        }),
        Command::PerToken {
            metric,
            min_metric,
            trail,
            lookback,
            max_run,
            regime_obs,
            trade_usdc,
            tokens,
            history,
            max_step,
            train_frac,
            strategy,
            z_entry,
            z_exit,
            z_stop,
            trend_obs,
            entry_dip_obs,
            entry_dip_z,
            vol_mode,
            chandelier_k,
            vol_obs,
            max_trail_pct,
            overbought_z,
            dip_confirm_obs,
        } => {
            let m = metric
                .parse::<RankMetric>()
                .map_err(|e| anyhow::anyhow!("bad --metric: {e}"))?;
            per_token(PerTokenArgs {
                cfg: &cfg,
                metric: m,
                min_metric,
                trail,
                lookback,
                max_run,
                regime_obs,
                trade_usdc,
                tokens,
                history_override: history,
                max_step,
                train_frac,
                strategy,
                z_entry,
                z_exit,
                z_stop,
                trend_obs,
                entry_dip_obs,
                entry_dip_z,
                vol_mode,
                chandelier_k,
                vol_obs,
                max_trail_pct,
                overbought_z,
                dip_confirm_obs,
            })
        }
        Command::RegimeCompare {
            train_frac, tokens, history, max_step, metric, lookback, trail, max_run,
            min_metric, trade_usdc, level_obs, trend_obs,
        } => {
            let m = metric.parse::<RankMetric>().map_err(|e| anyhow::anyhow!("bad --metric: {e}"))?;
            regime_compare(RegimeCompareArgs {
                cfg: &cfg, train_frac, tokens, history_override: history, max_step, metric: m,
                lookback, trail, max_run, min_metric, trade_usdc, level_obs, trend_obs,
            })
        }
    }
}

struct PerTokenArgs<'a> {
    cfg: &'a PortfolioConfig,
    metric: RankMetric,
    min_metric: f64,
    trail: f64,
    lookback: usize,
    max_run: f64,
    regime_obs: usize,
    trade_usdc: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
    train_frac: f64,
    strategy: String,
    z_entry: f64,
    z_exit: f64,
    z_stop: f64,
    trend_obs: usize,
    entry_dip_obs: usize,
    entry_dip_z: f64,
    vol_mode: String,
    chandelier_k: f64,
    vol_obs: usize,
    max_trail_pct: f64,
    overbought_z: f64,
    dip_confirm_obs: usize,
}

/// Run one fully-specified config on each watched token in isolation (single-token
/// universe per run) and print a per-token P&L breakdown. Supports momentum and
/// trend-filtered mean-reversion via `strategy`.
fn per_token(a: PerTokenArgs) -> Result<()> {
    let PerTokenArgs {
        cfg,
        metric,
        min_metric,
        trail,
        lookback,
        max_run,
        regime_obs,
        trade_usdc,
        tokens,
        history_override,
        max_step,
        train_frac,
        strategy,
        z_entry,
        z_exit,
        z_stop,
        trend_obs,
        entry_dip_obs,
        entry_dip_z,
        vol_mode,
        chandelier_k,
        vol_obs,
        max_trail_pct,
        overbought_z,
        dip_confirm_obs,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?
        .into_iter()
        .collect();
    let snapshots = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snapshots.len() >= 200, "only {} snapshots — need more history", snapshots.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);
    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;

    // ── trend-filtered mean-reversion per token ──
    if strategy == "meanrev" {
        let p = MeanRevParams {
            lookback_obs: lookback,
            z_entry,
            z_exit,
            z_stop,
            trend_filter_obs: trend_obs,
            reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
            max_trades_per_day: cfg.momentum_max_trades_per_day,
            trade_usdc,
            slippage_bps: cfg.momentum_slippage_bps,
            max_cost_bps: cfg.momentum_max_cost_bps,
        };
        println!(
            "Per-token MEAN-REVERSION — lookback={lookback} z_entry={z_entry} z_exit={z_exit} z_stop={z_stop} trend_filter_obs={trend_obs} trade_usdc={trade_usdc}"
        );
        println!(
            "Loaded {} snapshots (spike-filtered, max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens.\n",
            snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), watched.len()
        );
        println!(
            "{:<10} {:>12} {:>7} {:>12} {:>7} {:>7} {:>12}",
            "token", "train_pnl", "tr_trd", "test_pnl", "te_trd", "te_win%", "total_pnl"
        );
        println!("{}", "─".repeat(74));
        let (mut tot_tr, mut tot_te) = (0.0_f64, 0.0_f64);
        for w in &watched {
            let single = vec![w.clone()];
            let r_tr = sim::replay_meanrev_full(train, &single, &p);
            let r_te = sim::replay_meanrev_full(test, &single, &p);
            tot_tr += r_tr.net_pnl();
            tot_te += r_te.net_pnl();
            println!(
                "{:<10} {:>+12.2} {:>7} {:>+12.2} {:>7} {:>6.0}% {:>+12.2}",
                w.symbol, r_tr.net_pnl(), r_tr.n_trades(), r_te.net_pnl(), r_te.n_trades(),
                r_te.win_rate(), r_tr.net_pnl() + r_te.net_pnl()
            );
        }
        println!("{}", "─".repeat(74));
        println!("{:<10} {:>+12.2} {:>7} {:>+12.2}", "TOTAL", tot_tr, "", tot_te);
        return Ok(());
    }

    let base = ParamSet {
        metric,
        min_metric,
        trail_pct: trail,
        lookback_obs: lookback,
        max_run_pct: max_run,
        rotate_margin: 0.0, // rotation off
        regime_filter_obs: regime_obs,
        regime_mode: RegimeMode::Level,
        regime_threshold: 0.0,
        decel_lookback_min: cfg.momentum_decel_lookback_min,
        confirm_lag_obs: cfg.momentum_confirm_lag_obs,
        stale_minutes: cfg.momentum_stale_minutes,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
        exit_on_fade: cfg.momentum_exit_on_fade,
        vol_stop_mode: VolStopMode::parse(&vol_mode)
            .ok_or_else(|| anyhow::anyhow!("bad --vol-mode (want off|atr|sigma): {vol_mode}"))?,
        chandelier_k,
        vol_obs,
        overbought_z,
        entry_dip_obs,
        entry_dip_z,
        dip_confirm_obs,
        optimistic_fill: false,
        max_hold_min: 0,
        breakeven_exit: false,
        max_trail_pct,
        reinvest_frac: 0.0,
        size_ceiling_usdc: trade_usdc,
    };

    println!(
        "Per-token MOMENTUM (rotation off) — metric={metric} min_metric={min_metric} trail={trail}% lookback={lookback} max_run={max_run}% regime_obs={regime_obs} vol_mode={vol_mode} chandelier_k={chandelier_k} vol_obs={vol_obs} overbought_z={overbought_z} trade_usdc={trade_usdc}"
    );
    println!(
        "Frozen from .env: decel={} confirm_lag={} stale_min={} cooldown_s={} max_trades/day={} slippage={}bps max_cost={}bps exit_on_fade={}",
        base.decel_lookback_min, base.confirm_lag_obs, base.stale_minutes, base.reentry_cooldown_secs,
        base.max_trades_per_day, base.slippage_bps, base.max_cost_bps, base.exit_on_fade
    );
    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    println!(
        "Loaded {} snapshots (spike-filtered, max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens.\n",
        snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), watched.len()
    );

    println!(
        "{:<10} {:>12} {:>7} {:>12} {:>7} {:>7} {:>12}",
        "token", "train_pnl", "tr_trd", "test_pnl", "te_trd", "te_win%", "total_pnl"
    );
    println!("{}", "─".repeat(74));
    let (mut tot_tr, mut tot_te) = (0.0_f64, 0.0_f64);
    for w in &watched {
        let single = vec![w.clone()];
        let s_tr = sim::ranked_stream(train, &single, &base);
        let r_tr = sim::replay_with_stream(train, &single, &s_tr, &base);
        let s_te = sim::ranked_stream(test, &single, &base);
        let r_te = sim::replay_with_stream(test, &single, &s_te, &base);
        tot_tr += r_tr.net_pnl();
        tot_te += r_te.net_pnl();
        println!(
            "{:<10} {:>+12.2} {:>7} {:>+12.2} {:>7} {:>6.0}% {:>+12.2}",
            w.symbol, r_tr.net_pnl(), r_tr.n_trades(), r_te.net_pnl(), r_te.n_trades(),
            r_te.win_rate(), r_tr.net_pnl() + r_te.net_pnl()
        );
    }
    println!("{}", "─".repeat(74));
    println!("{:<10} {:>+12.2} {:>7} {:>+12.2}", "TOTAL", tot_tr, "", tot_te);
    Ok(())
}

struct RegimeCompareArgs<'a> {
    cfg: &'a PortfolioConfig,
    train_frac: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
    metric: RankMetric,
    lookback: usize,
    trail: f64,
    max_run: f64,
    min_metric: f64,
    trade_usdc: f64,
    level_obs: Vec<usize>,
    trend_obs: Vec<usize>,
}

/// q-quantile of an already-sorted slice (nearest-rank). `q` in [0,1].
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (((sorted.len() - 1) as f64) * q.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Three-way regime comparison (none / SOL>MA level / SOL trend-strength). Builds one
/// candidate stream per slice and replays it under each regime mask, so only entry
/// timing changes. Level sweeps MA windows; trend sweeps (window × train-quantile
/// threshold); each reports its best held-out (test) P&L row.
fn regime_compare(a: RegimeCompareArgs) -> Result<()> {
    let RegimeCompareArgs {
        cfg, train_frac, tokens, history_override, max_step, metric, lookback, trail, max_run,
        min_metric, trade_usdc, level_obs, trend_obs,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?
        .into_iter()
        .collect();
    let snapshots = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snapshots.len() >= 200, "only {} snapshots — need more history", snapshots.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);

    // Fixed config — regime is the ONLY thing varied. Frozen knobs from .env via base_params.
    let mut base = base_params(cfg);
    base.metric = metric;
    base.lookback_obs = lookback;
    base.trail_pct = trail;
    base.max_run_pct = max_run;
    base.min_metric = min_metric;
    base.trade_usdc = trade_usdc;
    base.rotate_margin = 0.0; // rotation off — clean single-name comparison
    base.regime_filter_obs = 0; // masks supplied externally; don't double-gate

    // One stream per slice — identical across every regime mode (isolates the regime effect).
    let s_tr = sim::ranked_stream(train, &watched, &base);
    let s_te = sim::ranked_stream(test, &watched, &base);

    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    println!(
        "Regime comparison — metric={metric} lookback={lookback} trail={trail}% max_run={max_run}% min_metric={min_metric} trade_usdc={trade_usdc}"
    );
    println!(
        "Loaded {} snapshots (max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens.\n",
        snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), watched.len()
    );

    struct Row { mode: String, param: String, pnl_tr: f64, pnl_te: f64, trd_te: usize, win_te: f64, dd_te: f64 }
    let run_mask = |mask_tr: &[bool], mask_te: &[bool], mode: &str, param: String| -> Row {
        let r_tr = sim::replay_with_regime(train, &watched, &s_tr, &base, mask_tr);
        let r_te = sim::replay_with_regime(test, &watched, &s_te, &base, mask_te);
        Row {
            mode: mode.into(), param,
            pnl_tr: r_tr.net_pnl(), pnl_te: r_te.net_pnl(),
            trd_te: r_te.n_trades(), win_te: r_te.win_rate(), dd_te: r_te.max_drawdown_pct(),
        }
    };

    let mut rows: Vec<Row> = Vec::new();
    // 1) No regime — baseline.
    rows.push(run_mask(&vec![true; train.len()], &vec![true; test.len()], "none", "—".into()));

    // 2) Level (SOL>MA) — best window by held-out P&L.
    let mut best_level: Option<Row> = None;
    for &w in level_obs.iter().filter(|&&w| w > 0) {
        let r = run_mask(&sim::regime_mask(train, w), &sim::regime_mask(test, w), "level", format!("MA{w}"));
        if best_level.as_ref().map_or(true, |b| r.pnl_te > b.pnl_te) {
            best_level = Some(r);
        }
    }
    rows.extend(best_level);

    // 3) Trend (regime momentum) — sweep window × train-quantile threshold; best held-out P&L.
    let mut best_trend: Option<Row> = None;
    for &w in trend_obs.iter().filter(|&&w| w > 0) {
        let series = sim::sol_slope_r2_series(train, w);
        if series.is_empty() {
            continue;
        }
        let mut sorted = series.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        for &q in &[0.0_f64, 0.25, 0.5, 0.70, 0.85] {
            let thr = quantile(&sorted, q);
            let r = run_mask(
                &sim::regime_mask_trend(train, w, thr),
                &sim::regime_mask_trend(test, w, thr),
                "trend",
                format!("sl{w}@p{:.0}", q * 100.0),
            );
            if best_trend.as_ref().map_or(true, |b| r.pnl_te > b.pnl_te) {
                best_trend = Some(r);
            }
        }
    }
    rows.extend(best_trend);

    println!(
        "{:<7} {:<11} {:>10} {:>10} {:>7} {:>6} {:>8} {:>11}",
        "mode", "param", "pnl_test", "pnl_train", "trd_te", "win%", "maxDD%", "pnl/trade"
    );
    println!("{}", "─".repeat(76));
    for r in &rows {
        let per = if r.trd_te > 0 { r.pnl_te / r.trd_te as f64 } else { 0.0 };
        println!(
            "{:<7} {:<11} {:>+10.2} {:>+10.2} {:>7} {:>5.0}% {:>7.1}% {:>+11.3}",
            r.mode, r.param, r.pnl_te, r.pnl_tr, r.trd_te, r.win_te, r.dd_te.abs(), per
        );
    }
    println!(
        "\nRead: a regime gate earns its place only if it lifts pnl_test AND cuts trd_te vs `none`.\n\
         43 days ≈ one regime — treat a win as suggestive, not proven; re-run as history grows."
    );
    Ok(())
}

/// Frozen knobs come from `.env`; the swept fields are placeholders overwritten by the grid.
fn base_params(cfg: &PortfolioConfig) -> ParamSet {
    ParamSet {
        metric: cfg.momentum_rank_metric,
        min_metric: cfg.momentum_min_score,
        trail_pct: cfg.momentum_trail_pct,
        lookback_obs: cfg.momentum_lookback_obs,
        max_run_pct: cfg.momentum_max_run_pct,
        rotate_margin: cfg.momentum_rotate_margin,
        regime_filter_obs: 0,
        regime_mode: RegimeMode::Level,
        regime_threshold: 0.0,
        decel_lookback_min: cfg.momentum_decel_lookback_min,
        confirm_lag_obs: cfg.momentum_confirm_lag_obs,
        stale_minutes: cfg.momentum_stale_minutes,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc: cfg.momentum_trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
        exit_on_fade: cfg.momentum_exit_on_fade,
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
        size_ceiling_usdc: cfg.momentum_trade_usdc,
    }
}

struct RunArgs<'a> {
    cfg: &'a PortfolioConfig,
    train_frac: f64,
    quick: bool,
    top: usize,
    tokens: Option<String>,
    history_override: Option<String>,
    csv_path: &'a str,
    max_step: f64,
    optimistic_fill: bool,
    lookbacks_override: Option<Vec<usize>>,
    trails_override: Option<Vec<f64>>,
    rotate_factors: Vec<f64>,
    min_trades: usize,
    strategy: StrategyArg,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    pair_cost_bps: u32,
    pair_funding_bps_day: f64,
    max_hold_min: u32,
    breakeven: bool,
    pair_entry_confirm_obs: Vec<usize>,
    atr_ks: Option<Vec<f64>>,
    sigma_ks: Option<Vec<f64>>,
    vol_obs: Option<Vec<usize>>,
    no_vol_stops: bool,
    max_trail_pcts: Option<Vec<f64>>,
    reinvest_fracs: Option<Vec<f64>>,
    size_ceilings: Option<Vec<f64>>,
}

fn run(a: RunArgs) -> Result<()> {
    let RunArgs {
        cfg,
        train_frac,
        quick,
        top,
        tokens,
        history_override,
        csv_path,
        max_step,
        optimistic_fill,
        lookbacks_override,
        trails_override,
        rotate_factors,
        min_trades,
        strategy,
        regime_obs,
        regime_trend_obs,
        pair_cost_bps,
        pair_funding_bps_day,
        max_hold_min,
        breakeven,
        pair_entry_confirm_obs,
        atr_ks,
        sigma_ks,
        vol_obs,
        no_vol_stops,
        max_trail_pcts,
        reinvest_fracs,
        size_ceilings,
    } = a;
    anyhow::ensure!(
        train_frac > 0.0 && train_frac < 1.0,
        "--train-frac must be between 0 and 1 (got {train_frac})"
    );

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());

    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading price history from {history_path}"))?
        .into_iter()
        .collect();
    let count_prices = |s: &[history::PriceSnapshot]| -> usize { s.iter().map(|x| x.prices.len()).sum() };
    let before = count_prices(&raw);
    let snapshots = sim::sanitize_history(&raw, max_step);
    let dropped = before.saturating_sub(count_prices(&snapshots));
    if max_step > 1.0 {
        println!("Spike filter (max_step={max_step}×): dropped {dropped} glitch price print(s).");
    }
    anyhow::ensure!(
        snapshots.len() >= 200,
        "only {} snapshots in {history_path} — need more history to backtest",
        snapshots.len()
    );
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading watched universe from {tokens_path}"))?;

    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);
    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    println!(
        "Loaded {} snapshots ({} watched tokens). Train {} (~{:.1}d) / Test {} (~{:.1}d).",
        snapshots.len(),
        watched.len(),
        train.len(),
        span_days(train),
        test.len(),
        span_days(test),
    );

    match strategy {
        StrategyArg::Momentum => momentum_grid(MomentumGrid {
            train,
            test,
            watched: &watched,
            cfg,
            quick,
            top,
            csv_path,
            optimistic_fill,
            lookbacks_override,
            trails_override,
            rotate_factors,
            min_trades,
            regime_obs,
            regime_trend_obs,
            max_hold_min,
            breakeven,
            atr_ks,
            sigma_ks,
            vol_obs,
            no_vol_stops,
            max_trail_pcts,
            reinvest_fracs,
            size_ceilings,
        }),
        StrategyArg::Meanrev => meanrev_grid(MeanRevGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        }),
        StrategyArg::Pairs => pairs_grid(PairsGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
            pair_cost_bps, pair_funding_bps_day, pair_entry_confirm_obs,
        }),
        StrategyArg::Relval => relval_grid(PairsGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
            pair_cost_bps, pair_funding_bps_day, pair_entry_confirm_obs,
        }),
        StrategyArg::Relstrength => relstrength_grid(MeanRevGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        }),
    }
}

struct MomentumGrid<'a> {
    train: &'a [history::PriceSnapshot],
    test: &'a [history::PriceSnapshot],
    watched: &'a [momentum_universe::WatchedToken],
    cfg: &'a PortfolioConfig,
    quick: bool,
    top: usize,
    csv_path: &'a str,
    optimistic_fill: bool,
    lookbacks_override: Option<Vec<usize>>,
    trails_override: Option<Vec<f64>>,
    rotate_factors: Vec<f64>,
    min_trades: usize,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    max_hold_min: u32,
    breakeven: bool,
    atr_ks: Option<Vec<f64>>,
    sigma_ks: Option<Vec<f64>>,
    vol_obs: Option<Vec<usize>>,
    no_vol_stops: bool,
    max_trail_pcts: Option<Vec<f64>>,
    reinvest_fracs: Option<Vec<f64>>,
    size_ceilings: Option<Vec<f64>>,
}

fn momentum_grid(g: MomentumGrid) -> Result<()> {
    let MomentumGrid {
        train,
        test,
        watched,
        cfg,
        quick,
        top,
        csv_path,
        optimistic_fill,
        lookbacks_override,
        trails_override,
        rotate_factors,
        min_trades,
        regime_obs,
        regime_trend_obs,
        max_hold_min,
        breakeven,
        atr_ks,
        sigma_ks,
        vol_obs,
        no_vol_stops,
        max_trail_pcts,
        reinvest_fracs,
        size_ceilings,
    } = g;
    let (metrics, def_lookbacks, max_runs, trails, quantiles) = if quick {
        (GRID_METRICS.to_vec(), vec![121, 480], vec![0.0, 10.0], vec![6.0, 10.0], vec![0.70, 0.90])
    } else {
        (
            GRID_METRICS.to_vec(),
            GRID_LOOKBACKS.to_vec(),
            GRID_MAX_RUNS.to_vec(),
            GRID_TRAILS.to_vec(),
            GRID_MIN_QUANTILES.to_vec(),
        )
    };
    let lookbacks = match lookbacks_override {
        Some(v) if !v.is_empty() => {
            anyhow::ensure!(v.iter().all(|&l| l > 120), "every --lookbacks value must exceed 120");
            v
        }
        _ => def_lookbacks,
    };
    let trails = match trails_override {
        Some(v) if !v.is_empty() => {
            anyhow::ensure!(v.iter().all(|&t| t > 0.0), "every --trails value must be > 0");
            v
        }
        _ => trails,
    };
    let rotate_factors = if rotate_factors.is_empty() {
        vec![0.0]
    } else {
        rotate_factors
    };
    let regime_obs = if regime_obs.is_empty() {
        vec![0]
    } else {
        regime_obs
    };
    // Volatility-stop sweep: off entirely with --no-vol-stops, else CLI overrides or the
    // default grid (a trimmed set in --quick to keep the grid small).
    let (atr_ks, sigma_ks, vol_obs_set) = if no_vol_stops {
        (vec![], vec![], vec![])
    } else if quick {
        (
            atr_ks.unwrap_or_else(|| vec![3.0]),
            sigma_ks.unwrap_or_else(|| vec![5.0]),
            vol_obs.unwrap_or_else(|| vec![120]),
        )
    } else {
        (
            atr_ks.unwrap_or_else(|| GRID_ATR_KS.to_vec()),
            sigma_ks.unwrap_or_else(|| GRID_SIGMA_KS.to_vec()),
            vol_obs.unwrap_or_else(|| GRID_VOL_OBS.to_vec()),
        )
    };
    // Profit-protected give-back sweep: CLI override, or the default grid (trimmed in
    // --quick). Independent of the vol-stop set; `0` entries are ignored (fixed-trail
    // is already the GRID_TRAILS baseline).
    let max_trails: Vec<f64> = match max_trail_pcts {
        Some(v) => v.into_iter().filter(|&m| m > 0.0).collect(),
        None if quick => vec![25.0],
        None => GRID_MAX_TRAILS.to_vec(),
    };
    // Equity-compounding sizing sweep: off unless --reinvest-fracs is passed. The `0`
    // fraction is the fixed-size baseline; ceilings are multiples of base trade_usdc.
    let reinvest_fracs: Vec<f64> = reinvest_fracs.unwrap_or_else(|| vec![0.0]);
    let size_ceiling_mults: Vec<f64> = size_ceilings.unwrap_or_else(|| GRID_SIZE_CEILING_MULTS.to_vec());
    let stop_variant_count =
        sim::stop_variants(&trails, &atr_ks, &sigma_ks, &vol_obs_set, &max_trails).len();
    let sizing_count = sim::sizing_variants(1.0, &reinvest_fracs, &size_ceiling_mults).len();
    println!(
        "Strategy: MOMENTUM. Grid: {} metrics × {} lookbacks × {} max_runs × {} stop-variants ({} fixed + {} ATR-k×{} σ-k over {} vol-obs + {} max-trail) × {} thresholds × {} rotate-factors × {} regime-level-windows (+{} trend-windows×3 thr) × {} sizing.",
        metrics.len(), lookbacks.len(), max_runs.len(), stop_variant_count,
        trails.len(), atr_ks.len(), sigma_ks.len(), vol_obs_set.len(), max_trails.len(),
        quantiles.len(), rotate_factors.len(), regime_obs.len(), regime_trend_obs.len(), sizing_count,
    );
    let mut base = base_params(cfg);
    base.optimistic_fill = optimistic_fill;
    base.max_hold_min = max_hold_min;
    base.breakeven_exit = breakeven;
    println!(
        "Fill model: {} stop fills.  Robustness gate: ≥{min_trades} trades in BOTH slices.",
        if optimistic_fill { "OPTIMISTIC (same-bar, upper bound)" } else { "conservative (next-snapshot)" }
    );
    if max_hold_min > 0 || breakeven {
        println!(
            "Extra exits: max-hold={} breakeven={}",
            if max_hold_min > 0 { format!("{max_hold_min}min") } else { "off".into() },
            if breakeven { "on" } else { "off" },
        );
    }
    let results = sim::run_grid(
        train,
        test,
        watched,
        &base,
        &metrics,
        &lookbacks,
        &max_runs,
        &trails,
        &quantiles,
        &rotate_factors,
        &regime_obs,
        &regime_trend_obs,
        &atr_ks,
        &sigma_ks,
        &vol_obs_set,
        &max_trails,
        &reinvest_fracs,
        &size_ceiling_mults,
    );
    anyhow::ensure!(!results.is_empty(), "grid produced no results");

    let mut robust: Vec<&SimResult> = results.iter().filter(|r| r.is_robust(min_trades)).collect();
    robust.sort_by(|a, b| worst_slice(b).partial_cmp(&worst_slice(a)).unwrap_or(std::cmp::Ordering::Equal));
    println!(
        "\n=== VERDICT: {}/{} configs ROBUST (profitable in train AND test, ≥{min_trades} trades each) ===",
        robust.len(), results.len()
    );
    if robust.is_empty() {
        println!("No robust edge in this sample. Showing best-by-test-P&L below — treat as overfit (not deployable).");
        print_table(&results, top);
    } else {
        println!("Robust configs (sorted by worst-slice P&L — most dependable first):");
        let owned: Vec<SimResult> = robust.iter().map(|r| (*r).clone()).collect();
        print_table(&owned, top);
        print_env_block(robust[0]);
    }
    write_csv(csv_path, &results)?;
    println!("\nFull grid ({} rows) written to {csv_path}", results.len());
    Ok(())
}

fn worst_slice(r: &SimResult) -> f64 {
    r.net_pnl_train.min(r.net_pnl_test)
}

struct MeanRevGrid<'a> {
    train: &'a [history::PriceSnapshot],
    test: &'a [history::PriceSnapshot],
    watched: &'a [momentum_universe::WatchedToken],
    cfg: &'a PortfolioConfig,
    quick: bool,
    top: usize,
    csv_path: &'a str,
    lookbacks_override: Option<Vec<usize>>,
    min_trades: usize,
}

fn meanrev_grid(g: MeanRevGrid) -> Result<()> {
    let MeanRevGrid { train, test, watched, cfg, quick, top, csv_path, lookbacks_override, min_trades } = g;
    let lookbacks: Vec<usize> = match lookbacks_override {
        Some(v) if !v.is_empty() => {
            anyhow::ensure!(
                v.iter().all(|&l| l > sim::MEANREV_MIN_OBS),
                "every --lookbacks value must exceed {}", sim::MEANREV_MIN_OBS
            );
            v
        }
        _ => if quick { vec![60, 240] } else { MR_LOOKBACKS.to_vec() },
    };
    let (z_entries, z_exits, z_stops) = if quick {
        (vec![2.0, 2.5], vec![0.0], vec![4.0])
    } else {
        (MR_Z_ENTRY.to_vec(), MR_Z_EXIT.to_vec(), MR_Z_STOP.to_vec())
    };
    println!(
        "Strategy: MEAN-REVERSION (buy z≤−entry, sell on reversion). Grid: {} lookbacks × {} z_entry × {} z_exit × {} z_stop. Gate: ≥{min_trades} trades both slices.",
        lookbacks.len(), z_entries.len(), z_exits.len(), z_stops.len(),
    );
    let base = MeanRevParams {
        lookback_obs: 120,
        z_entry: 2.0,
        z_exit: 0.0,
        z_stop: 4.0,
        trend_filter_obs: 0,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc: cfg.momentum_trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
    };
    // Trend filter ("buy the pullback in an uptrend"): 0 = off, plus a couple of
    // confirmed-uptrend windows. State-machine only, so sweeping it is cheap.
    let trend_filter = if quick { vec![0usize, 480] } else { vec![0usize, 240, 480] };
    println!("Trend filter (uptrend MA windows, 0=off): {trend_filter:?}");
    let results = sim::run_grid_meanrev(
        train, test, watched, &base, &lookbacks, &z_entries, &z_exits, &z_stops, &trend_filter,
    );
    anyhow::ensure!(!results.is_empty(), "grid produced no results");

    let mut robust: Vec<&MeanRevResult> = results.iter().filter(|r| r.is_robust(min_trades)).collect();
    robust.sort_by(|a, b| {
        let (ka, kb) = (a.net_pnl_train.min(a.net_pnl_test), b.net_pnl_train.min(b.net_pnl_test));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\n=== VERDICT: {}/{} configs ROBUST (profitable in train AND test, ≥{min_trades} trades each) ===",
        robust.len(), results.len()
    );
    if robust.is_empty() {
        println!("No robust mean-reversion edge in this sample. Best-by-test below (treat as overfit):");
        print_table_mr(&results, top);
    } else {
        println!("Robust configs (sorted by worst-slice P&L — most dependable first):");
        let owned: Vec<MeanRevResult> = robust.iter().map(|r| (*r).clone()).collect();
        print_table_mr(&owned, top);
        let b = &robust[0].params;
        println!("\nBest robust mean-reversion config (research params — the live trader doesn't implement this strategy yet):");
        println!("  lookback_obs={}  z_entry={:.2}  z_exit={:.2}  z_stop={:.2}", b.lookback_obs, b.z_entry, b.z_exit, b.z_stop);
    }
    write_csv_mr(csv_path, &results)?;
    println!("\nFull grid ({} rows) written to {csv_path}", results.len());
    Ok(())
}

fn relstrength_grid(g: MeanRevGrid) -> Result<()> {
    let MeanRevGrid { train, test, watched, cfg, quick, top, csv_path, lookbacks_override, min_trades } = g;
    let lookbacks: Vec<usize> = match lookbacks_override {
        Some(v) if !v.is_empty() => {
            anyhow::ensure!(v.iter().all(|&l| l > 120), "every --lookbacks value must exceed 120");
            v
        }
        _ => if quick { vec![121, 480] } else { GRID_LOOKBACKS.to_vec() },
    };
    let metrics = if quick {
        vec![RankMetric::SlopeR2, RankMetric::Return]
    } else {
        GRID_METRICS.to_vec()
    };
    let (trails, quantiles) = if quick {
        (vec![6.0, 10.0], vec![0.70, 0.90])
    } else {
        (GRID_TRAILS.to_vec(), GRID_MIN_QUANTILES.to_vec())
    };
    let base = RelStrengthParams {
        metric: RankMetric::SlopeR2,
        min_metric: 0.0,
        lookback_obs: 121,
        trail_pct: 6.0,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        notional_usdc: cfg.momentum_trade_usdc,
        cost_bps: cfg.momentum_slippage_bps,
    };
    println!(
        "Strategy: RELATIVE-STRENGTH market-neutral momentum (long leader, short SOL). Grid: {} metrics × {} lookbacks × {} trails × {} thresholds.",
        metrics.len(), lookbacks.len(), trails.len(), quantiles.len(),
    );
    println!(
        "notional/leg={} cost={}bps/leg (×4 round-trip) hedge=SOL. Gate: ≥{min_trades} trades both slices.",
        base.notional_usdc, base.cost_bps
    );
    let results =
        sim::run_grid_relstrength(train, test, watched, &metrics, &lookbacks, &trails, &quantiles, &base);
    anyhow::ensure!(!results.is_empty(), "grid produced no results");

    let mut robust: Vec<&RelStrengthResult> = results.iter().filter(|r| r.is_robust(min_trades)).collect();
    robust.sort_by(|a, b| {
        let (ka, kb) = (a.net_pnl_train.min(a.net_pnl_test), b.net_pnl_train.min(b.net_pnl_test));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\n=== VERDICT: {}/{} configs ROBUST (profitable in train AND test, ≥{min_trades} trades each) ===",
        robust.len(), results.len()
    );
    if robust.is_empty() {
        println!("No robust relative-strength edge in this sample. Best-by-test below (treat as overfit):");
        print_table_rs(&results, top);
    } else {
        println!("Robust configs (sorted by worst-slice P&L — most dependable first):");
        let owned: Vec<RelStrengthResult> = robust.iter().map(|r| (*r).clone()).collect();
        print_table_rs(&owned, top);
        let b = &robust[0].params;
        println!(
            "\nBest robust config: metric={} min={:.2} trail={:.1}% lookback={} (long leader / short SOL)",
            b.metric, b.min_metric, b.trail_pct, b.lookback_obs
        );
    }
    write_csv_rs(csv_path, &results)?;
    println!("\nFull grid ({} rows) written to {csv_path}", results.len());
    Ok(())
}

fn print_table_rs(results: &[RelStrengthResult], top: usize) {
    println!(
        "\n{:<8} {:>10} {:>6} {:>9} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "metric", "min", "trail", "lookback", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
    );
    println!("{}", "─".repeat(86));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:<8} {:>10.4} {:>5.1}% {:>9} {:>+11.2} {:>+11.2} {:>7} {:>6.0}% {:>6.1}%",
            p.metric.to_string(), p.min_metric, p.trail_pct, p.lookback_obs,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.win_rate_test, r.max_dd_test,
        );
    }
}

fn write_csv_rs(path: &str, results: &[RelStrengthResult]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    writeln!(f, "metric,min_metric,trail_pct,lookback_obs,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,max_dd_test")?;
    for r in results {
        let p = &r.params;
        writeln!(
            f, "{},{},{},{},{:.4},{:.4},{},{},{:.2},{:.2}",
            p.metric, p.min_metric, p.trail_pct, p.lookback_obs,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.n_trades_train, r.win_rate_test, r.max_dd_test,
        )?;
    }
    Ok(())
}

/// Compact regime descriptor for tables: `off` | `MA{obs}` (level) | `T{obs}` (trend).
/// The trend threshold is carried at full precision in the CSV and the env block.
fn regime_desc(p: &ParamSet) -> String {
    match p.regime_mode {
        RegimeMode::Off => "off".to_string(),
        RegimeMode::Level => format!("MA{}", p.regime_filter_obs),
        RegimeMode::Trend => format!("T{}", p.regime_filter_obs),
    }
}

fn print_table(results: &[SimResult], top: usize) {
    println!(
        "\n{:<8} {:>10} {:>6} {:>9} {:>8} {:>8} {:>9} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "metric", "min", "trail", "lookback", "maxrun", "rotate", "regime", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
    );
    println!("{}", "─".repeat(114));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:<8} {:>10.4} {:>5.1}% {:>9} {:>7.1}% {:>8.3} {:>9} {:>+11.2} {:>+11.2} {:>7} {:>6.0}% {:>6.1}%",
            p.metric.to_string(),
            p.min_metric,
            p.trail_pct,
            p.lookback_obs,
            p.max_run_pct,
            p.rotate_margin,
            regime_desc(p),
            r.net_pnl_test,
            r.net_pnl_train,
            r.n_trades_test,
            r.win_rate_test,
            r.max_dd_test,
        );
    }
}

struct PairsGrid<'a> {
    train: &'a [history::PriceSnapshot],
    test: &'a [history::PriceSnapshot],
    watched: &'a [WatchedToken],
    cfg: &'a PortfolioConfig,
    quick: bool,
    top: usize,
    csv_path: &'a str,
    lookbacks_override: Option<Vec<usize>>,
    min_trades: usize,
    pair_cost_bps: u32,
    pair_funding_bps_day: f64,
    pair_entry_confirm_obs: Vec<usize>,
}

fn pairs_grid(g: PairsGrid) -> Result<()> {
    let PairsGrid {
        train, test, watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        pair_cost_bps, pair_funding_bps_day, pair_entry_confirm_obs,
    } = g;
    // Every unordered pair of watched tokens.
    let mut pairs: Vec<(WatchedToken, WatchedToken)> = Vec::new();
    for i in 0..watched.len() {
        for j in (i + 1)..watched.len() {
            pairs.push((watched[i].clone(), watched[j].clone()));
        }
    }
    let lookbacks: Vec<usize> = match lookbacks_override {
        Some(v) if !v.is_empty() => {
            anyhow::ensure!(
                v.iter().all(|&l| l > sim::PAIRS_MIN_OBS),
                "every --lookbacks value must exceed {}", sim::PAIRS_MIN_OBS
            );
            v
        }
        _ => if quick { vec![120, 240] } else { vec![120, 240, 480] },
    };
    let (z_entries, z_exits, z_stops) = if quick {
        (vec![2.0, 2.5], vec![0.5], vec![4.0])
    } else {
        (vec![2.0, 2.5, 3.0], vec![0.0, 0.5], vec![3.5, 4.5])
    };
    let base = PairParams {
        lookback_obs: 120,
        z_entry: 2.0,
        z_exit: 0.5,
        z_stop: 4.0,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        notional_usdc: cfg.momentum_trade_usdc,
        cost_bps: pair_cost_bps,
        funding_bps_per_day: pair_funding_bps_day,
        entry_confirm_obs: 0, // swept below
    };
    println!(
        "Strategy: MARKET-NEUTRAL PAIRS (spread ln(A/B), dollar-neutral). {} pairs × {} lookbacks × {} z_entry × {} z_exit × {} z_stop.",
        pairs.len(), lookbacks.len(), z_entries.len(), z_exits.len(), z_stops.len(),
    );
    println!(
        "Costs: {pair_cost_bps} bps/leg ×4 per round-trip; funding/borrow {pair_funding_bps_day} bps/day (≈{:.1}% APY on the short leg). Gate: ≥{min_trades} trades both slices.",
        pair_funding_bps_day * 365.0 / 100.0,
    );

    let confirms = if pair_entry_confirm_obs.is_empty() { vec![0] } else { pair_entry_confirm_obs };
    println!("Reversal-confirm entry windows (0=off): {confirms:?}");
    let results = sim::run_grid_pairs(train, test, &pairs, &base, &lookbacks, &z_entries, &z_exits, &z_stops, &confirms);
    anyhow::ensure!(!results.is_empty(), "no pair had enough overlapping history to backtest");

    let mut robust: Vec<&PairResult> = results.iter().filter(|r| r.is_robust(min_trades)).collect();
    robust.sort_by(|a, b| {
        let (ka, kb) = (a.net_pnl_train.min(a.net_pnl_test), b.net_pnl_train.min(b.net_pnl_test));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\n=== VERDICT: {}/{} pair-configs ROBUST (profitable in train AND test, ≥{min_trades} trades each) ===",
        robust.len(), results.len()
    );
    if robust.is_empty() {
        println!("No robust market-neutral edge in this sample. Best-by-test below (treat as overfit):");
        print_table_pairs(&results, top);
        println!("\n→ Phase-0 conclusion: the spread does not converge profitably here — building perps execution is NOT justified yet.");
    } else {
        println!("Robust pairs (sorted by worst-slice P&L — most dependable first):");
        let owned: Vec<PairResult> = robust.iter().map(|r| (*r).clone()).collect();
        print_table_pairs(&owned, top);
        let b = robust[0];
        println!(
            "\n→ Phase-0 conclusion: {}/{}={} robust pair-config(s) exist. Best: {}–{} (lb {} z {:.1}/{:.1}/{:.1}). Worth scoping perps execution.",
            robust.len(), results.len(), robust.len(),
            b.symbol_a, b.symbol_b, b.params.lookback_obs, b.params.z_entry, b.params.z_exit, b.params.z_stop,
        );
    }
    write_csv_pairs(csv_path, &results)?;
    println!("\nFull grid ({} rows) written to {csv_path}", results.len());
    Ok(())
}

fn print_table_pairs(results: &[PairResult], top: usize) {
    println!(
        "\n{:<9} {:<9} {:>9} {:>8} {:>7} {:>7} {:>5} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "A", "B", "lookback", "z_entry", "z_exit", "z_stop", "cfm", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
    );
    println!("{}", "─".repeat(114));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:<9} {:<9} {:>9} {:>8.2} {:>7.2} {:>7.2} {:>5} {:>+11.2} {:>+11.2} {:>7} {:>6.0}% {:>6.1}%",
            r.symbol_a, r.symbol_b, p.lookback_obs, p.z_entry, p.z_exit, p.z_stop, p.entry_confirm_obs,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.win_rate_test, r.max_dd_test,
        );
    }
}

fn write_csv_pairs(path: &str, results: &[PairResult]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    writeln!(
        f,
        "symbol_a,symbol_b,lookback_obs,z_entry,z_exit,z_stop,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,max_dd_test"
    )?;
    for r in results {
        let p = &r.params;
        writeln!(
            f,
            "{},{},{},{},{},{},{:.4},{:.4},{},{},{:.2},{:.2}",
            r.symbol_a, r.symbol_b, p.lookback_obs, p.z_entry, p.z_exit, p.z_stop,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.n_trades_train,
            r.win_rate_test, r.max_dd_test,
        )?;
    }
    Ok(())
}

fn relval_grid(g: PairsGrid) -> Result<()> {
    // Long-only spot: pair_cost_bps/funding don't apply (uses cfg slippage + gas).
    let PairsGrid { train, test, watched, cfg, quick, top, csv_path, lookbacks_override, min_trades, .. } = g;
    let mut pairs: Vec<(WatchedToken, WatchedToken)> = Vec::new();
    for i in 0..watched.len() {
        for j in (i + 1)..watched.len() {
            pairs.push((watched[i].clone(), watched[j].clone()));
        }
    }
    let lookbacks: Vec<usize> = match lookbacks_override {
        Some(v) if !v.is_empty() => {
            anyhow::ensure!(
                v.iter().all(|&l| l > sim::PAIRS_MIN_OBS),
                "every --lookbacks value must exceed {}", sim::PAIRS_MIN_OBS
            );
            v
        }
        _ => if quick { vec![120, 240] } else { vec![120, 240, 480] },
    };
    let (z_entries, z_exits, z_stops) = if quick {
        (vec![2.0, 2.5], vec![0.5], vec![4.0])
    } else {
        (vec![2.0, 2.5, 3.0], vec![0.25, 0.5], vec![3.5, 4.5])
    };
    let base = RelValParams {
        lookback_obs: 120,
        z_entry: 2.0,
        z_exit: 0.5,
        z_stop: 4.0,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc: cfg.momentum_trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
    };
    println!(
        "Strategy: LONG-ONLY RELATIVE VALUE (buy the cheap leg, spot). {} pairs × {} lookbacks × {} z_entry × {} z_exit × {} z_stop. Gate: ≥{min_trades} trades both slices.",
        pairs.len(), lookbacks.len(), z_entries.len(), z_exits.len(), z_stops.len(),
    );
    println!("Fill: conservative spot ({} bps slippage/leg + gas).", base.slippage_bps);

    let results = sim::run_grid_relval(train, test, &pairs, &base, &lookbacks, &z_entries, &z_exits, &z_stops);
    anyhow::ensure!(!results.is_empty(), "no pair had enough overlapping history to backtest");

    let mut robust: Vec<&RelValResult> = results.iter().filter(|r| r.is_robust(min_trades)).collect();
    robust.sort_by(|a, b| {
        let (ka, kb) = (a.net_pnl_train.min(a.net_pnl_test), b.net_pnl_train.min(b.net_pnl_test));
        kb.partial_cmp(&ka).unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\n=== VERDICT: {}/{} relval-configs ROBUST (profitable in train AND test, ≥{min_trades} trades each) ===",
        robust.len(), results.len()
    );
    if robust.is_empty() {
        println!("No robust long-only relative-value edge in this sample. Best-by-test below (treat as overfit):");
        print_table_relval(&results, top);
    } else {
        println!("Robust configs (sorted by worst-slice P&L — most dependable first). These are SPOT-EXECUTABLE:");
        let owned: Vec<RelValResult> = robust.iter().map(|r| (*r).clone()).collect();
        print_table_relval(&owned, top);
        let b = robust[0];
        println!(
            "\n→ Best spot-executable edge: long the cheap leg of {}/{} (lb {} z {:.1}/{:.1}/{:.1}). Robust in both slices.",
            b.symbol_a, b.symbol_b, b.params.lookback_obs, b.params.z_entry, b.params.z_exit, b.params.z_stop,
        );
    }
    write_csv_relval(csv_path, &results)?;
    println!("\nFull grid ({} rows) written to {csv_path}", results.len());
    Ok(())
}

fn print_table_relval(results: &[RelValResult], top: usize) {
    println!(
        "\n{:<9} {:<9} {:>9} {:>8} {:>7} {:>7} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "longs", "vs", "lookback", "z_entry", "z_exit", "z_stop", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
    );
    println!("{}", "─".repeat(108));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:<9} {:<9} {:>9} {:>8.2} {:>7.2} {:>7.2} {:>+11.2} {:>+11.2} {:>7} {:>6.0}% {:>6.1}%",
            r.symbol_a, r.symbol_b, p.lookback_obs, p.z_entry, p.z_exit, p.z_stop,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.win_rate_test, r.max_dd_test,
        );
    }
}

fn write_csv_relval(path: &str, results: &[RelValResult]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    writeln!(
        f,
        "symbol_a,symbol_b,lookback_obs,z_entry,z_exit,z_stop,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,max_dd_test"
    )?;
    for r in results {
        let p = &r.params;
        writeln!(
            f,
            "{},{},{},{},{},{},{:.4},{:.4},{},{},{:.2},{:.2}",
            r.symbol_a, r.symbol_b, p.lookback_obs, p.z_entry, p.z_exit, p.z_stop,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.n_trades_train,
            r.win_rate_test, r.max_dd_test,
        )?;
    }
    Ok(())
}

fn print_table_mr(results: &[MeanRevResult], top: usize) {
    println!(
        "\n{:>9} {:>8} {:>7} {:>7} {:>7} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "lookback", "z_entry", "z_exit", "z_stop", "trend", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
    );
    println!("{}", "─".repeat(94));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:>9} {:>8.2} {:>7.2} {:>7.2} {:>7} {:>+11.2} {:>+11.2} {:>7} {:>6.0}% {:>6.1}%",
            p.lookback_obs, p.z_entry, p.z_exit, p.z_stop, p.trend_filter_obs,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.win_rate_test, r.max_dd_test,
        );
    }
}

fn write_csv_mr(path: &str, results: &[MeanRevResult]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    writeln!(
        f,
        "lookback_obs,z_entry,z_exit,z_stop,trend_filter_obs,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,max_dd_test"
    )?;
    for r in results {
        let p = &r.params;
        writeln!(
            f,
            "{},{},{},{},{},{:.4},{:.4},{},{},{:.2},{:.2}",
            p.lookback_obs, p.z_entry, p.z_exit, p.z_stop, p.trend_filter_obs,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.n_trades_train,
            r.win_rate_test, r.max_dd_test,
        )?;
    }
    Ok(())
}

fn write_csv(path: &str, results: &[SimResult]) -> Result<()> {
    if let Some(parent) = Path::new(path).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let mut f = std::fs::File::create(path).with_context(|| format!("creating {path}"))?;
    writeln!(
        f,
        "metric,min_metric,trail_pct,lookback_obs,max_run_pct,rotate_margin,regime_mode,regime_filter_obs,regime_threshold,vol_stop_mode,vol_k,vol_obs,max_trail_pct,reinvest_frac,size_ceiling_usdc,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,max_dd_test"
    )?;
    for r in results {
        let p = &r.params;
        writeln!(
            f,
            "{},{},{},{},{},{:.4},{},{},{:.2},{},{:.4},{},{},{:.4},{:.2},{:.4},{:.4},{},{},{:.2},{:.2}",
            p.metric,
            p.min_metric,
            p.trail_pct,
            p.lookback_obs,
            p.max_run_pct,
            p.rotate_margin,
            p.regime_mode,
            p.regime_filter_obs,
            p.regime_threshold,
            p.vol_stop_mode.as_str(),
            p.chandelier_k,
            p.vol_obs,
            p.max_trail_pct,
            p.reinvest_frac,
            p.size_ceiling_usdc,
            r.net_pnl_test,
            r.net_pnl_train,
            r.n_trades_test,
            r.n_trades_train,
            r.win_rate_test,
            r.max_dd_test,
        )?;
    }
    Ok(())
}

fn print_env_block(best: &SimResult) {
    let p = &best.params;
    println!(
        "\nBest by held-out net P&L ({:+.2} USDC test, {:+.2} train) — paste into .env:",
        best.net_pnl_test, best.net_pnl_train
    );
    println!("  MOMENTUM_RANK_METRIC={}", p.metric);
    println!("  MOMENTUM_MIN_METRIC={:.4}", p.min_metric);
    println!("  MOMENTUM_TRAIL_PCT={:.1}", p.trail_pct);
    println!("  MOMENTUM_LOOKBACK_OBS={}", p.lookback_obs);
    println!("  MOMENTUM_MAX_RUN_PCT={:.1}", p.max_run_pct);
    println!("  MOMENTUM_ROTATE_MARGIN={:.4}", p.rotate_margin);
    match p.vol_stop_mode {
        VolStopMode::Off => println!("  MOMENTUM_VOL_STOP_MODE=off   # fixed-% trailing stop"),
        mode => {
            println!("  MOMENTUM_VOL_STOP_MODE={}", mode.as_str());
            println!("  MOMENTUM_CHANDELIER_K={:.2}", p.chandelier_k);
            println!("  MOMENTUM_VOL_OBS={}", p.vol_obs);
            println!("  #   (MOMENTUM_TRAIL_PCT above is the warmup fallback for the vol stop)");
        }
    }
    if p.max_trail_pct > 0.0 {
        println!("  MOMENTUM_MAX_TRAIL_PCT={:.1}   # green positions give back up to this from peak, floored at breakeven", p.max_trail_pct);
    }
    if p.reinvest_frac > 0.0 {
        println!("  MOMENTUM_REINVEST_FRAC={:.2}   # compound this fraction of banked profit into the entry size", p.reinvest_frac);
        println!("  MOMENTUM_SIZE_CEILING_USDC={:.2}", p.size_ceiling_usdc);
    }
    match p.regime_mode {
        RegimeMode::Off => {}
        RegimeMode::Level => {
            println!("  MOMENTUM_REGIME_MODE=level");
            println!("  MOMENTUM_REGIME_OBS={}", p.regime_filter_obs);
        }
        RegimeMode::Trend => {
            println!("  MOMENTUM_REGIME_MODE=trend");
            println!("  MOMENTUM_REGIME_OBS={}", p.regime_filter_obs);
            println!("  MOMENTUM_REGIME_TREND_MIN={:.2}", p.regime_threshold);
        }
    }
}
