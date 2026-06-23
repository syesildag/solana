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

use solana_mev::portfolio::sim::{
    self, MeanRevParams, MeanRevResult, PairParams, PairResult, ParamSet, RelValParams,
    RelStrengthParams, RelStrengthResult, RelValResult, SimResult, GRID_LOOKBACKS, GRID_MAX_RUNS,
    GRID_METRICS, GRID_MIN_QUANTILES, GRID_TRAILS, MR_LOOKBACKS, MR_Z_ENTRY, MR_Z_EXIT, MR_Z_STOP,
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
use solana_mev::portfolio::{history, momentum_universe, PortfolioConfig, RankMetric};

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
        /// Pairs strategy: per-leg trading cost (slippage + perp/swap fee), bps.
        #[arg(long, default_value_t = 15)]
        pair_cost_bps: u32,
        /// Pairs strategy: borrow/funding drag on the short leg, bps PER DAY held.
        /// Plug in the live Kamino xStock borrow APY ÷ 365 to test on-chain viability.
        #[arg(long, default_value_t = 0.0)]
        pair_funding_bps_day: f64,
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
        /// momentum exit: volatility-scaled (Chandelier) trailing stop — exit at
        /// peak − k×ATR(vol_obs). 0 = off (use fixed --trail %).
        #[arg(long, default_value_t = 0.0)]
        chandelier_k: f64,
        /// window for ATR / overbought-z volatility.
        #[arg(long, default_value_t = 120)]
        vol_obs: usize,
        /// momentum exit: overbought take-profit — while green, exit when z over vol_obs
        /// ≥ this. 0 = off.
        #[arg(long, default_value_t = 0.0)]
        overbought_z: f64,
    },
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cli = Cli::parse();
    let cfg = PortfolioConfig::from_env()?;

    match cli.command {
        Command::Run {
            train_frac, quick, top, tokens, history, csv, max_step, optimistic_fill,
            lookbacks, rotate_factors, min_trades, strategy, regime_obs,
            pair_cost_bps, pair_funding_bps_day,
        } => run(RunArgs {
            cfg: &cfg, train_frac, quick, top, tokens, history_override: history, csv_path: &csv,
            max_step, optimistic_fill, lookbacks_override: lookbacks, rotate_factors, min_trades,
            strategy, regime_obs, pair_cost_bps, pair_funding_bps_day,
        }),
        Command::PerToken {
            metric, min_metric, trail, lookback, max_run, regime_obs, trade_usdc,
            tokens, history, max_step, train_frac, strategy, z_entry, z_exit, z_stop, trend_obs,
            entry_dip_obs, entry_dip_z, chandelier_k, vol_obs, overbought_z,
        } => {
            let m = metric
                .parse::<RankMetric>()
                .map_err(|e| anyhow::anyhow!("bad --metric: {e}"))?;
            per_token(PerTokenArgs {
                cfg: &cfg, metric: m, min_metric, trail, lookback, max_run, regime_obs,
                trade_usdc, tokens, history_override: history, max_step, train_frac,
                strategy, z_entry, z_exit, z_stop, trend_obs, entry_dip_obs, entry_dip_z,
                chandelier_k, vol_obs, overbought_z,
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
    chandelier_k: f64,
    vol_obs: usize,
    overbought_z: f64,
}

/// Run one fully-specified config on each watched token in isolation (single-token
/// universe per run) and print a per-token P&L breakdown. Supports momentum and
/// trend-filtered mean-reversion via `strategy`.
fn per_token(a: PerTokenArgs) -> Result<()> {
    let PerTokenArgs {
        cfg, metric, min_metric, trail, lookback, max_run, regime_obs, trade_usdc,
        tokens, history_override, max_step, train_frac, strategy, z_entry, z_exit, z_stop, trend_obs,
        entry_dip_obs, entry_dip_z, chandelier_k, vol_obs, overbought_z,
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
        decel_lookback_min: cfg.momentum_decel_lookback_min,
        confirm_lag_obs: cfg.momentum_confirm_lag_obs,
        stale_minutes: cfg.momentum_stale_minutes,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
        exit_on_fade: cfg.momentum_exit_on_fade,
        chandelier_k,
        vol_obs,
        overbought_z,
        entry_dip_obs,
        entry_dip_z,
        optimistic_fill: false,
    };

    println!(
        "Per-token MOMENTUM (rotation off) — metric={metric} min_metric={min_metric} trail={trail}% lookback={lookback} max_run={max_run}% regime_obs={regime_obs} chandelier_k={chandelier_k} vol_obs={vol_obs} overbought_z={overbought_z} trade_usdc={trade_usdc}"
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
        decel_lookback_min: cfg.momentum_decel_lookback_min,
        confirm_lag_obs: cfg.momentum_confirm_lag_obs,
        stale_minutes: cfg.momentum_stale_minutes,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc: cfg.momentum_trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
        exit_on_fade: cfg.momentum_exit_on_fade,
        chandelier_k: 0.0,
        vol_obs: 0,
        overbought_z: 0.0,
        entry_dip_obs: 0,
        entry_dip_z: 0.0,
        optimistic_fill: false,
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
    rotate_factors: Vec<f64>,
    min_trades: usize,
    strategy: StrategyArg,
    regime_obs: Vec<usize>,
    pair_cost_bps: u32,
    pair_funding_bps_day: f64,
}

fn run(a: RunArgs) -> Result<()> {
    let RunArgs {
        cfg, train_frac, quick, top, tokens, history_override, csv_path, max_step,
        optimistic_fill, lookbacks_override, rotate_factors, min_trades, strategy, regime_obs,
        pair_cost_bps, pair_funding_bps_day,
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
            train, test, watched: &watched, cfg, quick, top, csv_path,
            optimistic_fill, lookbacks_override, rotate_factors, min_trades, regime_obs,
        }),
        StrategyArg::Meanrev => meanrev_grid(MeanRevGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        }),
        StrategyArg::Pairs => pairs_grid(PairsGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
            pair_cost_bps, pair_funding_bps_day,
        }),
        StrategyArg::Relval => relval_grid(PairsGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
            pair_cost_bps, pair_funding_bps_day,
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
    rotate_factors: Vec<f64>,
    min_trades: usize,
    regime_obs: Vec<usize>,
}

fn momentum_grid(g: MomentumGrid) -> Result<()> {
    let MomentumGrid {
        train, test, watched, cfg, quick, top, csv_path, optimistic_fill, lookbacks_override,
        rotate_factors, min_trades, regime_obs,
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
    let rotate_factors = if rotate_factors.is_empty() { vec![0.0] } else { rotate_factors };
    let regime_obs = if regime_obs.is_empty() { vec![0] } else { regime_obs };
    println!(
        "Strategy: MOMENTUM. Grid: {} metrics × {} lookbacks × {} max_runs × {} trails × {} thresholds × {} rotate-factors × {} regime-windows.",
        metrics.len(), lookbacks.len(), max_runs.len(), trails.len(), quantiles.len(), rotate_factors.len(), regime_obs.len(),
    );
    let mut base = base_params(cfg);
    base.optimistic_fill = optimistic_fill;
    println!(
        "Fill model: {} stop fills.  Robustness gate: ≥{min_trades} trades in BOTH slices.",
        if optimistic_fill { "OPTIMISTIC (same-bar, upper bound)" } else { "conservative (next-snapshot)" }
    );
    let results = sim::run_grid(
        train, test, watched, &base, &metrics, &lookbacks, &max_runs, &trails, &quantiles,
        &rotate_factors, &regime_obs,
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

fn print_table(results: &[SimResult], top: usize) {
    println!(
        "\n{:<8} {:>10} {:>6} {:>9} {:>8} {:>8} {:>7} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "metric", "min", "trail", "lookback", "maxrun", "rotate", "regime", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
    );
    println!("{}", "─".repeat(112));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:<8} {:>10.4} {:>5.1}% {:>9} {:>7.1}% {:>8.3} {:>7} {:>+11.2} {:>+11.2} {:>7} {:>6.0}% {:>6.1}%",
            p.metric.to_string(),
            p.min_metric,
            p.trail_pct,
            p.lookback_obs,
            p.max_run_pct,
            p.rotate_margin,
            p.regime_filter_obs,
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
}

fn pairs_grid(g: PairsGrid) -> Result<()> {
    let PairsGrid {
        train, test, watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        pair_cost_bps, pair_funding_bps_day,
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
    };
    println!(
        "Strategy: MARKET-NEUTRAL PAIRS (spread ln(A/B), dollar-neutral). {} pairs × {} lookbacks × {} z_entry × {} z_exit × {} z_stop.",
        pairs.len(), lookbacks.len(), z_entries.len(), z_exits.len(), z_stops.len(),
    );
    println!(
        "Costs: {pair_cost_bps} bps/leg ×4 per round-trip; funding/borrow {pair_funding_bps_day} bps/day (≈{:.1}% APY on the short leg). Gate: ≥{min_trades} trades both slices.",
        pair_funding_bps_day * 365.0 / 100.0,
    );

    let results = sim::run_grid_pairs(train, test, &pairs, &base, &lookbacks, &z_entries, &z_exits, &z_stops);
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
        "\n{:<9} {:<9} {:>9} {:>8} {:>7} {:>7} {:>11} {:>11} {:>7} {:>7} {:>7}",
        "A", "B", "lookback", "z_entry", "z_exit", "z_stop", "pnl_test", "pnl_train", "trades", "win%", "maxDD%",
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
        "metric,min_metric,trail_pct,lookback_obs,max_run_pct,rotate_margin,regime_filter_obs,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,max_dd_test"
    )?;
    for r in results {
        let p = &r.params;
        writeln!(
            f,
            "{},{},{},{},{},{:.4},{},{:.4},{:.4},{},{},{:.2},{:.2}",
            p.metric, p.min_metric, p.trail_pct, p.lookback_obs, p.max_run_pct, p.rotate_margin, p.regime_filter_obs,
            r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.n_trades_train,
            r.win_rate_test, r.max_dd_test,
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
    if p.regime_filter_obs > 0 {
        println!(
            "  # NOTE: best config uses a SOL>MA({}-obs) regime filter — not yet a live-trader knob; implement before deploying.",
            p.regime_filter_obs
        );
    }
}
