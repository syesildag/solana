use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    instruction::Instruction,
    pubkey::Pubkey,
    system_instruction,
    transaction::{Transaction, TransactionError},
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};

use std::sync::Arc;

use crate::arbitrage::evaluator::build_swap_ix;
use crate::arbitrage::simulator::{is_infra_error, is_stale_tick_data};
use crate::dex::PoolRegistry;
use crate::dex::types::{mint_symbol, Pool, WSOL_PUBKEY};

// Small enough to avoid meaningful slippage; large enough that the program
// doesn't reject the instruction before reaching any account checks.
const CHECK_AMOUNT: u64 = 1_000_000; // 0.001 SOL or equivalent

#[derive(Debug)]
enum Outcome {
    Pass,
    /// Tick array staleness — transient, resolves on next gRPC update.
    Stale(TransactionError),
    /// Wrong pool account (owner, address, or account count) — fix pools.json.
    Config(TransactionError),
    /// Instruction ran but failed on market conditions (price, balance).
    /// Confirms accounts are structurally correct.
    Market(TransactionError),
    /// A pool account referenced in the instruction doesn't exist on-chain.
    MissingAccount,
    /// The instruction builder itself returned an error (pre-simulation).
    BuildFail(String),
}

impl Outcome {
    fn tag(&self) -> &'static str {
        match self {
            Self::Pass           => "PASS",
            Self::Stale(_)       => "STALE",
            Self::Config(_)      => "CONFIG !!",
            Self::Market(_)      => "market",
            Self::MissingAccount => "ACCT_MISSING !!",
            Self::BuildFail(_)   => "BUILD_FAIL !!",
        }
    }

}

fn classify(err: &TransactionError) -> Outcome {
    use solana_sdk::transaction::TransactionError as TE;
    if matches!(
        err,
        TE::AccountNotFound | TE::ProgramAccountNotFound | TE::AddressLookupTableNotFound
    ) {
        return Outcome::MissingAccount;
    }
    if is_infra_error(err) {
        return Outcome::Config(err.clone());
    }
    if is_stale_tick_data(err) {
        return Outcome::Stale(err.clone());
    }
    Outcome::Market(err.clone())
}

/// Simulate one a→b swap per pool and print a pass/fail table.
/// Returns `true` if all pools pass (no CONFIG or BUILD_FAIL outcomes).
pub async fn check_pools(registry: &PoolRegistry, rpc: &RpcClient, user: Pubkey) -> Result<bool> {
    let mut pools = registry.all_pools();
    pools.sort_by_key(|p| (p.dex.short_name(), p.id.to_string()));

    let sim_cfg = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: None,
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };

    println!("\nChecking {} pools (a→b direction, {CHECK_AMOUNT} lamports)…\n", pools.len());
    println!("{:<10}  {:<12}  {:<8} → {:<8}  {}", "Pool", "DEX", "From", "To", "Result");
    println!("{}", "─".repeat(72));

    let (mut n_pass, mut n_stale, mut n_config, mut n_market, mut n_build, mut n_acct) =
        (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);

    for pool in &pools {
        let short   = &pool.id.to_string()[..8];
        let from    = mint_symbol(&pool.token_a);
        let to      = mint_symbol(&pool.token_b);
        let dex     = pool.dex.short_name();

        let outcome = simulate_pool(Arc::clone(pool), user, &sim_cfg, rpc).await;

        let detail = match &outcome {
            Outcome::Config(e) | Outcome::Market(e) | Outcome::Stale(e) =>
                format!("  {e:?}"),
            Outcome::BuildFail(s) =>
                format!("  {s}"),
            _ => String::new(),
        };

        println!("{short:<10}  {dex:<12}  {from:<8} → {to:<8}  {}{detail}", outcome.tag());

        match outcome {
            Outcome::Pass           => n_pass   += 1,
            Outcome::Stale(_)       => n_stale  += 1,
            Outcome::Config(_)      => n_config += 1,
            Outcome::Market(_)      => n_market += 1,
            Outcome::BuildFail(_)   => n_build  += 1,
            Outcome::MissingAccount => n_acct   += 1,
        }
    }

    println!("{}", "─".repeat(72));
    println!(
        "PASS {n_pass}  STALE {n_stale}  market {n_market}  \
         CONFIG {n_config}  BUILD_FAIL {n_build}  ACCT_MISSING {n_acct}"
    );

    if n_config > 0 || n_build > 0 || n_acct > 0 {
        println!("\n!! Pools marked CONFIG / BUILD_FAIL / ACCT_MISSING need attention before going live.");
    }

    Ok(n_config == 0 && n_build == 0 && n_acct == 0)
}

async fn simulate_pool(
    pool: Arc<Pool>,
    user: Pubkey,
    sim_cfg: &RpcSimulateTransactionConfig,
    rpc: &RpcClient,
) -> Outcome {
    let user_src = get_associated_token_address(&user, &pool.token_a);
    let user_dst = get_associated_token_address(&user, &pool.token_b);

    let swap_ix = match build_swap_ix(&pool, user_src, user_dst, user, CHECK_AMOUNT, 0, true) {
        Ok(ix)  => ix,
        Err(e)  => return Outcome::BuildFail(e.to_string()),
    };

    let ixs = build_check_ixs(&pool, user, user_src, swap_ix);
    let tx  = Transaction::new_with_payer(&ixs, Some(&user));

    match rpc.simulate_transaction_with_config(&tx, sim_cfg.clone()).await {
        Err(e)   => Outcome::BuildFail(format!("RPC: {e}")),
        Ok(resp) => match resp.value.err {
            None      => Outcome::Pass,
            Some(err) => classify(&err),
        },
    }
}

/// Prepend idempotent ATA creation and (for WSOL input) the SOL wrap, then
/// append the swap instruction.  All instructions run in a single simulation
/// transaction so user accounts definitely exist when the swap runs.
fn build_check_ixs(pool: &Pool, user: Pubkey, user_src: Pubkey, swap_ix: Instruction) -> Vec<Instruction> {
    let mut ixs = vec![ComputeBudgetInstruction::set_compute_unit_limit(600_000)];

    // Create ATAs idempotently for both sides (no-op if they already exist)
    for &mint in &[pool.token_a, pool.token_b] {
        if mint != WSOL_PUBKEY {
            ixs.push(create_associated_token_account_idempotent(
                &user, &user, &mint, &spl_token::id(),
            ));
        }
    }

    // If swapping from WSOL: create WSOL ATA + wrap the check amount
    if pool.token_a == WSOL_PUBKEY {
        ixs.push(create_associated_token_account_idempotent(
            &user, &user, &WSOL_PUBKEY, &spl_token::id(),
        ));
        ixs.push(system_instruction::transfer(&user, &user_src, CHECK_AMOUNT));
        ixs.push(
            spl_token::instruction::sync_native(&spl_token::id(), &user_src)
                .expect("sync_native is always valid"),
        );
    }

    ixs.push(swap_ix);
    ixs
}
