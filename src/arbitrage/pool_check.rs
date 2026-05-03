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
use crate::dex::types::{DexKind, mint_symbol, Pool, WSOL_PUBKEY};

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
        Ok(resp) => {
            let logs = resp.value.logs.unwrap_or_default();
            match resp.value.err {
                None      => Outcome::Pass,
                Some(err) => {
                    let outcome = classify(&err);
                    if matches!(outcome, Outcome::Config(_) | Outcome::MissingAccount) {
                        for line in &logs {
                            if line.contains("AnchorError") || line.contains("Error Code:")
                                || line.contains("Error Number:") || line.contains("failed")
                            {
                                println!("        {line}");
                            }
                        }
                        diagnose_config_failure(&pool, rpc).await;
                    }
                    outcome
                }
            }
        }
    }
}

/// Deep diagnostics for CONFIG / ACCT_MISSING failures:
///
/// **CLMM**: Fetches the observation account loaded from pool-state offset 201.
///   Prints owner, discriminator (first 8 bytes), and whether it matches the
///   expected Anchor `ObservationState` discriminator.  Also shows how the
///   runtime key compares to the value in `pool.extra.clmm_observation`
///   (from pools.json) so we can spot pools.json / on-chain drift.
///
/// **DLMM**: Fetches the lb_pair (pool.id) account and reads every `has_one`
///   field — token_x_mint, token_y_mint, reserve_x, reserve_y — comparing
///   them against what `build_swap_instruction` will actually pass.
async fn diagnose_config_failure(pool: &Pool, rpc: &RpcClient) {
    match pool.dex {
        DexKind::RaydiumClmm => {
            use std::sync::atomic::Ordering;
            let words: [u64; 4] = std::array::from_fn(|i| pool.clmm_observation_key[i].load(Ordering::Relaxed));
            let bytes: [u8; 32] = unsafe { std::mem::transmute(words) };
            let obs_from_state = Pubkey::from(bytes);
            let clmm_program = pool.dex.program_id();

            // Anchor discriminator for ObservationState = sha256("account:ObservationState")[0..8]
            const OBS_DISC: [u8; 8] = [0x7a, 0xae, 0xc5, 0x35, 0x81, 0x09, 0xa5, 0x84];

            if obs_from_state == Pubkey::default() {
                println!("        obs key: NOT YET LOADED (clmm_observation_key is zero)");
                return;
            }

            // Show drift between pools.json clmm_observation and what we read from state
            let extra_obs = pool.extra.clmm_observation;
            if let Some(extra_key) = extra_obs {
                if extra_key != obs_from_state {
                    println!("        !! pools.json clmm_observation ({}) ≠ state offset-201 ({})",
                        extra_key, obs_from_state);
                } else {
                    println!("        pools.json clmm_observation matches state offset-201 ✓");
                }
            }

            println!("        obs key (full): {obs_from_state}");

            if let Ok(accounts) = rpc.get_multiple_accounts(&[obs_from_state]).await {
                match accounts.into_iter().next().flatten() {
                    None => println!("        obs account: MISSING on-chain"),
                    Some(acc) => {
                        let owner_tag = if acc.owner == clmm_program { "✓ CLMM" } else { "✗ WRONG" };
                        println!("        obs owner: {} {owner_tag}", acc.owner);
                        if acc.data.len() >= 8 {
                            let disc: [u8; 8] = acc.data[0..8].try_into().unwrap();
                            let disc_hex = disc.iter().map(|b| format!("{b:02x}")).collect::<String>();
                            let expected_hex = OBS_DISC.iter().map(|b| format!("{b:02x}")).collect::<String>();
                            let disc_tag = if disc == OBS_DISC { "✓ ObservationState" } else { "✗ WRONG DISCRIMINATOR" };
                            println!("        obs disc: {disc_hex} (expected {expected_hex}) {disc_tag}");
                        } else {
                            println!("        obs data too short: {} bytes", acc.data.len());
                        }
                    }
                }
            }
        }

        DexKind::MeteoraDlmm => {
            // First: verify vault mints (vaults hold the right tokens)
            if let Ok(vault_accs) = rpc.get_multiple_accounts(&[pool.vault_a, pool.vault_b]).await {
                let read_mint = |acc: Option<&solana_sdk::account::Account>| -> Option<Pubkey> {
                    acc.filter(|a| a.data.len() >= 32)
                       .and_then(|a| Pubkey::try_from(&a.data[0..32]).ok())
                };
                let va_mint = read_mint(vault_accs[0].as_ref());
                let vb_mint = read_mint(vault_accs[1].as_ref());
                let check = |actual: Option<Pubkey>, expected: &Pubkey| -> String {
                    match actual {
                        None    => "missing".to_string(),
                        Some(m) if &m == expected => format!("{m} ✓"),
                        Some(m) => format!("{m} ✗ (expected {expected})"),
                    }
                };
                println!("        vault_a ({}) mint → {}", &pool.vault_a.to_string()[..8], check(va_mint, &pool.token_a));
                println!("        vault_b ({}) mint → {}", &pool.vault_b.to_string()[..8], check(vb_mint, &pool.token_b));
            }

            // Second: read lb_pair and compare every has_one field
            // LbPair layout (after 8-byte discriminator):
            //   [8..72]   StaticParameters+VariableParameters (64 bytes)
            //   [72..76]  bump+bin_step_seed+pair_type (4 bytes)
            //   [76..88]  active_id+bin_step+status+padding (12 bytes)
            //   [88..120] token_x_mint
            //   [120..152] token_y_mint
            //   [152..184] reserve_x
            //   [184..216] reserve_y
            if let Ok(lb_pair_accs) = rpc.get_multiple_accounts(&[pool.id]).await {
                match lb_pair_accs.into_iter().next().flatten() {
                    None => println!("        lb_pair account MISSING on-chain"),
                    Some(acc) if acc.data.len() < 216 => {
                        println!("        lb_pair data too short: {} bytes", acc.data.len());
                    }
                    Some(acc) => {
                        let read_key = |range: std::ops::Range<usize>| -> Option<Pubkey> {
                            Pubkey::try_from(&acc.data[range]).ok()
                        };
                        let on_chain_x_mint  = read_key(88..120);
                        let on_chain_y_mint  = read_key(120..152);
                        let on_chain_res_x   = read_key(152..184);
                        let on_chain_res_y   = read_key(184..216);

                        let token_a_is_x = pool.token_a < pool.token_b;
                        let (exp_x_mint, exp_y_mint, exp_res_x, exp_res_y) = if token_a_is_x {
                            (pool.token_a, pool.token_b, pool.vault_a, pool.vault_b)
                        } else {
                            (pool.token_b, pool.token_a, pool.vault_b, pool.vault_a)
                        };

                        let cmp = |label: &str, actual: Option<Pubkey>, expected: Pubkey| {
                            match actual {
                                None => println!("        {label}: unreadable"),
                                Some(a) if a == expected => println!("        {label}: {a} ✓"),
                                Some(a) => println!("        {label}: {a} ✗ (we pass {expected})"),
                            }
                        };
                        cmp("lb_pair.token_x_mint", on_chain_x_mint,  exp_x_mint);
                        cmp("lb_pair.token_y_mint", on_chain_y_mint,  exp_y_mint);
                        cmp("lb_pair.reserve_x   ", on_chain_res_x,   exp_res_x);
                        cmp("lb_pair.reserve_y   ", on_chain_res_y,   exp_res_y);
                    }
                }
            }
        }

        _ => {}
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
