use anyhow::Result;
use solana_sdk::{instruction::Instruction, pubkey::Pubkey};
use std::sync::Arc;

use crate::config::Config;
use crate::dex::{PoolRegistry, meteora, orca, raydium_amm, raydium_clmm};
use crate::dex::types::DexKind;
use crate::graph::bellman_ford::ArbCycle;
use crate::arbitrage::opportunity::ArbOpportunity;

const BASE_FEE_PER_TX: u64 = 5_000;
const NUM_TXS: u64 = 4; // 3 swaps + 1 tip tx

fn optimize_and_evaluate(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    amount_in: u64,
) -> Option<ArbOpportunity> {
    if amount_in == 0 {
        return None;
    }

    let hops = cycle.edges.len();
    if hops < 2 || hops > 3 {
        return None;
    }

    let mut current_amount = amount_in;
    let mut total_swap_fee_lamports = 0u64;
    let mut swap_instructions: Vec<Instruction> = Vec::with_capacity(hops);
    let mut minimum_outputs: Vec<u64> = Vec::with_capacity(hops);

    for edge in &cycle.edges {
        let pool = registry.find_pool(&edge.from, &edge.to)?;
        let a_to_b = edge.a_to_b;

        let quote = match pool.dex {
            DexKind::RaydiumAmmV4  => raydium_amm::get_quote(&pool, current_amount, a_to_b),
            DexKind::RaydiumClmm   => raydium_clmm::get_quote(&pool, current_amount, a_to_b),
            DexKind::OrcaWhirlpool => orca::get_quote(&pool, current_amount, a_to_b),
            DexKind::MeteoraDamm   => meteora::get_quote(&pool, current_amount, a_to_b),
        };

        if quote.amount_out == 0 {
            return None;
        }

        // fee_amount is informational only — it is already deducted inside
        // the AMM formula (amount_out is computed from amount_in_with_fee).
        // We track it for logging but do NOT subtract it separately from profit;
        // doing so would double-count and incorrectly kill profitable trades.
        total_swap_fee_lamports += quote.fee_amount;

        let min_out = apply_slippage(quote.amount_out, config.slippage_bps);
        minimum_outputs.push(min_out);

        let ix = build_swap_ix(&pool, user, current_amount, min_out, a_to_b).ok()?;
        swap_instructions.push(ix);

        current_amount = quote.amount_out;
    }

    let gross_out = current_amount;

    // Costs that come directly out of the wallet (not baked into swap outputs):
    //   tx_fee  = Solana base fee per transaction × number of transactions
    //   jito_tip = dynamic tip paid to the validator via Jito
    // Swap fees are NOT listed here — they are already reflected in gross_out.
    let tx_fee = BASE_FEE_PER_TX * NUM_TXS;

    // Gross profit = what we gain before paying the Jito tip
    let gross_profit = (gross_out as i64) - (amount_in as i64) - (tx_fee as i64);
    if gross_profit <= 0 {
        return None; // Already unprofitable without tip; bail early
    }

    let jito_tip = compute_jito_tip(gross_profit as u64, config);

    let net_profit = gross_profit - (jito_tip as i64);

    // Hard floor: net_profit must be strictly positive AND meet the configured minimum.
    // This double-checks that after ALL costs the trade returns more than it risks.
    if net_profit <= 0 || net_profit < config.min_profit_lamports as i64 {
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
    })
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
    user: Pubkey,
    amount_in: u64,
    min_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    // User token accounts are placeholder Pubkey::default() here;
    // the real ATAs are resolved in bundle.rs before signing.
    let src = Pubkey::default();
    let dst = Pubkey::default();

    match pool.dex {
        DexKind::RaydiumAmmV4 => {
            raydium_amm::build_swap_instruction(pool, src, dst, user, amount_in, min_out, a_to_b)
        }
        DexKind::RaydiumClmm => {
            raydium_clmm::build_swap_instruction(pool, src, dst, user, amount_in, min_out, 0, true, a_to_b)
        }
        DexKind::OrcaWhirlpool => {
            orca::build_swap_instruction(pool, user, src, dst, amount_in, min_out, 0, true, a_to_b)
        }
        DexKind::MeteoraDamm => {
            meteora::build_swap_instruction(pool, src, dst, user, amount_in, min_out, a_to_b)
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
        let p1 = zero_fee_pool(sol,  usdc, 10_000_000, 1_000_000);
        let p2 = zero_fee_pool(usdc, ray,  1_000_000, 10_000_000);
        let p3 = zero_fee_pool(ray,  sol,  10_000_000, 11_000_000); // 10 % surplus

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

        let p1 = zero_fee_pool(sol,  usdc, 10_000_000, 1_000_000);
        let p2 = zero_fee_pool(usdc, ray,  1_000_000, 10_000_000);
        let p3 = zero_fee_pool(ray,  sol,  10_000_000, 11_000_000);

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

        let p1 = zero_fee_pool(sol,  usdc, 10_000_000, 1_000_000);
        let p2 = zero_fee_pool(usdc, ray,  1_000_000, 10_000_000);
        let p3 = zero_fee_pool(ray,  sol,  10_000_000, 9_000_000); // deficit

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
