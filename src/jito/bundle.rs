use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use tracing::debug;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
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

/// A signed Jito bundle: up to 5 versioned transactions submitted atomically.
pub struct JitoBundle {
    pub transactions: Vec<VersionedTransaction>,
}

fn build_versioned_tx(
    ixs: &[Instruction],
    keypair: &Keypair,
    blockhash: Hash,
    alt: &AddressLookupTableAccount,
) -> Result<VersionedTransaction> {
    let message = v0::Message::try_compile(
        &keypair.pubkey(),
        ixs,
        &[alt.clone()],
        blockhash,
    )?;
    Ok(VersionedTransaction::try_new(VersionedMessage::V0(message), &[keypair])?)
}

impl JitoBundle {
    /// Build and sign a bundle from an ArbOpportunity using versioned transactions.
    ///
    /// Normal layout (enable_flash_loan=false):
    ///   tx[0..n-1] = swap instructions (one tx per hop)
    ///   tx[n]      = Jito tip transfer
    ///
    /// Flash loan layout (enable_flash_loan=true):
    ///   tx[0] = setup + all swaps + teardown
    ///   tx[1] = Jito tip transfer
    ///
    /// All transactions are v0 versioned with ALT compression.
    pub fn build(
        opportunity: &ArbOpportunity,
        keypair: &Keypair,
        recent_blockhash: Hash,
        config: &Config,
        alt: &AddressLookupTableAccount,
    ) -> Result<Self> {
        let payer = keypair.pubkey();
        let mut txs: Vec<VersionedTransaction> = Vec::new();

        let cu_price = config.compute_unit_price_micro_lamports;

        if config.enable_flash_loan {
            let cu_limit = config.compute_unit_limit.max(1_200_000) as u32;
            let mut ixs: Vec<Instruction> = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
            ];
            ixs.extend(opportunity.setup_instructions.iter().cloned());
            ixs.extend(opportunity.swap_instructions.iter().cloned());
            ixs.extend(opportunity.teardown_instructions.iter().cloned());
            txs.push(build_versioned_tx(&ixs, keypair, recent_blockhash, alt)?);
        } else {
            let cu_limit = config.compute_unit_limit as u32;
            let last_swap = opportunity.swap_instructions.len().saturating_sub(1);
            for (i, ix) in opportunity.swap_instructions.iter().enumerate() {
                let mut ixs: Vec<Instruction> = vec![
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
                txs.push(build_versioned_tx(&ixs, keypair, recent_blockhash, alt)?);
            }
        }

        // Tip transaction
        let tip_account = random_tip_account()?;
        let tip_ix = system_instruction::transfer(&payer, &tip_account, opportunity.jito_tip_lamports);
        txs.push(build_versioned_tx(&[tip_ix], keypair, recent_blockhash, alt)?);

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
                        "tx[{}] is {} bytes — exceeds Solana's 1232-byte limit",
                        i, bytes.len()
                    );
                }
                Ok(bs58::encode(bytes).into_string())
            })
            .collect()
    }

    #[allow(dead_code)]
    pub fn first_tx(&self) -> Option<&VersionedTransaction> {
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
