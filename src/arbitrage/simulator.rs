use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::RpcSimulateTransactionConfig;
use solana_sdk::transaction::Transaction;
use tracing::{debug, info};

use crate::arbitrage::opportunity::ArbOpportunity;

/// Simulate every swap transaction in the bundle (all hops, excluding the tip tx).
/// Returns true only if ALL simulations pass.
///
/// Each tx is simulated independently with replace_recent_blockhash=true.
/// Jito bundles are atomic — all-or-nothing — so we verify every leg before committing.
pub async fn simulate_opportunity(
    opportunity: &ArbOpportunity,
    swap_txs: &[Transaction],
    rpc: &RpcClient,
) -> Result<bool> {
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

        if let Some(err) = &result.value.err {
            info!(
                hop,
                ?err,
                cycle = ?opportunity.cycle.path,
                "Simulation rejected"
            );
            return Ok(false);
        }

        debug!(
            hop,
            units = result.value.units_consumed,
            "Simulation passed"
        );
    }

    Ok(true)
}
