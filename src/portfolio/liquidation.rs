//! Kamino liquidation **detection** bot (Phase A — paper only, no submission).
//!
//! Each scan: pull near-/at-liquidation obligations from the klend sidecar, pick the
//! repay+seize legs, quote the **real** seize→USDC impact on Jupiter (the size-dependent
//! number a flat backtest cost can't see), compute net profit, and log `Detected` /
//! `SkipUnprofitable` to the JSONL audit trail. Nothing is submitted. Phase B adds the
//! flash-loan + liquidate-ix + Jito-submit path.

use std::collections::HashMap;

use anyhow::Result;
use tracing::{info, warn};

use super::kamino::KlendClient;
use super::liquidation_actions::{self, LiquidationAction, LiquidationActionKind};
use super::liquidation_config::{LiquidationConfig, USDC_MINT};
use super::liquidation_signal::{choose_legs, liquidation_profit};
use super::liquidation_state::{self, DetectionRecord};
use super::momentum::est_gas_usdc;
use super::jupiter;

/// Don't re-log a `Detected` for the same obligation more often than this (seconds).
const DETECT_THROTTLE_SECS: i64 = 3600;
/// Slippage tolerance passed to the impact quote — high so the quote always returns; we only
/// read `priceImpactPct`, we don't enforce this.
const QUOTE_SLIPPAGE_BPS: u32 = 1000;

fn audit(cfg: &LiquidationConfig, ts: i64, kind: LiquidationActionKind) {
    if cfg.actions_path.is_empty() {
        return;
    }
    if let Err(e) = liquidation_actions::append(std::path::Path::new(&cfg.actions_path), &LiquidationAction { ts, kind }) {
        warn!("liquidation: audit append failed: {e}");
    }
}

/// One detection tick (paper). Paces itself to `scan_every_secs`; the watcher may call it
/// every 60s but the heavy bulk scan only runs on schedule.
pub async fn tick(cfg: &LiquidationConfig, port_cfg: &super::PortfolioConfig, prices: &HashMap<String, f64>, http: &reqwest::Client) -> Result<()> {
    if !cfg.enable {
        return Ok(());
    }
    let state_path = std::path::Path::new(&cfg.state_path);
    let mut state = liquidation_state::load(state_path)?;
    let now = chrono::Utc::now().timestamp();
    if now - state.last_scan_ts < cfg.scan_every_secs {
        return Ok(()); // not due yet
    }
    state.last_scan_ts = now;

    let obligations = match KlendClient::new(cfg.klend_sidecar_url.as_str()).liquidatable(cfg.scan_max_hf).await {
        Ok(v) => v,
        Err(e) => {
            warn!("liquidation: scan failed — {e}");
            audit(cfg, now, LiquidationActionKind::ScanFailed { reason: e.to_string() });
            liquidation_state::save(state_path, &state)?;
            return Ok(());
        }
    };

    let gas_usd = est_gas_usdc(prices.get("SOL").copied().unwrap_or(0.0));
    let mut profitable = 0usize;
    // New (non-throttled) profitable detections this scan — emailed as one summary below.
    let mut new_alerts: Vec<String> = Vec::new();

    for ob in &obligations {
        let Some(legs) = choose_legs(&ob.deposits, &ob.borrows, cfg.close_factor, cfg.liq_bonus_pct) else {
            continue;
        };
        if legs.repay_usd <= 0.0 || legs.seize_leg_usd <= 0.0 {
            continue;
        }
        // Raw base-unit size of the collateral slice we'd seize, for the live impact quote.
        let seize_raw = (legs.seize_leg_raw * (legs.seize_usd / legs.seize_leg_usd)).round();
        if !seize_raw.is_finite() || seize_raw < 1.0 {
            continue;
        }
        let impact_bps = match jupiter::quote(http, &cfg.jupiter_api_url, &legs.seize_mint, USDC_MINT, seize_raw as u64, QUOTE_SLIPPAGE_BPS).await {
            Ok(q) => jupiter::price_impact_bps(&q),
            Err(e) => {
                audit(cfg, now, LiquidationActionKind::SkipUnprofitable {
                    obligation: ob.address.clone(), seize_sym: legs.seize_sym.clone(),
                    seize_impact_bps: 0, est_net_usd: 0.0,
                    reason: format!("seize→USDC quote failed: {e}"),
                });
                continue;
            }
        };
        let eval = liquidation_profit(legs.repay_usd, cfg.liq_bonus_pct, impact_bps, cfg.flash_fee_bps, gas_usd, cfg.min_profit_usd);
        if eval.profitable {
            profitable += 1;
            let throttled = state.last_detected_ts.get(&ob.address).is_some_and(|&t| now - t < DETECT_THROTTLE_SECS);
            if !throttled {
                state.last_detected_ts.insert(ob.address.clone(), now);
                state.detections.push(DetectionRecord {
                    ts: now, obligation: ob.address.clone(), owner: ob.owner.clone(), health_factor: ob.health_factor,
                    repay_sym: legs.repay_sym.clone(), repay_usd: legs.repay_usd, seize_sym: legs.seize_sym.clone(),
                    seize_impact_bps: impact_bps, est_net_usd: eval.net_usd,
                });
                new_alerts.push(format!(
                    "  {} hf={:.3} repay {:.0} {} → seize {:.0} {} (impact {}bps) net {:+.2} USDC",
                    &ob.address[..8.min(ob.address.len())], ob.health_factor, legs.repay_usd, legs.repay_sym,
                    legs.seize_usd, legs.seize_sym, impact_bps, eval.net_usd,
                ));
                info!("liquidation(paper): {} hf={:.3} repay {:.0} {} → seize {:.0} {} (impact {}bps) net {:+.2} USDC",
                    &ob.address[..8.min(ob.address.len())], ob.health_factor, legs.repay_usd, legs.repay_sym,
                    legs.seize_usd, legs.seize_sym, impact_bps, eval.net_usd);
                audit(cfg, now, LiquidationActionKind::Detected {
                    obligation: ob.address.clone(), owner: ob.owner.clone(), health_factor: ob.health_factor,
                    repay_sym: legs.repay_sym.clone(), repay_usd: legs.repay_usd, seize_sym: legs.seize_sym.clone(),
                    seize_usd: legs.seize_usd, seize_impact_bps: impact_bps, est_net_usd: eval.net_usd,
                });
            }
        } else {
            audit(cfg, now, LiquidationActionKind::SkipUnprofitable {
                obligation: ob.address.clone(), seize_sym: legs.seize_sym.clone(), seize_impact_bps: impact_bps,
                est_net_usd: eval.net_usd,
                reason: format!("net {:+.2} < min {:.2} (bonus {:.1}% vs seize impact {}bps)", eval.net_usd, cfg.min_profit_usd, cfg.liq_bonus_pct, impact_bps),
            });
        }
    }

    info!("liquidation: scanned {} near-liq obligation(s) on {}, {} profitable",
        obligations.len(), &cfg.market[..8.min(cfg.market.len())], profitable);
    audit(cfg, now, LiquidationActionKind::Heartbeat { market: cfg.market.clone(), scanned: obligations.len(), profitable });

    // One summary email per scan listing the NEW profitable opportunities (fires in paper
    // too; gated only by SMTP config). Per-obligation throttling above keeps this quiet.
    if !new_alerts.is_empty() {
        let subject = format!("[PAPER] liquidation FOUND — {} new opportunity(ies) on Kamino", new_alerts.len());
        let body = format!(
            "Kamino liquidation detector found {} new profitable opportunity(ies) (paper — not executed):\n\n{}\n\nmarket: {}\n",
            new_alerts.len(), new_alerts.join("\n"), cfg.market,
        );
        if let Err(e) = super::emailer::send_alert(port_cfg, &subject, &body).await {
            warn!("liquidation: detection email failed: {e}");
        }
    }

    liquidation_state::save(state_path, &state)?;
    Ok(())
}
