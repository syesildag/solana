use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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
    /// (from_mint, to_mint, pool_id) → edge
    edges: DashMap<(Pubkey, Pubkey, Pubkey), Edge>,
    /// Incremented (via Release) after every edge write in update_pool.
    /// snapshot_edges uses this to detect stale cached snapshots.
    generation: AtomicU64,
    /// Cached snapshot: (generation_when_built, the_snapshot).
    /// Initialised with generation=u64::MAX so the first call always rebuilds.
    snapshot_cache: Mutex<(u64, Arc<Vec<Edge>>)>,
}

impl ExchangeGraph {
    pub fn new() -> Self {
        Self {
            edges: DashMap::new(),
            generation: AtomicU64::new(0),
            snapshot_cache: Mutex::new((u64::MAX, Arc::new(Vec::new()))),
        }
    }

    /// Recompute and upsert both edge directions for a pool after a reserve update.
    pub fn update_pool(&self, pool: &Arc<Pool>) {
        // Meteora DAMM stable-swap pools (USDC/USDT etc.) use the Curve invariant, not x*y=k.
        // The CP formula on LP-fraction reserves gives rates 2× off or worse for these pools.
        // CP DAMM pools are fine — their LP-fraction reserves feed the same x*y=k formula
        // used for Raydium AMM. `pool.stable` is set from pools.json and identifies which
        // pools are Curve-style so they can be excluded until a Curve quote is implemented.
        if pool.stable {
            return;
        }

        let (rate_a_to_b, rate_b_to_a) = match pool.dex {
            DexKind::OrcaWhirlpool | DexKind::RaydiumClmm | DexKind::MeteoraDlmm => {
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
                // For Raydium AMM V4, require at least 1 SOL worth of raw lamports on each
                // side. A tiny AMM pool has near-100% price impact on any real trade,
                // making the marginal rate useless as an arb signal.
                //
                // Meteora DAMM skips this floor: its reserve_a/b already holds the
                // LP-fraction effective reserve (vault_total × pool_lp / vault_lp_supply),
                // which is inherently the pool's actual liquidity slice. Applying a raw-unit
                // floor penalises 8-decimal tokens (BTC/ETH) by a factor of ~1000×.
                if matches!(pool.dex, DexKind::RaydiumAmmV4) {
                    const MIN_RESERVE: u64 = 1_000_000_000;
                    let ra = pool.reserve_a.load(Ordering::Relaxed);
                    let rb = pool.reserve_b.load(Ordering::Relaxed);
                    if ra < MIN_RESERVE || rb < MIN_RESERVE {
                        return;
                    }
                }
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
            (pool.token_a, pool.token_b, pool.id),
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
            (pool.token_b, pool.token_a, pool.id),
            Edge {
                from: pool.token_b,
                to: pool.token_a,
                weight: weight_b_to_a,
                pool_id: pool.id,
                dex: pool.dex,
                a_to_b: false,
            },
        );

        // Signal that the snapshot cache is now stale. Release ordering ensures
        // that both edge inserts above are visible before the incremented generation.
        self.generation.fetch_add(1, Ordering::Release);
    }

    /// Returns a snapshot of all edges, using a cached copy when the graph hasn't changed.
    ///
    /// The return type is `Arc<Vec<Edge>>` so the cache can hand out shared ownership
    /// without cloning the Vec on every call — a cache hit is just an atomic ref-count bump.
    ///
    /// Concurrency: `update_pool` does (edge-write → generation.fetch_add(Release))
    /// without holding this Mutex. Reading the generation *inside* the lock with Acquire
    /// pairs with that Release, so the rebuild sees every edge write that preceded the
    /// generation we're caching against.
    pub fn snapshot_edges(&self) -> Arc<Vec<Edge>> {
        let mut cache = self.snapshot_cache.lock().expect("snapshot_cache poisoned");
        let current_gen = self.generation.load(Ordering::Acquire);
        if cache.0 == current_gen {
            return Arc::clone(&cache.1);
        }
        let snapshot: Arc<Vec<Edge>> =
            Arc::new(self.edges.iter().map(|r| r.value().clone()).collect());
        *cache = (current_gen, Arc::clone(&snapshot));
        snapshot
    }

    /// Log all edge rates so startup pool pricing can be audited.
    /// Compares each edge's implied rate against a reference SOL price to spot
    /// pools with stale or wrong reserve data.
    pub fn log_rates(&self, _sol_mint: &Pubkey) {
        use crate::dex::types::mint_symbol;
        let mut edges: Vec<_> = self.edges.iter()
            .map(|r| r.value().clone())
            .collect();
        // Sort by from-symbol then to-symbol for consistent output
        edges.sort_by(|a, b| {
            mint_symbol(&a.from).cmp(&mint_symbol(&b.from))
                .then(mint_symbol(&a.to).cmp(&mint_symbol(&b.to)))
        });

        tracing::info!("── Graph edge rates (marginal, after fee) ──────────────────────────");
        for e in &edges {
            let rate = (-e.weight).exp();
            let from = mint_symbol(&e.from);
            let to   = mint_symbol(&e.to);
            let provider = e.dex.short_name();
            tracing::info!("  {from:>10} -[{provider}]→ {to:<10}  rate={rate:.6}  pool={}", &e.pool_id.to_string()[..8]);
        }
        tracing::info!("────────────────────────────────────────────────────────────────────");
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

    /// Total number of directed edges (each pool contributes 2 edges if both rates are valid).
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Edge counts broken down by DEX kind. Useful for spotting a category of pools
    /// (e.g. CLMM) that aren't contributing edges due to stale sqrt_price.
    /// Order: [RaydiumAmmV4, RaydiumClmm, OrcaWhirlpool, MeteoraDamm, MeteoraDlmm, Phoenix]
    pub fn edge_count_by_dex(&self) -> [usize; 6] {
        let mut counts = [0usize; 6];
        for r in self.edges.iter() {
            let idx = match r.value().dex {
                DexKind::RaydiumAmmV4  => 0,
                DexKind::RaydiumClmm   => 1,
                DexKind::OrcaWhirlpool => 2,
                DexKind::MeteoraDamm   => 3,
                DexKind::MeteoraDlmm   => 4,
                DexKind::Phoenix       => 5,
            };
            counts[idx] += 1;
        }
        counts
    }
}
