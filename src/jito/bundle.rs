use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use solana_sdk::{
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};

/// CU limit per swap transaction. Conservative ceiling for 2-hop arb bundles;
/// most single-hop swaps use 150k–350k CU depending on DEX and tick crossings.
const COMPUTE_UNIT_LIMIT: u32 = 600_000;
/// Priority fee in micro-lamports per CU. At 600k CU this adds ~0.0006 lamports —
/// negligible cost, but signals tx priority to the block engine's internal filter.
const COMPUTE_UNIT_PRICE_MICRO_LAMPORTS: u64 = 1_000;

use crate::arbitrage::opportunity::ArbOpportunity;

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
    /// Layout:
    ///   tx[0..n-1] = swap instructions (one tx per hop)
    ///   tx[n]      = Jito tip transfer
    ///
    /// All transactions share the same recent blockhash so they land in the same block.
    pub fn build(
        opportunity: &ArbOpportunity,
        keypair: &Keypair,
        recent_blockhash: Hash,
    ) -> Result<Self> {
        let payer = keypair.pubkey();
        let mut txs: Vec<Transaction> = Vec::new();

        let last_swap = opportunity.swap_instructions.len().saturating_sub(1);

        // Build one transaction per swap instruction, with setup prepended to tx[0]
        // and teardown appended to the last swap tx.
        for (i, ix) in opportunity.swap_instructions.iter().enumerate() {
            // ComputeBudget instructions must be first in the transaction.
            let mut ixs: Vec<solana_sdk::instruction::Instruction> = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(COMPUTE_UNIT_LIMIT),
                ComputeBudgetInstruction::set_compute_unit_price(COMPUTE_UNIT_PRICE_MICRO_LAMPORTS),
            ];
            if i == 0 {
                ixs.extend(opportunity.setup_instructions.iter().cloned());
            }
            ixs.push(ix.clone());
            if i == last_swap {
                ixs.extend(opportunity.teardown_instructions.iter().cloned());
            }
            let tx = Transaction::new_signed_with_payer(
                &ixs,
                Some(&payer),
                &[keypair],
                recent_blockhash,
            );
            txs.push(tx);
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
    pub fn encode(&self) -> Result<Vec<String>> {
        self.transactions
            .iter()
            .map(|tx| {
                let bytes = bincode::serialize(tx)
                    .context("Failed to serialize transaction")?;
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
