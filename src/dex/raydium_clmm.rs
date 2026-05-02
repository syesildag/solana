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
use anyhow::{anyhow, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::dex::types::{Pool, SwapQuote};

const SQRT_PRICE_OFFSET: usize = 253;
const TICK_ARRAY_SIZE: i32 = 60;

const RAYDIUM_CLMM_PROGRAM: Pubkey = solana_sdk::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// Parse pool state to extract (price_a_to_b as f64, fee_bps).
/// Returns fee_bps=0 — fee is stored in amm_config, not pool state.
/// Callers must guard `if fee_bps > 0` before overwriting the JSON-configured value.
///
/// If `expected_amm_config` is Some, the amm_config pubkey at offset 9–40 is
/// compared against the expected value. This guards against a wrong pool ID
/// pointing to an unrelated account whose bytes at offset 253 happen to decode
/// as a plausible sqrt_price, causing phantom graph edges.
pub fn parse_state(data: &[u8], expected_amm_config: Option<solana_sdk::pubkey::Pubkey>) -> Option<(f64, u64)> {
    if data.len() < SQRT_PRICE_OFFSET + 16 {
        return None;
    }
    if let Some(expected) = expected_amm_config {
        let actual = solana_sdk::pubkey::Pubkey::try_from(&data[9..41]).ok()?;
        if actual != expected {
            return None;
        }
    }
    let raw = u128::from_le_bytes(
        data[SQRT_PRICE_OFFSET..SQRT_PRICE_OFFSET + 16].try_into().ok()?,
    );
    let sqrt_price = raw as f64 / (1u128 << 64) as f64;
    let price = sqrt_price * sqrt_price;
    Some((price, 0))
}

/// Compute the start index of the tick array containing `tick`.
/// Tick arrays cover `tick_spacing * TICK_ARRAY_SIZE` ticks each.
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let span = tick_spacing as i32 * TICK_ARRAY_SIZE;
    // floor_div: rounds toward negative infinity
    let q = tick / span;
    let start = if tick < 0 && tick % span != 0 { q - 1 } else { q };
    start * span
}

/// Derive the tick array PDA for a given start index.
/// Raydium CLMM encodes the start index as big-endian (unlike Orca which uses little-endian).
pub fn tick_array_pda(pool_id: &Pubkey, start_index: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool_id.as_ref(), &start_index.to_be_bytes()],
        &RAYDIUM_CLMM_PROGRAM,
    )
    .0
}

/// Compute the three consecutive tick array PDAs needed for a swap.
/// Returns [current_array, next_array, next_next_array] PDAs.
pub fn swap_tick_arrays(pool_id: &Pubkey, tick: i32, tick_spacing: u16, a_to_b: bool) -> [Pubkey; 3] {
    let span = tick_spacing as i32 * TICK_ARRAY_SIZE;
    let start0 = tick_array_start_index(tick, tick_spacing);

    let (start1, start2) = if a_to_b {
        // Swapping a→b decreases tick (price falls)
        (start0 - span, start0 - 2 * span)
    } else {
        (start0 + span, start0 + 2 * span)
    };

    [
        tick_array_pda(pool_id, start0),
        tick_array_pda(pool_id, start1),
        tick_array_pda(pool_id, start2),
    ]
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

/// CLMM swap instruction discriminator = sha256("global:swap_v2")[0..8]
const SWAP_V2_DISCRIMINATOR: [u8; 8] = [0x2b, 0x04, 0xed, 0x0b, 0x1a, 0xc9, 0x1e, 0x62];

/// Memo program ID (needed by Raydium CLMM swap_v2)
const MEMO_PROGRAM: Pubkey = solana_sdk::pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

/// Build a swap_v2 instruction for Raydium CLMM.
/// All 16 accounts are required; tick arrays are derived from current sqrt_price at call time.
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
    use std::sync::atomic::Ordering;

    let amm_config = pool.extra.clmm_amm_config
        .ok_or_else(|| anyhow!("CLMM pool {} missing clmm_amm_config", pool.id))?;
    let observation = pool.extra.clmm_observation
        .ok_or_else(|| anyhow!("CLMM pool {} missing clmm_observation", pool.id))?;
    let tick_spacing = pool.extra.clmm_tick_spacing
        .ok_or_else(|| anyhow!("CLMM pool {} missing clmm_tick_spacing", pool.id))?;

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

    if pool.sqrt_price_x64.load(Ordering::Relaxed) == 0 {
        return Err(anyhow!("CLMM pool {} price not yet initialized — tick array derivation unsafe", pool.id));
    }
    let tick = pool.tick_current_index.load(Ordering::Relaxed);
    let tick_arrays = swap_tick_arrays(&pool.id, tick, tick_spacing, a_to_b);

    let mut data = SWAP_V2_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&other_amount_threshold.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit_x64.to_le_bytes());
    data.push(is_base_input as u8);

    // All 16 accounts required by Raydium CLMM swap_v2 IDL
    let accounts = vec![
        AccountMeta::new_readonly(user_owner, true),           // payer / authority
        AccountMeta::new_readonly(amm_config, false),          // amm_config
        AccountMeta::new(pool.id, false),                      // pool_state
        AccountMeta::new(user_input_token, false),             // input_token_account
        AccountMeta::new(user_output_token, false),            // output_token_account
        AccountMeta::new(input_vault, false),                  // input_vault
        AccountMeta::new(output_vault, false),                 // output_vault
        AccountMeta::new_readonly(input_mint, false),          // input_token_mint
        AccountMeta::new_readonly(output_mint, false),         // output_token_mint
        AccountMeta::new_readonly(spl_token::id(), false),     // token_program
        AccountMeta::new_readonly(spl_token_2022::id(), false),// token_program_2022
        AccountMeta::new_readonly(MEMO_PROGRAM, false),        // memo_program
        AccountMeta::new(tick_arrays[0], false),               // tick_array_0
        AccountMeta::new(tick_arrays[1], false),               // tick_array_1
        AccountMeta::new(tick_arrays[2], false),               // tick_array_2
        AccountMeta::new(observation, false),                  // observation_state
    ];

    Ok(Instruction {
        program_id: pool.dex.program_id(),
        accounts,
        data,
    })
}
