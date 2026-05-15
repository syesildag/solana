use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use tracing::debug;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};

use crate::arbitrage::opportunity::ArbOpportunity;
use crate::config::Config;

/// The 8 Jito tip accounts (rotated per bundle for load distribution).
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// A signed Jito bundle: up to 5 transactions submitted atomically.
pub struct JitoBundle {
    pub transactions: Vec<Transaction>,
}

impl JitoBundle {
    /// Build and sign a bundle from an ArbOpportunity.
    ///
    /// Normal layout (enable_flash_loan=false):
    ///   tx[0..n-1] = swap instructions (one tx per hop)
    ///   tx[n]      = Jito tip transfer
    ///
    /// Flash loan layout (enable_flash_loan=true):
    ///   tx[0] = setup (StartFlashloan + Borrow) + all swaps + teardown (Repay + EndFlashloan + Close)
    ///   tx[1] = Jito tip transfer
    ///
    /// All transactions share the same recent blockhash so they land in the same block.
    pub fn build(
        opportunity: &ArbOpportunity,
        keypair: &Keypair,
        recent_blockhash: Hash,
        config: &Config,
    ) -> Result<Self> {
        let payer = keypair.pubkey();
        let mut txs: Vec<Transaction> = Vec::new();

        let cu_price = config.compute_unit_price_micro_lamports;

        if config.enable_flash_loan {
            // Flash loan: all instructions go into a single transaction.
            // CU limit is raised to accommodate MarginFi instructions alongside the swaps.
            let cu_limit = config.compute_unit_limit.max(1_200_000) as u32;
            let mut ixs: Vec<solana_sdk::instruction::Instruction> = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
            ];
            ixs.extend(opportunity.setup_instructions.iter().cloned());
            ixs.extend(opportunity.swap_instructions.iter().cloned());
            ixs.extend(opportunity.teardown_instructions.iter().cloned());
            txs.push(Transaction::new_signed_with_payer(
                &ixs,
                Some(&payer),
                &[keypair],
                recent_blockhash,
            ));
        } else {
            // Normal: one transaction per swap hop.
            let cu_limit = config.compute_unit_limit as u32;
            let last_swap = opportunity.swap_instructions.len().saturating_sub(1);
            for (i, ix) in opportunity.swap_instructions.iter().enumerate() {
                let mut ixs: Vec<solana_sdk::instruction::Instruction> = vec![
                    ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                    ComputeBudgetInstruction::set_compute_unit_price(cu_price),
                ];
                if i == 0 {
                    ixs.extend(opportunity.setup_instructions.iter().cloned());
                }
                ixs.push(ix.clone());
                if i == last_swap {
                    ixs.extend(opportunity.teardown_instructions.iter().cloned());
                }
                txs.push(Transaction::new_signed_with_payer(
                    &ixs,
                    Some(&payer),
                    &[keypair],
                    recent_blockhash,
                ));
            }
        }

        // Tip transaction: SOL transfer to a randomly selected Jito tip account
        let tip_account = random_tip_account()?;
        let tip_ix = system_instruction::transfer(&payer, &tip_account, opportunity.jito_tip_lamports);
        let tip_tx = Transaction::new_signed_with_payer(
            &[tip_ix],
            Some(&payer),
            &[keypair],
            recent_blockhash,
        );
        txs.push(tip_tx);

        if txs.len() > 5 {
            anyhow::bail!("Bundle exceeds Jito's 5-transaction limit ({} txs)", txs.len());
        }

        Ok(Self { transactions: txs })
    }

    /// Serialize all transactions to base58 for Jito Block Engine submission.
    /// Fails fast if any transaction exceeds Solana's 1232-byte wire limit.
    pub fn encode(&self) -> Result<Vec<String>> {
        self.transactions
            .iter()
            .enumerate()
            .map(|(i, tx)| {
                let bytes = bincode::serialize(tx)
                    .context("Failed to serialize transaction")?;
                debug!(tx = i, bytes = bytes.len(), "tx wire size");
                if bytes.len() > 1232 {
                    anyhow::bail!(
                        "tx[{}] is {} bytes — exceeds Solana's 1232-byte limit (flash loan packs too many accounts)",
                        i, bytes.len()
                    );
                }
                Ok(bs58::encode(bytes).into_string())
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn first_tx(&self) -> Option<&Transaction> {
        self.transactions.first()
    }
}

fn random_tip_account() -> Result<Pubkey> {
    let mut rng = rand::thread_rng();
    let addr = JITO_TIP_ACCOUNTS
        .choose(&mut rng)
        .context("Empty tip accounts list")?;
    addr.parse().context("Invalid tip account pubkey")
}
