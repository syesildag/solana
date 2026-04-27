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

    for (hop, tx) in swap_txs.iter().enumerate() {
        let result = rpc
            .simulate_transaction_with_config(tx, sim_config.clone())
            .await
            .with_context(|| format!("RPC simulate_transaction failed for hop {hop}"))?;

        let Some(err) = result.value.err else {
            debug!(
                hop,
                units = result.value.units_consumed,
                "Simulation passed"
            );
            continue;
        };

        // Classify the error so the caller can decide whether to cooldown.
        let outcome = if is_infra_error(&err) {
            warn!(
                hop,
                ?err,
                cycle = ?opportunity.cycle.path,
                "Simulation failed — infrastructure/config error (no cooldown applied)"
            );
            SimOutcome::InfraError { hop, err }
        } else {
            info!(
                hop,
                ?err,
                cycle = ?opportunity.cycle.path,
                "Simulation rejected — market condition"
            );
            SimOutcome::MarketRejected { hop, err }
        };

        return Ok(outcome);
    }

    Ok(SimOutcome::Passed)
}

/// Returns true for errors that indicate a broken instruction or missing account,
/// not a market-level rejection.
fn is_infra_error(err: &TransactionError) -> bool {
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
