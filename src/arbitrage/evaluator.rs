use anyhow::Result;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, system_instruction};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};
use std::sync::Arc;

use crate::config::Config;
use crate::dex::{PoolRegistry, dlmm, meteora, orca, phoenix, raydium_amm, raydium_clmm};
use crate::dex::types::{DexKind, Pool, WSOL_PUBKEY};
use crate::graph::bellman_ford::ArbCycle;
use crate::arbitrage::opportunity::ArbOpportunity;
use tracing::{debug, trace, warn};

const BASE_FEE_PER_TX: u64 = 5_000;
const MAX_GROSS_RATIO: f64 = 1.10;
const MAX_ACTUAL_GROSS_RATIO: f64 = 1.10;

/// Result of chaining quotes through all cycle hops for a specific amount_in.
/// Carries everything needed to build swap instructions without re-running AMM math.
struct QuoteResult {
    gross_out: u64,
    total_swap_fee: u64,
    tx_fee: u64,
    jito_tip: u64,
    net_profit: i64,
    /// amount_in for each hop: hop_in_amounts[0] = amount_in, hop_in_amounts[i+1] = out of hop i
    hop_in_amounts: Vec<u64>,
    /// post-slippage minimum output for each hop
    hop_min_outs: Vec<u64>,
}

/// Run the quote chain for `amount_in` and return gross_out/amount_in.
/// Only checks that the chain completes — does NOT gate on profitability.
/// Returns None if any hop produces zero output, hits price impact, or triggers the sanity cap.
fn probe_gross_ratio(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    config: &Config,
    amount_in: u64,
) -> Option<f64> {
    let mut current = amount_in;
    for (edge, pool) in cycle.edges.iter().zip(pools.iter()) {
        let q = match pool.dex {
            DexKind::RaydiumAmmV4  => raydium_amm::get_quote(pool, current, edge.a_to_b),
            DexKind::RaydiumClmm   => raydium_clmm::get_quote(pool, current, edge.a_to_b),
            DexKind::OrcaWhirlpool => orca::get_quote(pool, current, edge.a_to_b),
            DexKind::MeteoraDamm   => meteora::get_quote(pool, current, edge.a_to_b),
            DexKind::MeteoraDlmm   => dlmm::get_quote(pool, current, edge.a_to_b),
            DexKind::Phoenix       => phoenix::get_quote(pool, current, edge.a_to_b),
        };
        if q.amount_out == 0 { return None; }
        if (q.price_impact * 10_000.0) as u64 >= config.max_price_impact_bps { return None; }
        current = q.amount_out;
    }
    if current as f64 > amount_in as f64 * MAX_ACTUAL_GROSS_RATIO { return None; }
    Some(current as f64 / amount_in as f64)
}

/// Chain AMM quotes through cycle.edges for the given amount_in.
/// Returns None if any hop produces zero output, exceeds price impact, or the cycle is unprofitable.
/// Does NOT build swap instructions — used in pass 1 of optimize_input_and_tip.
///
/// Takes pre-fetched `pools` (one per hop, already looked up from the registry once)
/// instead of doing a DashMap lookup per hop per fraction.
fn evaluate_quotes(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    config: &Config,
    amount_in: u64,
) -> Option<QuoteResult> {
    let hops = cycle.edges.len();
    let mut current_amount = amount_in;
    let mut total_swap_fee = 0u64;
    let mut hop_in_amounts = Vec::with_capacity(hops);
    let mut hop_min_outs = Vec::with_capacity(hops);

    for (hop_idx, (edge, pool)) in cycle.edges.iter().zip(pools.iter()).enumerate() {
        let quote = match pool.dex {
            DexKind::RaydiumAmmV4  => raydium_amm::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::RaydiumClmm   => raydium_clmm::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::OrcaWhirlpool => orca::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::MeteoraDamm   => meteora::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::MeteoraDlmm   => dlmm::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::Phoenix       => phoenix::get_quote(&pool, current_amount, edge.a_to_b),
        };

        if quote.amount_out == 0 {
            trace!(
                amount_in, hop = hop_idx, dex = pool.dex.short_name(),
                pool = &pool.id.to_string()[..8],
                "fraction rejected: hop zero output",
            );
            return None;
        }

        let impact_bps = (quote.price_impact * 10_000.0) as u64;
        if impact_bps >= config.max_price_impact_bps {
            trace!(
                amount_in, hop = hop_idx, dex = pool.dex.short_name(),
                pool = &pool.id.to_string()[..8],
                impact_bps, threshold = config.max_price_impact_bps,
                "fraction rejected: price impact",
            );
            return None;
        }

        hop_in_amounts.push(current_amount);
        total_swap_fee += quote.fee_amount;
        hop_min_outs.push(apply_slippage(quote.amount_out, config.slippage_bps));
        current_amount = quote.amount_out;
    }

    let gross_out = current_amount;

    if gross_out as f64 > amount_in as f64 * MAX_ACTUAL_GROSS_RATIO {
        warn!(
            "Quoted gross_out={gross_out} from amount_in={amount_in} (ratio={:.4}) exceeds sanity cap — phantom CLMM vault skew, skipping",
            gross_out as f64 / amount_in as f64,
        );
        return None;
    }

    let num_swap_txs = hops as u64;
    let cu_fee = config.compute_unit_limit * config.compute_unit_price_micro_lamports / 1_000_000;
    let tx_fee = BASE_FEE_PER_TX * (num_swap_txs + 1) + cu_fee * num_swap_txs;
    let gross_profit = (gross_out as i64) - (amount_in as i64) - (tx_fee as i64);
    if gross_profit <= 0 {
        trace!(
            amount_in, gross_out, tx_fee,
            gross_bps = (gross_out as f64 / amount_in as f64 - 1.0) * 10_000.0,
            "fraction rejected: gross_profit={gross_profit} (fees ate the margin)",
        );
        return None;
    }

    let jito_tip = compute_jito_tip(gross_profit as u64, config);
    let net_profit = gross_profit - jito_tip as i64;
    if net_profit <= 0 || net_profit < config.min_profit_lamports as i64 {
        trace!(
            amount_in, gross_profit, jito_tip, net_profit,
            min = config.min_profit_lamports,
            "fraction rejected: net_profit below threshold",
        );
        return None;
    }

    Some(QuoteResult { gross_out, total_swap_fee, tx_fee, jito_tip, net_profit, hop_in_amounts, hop_min_outs })
}

/// Build swap instructions using pre-computed quote data.
/// Called only for the winning fraction — avoids instruction building for discarded candidates.
fn build_opportunity(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    user: Pubkey,
    amount_in: u64,
    quote: QuoteResult,
) -> Option<ArbOpportunity> {
    let hops = cycle.edges.len();
    let mut swap_instructions = Vec::with_capacity(hops);

    for (i, (edge, pool)) in cycle.edges.iter().zip(pools.iter()).enumerate() {
        let user_src = get_associated_token_address(&user, &cycle.path[i]);
        let user_dst = get_associated_token_address(&user, &cycle.path[i + 1]);
        let ix = build_swap_ix(
            pool, user_src, user_dst, user,
            quote.hop_in_amounts[i], quote.hop_min_outs[i],
            edge.a_to_b,
        ).ok()?;
        swap_instructions.push(ix);
    }

    Some(ArbOpportunity {
        cycle: cycle.clone(),
        amount_in,
        gross_out: quote.gross_out,
        total_swap_fee_lamports: quote.total_swap_fee,
        tx_fee_lamports: quote.tx_fee,
        jito_tip_lamports: quote.jito_tip,
        net_profit_lamports: quote.net_profit,
        swap_instructions,
        minimum_outputs: quote.hop_min_outs,
        setup_instructions: build_setup_instructions(user, amount_in, &cycle.path),
        teardown_instructions: build_teardown_instructions(user),
    })
}

/// Build setup instructions for tx[0]:
///   1. create_associated_token_account_idempotent for each non-WSOL mint in cycle
///   2. create_associated_token_account_idempotent for WSOL itself
///   3. system transfer: user → WSOL ATA (fund the wrap)
///   4. sync_native: tell token program the WSOL ATA was topped up
fn build_setup_instructions(user: Pubkey, amount_in: u64, path: &[Pubkey]) -> Vec<Instruction> {
    let wsol_ata = get_associated_token_address(&user, &WSOL_PUBKEY);

    let mut ixs: Vec<Instruction> = Vec::new();

    // Create ATAs for all non-WSOL intermediate mints (idempotent — no-op if exists)
    let mut seen = std::collections::HashSet::new();
    for &mint in path {
        if mint != WSOL_PUBKEY && seen.insert(mint) {
            ixs.push(create_associated_token_account_idempotent(
                &user, &user, &mint, &spl_token::id(),
            ));
        }
    }

    // Create (or verify) WSOL ATA
    ixs.push(create_associated_token_account_idempotent(
        &user, &user, &WSOL_PUBKEY, &spl_token::id(),
    ));

    // Fund the WSOL ATA with the arb input amount
    ixs.push(system_instruction::transfer(&user, &wsol_ata, amount_in));

    // Sync the native balance so the token program sees the deposited lamports as WSOL
    ixs.push(
        spl_token::instruction::sync_native(&spl_token::id(), &wsol_ata)
            .expect("sync_native is always valid"),
    );

    ixs
}

/// Build teardown instructions appended to the last swap tx:
///   close the WSOL ATA — converts all remaining WSOL lamports back to SOL in the user's account.
fn build_teardown_instructions(user: Pubkey) -> Vec<Instruction> {
    let wsol_ata = get_associated_token_address(&user, &WSOL_PUBKEY);
    vec![
        spl_token::instruction::close_account(&spl_token::id(), &wsol_ata, &user, &user, &[])
            .expect("close_account is always valid"),
    ]
}

/// Apply slippage tolerance to a quote amount, returning the minimum acceptable output.
/// Uses u128 for intermediate math to prevent overflow.
fn apply_slippage(amount: u64, slippage_bps: u64) -> u64 {
    let reduction = (amount as u128 * slippage_bps as u128 / 10_000) as u64;
    amount.saturating_sub(reduction)
}

fn compute_jito_tip(gross_profit: u64, config: &Config) -> u64 {
    const MIN_TIP: u64 = 1_000;
    let tip = (gross_profit as f64 * config.tip_ratio) as u64;
    tip.clamp(MIN_TIP, config.max_tip_lamports)
}

fn build_swap_ix(
    pool: &Arc<crate::dex::types::Pool>,
    user_src: Pubkey,
    user_dst: Pubkey,
    user: Pubkey,
    amount_in: u64,
    min_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    match pool.dex {
        DexKind::RaydiumAmmV4 => {
            raydium_amm::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
        DexKind::RaydiumClmm => {
            raydium_clmm::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, 0, true, a_to_b)
        }
        DexKind::OrcaWhirlpool => {
            // Orca expects token accounts in fixed canonical (token_a, token_b) order
            // regardless of swap direction; direction is encoded in the instruction data.
            let (account_a, account_b) = if a_to_b { (user_src, user_dst) } else { (user_dst, user_src) };
            orca::build_swap_instruction(pool, user, account_a, account_b, amount_in, min_out, 0, true, a_to_b)
        }
        DexKind::MeteoraDamm => {
            meteora::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
        DexKind::MeteoraDlmm => {
            dlmm::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
        DexKind::Phoenix => {
            phoenix::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::dex::types::{DexKind, Pool, PoolExtra, WSOL_MINT};
    use crate::dex::PoolRegistry;
    use crate::graph::bellman_ford::find_negative_cycles;
    use crate::graph::exchange_graph::ExchangeGraph;
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;
    use std::sync::atomic::{AtomicI32, AtomicU64};
    use std::sync::Arc;

    fn test_config() -> Config {
        Config {
            grpc_endpoint: String::new(),
            grpc_token: None,
            wallet_keypair_path: String::new(),
            rpc_url: String::new(),
            pools_config_path: String::new(),
            min_profit_lamports: 1_000,
            input_sol_lamports: 100_000_000,
            slippage_bps: 50,
            tip_ratio: 0.5,
            max_tip_lamports: 1_000_000,
            dry_run: false,
            bellman_ford_debounce_ms: 10,
            max_price_impact_bps: 10_000, // no impact cap in tests (pools are tiny by design)
            compute_unit_limit: 600_000,
            compute_unit_price_micro_lamports: 1_000,
            log_cycle_threshold_bps: 0.0,
        }
    }

    fn zero_fee_pool(token_a: Pubkey, token_b: Pubkey, reserve_a: u64, reserve_b: u64) -> Arc<Pool> {
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
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
            stable: false,
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }

    // ─── apply_slippage ───────────────────────────────────────────────────────

    #[test]
    fn slippage_zero_bps_is_identity() {
        assert_eq!(apply_slippage(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn slippage_100_bps_reduces_by_one_pct() {
        assert_eq!(apply_slippage(1_000_000, 100), 990_000);
    }

    #[test]
    fn slippage_50_bps_reduces_by_half_pct() {
        assert_eq!(apply_slippage(1_000_000, 50), 995_000);
    }

    #[test]
    fn slippage_never_overflows_with_max_u64() {
        // With 50 bps and u64::MAX, intermediate u128 must be used.
        let result = apply_slippage(u64::MAX, 50);
        assert!(result < u64::MAX, "result must be less than input");
    }

    #[test]
    fn slippage_result_never_exceeds_input() {
        for bps in [0u64, 1, 50, 100, 500, 10_000] {
            let result = apply_slippage(999_999, bps);
            assert!(result <= 999_999, "bps={bps}: result {result} exceeded input");
        }
    }

    // ─── compute_jito_tip ─────────────────────────────────────────────────────

    #[test]
    fn tip_clamps_to_min_when_profit_is_tiny() {
        let config = test_config(); // max_tip = 1_000_000, ratio = 0.5
        // 10 * 0.5 = 5 → below MIN_TIP of 1_000
        assert_eq!(compute_jito_tip(10, &config), 1_000);
    }

    #[test]
    fn tip_is_ratio_of_profit_in_normal_range() {
        let config = test_config();
        // 400_000 * 0.5 = 200_000, within [1_000, 1_000_000]
        assert_eq!(compute_jito_tip(400_000, &config), 200_000);
    }

    #[test]
    fn tip_clamps_to_max_when_profit_is_large() {
        let config = test_config();
        // 10_000_000 * 0.5 = 5_000_000 → clamped to max_tip = 1_000_000
        assert_eq!(compute_jito_tip(10_000_000, &config), 1_000_000);
    }

    // ─── profit accounting identity ───────────────────────────────────────────

    /// Core invariant: the `net_profit_lamports` stored in every ArbOpportunity
    /// must equal the arithmetic sum of all wallet-level costs.
    /// This verifies there is no double-counting of swap fees.
    #[test]
    fn net_profit_equals_gross_out_minus_wallet_costs() {
        let sol  = Pubkey::from_str(WSOL_MINT).unwrap();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();

        // 3-hop profitable cycle: 10 % gross profit, zero swap fees
        let p1 = zero_fee_pool(sol,  usdc, 20_000_000_000, 2_000_000_000);
        let p2 = zero_fee_pool(usdc, ray,  2_000_000_000, 20_000_000_000);
        let p3 = zero_fee_pool(ray,  sol,  20_000_000_000, 22_000_000_000); // 10 % surplus

        let registry = PoolRegistry::from_pools(vec![
            Arc::clone(&p1), Arc::clone(&p2), Arc::clone(&p3),
        ]);
        let config = test_config();

        // Build the cycle via the same Bellman-Ford path the real bot uses
        let graph = ExchangeGraph::new();
        graph.update_pool(&p1);
        graph.update_pool(&p2);
        graph.update_pool(&p3);
        let cycles = find_negative_cycles(&graph, sol);
        assert!(!cycles.is_empty(), "test setup must produce a profitable cycle");

        for cycle in &cycles {
            if let Some(opp) = optimize_input_and_tip(cycle, &registry, &config, sol, config.input_sol_lamports) {
                // 1. Net profit must be strictly positive
                assert!(opp.net_profit_lamports > 0, "net_profit must be > 0");

                // 2. Net profit must meet the configured minimum
                assert!(
                    opp.net_profit_lamports >= config.min_profit_lamports as i64,
                    "net_profit {} below minimum {}",
                    opp.net_profit_lamports, config.min_profit_lamports
                );

                // 3. The accounting identity (no hidden costs, no double-counted fees):
                //    net_profit == gross_out - amount_in - tx_fee - jito_tip
                //    Swap fees are NOT subtracted separately — they are already
                //    reflected in gross_out (baked into each AMM quote).
                let expected = opp.gross_out as i64
                    - opp.amount_in as i64
                    - opp.tx_fee_lamports as i64
                    - opp.jito_tip_lamports as i64;
                assert_eq!(
                    opp.net_profit_lamports, expected,
                    "accounting identity broken: net_profit={} expected={}",
                    opp.net_profit_lamports, expected
                );
            }
        }
    }

    #[test]
    fn zero_amount_in_returns_none() {
        let sol  = Pubkey::from_str(WSOL_MINT).unwrap();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();

        let p1 = zero_fee_pool(sol,  usdc, 20_000_000_000, 2_000_000_000);
        let p2 = zero_fee_pool(usdc, ray,  2_000_000_000, 20_000_000_000);
        let p3 = zero_fee_pool(ray,  sol,  20_000_000_000, 22_000_000_000);

        let registry = PoolRegistry::from_pools(vec![Arc::clone(&p1), Arc::clone(&p2), Arc::clone(&p3)]);
        let config   = test_config();
        let graph    = ExchangeGraph::new();
        graph.update_pool(&p1);
        graph.update_pool(&p2);
        graph.update_pool(&p3);

        for cycle in find_negative_cycles(&graph, sol) {
            let result = optimize_input_and_tip(&cycle, &registry, &config, sol, 0);
            assert!(result.is_none(), "zero available_sol must return None");
        }
    }

    #[test]
    fn unprofitable_cycle_returns_none() {
        // Pool 3 has a 10 % deficit → gross_profit < 0 → must return None
        let sol  = Pubkey::from_str(WSOL_MINT).unwrap();
        let usdc = Pubkey::new_unique();
        let ray  = Pubkey::new_unique();

        let p1 = zero_fee_pool(sol,  usdc, 20_000_000_000, 2_000_000_000);
        let p2 = zero_fee_pool(usdc, ray,  2_000_000_000, 20_000_000_000);
        let p3 = zero_fee_pool(ray,  sol,  20_000_000_000, 18_000_000_000); // deficit

        let registry = PoolRegistry::from_pools(vec![Arc::clone(&p1), Arc::clone(&p2), Arc::clone(&p3)]);
        let config   = test_config();
        let graph    = ExchangeGraph::new();
        graph.update_pool(&p1);
        graph.update_pool(&p2);
        graph.update_pool(&p3);

        // Bellman-Ford should not detect this cycle at all, but even if somehow
        // an ArbCycle is constructed manually, the evaluator must still reject it.
        for cycle in find_negative_cycles(&graph, sol) {
            let result = optimize_input_and_tip(&cycle, &registry, &config, sol, u64::MAX);
            assert!(result.is_none(), "unprofitable cycle must return None");
        }
    }
}

pub fn optimize_input_and_tip(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    available_sol: u64,
) -> Option<ArbOpportunity> {
    // Per-cycle sanity check: MAX_GROSS_RATIO is a property of the cycle, not of
    // amount_in — running it inside the fraction loop would fire up to 5× per cycle.
    let gross_ratio = cycle.gross_ratio();
    if gross_ratio > MAX_GROSS_RATIO {
        let hop_detail: String = cycle.edges.iter().enumerate().map(|(i, e)| {
            let rate = (-e.weight).exp();
            format!(
                "\n    hop {i}: {} -[{}]→ {}  rate={:.6}  pool={}",
                crate::dex::types::mint_symbol(&e.from),
                e.dex.short_name(),
                crate::dex::types::mint_symbol(&e.to),
                rate,
                &e.pool_id.to_string()[..8],
            )
        }).collect();
        warn!(
            "Cycle skipped — gross_ratio={gross_ratio:.4} ({:.1} bps) exceeds sanity cap {MAX_GROSS_RATIO} (phantom pool pricing){hop_detail}",
            (gross_ratio - 1.0) * 10_000.0,
        );
        return None;
    }

    let hops = cycle.edges.len();
    if hops < 2 || hops > 3 { return None; }

    // Cache pool refs once per cycle. Without this, evaluate_quotes would do
    // 3 DashMap lookups per fraction × 5 fractions = 15 lookups per cycle.
    // With caching: 3 lookups total.
    let pools: Vec<Arc<Pool>> = cycle.edges.iter()
        .map(|e| registry.get_by_pool_id(&e.pool_id))
        .collect::<Option<Vec<_>>>()?;

    const FRACTIONS: [f64; 5] = [0.10, 0.25, 0.50, 0.75, 1.00];
    let cap = config.input_sol_lamports.min(available_sol);

    // Pass 1: quote-only sweep — finds the most profitable amount_in without
    // allocating any Instruction or AccountMeta objects.
    let best_result = FRACTIONS.iter()
        .filter_map(|&f| {
            let amount_in = (cap as f64 * f) as u64;
            if amount_in == 0 { return None; }
            evaluate_quotes(cycle, &pools, config, amount_in)
                .map(|q| (amount_in, q))
        })
        .max_by_key(|(_, q)| q.net_profit);

    let (best_amount_in, best_quote) = match best_result {
        Some(r) => r,
        None => {
            // Probe at 50% to surface how close this cycle is to break-even.
            // "Graph says profitable, evaluator says no" usually means fees ate the margin.
            let probe = (cap as f64 * 0.50) as u64;
            if probe > 0 {
                if let Some(ratio) = probe_gross_ratio(cycle, &pools, config, probe) {
                    let gross_bps = (ratio - 1.0) * 10_000.0;
                    let path: String = cycle.path.iter()
                        .map(crate::dex::types::mint_symbol)
                        .collect::<Vec<_>>()
                        .join("→");
                    debug!(
                        "Near-miss [{path}] gross={gross_bps:+.2}bps probe={probe}L — profitable on graph, rejected after fees",
                    );
                }
            }
            return None;
        }
    };

    debug!(
        "Best fraction: amount_in={} gross_out={} net_profit={}",
        best_amount_in, best_quote.gross_out, best_quote.net_profit,
    );

    // Pass 2: build swap instructions only for the winning fraction.
    build_opportunity(cycle, &pools, user, best_amount_in, best_quote)
}
