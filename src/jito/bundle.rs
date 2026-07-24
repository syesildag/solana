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
    alts: &[AddressLookupTableAccount],
) -> Result<VersionedTransaction> {
    let message = v0::Message::try_compile(
        &keypair.pubkey(),
        ixs,
        alts,
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
    ///   tx[1] = Jito tip transfer  (floor-anchored for thin cycles, ratio-based for fat cycles)
    ///
    /// All transactions are v0 versioned with ALT compression.
    /// Both thin (use_direct_rpc=true) and fat cycles go via Jito — raw RPC fails with
    /// v0+ALT on non-Jito validators. Thin cycles use a floor-only tip (~6_000L) instead
    /// of the ratio tip so the wallet keeps most of the profit.
    pub fn build(
        opportunity: &ArbOpportunity,
        keypair: &Keypair,
        recent_blockhash: Hash,
        config: &Config,
        alts: &[AddressLookupTableAccount],
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
            txs.push(build_versioned_tx(&ixs, keypair, recent_blockhash, alts)?);
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
                txs.push(build_versioned_tx(&ixs, keypair, recent_blockhash, alts)?);
            }
        }

        // Tip transaction — always included. Thin cycles use floor-anchored tip (~6_000L);
        // fat cycles use ratio-based tip. Both go via Jito for reliable v0+ALT handling.
        let tip_account = random_tip_account()?;
        let tip_ix = system_instruction::transfer(&payer, &tip_account, opportunity.jito_tip_lamports);
        txs.push(build_versioned_tx(&[tip_ix], keypair, recent_blockhash, alts)?);

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

/// Build the single no-ALT wallet-funded transaction for the raw-RPC path: ONE tx
/// `[CU-limit, CU-price, setup…, swaps…, teardown…]`, compiled with ZERO lookup
/// tables — a v0 message with no `address_table_lookups` needs no ALT resolution at
/// load time, so it cannot hit the non-Jito-validator ProgramAccountNotFound failure
/// and is valid on every leader. No tip transaction: the raw path pays only base +
/// priority fees. Both invariants (no lookups, ≤1232 bytes) are re-asserted here even
/// though the evaluator's size gate already vetted the shape.
pub fn build_raw_wallet_tx(
    opportunity: &ArbOpportunity,
    keypair: &Keypair,
    recent_blockhash: Hash,
    config: &Config,
) -> Result<VersionedTransaction> {
    let mut ixs: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(config.compute_unit_limit as u32),
        ComputeBudgetInstruction::set_compute_unit_price(config.compute_unit_price_micro_lamports),
    ];
    ixs.extend(opportunity.setup_instructions.iter().cloned());
    ixs.extend(opportunity.swap_instructions.iter().cloned());
    ixs.extend(opportunity.teardown_instructions.iter().cloned());

    let tx = build_versioned_tx(&ixs, keypair, recent_blockhash, &[])?;
    if let VersionedMessage::V0(m) = &tx.message {
        if !m.address_table_lookups.is_empty() {
            anyhow::bail!("raw tx unexpectedly carries ALT lookups");
        }
    }
    let bytes = bincode::serialize(&tx).context("Failed to serialize raw tx")?;
    if bytes.len() > 1232 {
        anyhow::bail!("raw tx is {} bytes — exceeds the 1232-byte wire limit", bytes.len());
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::bellman_ford::ArbCycle;

    fn ix_with_metas(n: usize) -> Instruction {
        use solana_sdk::instruction::AccountMeta;
        Instruction {
            program_id: Pubkey::new_unique(),
            accounts: (0..n).map(|_| AccountMeta::new(Pubkey::new_unique(), false)).collect(),
            data: vec![9, 0, 0, 0, 0, 0, 0, 0, 0],
        }
    }

    fn test_opp(swap_metas: usize) -> ArbOpportunity {
        let a = Pubkey::new_unique();
        let b = Pubkey::new_unique();
        ArbOpportunity {
            cycle: ArbCycle { path: vec![a, b, a], edges: vec![], total_weight: -0.01 },
            amount_in: 10_000_000,
            gross_out: 10_100_000,
            total_swap_fee_lamports: 0,
            tx_fee_lamports: 10_000,
            jito_tip_lamports: 12_000,
            net_profit_base_units: 50_000,
            swap_instructions: vec![ix_with_metas(swap_metas), ix_with_metas(swap_metas)],
            minimum_outputs: vec![9_900_000, 10_050_000],
            setup_instructions: vec![ix_with_metas(3)],
            teardown_instructions: vec![],
            flash_loan_fee_lamports: 0,
            use_direct_rpc: true,
            raw_rpc: true,
            jupiter_hops: vec![],
        }
    }

    #[test]
    fn raw_tx_has_no_alt_lookups_and_no_tip() {
        let opp = test_opp(6);
        let kp = Keypair::new();
        let cfg = Config::test_default();
        let tx = build_raw_wallet_tx(&opp, &kp, Hash::default(), &cfg).expect("must build");
        let VersionedMessage::V0(msg) = &tx.message else { panic!("must be v0") };
        assert!(msg.address_table_lookups.is_empty(), "raw tx must carry zero ALT lookups");
        // No Jito tip account may appear anywhere in the account keys.
        for tip in JITO_TIP_ACCOUNTS {
            let tip: Pubkey = tip.parse().unwrap();
            assert!(!msg.account_keys.contains(&tip), "raw tx must not pay a Jito tip");
        }
        // Instruction order: CU-limit, CU-price, setup, swap, swap (teardown empty).
        assert_eq!(msg.instructions.len(), 2 + 1 + 2, "CU×2 + setup + 2 swaps");
        assert_eq!(tx.signatures.len(), 1, "single signer");
    }

    #[test]
    fn raw_tx_bails_when_over_wire_limit() {
        // Two 20-meta swaps = 40+ unique keys ≈ >1280B of keys alone → must bail.
        let opp = test_opp(20);
        let kp = Keypair::new();
        let cfg = Config::test_default();
        let err = build_raw_wallet_tx(&opp, &kp, Hash::default(), &cfg).unwrap_err();
        assert!(err.to_string().contains("1232"), "got: {err}");
    }

    #[test]
    fn jito_bundle_shape_ignores_raw_rpc_flag() {
        // The Jito path must be byte-identical whether or not the opportunity is
        // raw-eligible (the transport decision lives in main.rs, not the bundle).
        let kp = Keypair::new();
        let cfg = Config::test_default();
        let mut opp = test_opp(6);
        opp.raw_rpc = true;
        let with_flag = JitoBundle::build(&opp, &kp, Hash::default(), &cfg, &[]).unwrap();
        opp.raw_rpc = false;
        let without_flag = JitoBundle::build(&opp, &kp, Hash::default(), &cfg, &[]).unwrap();
        assert_eq!(with_flag.transactions.len(), without_flag.transactions.len());
        // Compare all swap txs (tip tx destination is random — skip the last).
        for (a, b) in with_flag.transactions.iter().zip(&without_flag.transactions).take(with_flag.transactions.len() - 1) {
            assert_eq!(
                bincode::serialize(&a.message).unwrap(),
                bincode::serialize(&b.message).unwrap(),
                "swap tx bytes must not depend on raw_rpc"
            );
        }
    }
}
