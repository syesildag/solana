use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const RAYDIUM_AMM_V4_PROGRAM: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp4";
pub const RAYDIUM_CLMM_PROGRAM: &str = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
pub const ORCA_WHIRLPOOL_PROGRAM: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const METEORA_DAMM_PROGRAM: &str = "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DexKind {
    RaydiumAmmV4,
    RaydiumClmm,
    OrcaWhirlpool,
    MeteoraDamm,
}

impl DexKind {
    pub fn program_id(&self) -> Pubkey {
        match self {
            Self::RaydiumAmmV4 => Pubkey::from_str(RAYDIUM_AMM_V4_PROGRAM).unwrap(),
            Self::RaydiumClmm => Pubkey::from_str(RAYDIUM_CLMM_PROGRAM).unwrap(),
            Self::OrcaWhirlpool => Pubkey::from_str(ORCA_WHIRLPOOL_PROGRAM).unwrap(),
            Self::MeteoraDamm => Pubkey::from_str(METEORA_DAMM_PROGRAM).unwrap(),
        }
    }

    pub fn fee_bps(&self) -> u64 {
        match self {
            Self::RaydiumAmmV4 => 25,
            Self::RaydiumClmm => 0,    // per-pool, read from state
            Self::OrcaWhirlpool => 0,  // per-pool, read from state
            Self::MeteoraDamm => 0,    // dynamic, read from state
        }
    }
}

/// Parsed on-chain pool state. Constant-product pools only need reserve_a / reserve_b.
/// Concentrated liquidity pools also carry sqrt_price for approximate graph edges.
#[derive(Debug, Clone)]
pub enum PoolState {
    ConstantProduct {
        reserve_a: u64,
        reserve_b: u64,
        fee_bps: u64,
    },
    ConcentratedLiquidity {
        sqrt_price_x64: u128,
        liquidity: u128,
        fee_bps: u64,
    },
}

impl PoolState {
    /// Returns the approximate exchange rate: units of token_b per 1 unit of token_a.
    pub fn rate_a_to_b(&self) -> f64 {
        match self {
            Self::ConstantProduct { reserve_a, reserve_b, fee_bps } => {
                let fee = 1.0 - (*fee_bps as f64 / 10_000.0);
                (*reserve_b as f64 / *reserve_a as f64) * fee
            }
            Self::ConcentratedLiquidity { sqrt_price_x64, fee_bps, .. } => {
                // price = (sqrt_price_x64 / 2^64)^2
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

    /// Compute exact amount_out using constant-product formula.
    /// For CL pools this is approximate (single-tick, no tick crossing).
    pub fn get_amount_out(&self, amount_in: u64, a_to_b: bool) -> u64 {
        match self {
            Self::ConstantProduct { reserve_a, reserve_b, fee_bps } => {
                let (reserve_in, reserve_out) = if a_to_b {
                    (*reserve_a, *reserve_b)
                } else {
                    (*reserve_b, *reserve_a)
                };
                if reserve_in == 0 {
                    return 0;
                }
                // amount_out = reserve_out * amount_in * (10000 - fee) / (reserve_in * 10000 + amount_in * (10000 - fee))
                let numerator = (reserve_out as u128)
                    .saturating_mul(amount_in as u128)
                    .saturating_mul(10_000 - *fee_bps as u128);
                let denominator = (reserve_in as u128)
                    .saturating_mul(10_000)
                    .saturating_add((amount_in as u128).saturating_mul(10_000 - *fee_bps as u128));
                if denominator == 0 {
                    return 0;
                }
                (numerator / denominator) as u64
            }
            Self::ConcentratedLiquidity { sqrt_price_x64, liquidity, fee_bps } => {
                // Single-tick approximation using virtual reserves
                // virtual_reserve_a = liquidity / sqrt_price, virtual_reserve_b = liquidity * sqrt_price
                let sqrt = *sqrt_price_x64 as f64 / (1u128 << 64) as f64;
                let liq = *liquidity as f64;
                let (vr_in, vr_out) = if a_to_b {
                    (liq / sqrt, liq * sqrt)
                } else {
                    (liq * sqrt, liq / sqrt)
                };
                let fee = 10_000 - *fee_bps;
                let numerator = vr_out * amount_in as f64 * fee as f64;
                let denominator = vr_in * 10_000.0 + amount_in as f64 * fee as f64;
                if denominator == 0.0 {
                    return 0;
                }
                (numerator / denominator) as u64
            }
        }
    }
}

/// A single liquidity pool tracked by the bot.
#[derive(Debug)]
pub struct Pool {
    pub id: Pubkey,
    pub dex: DexKind,
    /// Mint of token A (for Raydium: coin_mint)
    pub token_a: Pubkey,
    /// Mint of token B (for Raydium: pc_mint)
    pub token_b: Pubkey,
    /// SPL token vault account holding token A reserves
    pub vault_a: Pubkey,
    /// SPL token vault account holding token B reserves
    pub vault_b: Pubkey,

    // Live state updated from gRPC stream
    pub reserve_a: AtomicU64,
    pub reserve_b: AtomicU64,
    /// Cached fee for CL pools (read from pool state, not vault)
    pub fee_bps: AtomicU64,
    /// Cached sqrt_price_x64 for CL pools
    pub sqrt_price_x64: AtomicU64,
    /// For CL pools: pool state account to subscribe to
    pub state_account: Option<Pubkey>,

    // Extra accounts needed to build swap instructions
    pub extra: PoolExtra,
}

impl Pool {
    pub fn snapshot_state(&self) -> PoolState {
        let fee = self.fee_bps.load(Ordering::Relaxed);
        match self.dex {
            DexKind::RaydiumAmmV4 | DexKind::MeteoraDamm => PoolState::ConstantProduct {
                reserve_a: self.reserve_a.load(Ordering::Relaxed),
                reserve_b: self.reserve_b.load(Ordering::Relaxed),
                fee_bps: if fee == 0 { self.dex.fee_bps() } else { fee },
            },
            DexKind::RaydiumClmm | DexKind::OrcaWhirlpool => {
                // Use the CP approximation with actual vault balances.
                // The true CL formula requires the liquidity L parameter (stored in pool state),
                // which we don't yet track persistently. Using vault reserves gives the correct
                // average price and avoids the overflow/phantom-profit bug that comes from
                // using reserve_a as a proxy for L (they differ by 1/sqrt_price).
                PoolState::ConstantProduct {
                    reserve_a: self.reserve_a.load(Ordering::Relaxed),
                    reserve_b: self.reserve_b.load(Ordering::Relaxed),
                    fee_bps: if fee == 0 { 30 } else { fee },
                }
            }
        }
    }
}

/// DEX-specific extra accounts required to build swap instructions.
#[derive(Debug, Clone, Default)]
pub struct PoolExtra {
    // Raydium AMM V4
    pub amm_authority: Option<Pubkey>,
    pub open_orders: Option<Pubkey>,
    pub target_orders: Option<Pubkey>,
    pub market_program: Option<Pubkey>,
    pub market: Option<Pubkey>,
    pub market_bids: Option<Pubkey>,
    pub market_asks: Option<Pubkey>,
    pub market_event_queue: Option<Pubkey>,
    pub market_coin_vault: Option<Pubkey>,
    pub market_pc_vault: Option<Pubkey>,
    pub market_vault_signer: Option<Pubkey>,
    // Orca Whirlpool
    pub tick_array_0: Option<Pubkey>,
    pub tick_array_1: Option<Pubkey>,
    pub tick_array_2: Option<Pubkey>,
    pub oracle: Option<Pubkey>,
}

/// Serializable pool config loaded from pools.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    pub id: String,
    pub dex: DexKind,
    pub token_a: String,
    pub token_b: String,
    pub vault_a: String,
    pub vault_b: String,
    #[serde(default)]
    pub fee_bps: u64,
    #[serde(default)]
    pub state_account: Option<String>,
    #[serde(default)]
    pub extra: ExtraConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExtraConfig {
    pub amm_authority: Option<String>,
    pub open_orders: Option<String>,
    pub target_orders: Option<String>,
    pub market_program: Option<String>,
    pub market: Option<String>,
    pub market_bids: Option<String>,
    pub market_asks: Option<String>,
    pub market_event_queue: Option<String>,
    pub market_coin_vault: Option<String>,
    pub market_pc_vault: Option<String>,
    pub market_vault_signer: Option<String>,
    pub tick_array_0: Option<String>,
    pub tick_array_1: Option<String>,
    pub tick_array_2: Option<String>,
    pub oracle: Option<String>,
}

/// A quote returned by a DEX quote function.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SwapQuote {
    pub amount_in: u64,
    pub amount_out: u64,
    pub fee_amount: u64,
    /// Price impact as a fraction (0.01 = 1%)
    pub price_impact: f64,
    pub a_to_b: bool,
}

fn parse_pubkey_opt(s: &Option<String>) -> Option<Pubkey> {
    s.as_ref().and_then(|v| Pubkey::from_str(v).ok())
}

impl TryFrom<PoolConfig> for Arc<Pool> {
    type Error = anyhow::Error;

    fn try_from(cfg: PoolConfig) -> anyhow::Result<Arc<Pool>> {
        use anyhow::Context;
        Ok(Arc::new(Pool {
            id: Pubkey::from_str(&cfg.id).context("invalid pool id")?,
            dex: cfg.dex,
            token_a: Pubkey::from_str(&cfg.token_a).context("invalid token_a")?,
            token_b: Pubkey::from_str(&cfg.token_b).context("invalid token_b")?,
            vault_a: Pubkey::from_str(&cfg.vault_a).context("invalid vault_a")?,
            vault_b: Pubkey::from_str(&cfg.vault_b).context("invalid vault_b")?,
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(cfg.fee_bps),
            sqrt_price_x64: AtomicU64::new(0),
            state_account: parse_pubkey_opt(&cfg.state_account),
            extra: PoolExtra {
                amm_authority: parse_pubkey_opt(&cfg.extra.amm_authority),
                open_orders: parse_pubkey_opt(&cfg.extra.open_orders),
                target_orders: parse_pubkey_opt(&cfg.extra.target_orders),
                market_program: parse_pubkey_opt(&cfg.extra.market_program),
                market: parse_pubkey_opt(&cfg.extra.market),
                market_bids: parse_pubkey_opt(&cfg.extra.market_bids),
                market_asks: parse_pubkey_opt(&cfg.extra.market_asks),
                market_event_queue: parse_pubkey_opt(&cfg.extra.market_event_queue),
                market_coin_vault: parse_pubkey_opt(&cfg.extra.market_coin_vault),
                market_pc_vault: parse_pubkey_opt(&cfg.extra.market_pc_vault),
                market_vault_signer: parse_pubkey_opt(&cfg.extra.market_vault_signer),
                tick_array_0: parse_pubkey_opt(&cfg.extra.tick_array_0),
                tick_array_1: parse_pubkey_opt(&cfg.extra.tick_array_1),
                tick_array_2: parse_pubkey_opt(&cfg.extra.tick_array_2),
                oracle: parse_pubkey_opt(&cfg.extra.oracle),
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cp(reserve_a: u64, reserve_b: u64, fee_bps: u64) -> PoolState {
        PoolState::ConstantProduct { reserve_a, reserve_b, fee_bps }
    }

    // ─── amount_out formula ───────────────────────────────────────────────────

    #[test]
    fn zero_fee_equal_reserves_halves_output() {
        // x * y = k: (1000 + 1000) * (1000 - out) = 1000 * 1000 → out = 500
        assert_eq!(cp(1_000, 1_000, 0).get_amount_out(1_000, true), 500);
    }

    #[test]
    fn fee_reduces_output_vs_zero_fee() {
        let no_fee   = cp(1_000_000, 1_000_000, 0).get_amount_out(10_000, true);
        let with_fee = cp(1_000_000, 1_000_000, 25).get_amount_out(10_000, true);
        assert!(with_fee < no_fee, "fee must reduce output");
    }

    #[test]
    fn zero_reserve_in_returns_zero() {
        assert_eq!(cp(0, 1_000_000, 25).get_amount_out(10_000, true), 0);
    }

    #[test]
    fn zero_amount_in_returns_zero() {
        assert_eq!(cp(1_000_000, 1_000_000, 25).get_amount_out(0, true), 0);
    }

    #[test]
    fn b_to_a_mirrors_a_to_b_on_symmetric_pool() {
        let state = cp(1_000_000, 1_000_000, 25);
        assert_eq!(
            state.get_amount_out(50_000, true),
            state.get_amount_out(50_000, false),
        );
    }

    // ─── rate calculations ────────────────────────────────────────────────────

    #[test]
    fn rates_are_reciprocal_with_zero_fee() {
        let state = cp(3_000, 7_000, 0);
        let product = state.rate_a_to_b() * state.rate_b_to_a();
        assert!((product - 1.0).abs() < 1e-10, "product was {product}");
    }

    #[test]
    fn fee_reduces_rate() {
        let no_fee   = cp(1_000, 1_000, 0);
        let with_fee = cp(1_000, 1_000, 100);
        assert!(with_fee.rate_a_to_b() < no_fee.rate_a_to_b());
    }

    // ─── round-trip invariant ─────────────────────────────────────────────────

    #[test]
    fn round_trip_on_same_pool_always_loses_money() {
        // Fundamental AMM property: trading A→B→A on a single pool never profits.
        let state = cp(10_000_000, 10_000_000, 25);
        let mid   = state.get_amount_out(100_000, true);
        let back  = state.get_amount_out(mid, false);
        assert!(back < 100_000, "round-trip returned {back}, expected < 100_000");
    }
}
