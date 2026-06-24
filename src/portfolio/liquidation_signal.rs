//! Pure liquidation-profit model — no I/O, fully unit-tested. Mirrors the economic
//! reality the detection MVP measures: a liquidation repays `repay_usd` of a borrower's
//! debt and seizes collateral worth `repay_usd × (1 + liquidation_bonus)`, which must then
//! be sold back to the repay asset. The bot profits iff the bonus outweighs the
//! seize-collateral **sell impact** (size-dependent, quoted live) + flash fee + gas.
//!
//! This is why illiquid collateral matters: a 5% bonus easily covers a 1bps SPYx sell, but
//! a large position in a thin token (the AVGOx/GOOGLx lesson — sell impact in the hundreds
//! of bps at size) can wipe the bonus and turn a nominally-liquidatable obligation
//! net-negative.

use super::kamino::ObligationLeg;

/// The legs chosen for a candidate liquidation: which debt to repay, which collateral to
/// seize, and the dollar sizes (already bounded by close factor + available collateral).
#[derive(Debug, Clone, PartialEq)]
pub struct LegChoice {
    pub repay_sym: String,
    pub repay_mint: String,
    pub seize_sym: String,
    pub seize_mint: String,
    /// USD value of debt repaid (≤ close_factor × debt, and ≤ collateral ÷ (1+bonus)).
    pub repay_usd: f64,
    /// USD value of collateral seized = `repay_usd × (1 + bonus)`.
    pub seize_usd: f64,
    /// The chosen seize leg's full available collateral (USD) — for raw-amount scaling.
    pub seize_leg_usd: f64,
    pub seize_leg_raw: f64,
}

/// Pick the largest debt leg to repay and the largest collateral leg to seize, sizing the
/// repay to the close factor and bounding it so the seized value can't exceed available
/// collateral. Returns `None` if the obligation has no debt or no collateral.
pub fn choose_legs(
    deposits: &[ObligationLeg],
    borrows: &[ObligationLeg],
    close_factor: f64,
    liq_bonus_pct: f64,
) -> Option<LegChoice> {
    let max_by_usd = |legs: &[ObligationLeg]| {
        legs.iter()
            .filter(|l| l.amount_usd > 0.0)
            .max_by(|a, b| a.amount_usd.total_cmp(&b.amount_usd))
            .cloned()
    };
    let debt = max_by_usd(borrows)?;
    let coll = max_by_usd(deposits)?;
    let bonus = liq_bonus_pct / 100.0;
    let want_repay = debt.amount_usd * close_factor;
    let max_repay_by_collateral = coll.amount_usd / (1.0 + bonus);
    let repay_usd = want_repay.min(max_repay_by_collateral).max(0.0);
    Some(LegChoice {
        repay_sym: debt.symbol,
        repay_mint: debt.mint,
        seize_sym: coll.symbol,
        seize_mint: coll.mint,
        repay_usd,
        seize_usd: repay_usd * (1.0 + bonus),
        seize_leg_usd: coll.amount_usd,
        seize_leg_raw: coll.amount_raw,
    })
}

/// Outcome of evaluating one candidate liquidation.
#[derive(Debug, Clone, PartialEq)]
pub struct LiquidationEval {
    pub repay_usd: f64,
    pub seize_usd: f64,
    pub seize_impact_bps: u32,
    pub net_usd: f64,
    pub profitable: bool,
}

/// Net USD profit of a liquidation, given the live seize→repay-asset sell impact.
///
/// `seize_usd = repay_usd × (1 + bonus)`; you receive `seize_usd × (1 − impact)` after the
/// swap, pay back `repay_usd` (+ `flash_fee` on it in Phase B) and `gas_usd`. Profitable iff
/// the net clears `min_profit_usd`.
pub fn liquidation_profit(
    repay_usd: f64,
    liq_bonus_pct: f64,
    seize_impact_bps: u32,
    flash_fee_bps: u32,
    gas_usd: f64,
    min_profit_usd: f64,
) -> LiquidationEval {
    let bonus = liq_bonus_pct / 100.0;
    let impact = seize_impact_bps as f64 / 10_000.0;
    let flash = flash_fee_bps as f64 / 10_000.0;
    let seize_usd = repay_usd * (1.0 + bonus);
    let proceeds = seize_usd * (1.0 - impact);
    let net = proceeds - repay_usd - repay_usd * flash - gas_usd;
    LiquidationEval {
        repay_usd,
        seize_usd,
        seize_impact_bps,
        net_usd: net,
        profitable: net >= min_profit_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(sym: &str, usd: f64, raw: f64) -> ObligationLeg {
        ObligationLeg { symbol: sym.into(), mint: format!("m{sym}"), amount_usd: usd, amount_raw: raw }
    }

    #[test]
    fn choose_legs_picks_largest_debt_and_collateral_bounded_by_close_factor() {
        let deps = vec![leg("SPYx", 1000.0, 2_000_000.0), leg("QQQx", 300.0, 600_000.0)];
        let bors = vec![leg("USDC", 600.0, 600_000_000.0), leg("SPYx", 100.0, 200_000.0)];
        let c = choose_legs(&deps, &bors, 0.5, 5.0).expect("legs");
        assert_eq!((c.repay_sym.as_str(), c.seize_sym.as_str()), ("USDC", "SPYx"));
        // close factor 0.5 × 600 = 300 repay; seize = 300 × 1.05 = 315 (< 1000 collateral).
        assert!((c.repay_usd - 300.0).abs() < 1e-9, "repay {}", c.repay_usd);
        assert!((c.seize_usd - 315.0).abs() < 1e-9, "seize {}", c.seize_usd);
    }

    #[test]
    fn choose_legs_caps_repay_when_collateral_is_thin() {
        // Big debt, tiny collateral: repay is capped so seize ≤ available collateral.
        let deps = vec![leg("NVDAx", 105.0, 1.0)];
        let bors = vec![leg("USDC", 10_000.0, 1.0)];
        let c = choose_legs(&deps, &bors, 0.5, 5.0).unwrap();
        assert!((c.seize_usd - 105.0).abs() < 1e-6, "seize capped to collateral, got {}", c.seize_usd);
        assert!((c.repay_usd - 100.0).abs() < 1e-6, "repay = 105/1.05 = 100, got {}", c.repay_usd);
    }

    #[test]
    fn choose_legs_none_without_debt_or_collateral() {
        assert!(choose_legs(&[leg("SPYx", 100.0, 1.0)], &[], 0.5, 5.0).is_none());
        assert!(choose_legs(&[], &[leg("USDC", 100.0, 1.0)], 0.5, 5.0).is_none());
    }

    #[test]
    fn profit_positive_for_liquid_collateral() {
        // $1000 repay, 5% bonus, SPYx-like 2bps sell impact, no flash fee, $0.01 gas.
        let e = liquidation_profit(1000.0, 5.0, 2, 0, 0.01, 1.0);
        // seize 1050, proceeds 1050×0.9998 ≈ 1049.79, net ≈ 49.78
        assert!(e.profitable, "net {}", e.net_usd);
        assert!((e.net_usd - 49.78).abs() < 0.5, "net {}", e.net_usd);
    }

    #[test]
    fn profit_negative_when_seize_impact_exceeds_bonus() {
        // Large seize in a thin token: 5% bonus but 600bps (6%) sell impact → net-negative.
        let e = liquidation_profit(5000.0, 5.0, 600, 9, 0.05, 1.0);
        assert!(!e.profitable, "should be unprofitable, net {}", e.net_usd);
        assert!(e.net_usd < 0.0, "net {}", e.net_usd);
    }

    #[test]
    fn flash_fee_and_gas_reduce_net() {
        let no_fee = liquidation_profit(1000.0, 5.0, 10, 0, 0.0, 0.0).net_usd;
        let with_fee = liquidation_profit(1000.0, 5.0, 10, 9, 0.0, 0.0).net_usd;
        assert!(with_fee < no_fee, "flash fee must reduce net ({with_fee} < {no_fee})");
        assert!((no_fee - with_fee - 1000.0 * 9.0 / 10_000.0).abs() < 1e-9, "fee = 0.9 USDC");
    }
}
