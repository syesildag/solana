use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use spl_associated_token_account::{
    get_associated_token_address,
    instruction::create_associated_token_account_idempotent,
};

use crate::config::FlashLoanConfig;
use crate::dex::types::WSOL_PUBKEY;

/// MarginFi v2 program ID (mainnet).
pub const MARGINFI_PROGRAM_ID: &str = "MFv2hWf31Z9kbCa1snEPdcgp8b3wL2KLJ95EAn3r4mJ";

/// Flash loan origination fee charged by MarginFi on the SOL bank.
/// Used in profit calculations as a conservative upper bound.
pub const FLASH_LOAN_FEE_BPS: u64 = 9;

/// Compute Anchor discriminator: sha256("global:<name>")[..8].
/// Uses solana_sdk's SHA-256 implementation — no extra crate needed.
fn anchor_discriminator(name: &str) -> [u8; 8] {
    use solana_sdk::hash::hash;
    let preimage = format!("global:{name}");
    hash(preimage.as_bytes()).to_bytes()[..8]
        .try_into()
        .expect("sha256 is always 32 bytes")
}

/// Derive the bank liquidity vault PDA for a given bank.
fn bank_liquidity_vault(bank: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"liquidity_vault", bank.as_ref()], program_id).0
}

/// Derive the bank liquidity vault authority PDA for a given bank.
fn bank_liquidity_vault_authority(bank: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"liquidity_vault_authority", bank.as_ref()], program_id).0
}

/// Count unique non-WSOL mints in a cycle path.
/// Used both here (to build ATAs) and in evaluator.rs (to compute end_index).
pub fn count_unique_non_wsol_mints(path: &[Pubkey]) -> usize {
    let mut seen = std::collections::HashSet::new();
    for &mint in path {
        if mint != WSOL_PUBKEY {
            seen.insert(mint);
        }
    }
    seen.len()
}

/// Build flash-loan setup instructions for tx[0]:
///   1. CreateATA (idempotent) for each unique non-WSOL intermediate mint
///   2. CreateATA (idempotent) for WSOL
///   3. LendingAccountStartFlashloan(end_index)
///   4. LendingAccountBorrow(amount_in) → WSOL arrives in user's WSOL ATA
///
/// Transaction layout assumed by end_index computation (ComputeBudget is tx[0] and tx[1]):
///   [0]      SetComputeUnitLimit
///   [1]      SetComputeUnitPrice
///   [2..N+1] CreateATA × N  (N = unique non-WSOL mints)
///   [N+2]    CreateATA(WSOL)
///   [N+3]    StartFlashloan
///   [N+4]    Borrow
///   [N+5..N+4+H] Swap × H
///   [N+5+H]  Repay
///   [N+6+H]  EndFlashloan  ← end_index = N + 6 + H
///   [N+7+H]  CloseAccount
pub fn build_setup_instructions(
    user: Pubkey,
    path: &[Pubkey],
    hops: usize,
    amount_in: u64,
    flash: &FlashLoanConfig,
) -> Vec<Instruction> {
    let program_id: Pubkey = MARGINFI_PROGRAM_ID.parse().expect("valid program id");
    let wsol_ata = get_associated_token_address(&user, &WSOL_PUBKEY);
    let bank_vault = bank_liquidity_vault(&flash.marginfi_sol_bank, &program_id);
    let bank_vault_authority = bank_liquidity_vault_authority(&flash.marginfi_sol_bank, &program_id);

    let mut ixs: Vec<Instruction> = Vec::new();

    // 1. CreateATA for each unique non-WSOL intermediate mint
    let mut seen = std::collections::HashSet::new();
    let n = count_unique_non_wsol_mints(path);
    for &mint in path {
        if mint != WSOL_PUBKEY && seen.insert(mint) {
            ixs.push(create_associated_token_account_idempotent(
                &user, &user, &mint, &spl_token::id(),
            ));
        }
    }

    // 2. CreateATA for WSOL
    ixs.push(create_associated_token_account_idempotent(
        &user, &user, &WSOL_PUBKEY, &spl_token::id(),
    ));

    // 3. StartFlashloan — end_index points to EndFlashloan instruction in the tx.
    //    end_index = 2 (compute budget) + N + 1 (wsol ata) + 1 (start) + 1 (borrow) + H (swaps) + 1 (repay) + 1 (end)
    //              = N + 6 + H
    let end_index: u64 = (n + 6 + hops) as u64;
    {
        let disc = anchor_discriminator("lending_account_start_flashloan");
        let mut data = disc.to_vec();
        data.extend_from_slice(&end_index.to_le_bytes());
        ixs.push(Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new(flash.marginfi_account, false),
                AccountMeta::new_readonly(user, true),
                AccountMeta::new_readonly(sysvar::instructions::id(), false),
            ],
            data,
        });
    }

    // 4. Borrow — MarginFi transfers WSOL from its vault into user's WSOL ATA.
    //    No sync_native needed: this is a standard SPL token::transfer, not a lamport deposit.
    {
        let disc = anchor_discriminator("lending_account_borrow");
        let mut data = disc.to_vec();
        data.extend_from_slice(&amount_in.to_le_bytes());
        ixs.push(Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(flash.marginfi_group, false),
                AccountMeta::new(flash.marginfi_account, false),
                AccountMeta::new(user, true),
                AccountMeta::new(flash.marginfi_sol_bank, false),
                AccountMeta::new(wsol_ata, false),
                AccountMeta::new_readonly(bank_vault_authority, false),
                AccountMeta::new(bank_vault, false),
                AccountMeta::new_readonly(spl_token::id(), false),
            ],
            data,
        });
    }

    ixs
}

/// Build flash-loan teardown instructions appended after the last swap:
///   1. LendingAccountRepay(repay_amount) — exact repayment from WSOL ATA
///   2. LendingAccountEndFlashloan — validates health; remaining accounts = (bank, oracle)
///   3. CloseAccount(WSOL ATA) — remaining WSOL profit unwrapped back to SOL
///
/// repay_amount = amount_in + ceil(amount_in * FLASH_LOAN_FEE_BPS / 10_000)
pub fn build_teardown_instructions(
    user: Pubkey,
    repay_amount: u64,
    flash: &FlashLoanConfig,
) -> Vec<Instruction> {
    let program_id: Pubkey = MARGINFI_PROGRAM_ID.parse().expect("valid program id");
    let wsol_ata = get_associated_token_address(&user, &WSOL_PUBKEY);
    let bank_vault = bank_liquidity_vault(&flash.marginfi_sol_bank, &program_id);

    let mut ixs: Vec<Instruction> = Vec::new();

    // 1. Repay — exact amount (repay_all=false) so we repay exactly what we owe.
    {
        let disc = anchor_discriminator("lending_account_repay");
        let mut data = disc.to_vec();
        data.extend_from_slice(&repay_amount.to_le_bytes());
        data.push(0u8); // repay_all = false
        ixs.push(Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(flash.marginfi_group, false),
                AccountMeta::new(flash.marginfi_account, false),
                AccountMeta::new(user, true),
                AccountMeta::new(flash.marginfi_sol_bank, false),
                AccountMeta::new(wsol_ata, false),
                AccountMeta::new(bank_vault, false),
                AccountMeta::new_readonly(spl_token::id(), false),
            ],
            data,
        });
    }

    // 2. EndFlashloan — MarginFi validates health via sysvar cross-check.
    //    Remaining accounts: (bank, oracle) so the health checker can price the SOL position.
    //    After full repayment the position balance is zero, so health trivially passes.
    {
        let disc = anchor_discriminator("lending_account_end_flashloan");
        ixs.push(Instruction {
            program_id,
            accounts: vec![
                AccountMeta::new_readonly(flash.marginfi_group, false),
                AccountMeta::new(flash.marginfi_account, false),
                AccountMeta::new_readonly(user, true),
                // remaining accounts: bank + oracle for the health check
                AccountMeta::new_readonly(flash.marginfi_sol_bank, false),
                AccountMeta::new_readonly(flash.marginfi_sol_bank_oracle, false),
            ],
            data: disc.to_vec(),
        });
    }

    // 3. CloseAccount — remaining WSOL (profit) becomes SOL in the user's wallet.
    ixs.push(
        spl_token::instruction::close_account(&spl_token::id(), &wsol_ata, &user, &user, &[])
            .expect("close_account is always valid"),
    );

    ixs
}
