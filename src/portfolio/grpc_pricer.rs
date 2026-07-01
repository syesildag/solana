//! gRPC price feed for the momentum trader.
//!
//! This module provides an opt-in Yellowstone gRPC-based price update stream for the
//! momentum trader, complementing the existing Jupiter REST quote API. Configuration
//! is provided via `PortfolioConfig` fields: `momentum_grpc_pricing` (master switch),
//! `grpc_endpoint`, `grpc_token`, `pools_path` (pool metadata), and
//! `momentum_grpc_stale_secs` (staleness threshold).
//!
//! `WatchedToken` entries optionally carry `pool` (Raydium/Meteora/Orca pool pubkey)
//! and `quote` (quote token mint) for normalized pricing; these are populated by
//! later tasks.

// PoolState from dex::types is available at the binary level (main.rs).
// For test context, we provide a mock that matches the real PoolState API.
#[cfg(test)]
mod pool_state_for_tests {
    /// Test double for dex::types::PoolState, matching its exact enum structure.
    #[derive(Debug, Clone)]
    pub enum PoolState {
        ConstantProduct {
            reserve_a: u64,
            reserve_b: u64,
            fee_bps: u64,
        },
        ConcentratedLiquidity {
            sqrt_price_x64: u128,
            _liquidity: u128,
            fee_bps: u64,
        },
    }

    impl PoolState {
        pub fn rate_a_to_b(&self) -> f64 {
            match self {
                Self::ConstantProduct { reserve_a, reserve_b, fee_bps } => {
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    (*reserve_b as f64 / *reserve_a as f64) * fee
                }
                Self::ConcentratedLiquidity { sqrt_price_x64, fee_bps, .. } => {
                    let sqrt_price = *sqrt_price_x64 as f64 / (1u128 << 64) as f64;
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    sqrt_price * sqrt_price * fee
                }
            }
        }

        pub fn rate_b_to_a(&self) -> f64 {
            match self {
                Self::ConstantProduct { reserve_a, reserve_b, fee_bps } => {
                    if *reserve_b == 0 { return 0.0; }
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    (*reserve_a as f64 / *reserve_b as f64) * fee
                }
                Self::ConcentratedLiquidity { sqrt_price_x64, fee_bps, .. } => {
                    let sqrt_price = *sqrt_price_x64 as f64 / (1u128 << 64) as f64;
                    if sqrt_price == 0.0 { return 0.0; }
                    let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                    fee / (sqrt_price * sqrt_price)
                }
            }
        }
    }
}

#[cfg(test)]
use pool_state_for_tests::PoolState;

/// Trait for types that provide pool exchange rates.
/// Implemented by PoolState in both test and production contexts.
pub trait PoolRates {
    fn rate_a_to_b(&self) -> f64;
    fn rate_b_to_a(&self) -> f64;
}

#[cfg(test)]
impl PoolRates for PoolState {
    fn rate_a_to_b(&self) -> f64 {
        PoolState::rate_a_to_b(self)
    }
    fn rate_b_to_a(&self) -> f64 {
        PoolState::rate_b_to_a(self)
    }
}

/// USD price of the momentum token from current pool state.
/// `rate_a_to_b`/`rate_b_to_a` are atomic-unit rates (quote-atomic per momentum-atomic),
/// so we convert to human units with 10^(dec_momentum - dec_quote), then to USD
/// (quote=USDC → identity; quote=SOL → × sol_usd). Returns None on degenerate state.
pub fn price_usd(
    state: &dyn PoolRates,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    sol_usd: f64,
) -> Option<f64> {
    let raw = if momentum_is_token_a { state.rate_a_to_b() } else { state.rate_b_to_a() };
    if !raw.is_finite() || raw <= 0.0 { return None; }
    let price_in_quote = raw * 10f64.powi(dec_momentum as i32 - dec_quote as i32);
    let usd = if quote_is_usdc { price_in_quote } else { price_in_quote * sol_usd };
    if usd.is_finite() && usd > 0.0 { Some(usd) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Constant-product, momentum=token_a, quote=token_b=USDC, equal decimals (6/6).
    // reserve_b/reserve_a = 200/100 = 2.0 (fee 0 for simplicity via fee_bps=0).
    #[test]
    fn cp_usdc_quote_equal_decimals() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 6, 6, true, 0.0).unwrap();
        assert!((p - 2.0).abs() < 1e-9);
    }

    // SOL quote: price_in_sol × sol_usd. reserveB/reserveA=2.0 SOL per token, SOL=$150 → $300.
    #[test]
    fn cp_sol_quote_applies_sol_usd() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 9, 9, false, 150.0).unwrap();
        assert!((p - 300.0).abs() < 1e-6);
    }

    // Decimal adjustment: momentum has 6 dp, quote(USDC) 6 dp already covered;
    // here momentum=token_a 9dp, quote=token_b 6dp → ×10^(9-6)=1000.
    #[test]
    fn decimal_adjustment_scales_price() {
        let s = PoolState::ConstantProduct { reserve_a: 100, reserve_b: 200, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 9, 6, true, 0.0).unwrap();
        assert!((p - 2000.0).abs() < 1e-6); // 2.0 × 10^3
    }

    // momentum=token_b path uses rate_b_to_a.
    #[test]
    fn momentum_is_token_b_uses_inverse_rate() {
        let s = PoolState::ConstantProduct { reserve_a: 200, reserve_b: 100, fee_bps: 0 };
        // momentum=token_b, quote=token_a=USDC, equal dp: rate_b_to_a = reserve_a/reserve_b = 2.0
        let p = price_usd(&s as &dyn PoolRates, false, 6, 6, true, 0.0).unwrap();
        assert!((p - 2.0).abs() < 1e-9);
    }

    // Degenerate input → None (not a panic, not a zero price).
    #[test]
    fn zero_reserves_returns_none() {
        let s = PoolState::ConstantProduct { reserve_a: 0, reserve_b: 200, fee_bps: 0 };
        assert!(price_usd(&s as &dyn PoolRates, true, 6, 6, true, 0.0).is_none());
    }

    #[test]
    fn cl_pool_uses_sqrt_price() {
        // sqrt_price_x64 = 2^64 → price = 1.0; equal dp, USDC quote → $1.0
        let s = PoolState::ConcentratedLiquidity { sqrt_price_x64: 1u128 << 64, _liquidity: 0, fee_bps: 0 };
        let p = price_usd(&s as &dyn PoolRates, true, 6, 6, true, 0.0).unwrap();
        assert!((p - 1.0).abs() < 1e-9);
    }
}
