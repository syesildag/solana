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
    // TODO(Phase 2b): paper P&L omits the short-leg borrow/funding cost. The backtest
    // (sim::replay_pairs) subtracts funding = notional × funding_bps_per_day × hold_days;
    // wire the live Kamino borrow APY here once 2b lands. Until then paper P&L is
    // directionally correct but optimistic vs the funded backtest.
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
/// `trade_usdc`, so health = (usdc_liq_thr + long_liq_thr) before slippage. A non-`Open`
/// decision passes trivially.
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

    // HOLDING: evaluate close.
    if let Some(pos) = state.position.clone() {
        let Some(spec) = spec_for(cfg, &pos.pair_key) else {
            tracing::warn!("pairs: held pair {} no longer in config — leaving position open, add it back or close manually", pos.pair_key);
            return Ok(());
        };
        if let Some(z) = live_spread_z(history, &spec, cfg.lookback_obs) {
            if matches!(pair_decision(z, true, &spec, cfg), PairDecision::Close) {
                let lpx = prices.get(&pos.long_mint).copied().unwrap_or(0.0);
                let spx = prices.get(&pos.short_mint).copied().unwrap_or(0.0);
                if lpx <= 0.0 || spx <= 0.0 {
                    tracing::warn!("pairs: skipping close of {} — missing price (lpx={lpx}, spx={spx}), will retry next tick", pos.pair_key);
                    return Ok(());
                }
                let sol = prices.get("SOL").copied().unwrap_or(0.0);
                let pnl = simulate_pair_pnl(&pos, lpx, spx, cfg.slippage_bps, sol);
                info!("pairs(paper): CLOSE {} z={z:.2} simulated pnl={pnl:+.4} USDC", pos.pair_key);
                state.trades.push(pairs_state::PairTradeRecord { pair_key: pos.pair_key.clone(),
                    entry_ts: pos.entry_ts, exit_ts: now, entry_z: pos.entry_z, exit_z: z, pnl_usdc: pnl, dry_run: true });
                state.last_close_ts_per_pair.insert(pos.pair_key.clone(), now);
                state.position = None;
                pairs_state::save(state_path, &state)?;
            }
        }
        return Ok(());
    }

    // FLAT: scan pairs, open the first whose signal fires + gates pass (paper).
    if pairs_state::trades_last_24h(&state, now) >= cfg.max_trades_per_day as usize { return Ok(()); }
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
        let PairDecision::Open { long_mint, long_sym, short_mint, short_sym } = &decision else { continue };
        let key = format!("{}/{}", spec.symbol_a, spec.symbol_b);
        if state.last_close_ts_per_pair.get(&key).is_some_and(|&t| now - t < cfg.reentry_cooldown_secs) { continue; }
        // Borrowability / APY / health gate (e.g. never short GOOGLx) when enabled.
        if let Some(res) = &reserves {
            match preflight_open(&decision, res, cfg.trade_usdc, cfg) {
                Preflight::Ok { borrow_apy_pct, health_factor } =>
                    info!("pairs: {key} preflight OK (borrow apy {borrow_apy_pct:.2}%, hf {health_factor:.2})"),
                reason => { info!("pairs: skip {key} — preflight {reason:?}"); continue; }
            }
        }
        let lpx = prices.get(long_mint.as_str()).copied().unwrap_or(0.0);
        let spx = prices.get(short_mint.as_str()).copied().unwrap_or(0.0);
        if lpx <= 0.0 || spx <= 0.0 { continue; }
        let pos = PairPosition { pair_key: key.clone(),
            long_mint: long_mint.clone(), long_sym: long_sym.clone(), long_amount: cfg.trade_usdc / lpx,
            short_mint: short_mint.clone(), short_sym: short_sym.clone(), short_amount: cfg.trade_usdc / spx,
            usdc_collateral: cfg.trade_usdc, entry_ts: now, entry_z: z,
            entry_long_px: lpx, entry_short_px: spx, dry_run: true };
        info!("pairs(paper): OPEN {key} z={z:.2} long {} short {}", pos.long_sym, pos.short_sym);
        state.position = Some(pos);
        pairs_state::save(state_path, &state)?;
        break;
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
            entry_long_px: 100.0, entry_short_px: 100.0, dry_run: true };
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
}
