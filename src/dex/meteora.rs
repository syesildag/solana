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
    let state = crate::dex::types::PoolState::ConstantProduct {
        reserve_a: pool.reserve_a.load(Ordering::Relaxed),
        reserve_b: pool.reserve_b.load(Ordering::Relaxed),
        fee_bps,
    };
    let amount_out = state.get_amount_out(amount_in, a_to_b);
    let fee_amount = amount_in * fee_bps / 10_000;
    let reserve_in = if a_to_b {
        pool.reserve_a.load(Ordering::Relaxed)
    } else {
        pool.reserve_b.load(Ordering::Relaxed)
    };
    let price_impact = if reserve_in == 0 {
        1.0
    } else {
        amount_in as f64 / (reserve_in as f64 + amount_in as f64)
    };
    SwapQuote { amount_in, amount_out, fee_amount, price_impact, a_to_b }
}

/// Anchor discriminator for Meteora DAMM "swap" instruction.
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xd0];

pub fn build_swap_instruction(
    pool: &Pool,
    user_source_token: Pubkey,
    user_destination_token: Pubkey,
    user: Pubkey,
    in_amount: u64,
    minimum_out_amount: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let (source_vault, dest_vault) = if a_to_b {
        (pool.vault_a, pool.vault_b)
    } else {
        (pool.vault_b, pool.vault_a)
    };

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&in_amount.to_le_bytes());
    data.extend_from_slice(&minimum_out_amount.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(pool.id, false),
        AccountMeta::new(user_source_token, false),
        AccountMeta::new(user_destination_token, false),
        AccountMeta::new(source_vault, false),
        AccountMeta::new(dest_vault, false),
        AccountMeta::new_readonly(user, true),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    Ok(Instruction {
        program_id: pool.dex.program_id(),
        accounts,
        data,
    })
}
