/// Invariant — Concentrated Liquidity Market Maker (CLMM).
///
/// Invariant uses Q64.64 sqrt_price_x64 and a tick-based layout functionally
/// identical to Orca Whirlpool for the fields the bot uses. get_quote delegates
/// to the Orca implementation; build_swap_instruction uses Invariant's own IDL.
///
/// Pool state layout (after 8-byte Anchor discriminator):
///   sqrt_price:          u128   offset 65   (Q64.64, same as Orca)
///   tick_current_index:  i32    offset 81   (same as Orca)
///
/// ⚠️ Verify these offsets against the Invariant program IDL before enabling
/// live pools: https://github.com/invariant-labs/protocol-solana
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::dex::types::{Pool, SwapQuote};

pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    // Invariant CLMM uses the same sqrt_price_x64 layout as Orca Whirlpool.
    crate::dex::orca::get_quote(pool, amount_in, a_to_b)
}

/// Anchor discriminator for Invariant swap instruction.
/// ⚠️ Placeholder — verify against the Invariant IDL before enabling live pools.
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc1];

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
    let tick_array_0 = ex.tick_array_0
        .ok_or_else(|| anyhow::anyhow!("Invariant missing tick_array_0"))?;
    let oracle = ex.oracle
        .ok_or_else(|| anyhow::anyhow!("Invariant missing oracle"))?;

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.push(a_to_b as u8);
    data.extend_from_slice(&in_amount.to_le_bytes());
    data.push(1u8); // by_amount_in = true
    data.extend_from_slice(&minimum_out_amount.to_le_bytes());

    // Invariant swap accounts (verify against IDL):
    //  0. pool_state          — writable
    //  1. user_source_token   — writable
    //  2. user_dest_token     — writable
    //  3. vault_a             — writable
    //  4. vault_b             — writable
    //  5. tick_array          — writable
    //  6. oracle              — writable
    //  7. user                — signer
    //  8. token_program       — readonly
    let accounts = vec![
        AccountMeta::new(pool.id, false),
        AccountMeta::new(user_source_token, false),
        AccountMeta::new(user_destination_token, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new(tick_array_0, false),
        AccountMeta::new(oracle, false),
        AccountMeta::new_readonly(user, true),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    Ok(Instruction { program_id: pool.dex.program_id(), accounts, data })
}
