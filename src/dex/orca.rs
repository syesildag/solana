/// Orca Whirlpool (Concentrated Liquidity)
/// Program: whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc
///
/// Whirlpool state layout (after 8-byte Anchor discriminator):
///   whirlpools_config: Pubkey  (offset 8,  32 bytes)
///   whirlpool_bump: [u8; 1]    (offset 40)
///   tick_spacing: u16          (offset 41)
///   tick_spacing_seed: [u8;2]  (offset 43)
///   fee_rate: u16              (offset 45) — fee in hundredths of a bip (e.g. 300 = 0.03%)
///   protocol_fee_rate: u16     (offset 47)
///   liquidity: u128            (offset 49)
///   sqrt_price: u128           (offset 65)
///   tick_current_index: i32    (offset 81)
///   ...
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::dex::types::{Pool, SwapQuote};

const SQRT_PRICE_OFFSET: usize = 65;
const FEE_RATE_OFFSET: usize = 45;

/// Parse Whirlpool state account to extract (price_a_to_b as f64, fee_bps).
/// price_a_to_b = (sqrt_price_x64 / 2^64)^2 = raw token_b units per raw token_a unit.
pub fn parse_state(data: &[u8]) -> Option<(f64, u64)> {
    if data.len() < SQRT_PRICE_OFFSET + 16 {
        return None;
    }
    // sqrt_price_x64 is Q64.64: actual sqrt_price = value / 2^64
    // Computing as f64 first avoids u64 overflow for high-price pairs (e.g. BTC/USDC).
    let raw = u128::from_le_bytes(
        data[SQRT_PRICE_OFFSET..SQRT_PRICE_OFFSET + 16].try_into().ok()?,
    );
    let sqrt_price = raw as f64 / (1u128 << 64) as f64;
    let price = sqrt_price * sqrt_price;
    let fee_rate = u16::from_le_bytes(data[FEE_RATE_OFFSET..FEE_RATE_OFFSET + 2].try_into().ok()?);
    // fee_rate is in hundredths of a bip: 300 = 30 bps = 0.30%
    let fee_bps = fee_rate as u64 / 100;
    Some((price, fee_bps))
}

pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    let state = pool.snapshot_state();
    let amount_out = state.get_amount_out(amount_in, a_to_b);
    let fee_amount = amount_in * pool.fee_bps.load(std::sync::atomic::Ordering::Relaxed) / 10_000;
    let price_impact = pool.price_impact(amount_in, amount_out, a_to_b);
    SwapQuote { amount_in, amount_out, fee_amount, price_impact, a_to_b }
}

/// Anchor discriminator for "swap" instruction in Orca Whirlpool program.
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

/// Build a swap instruction for Orca Whirlpool.
/// tick_arrays and oracle must be derived off-chain based on the current tick index.
pub fn build_swap_instruction(
    pool: &Pool,
    token_authority: Pubkey,
    token_owner_account_a: Pubkey,
    token_owner_account_b: Pubkey,
    amount: u64,
    other_amount_threshold: u64,
    sqrt_price_limit: u128,
    amount_specified_is_input: bool,
    a_to_b: bool,
) -> Result<Instruction> {
    let extra = &pool.extra;
    let tick_array_0 = extra.tick_array_0.ok_or_else(|| anyhow::anyhow!("missing tick_array_0"))?;
    let tick_array_1 = extra.tick_array_1.ok_or_else(|| anyhow::anyhow!("missing tick_array_1"))?;
    let tick_array_2 = extra.tick_array_2.ok_or_else(|| anyhow::anyhow!("missing tick_array_2"))?;
    let oracle = extra.oracle.ok_or_else(|| anyhow::anyhow!("missing oracle"))?;

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(amount_specified_is_input as u8);
    data.push(a_to_b as u8);

    // Use the input token's program (Token or Token-2022). Mixed-program pools
    // (one Token, one Token-2022) require the Orca swap_v2 instruction format.
    let token_program = pool.token_program_for(a_to_b);
    let accounts = vec![
        AccountMeta::new_readonly(token_program, false),
        AccountMeta::new_readonly(token_authority, true),
        AccountMeta::new(pool.id, false),
        AccountMeta::new(token_owner_account_a, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(token_owner_account_b, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new(tick_array_0, false),
        AccountMeta::new(tick_array_1, false),
        AccountMeta::new(tick_array_2, false),
        AccountMeta::new_readonly(oracle, false),
    ];

    Ok(Instruction {
        program_id: pool.dex.program_id(),
        accounts,
        data,
    })
}
