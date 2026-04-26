use std::collections::HashMap;
use solana_sdk::pubkey::Pubkey;

use crate::graph::exchange_graph::{Edge, ExchangeGraph};

const INF: f64 = f64::INFINITY;
const MAX_HOPS: usize = 3;

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

/// Run Bellman-Ford from `source` (SOL mint), returning all negative cycles
/// of length ≤ MAX_HOPS that start and end at `source`.
pub fn find_negative_cycles(graph: &ExchangeGraph, source: Pubkey) -> Vec<ArbCycle> {
    let edges = graph.snapshot_edges();
    let nodes = graph.nodes();
    let n = nodes.len();

    // Map node → index for fast lookup
    let node_idx: HashMap<Pubkey, usize> = nodes.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let Some(&src_idx) = node_idx.get(&source) else {
        return vec![];
    };

    let num_nodes = n;
    let mut dist = vec![INF; num_nodes];
    let mut predecessor: Vec<Option<usize>> = vec![None; num_nodes];
    let mut hop_count = vec![0usize; num_nodes];
    dist[src_idx] = 0.0;

    // Relax edges MAX_HOPS times (we only care about paths ≤ MAX_HOPS hops)
    for _ in 0..MAX_HOPS {
        let prev_dist = dist.clone();
        for edge in &edges {
            let Some(&u) = node_idx.get(&edge.from) else { continue };
            let Some(&v) = node_idx.get(&edge.to) else { continue };
            if prev_dist[u] == INF { continue }
            let new_dist = prev_dist[u] + edge.weight;
            if new_dist < dist[v] {
                dist[v] = new_dist;
                predecessor[v] = Some(u);
                hop_count[v] = hop_count[u] + 1;
            }
        }
    }

    // Additional relaxation pass to detect negative-weight cycles
    // Any edge that still relaxes is part of a negative cycle reachable from source
    let mut cycles = Vec::new();
    let prev_dist = dist.clone();

    for edge in &edges {
        let Some(&u) = node_idx.get(&edge.from) else { continue };
        let Some(&v) = node_idx.get(&edge.to) else { continue };
        if prev_dist[u] == INF { continue }

        let new_dist = prev_dist[u] + edge.weight;
        if new_dist >= dist[v] { continue }

        // v is in a negative cycle. Try to extract a cycle through `source`.
        if let Some(cycle) = extract_cycle_through_source(
            &nodes,
            &node_idx,
            &predecessor,
            &hop_count,
            &edges,
            u,
            v,
            src_idx,
            source,
            edge,
        ) {
            // Deduplicate: skip if we already found this cycle (same set of pools)
            let is_dup = cycles.iter().any(|c: &ArbCycle| {
                c.path.len() == cycle.path.len()
                    && c.path.iter().zip(&cycle.path).all(|(a, b)| a == b)
            });
            if !is_dup {
                cycles.push(cycle);
            }
        }
    }

    cycles
}

fn extract_cycle_through_source(
    nodes: &[Pubkey],
    _node_idx: &HashMap<Pubkey, usize>,
    predecessor: &[Option<usize>],
    _hop_count: &[usize],
    edges: &[Edge],
    _u: usize,
    relaxed_v: usize,
    src_idx: usize,
    source: Pubkey,
    _triggering_edge: &Edge,
) -> Option<ArbCycle> {
    // Walk the predecessor chain from relaxed_v to reconstruct the path back to source.
    // We limit to MAX_HOPS steps to avoid getting stuck in long chains.
    let mut path_indices = vec![relaxed_v];
    let mut current = relaxed_v;
    for _ in 0..MAX_HOPS {
        match predecessor[current] {
            Some(prev) => {
                path_indices.push(prev);
                if prev == src_idx {
                    break;
                }
                current = prev;
            }
            None => return None,
        }
    }

    // The path goes backwards from cycle end → source; reverse it
    path_indices.reverse();

    // Must start at source
    if path_indices.first() != Some(&src_idx) {
        return None;
    }

    // We want SOL→...→SOL, so append source at the end too
    if path_indices.last() != Some(&src_idx) {
        // Check if there's a direct edge back to source from the last node
        let last_idx = *path_indices.last()?;
        let last_node = nodes[last_idx];
        let _edge_back = edges.iter().find(|e| e.from == last_node && e.to == source)?;
        path_indices.push(src_idx);
        // Validate total hops
        let hops = path_indices.len() - 1;
        if hops > MAX_HOPS || hops < 2 {
            return None;
        }
        let path_mints: Vec<Pubkey> = path_indices.iter().map(|&i| nodes[i]).collect();
        return build_cycle(path_mints, nodes, edges);
    }

    let hops = path_indices.len() - 1;
    if hops > MAX_HOPS || hops < 2 {
        return None;
    }

    let path_mints: Vec<Pubkey> = path_indices.iter().map(|&i| nodes[i]).collect();
    build_cycle(path_mints, nodes, edges)
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

    /// Build a zero-fee constant-product pool for clean, predictable weights.
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
            fee_bps: AtomicU64::new(0), // zero fee for exact arithmetic
            sqrt_price_x64: AtomicU64::new(0),
            state_account: None,
            extra: PoolExtra::default(),
        })
    }

    // ─── no cycle ─────────────────────────────────────────────────────────────

    #[test]
    fn single_pool_has_no_cycle() {
        // Two nodes (SOL, USDC) form no profitable cycle on their own.
        let g = ExchangeGraph::new();
        let sol = sol();
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
        // Querying from a node that is not connected returns nothing
        let unknown = Pubkey::new_unique();
        assert!(find_negative_cycles(&g, unknown).is_empty());
    }

    #[test]
    fn unprofitable_3hop_cycle_not_detected() {
        // Pool 3 gives back 10 % LESS than we put in → total cycle rate < 1.
        // rate(SOL→USDC) = 0.1, rate(USDC→RAY) = 10.0, rate(RAY→SOL) = 0.9
        // product = 0.1 * 10.0 * 0.9 = 0.9 < 1.0 → no profit
        let g    = ExchangeGraph::new();
        let sol  = sol();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();
        g.update_pool(&pool(sol,  usdc, 1_000_000, 100_000));
        g.update_pool(&pool(usdc, ray,  100_000, 1_000_000));
        g.update_pool(&pool(ray,  sol,  1_000_000, 900_000)); // 10 % discount
        assert!(find_negative_cycles(&g, sol).is_empty());
    }

    // ─── profitable cycle ─────────────────────────────────────────────────────

    #[test]
    fn profitable_3hop_cycle_is_detected() {
        // rate(SOL→USDC) = 0.1, rate(USDC→RAY) = 10.0, rate(RAY→SOL) = 1.1
        // product = 0.1 * 10.0 * 1.1 = 1.1 → 10 % gross profit
        let g    = ExchangeGraph::new();
        let sol  = sol();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();
        g.update_pool(&pool(sol,  usdc, 1_000_000, 100_000));
        g.update_pool(&pool(usdc, ray,  100_000, 1_000_000));
        g.update_pool(&pool(ray,  sol,  1_000_000, 1_100_000)); // 10 % premium

        let cycles = find_negative_cycles(&g, sol);
        assert!(!cycles.is_empty(), "expected a profitable cycle");

        let c = &cycles[0];
        assert_eq!(c.path.first(), Some(&sol), "cycle must start at SOL");
        assert_eq!(c.path.last(),  Some(&sol), "cycle must end at SOL");
        assert!(c.total_weight < 0.0, "total_weight must be negative, got {}", c.total_weight);
        assert!(c.gross_ratio() > 1.0, "gross_ratio must exceed 1.0, got {}", c.gross_ratio());
    }

    #[test]
    fn cycle_disconnected_from_source_is_ignored() {
        // A→B→C→A is profitable, but SOL is not part of that cycle.
        let g   = ExchangeGraph::new();
        let sol = sol();
        let a   = Pubkey::new_unique();
        let b   = Pubkey::new_unique();
        let c   = Pubkey::new_unique();
        // Extremely profitable cycle not involving SOL
        g.update_pool(&pool(a, b, 1_000, 10_000));
        g.update_pool(&pool(b, c, 1_000, 10_000));
        g.update_pool(&pool(c, a, 1_000, 10_000));
        // SOL→A only (no path back to SOL)
        g.update_pool(&pool(sol, a, 1_000_000, 1_000_000));
        assert!(find_negative_cycles(&g, sol).is_empty());
    }
}

fn build_cycle(path_mints: Vec<Pubkey>, _nodes: &[Pubkey], edges: &[Edge]) -> Option<ArbCycle> {
    let mut cycle_edges = Vec::new();
    let mut total_weight = 0.0;

    for window in path_mints.windows(2) {
        let from = window[0];
        let to = window[1];
        let edge = edges.iter().find(|e| e.from == from && e.to == to)?;
        total_weight += edge.weight;
        cycle_edges.push(edge.clone());
    }

    // Only return if genuinely negative (profitable)
    if total_weight >= 0.0 {
        return None;
    }

    Some(ArbCycle { path: path_mints, edges: cycle_edges, total_weight })
}
