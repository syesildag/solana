use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::transaction::{Transaction, TransactionError};
use tracing::{debug, info, warn};

use crate::arbitrage::opportunity::ArbOpportunity;

/// Outcome of a simulation attempt.
#[derive(Debug)]
pub enum SimOutcome {
    /// All hops passed — safe to submit.
    Passed,
    /// A hop was rejected for a market/DEX reason (slippage, price, custom program
    /// error, insufficient token balance). Applying a cooldown makes sense here:
    /// the same instruction is unlikely to succeed until the market moves.
    MarketRejected { hop: usize, err: TransactionError },
    /// A hop was rejected because an account was missing or the instruction was
    /// malformed (AccountNotFound, InvalidAccountData, InvalidProgramId …).
    /// Applying a cooldown prevents repeated RPC simulation calls for a broken
    /// bundle that won't succeed until the underlying config issue is resolved.
    InfraError { hop: usize, err: TransactionError },
    /// Stale tick array — the CLMM price moved between when we read
    /// tick_current_index and when the simulation RPC ran. Covers:
    ///   Custom(2006) Anchor ConstraintSeeds — PDA derived from stale tick
    ///   Custom(3012) Anchor AccountNotInitialized — tick array PDA we derived doesn't exist
    ///   Custom(6023) Orca InvalidTickArraySequence — arrays no longer cover current tick
    /// One gRPC state-account update (~< 1 s) resolves all three; use a 2-second cooldown.
    StaleTickData { hop: usize, err: TransactionError },
}

/// Simulate every swap transaction in the bundle (all hops, excluding the tip tx).
///
/// Returns a structured [`SimOutcome`] so callers can distinguish market failures
/// (worth cooling down) from infrastructure/config failures (worth fixing, not suppressing).
pub async fn simulate_opportunity(
    opportunity: &ArbOpportunity,
    swap_txs: &[Transaction],
    rpc: &RpcClient,
) -> Result<SimOutcome> {
    let sim_config = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: None,
        encoding: None,
        accounts: None,
        min_context_slot: None,
        inner_instructions: false,
    };

    // Simulate all hops concurrently — each tx is independent so order doesn't matter.
    let futs = swap_txs.iter().enumerate().map(|(hop, tx)| {
        let cfg = sim_config.clone();
        async move {
            let res = rpc
                .simulate_transaction_with_config(tx, cfg)
                .await
                .with_context(|| format!("RPC simulate_transaction failed for hop {hop}"))?;
            Ok::<_, anyhow::Error>((hop, res))
        }
    });
    let results = futures::future::try_join_all(futs).await?;

    for (hop, result) in results {
        let Some(err) = result.value.err else {
            debug!(hop, units = result.value.units_consumed, "Simulation passed");
            continue;
        };

        if let Some(logs) = &result.value.logs {
            for line in logs {
                debug!(hop, log = %line, "sim log");
            }
        }

        let outcome = if is_infra_error(&err) {
            warn!(hop, ?err, cycle = ?opportunity.cycle.path,
                "Simulation failed — infrastructure/config error (no cooldown applied)");
            SimOutcome::InfraError { hop, err }
        } else if is_stale_tick_data(&err) {
            info!(hop, ?err, cycle = ?opportunity.cycle.path,
                "Simulation rejected — stale tick array (ConstraintSeeds), price crossed boundary");
            SimOutcome::StaleTickData { hop, err }
        } else {
            info!(hop, ?err, cycle = ?opportunity.cycle.path,
                "Simulation rejected — market condition");
            SimOutcome::MarketRejected { hop, err }
        };

        return Ok(outcome);
    }

    Ok(SimOutcome::Passed)
}

/// Returns true for CLMM tick-array staleness errors caused by the pool price moving
/// between our last gRPC tick_current_index update and the simulation RPC call.
/// All three resolve after one state-account update (< 1 s), so a 2-second cooldown suffices.
///
/// - Custom(2006) = Anchor ConstraintSeeds: tick moved to a new tick array, so the PDA
///   we derived from the stale tick doesn't match what the program expects.
/// - Custom(3012) = Anchor AccountNotInitialized: tick moved into a range where no tick
///   array has been initialized yet — the PDA exists in our derivation but not on-chain.
/// - Custom(6023) = Orca InvalidTickArraySequence: the three tick arrays are consecutive
///   but no longer cover the pool's current tick at simulation time.
fn is_stale_tick_data(err: &TransactionError) -> bool {
    use solana_sdk::instruction::InstructionError;
    matches!(
        err,
        TransactionError::InstructionError(_, InstructionError::Custom(2006 | 3012 | 6023))
    )
}

/// Returns true for errors that indicate a broken instruction or missing account,
/// not a market-level rejection.
fn is_infra_error(err: &TransactionError) -> bool {
    use solana_sdk::transaction::TransactionError;
    use solana_sdk::instruction::InstructionError;

    if let TransactionError::InstructionError(_, InstructionError::Custom(code)) = err {
        match code {
            // Anchor account-relationship constraint failures — wrong account in pools.json,
            // never caused by price movement:
            //   2001 ConstraintHasOne  — pool field (open_orders, market, …) ≠ passed account
            //   2004 ConstraintOwner   — account owned by wrong program
            //   2012 ConstraintAddress — account key ≠ required address
            2001 | 2004 | 2012 => return true,

            // Anchor account loading errors — config bug, not a market condition.
            // 3007 AccountOwnedByWrongProgram — wrong account in pools.json
            // 3012 AccountNotInitialized is excluded: it signals a tick array PDA
            //      we derived from a stale tick that doesn't exist on-chain yet,
            //      which is a transient race condition handled by is_stale_tick_data.
            3000..=3011 | 3013..=3099 => return true,

            _ => {}
        }
    }

    matches!(
        err,
        TransactionError::AccountNotFound
            | TransactionError::ProgramAccountNotFound
            | TransactionError::AccountInUse
            | TransactionError::AccountLoadedTwice
            | TransactionError::InvalidAccountForFee
            | TransactionError::InvalidProgramForExecution
            | TransactionError::InvalidWritableAccount
            | TransactionError::AddressLookupTableNotFound
            | TransactionError::InvalidAddressLookupTableOwner
            | TransactionError::InvalidAddressLookupTableData
            | TransactionError::InvalidAddressLookupTableIndex
    )
}
