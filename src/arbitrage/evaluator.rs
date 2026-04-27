use anyhow::Result;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey, system_instruction};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};
use std::sync::Arc;

use crate::config::Config;
use crate::dex::{PoolRegistry, meteora, orca, raydium_amm, raydium_clmm};
use crate::dex::types::{DexKind, WSOL_MINT};
use crate::graph::bellman_ford::ArbCycle;
use crate::arbitrage::opportunity::ArbOpportunity;
use tracing::{debug, warn, info};

const BASE_FEE_PER_TX: u64 = 5_000;
const NUM_TXS: u64 = 4; // 3 swaps + 1 tip tx

fn optimize_and_evaluate(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    amount_in: u64,
) -> Option<ArbOpportunity> {
    let path_str = cycle.edges.iter()
        .map(|e| e.from.to_string()[..6].to_string())
        .chain(std::iter::once(cycle.edges.last()?.to.to_string()[..6].to_string()))
        .collect::<Vec<_>>()
        .join("→");

    let gross_ratio = cycle.gross_ratio();
    debug!("Evaluating cycle {} hops gross_ratio={:.6}", cycle.edges.len(), gross_ratio);

    // A gross_ratio > 5 (500%) is always phantom: real arbitrage is 0.01–2%.
    // Evaluating it wastes RPC budget on quote chains that will definitely fail.
    // The threshold is conservative — even a 100% gross cycle won't survive
    // DEX fees and price impact once the actual quote is computed.
    const MAX_GROSS_RATIO: f64 = 5.0;
    if gross_ratio > MAX_GROSS_RATIO {
        debug!(
            "Cycle {path_str}: skipped — gross_ratio={gross_ratio:.2} exceeds sanity cap {MAX_GROSS_RATIO} (phantom pool pricing)"
        );
        return None;
    }

    if amount_in == 0 {
        debug!("Cycle {path_str}: skipped — amount_in=0");
        return None;
    }

    let hops = cycle.edges.len();
    if hops < 2 || hops > 3 {
        debug!("Cycle {path_str}: skipped — unsupported hop count {hops}");
        return None;
    }

    let mut current_amount = amount_in;
    let mut total_swap_fee_lamports = 0u64;
    let mut swap_instructions: Vec<Instruction> = Vec::with_capacity(hops);
    let mut minimum_outputs: Vec<u64> = Vec::with_capacity(hops);

    for (i, edge) in cycle.edges.iter().enumerate() {
        let pool = match registry.get_by_pool_id(&edge.pool_id) {
            Some(p) => p,
            None => {
                debug!("Cycle {path_str}: hop {i} — pool {} not in registry", &edge.pool_id.to_string()[..6]);
                return None;
            }
        };
        let a_to_b = edge.a_to_b;

        let quote = match pool.dex {
            DexKind::RaydiumAmmV4  => raydium_amm::get_quote(&pool, current_amount, a_to_b),
            DexKind::RaydiumClmm   => raydium_clmm::get_quote(&pool, current_amount, a_to_b),
            DexKind::OrcaWhirlpool => orca::get_quote(&pool, current_amount, a_to_b),
            DexKind::MeteoraDamm   => meteora::get_quote(&pool, current_amount, a_to_b),
        };

        let impact_bps = (quote.price_impact * 10_000.0) as u64;
        debug!(
            "Cycle {path_str}: hop {i} in={} out={} fee={} impact={:.4}% ({impact_bps} bps)",
            quote.amount_in, quote.amount_out, quote.fee_amount, quote.price_impact * 100.0
        );

        if quote.amount_out == 0 {
            debug!("Cycle {path_str}: hop {i} — zero output, skipping");
            return None;
        }

        // Reject if price impact per hop exceeds the configured maximum.
        // High impact means the pool is too small relative to the trade size —
        // the marginal rate the graph used was correct but the actual fill is terrible.
        if impact_bps >= config.max_price_impact_bps {
            debug!(
                "Cycle {path_str}: hop {i} — price impact {impact_bps} bps ≥ max {} bps (pool too small for trade size)",
                config.max_price_impact_bps
            );
            return None;
        }

        total_swap_fee_lamports += quote.fee_amount;

        let min_out = apply_slippage(quote.amount_out, config.slippage_bps);
        minimum_outputs.push(min_out);

        // Resolve the user's Associated Token Accounts for this hop.
        // cycle.path contains the mint sequence: [SOL, X, Y, SOL].
        // For hop i: from_mint = path[i], to_mint = path[i+1].
        // ATA derivation is deterministic (no RPC) so this is free.
        let from_mint = cycle.path[i];
        let to_mint   = cycle.path[i + 1];
        let user_src  = get_associated_token_address(&user, &from_mint);
        let user_dst  = get_associated_token_address(&user, &to_mint);

        let ix = build_swap_ix(&pool, user_src, user_dst, user, current_amount, min_out, a_to_b).ok()?;
        swap_instructions.push(ix);

        current_amount = quote.amount_out;
    }

    let gross_out = current_amount;
    let tx_fee = BASE_FEE_PER_TX * NUM_TXS;
    let gross_profit = (gross_out as i64) - (amount_in as i64) - (tx_fee as i64);

    debug!(
        "Cycle {path_str}: amount_in={} gross_out={} tx_fee={} gross_profit={}",
        amount_in, gross_out, tx_fee, gross_profit
    );

    if gross_profit <= 0 {
        info!(
            "Cycle {path_str}: unprofitable — gross_out={} amount_in={} gross_profit={} (tx_fee={} swap_fees={})",
            gross_out, amount_in, gross_profit, tx_fee, total_swap_fee_lamports
        );
        return None;
    }

    let jito_tip = compute_jito_tip(gross_profit as u64, config);
    let net_profit = gross_profit - (jito_tip as i64);

    debug!(
        "Cycle {path_str}: jito_tip={} net_profit={} min_required={}",
        jito_tip, net_profit, config.min_profit_lamports
    );

    if net_profit <= 0 || net_profit < config.min_profit_lamports as i64 {
        warn!(
            "Cycle {path_str}: below threshold — net_profit={} min={} (gross_profit={} tip={})",
            net_profit, config.min_profit_lamports, gross_profit, jito_tip
        );
        return None;
    }

    Some(ArbOpportunity {
        cycle: cycle.clone(),
        amount_in,
        gross_out,
        total_swap_fee_lamports,
        tx_fee_lamports: tx_fee,
        jito_tip_lamports: jito_tip,
        net_profit_lamports: net_profit,
        swap_instructions,
        minimum_outputs,
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
    use std::str::FromStr;
    let wsol = Pubkey::from_str(WSOL_MINT).expect("WSOL_MINT is a valid pubkey");
    let wsol_ata = get_associated_token_address(&user, &wsol);

    let mut ixs: Vec<Instruction> = Vec::new();

    // Create ATAs for all non-WSOL intermediate mints (idempotent — no-op if exists)
    let mut seen = std::collections::HashSet::new();
    for &mint in path {
        if mint != wsol && seen.insert(mint) {
            ixs.push(create_associated_token_account_idempotent(
                &user, &user, &mint, &spl_token::id(),
            ));
        }
    }

    // Create (or verify) WSOL ATA
    ixs.push(create_associated_token_account_idempotent(
        &user, &user, &wsol, &spl_token::id(),
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
    use std::str::FromStr;
    let wsol = Pubkey::from_str(WSOL_MINT).expect("WSOL_MINT is a valid pubkey");
    let wsol_ata = get_associated_token_address(&user, &wsol);
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
            orca::build_swap_instruction(pool, user, user_src, user_dst, amount_in, min_out, 0, true, a_to_b)
        }
        DexKind::MeteoraDamm => {
            meteora::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
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
    use std::sync::atomic::AtomicU64;
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
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
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

// ─── User Contribution Point ─────────────────────────────────────────────────
//
// Implement this function to find the input amount that maximises net_profit.
//
// Trade-offs:
//   - Larger amount_in → higher absolute profit but steeper price impact
//   - Higher tip_ratio → better landing probability but smaller net profit
//
// Hint: call optimize_and_evaluate() at several candidate fractions of
// available_sol and pick the amount_in with the highest net_profit_lamports.
#[allow(dead_code)]
pub fn optimize_input_and_tip(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    available_sol: u64,
) -> Option<ArbOpportunity> {
    // TODO: replace with binary-search or multi-candidate sweep
    let capped = config.input_sol_lamports.min(available_sol);
    optimize_and_evaluate(cycle, registry, config, user, capped)
}
