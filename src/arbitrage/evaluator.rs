use anyhow::Result;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Signature,
    system_instruction,
    transaction::VersionedTransaction,
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::config::Config;
use crate::flash_loan;
use crate::dex::{PoolRegistry, dlmm, invariant, jupiter, lifinity, meteora, orca, phoenix, raydium_amm, raydium_clmm, saber};
use crate::dex::types::{DexKind, Pool, WSOL_PUBKEY};
use crate::graph::bellman_ford::ArbCycle;
use crate::arbitrage::opportunity::ArbOpportunity;
use tracing::{debug, info, trace, warn};

const BASE_FEE_PER_TX: u64 = 5_000;
const MAX_GROSS_RATIO: f64 = 1.10;
const MAX_ACTUAL_GROSS_RATIO: f64 = 1.10;

/// Per-path rate limiter — prevents the same near-miss from logging more than
/// once every NEAR_MISS_COOLDOWN_SECS seconds regardless of how many BF runs
/// evaluate the same cycle (which can be 50+ per second on busy markets).
static NEAR_MISS_SEEN: OnceLock<Mutex<HashMap<u64, Instant>>> = OnceLock::new();
const NEAR_MISS_COOLDOWN_SECS: u64 = 10;

fn near_miss_path_hash(path: &[Pubkey]) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in path {
        h.write(p.as_ref());
    }
    h.finish()
}

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

/// Walk the quote chain hop-by-hop and return a human-readable string describing the first
/// failure. Used only in the near-miss diagnostic path — not called on the hot path.
///
/// Returns a string like:
///   "hop1: price_impact=1450bps≥500bps [Orca EbvHdZkL EURC→SOL]"
///   "hop0: zero_output [DLMM HTvjzsfX SOL→USDC]"
///   "sanity_cap: gross_ratio=1.43 (phantom CLMM tick)"
fn diagnose_quote_failure(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    config: &Config,
    amount_in: u64,
) -> String {
    use crate::dex::types::mint_symbol;
    let mut current = amount_in;
    for (hop_idx, (edge, pool)) in cycle.edges.iter().zip(pools.iter()).enumerate() {
        let q = match pool.dex {
            DexKind::RaydiumAmmV4  => raydium_amm::get_quote(pool, current, edge.a_to_b),
            DexKind::RaydiumClmm   => raydium_clmm::get_quote(pool, current, edge.a_to_b),
            DexKind::OrcaWhirlpool => orca::get_quote(pool, current, edge.a_to_b),
            DexKind::MeteoraDamm   => meteora::get_quote(pool, current, edge.a_to_b),
            DexKind::MeteoraDlmm   => dlmm::get_quote(pool, current, edge.a_to_b),
            DexKind::Phoenix       => phoenix::get_quote(pool, current, edge.a_to_b),
            DexKind::Lifinity      => lifinity::get_quote(pool, current, edge.a_to_b),
            DexKind::Invariant     => invariant::get_quote(pool, current, edge.a_to_b),
            DexKind::Saber         => saber::get_quote(pool, current, edge.a_to_b),
            DexKind::Jupiter       => jupiter::get_quote(pool, current, edge.a_to_b),
        };
        let pair = format!("{}→{}", mint_symbol(&edge.from), mint_symbol(&edge.to));
        let pool_short = &pool.id.to_string()[..8];
        if q.amount_out == 0 {
            return format!("hop{hop_idx}: zero_output [{} {pool_short} {pair}]", pool.dex.short_name());
        }
        let impact_bps = (q.price_impact * 10_000.0) as u64;
        if impact_bps >= config.max_price_impact_bps {
            return format!(
                "hop{hop_idx}: price_impact={impact_bps}bps≥{}bps [{} {pool_short} {pair}]",
                config.max_price_impact_bps,
                pool.dex.short_name(),
            );
        }
        current = q.amount_out;
    }
    let gross_ratio = current as f64 / amount_in as f64;
    format!("sanity_cap: gross_ratio={gross_ratio:.4} (phantom CLMM tick)")
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
            DexKind::Lifinity      => lifinity::get_quote(pool, current, edge.a_to_b),
            DexKind::Invariant     => invariant::get_quote(pool, current, edge.a_to_b),
            DexKind::Saber         => saber::get_quote(pool, current, edge.a_to_b),
            DexKind::Jupiter       => jupiter::get_quote(pool, current, edge.a_to_b),
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
    tip_floor: u64,
    use_direct: bool,
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
            DexKind::Lifinity      => lifinity::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::Invariant     => invariant::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::Saber         => saber::get_quote(&pool, current_amount, edge.a_to_b),
            DexKind::Jupiter       => jupiter::get_quote(&pool, current_amount, edge.a_to_b),
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

    // All paths (flash loan thin/fat, normal wallet) use 2+ txs: arb tx(s) + Jito tip tx.
    // Thin flash loan cycles (use_direct=true) still go via Jito but with floor-anchored
    // tip only — raw RPC fails with v0+ALT on non-Jito validators (~10% of stake).
    let (num_swap_txs, cu_limit) = if config.enable_flash_loan {
        (1u64, config.compute_unit_limit.max(1_200_000))
    } else {
        (hops as u64, config.compute_unit_limit)
    };
    let cu_fee = cu_limit * config.compute_unit_price_micro_lamports / 1_000_000;
    let tx_fee = BASE_FEE_PER_TX * (num_swap_txs + 1) + cu_fee * num_swap_txs;
    let flash_loan_fee = if config.enable_flash_loan {
        amount_in * flash_loan::FLASH_LOAN_FEE_BPS / 10_000
    } else {
        0
    };
    let gross_profit = (gross_out as i64) - (amount_in as i64) - (tx_fee as i64) - (flash_loan_fee as i64);
    if gross_profit <= 0 {
        trace!(
            amount_in, gross_out, tx_fee,
            gross_bps = (gross_out as f64 / amount_in as f64 - 1.0) * 10_000.0,
            "fraction rejected: gross_profit={gross_profit} (fees ate the margin)",
        );
        return None;
    }

    let (jito_tip, net_profit) = if use_direct {
        // Thin cycle (≤ threshold): floor-anchored tip only. Sent via Jito (not raw RPC)
        // because non-Jito validators can't resolve v0+ALT program accounts reliably.
        // floor_tip ≈ 6_000 lamports vs ratio_tip ≈ 500_000 lamports — keeps 99.5% profit.
        let floor_tip = if tip_floor > 0 && config.tip_floor_multiplier > 0.0 {
            (tip_floor as f64 * config.tip_floor_multiplier) as u64
        } else {
            1_000u64
        };
        let tip = floor_tip.clamp(1_000, config.max_tip_lamports);
        (tip, gross_profit - tip as i64)
    } else {
        let tip = compute_jito_tip(gross_profit as u64, config, tip_floor);
        (tip, gross_profit - tip as i64)
    };
    if net_profit <= 0 || net_profit < config.min_profit_lamports as i64 {
        trace!(
            amount_in, gross_profit, jito_tip, net_profit,
            min = config.min_profit_lamports,
            "fraction rejected: net_profit below threshold",
        );
        return None;
    }
    if config.min_tip_lamports > 0 && jito_tip < config.min_tip_lamports {
        trace!(
            amount_in, jito_tip, min = config.min_tip_lamports,
            "fraction rejected: tip below MIN_TIP_LAMPORTS",
        );
        return None;
    }
    if config.min_tip_floor_multiple > 0.0 && tip_floor > 0 {
        let floor_gate = (tip_floor as f64 * config.min_tip_floor_multiple) as u64;
        if jito_tip < floor_gate {
            trace!(
                amount_in, jito_tip, tip_floor,
                multiple = config.min_tip_floor_multiple,
                floor_gate,
                "fraction rejected: tip below floor × MIN_TIP_FLOOR_MULTIPLE",
            );
            return None;
        }
    }

    Some(QuoteResult { gross_out, total_swap_fee, tx_fee, jito_tip, net_profit, hop_in_amounts, hop_min_outs })
}

/// Ternary search for the amount_in in [lo, hi] that maximises net_profit.
///
/// ## Why the profit curve is concave
///
/// For a constant-product AMM (xy = k), output for input dx is:
///
///   out(x) = y · dx / (x + dx)
///
/// gross_profit(dx) = out(dx) − dx is strictly concave: the marginal gain
/// per extra lamport decreases as dx grows because the pool price moves against
/// you (slippage). The function rises to a single peak, then falls as slippage
/// consumes the gain. CLMM pools (Orca, Raydium CLMM) are piecewise-linear
/// between tick boundaries but concave in aggregate for the same reason.
///
/// ## Implication for wallet sizing
///
/// More capital only increases landing probability while `available_sol` is
/// below the peak. Once the cap exceeds the peak, extra SOL does not help —
/// the ternary search will settle well below the cap and profit stays the same.
/// To diagnose: run with RUST_LOG=solana_mev::arbitrage::evaluator=debug and
/// watch `Best input: amount_in=X`. If X ≈ cap on every profitable cycle,
/// the peak is above the current balance and more capital will directly raise
/// the absolute tip. If X ≪ cap, the balance is already sufficient.
///
/// ## Why ternary search
///
/// For a unimodal function, ternary search halves the uncertainty interval by
/// 1/3 each iteration: after 25 steps the range shrinks to (2/3)^25 ≈ 0.003%
/// of the original, giving lamport-scale precision on a 1 SOL cap. None results
/// (price impact exceeded or zero output) are treated as −∞ so the search
/// naturally stays inside the feasible region without pre-computing its boundaries.
fn ternary_search_net_profit(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    config: &Config,
    mut lo: u64,
    mut hi: u64,
    tip_floor: u64,
    use_direct: bool,
) -> Option<(u64, QuoteResult)> {
    let profit = |x: u64| -> i64 {
        evaluate_quotes(cycle, pools, config, x, tip_floor, use_direct)
            .map(|q| q.net_profit)
            .unwrap_or(i64::MIN / 2)
    };

    for _ in 0..25 {
        if hi <= lo + 2 { break; }
        let third = (hi - lo) / 3;
        let m1 = lo + third;
        let m2 = hi - third;
        if profit(m1) < profit(m2) { lo = m1; } else { hi = m2; }
    }

    let mid = (lo + hi) / 2;
    [lo, mid, hi]
        .iter()
        .filter_map(|&x| evaluate_quotes(cycle, pools, config, x, tip_floor, use_direct).map(|q| (x, q)))
        .max_by_key(|(_, q)| q.net_profit)
}

/// Build swap instructions using pre-computed quote data.
/// Called only for the winning fraction — avoids instruction building for discarded candidates.
fn build_opportunity(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    user: Pubkey,
    amount_in: u64,
    quote: QuoteResult,
    config: &Config,
    alts: &[AddressLookupTableAccount],
    use_direct: bool,
) -> Option<ArbOpportunity> {
    let hops = cycle.edges.len();
    let mut swap_instructions = Vec::with_capacity(hops);
    let mut jupiter_hops: Vec<crate::arbitrage::opportunity::JupiterHopRequest> = Vec::new();

    for (i, (edge, pool)) in cycle.edges.iter().zip(pools.iter()).enumerate() {
        if pool.dex == DexKind::Jupiter {
            // Jupiter hops are resolved asynchronously from /swap-instructions at submit time.
            // Emit a positional placeholder (replaced 1→N by resolve_jupiter_hops) and record
            // the request. input/output mints follow the swap direction for this edge.
            swap_instructions.push(Instruction {
                program_id: DexKind::Jupiter.program_id(),
                accounts: Vec::new(),
                data: Vec::new(),
            });
            jupiter_hops.push(crate::arbitrage::opportunity::JupiterHopRequest {
                hop_index: i,
                input_mint: cycle.path[i],
                output_mint: cycle.path[i + 1],
                amount_in: quote.hop_in_amounts[i],
                min_out: quote.hop_min_outs[i],
            });
            continue;
        }
        let user_src = get_associated_token_address(&user, &cycle.path[i]);
        let user_dst = get_associated_token_address(&user, &cycle.path[i + 1]);
        let ix = build_swap_ix(
            pool, user_src, user_dst, user,
            quote.hop_in_amounts[i], quote.hop_min_outs[i],
            edge.a_to_b,
        ).ok()?;
        swap_instructions.push(ix);
    }

    let (setup_instructions, teardown_instructions, flash_loan_fee_lamports) =
        if config.enable_flash_loan {
            let flash = config.flash_loan.as_ref()?;
            let fee = amount_in * flash_loan::FLASH_LOAN_FEE_BPS / 10_000;
            let repay_amount = amount_in + fee;
            let setup = flash_loan::build_setup_instructions(user, &cycle.path, hops, amount_in, flash);
            let teardown = flash_loan::build_teardown_instructions(user, repay_amount, flash);

            // Probe the wire size before committing. Orca (15+ accounts) + MarginFi (~12 accounts)
            // easily blow past Solana's 1232-byte limit. Fall back to the normal wrap/unwrap path
            // when that happens — the opportunity was already filtered as profitable, so the
            // slightly higher tx_fee under normal mode still clears the threshold.
            let cu_limit = config.compute_unit_limit.max(1_200_000) as u32;
            let cu_price = config.compute_unit_price_micro_lamports;
            let mut probe: Vec<Instruction> = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
            ];
            probe.extend(setup.iter().cloned());
            probe.extend(swap_instructions.iter().cloned());
            probe.extend(teardown.iter().cloned());
            // Skip the wire-size guard for Jupiter cycles: the placeholder is a no-op and the
            // real (multi-)instructions + Jupiter ALTs aren't known until resolve_jupiter_hops
            // runs. The guard is re-applied there against the fully spliced tx + merged ALTs.
            if jupiter_hops.is_empty() {
                let wire_size = estimate_v0_wire_size(&probe, &user, alts);
                if wire_size > 1232 {
                    // Safety net: v0 + ALT compression should keep flash loan txs well under 1232 bytes.
                    // If this fires, the ALT is missing accounts for this cycle — re-run --init-alt.
                    warn!(wire_size, amount_in, "Flash loan tx too large even with ALT — skipping opportunity");
                    return None;
                }
            }
            (setup, teardown, fee)
        } else {
            let setup = build_setup_instructions(user, amount_in, &cycle.path);
            let teardown = build_teardown_instructions(user);
            (setup, teardown, 0u64)
        };

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
        setup_instructions,
        teardown_instructions,
        flash_loan_fee_lamports,
        use_direct_rpc: use_direct,
        jupiter_hops,
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

/// Estimate the v0 versioned transaction wire size for `ixs` with ALT compression.
/// Uses zeroed signatures and the default blockhash — accurate without a live keypair or RPC call.
pub(crate) fn estimate_v0_wire_size(ixs: &[Instruction], payer: &Pubkey, alts: &[AddressLookupTableAccount]) -> usize {
    let Ok(message) = v0::Message::try_compile(payer, ixs, alts, Hash::default())
        else { return usize::MAX };
    let num_sigs = message.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_sigs],
        message: VersionedMessage::V0(message),
    };
    bincode::serialized_size(&tx).unwrap_or(u64::MAX) as usize
}

/// Apply slippage tolerance to a quote amount, returning the minimum acceptable output.
/// Uses u128 for intermediate math to prevent overflow.
fn apply_slippage(amount: u64, slippage_bps: u64) -> u64 {
    let reduction = (amount as u128 * slippage_bps as u128 / 10_000) as u64;
    amount.saturating_sub(reduction)
}

fn compute_jito_tip(gross_profit: u64, config: &Config, tip_floor: u64) -> u64 {
    const MIN_TIP: u64 = 1_000;
    let ratio_tip = (gross_profit as f64 * config.tip_ratio) as u64;
    let floor_tip = if tip_floor > 0 && config.tip_floor_multiplier > 0.0 {
        (tip_floor as f64 * config.tip_floor_multiplier) as u64
    } else {
        0
    };
    ratio_tip.max(floor_tip).clamp(MIN_TIP, config.max_tip_lamports)
}

pub(crate) fn build_swap_ix(
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
        DexKind::Lifinity => {
            lifinity::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
        DexKind::Invariant => {
            invariant::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
        DexKind::Saber => {
            saber::build_swap_instruction(pool, user_src, user_dst, user, amount_in, min_out, a_to_b)
        }
        // Jupiter hops are resolved asynchronously from /swap-instructions at submit time
        // (see resolve_jupiter_hops in main.rs); build_opportunity emits a placeholder for
        // them and never calls this. Reaching here is a logic error.
        DexKind::Jupiter => {
            anyhow::bail!("Jupiter hops must be resolved via /swap-instructions, not build_swap_ix")
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
    use solana_sdk::{address_lookup_table::AddressLookupTableAccount, pubkey::Pubkey};
    use std::str::FromStr;
    use std::sync::atomic::{AtomicI32, AtomicU64};
    use std::sync::Arc;

    fn empty_alts() -> Vec<AddressLookupTableAccount> {
        vec![]
    }

    fn test_config() -> Config {
        Config {
            grpc_endpoint: String::new(),
            grpc_token: None,
            wallet_keypair_path: String::new(),
            rpc_url: String::new(),
            pools_config_path: String::new(),
            base_token: crate::dex::types::resolve_base_token(crate::dex::types::WSOL_MINT).unwrap(),
            min_sol_gas_lamports: 100_000_000,
            min_profit_lamports: 1_000,
            input_sol_lamports: 100_000_000,
            slippage_bps: 50,
            tip_ratio: 0.5,
            max_tip_lamports: 1_000_000,
            min_tip_lamports: 0,
            dry_run: false,
            bellman_ford_debounce_ms: 10,
            max_price_impact_bps: 10_000, // no impact cap in tests (pools are tiny by design)
            compute_unit_limit: 600_000,
            compute_unit_price_micro_lamports: 1_000,
            log_cycle_threshold_bps: 0.0,
            check_pools: false,
            disable_simulation: false,
            enable_flash_loan: false,
            flash_loan_max_input_lamports: 500_000_000_000,
            flash_loan: None,
            tip_floor_multiplier: 1.2,
            min_tip_floor_multiple: 0.0,
            alt_addresses: vec![],
            bypass_jito_bundle: false,
            jito_bundle_threshold_bps: 20.0,
            whale_min_sol_lamports: 0,
            whale_back_run_delay_ms: 0,
            enable_jupiter: false,
            jupiter_api_url: String::new(),
            jupiter_binary_path: None,
            jupiter_binary_key: None,
            jupiter_pairs_path: String::new(),
            jupiter_poll_interval_ms: 500,
            jupiter_probe_lamports: 1_000_000_000,
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
            damm_virtual_price: AtomicU64::new(0),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
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
        // 10 * 0.5 = 5 → below MIN_TIP of 1_000; tip_floor=0 disables floor anchor
        assert_eq!(compute_jito_tip(10, &config, 0), 1_000);
    }

    #[test]
    fn tip_is_ratio_of_profit_in_normal_range() {
        let config = test_config();
        // 400_000 * 0.5 = 200_000, within [1_000, 1_000_000]; floor anchor inactive
        assert_eq!(compute_jito_tip(400_000, &config, 0), 200_000);
    }

    #[test]
    fn tip_clamps_to_max_when_profit_is_large() {
        let config = test_config();
        // 10_000_000 * 0.5 = 5_000_000 → clamped to max_tip = 1_000_000
        assert_eq!(compute_jito_tip(10_000_000, &config, 0), 1_000_000);
    }

    #[test]
    fn tip_uses_floor_when_floor_exceeds_ratio() {
        let config = test_config(); // tip_ratio=0.5, multiplier=1.2, max_tip=1_000_000
        // ratio_tip = 100_000 * 0.5 = 50_000
        // floor_tip = 60_000 * 1.2 = 72_000
        assert_eq!(compute_jito_tip(100_000, &config, 60_000), 72_000);
    }

    #[test]
    fn tip_uses_ratio_when_ratio_exceeds_floor() {
        let config = test_config();
        // ratio_tip = 400_000 * 0.5 = 200_000
        // floor_tip = 100_000 * 1.2 = 120_000
        assert_eq!(compute_jito_tip(400_000, &config, 100_000), 200_000);
    }

    #[test]
    fn tip_floor_zero_fallback_matches_ratio_behavior() {
        let config = test_config();
        // tip_floor=0 → floor anchor disabled → identical to legacy ratio behavior
        assert_eq!(compute_jito_tip(400_000, &config, 0), 200_000);
    }

    #[test]
    fn tip_floor_clamped_by_max_tip() {
        let config = test_config(); // max_tip=1_000_000, multiplier=1.2
        // floor_tip = 2_000_000 * 1.2 = 2_400_000 → clamped to 1_000_000
        assert_eq!(compute_jito_tip(100_000, &config, 2_000_000), 1_000_000);
    }

    #[test]
    fn tip_floor_multiplier_zero_disables_floor_anchor() {
        let mut config = test_config();
        config.tip_floor_multiplier = 0.0;
        // floor anchor disabled → only ratio_tip applies
        assert_eq!(compute_jito_tip(400_000, &config, 500_000), 200_000);
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
            if let Some(opp) = optimize_input_and_tip(cycle, &registry, &config, sol, config.input_sol_lamports, 0, &empty_alts()) {
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
            let result = optimize_input_and_tip(&cycle, &registry, &config, sol, 0, 0, &empty_alts());
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
            let result = optimize_input_and_tip(&cycle, &registry, &config, sol, u64::MAX, 0, &empty_alts());
            assert!(result.is_none(), "unprofitable cycle must return None");
        }
    }

    #[test]
    fn v0_wire_size_with_alt_fits_in_1232_bytes() {
        use solana_sdk::instruction::{AccountMeta, Instruction};

        // Synthetic ALT with 200 accounts — representative of real bot ALT size
        let alt_accounts: Vec<Pubkey> = (0..200).map(|_| Pubkey::new_unique()).collect();
        let alt = AddressLookupTableAccount {
            key: Pubkey::new_unique(),
            addresses: alt_accounts.clone(),
        };
        let payer = Pubkey::new_unique();

        // 12 instructions each touching 5 accounts from the ALT — simulates a 3-hop
        // flash loan tx (compute budget × 2 + MarginFi × 4 + swaps × 3 + teardown × 3)
        let ixs: Vec<Instruction> = (0..12)
            .map(|i| Instruction {
                program_id: alt_accounts[i],
                accounts: (0..5)
                    .map(|j| AccountMeta::new(alt_accounts[i * 5 + j + 60], false))
                    .collect(),
                data: vec![1u8; 16],
            })
            .collect();

        let size = estimate_v0_wire_size(&ixs, &payer, &[alt]);
        assert!(size < 1232, "v0 tx with ALT must be < 1232 bytes, got {size}");
    }
}

/// Classify why a cycle was rejected by the evaluator at a given `amount_in`.
/// Mirrors the exact fee math in `evaluate_quotes` so the reason is always accurate.
fn rejection_reason(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    config: &Config,
    amount_in: u64,
    tip_floor: u64,
    use_direct: bool,
) -> &'static str {
    let Some(gross_ratio) = probe_gross_ratio(cycle, pools, config, amount_in) else {
        return "slippage";
    };
    let gross_out = (amount_in as f64 * gross_ratio) as u64;
    let (num_swap_txs, cu_limit) = if config.enable_flash_loan {
        (1u64, config.compute_unit_limit.max(1_200_000))
    } else {
        (cycle.edges.len() as u64, config.compute_unit_limit)
    };
    let cu_fee = cu_limit * config.compute_unit_price_micro_lamports / 1_000_000;
    let tx_fee = BASE_FEE_PER_TX * (num_swap_txs + 1) + cu_fee * num_swap_txs;
    let flash_fee = if config.enable_flash_loan {
        amount_in * crate::flash_loan::FLASH_LOAN_FEE_BPS / 10_000
    } else {
        0
    };
    let gross_profit = gross_out as i64 - amount_in as i64 - tx_fee as i64 - flash_fee as i64;
    if gross_profit <= 0 {
        return "fees_ate_margin";
    }
    let jito_tip = if use_direct {
        let floor_tip = if tip_floor > 0 && config.tip_floor_multiplier > 0.0 {
            (tip_floor as f64 * config.tip_floor_multiplier) as u64
        } else {
            1_000
        };
        floor_tip.clamp(1_000, config.max_tip_lamports)
    } else {
        compute_jito_tip(gross_profit as u64, config, tip_floor)
    };
    if config.min_tip_lamports > 0 && jito_tip < config.min_tip_lamports {
        return "tip_below_min";
    }
    let net_profit = gross_profit - jito_tip as i64;
    if net_profit <= 0 || net_profit < config.min_profit_lamports as i64 {
        return "net_below_min";
    }
    "unknown"
}

pub fn optimize_input_and_tip(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    available_sol: u64,
    tip_floor: u64,
    alts: &[AddressLookupTableAccount],
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

    // Cache pool refs once — 3 DashMap lookups total instead of per-iteration.
    let pools: Vec<Arc<Pool>> = cycle.edges.iter()
        .map(|e| registry.get_by_pool_id(&e.pool_id))
        .collect::<Option<Vec<_>>>()?;

    // Flash loan: INPUT_SOL_LAMPORTS is ignored — capital is borrowed, not from the wallet.
    // The ternary search finds the slippage-optimal peak within [MIN_PROBE, available_sol].
    // Normal mode: cap is bounded by both the wallet balance and the configured max.
    let cap = if config.enable_flash_loan {
        available_sol
    } else {
        config.input_sol_lamports.min(available_sol)
    };
    const MIN_PROBE: u64 = 1_000_000; // 0.001 SOL — below this, fees consume all profit
    if cap < MIN_PROBE { return None; }

    // When bypass_jito_bundle is active, run the ternary search with use_direct=true first.
    // This finds the slippage-optimal input under the direct-RPC fee model (no Jito tip),
    // giving us the real AMM output (gross_out) from which we compute the ACTUAL margin.
    // We then re-route based on that actual margin rather than the zero-impact graph rate,
    // closing the gap where a 29 bps graph cycle delivers only 19 bps after AMM slippage.
    let candidate_direct = config.enable_flash_loan && config.bypass_jito_bundle;

    // Pass 1: ternary search for the optimal amount_in.
    // Net-profit is concave in amount_in (AMM slippage), so ternary search finds
    // the global maximum in 25 pure-math evaluations with lamport-scale precision.
    let best_result = ternary_search_net_profit(cycle, &pools, config, MIN_PROBE, cap, tip_floor, candidate_direct);

    // Fallback: if candidate_direct=true (floor-tip mode) produced no result, retry with
    // ratio-based tip. This handles fat cycles (>threshold bps) where the floor tip is
    // below MIN_TIP_LAMPORTS — the routing correction later will re-evaluate fees correctly.
    let best_result = if best_result.is_none() && candidate_direct {
        ternary_search_net_profit(cycle, &pools, config, MIN_PROBE, cap, tip_floor, false)
    } else {
        best_result
    };

    let (best_amount_in, mut best_quote) = match best_result {
        Some(r) => r,
        None => {
            // Only diagnose cycles that exceed the configured log threshold,
            // and rate-limit to once per NEAR_MISS_COOLDOWN_SECS per unique path.
            let graph_bps = (gross_ratio - 1.0) * 10_000.0;
            if graph_bps >= config.log_cycle_threshold_bps {
                let hash = near_miss_path_hash(&cycle.path);
                let should_log = {
                    let map = NEAR_MISS_SEEN.get_or_init(|| Mutex::new(HashMap::new()));
                    let mut guard = map.lock().unwrap_or_else(|e| e.into_inner());
                    let now = Instant::now();
                    match guard.get(&hash) {
                        Some(&last) if last.elapsed().as_secs() < NEAR_MISS_COOLDOWN_SECS => false,
                        _ => { guard.insert(hash, now); true }
                    }
                };
                if !should_log { return None; }

                let path: String = cycle.path.iter()
                    .map(crate::dex::types::mint_symbol)
                    .collect::<Vec<_>>()
                    .join("→");

                // Try small fixed probe amounts — cap/2 (250 SOL) always exceeds the
                // 1% price impact limit, making probe_gross_ratio return None for every cycle.
                // Start at 0.1 SOL and step up until a quote succeeds.
                const PROBE_SIZES: &[u64] = &[
                    100_000_000,    // 0.1 SOL
                    1_000_000_000,  // 1 SOL
                    10_000_000_000, // 10 SOL
                ];
                let mut diagnosed = false;
                for &probe in PROBE_SIZES {
                    let probe = probe.min(cap).max(MIN_PROBE);
                    if let Some(ratio) = probe_gross_ratio(cycle, &pools, config, probe) {
                        let probe_bps = (ratio - 1.0) * 10_000.0;
                        // Skip cycles where the AMM itself gives a net-negative result
                        // (e.g. a 100 bps DAMM fee eating a 5 bps price edge). Those are
                        // structurally unprofitable and not useful near-miss candidates.
                        if probe_bps < 0.0 { break; }
                        let reason = rejection_reason(cycle, &pools, config, probe, tip_floor, candidate_direct);
                        info!(
                            "near-miss [{path}] graph={graph_bps:+.2}bps realized={probe_bps:+.2}bps probe={}L reason={reason}",
                            probe,
                        );
                        diagnosed = true;
                        break;
                    }
                }
                if !diagnosed {
                    let detail = diagnose_quote_failure(cycle, &pools, config, 100_000_000);
                    info!("near-miss [{path}] graph={graph_bps:+.2}bps reason=quote_failed ({detail})");
                }
            }
            return None;
        }
    };

    // Use the actual AMM output (with slippage) to decide routing — not the graph rate.
    // The ternary search found the slippage-optimal input; gross_out reflects real impact.
    let actual_gross_bps = (best_quote.gross_out as f64 / best_amount_in as f64 - 1.0) * 10_000.0;
    let use_direct = config.enable_flash_loan
        && config.bypass_jito_bundle
        && actual_gross_bps <= config.jito_bundle_threshold_bps;

    // If the actual routing differs from the candidate (graph said Jito but AMM says direct,
    // or vice versa), re-evaluate at best_amount_in with the correct fee model.
    // The optimal input doesn't change (slippage peak is fee-model-independent), but
    // tx_fee and jito_tip must match the final routing decision.
    if use_direct != candidate_direct {
        match evaluate_quotes(cycle, &pools, config, best_amount_in, tip_floor, use_direct) {
            Some(corrected) => {
                debug!(
                    "Routing corrected: graph={:.2}bps actual={actual_gross_bps:.2}bps → {} (was {})",
                    (gross_ratio - 1.0) * 10_000.0,
                    if use_direct { "direct-RPC" } else { "jito" },
                    if candidate_direct { "direct-RPC" } else { "jito" },
                );
                best_quote = corrected;
            }
            None => {
                // Correct fee model makes this unprofitable — skip
                return None;
            }
        }
    }

    debug!(
        "Best input: amount_in={} gross_out={} actual_gross={actual_gross_bps:.2}bps net_profit={}",
        best_amount_in, best_quote.gross_out, best_quote.net_profit,
    );

    // Pass 2: build swap instructions only for the winning fraction.
    let result = build_opportunity(cycle, &pools, user, best_amount_in, best_quote, config, alts, use_direct);

    // Safety net: if ALT is missing accounts and the tx is still too large, retry wallet-funded.
    if result.is_none() && config.enable_flash_loan {
        debug!("Flash loan tx too large after ALT compression — retrying cycle as wallet-funded");
        let wallet_config = Config { enable_flash_loan: false, flash_loan: None, ..config.clone() };
        let wallet_cap = config.input_sol_lamports.min(available_sol);
        if wallet_cap < MIN_PROBE { return None; }

        let wallet_best = ternary_search_net_profit(cycle, &pools, &wallet_config, MIN_PROBE, wallet_cap, tip_floor, false);
        if let Some((wallet_amount, wallet_quote)) = wallet_best {
            debug!(
                "Wallet-funded retry: amount_in={} net_profit={}",
                wallet_amount, wallet_quote.net_profit,
            );
            return build_opportunity(cycle, &pools, user, wallet_amount, wallet_quote, &wallet_config, alts, false);
        }
    }

    result
}
