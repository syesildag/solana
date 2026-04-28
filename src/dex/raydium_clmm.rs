/// Raydium CLMM (Concentrated Liquidity Market Maker)
/// Program: CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK
///
/// Pool state account layout (after 8-byte Anchor discriminator):
///   bump: u8                    (offset 8)
///   amm_config: Pubkey (32)     (offset 9)
///   owner: Pubkey (32)          (offset 41)
///   token_mint_0: Pubkey (32)   (offset 73)
///   token_mint_1: Pubkey (32)   (offset 105)
///   token_vault_0: Pubkey (32)  (offset 137)
///   token_vault_1: Pubkey (32)  (offset 169)
///   observation_key: Pubkey(32) (offset 201)
///   mint_decimals_0: u8         (offset 233)
///   mint_decimals_1: u8         (offset 234)
///   tick_spacing: u16           (offset 235)
///   liquidity: u128             (offset 237)
///   sqrt_price_x64: u128        (offset 253)
///   tick_current: i32           (offset 269)
///   ...
///   fee_rate: u32               (offset at ~300 area — use amm_config lookup for accuracy)
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::dex::types::{Pool, SwapQuote};

const SQRT_PRICE_OFFSET: usize = 253;

/// Parse pool state to extract (price_a_to_b as f64, fee_bps).
/// price_a_to_b = (sqrt_price_x64 / 2^64)^2 = raw token_1 units per raw token_0 unit.
pub fn parse_state(data: &[u8]) -> Option<(f64, u64)> {
    if data.len() < SQRT_PRICE_OFFSET + 16 {
        return None;
    }
    let raw = u128::from_le_bytes(
        data[SQRT_PRICE_OFFSET..SQRT_PRICE_OFFSET + 16].try_into().ok()?,
    );
    let sqrt_price = raw as f64 / (1u128 << 64) as f64;
    let price = sqrt_price * sqrt_price;
    // Fee is stored in the linked amm_config account. Use a sensible default (30 bps).
    Some((price, 30))
}

/// Quote using sqrt_price-derived marginal rate, consistent with exchange_graph edge weights.
/// Vault-balance CP approximation is invalid for CLMM when balances are heavily skewed.
pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    use std::sync::atomic::Ordering;
    let fee_bps    = pool.fee_bps.load(Ordering::Relaxed);
    let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);

    let amount_out = if price_bits == 0 || amount_in == 0 {
        0
    } else {
        let price = f64::from_bits(price_bits); // token_1 per token_0 (raw units)
        let fee   = 1.0 - (fee_bps as f64 / 10_000.0);
        let raw   = if a_to_b { amount_in as f64 * price * fee }
                    else      { amount_in as f64 / price * fee };
        raw as u64
    };

    let fee_amount = amount_in * fee_bps / 10_000;
    SwapQuote { amount_in, amount_out, fee_amount, price_impact: 0.0, a_to_b }
}

/// CLMM swap instruction discriminator (Anchor hash of "global:swap_v2")
const SWAP_V2_DISCRIMINATOR: [u8; 8] = [0x43, 0x08, 0x4b, 0x6d, 0x0e, 0xf4, 0x61, 0x0b];

/// Build a swap_v2 instruction for Raydium CLMM.
pub fn build_swap_instruction(
    pool: &Pool,
    user_input_token: Pubkey,
    user_output_token: Pubkey,
    user_owner: Pubkey,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit_x64: u128,
    is_base_input: bool,
    a_to_b: bool,
) -> Result<Instruction> {
    let (input_vault, output_vault) = if a_to_b {
        (pool.vault_a, pool.vault_b)
    } else {
        (pool.vault_b, pool.vault_a)
    };
    let (input_mint, output_mint) = if a_to_b {
        (pool.token_a, pool.token_b)
    } else {
        (pool.token_b, pool.token_a)
    };

    let mut data = SWAP_V2_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit_x64.to_le_bytes());
    data.push(is_base_input as u8);

    // Required accounts per Raydium CLMM IDL
    let accounts = vec![
        AccountMeta::new_readonly(user_owner, true),
        AccountMeta::new(pool.id, false),
        AccountMeta::new(user_input_token, false),
        AccountMeta::new(user_output_token, false),
        AccountMeta::new(input_vault, false),
        AccountMeta::new(output_vault, false),
        AccountMeta::new_readonly(input_mint, false),
        AccountMeta::new_readonly(output_mint, false),
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new_readonly(spl_token_2022::id(), false),
        // tick arrays and observation must be provided at runtime
    ];

    Ok(Instruction {
        program_id: pool.dex.program_id(),
        accounts,
        data,
    })
}
