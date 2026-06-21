use std::collections::{HashMap, VecDeque};
use anyhow::Result;
use tracing::info;

use super::history::PriceSnapshot;
use super::pairs_config::{PairSpec, PairsConfig};
use super::pairs_signal::{pair_decision, PairDecision};
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
    let slip = slippage_bps as f64 / 10_000.0;
    let long_pl = pos.long_amount * (long_px * (1.0 - slip) - pos.entry_long_px);
    let short_pl = pos.short_amount * (pos.entry_short_px - short_px * (1.0 + slip));
    long_pl + short_pl - 2.0 * est_gas_usdc(sol_px)
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
    for spec in &cfg.pairs {
        let Some(z) = live_spread_z(history, spec, cfg.lookback_obs) else { continue };
        if let PairDecision::Open { long_mint, long_sym, short_mint, short_sym } = pair_decision(z, false, spec, cfg) {
            let key = format!("{}/{}", spec.symbol_a, spec.symbol_b);
            if state.last_close_ts_per_pair.get(&key).is_some_and(|&t| now - t < cfg.reentry_cooldown_secs) { continue; }
            let lpx = prices.get(&long_mint).copied().unwrap_or(0.0);
            let spx = prices.get(&short_mint).copied().unwrap_or(0.0);
            if lpx <= 0.0 || spx <= 0.0 { continue; }
            let pos = PairPosition { pair_key: key.clone(),
                long_mint, long_sym, long_amount: cfg.trade_usdc / lpx,
                short_mint, short_sym, short_amount: cfg.trade_usdc / spx,
                usdc_collateral: cfg.trade_usdc, entry_ts: now, entry_z: z,
                entry_long_px: lpx, entry_short_px: spx, dry_run: true };
            info!("pairs(paper): OPEN {key} z={z:.2} long {} short {}", pos.long_sym, pos.short_sym);
            state.position = Some(pos);
            pairs_state::save(state_path, &state)?;
            break;
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
            entry_long_px: 100.0, entry_short_px: 100.0, dry_run: true };
        // long leg +~9.45 (110×0.995−100), short leg −0.5 (100−100×1.005) → net positive.
        let pnl = simulate_pair_pnl(&pos, 110.0, 100.0, 50, 150.0);
        assert!(pnl > 0.0, "convergence in our favor → profit, got {pnl}");
    }
}
