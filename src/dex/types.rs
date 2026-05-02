use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::Arc;

pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";

// Compile-time-decoded Pubkeys for hot-path use. The `pubkey!` macro decodes
// the base58 string at compile time, so accessing these is a 32-byte memcpy
// vs. a runtime base58 decode (~µs each).
pub const WSOL_PUBKEY: Pubkey = solana_sdk::pubkey!("So11111111111111111111111111111111111111112");
pub const RAYDIUM_AMM_V4_PUBKEY: Pubkey = solana_sdk::pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const RAYDIUM_CLMM_PUBKEY: Pubkey = solana_sdk::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
pub const ORCA_WHIRLPOOL_PUBKEY: Pubkey = solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
pub const METEORA_DAMM_PUBKEY:  Pubkey = solana_sdk::pubkey!("Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB");
pub const METEORA_DLMM_PUBKEY:  Pubkey = solana_sdk::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
pub const PHOENIX_PUBKEY:        Pubkey = solana_sdk::pubkey!("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY");

/// Returns a short human-readable symbol for known token mints.
/// Falls back to the first 6 characters of the base58 pubkey for unknown mints.
pub fn mint_symbol(pubkey: &Pubkey) -> String {
    match pubkey.to_string().as_str() {
        "So11111111111111111111111111111111111111112" => "SOL".into(),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => "USDC".into(),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => "USDT".into(),
        "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So" => "mSOL".into(),
        "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R" => "RAY".into(),
        "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs" => "ETH".into(),
        "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh" => "BTC".into(),
        "HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr" => "EURC".into(),
        s => s[..6].to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DexKind {
    RaydiumAmmV4,
    RaydiumClmm,
    OrcaWhirlpool,
    MeteoraDamm,
    /// Meteora DLMM: discrete concentrated-bin AMM.
    /// Price = (1 + bin_step/10000)^active_id (in natural token units).
    /// active_id is read from state_account; stored as f64 bits in sqrt_price_x64.
    MeteoraDlmm,
    /// Phoenix v1 CLOB. Price derived from best bid/ask in market account.
    /// Edge generation is not yet implemented; pools are loaded but skipped in the graph.
    Phoenix,
}

impl DexKind {
    /// Short display name used in cycle logs.
    pub fn short_name(&self) -> &'static str {
        match self {
            Self::RaydiumAmmV4  => "Raydium",
            Self::RaydiumClmm   => "Raydium CL",
            Self::OrcaWhirlpool => "Orca",
            Self::MeteoraDamm   => "Meteora",
            Self::MeteoraDlmm   => "DLMM",
            Self::Phoenix       => "Phoenix",
        }
    }

    pub fn program_id(&self) -> Pubkey {
        match self {
            Self::RaydiumAmmV4  => RAYDIUM_AMM_V4_PUBKEY,
            Self::RaydiumClmm   => RAYDIUM_CLMM_PUBKEY,
            Self::OrcaWhirlpool => ORCA_WHIRLPOOL_PUBKEY,
            Self::MeteoraDamm   => METEORA_DAMM_PUBKEY,
            Self::MeteoraDlmm   => METEORA_DLMM_PUBKEY,
            Self::Phoenix       => PHOENIX_PUBKEY,
        }
    }

    pub fn fee_bps(&self) -> u64 {
        match self {
            Self::RaydiumAmmV4  => 25,
            Self::RaydiumClmm   => 0,   // per-pool, read from state
            Self::OrcaWhirlpool => 0,   // per-pool, read from state
            Self::MeteoraDamm   => 0,   // dynamic, read from state
            Self::MeteoraDlmm   => 0,   // per-pool, read from state
            Self::Phoenix       => 10,  // default taker fee; varies by market
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
    /// DLMM only: active bin id (i32 stored as AtomicI32), needed to derive bin array PDAs for swap
    pub active_bin_id: AtomicI32,
    /// CL pools (Orca, Raydium CLMM): tick_current_index parsed from pool state account.
    /// Avoids re-deriving the tick via float arithmetic; valid when sqrt_price_x64 != 0.
    pub tick_current_index: AtomicI32,
    /// For CL pools: pool state account to subscribe to
    pub state_account: Option<Pubkey>,
    /// True for Meteora DAMM pools that use the stable-swap (Curve) invariant.
    /// These pools give wrong rates when priced with the CP formula and are excluded
    /// from the graph until a Curve-invariant quote is implemented.
    pub stable: bool,
    /// Meteora DAMM: pool's LP balance inside vault A (scaled reserve tracking)
    pub a_lp_balance: AtomicU64,
    /// Meteora DAMM: pool's LP balance inside vault B (scaled reserve tracking)
    pub b_lp_balance: AtomicU64,

    // Extra accounts needed to build swap instructions
    pub extra: PoolExtra,
}

impl Pool {
    /// Pure size-based price impact: amount_in / (reserve_in + amount_in).
    /// This matches the Raydium AMM formula and measures only the pool-level shift,
    /// not fee drag. Fees are already baked into amount_out by the DEX quote
    /// functions and would otherwise be double-counted here.
    /// Returns 0.0 if reserves or amount_in are zero.
    pub fn price_impact(&self, amount_in: u64, _amount_out: u64, a_to_b: bool) -> f64 {
        let reserve_in = if a_to_b {
            self.reserve_a.load(Ordering::Relaxed)
        } else {
            self.reserve_b.load(Ordering::Relaxed)
        };
        if reserve_in == 0 || amount_in == 0 {
            return 0.0;
        }
        amount_in as f64 / (reserve_in as f64 + amount_in as f64)
    }

    /// Token program for the given mint (defaults to SPL Token if not overridden).
    pub fn token_program_for(&self, a_side: bool) -> Pubkey {
        if a_side {
            self.extra.token_program_a.unwrap_or_else(spl_token::id)
        } else {
            self.extra.token_program_b.unwrap_or_else(spl_token::id)
        }
    }

    pub fn snapshot_state(&self) -> PoolState {
        let fee = self.fee_bps.load(Ordering::Relaxed);
        match self.dex {
            DexKind::RaydiumAmmV4 | DexKind::MeteoraDamm => PoolState::ConstantProduct {
                reserve_a: self.reserve_a.load(Ordering::Relaxed),
                reserve_b: self.reserve_b.load(Ordering::Relaxed),
                fee_bps: if fee == 0 { self.dex.fee_bps() } else { fee },
            },
            // DLMM uses same sqrt_price_x64 slot but stores price (not sqrt) as f64 bits.
            // snapshot_state is only called for CP-formula evaluation; DLMM edges use
            // the sqrt_price_x64 path in exchange_graph::update_pool directly.
            DexKind::MeteoraDlmm | DexKind::Phoenix => PoolState::ConstantProduct {
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
    // Raydium CLMM — static accounts derived from pool state at startup
    pub clmm_amm_config: Option<Pubkey>,   // fee / tick config (offset 9 of pool state)
    pub clmm_observation: Option<Pubkey>,  // observation state (offset 201)
    pub clmm_tick_spacing: Option<u16>,    // ticks between initializable prices (offset 235)
    // Meteora DAMM — pool's LP token accounts inside shared vaults
    pub a_vault_lp: Option<Pubkey>,
    pub b_vault_lp: Option<Pubkey>,
    // Meteora DAMM — vault-derived accounts needed for swap instruction
    pub a_token_vault: Option<Pubkey>,   // SPL token account inside vault A (vault off 19)
    pub b_token_vault: Option<Pubkey>,   // SPL token account inside vault B
    pub a_vault_lp_mint: Option<Pubkey>, // LP mint of vault A (vault off 115)
    pub b_vault_lp_mint: Option<Pubkey>, // LP mint of vault B
    pub admin_token_fee_a: Option<Pubkey>, // pool off 232
    pub admin_token_fee_b: Option<Pubkey>, // pool off 264
    /// Override SPL Token program per mint. Defaults to Token (keg) if None.
    /// Set to spl_token_2022::id() for Token-2022 pools. Mixed-program pools
    /// (one Token, one Token-2022) require the Orca swap_v2 instruction format.
    pub token_program_a: Option<Pubkey>,
    pub token_program_b: Option<Pubkey>,
    // Meteora DLMM
    pub dlmm_bin_step: Option<u16>,
    // Phoenix CLOB
    pub phoenix_base_lot_size:  Option<u64>,
    pub phoenix_quote_lot_size: Option<u64>,
    pub phoenix_tick_size:      Option<u64>,
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
    pub stable: bool,
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
    pub clmm_amm_config: Option<String>,
    pub clmm_observation: Option<String>,
    pub clmm_tick_spacing: Option<u16>,
    pub a_vault_lp: Option<String>,
    pub b_vault_lp: Option<String>,
    pub a_token_vault: Option<String>,
    pub b_token_vault: Option<String>,
    pub a_vault_lp_mint: Option<String>,
    pub b_vault_lp_mint: Option<String>,
    pub admin_token_fee_a: Option<String>,
    pub admin_token_fee_b: Option<String>,
    pub token_program_a: Option<String>,
    pub token_program_b: Option<String>,
    pub dlmm_bin_step: Option<u16>,
    pub phoenix_base_lot_size:  Option<String>,
    pub phoenix_quote_lot_size: Option<String>,
    pub phoenix_tick_size:      Option<String>,
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
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: parse_pubkey_opt(&cfg.state_account),
            stable: cfg.stable,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
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
                clmm_amm_config: parse_pubkey_opt(&cfg.extra.clmm_amm_config),
                clmm_observation: parse_pubkey_opt(&cfg.extra.clmm_observation),
                clmm_tick_spacing: cfg.extra.clmm_tick_spacing,
                a_vault_lp: parse_pubkey_opt(&cfg.extra.a_vault_lp),
                b_vault_lp: parse_pubkey_opt(&cfg.extra.b_vault_lp),
                a_token_vault: parse_pubkey_opt(&cfg.extra.a_token_vault),
                b_token_vault: parse_pubkey_opt(&cfg.extra.b_token_vault),
                a_vault_lp_mint: parse_pubkey_opt(&cfg.extra.a_vault_lp_mint),
                b_vault_lp_mint: parse_pubkey_opt(&cfg.extra.b_vault_lp_mint),
                admin_token_fee_a: parse_pubkey_opt(&cfg.extra.admin_token_fee_a),
                admin_token_fee_b: parse_pubkey_opt(&cfg.extra.admin_token_fee_b),
                token_program_a: parse_pubkey_opt(&cfg.extra.token_program_a),
                token_program_b: parse_pubkey_opt(&cfg.extra.token_program_b),
                dlmm_bin_step: cfg.extra.dlmm_bin_step,
                phoenix_base_lot_size:  cfg.extra.phoenix_base_lot_size.as_deref()
                    .and_then(|s| s.parse().ok()),
                phoenix_quote_lot_size: cfg.extra.phoenix_quote_lot_size.as_deref()
                    .and_then(|s| s.parse().ok()),
                phoenix_tick_size:      cfg.extra.phoenix_tick_size.as_deref()
                    .and_then(|s| s.parse().ok()),
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
