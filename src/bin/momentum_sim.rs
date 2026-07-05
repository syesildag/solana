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
use rayon::prelude::*;

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

/// What the `run` grid optimizes for when ranking robust configs (momentum only).
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Objective {
    /// Worst-slice net P&L, min(pnl_train, pnl_test) — today's default, unchanged.
    NetPnl,
    /// Worst-slice capital efficiency, min($/h_train, $/h_test) — P&L per hour deployed.
    PnlPerHold,
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
        /// Ranking objective for robust configs: net-pnl (default; worst-slice P&L,
        /// unchanged behavior) or pnl-per-hold (worst-slice $/hour-deployed — capital
        /// efficiency). Momentum strategy only.
        #[arg(long, value_enum, default_value_t = Objective::NetPnl)]
        objective: Objective,
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
        /// Pairs/relval strategy: comma-separated z_exit (take-profit) bands to sweep,
        /// overriding the default grid. Must be > 0 (|z| <= 0 is unreachable for a float, so
        /// 0.0 disables the take-profit and forces every trade to ride to the stop).
        /// e.g. --z-exits 0.1,0.25,0.5,0.75,1.0
        #[arg(long, value_delimiter = ',')]
        z_exits: Option<Vec<f64>>,
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
        /// Overbought entry gate window (obs) for the mean-reversion filter. `0` = gate off
        /// (default; grid unchanged). When > 0, the grid sweeps `off` plus each --entry-max-zs
        /// threshold over this window. e.g. --entry-max-z-obs 480
        #[arg(long, default_value_t = 0)]
        entry_max_z_obs: usize,
        /// Overbought entry-gate z thresholds to sweep (only used when --entry-max-z-obs > 0).
        /// Block a new entry when the candidate's z over the window exceeds the value. Omit ⇒
        /// gate off. e.g. --entry-max-zs 0.5,1.0,1.5,2.0
        #[arg(long, value_delimiter = ',')]
        entry_max_zs: Option<Vec<f64>>,
        /// Multi-metric sign-confirmation Ks to sweep: a candidate may enter only when ≥ K of
        /// its 4 metrics (sortino/sharpe/slope_r2/return) are > 0. 0 = off (default; grid
        /// unchanged). Note sortino/sharpe/return always share sign, so K=2 ≡ K=3; K=4
        /// additionally requires a positive regression slope. e.g. --confirm-ks 0,3,4
        #[arg(long, value_delimiter = ',', default_value = "0")]
        confirm_ks: Vec<usize>,
        /// After ranking, replay the single most-dependable (worst-slice) robust config and
        /// print its individual round-trip trades (entry/exit time, token, prices, P&L) for
        /// the TRAIN and TEST slices. Momentum strategy only; no effect when no robust config.
        #[arg(long, default_value_t = false)]
        dump_trades: bool,
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
        /// Which regime gate --regime-obs drives: level (SOL>MA, default) or trend
        /// (SOL slope_r2 ≥ --regime-trend-min over the window). off ignores both.
        #[arg(long, default_value = "level")]
        regime_mode: String,
        /// Min SOL slope_r2 for --regime-mode trend (annualized slope×R² units).
        #[arg(long, default_value_t = 0.0)]
        regime_trend_min: f64,
        /// Print every round-trip trade (entry/exit time, prices, P&L) per token
        /// for the TRAIN and TEST slices, after the summary table.
        #[arg(long, default_value_t = false)]
        dump_trades: bool,
        /// Stop-on-fade: with exit_on_fade, also exit UNDERWATER positions whose metric
        /// faded below min (drops the green gate) — small early losses instead of
        /// ride-to-trail losers. Sim-only experiment knob.
        #[arg(long, default_value_t = false)]
        fade_stop: bool,
        /// Force-exit any position held longer than this many minutes (0 = off).
        #[arg(long, default_value_t = 0)]
        max_hold_min: u32,
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
        /// momentum: overbought entry gate — block a new entry when the candidate's z-score
        /// over the last N obs exceeds --entry-max-z (extended above its mean). 0 = off.
        #[arg(long, default_value_t = 0)]
        entry_max_z_obs: usize,
        #[arg(long, default_value_t = 1.0)]
        entry_max_z: f64,
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
    /// Compare holding up to N concurrent positions (N=1..max_n) under ONE fixed config,
    /// isolating the max-positions effect on held-out P&L. Fixed notional per slot, so the
    /// table also reports P&L per $1k deployed (higher N deploys more capital).
    MaxnCompare {
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
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
        /// Rotation margin in the metric's units (>0 enables weakest-green eviction). 0 = off.
        #[arg(long, default_value_t = 0.0)]
        rotate_margin: f64,
        #[arg(long, default_value_t = 1000.0)]
        trade_usdc: f64,
        /// Level regime gate: only enter when SOL is above its N-obs MA. 0 = off.
        #[arg(long, default_value_t = 0)]
        regime_obs: usize,
        /// Maximum number of concurrent positions to sweep up to (rows N=1..max_n).
        #[arg(long, default_value_t = 5)]
        max_n: usize,
    },
    /// Grid-tune the hold-all-curated portfolio (N = #tokens) and the single-slot trader
    /// (N=1) each to its own best ROBUST config at EQUAL total capital (trade_usdc =
    /// pool/N), then print a head-to-head. Answers: does a best-tuned basket beat a
    /// best-tuned single name on the same money? Fixed-trail only (live-reproducible).
    MaxnOptimize {
        /// Total capital both models compete for. Default: live MOMENTUM_TRADE_USDC.
        #[arg(long)]
        pool_usdc: Option<f64>,
        /// Upper endpoint N to compare against N=1. Default: number of curated tokens.
        #[arg(long)]
        max_n: Option<usize>,
        /// Robustness gate: a config must trade ≥ this in BOTH slices to be eligible.
        #[arg(long, default_value_t = 3)]
        min_trades: usize,
        /// Selection objective for the MULTI-slot (N>1) arm only: net-pnl (default;
        /// best held-out P&L, unchanged) or pnl-per-hold (best worst-slice $/hour-deployed,
        /// same key as `run --objective pnl-per-hold`). The N=1 arm is always selected by
        /// net-pnl, so this answers: does an efficiency-tuned basket beat the best
        /// P&L-tuned single slot on the same total capital?
        #[arg(long, value_enum, default_value_t = Objective::NetPnl)]
        multi_objective: Objective,
        /// Rotation factors swept for the N=1 grid (× min_metric; 0 = off). The N=#curated
        /// grid forces [0.0] (rotation is moot when N == token count).
        #[arg(long, value_delimiter = ',', default_value = "0.0")]
        rotate_factors: Vec<f64>,
        /// Level regime-gate MA windows to sweep (0 = off). e.g. 0,480
        #[arg(long, value_delimiter = ',', default_value = "0,480")]
        regime_obs: Vec<usize>,
        /// Trend regime-gate windows to sweep (thresholds from train quantiles). e.g. 480
        #[arg(long, value_delimiter = ',', default_value = "480")]
        regime_trend_obs: Vec<usize>,
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
    },
    /// Compute each token's best {min_metric,trail,max_run} (single-name grid at the global
    /// metric/lookback), run a 3-arm equal-capital validation (single-slot global vs
    /// hold-all global vs hold-all per-token) with risk metrics, and print the verdict.
    /// --apply persists per-token params into momentum_tokens.json.
    PerTokenTune {
        #[arg(long)]
        pool_usdc: Option<f64>,
        #[arg(long, default_value_t = 3)]
        min_trades: usize,
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        #[arg(long, value_delimiter = ',', default_value = "0,480")]
        regime_obs: Vec<usize>,
        #[arg(long, value_delimiter = ',', default_value = "480")]
        regime_trend_obs: Vec<usize>,
        /// Also write the computed per-token params into momentum_tokens.json.
        #[arg(long, default_value_t = false)]
        apply: bool,
    },
    /// Reconcile realized paper performance vs the backtest prediction over the forward window.
    /// List the trader's recorded round-trips (live + paper) with the SOL
    /// trend-regime metric (slope_r2, shown as %/yr) at each trade's entry.
    Trades {
        /// Trader state file holding the closed-trade audit trail.
        #[arg(long)]
        state: Option<String>,
        /// Override the price-history path (defaults to HISTORY_PATH).
        #[arg(long)]
        history: Option<String>,
        /// SOL slope_r2 window in observations (defaults to MOMENTUM_REGIME_OBS, or 480 if unset).
        #[arg(long)]
        trend_obs: Option<usize>,
        /// Spike filter, same semantics as `run --max-step`. ≤1.0 disables it.
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
    },
    /// Perfect-foresight oracle: per-token profit ceiling under the replay's exact
    /// cost model, an achievable single-slot schedule across tokens, the capture
    /// ratio of the live .env config, and metric distributions at oracle entries.
    /// DIAGNOSTIC ONLY — oracle trades are future-peeked and must never be used as
    /// a parameter-fitting target (hypotheses still go through `run`'s walk-forward).
    Oracle {
        /// Override the watched-token list path (defaults to MOMENTUM_TOKENS_PATH).
        #[arg(long)]
        tokens: Option<String>,
        /// Override the price-history path (defaults to HISTORY_PATH).
        #[arg(long)]
        history: Option<String>,
        /// Spike filter, same semantics as `run --max-step`. ≤1.0 disables it.
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        /// Per-side slippage bps for oracle costs (default: MOMENTUM_SLIPPAGE_BPS).
        #[arg(long)]
        slippage_bps: Option<u32>,
        /// Minimum hold per oracle trade, in minutes. 0 = unconstrained (ceiling is
        /// then dominated by 1-2 snapshot print flickers no strategy can trade);
        /// ~30-60 gives the strategy-timescale ceiling.
        #[arg(long, default_value_t = 0)]
        min_hold_min: u32,
        /// How many of the schedule's largest trades to print.
        #[arg(long, default_value_t = 12)]
        show: usize,
    },
    ForwardReport {
        #[arg(long, default_value = "assets/momentum_actions.jsonl")]
        actions: String,
        #[arg(long)]
        history: Option<String>,
        /// Forward-window start (RFC3339). Defaults to the config-lock date you pass.
        #[arg(long)]
        since: Option<String>,
        #[arg(long, default_value_t = true)]
        paper_only: bool,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        #[arg(long, default_value_t = 6.0)]
        min_weeks: f64,
        #[arg(long, default_value_t = 20)]
        min_trades: usize,
        #[arg(long, default_value_t = 0.6)]
        min_pnl_frac: f64,
        #[arg(long, default_value_t = 50.0)]
        max_dd_pct: f64,
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
            objective,
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
            entry_max_z_obs,
            entry_max_zs,
            confirm_ks,
            z_exits,
            dump_trades,
        } => run(RunArgs {
            cfg: &cfg, train_frac, quick, top, tokens, history_override: history, csv_path: &csv,
            max_step, optimistic_fill, lookbacks_override: lookbacks, trails_override: trails,
            rotate_factors, min_trades,
            strategy, objective, regime_obs, regime_trend_obs, pair_cost_bps, pair_funding_bps_day, max_hold_min, breakeven,
            pair_entry_confirm_obs,
            pair_z_exits: z_exits,
            atr_ks,
            sigma_ks,
            vol_obs,
            no_vol_stops,
            max_trail_pcts,
            reinvest_fracs,
            size_ceilings,
            entry_max_z_obs,
            entry_max_zs,
            confirm_ks,
            dump_trades,
        }),
        Command::PerToken {
            metric,
            min_metric,
            trail,
            lookback,
            max_run,
            regime_obs,
            regime_mode,
            regime_trend_min,
            dump_trades,
            fade_stop,
            max_hold_min,
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
            entry_max_z_obs,
            entry_max_z,
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
                regime_mode,
                regime_trend_min,
                dump_trades,
                fade_stop,
                max_hold_min,
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
                entry_max_z_obs,
                entry_max_z,
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
        Command::MaxnCompare {
            train_frac, tokens, history, max_step, metric, lookback, trail, max_run,
            min_metric, rotate_margin, trade_usdc, regime_obs, max_n,
        } => {
            let m = metric.parse::<RankMetric>().map_err(|e| anyhow::anyhow!("bad --metric: {e}"))?;
            maxn_compare(MaxnCompareArgs {
                cfg: &cfg, train_frac, tokens, history_override: history, max_step, metric: m,
                lookback, trail, max_run, min_metric, rotate_margin, trade_usdc, regime_obs, max_n,
            })
        }
        Command::MaxnOptimize {
            pool_usdc, max_n, min_trades, multi_objective, rotate_factors, regime_obs, regime_trend_obs,
            train_frac, tokens, history, max_step,
        } => maxn_optimize(MaxnOptimizeArgs {
            cfg: &cfg, pool_usdc, max_n, min_trades, multi_objective, rotate_factors, regime_obs,
            regime_trend_obs, train_frac, tokens, history_override: history, max_step,
        }),
        Command::PerTokenTune {
            pool_usdc, min_trades, train_frac, tokens, history, max_step,
            regime_obs, regime_trend_obs, apply,
        } => per_token_tune(PerTokenTuneArgs {
            cfg: &cfg, pool_usdc, min_trades, train_frac, tokens,
            history_override: history, max_step, regime_obs, regime_trend_obs, apply,
        }),
        Command::Trades { state, history, trend_obs, max_step } => {
            live_trades_report(&cfg, state, history, trend_obs, max_step)
        }
        Command::Oracle { tokens, history, max_step, slippage_bps, min_hold_min, show } => {
            oracle_report(&cfg, tokens, history, max_step, slippage_bps, min_hold_min, show)
        }
        Command::ForwardReport {
            actions,
            history,
            since,
            paper_only,
            max_step,
            min_weeks,
            min_trades,
            min_pnl_frac,
            max_dd_pct,
        } => {
            let since_ts = match since {
                Some(s) => Some(
                    chrono::DateTime::parse_from_rfc3339(&s)
                        .map(|d| d.timestamp() as u64)
                        .map_err(|e| anyhow::anyhow!("bad --since (want RFC3339 like 2026-06-21T00:00:00Z): {e}"))?,
                ),
                None => None,
            };
            let history_path = history.unwrap_or_else(|| cfg.history_path.clone());
            let bar = solana_mev::portfolio::forward_report::GraduationBar {
                min_weeks,
                min_trades,
                min_pnl_frac,
                max_dd_pct,
            };
            solana_mev::portfolio::forward_report::run_forward_report(
                &cfg, &actions, &history_path, since_ts, paper_only, bar, max_step,
            )
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
    regime_mode: String,
    regime_trend_min: f64,
    dump_trades: bool,
    fade_stop: bool,
    max_hold_min: u32,
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
    entry_max_z_obs: usize,
    entry_max_z: f64,
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
        regime_mode,
        regime_trend_min,
        dump_trades,
        fade_stop,
        max_hold_min,
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
        entry_max_z_obs,
        entry_max_z,
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
        confirm_k: 0,
        trail_pct: trail,
        lookback_obs: lookback,
        max_run_pct: max_run,
        rotate_margin: 0.0, // rotation off
        regime_filter_obs: regime_obs,
        regime_mode: regime_mode
            .parse::<RegimeMode>()
            .map_err(|e| anyhow::anyhow!("bad --regime-mode: {e}"))?,
        regime_threshold: regime_trend_min,
        decel_lookback_min: cfg.momentum_decel_lookback_min,
        confirm_lag_obs: cfg.momentum_confirm_lag_obs,
        stale_minutes: cfg.momentum_stale_minutes,
        reentry_cooldown_secs: cfg.momentum_reentry_cooldown_secs,
        max_trades_per_day: cfg.momentum_max_trades_per_day,
        trade_usdc,
        slippage_bps: cfg.momentum_slippage_bps,
        max_cost_bps: cfg.momentum_max_cost_bps,
        exit_on_fade: cfg.momentum_exit_on_fade || fade_stop, // fade_stop implies the fade exit is armed
        fade_stop,
        vol_stop_mode: VolStopMode::parse(&vol_mode)
            .ok_or_else(|| anyhow::anyhow!("bad --vol-mode (want off|atr|sigma): {vol_mode}"))?,
        chandelier_k,
        vol_obs,
        overbought_z,
        entry_dip_obs,
        entry_dip_z,
        entry_max_z_obs,
        entry_max_z,
        dip_confirm_obs,
        optimistic_fill: false,
        max_hold_min,
        breakeven_exit: false,
        max_trail_pct,
        reinvest_frac: 0.0,
        size_ceiling_usdc: trade_usdc,
    };

    println!(
        "Per-token MOMENTUM (rotation off) — metric={metric} min_metric={min_metric} trail={trail}% lookback={lookback} max_run={max_run}% regime={}@{regime_obs} thr={regime_trend_min} vol_mode={vol_mode} chandelier_k={chandelier_k} vol_obs={vol_obs} overbought_z={overbought_z} trade_usdc={trade_usdc}",
        base.regime_mode,
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
    let mut dumps: Vec<(String, sim::SimRun, sim::SimRun)> = Vec::new();
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
        if dump_trades {
            dumps.push((w.symbol.clone(), r_tr, r_te));
        }
    }
    println!("{}", "─".repeat(74));
    println!("{:<10} {:>+12.2} {:>7} {:>+12.2}", "TOTAL", tot_tr, "", tot_te);
    // Trade dump AFTER the table: unlike run --dump-trades (regime-off winner replay),
    // this lists the trades of the EXACT config above, regime gate included.
    for (sym, r_tr, r_te) in &dumps {
        print_trades(&format!("TRAIN {sym} (exact config, regime {} applied)", base.regime_mode), r_tr);
        print_trades(&format!("TEST (held-out) {sym} (exact config, regime {} applied)", base.regime_mode), r_te);
    }
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
    let mut base = sim::base_params(cfg);
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

struct MaxnCompareArgs<'a> {
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
    rotate_margin: f64,
    trade_usdc: f64,
    regime_obs: usize,
    max_n: usize,
}

/// Replay one fixed config at N=1..max_n and print a per-N table. Capital is fixed per
/// slot, so the table reports both absolute test P&L and P&L per $1k deployed (= pnl_test
/// / (N × trade_usdc / 1000)) — N>1 must win per-dollar, not merely by deploying more.
fn maxn_compare(a: MaxnCompareArgs) -> Result<()> {
    let MaxnCompareArgs {
        cfg, train_frac, tokens, history_override, max_step, metric, lookback, trail, max_run,
        min_metric, rotate_margin, trade_usdc, regime_obs, max_n,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");
    anyhow::ensure!(max_n >= 1, "--max-n must be ≥ 1");

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

    let mut base = sim::base_params(cfg);
    base.metric = metric;
    base.lookback_obs = lookback;
    base.trail_pct = trail;
    base.max_run_pct = max_run;
    base.min_metric = min_metric;
    base.trade_usdc = trade_usdc;
    base.size_ceiling_usdc = trade_usdc; // fixed notional per slot (no compounding here)
    base.reinvest_frac = 0.0;
    base.rotate_margin = rotate_margin;
    base.regime_filter_obs = 0; // mask is supplied via regime_obs in maxn_rows; don't double-gate

    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    println!(
        "Max-N comparison — metric={metric} lookback={lookback} trail={trail}% max_run={max_run}% \
         min_metric={min_metric} rotate_margin={rotate_margin} trade_usdc={trade_usdc} regime_obs={regime_obs}"
    );
    println!(
        "Loaded {} snapshots (max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens.\n",
        snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), watched.len()
    );

    let rows = sim::maxn_rows(train, test, &watched, &base, regime_obs, max_n);

    println!(
        "{:>3} {:>10} {:>10} {:>7} {:>6} {:>8} {:>16}",
        "N", "pnl_train", "pnl_test", "trd_te", "win%", "maxDD%", "pnl_test/$1k"
    );
    println!("{}", "─".repeat(66));
    for r in &rows {
        let deployed_k = (r.n as f64) * trade_usdc / 1000.0;
        let per_k = if deployed_k > 0.0 { r.pnl_test / deployed_k } else { 0.0 };
        println!(
            "{:>3} {:>+10.2} {:>+10.2} {:>7} {:>5.0}% {:>7.1}% {:>+16.2}",
            r.n, r.pnl_train, r.pnl_test, r.trades_test, r.win_test, r.dd_test.abs(), per_k
        );
    }
    println!(
        "\nRead: N>1 earns its place only if pnl_test/$1k rises with N (not just absolute pnl_test, \
         which grows because higher N deploys more capital). Treat a short sample as suggestive, not proven."
    );
    Ok(())
}

struct MaxnOptimizeArgs<'a> {
    cfg: &'a PortfolioConfig,
    pool_usdc: Option<f64>,
    max_n: Option<usize>,
    min_trades: usize,
    multi_objective: Objective,
    rotate_factors: Vec<f64>,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    train_frac: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
}

/// Format a winning config's params into one human line.
fn fmt_cfg(r: &sim::SimResult) -> String {
    let p = &r.params;
    format!(
        "metric={} min={:.4} trail={}% lookback={} max_run={} regime={}@{} rotate={:.4} confirm={}",
        p.metric, p.min_metric, p.trail_pct, p.lookback_obs, p.max_run_pct,
        p.regime_mode, p.regime_filter_obs, p.rotate_margin, p.confirm_k,
    )
}

/// Grid-tune N=1 and N=#curated each to its best robust config at equal total capital,
/// then print the head-to-head. Fixed-trail only (no vol/max-trail/compounding) so the
/// winner is reproducible by the live trader.
fn maxn_optimize(a: MaxnOptimizeArgs) -> Result<()> {
    let MaxnOptimizeArgs {
        cfg, pool_usdc, max_n, min_trades, multi_objective, rotate_factors, regime_obs, regime_trend_obs,
        train_frac, tokens, history_override, max_step,
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
    let k_tokens = watched.len();
    anyhow::ensure!(k_tokens >= 1, "no curated tokens");
    let pool = pool_usdc.unwrap_or(cfg.momentum_trade_usdc);
    anyhow::ensure!(pool > 0.0, "--pool-usdc must be > 0");
    let upper = max_n.unwrap_or(k_tokens).max(1);

    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);

    // Arms: (N, selection objective). Normally N=1-by-pnl vs N=upper-by-multi_objective.
    // Degenerate upper==1 with a non-default objective = SAME grid, two selections —
    // the "N=1 best-pnl vs N=1 best-$/h" head-to-head.
    let arms: Vec<(usize, Objective)> = if upper <= 1 {
        if multi_objective == Objective::NetPnl {
            vec![(1, Objective::NetPnl)]
        } else {
            vec![(1, Objective::NetPnl), (1, multi_objective)]
        }
    } else {
        vec![(1, Objective::NetPnl), (upper, multi_objective)]
    };
    let same_n = arms.len() == 2 && arms[0].0 == arms[1].0;

    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    if same_n {
        println!("Best-tuned net-pnl vs pnl-per-hold at N=1 — pool ${pool} (same capital, same grid, two selections)");
    } else {
        println!("Best-tuned hold-all vs single-slot — pool ${pool} (equal total capital)");
        if multi_objective == Objective::PnlPerHold {
            println!("Arm selection: N=1 by best held-out P&L; N>1 by worst-slice $/hour-deployed (pnl-per-hold).");
        }
    }
    println!(
        "Loaded {} snapshots (max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens. min_trades={min_trades}\n",
        snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), k_tokens
    );

    let no_f: [f64; 0] = [];
    let no_u: [usize; 0] = [];
    // Annualization cadence: nominal 184 s/snapshot (matches span_days). Both N share it.
    let periods_per_year = 365.0 * 86_400.0 / 184.0;
    // One grid run per distinct N — same-N arms share the grid and differ only in selection.
    let mut results_by_n: std::collections::HashMap<usize, Vec<sim::SimResult>> =
        std::collections::HashMap::new();
    for &(n, _) in &arms {
        if results_by_n.contains_key(&n) {
            continue;
        }
        let mut base = sim::base_params(cfg);
        base.trade_usdc = pool / n as f64;
        base.size_ceiling_usdc = base.trade_usdc; // fixed notional per slot
        base.reinvest_frac = 0.0;
        // Rotation is a real lever only at N=1; moot at N == token count.
        let rf: Vec<f64> = if n == 1 { rotate_factors.clone() } else { vec![0.0] };
        let results = sim::run_grid_multi(
            train, test, &watched, &base,
            &sim::GRID_METRICS, &sim::GRID_LOOKBACKS, &sim::GRID_MAX_RUNS, &sim::GRID_TRAILS,
            &sim::GRID_MIN_QUANTILES, &rf, &regime_obs, &regime_trend_obs,
            &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, &[0], n,
        );
        results_by_n.insert(n, results);
    }

    // (N, selection objective, best robust config or None, per-slot notional, test-MTM risk)
    let mut summary: Vec<(usize, Objective, Option<sim::SimResult>, f64, Option<sim::RiskMetrics>)> =
        Vec::new();
    for &(n, arm_objective) in &arms {
        let results = &results_by_n[&n];
        let best = best_robust_by(results, min_trades, arm_objective).cloned();
        // For the winning config: mark-to-market the test slice → risk metrics.
        let risk = best.as_ref().map(|r| {
            let p = &r.params;
            let stream = sim::ranked_stream(test, &watched, p);
            let mask: Vec<bool> = match p.regime_mode {
                RegimeMode::Off => vec![true; test.len()],
                RegimeMode::Level => sim::regime_mask(test, p.regime_filter_obs),
                RegimeMode::Trend => sim::regime_mask_trend(test, p.regime_filter_obs, p.regime_threshold),
            };
            let (_, mtm) = sim::replay_multi_mtm(test, &watched, &stream, p, &mask, n);
            sim::risk_metrics(&mtm, periods_per_year)
        });
        summary.push((n, arm_objective, best, pool / n as f64, risk));
    }

    // Arm display name: objective-based when both arms share one N, N-based otherwise.
    let arm_name = |n: usize, o: Objective| -> String {
        if same_n {
            match o {
                Objective::NetPnl => "net-pnl pick (N=1)".to_string(),
                Objective::PnlPerHold => "$/h pick (N=1)".to_string(),
            }
        } else if n == 1 {
            "single-slot (N=1)".to_string()
        } else {
            format!("hold-all (N={n})")
        }
    };

    for (n, arm_objective, best, notional, risk) in &summary {
        let sel_tag = match arm_objective {
            Objective::NetPnl => "best held-out P&L",
            Objective::PnlPerHold => "best worst-slice $/h",
        };
        println!("{}  (${:.2}/slot, selected by {sel_tag}):", arm_name(*n, *arm_objective), notional);
        match (best, risk) {
            (Some(r), Some(rm)) => {
                println!("  {}", fmt_cfg(r));
                println!(
                    "  test {:+.2} | train {:+.2} | trades {} | win {:.0}%",
                    r.net_pnl_test, r.net_pnl_train, r.n_trades_test, r.win_rate_test
                );
                println!(
                    "  in-market test {:.1}h ({:+.3} $/h) | train {:.1}h ({:+.3} $/h)",
                    r.hold_hours_test, r.rate_test(), r.hold_hours_train, r.rate_train()
                );
                println!(
                    "  risk(test MTM): Sharpe {:.2} | Sortino {:.2} | trueDD {:.1}%\n",
                    rm.sharpe, rm.sortino, rm.true_max_dd_pct
                );
            }
            _ => println!("  no robust config at N={n} (min_trades={min_trades})\n"),
        }
    }

    // Verdict — only when both arms exist and both have a robust winner.
    if summary.len() == 2 {
        let (n1, o1, b1, _, rm1) = &summary[0];
        let (nk, ok, bk, _, rmk) = &summary[1];
        let name1 = arm_name(*n1, *o1);
        let namek = arm_name(*nk, *ok);
        match (b1, bk) {
            (Some(r1), Some(rk)) => {
                let (winner, delta) = if rk.net_pnl_test >= r1.net_pnl_test {
                    (namek.clone(), rk.net_pnl_test - r1.net_pnl_test)
                } else {
                    (name1.clone(), r1.net_pnl_test - rk.net_pnl_test)
                };
                println!(
                    "\nP&L VERDICT:  {winner} wins held-out P&L by {:+.2} USDC (equal ${pool} capital).",
                    delta
                );
                // Efficiency axis: $/h is per-slot (each slot's own hours), so also show
                // pool-level efficiency = test PnL per wall-clock hour of the slice.
                let wall_h = test.len() as f64 * 184.0 / 3600.0;
                println!(
                    "EFFICIENCY:   in-market — {name1}: {:.1}h ({:+.3} $/h)  vs  {namek}: {:.1}h ({:+.3} $/h). \
                     Pool-level: {:+.3} vs {:+.3} $/wall-clock-h over {:.0}h.",
                    r1.hold_hours_test, r1.rate_test(), rk.hold_hours_test, rk.rate_test(),
                    r1.net_pnl_test / wall_h, rk.net_pnl_test / wall_h, wall_h,
                );
                if same_n {
                    // Same N: the "N>1 diversification" intuition doesn't apply — just
                    // compare the two picks' risk profiles head-on.
                    if let (Some(s1), Some(sk)) = (rm1, rmk) {
                        let sharpe_w = if sk.sharpe > s1.sharpe { &namek } else { &name1 };
                        let dd_w = if sk.true_max_dd_pct < s1.true_max_dd_pct { &namek } else { &name1 };
                        println!(
                            "RISK VERDICT: Sharpe — {name1} {:.2} vs {namek} {:.2} ({sharpe_w} wins); \
                             trueDD — {name1} {:.1}% vs {namek} {:.1}% ({dd_w} wins).",
                            s1.sharpe, sk.sharpe, s1.true_max_dd_pct, sk.true_max_dd_pct
                        );
                    }
                } else if let (Some(s1), Some(sk)) = (rm1, rmk) {
                    let sharpe_winner = if sk.sharpe > s1.sharpe { *nk } else { *n1 };
                    let dd_winner = if sk.true_max_dd_pct < s1.true_max_dd_pct { *nk } else { *n1 };
                    let supported = sk.sharpe > s1.sharpe && sk.true_max_dd_pct < s1.true_max_dd_pct;
                    if supported {
                        println!(
                            "RISK VERDICT: hold-all (N={nk}) is the smoother ride on BOTH axes — \
                             Sharpe: N={n1} {:.2} vs N={nk} {:.2}; trueDD: N={n1} {:.1}% vs N={nk} {:.1}%.",
                            s1.sharpe, sk.sharpe, s1.true_max_dd_pct, sk.true_max_dd_pct
                        );
                        println!("              Intuition \"N>1 more robust though lower P&L\": SUPPORTED on this sample.");
                    } else if sharpe_winner == *n1 && dd_winner == *n1 {
                        println!(
                            "RISK VERDICT: single-slot (N={n1}) is the smoother ride on BOTH axes — \
                             Sharpe: N={n1} {:.2} vs N={nk} {:.2}; trueDD: N={n1} {:.1}% vs N={nk} {:.1}%.",
                            s1.sharpe, sk.sharpe, s1.true_max_dd_pct, sk.true_max_dd_pct
                        );
                        println!("              Intuition \"N>1 more robust though lower P&L\": NOT supported.");
                    } else {
                        // axes disagree
                        println!(
                            "RISK VERDICT: MIXED — N={sharpe_winner} wins Sharpe, N={dd_winner} wins drawdown. \
                             Sharpe: N={n1} {:.2} vs N={nk} {:.2}; trueDD: N={n1} {:.1}% vs N={nk} {:.1}%.",
                            s1.sharpe, sk.sharpe, s1.true_max_dd_pct, sk.true_max_dd_pct
                        );
                        println!(
                            "              Intuition \"N>1 more robust though lower P&L\": PARTIAL — \
                              N={nk} cut drawdown but did not improve risk-adjusted return."
                        );
                    }
                }
            }
            _ => println!("\nVERDICT: inconclusive — at least one endpoint had no robust config."),
        }
    } else {
        println!("Only one endpoint (N=1 == upper endpoint) — nothing to compare against.");
    }
    println!(
        "\nCaveat: one held-out slice (~{:.0}d) — suggestive, not proven. Risk metrics are on the\n\
         held-out test mark-to-market curve, annualized at the nominal 184 s/snapshot cadence;\n\
         crypto names co-move with SOL, so realized variance reduction may be modest.",
        span_days(test)
    );
    Ok(())
}

struct PerTokenTuneArgs<'a> {
    cfg: &'a PortfolioConfig,
    pool_usdc: Option<f64>,
    min_trades: usize,
    train_frac: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    apply: bool,
}

/// Merge per-token params into a tokens JSON file by mint, preserving all entries and
/// other fields. Reads the RAW (unfiltered) array so USDC/invalid entries aren't dropped.
fn write_token_params(
    path: &str,
    overrides: &std::collections::HashMap<String, momentum_universe::TokenParams>,
) -> Result<usize> {
    let data = std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    let mut toks: Vec<momentum_universe::WatchedToken> =
        serde_json::from_str(&data).with_context(|| format!("parsing {path}"))?;
    let mut n = 0;
    for t in toks.iter_mut() {
        if let Some(p) = overrides.get(&t.mint) {
            let mut merged = p.clone();
            // trade_usdc is operator-set — the tuner never produces it, so carry forward any
            // hand-set value rather than wiping it on a wholesale replace. (The tuner DOES own
            // min_metric/trail/max_run/regime_filter/exit_on_fade/reentry_cooldown, so those
            // are intentionally overwritten with the tuned decision, including None = global.)
            if merged.trade_usdc.is_none() {
                merged.trade_usdc = t.params.as_ref().and_then(|old| old.trade_usdc);
            }
            t.params = Some(merged);
            n += 1;
        }
    }
    let out = serde_json::to_string_pretty(&toks)? + "\n";
    std::fs::write(path, out).with_context(|| format!("writing {path}"))?;
    Ok(n)
}

/// Build the per-snapshot regime mask for a config's own regime params.
fn regime_mask_for(slice: &[history::PriceSnapshot], p: &ParamSet) -> Vec<bool> {
    match p.regime_mode {
        RegimeMode::Off => vec![true; slice.len()],
        RegimeMode::Level => sim::regime_mask(slice, p.regime_filter_obs),
        RegimeMode::Trend => sim::regime_mask_trend(slice, p.regime_filter_obs, p.regime_threshold),
    }
}

fn per_token_tune(a: PerTokenTuneArgs) -> Result<()> {
    let PerTokenTuneArgs {
        cfg, pool_usdc, min_trades, train_frac, tokens, history_override, max_step,
        regime_obs, regime_trend_obs, apply,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");
    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?.into_iter().collect();
    let snapshots = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snapshots.len() >= 200, "only {} snapshots — need more history", snapshots.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let k = watched.len();
    anyhow::ensure!(k >= 1, "no curated tokens");
    let pool = pool_usdc.unwrap_or(cfg.momentum_trade_usdc);
    anyhow::ensure!(pool > 0.0, "--pool-usdc must be > 0");
    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);
    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    let periods_per_year = 365.0 * 86_400.0 / 184.0;
    let no_f: [f64; 0] = [];
    let no_u: [usize; 0] = [];

    println!("Per-token tuning — pool ${pool}, K={k} tokens. Train ~{:.0}d / Test ~{:.0}d. min_trades={min_trades}",
        span_days(train), span_days(test));

    // ── Global grid → Arm A (N=1 best) and Arm B (N=K best) ──
    let global_grid = |n: usize, td: f64| -> Option<sim::SimResult> {
        let mut base = sim::base_params(cfg);
        base.trade_usdc = td;
        base.size_ceiling_usdc = td;
        base.reinvest_frac = 0.0;
        let rf = if n == 1 { vec![0.0_f64] } else { vec![0.0_f64] };
        // Pin metric+lookback to the .env (live) config — NOT a fresh metric sweep. The
        // per-token min_metric we emit must be in the SAME metric's units the live trader
        // ranks in (.env's MOMENTUM_RANK_METRIC), else a slope_r2-scale threshold (~100)
        // would be compared against a return-scale score (~0.01) and silently block every
        // entry. All arms + the per-token grid therefore share .env's metric/lookback and
        // sweep only trail×max_run×min_metric (also much smaller/faster than the full grid).
        let results = sim::run_grid_multi(
            train, test, &watched, &base,
            &[base.metric], &[base.lookback_obs], &sim::GRID_MAX_RUNS, &sim::GRID_TRAILS,
            &sim::GRID_MIN_QUANTILES, &rf, &regime_obs, &regime_trend_obs,
            &no_f, &no_f, &no_u, &no_f, &no_f, &no_f, &[0], n,
        );
        sim::best_robust_by_test(&results, min_trades).cloned()
    };
    let arm_a = global_grid(1, pool);
    let arm_b = global_grid(k, pool / k as f64);

    // The global single-name best (Arm A) fixes metric/lookback/regime for per-token tuning.
    let Some(ref ga) = arm_a else {
        println!("\nNo robust single-slot (N=1) config — cannot establish a global baseline. Stopping.");
        return Ok(());
    };
    println!("Global best (single-name, Arm A): metric={} lookback={} regime={}@{} min={:.4} trail={}% max_run={}",
        ga.params.metric, ga.params.lookback_obs, ga.params.regime_mode, ga.params.regime_filter_obs,
        ga.params.min_metric, ga.params.trail_pct, ga.params.max_run_pct);

    // ── Per-token tuning (metric/lookback = .env live config; regime off inside
    // tune_per_token) ── tune_base is sim::base_params(cfg) = the .env config, so the emitted
    // per-token min_metric is in .env's metric units (valid for the live trader). Since the
    // global grid above is now pinned to .env's metric/lookback, ga.params already carries
    // them too — but we read straight from .env to make the invariant explicit and robust.
    let mut tune_base = sim::base_params(cfg);
    tune_base.trade_usdc = pool / k as f64; // per-slot notional for isolated grids
    let per_token = sim::tune_per_token(train, test, &watched, &tune_base, min_trades,
        &regime_obs, &regime_trend_obs);

    println!("\nPer-token best {{min_metric, trail, max_run}} (single-name grid, isolated test P&L):");
    let mut overrides: std::collections::HashMap<String, momentum_universe::TokenParams> = Default::default();
    for pt in &per_token {
        match &pt.params {
            Some(p) => {
                let regime_label = if p.regime_filter == Some(false) { "exempt" } else { "gated" };
                // Secondary knobs only show when the tuner chose a non-default value.
                let fade_label = match p.exit_on_fade { Some(false) => " fade=off", Some(true) => " fade=on", None => "" };
                let cd_label = match p.reentry_cooldown_secs { Some(c) => format!(" cooldown={c}s"), None => String::new() };
                println!("  {:<6} min={:.4} trail={}% max_run={}  regime={:<6}{}{}  test {:+.2}",
                    pt.symbol, p.min_metric.unwrap(), p.trail_pct.unwrap(), p.max_run_pct.unwrap(),
                    regime_label, fade_label, cd_label, pt.test_pnl);
                overrides.insert(pt.mint.clone(), p.clone());
            }
            None => println!("  {:<6} (no robust single-name config → global fallback)", pt.symbol),
        }
    }

    // ── Arm C: hold-all with per-token overrides applied in-memory ──
    // Config = the .env (live) global as the fallback for non-overridden tokens; per-token
    // overrides win for tokens that have them. Using base_params (= .env) keeps Arm C in the
    // same metric/lookback the per-token min_metric was tuned in, and mirrors exactly what
    // the live multi-slot trader runs.
    let mut c_params = sim::base_params(cfg);
    c_params.trade_usdc = pool / k as f64;
    c_params.size_ceiling_usdc = c_params.trade_usdc;
    c_params.reinvest_frac = 0.0;
    let watched_c: Vec<momentum_universe::WatchedToken> = watched.iter().map(|w| {
        let mut w2 = w.clone();
        w2.params = overrides.get(&w.mint).cloned();
        w2
    }).collect();
    let stream_c = sim::ranked_stream(test, &watched_c, &c_params);
    let mask_c = regime_mask_for(test, &c_params);
    let (run_c, mtm_c) = sim::replay_multi_mtm(test, &watched_c, &stream_c, &c_params, &mask_c, k);
    let risk_c = sim::risk_metrics(&mtm_c, periods_per_year);

    // Risk for arms A and B (replay their best configs on test with MTM).
    let arm_risk = |best: &Option<sim::SimResult>, n: usize| -> Option<(f64, sim::RiskMetrics)> {
        best.as_ref().map(|r| {
            let stream = sim::ranked_stream(test, &watched, &r.params);
            let mask = regime_mask_for(test, &r.params);
            let (run, mtm) = sim::replay_multi_mtm(test, &watched, &stream, &r.params, &mask, n);
            (run.net_pnl(), sim::risk_metrics(&mtm, periods_per_year))
        })
    };
    let a = arm_risk(&arm_a, 1);
    let b = arm_risk(&arm_b, k);

    println!("\n3-arm validation (equal ${pool}, held-out test):");
    println!("  {:<28} {:>10} {:>8} {:>8}", "arm", "test P&L", "Sharpe", "trueDD");
    if let Some((pnl, rm)) = &a {
        println!("  {:<28} {:>+10.2} {:>8.2} {:>7.1}%", "A single-slot (global)", pnl, rm.sharpe, rm.true_max_dd_pct);
    }
    if let Some((pnl, rm)) = &b {
        println!("  {:<28} {:>+10.2} {:>8.2} {:>7.1}%", "B hold-all (global cfg)", pnl, rm.sharpe, rm.true_max_dd_pct);
    } else {
        println!("  {:<28} {:>10}", "B hold-all (global cfg)", "no robust");
    }
    println!("  {:<28} {:>+10.2} {:>8.2} {:>7.1}%", "C hold-all (per-token)", run_c.net_pnl(), risk_c.sharpe, risk_c.true_max_dd_pct);

    // ── Verdict (the SP3 gate): does per-token (C) beat single-slot (A)? ──
    if let Some((a_pnl, a_rm)) = &a {
        let c_pnl = run_c.net_pnl();
        let pnl_win = c_pnl > *a_pnl;
        let sharpe_win = risk_c.sharpe > a_rm.sharpe;
        let verdict = if pnl_win && sharpe_win {
            "SUPPORTED — per-token hold-all beats single-slot on BOTH P&L and Sharpe"
        } else if pnl_win || sharpe_win {
            "MIXED — per-token hold-all wins one of {P&L, Sharpe} vs single-slot"
        } else {
            "NOT SUPPORTED — single-slot still dominates per-token hold-all"
        };
        println!("\nVERDICT (SP3 gate): {verdict}.");
        println!("  C vs A — P&L {:+.2} vs {:+.2} (Δ {:+.2}); Sharpe {:.2} vs {:.2}; trueDD {:.1}% vs {:.1}%.",
            c_pnl, a_pnl, c_pnl - a_pnl, risk_c.sharpe, a_rm.sharpe, risk_c.true_max_dd_pct, a_rm.true_max_dd_pct);
    }
    println!("\nCaveat: one held-out slice; regime=exempt only for tokens that strictly outperform gated; crypto names co-move. Suggestive, not proven.");

    if apply {
        let n = write_token_params(&tokens_path, &overrides)?;
        println!("\n--apply: wrote per-token params for {n} token(s) to {tokens_path}.");
    } else {
        println!("\n(preview only — re-run with --apply to write per-token params into {tokens_path})");
    }
    Ok(())
}

// base_params lives in sim::base_params — call that directly.

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
    objective: Objective,
    regime_obs: Vec<usize>,
    regime_trend_obs: Vec<usize>,
    pair_cost_bps: u32,
    pair_funding_bps_day: f64,
    max_hold_min: u32,
    breakeven: bool,
    pair_entry_confirm_obs: Vec<usize>,
    pair_z_exits: Option<Vec<f64>>,
    atr_ks: Option<Vec<f64>>,
    sigma_ks: Option<Vec<f64>>,
    vol_obs: Option<Vec<usize>>,
    no_vol_stops: bool,
    max_trail_pcts: Option<Vec<f64>>,
    reinvest_fracs: Option<Vec<f64>>,
    size_ceilings: Option<Vec<f64>>,
    entry_max_z_obs: usize,
    entry_max_zs: Option<Vec<f64>>,
    confirm_ks: Vec<usize>,
    dump_trades: bool,
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
        objective,
        regime_obs,
        regime_trend_obs,
        pair_cost_bps,
        pair_funding_bps_day,
        max_hold_min,
        breakeven,
        pair_entry_confirm_obs,
        pair_z_exits,
        atr_ks,
        sigma_ks,
        vol_obs,
        no_vol_stops,
        max_trail_pcts,
        reinvest_fracs,
        size_ceilings,
        entry_max_z_obs,
        entry_max_zs,
        confirm_ks,
        dump_trades,
    } = a;
    anyhow::ensure!(
        train_frac > 0.0 && train_frac < 1.0,
        "--train-frac must be between 0 and 1 (got {train_frac})"
    );
    anyhow::ensure!(
        objective == Objective::NetPnl || matches!(strategy, StrategyArg::Momentum),
        "--objective pnl-per-hold is only supported for --strategy momentum; \
         this strategy's results don't carry hold-time — use --objective net-pnl"
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
            objective,
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
            entry_max_z_obs,
            entry_max_zs,
            confirm_ks,
            dump_trades,
        }),
        StrategyArg::Meanrev => meanrev_grid(MeanRevGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        }),
        StrategyArg::Pairs => pairs_grid(PairsGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
            pair_cost_bps, pair_funding_bps_day, pair_entry_confirm_obs, pair_z_exits,
        }),
        StrategyArg::Relval => relval_grid(PairsGrid {
            train, test, watched: &watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
            pair_cost_bps, pair_funding_bps_day, pair_entry_confirm_obs, pair_z_exits,
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
    objective: Objective,
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
    entry_max_z_obs: usize,
    entry_max_zs: Option<Vec<f64>>,
    confirm_ks: Vec<usize>,
    dump_trades: bool,
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
        objective,
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
        entry_max_z_obs,
        entry_max_zs,
        confirm_ks,
        dump_trades,
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
    // Overbought entry-gate sweep. Always includes `(0, 0.0)` (gate off) so the grid can
    // pick "no gate"; when a window is given, each --entry-max-zs threshold is swept over it.
    // Off by default ⇒ single `(0,0.0)` variant ⇒ grid size unchanged.
    let entry_max_z_variants: Vec<(usize, f64)> = {
        let mut v = vec![(0usize, 0.0f64)];
        if entry_max_z_obs > 0 {
            for z in entry_max_zs.unwrap_or_default() {
                v.push((entry_max_z_obs, z));
            }
        }
        v
    };
    // Multi-metric sign-confirmation sweep: dedupe/sort so `0,3,3` doesn't waste
    // replays; empty (CLI can't produce it, but callers can) → [0] = gate off.
    let confirm_ks: Vec<usize> = {
        let mut v = confirm_ks;
        if v.is_empty() {
            v.push(0);
        }
        v.sort_unstable();
        v.dedup();
        v
    };
    let stop_variant_count =
        sim::stop_variants(&trails, &atr_ks, &sigma_ks, &vol_obs_set, &max_trails).len();
    let sizing_count = sim::sizing_variants(1.0, &reinvest_fracs, &size_ceiling_mults).len();
    println!(
        "Strategy: MOMENTUM. Grid: {} metrics × {} lookbacks × {} max_runs × {} stop-variants ({} fixed + {} ATR-k×{} σ-k over {} vol-obs + {} max-trail) × {} thresholds × {} rotate-factors × {} regime-level-windows (+{} trend-windows×3 thr) × {} sizing × {} overbought-gate-variants × {} confirm-Ks.",
        metrics.len(), lookbacks.len(), max_runs.len(), stop_variant_count,
        trails.len(), atr_ks.len(), sigma_ks.len(), vol_obs_set.len(), max_trails.len(),
        quantiles.len(), rotate_factors.len(), regime_obs.len(), regime_trend_obs.len(), sizing_count,
        entry_max_z_variants.len(), confirm_ks.len(),
    );
    let mut base = sim::base_params(cfg);
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
        &confirm_ks,
        &entry_max_z_variants,
    );
    anyhow::ensure!(!results.is_empty(), "grid produced no results");

    let mut robust: Vec<&SimResult> = results.iter().filter(|r| r.is_robust(min_trades)).collect();
    robust.sort_by(|a, b| {
        dependability(b, objective)
            .partial_cmp(&dependability(a, objective))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    println!(
        "\n=== VERDICT: {}/{} configs ROBUST (profitable in train AND test, ≥{min_trades} trades each) ===",
        robust.len(), results.len()
    );
    if robust.is_empty() {
        println!("No robust edge in this sample. Showing best-by-test-P&L below — treat as overfit (not deployable).");
        print_table(&results, top, objective);
    } else {
        match objective {
            Objective::NetPnl => println!("Robust configs (sorted by worst-slice P&L — most dependable first):"),
            Objective::PnlPerHold => println!("Robust configs (sorted by worst-slice $/hour-deployed — most capital-efficient first):"),
        }
        let owned: Vec<SimResult> = robust.iter().map(|r| (*r).clone()).collect();
        print_table(&owned, top, objective);
        print_env_block(robust[0], objective);
        if objective == Objective::PnlPerHold {
            print_objective_comparison(&robust);
        }
        if dump_trades {
            // Replay the most-dependable winner's tradeable knobs (regime-off, single-slot —
            // exactly the params the optimizer writes to .env) and list every round-trip.
            let p = &robust[0].params;
            print_trades("TRAIN (winning config, regime-off single-slot replay)", &sim::replay(train, watched, p));
            print_trades("TEST (held-out) (winning config, regime-off single-slot replay)", &sim::replay(test, watched, p));
        }
    }
    write_csv(csv_path, &results)?;
    println!("\nFull grid ({} rows) written to {csv_path}", results.len());
    Ok(())
}

/// The robust-sort key: min over the two walk-forward slices, in the objective's
/// units — worst-slice net P&L (default) or worst-slice $/hour-deployed.
fn dependability(r: &SimResult, objective: Objective) -> f64 {
    match objective {
        Objective::NetPnl => r.net_pnl_train.min(r.net_pnl_test),
        Objective::PnlPerHold => r.rate_train().min(r.rate_test()),
    }
}

/// Pick one arm's winner among the robust set. NetPnl delegates to the historical
/// `best_robust_by_test` (highest held-out P&L — unchanged behavior); PnlPerHold picks
/// the best worst-slice $/h (the `dependability` key), so a test-only turnover fluke
/// can't represent the arm.
fn best_robust_by(results: &[SimResult], min_trades: usize, objective: Objective) -> Option<&SimResult> {
    match objective {
        Objective::NetPnl => sim::best_robust_by_test(results, min_trades),
        Objective::PnlPerHold => results
            .iter()
            .filter(|r| r.is_robust(min_trades))
            .max_by(|a, b| {
                dependability(a, objective)
                    .partial_cmp(&dependability(b, objective))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }),
    }
}

/// Head-to-head of the two objectives' winners among the SAME robust set, so a
/// pnl-per-hold run shows what it trades away vs the absolute-P&L pick. `robust`
/// arrives sorted by rate (PnlPerHold), so [0] is the efficiency winner.
fn print_objective_comparison(robust: &[&SimResult]) {
    let Some(&by_rate) = robust.first() else { return };
    let Some(&by_pnl) = robust.iter().max_by(|a, b| {
        dependability(a, Objective::NetPnl)
            .partial_cmp(&dependability(b, Objective::NetPnl))
            .unwrap_or(std::cmp::Ordering::Equal)
    }) else { return };

    println!("\n=== OBJECTIVE COMPARISON (same robust set) ===");
    let show = |label: &str, r: &SimResult| {
        println!("  {label}: {}", fmt_cfg(r));
        println!(
            "      pnl train {:+.2} / test {:+.2} USDC   hold train {:.1}h / test {:.1}h   rate train {:+.3} / test {:+.3} $/h   trades te {}",
            r.net_pnl_train, r.net_pnl_test,
            r.hold_hours_train, r.hold_hours_test,
            r.rate_train(), r.rate_test(),
            r.n_trades_test,
        );
    };
    show("best $/h   ", by_rate);
    if std::ptr::eq(by_rate, by_pnl) {
        println!("  best net-pnl: SAME config — efficiency and absolute P&L agree here.");
    } else {
        show("best net-pnl", by_pnl);
        let dpnl = by_rate.net_pnl_test - by_pnl.net_pnl_test;
        let dhold = by_pnl.hold_hours_test - by_rate.hold_hours_test;
        if dpnl >= 0.0 {
            println!(
                "  → $/h pick ALSO wins test P&L ({dpnl:+.2} USDC) and frees {dhold:.1}h of capital vs the net-pnl pick. \
                 (Re-run with --objective net-pnl --dump-trades for the other side's trade list.)"
            );
        } else {
            println!(
                "  → $/h pick gives up {dpnl:+.2} test USDC but frees {dhold:.1}h of capital vs the net-pnl pick. \
                 (Re-run with --objective net-pnl --dump-trades for the other side's trade list.)"
            );
        }
    }
}

/// Epoch seconds → compact "YYYY-MM-DD HH:MM" UTC (no chrono dependency leak elsewhere).
fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// `trades` subcommand: the trader's recorded round-trips (state-file audit trail)
/// annotated with the SOL trend-regime metric (slope_r2, as %/yr) at each entry.
fn live_trades_report(
    cfg: &PortfolioConfig,
    state: Option<String>,
    history_override: Option<String>,
    trend_obs: Option<usize>,
    max_step: f64,
) -> Result<()> {
    let state_path = state.unwrap_or_else(|| cfg.momentum_state_path.clone());
    let st = solana_mev::portfolio::momentum_state::load(Path::new(&state_path))
        .with_context(|| format!("loading {state_path}"))?;
    anyhow::ensure!(!st.trades.is_empty(), "no closed trades recorded in {state_path}");

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?
        .into_iter()
        .collect();
    let snaps = sim::sanitize_history(&raw, max_step);
    let obs = trend_obs
        .or((cfg.momentum_regime_obs > 0).then_some(cfg.momentum_regime_obs))
        .unwrap_or(480);
    let series = sim::sol_slope_r2_series_ts(&snaps, obs);
    let gate = (cfg.momentum_regime_mode == RegimeMode::Trend)
        .then_some(cfg.momentum_regime_trend_min);

    println!(
        "=== {} round-trips from {state_path} — SOL trend = slope_r2 over {obs} obs, annualized %/yr ===",
        st.trades.len()
    );
    if let Some(min) = gate {
        println!("    live trend gate: MOMENTUM_REGIME_TREND_MIN = {:.2} ({:+.0}%/yr); ✓/✗ = entry passes it", min, min * 100.0);
    }
    println!(
        "  {:>3}  {:<6} {:<5} {:<16} {:<16} {:>7}  {:>8} {:>9}  {:>9} {:>8}  {:>13}",
        "#", "token", "mode", "entry (UTC)", "exit (UTC)", "hold", "in$", "out$", "pnl$", "pnl%", "trend@entry"
    );
    for (i, t) in st.trades.iter().enumerate() {
        let pnl = t.usdc_out - t.usdc_in;
        let hold_h = (t.exit_ts - t.entry_ts) as f64 / 3600.0;
        let trend = sim::slope_r2_at(&series, t.entry_ts);
        let trend_col = match trend {
            Some(v) => {
                let mark = match gate {
                    Some(min) => if v >= min { " ✓" } else { " ✗" },
                    None => "",
                };
                format!("{:+8.0}%/yr{mark}", v * 100.0)
            }
            None => "  (no data)".to_string(),
        };
        println!(
            "  {:>3}  {:<6} {:<5} {:<16} {:<16} {:>6.1}h  {:>8.2} {:>9.2}  {:>+9.2} {:>+7.1}%  {trend_col}",
            i + 1, t.symbol, if t.dry_run { "paper" } else { "live" },
            fmt_ts(t.entry_ts), fmt_ts(t.exit_ts), hold_h, t.usdc_in, t.usdc_out, pnl, t.pnl_pct
        );
    }
    let net: f64 = st.trades.iter().map(|t| t.usdc_out - t.usdc_in).sum();
    let wins = st.trades.iter().filter(|t| t.usdc_out >= t.usdc_in).count();
    println!(
        "  Totals: {} trades, net {net:+.2} USDC, win {:.0}%",
        st.trades.len(),
        100.0 * wins as f64 / st.trades.len() as f64
    );
    Ok(())
}

/// `oracle` subcommand: perfect-foresight profit ceiling (per token + achievable
/// single-slot), capture ratio of the live .env config, and metric distributions
/// at oracle entries. Diagnostic — never a tuning target (labels are future-peeked).
fn oracle_report(
    cfg: &PortfolioConfig,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
    slippage_override: Option<u32>,
    min_hold_min: u32,
    show: usize,
) -> Result<()> {
    use solana_mev::portfolio::momentum::est_gas_usdc;
    use solana_mev::portfolio::oracle::{oracle_trades, single_slot_schedule, OracleCosts, OracleTrade};

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?
        .into_iter()
        .collect();
    let snaps = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snaps.len() >= 200, "only {} snapshots — need more history", snaps.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let span_days = (snaps.last().unwrap().ts - snaps.first().unwrap().ts) as f64 / 86_400.0;

    // Flat gas per swap from the sample's median SOL price (same estimator the
    // replay charges per swap; a flat median keeps the DP price-only).
    let mut sol_px: Vec<f64> = snaps.iter().filter_map(|s| s.prices.get("SOL").copied().filter(|p| *p > 0.0)).collect();
    sol_px.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let sol_median = sol_px.get(sol_px.len() / 2).copied().unwrap_or(0.0);
    let costs = OracleCosts {
        trade_usdc: cfg.momentum_trade_usdc,
        slippage_bps: slippage_override.unwrap_or(cfg.momentum_slippage_bps),
        gas_usdc: est_gas_usdc(sol_median),
    };
    println!(
        "=== ORACLE (perfect foresight, replay cost model) — {} snapshots / {span_days:.1} days ===",
        snaps.len()
    );
    println!(
        "    costs: {} USDC notional, {} bps/side slippage, ${:.4} gas/swap (median SOL ${:.0}), min hold {}min",
        costs.trade_usdc, costs.slippage_bps, costs.gas_usdc, sol_median, min_hold_min
    );

    // Per-token exact DP (rayon: O(N²) each).
    let per_token: Vec<(String, Vec<OracleTrade>)> = watched
        .par_iter()
        .map(|w| {
            let series: Vec<(usize, i64, f64)> = snaps
                .iter()
                .enumerate()
                .filter_map(|(gi, s)| {
                    s.prices.get(&w.mint).copied().filter(|p| *p > 0.0).map(|p| (gi, s.ts as i64, p))
                })
                .collect();
            let pxs: Vec<(i64, f64)> = series.iter().map(|&(_, ts, p)| (ts, p)).collect();
            let trades = oracle_trades(&pxs, &costs, min_hold_min as i64 * 60)
                .into_iter()
                .map(|(e, x, pnl)| OracleTrade {
                    symbol: w.symbol.clone(),
                    mint: w.mint.clone(),
                    entry_i: series[e].0,
                    exit_i: series[x].0,
                    entry_ts: series[e].1,
                    exit_ts: series[x].1,
                    entry_px: series[e].2,
                    exit_px: series[x].2,
                    pnl_usdc: pnl,
                })
                .collect();
            (w.symbol.clone(), trades)
        })
        .collect();

    println!("\n  Per-token ceiling (unconstrained: each token alone, single slot per token):");
    println!("  {:<8} {:>7} {:>10} {:>9} {:>9} {:>10}", "token", "trades", "pnl$", "med$/tr", "med hold", "in-mkt");
    for (sym, trades) in &per_token {
        if trades.is_empty() {
            println!("  {sym:<8} {:>7} {:>10} {:>9} {:>9} {:>10}", 0, "0.00", "-", "-", "-");
            continue;
        }
        let total: f64 = trades.iter().map(|t| t.pnl_usdc).sum();
        let mut pnls: Vec<f64> = trades.iter().map(|t| t.pnl_usdc).collect();
        pnls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut holds: Vec<f64> = trades.iter().map(|t| (t.exit_ts - t.entry_ts) as f64 / 3600.0).collect();
        holds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let in_mkt: f64 = holds.iter().sum();
        println!(
            "  {sym:<8} {:>7} {:>10.2} {:>9.2} {:>8.1}h {:>9.1}h",
            trades.len(), total, pnls[pnls.len() / 2], holds[holds.len() / 2], in_mkt
        );
    }

    // Achievable single-slot schedule across all tokens.
    let all: Vec<OracleTrade> = per_token.into_iter().flat_map(|(_, t)| t).collect();
    let schedule = single_slot_schedule(&all);
    let ceiling: f64 = schedule.iter().map(|t| t.pnl_usdc).sum();
    let in_mkt: f64 = schedule.iter().map(|t| (t.exit_ts - t.entry_ts) as f64 / 3600.0).sum();
    println!(
        "\n  Achievable single-slot schedule: {} trades, +{ceiling:.2} USDC over {span_days:.1} days, in-market {in_mkt:.0}h ({:.0}%)",
        schedule.len(), 100.0 * in_mkt / (span_days * 24.0)
    );
    let mut biggest: Vec<&OracleTrade> = schedule.iter().collect();
    biggest.sort_by(|a, b| b.pnl_usdc.partial_cmp(&a.pnl_usdc).unwrap_or(std::cmp::Ordering::Equal));
    println!("  {} largest:", show.min(biggest.len()));
    for t in biggest.iter().take(show) {
        println!(
            "    {:<6} {} → {}  {:>6.1}h  {:>+8.2}$  ({:.4} → {:.4})",
            t.symbol, fmt_ts(t.entry_ts), fmt_ts(t.exit_ts),
            (t.exit_ts - t.entry_ts) as f64 / 3600.0, t.pnl_usdc, t.entry_px, t.exit_px
        );
    }

    // Capture: what the live .env config extracts of that ceiling on the same span.
    let mut p = sim::base_params(cfg);
    p.regime_mode = cfg.momentum_regime_mode;
    p.regime_filter_obs = cfg.momentum_regime_obs;
    p.regime_threshold = cfg.momentum_regime_trend_min;
    let run = sim::replay(&snaps, &watched, &p);
    let strat_pnl = run.net_pnl();
    println!(
        "\n  Live-config replay (metric={} min={:.4} trail={}% lookback={} regime={}@{}): {} trades, {strat_pnl:+.2} USDC",
        p.metric, p.min_metric, p.trail_pct, p.lookback_obs, p.regime_mode, p.regime_filter_obs, run.n_trades()
    );
    if ceiling > 0.0 {
        println!("  → capture ratio: {:.1}% of the achievable single-slot ceiling", 100.0 * strat_pnl / ceiling);
    }

    // Feature diagnosis: the causal observables at oracle-entry snapshots vs overall.
    // Read metrics from the production candidate stream (same lookback the live
    // trader uses) — if a metric separates the two populations, it earns a grid
    // dimension; thresholds still go through run's walk-forward gate.
    let stream = sim::ranked_stream(&snaps, &watched, &p);
    let mut at_entry: Vec<solana_mev::portfolio::suggestions::Metrics> = Vec::new();
    let mut unrankable = 0usize;
    for t in &schedule {
        match stream[t.entry_i].iter().find(|c| c.mint == t.mint) {
            Some(c) => at_entry.push(c.metrics),
            None => unrankable += 1,
        }
    }
    let overall: Vec<solana_mev::portfolio::suggestions::Metrics> =
        stream.iter().flat_map(|row| row.iter().map(|c| c.metrics)).collect();
    let med = |mut v: Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if v.is_empty() { f64::NAN } else { v[v.len() / 2] }
    };
    let pos = |v: &[f64]| if v.is_empty() { f64::NAN } else { 100.0 * v.iter().filter(|&&x| x > 0.0).count() as f64 / v.len() as f64 };
    println!(
        "\n  Observables at oracle entries (lookback={} obs; {} of {} entries rankable, {} pre-warm-up/unrankable):",
        p.lookback_obs, at_entry.len(), schedule.len(), unrankable
    );
    println!("  {:<10} {:>13} {:>13} {:>10} {:>10}", "metric", "med@entry", "med overall", ">0@entry", ">0 overall");
    let fields: [(&str, fn(&solana_mev::portfolio::suggestions::Metrics) -> f64); 4] = [
        ("sortino", |m| m.sortino),
        ("sharpe", |m| m.sharpe),
        ("slope_r2", |m| m.slope_r2),
        ("return", |m| m.ret),
    ];
    for (name, f) in fields {
        let e: Vec<f64> = at_entry.iter().map(f).collect();
        let o: Vec<f64> = overall.iter().map(f).collect();
        println!(
            "  {name:<10} {:>13.4} {:>13.4} {:>9.0}% {:>9.0}%",
            med(e.clone()), med(o.clone()), pos(&e), pos(&o)
        );
    }
    println!("\n  NOTE: oracle labels are future-peeked — a ceiling and a diagnosis, not a target.");
    println!("  Sample is ~{span_days:.0} days ≈ one regime; validate any hypothesis via `run`'s walk-forward gate.");
    Ok(())
}

/// List each round-trip trade of one slice: entry/exit time, token, prices, USDC in/out, P&L.
/// The label carries the replay context (which config, regime on/off) — set it at the call site.
fn print_trades(label: &str, run: &sim::SimRun) {
    println!("\n=== TRADES — {label} ===");
    if run.trades.is_empty() {
        println!("  (no trades in this slice)");
        return;
    }
    println!(
        "  {:>3}  {:<6} {:<16} {:<16} {:>7}  {:>8} {:>9}  {:>9} {:>8}",
        "#", "token", "entry (UTC)", "exit (UTC)", "hold", "in$", "out$", "pnl$", "pnl%"
    );
    let (mut net, mut wins) = (0.0_f64, 0usize);
    for (i, t) in run.trades.iter().enumerate() {
        let pnl = t.usdc_out - t.usdc_in;
        net += pnl;
        if pnl >= 0.0 { wins += 1; }
        let hold_h = (t.exit_ts - t.entry_ts) as f64 / 3600.0;
        println!(
            "  {:>3}  {:<6} {:<16} {:<16} {:>6.1}h  {:>8.2} {:>9.2}  {:>+9.2} {:>+7.1}%",
            i + 1, t.symbol, fmt_ts(t.entry_ts), fmt_ts(t.exit_ts), hold_h,
            t.usdc_in, t.usdc_out, pnl, t.pnl_pct
        );
    }
    let n = run.trades.len();
    let hold_h = run.total_hold_hours();
    let rate = if hold_h > 0.0 { net / hold_h } else { 0.0 };
    println!(
        "  Totals: {n} trades, net {net:+.2} USDC, win {:.0}%, in-market {hold_h:.1}h ({rate:+.3} $/h)",
        100.0 * wins as f64 / n as f64
    );
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

fn print_table(results: &[SimResult], top: usize, objective: Objective) {
    // Under pnl-per-hold, re-rank whatever the caller passes by worst-slice rate (the
    // non-robust fallback arrives sorted by test P&L). Under net-pnl the caller's
    // order is preserved — today's output, unchanged.
    let resorted: Vec<SimResult>;
    let results: &[SimResult] = if objective == Objective::PnlPerHold {
        let mut v = results.to_vec();
        v.sort_by(|a, b| {
            dependability(b, objective)
                .partial_cmp(&dependability(a, objective))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        resorted = v;
        &resorted
    } else {
        results
    };
    println!(
        "\n{:<8} {:>10} {:>6} {:>9} {:>8} {:>8} {:>9} {:>7} {:>11} {:>11} {:>8} {:>7} {:>7} {:>7} {:>7}",
        "metric", "min", "trail", "lookback", "maxrun", "rotate", "regime", "confirm", "pnl_test", "pnl_train", "$/h_te", "hold_te", "trades", "win%", "mtmDD%",
    );
    println!("{}", "─".repeat(138));
    for r in results.iter().take(top) {
        let p = &r.params;
        println!(
            "{:<8} {:>10.4} {:>5.1}% {:>9} {:>7.1}% {:>8.3} {:>9} {:>7} {:>+11.2} {:>+11.2} {:>+8.3} {:>6.1}h {:>7} {:>6.0}% {:>6.1}%",
            p.metric.to_string(),
            p.min_metric,
            p.trail_pct,
            p.lookback_obs,
            p.max_run_pct,
            p.rotate_margin,
            regime_desc(p),
            if p.confirm_k > 0 { format!("K={}", p.confirm_k) } else { "off".to_string() },
            r.net_pnl_test,
            r.net_pnl_train,
            r.rate_test(),
            r.hold_hours_test,
            r.n_trades_test,
            r.win_rate_test,
            r.true_max_dd_test,
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
    pair_z_exits: Option<Vec<f64>>,
}

fn pairs_grid(g: PairsGrid) -> Result<()> {
    let PairsGrid {
        train, test, watched, cfg, quick, top, csv_path, lookbacks_override, min_trades,
        pair_cost_bps, pair_funding_bps_day, pair_entry_confirm_obs, pair_z_exits,
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
    let (z_entries, mut z_exits, z_stops) = if quick {
        (vec![2.0, 2.5], vec![0.5], vec![4.0])
    } else {
        (vec![2.0, 2.5, 3.0], vec![0.0, 0.5], vec![3.5, 4.5])
    };
    if let Some(v) = pair_z_exits {
        if !v.is_empty() {
            anyhow::ensure!(
                v.iter().all(|&x| x.is_finite() && x >= 0.0),
                "every --z-exits value must be finite and >= 0"
            );
            z_exits = v;
        }
    }
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
        "metric,min_metric,trail_pct,lookback_obs,max_run_pct,rotate_margin,regime_mode,regime_filter_obs,regime_threshold,vol_stop_mode,vol_k,vol_obs,max_trail_pct,reinvest_frac,size_ceiling_usdc,entry_max_z_obs,entry_max_z,confirm_k,pnl_std_test,pnl_std_train,net_pnl_test,net_pnl_train,n_trades_test,n_trades_train,win_rate_test,profit_dd_test,mtm_dd_test,hold_hours_test,hold_hours_train"
    )?;
    for r in results {
        let p = &r.params;
        writeln!(
            f,
            "{},{},{},{},{},{:.4},{},{},{:.2},{},{:.4},{},{},{:.4},{:.2},{},{:.2},{},{:.2},{:.2},{:.4},{:.4},{},{},{:.2},{:.2},{:.2},{:.2},{:.2}",
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
            p.entry_max_z_obs,
            p.entry_max_z,
            p.confirm_k,
            r.pnl_std_test,
            r.pnl_std_train,
            r.net_pnl_test,
            r.net_pnl_train,
            r.n_trades_test,
            r.n_trades_train,
            r.win_rate_test,
            r.max_dd_test,
            r.true_max_dd_test,
            r.hold_hours_test,
            r.hold_hours_train,
        )?;
    }
    Ok(())
}

fn print_env_block(best: &SimResult, objective: Objective) {
    let p = &best.params;
    match objective {
        Objective::NetPnl => println!(
            "\nBest by held-out net P&L ({:+.2} USDC test, {:+.2} train) — paste into .env:",
            best.net_pnl_test, best.net_pnl_train
        ),
        Objective::PnlPerHold => {
            println!(
                "\nBest by worst-slice $/hour-deployed ({:+.3} $/h test over {:.1}h, {:+.3} $/h train over {:.1}h; {:+.2} USDC test) — paste into .env:",
                best.rate_test(), best.hold_hours_test,
                best.rate_train(), best.hold_hours_train,
                best.net_pnl_test,
            );
            println!("  # Selected via: momentum-sim run --objective pnl-per-hold");
        }
    }
    if best.true_max_dd_test.is_finite() {
        println!(
            "  # honest max drawdown (mark-to-market, % of account equity): {:.1}%",
            best.true_max_dd_test
        );
    }
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
    if p.confirm_k > 0 {
        println!("  # MOMENTUM_CONFIRM_METRICS={}   # multi-metric sign confirm (entry needs ≥K of 4 metrics > 0) — NOT yet consumed by the live trader", p.confirm_k);
    }
    if p.entry_max_z_obs > 0 {
        println!("  MOMENTUM_ENTRY_MAX_Z_OBS={}   # overbought gate: skip entry when z over this window exceeds MOMENTUM_ENTRY_MAX_Z", p.entry_max_z_obs);
        println!("  MOMENTUM_ENTRY_MAX_Z={:.2}", p.entry_max_z);
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
