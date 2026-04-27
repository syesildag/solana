use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::dex::types::{DexKind, Pool};

/// A directed edge in the token exchange graph.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct Edge {
    pub from: Pubkey,
    pub to: Pubkey,
    /// Negative log of the exchange rate: -ln(amount_out / amount_in).
    /// A negative cycle (sum < 0) means arbitrage profit.
    pub weight: f64,
    pub pool_id: Pubkey,
    pub dex: DexKind,
    /// True if this edge goes from pool.token_a → pool.token_b
    pub a_to_b: bool,
}

/// The live token exchange graph.
/// Each pool contributes two directed edges (both swap directions).
pub struct ExchangeGraph {
    /// (from_mint, to_mint) → edge
    edges: DashMap<(Pubkey, Pubkey), Edge>,
}

impl ExchangeGraph {
    pub fn new() -> Self {
        Self { edges: DashMap::new() }
    }

    /// Recompute and upsert both edge directions for a pool after a reserve update.
    pub fn update_pool(&self, pool: &Arc<Pool>) {
        let (rate_a_to_b, rate_b_to_a) = match pool.dex {
            DexKind::OrcaWhirlpool | DexKind::RaydiumClmm => {
                // For CLMM pools, vault token balances can be heavily skewed when the
                // current price is near the edge of (or outside) the concentrated
                // liquidity range: one vault can hold almost all tokens while the other
                // is near-empty. Using those vault balances in a CP formula produces
                // wildly wrong rates (phantom arbitrage opportunities).
                //
                // The `sqrt_price` field encodes the actual marginal price as a
                // Q64.64 fixed-point number. We store it as f64 bits to avoid u64
                // overflow (e.g. BTC/USDC has sqrt_price ≈ 29·2^64 > u64::MAX).
                // Using this price for graph edges gives the correct marginal rate
                // regardless of vault imbalance.
                let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
                if price_bits == 0 {
                    return; // not yet initialised from RPC
                }
                let price = f64::from_bits(price_bits); // token_b per token_a (raw units)
                let fee = 1.0 - (pool.fee_bps.load(Ordering::Relaxed) as f64 / 10_000.0);
                (price * fee, (1.0 / price) * fee)
            }
            _ => {
                let state = pool.snapshot_state();
                (state.rate_a_to_b(), state.rate_b_to_a())
            }
        };

        // Guard against degenerate pools: zero reserves, infinity, or NaN.
        // Note: `!(x > 0.0)` is true for NaN, 0.0, and negatives — more robust than `x <= 0.0`.
        if !(rate_a_to_b > 0.0) || !rate_a_to_b.is_finite()
            || !(rate_b_to_a > 0.0) || !rate_b_to_a.is_finite()
        {
            return;
        }

        let weight_a_to_b = -rate_a_to_b.ln();
        let weight_b_to_a = -rate_b_to_a.ln();

        self.edges.insert(
            (pool.token_a, pool.token_b),
            Edge {
                from: pool.token_a,
                to: pool.token_b,
                weight: weight_a_to_b,
                pool_id: pool.id,
                dex: pool.dex,
                a_to_b: true,
            },
        );

        self.edges.insert(
            (pool.token_b, pool.token_a),
            Edge {
                from: pool.token_b,
                to: pool.token_a,
                weight: weight_b_to_a,
                pool_id: pool.id,
                dex: pool.dex,
                a_to_b: false,
            },
        );
    }

    /// Snapshot all edges for Bellman-Ford (collects while holding no locks).
    pub fn snapshot_edges(&self) -> Vec<Edge> {
        self.edges.iter().map(|r| r.value().clone()).collect()
    }

    /// All unique token nodes.
    #[allow(dead_code)]
    pub fn nodes(&self) -> Vec<Pubkey> {
        let mut seen = std::collections::HashSet::new();
        for r in self.edges.iter() {
            seen.insert(r.value().from);
            seen.insert(r.value().to);
        }
        seen.into_iter().collect()
    }
}
