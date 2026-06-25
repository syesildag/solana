use super::pairs_config::{PairSpec, PairsConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairDecision {
    Hold,
    Open {
        long_mint: String,
        long_sym: String,
        short_mint: String,
        short_sym: String,
    },
    Close,
}

/// Mirrors `sim::replay_pairs` exactly so live behavior matches the backtest:
/// open only when the spread is stretched but not broken; z<0 ⇒ A cheap ⇒ long A.
pub fn pair_decision(z: f64, holding: bool, spec: &PairSpec, cfg: &PairsConfig) -> PairDecision {
    let (z_entry, z_exit, z_stop) = (spec.eff_z_entry(cfg), spec.eff_z_exit(cfg), spec.eff_z_stop(cfg));
    if holding {
        if z.abs() <= z_exit || z.abs() >= z_stop {
            PairDecision::Close
        } else {
            PairDecision::Hold
        }
    } else if z.abs() >= z_entry && z.abs() < z_stop {
        if z < 0.0 {
            PairDecision::Open {
                long_mint: spec.mint_a.clone(),
                long_sym: spec.symbol_a.clone(),
                short_mint: spec.mint_b.clone(),
                short_sym: spec.symbol_b.clone(),
            }
        } else {
            PairDecision::Open {
                long_mint: spec.mint_b.clone(),
                long_sym: spec.symbol_b.clone(),
                short_mint: spec.mint_a.clone(),
                short_sym: spec.symbol_a.clone(),
            }
        }
    } else {
        PairDecision::Hold
    }
}

/// Kamino-style health: collateral×liq_threshold ÷ debt. ∞ when there is no debt.
pub fn estimate_health_factor(collateral_usd: f64, debt_usd: f64, liq_threshold: f64) -> f64 {
    if debt_usd <= 0.0 {
        return f64::INFINITY;
    }
    collateral_usd * liq_threshold / debt_usd
}

pub fn borrow_apy_ok(borrow_apy_pct: f64, cfg: &PairsConfig) -> bool {
    borrow_apy_pct <= cfg.max_borrow_apy_pct
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::pairs_config::{PairSpec, PairsConfig};

    fn spec() -> PairSpec {
        PairSpec {
            symbol_a: "A".into(),
            mint_a: "MA".into(),
            symbol_b: "B".into(),
            mint_b: "MB".into(),
            lookback_obs: None,
            z_entry: None,
            z_exit: None,
            z_stop: None,
            entry_confirm_obs: None,
        }
    }

    fn cfg() -> PairsConfig {
        PairsConfig::test_default()
    }

    #[test]
    fn opens_long_a_when_a_is_cheap() {
        match pair_decision(-2.5, false, &spec(), &cfg()) {
            PairDecision::Open {
                long_mint,
                short_mint,
                ..
            } => assert_eq!((long_mint.as_str(), short_mint.as_str()), ("MA", "MB")),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn opens_long_b_when_b_is_cheap() {
        match pair_decision(2.5, false, &spec(), &cfg()) {
            PairDecision::Open {
                long_mint,
                short_mint,
                ..
            } => assert_eq!((long_mint.as_str(), short_mint.as_str()), ("MB", "MA")),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn per_pair_overrides_beat_the_global_default() {
        // Global entry is 2.0, but this pair overrides to 3.0 → z=-2.5 must NOT open.
        let s = PairSpec { z_entry: Some(3.0), ..spec() };
        assert!(
            matches!(pair_decision(-2.5, false, &s, &cfg()), PairDecision::Hold),
            "z=-2.5 is below the pair's own 3.0 entry"
        );
        // …and z=-3.2 clears the override (still under the global 4.5 stop) → opens.
        assert!(
            matches!(pair_decision(-3.2, false, &s, &cfg()), PairDecision::Open { .. }),
            "z=-3.2 clears the pair's 3.0 entry"
        );
    }

    #[test]
    fn holds_when_spread_not_stretched_or_already_broken() {
        assert!(
            matches!(pair_decision(1.0, false, &spec(), &cfg()), PairDecision::Hold),
            "below entry"
        );
        assert!(
            matches!(pair_decision(5.0, false, &spec(), &cfg()), PairDecision::Hold),
            "past stop, never open"
        );
    }

    #[test]
    fn closes_on_reversion_or_stop_while_holding() {
        assert!(
            matches!(pair_decision(0.3, true, &spec(), &cfg()), PairDecision::Close),
            "reverted"
        );
        assert!(
            matches!(pair_decision(4.6, true, &spec(), &cfg()), PairDecision::Close),
            "stopped"
        );
        assert!(
            matches!(pair_decision(2.0, true, &spec(), &cfg()), PairDecision::Hold),
            "still on"
        );
    }

    #[test]
    fn health_factor_and_borrow_gate() {
        assert!((estimate_health_factor(150.0, 50.0, 0.8) - 2.4).abs() < 1e-9);
        assert_eq!(estimate_health_factor(150.0, 0.0, 0.8), f64::INFINITY);
        assert!(borrow_apy_ok(25.0, &cfg()));
        assert!(!borrow_apy_ok(35.0, &cfg()));
    }
}
