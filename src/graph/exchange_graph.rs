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
        // Meteora DAMM stable-swap pools (USDC/USDT, SOL/mSOL) use the Curve StableSwap
        // invariant. marginal_rate probes with a tiny amount so graph edges reflect the
        // actual near-peg rate rather than the 2× wrong constant-product formula.
        if pool.stable {
            use std::sync::atomic::Ordering;
            let amp = pool.extra.damm_amp.unwrap_or(100);
            let fee = pool.fee_bps.load(Ordering::Relaxed)
                .max(if matches!(pool.dex, DexKind::MeteoraDamm) { 25 } else { 0 });
            let ra  = pool.reserve_a.load(Ordering::Relaxed);
            let rb  = pool.reserve_b.load(Ordering::Relaxed);
            if ra == 0 || rb == 0 {
                return;
            }
            let vpr = pool.damm_virtual_price.load(Ordering::Relaxed);
            let price_scale = if vpr == 0 { crate::dex::stable_math::PRICE_SCALE } else { vpr };
            let rate_a_to_b = crate::dex::stable_math::marginal_rate_damm(ra, rb, amp, fee, price_scale, true);
            let rate_b_to_a = crate::dex::stable_math::marginal_rate_damm(ra, rb, amp, fee, price_scale, false);
            if !(rate_a_to_b > 0.0) || !rate_a_to_b.is_finite()
                || !(rate_b_to_a > 0.0) || !rate_b_to_a.is_finite()
            {
                return;
            }
            let weight_a_to_b = -rate_a_to_b.ln();
            let weight_b_to_a = -rate_b_to_a.ln();
            self.edges.insert(
                (pool.token_a, pool.token_b, pool.id),
                Edge { from: pool.token_a, to: pool.token_b, weight: weight_a_to_b,
                       pool_id: pool.id, dex: pool.dex, a_to_b: true },
            );
            self.edges.insert(
                (pool.token_b, pool.token_a, pool.id),
                Edge { from: pool.token_b, to: pool.token_a, weight: weight_b_to_a,
                       pool_id: pool.id, dex: pool.dex, a_to_b: false },
            );
            self.generation.fetch_add(1, Ordering::Release);
            return;
        }

        // Phoenix CLOB: bid and ask prices live in separate atomics. Handle each edge
        // direction independently — only insert edges with a valid non-zero price, and
        // remove stale edges when a price drops to zero (e.g., one side of the book dries up).
        if pool.dex == DexKind::Phoenix {
            let bid_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
            let ask_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
            if bid_bits == 0 && ask_bits == 0 {
                return;
            }
            let fee = 1.0 - (pool.fee_bps.load(Ordering::Relaxed) as f64 / 10_000.0);

            // parse_state validates prices before storing, so non-zero bits imply a
            // positive finite f64. We always insert-or-remove to prevent stale edges.
            if bid_bits > 0 {
                self.edges.insert(
                    (pool.token_a, pool.token_b, pool.id),
                    Edge { from: pool.token_a, to: pool.token_b,
                           weight: -(f64::from_bits(bid_bits) * fee).ln(),
                           pool_id: pool.id, dex: pool.dex, a_to_b: true },
                );
            } else {
                self.edges.remove(&(pool.token_a, pool.token_b, pool.id));
            }

            if ask_bits > 0 {
                self.edges.insert(
                    (pool.token_b, pool.token_a, pool.id),
                    Edge { from: pool.token_b, to: pool.token_a,
                           weight: -(1.0 / f64::from_bits(ask_bits) * fee).ln(),
                           pool_id: pool.id, dex: pool.dex, a_to_b: false },
                );
            } else {
                self.edges.remove(&(pool.token_b, pool.token_a, pool.id));
            }

            self.generation.fetch_add(1, Ordering::Release);
            return;
        }

        // Jupiter synthetic edges: the poller stores the implied marginal rate per direction
        // as f64 bits (a→b in sqrt_price_x64, b→a in damm_virtual_price), since the two
        // directions are independently quoted and NOT reciprocal (fees + asymmetric routing).
        // Mirror the Phoenix two-atomic pattern: insert-or-remove each direction independently.
        if pool.dex == DexKind::Jupiter {
            let ab_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
            let ba_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
            if ab_bits == 0 && ba_bits == 0 {
                return;
            }
            let rate_a_to_b = f64::from_bits(ab_bits);
            let rate_b_to_a = f64::from_bits(ba_bits);

            if rate_a_to_b > 0.0 && rate_a_to_b.is_finite() {
                self.edges.insert(
                    (pool.token_a, pool.token_b, pool.id),
                    Edge { from: pool.token_a, to: pool.token_b, weight: -rate_a_to_b.ln(),
                           pool_id: pool.id, dex: pool.dex, a_to_b: true },
                );
            } else {
                self.edges.remove(&(pool.token_a, pool.token_b, pool.id));
            }

            if rate_b_to_a > 0.0 && rate_b_to_a.is_finite() {
                self.edges.insert(
                    (pool.token_b, pool.token_a, pool.id),
                    Edge { from: pool.token_b, to: pool.token_a, weight: -rate_b_to_a.ln(),
                           pool_id: pool.id, dex: pool.dex, a_to_b: false },
                );
            } else {
                self.edges.remove(&(pool.token_b, pool.token_a, pool.id));
            }

            self.generation.fetch_add(1, Ordering::Release);
            return;
        }

        let (rate_a_to_b, rate_b_to_a) = match pool.dex {
            DexKind::OrcaWhirlpool | DexKind::RaydiumClmm | DexKind::MeteoraDlmm
            | DexKind::Lifinity | DexKind::Invariant => {
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

    /// Total number of directed edges (each pool contributes up to 2 edges).
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    #[cfg(test)]
    pub fn edges_vec(&self) -> Vec<Edge> {
        self.edges.iter().map(|r| r.value().clone()).collect()
    }

    /// Edge counts broken down by DEX kind. Useful for spotting a category of pools
    /// (e.g. CLMM) that aren't contributing edges due to stale sqrt_price.
    /// Order: [RaydiumAmmV4, RaydiumClmm, OrcaWhirlpool, MeteoraDamm, MeteoraDlmm, Phoenix, Lifinity, Invariant, Saber, Jupiter, PumpSwap]
    pub fn edge_count_by_dex(&self) -> [usize; 11] {
        let mut counts = [0usize; 11];
        for r in self.edges.iter() {
            let idx = match r.value().dex {
                DexKind::RaydiumAmmV4  => 0,
                DexKind::RaydiumClmm   => 1,
                DexKind::OrcaWhirlpool => 2,
                DexKind::MeteoraDamm   => 3,
                DexKind::MeteoraDlmm   => 4,
                DexKind::Phoenix       => 5,
                DexKind::Lifinity      => 6,
                DexKind::Invariant     => 7,
                DexKind::Saber         => 8,
                DexKind::Jupiter       => 9,
                // pricing-only; never in the arb registry, so always 0 here
                DexKind::PumpSwap      => 10,
            };
            counts[idx] += 1;
        }
        counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, Pool, PoolExtra};
    use solana_sdk::pubkey::Pubkey;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
    use std::sync::Arc;

    fn phoenix_pool_with_prices(bid: f64, ask: f64) -> Arc<Pool> {
        let p = Arc::new(Pool {
            id: Pubkey::new_unique(),
            dex: DexKind::Phoenix,
            token_a: Pubkey::new_unique(),
            token_b: Pubkey::new_unique(),
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(0),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        });
        p.sqrt_price_x64.store(bid.to_bits(), Ordering::Relaxed);
        p.damm_virtual_price.store(ask.to_bits(), Ordering::Relaxed);
        p
    }

    #[test]
    fn phoenix_two_sided_book_creates_two_edges() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 2, "two-sided book must produce 2 edges");
    }

    #[test]
    fn phoenix_asks_only_creates_one_b_to_a_edge() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(0.0, 11.0);
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        assert_eq!(edges.len(), 1, "asks-only must produce exactly 1 edge");
        assert!(!edges[0].a_to_b, "the single edge must be b→a");
    }

    #[test]
    fn phoenix_bids_only_creates_one_a_to_b_edge() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 0.0);
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        assert_eq!(edges.len(), 1, "bids-only must produce exactly 1 edge");
        assert!(edges[0].a_to_b, "the single edge must be a→b");
    }

    #[test]
    fn phoenix_empty_book_creates_no_edges() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(0.0, 0.0);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 0, "empty book must produce 0 edges");
    }

    #[test]
    fn phoenix_a_to_b_weight_uses_bid_price() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0); // fee_bps=0
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        let a_to_b = edges.iter().find(|e| e.a_to_b).expect("a→b edge must exist");
        // weight = -ln(bid * fee) = -ln(10.0 * 1.0) = -ln(10.0)
        let expected = -(10.0f64).ln();
        assert!((a_to_b.weight - expected).abs() < 1e-9,
            "a→b weight should be {expected}, got {}", a_to_b.weight);
    }

    // ── Jupiter synthetic edges (poller writes both directions independently) ──

    fn jupiter_pool_with_rates(rate_ab: f64, rate_ba: f64) -> Arc<Pool> {
        let p = Pool::new_jupiter(Pubkey::new_unique(), Pubkey::new_unique());
        p.sqrt_price_x64.store(rate_ab.to_bits(), Ordering::Relaxed);
        p.damm_virtual_price.store(rate_ba.to_bits(), Ordering::Relaxed);
        p
    }

    #[test]
    fn jupiter_both_directions_create_two_edges() {
        let graph = ExchangeGraph::new();
        let pool = jupiter_pool_with_rates(0.15, 6.5);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 2, "both rates present must produce 2 edges");
    }

    #[test]
    fn jupiter_directions_are_independent_not_reciprocal() {
        let graph = ExchangeGraph::new();
        // Deliberately non-reciprocal: 0.15 and 6.5 (not 1/0.15 ≈ 6.667).
        let pool = jupiter_pool_with_rates(0.15, 6.5);
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        let ab = edges.iter().find(|e| e.a_to_b).expect("a→b edge");
        let ba = edges.iter().find(|e| !e.a_to_b).expect("b→a edge");
        assert!((ab.weight - -(0.15f64).ln()).abs() < 1e-9);
        assert!((ba.weight - -(6.5f64).ln()).abs() < 1e-9);
    }

    #[test]
    fn jupiter_missing_direction_creates_one_edge() {
        let graph = ExchangeGraph::new();
        let pool = jupiter_pool_with_rates(0.15, 0.0); // no route b→a
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        assert_eq!(edges.len(), 1, "one routable direction must produce exactly 1 edge");
        assert!(edges[0].a_to_b, "the single edge must be a→b");
    }

    #[test]
    fn jupiter_stale_edge_removed_when_rate_drops_to_zero() {
        let graph = ExchangeGraph::new();
        let pool = jupiter_pool_with_rates(0.15, 6.5);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 2);
        // A later poll finds no route b→a — that edge must be removed, not left stale.
        pool.damm_virtual_price.store(0.0f64.to_bits(), Ordering::Relaxed);
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        assert_eq!(edges.len(), 1, "dropped direction must be removed");
        assert!(edges[0].a_to_b);
    }

    #[test]
    fn phoenix_b_to_a_weight_uses_ask_price() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0); // fee_bps=0
        graph.update_pool(&pool);
        let edges = graph.edges_vec();
        let b_to_a = edges.iter().find(|e| !e.a_to_b).expect("b→a edge must exist");
        // weight = -ln((1/ask) * fee) = -ln(1/11.0) = ln(11.0)
        let expected = -(1.0 / 11.0f64).ln();
        assert!((b_to_a.weight - expected).abs() < 1e-9,
            "b→a weight should be {expected}, got {}", b_to_a.weight);
    }

    #[test]
    fn phoenix_stale_edge_removed_when_price_drops_to_zero() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 2);

        // Ask dries up — clear damm_virtual_price
        pool.damm_virtual_price.store(0, Ordering::Relaxed);
        graph.update_pool(&pool);

        let edges = graph.edges_vec();
        assert_eq!(edges.len(), 1, "stale b→a edge must be removed when ask drops to 0");
        assert!(edges[0].a_to_b, "remaining edge must be a→b");
    }
}
