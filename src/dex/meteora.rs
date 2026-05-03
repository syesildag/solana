/// Meteora Dynamic AMM (DAMM)
/// Program: Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EkAW7cP
///
/// DAMM uses a constant-product formula extended with virtual price reserves
/// and a dynamic fee that adjusts based on volatility.
///
/// Pool state layout (key fields, after 8-byte discriminator):
///   lp_mint: Pubkey (32)               (offset 8)
///   token_a_mint: Pubkey (32)          (offset 40)
///   token_b_mint: Pubkey (32)          (offset 72)
///   a_vault: Pubkey (32)               (offset 104)
///   b_vault: Pubkey (32)               (offset 136)
///   ...
///   fees.trade_fee_numerator: u64      (varies — default 25)
///   fees.trade_fee_denominator: u64    (varies — default 10000)
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::dex::types::{Pool, SwapQuote};

pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    use std::sync::atomic::Ordering;
    let fee_bps = pool.fee_bps.load(Ordering::Relaxed).max(25);

    let (amount_out, price_impact) = if pool.stable {
        let amp = pool.extra.damm_amp.unwrap_or(100);
        let ra = pool.reserve_a.load(Ordering::Relaxed);
        let rb = pool.reserve_b.load(Ordering::Relaxed);
        let vpr = pool.damm_virtual_price.load(Ordering::Relaxed);
        let price_scale = if vpr == 0 { crate::dex::stable_math::PRICE_SCALE } else { vpr };
        let out = crate::dex::stable_math::get_amount_out(amount_in, ra, rb, amp, fee_bps, price_scale, a_to_b);
        let reserve_in = if a_to_b { ra } else { rb };
        let impact = if reserve_in == 0 { 1.0 } else {
            amount_in as f64 / (reserve_in as f64 + amount_in as f64)
        };
        (out, impact)
    } else {
        let state = crate::dex::types::PoolState::ConstantProduct {
            reserve_a: pool.reserve_a.load(Ordering::Relaxed),
            reserve_b: pool.reserve_b.load(Ordering::Relaxed),
            fee_bps,
        };
        let out = state.get_amount_out(amount_in, a_to_b);
        let reserve_in = if a_to_b {
            pool.reserve_a.load(Ordering::Relaxed)
        } else {
            pool.reserve_b.load(Ordering::Relaxed)
        };
        let impact = if reserve_in == 0 { 1.0 } else {
            amount_in as f64 / (reserve_in as f64 + amount_in as f64)
        };
        (out, impact)
    };

    let fee_amount = amount_in * fee_bps / 10_000;
    SwapQuote { amount_in, amount_out, fee_amount, price_impact, a_to_b }
}

/// Anchor discriminator for Meteora DAMM "swap" instruction.
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xd0];

/// Meteora vault program: handles the underlying token/LP accounting.
const VAULT_PROGRAM: Pubkey = solana_sdk::pubkey!("24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi");

pub fn build_swap_instruction(
    pool: &Pool,
    user_source_token: Pubkey,
    user_destination_token: Pubkey,
    user: Pubkey,
    in_amount: u64,
    minimum_out_amount: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let ex = &pool.extra;

    let a_vault_lp      = ex.a_vault_lp      .ok_or_else(|| anyhow::anyhow!("DAMM missing a_vault_lp"))?;
    let b_vault_lp      = ex.b_vault_lp      .ok_or_else(|| anyhow::anyhow!("DAMM missing b_vault_lp"))?;
    let a_token_vault   = ex.a_token_vault   .ok_or_else(|| anyhow::anyhow!("DAMM missing a_token_vault"))?;
    let b_token_vault   = ex.b_token_vault   .ok_or_else(|| anyhow::anyhow!("DAMM missing b_token_vault"))?;
    let a_vault_lp_mint = ex.a_vault_lp_mint .ok_or_else(|| anyhow::anyhow!("DAMM missing a_vault_lp_mint"))?;
    let b_vault_lp_mint = ex.b_vault_lp_mint .ok_or_else(|| anyhow::anyhow!("DAMM missing b_vault_lp_mint"))?;
    let admin_token_fee = if a_to_b {
        ex.admin_token_fee_a.ok_or_else(|| anyhow::anyhow!("DAMM missing admin_token_fee_a"))?
    } else {
        ex.admin_token_fee_b.ok_or_else(|| anyhow::anyhow!("DAMM missing admin_token_fee_b"))?
    };

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&in_amount.to_le_bytes());
    data.extend_from_slice(&minimum_out_amount.to_le_bytes());

    // Full 15-account list required by Meteora DAMM swap instruction (Anchor IDL order):
    //  1. pool           — writable
    //  2. userSourceToken — writable
    //  3. userDestinationToken — writable
    //  4. aVault         — writable
    //  5. bVault         — writable
    //  6. aTokenVault    — writable (SPL token acct inside vault A)
    //  7. bTokenVault    — writable (SPL token acct inside vault B)
    //  8. aVaultLpMint   — writable (LP mint of vault A)
    //  9. bVaultLpMint   — writable (LP mint of vault B)
    // 10. aVaultLp       — writable (pool's LP acct in vault A)
    // 11. bVaultLp       — writable (pool's LP acct in vault B)
    // 12. adminTokenFee  — writable (fee receiver; direction-dependent)
    // 13. user           — signer
    // 14. vaultProgram   — readonly
    // 15. tokenProgram   — readonly
    let accounts = vec![
        AccountMeta::new(pool.id, false),
        AccountMeta::new(user_source_token, false),
        AccountMeta::new(user_destination_token, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new(a_token_vault, false),
        AccountMeta::new(b_token_vault, false),
        AccountMeta::new(a_vault_lp_mint, false),
        AccountMeta::new(b_vault_lp_mint, false),
        AccountMeta::new(a_vault_lp, false),
        AccountMeta::new(b_vault_lp, false),
        AccountMeta::new(admin_token_fee, false),
        AccountMeta::new_readonly(user, true),
        AccountMeta::new_readonly(VAULT_PROGRAM, false),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    Ok(Instruction {
        program_id: pool.dex.program_id(),
        accounts,
        data,
    })
}
