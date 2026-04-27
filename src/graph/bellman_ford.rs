use std::collections::HashMap;
use solana_sdk::pubkey::Pubkey;

use crate::graph::exchange_graph::{Edge, ExchangeGraph};

/// A detected arbitrage cycle, e.g. [SOL, USDC, RAY, SOL].
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ArbCycle {
    /// Ordered token mints forming the cycle (first == last == source).
    pub path: Vec<Pubkey>,
    /// Edges corresponding to each hop in the path.
    pub edges: Vec<Edge>,
    /// Sum of log-weights (negative = profit). More negative = larger gross profit.
    pub total_weight: f64,
}

impl ArbCycle {
    /// Gross profit ratio: if > 1.0, the cycle is profitable before fees.
    /// Computed as exp(-total_weight) = product of exchange rates.
    #[allow(dead_code)]
    pub fn gross_ratio(&self) -> f64 {
        (-self.total_weight).exp()
    }
}

/// Find all profitable arbitrage cycles (length 2–3) that start and end at
/// `source` (the SOL mint).
///
/// Uses explicit path enumeration rather than Bellman-Ford relaxation.
/// Rationale: standard Bellman-Ford detects negative cycles by checking
/// whether a V-th relaxation pass can still improve distances. For cycles
/// that pass exactly once through the source node, the distance at `source`
/// stabilises after exactly MAX_HOPS passes and the V+1-th pass cannot
/// improve it further — so the cycle is never flagged. Explicit enumeration
/// avoids this blind spot entirely and is O(E²) for 2-hop / O(E³) for 3-hop,
/// fast enough for the pool counts we track (≤ a few hundred pools).
pub fn find_negative_cycles(graph: &ExchangeGraph, source: Pubkey) -> Vec<ArbCycle> {
    let edges = graph.snapshot_edges();

    // Adjacency list: from_mint → outgoing edges
    let mut adj: HashMap<Pubkey, Vec<usize>> = HashMap::new();
    for (i, edge) in edges.iter().enumerate() {
        adj.entry(edge.from).or_default().push(i);
    }

    // Fast O(1) edge lookup: (from, to) → edge index
    let edge_map: HashMap<(Pubkey, Pubkey), usize> = edges
        .iter()
        .enumerate()
        .map(|(i, e)| ((e.from, e.to), i))
        .collect();

    let Some(src_out) = adj.get(&source) else {
        return vec![];
    };

    let mut cycles: Vec<ArbCycle> = Vec::new();

    // ── 2-hop: source → X → source ───────────────────────────────────────────
    for &i1 in src_out {
        let e1 = &edges[i1];
        let x = e1.to;
        if x == source { continue; }

        if let Some(&i2) = edge_map.get(&(x, source)) {
            let e2 = &edges[i2];
            let w = e1.weight + e2.weight;
            if w < 0.0 {
                let cycle = ArbCycle {
                    path: vec![source, x, source],
                    edges: vec![e1.clone(), e2.clone()],
                    total_weight: w,
                };
                push_unique(&mut cycles, cycle);
            }
        }
    }

    // ── 3-hop: source → X → Y → source ───────────────────────────────────────
    for &i1 in src_out {
        let e1 = &edges[i1];
        let x = e1.to;
        if x == source { continue; }

        let Some(x_out) = adj.get(&x) else { continue };

        for &i2 in x_out {
            let e2 = &edges[i2];
            let y = e2.to;
            if y == source || y == x { continue; }

            if let Some(&i3) = edge_map.get(&(y, source)) {
                let e3 = &edges[i3];
                let w = e1.weight + e2.weight + e3.weight;
                if w < 0.0 {
                    let cycle = ArbCycle {
                        path: vec![source, x, y, source],
                        edges: vec![e1.clone(), e2.clone(), e3.clone()],
                        total_weight: w,
                    };
                    push_unique(&mut cycles, cycle);
                }
            }
        }
    }

    // Most profitable (most negative weight) first
    cycles.sort_by(|a, b| {
        a.total_weight.partial_cmp(&b.total_weight).unwrap_or(std::cmp::Ordering::Equal)
    });
    cycles
}

fn push_unique(cycles: &mut Vec<ArbCycle>, candidate: ArbCycle) {
    let is_dup = cycles.iter().any(|c| {
        c.path.len() == candidate.path.len()
            && c.path.iter().zip(&candidate.path).all(|(a, b)| a == b)
    });
    if !is_dup {
        cycles.push(candidate);
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, Pool, PoolExtra, WSOL_MINT};
    use crate::graph::exchange_graph::ExchangeGraph;
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;
    use std::sync::atomic::AtomicU64;
    use std::sync::Arc;

    fn sol() -> Pubkey { Pubkey::from_str(WSOL_MINT).unwrap() }

    /// Zero-fee pool so edge weights are exactly -ln(reserve_b / reserve_a).
    fn pool(token_a: Pubkey, token_b: Pubkey, reserve_a: u64, reserve_b: u64) -> Arc<Pool> {
        Arc::new(Pool {
            id: Pubkey::new_unique(),
            dex: DexKind::RaydiumAmmV4,
            token_a,
            token_b,
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(reserve_a),
            reserve_b: AtomicU64::new(reserve_b),
            fee_bps: AtomicU64::new(0),
            sqrt_price_x64: AtomicU64::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
        })
    }

    // ─── no cycle ─────────────────────────────────────────────────────────────

    #[test]
    fn single_pool_has_no_cycle() {
        let g    = ExchangeGraph::new();
        let sol  = sol();
        let usdc = Pubkey::new_unique();
        g.update_pool(&pool(sol, usdc, 1_000_000, 1_000_000));
        assert!(find_negative_cycles(&g, sol).is_empty());
    }

    #[test]
    fn source_not_in_graph_returns_empty() {
        let g = ExchangeGraph::new();
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        g.update_pool(&pool(a, b, 1_000_000, 2_000_000));
        assert!(find_negative_cycles(&g, Pubkey::new_unique()).is_empty());
    }

    #[test]
    fn unprofitable_3hop_cycle_not_detected() {
        // All pools are balanced (1:1 reserves). With 25 bps fee per hop the
        // rate in every direction is 0.9975, giving a 3-hop product of
        // 0.9975^3 ≈ 0.9925 < 1.0 in *both* forward and reverse directions.
        //
        // Note: a skewed setup like reserve_b = 0.9 * reserve_a creates a
        // profitable *reverse* cycle (product 1/0.9 > 1), so equal reserves
        // are the correct choice for an "all directions unprofitable" test.
        let g    = ExchangeGraph::new();
        let sol  = sol();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();
        g.update_pool(&pool(sol,  usdc, 1_000_000, 1_000_000));
        g.update_pool(&pool(usdc, ray,  1_000_000, 1_000_000));
        g.update_pool(&pool(ray,  sol,  1_000_000, 1_000_000));
        assert!(find_negative_cycles(&g, sol).is_empty());
    }

    // ─── profitable cycle ─────────────────────────────────────────────────────

    #[test]
    fn profitable_3hop_cycle_is_detected() {
        // rate product = 0.1 * 10.0 * 1.1 = 1.1 → 10 % gross profit
        let g    = ExchangeGraph::new();
        let sol  = sol();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();
        g.update_pool(&pool(sol,  usdc, 1_000_000, 100_000));
        g.update_pool(&pool(usdc, ray,  100_000, 1_000_000));
        g.update_pool(&pool(ray,  sol,  1_000_000, 1_100_000));

        let cycles = find_negative_cycles(&g, sol);
        assert!(!cycles.is_empty(), "expected a profitable cycle");

        let c = &cycles[0];
        assert_eq!(c.path.first(), Some(&sol));
        assert_eq!(c.path.last(),  Some(&sol));
        assert!(c.total_weight < 0.0, "total_weight={}", c.total_weight);
        assert!(c.gross_ratio() > 1.0, "gross_ratio={}", c.gross_ratio());
    }

    #[test]
    fn cycles_sorted_most_profitable_first() {
        // Two profitable 3-hop cycles; the one with higher profit comes first.
        let g    = ExchangeGraph::new();
        let sol  = sol();
        let a    = Pubkey::new_unique();
        let b    = Pubkey::new_unique();
        let c    = Pubkey::new_unique();
        let d    = Pubkey::new_unique();

        // Cycle 1: 10 % profit
        g.update_pool(&pool(sol, a, 1_000_000, 100_000));
        g.update_pool(&pool(a,   b, 100_000, 1_000_000));
        g.update_pool(&pool(b,   sol, 1_000_000, 1_100_000));

        // Cycle 2: 5 % profit
        g.update_pool(&pool(sol, c, 1_000_000, 100_000));
        g.update_pool(&pool(c,   d, 100_000, 1_000_000));
        g.update_pool(&pool(d,   sol, 1_000_000, 1_050_000));

        let cycles = find_negative_cycles(&g, sol);
        assert!(cycles.len() >= 2, "expected at least 2 cycles");
        assert!(
            cycles[0].total_weight <= cycles[1].total_weight,
            "most profitable cycle must be first"
        );
    }

    #[test]
    fn cycle_disconnected_from_source_is_ignored() {
        let g   = ExchangeGraph::new();
        let sol = sol();
        let a   = Pubkey::new_unique();
        let b   = Pubkey::new_unique();
        let c_m = Pubkey::new_unique();
        // Profitable A→B→C→A not involving SOL
        g.update_pool(&pool(a,   b,   1_000, 10_000));
        g.update_pool(&pool(b,   c_m, 1_000, 10_000));
        g.update_pool(&pool(c_m, a,   1_000, 10_000));
        // SOL→A only (no path back)
        g.update_pool(&pool(sol, a, 1_000_000, 1_000_000));
        assert!(find_negative_cycles(&g, sol).is_empty());
    }
}
