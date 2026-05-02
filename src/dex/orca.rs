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
    use std::sync::atomic::Ordering;
    let fee_bps = pool.fee_bps.load(Ordering::Relaxed);
    let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);

    // Use sqrt_price-derived marginal rate, consistent with exchange_graph edge weights.
    // Vault-balance CP approximation is invalid for CLMM pools when balances are skewed
    // (price near a range boundary), producing wildly wrong outputs.
    let amount_out = if price_bits == 0 || amount_in == 0 {
        0
    } else {
        let price = f64::from_bits(price_bits); // token_b per token_a (raw units), stored by parse_state
        let fee   = 1.0 - (fee_bps as f64 / 10_000.0);
        let raw   = if a_to_b { amount_in as f64 * price * fee }
                    else      { amount_in as f64 / price * fee };
        raw as u64
    };

    let fee_amount = amount_in * fee_bps / 10_000;
    SwapQuote { amount_in, amount_out, fee_amount, price_impact: 0.0, a_to_b }
}

/// Anchor discriminator for "swap" instruction in Orca Whirlpool program.
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];

const TICK_ARRAY_SIZE: i32 = 88;
const ORCA_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// Derive the start tick index of the tick array containing `tick`.
fn tick_array_start(tick: i32, spacing: u16) -> i32 {
    let span = spacing as i32 * TICK_ARRAY_SIZE;
    let q = tick / span;
    (if tick < 0 && tick % span != 0 { q - 1 } else { q }) * span
}

/// Derive the Orca tick array PDA for a given start index.
fn tick_array_pda(pool_id: &Pubkey, start: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool_id.as_ref(), &start.to_le_bytes()],
        &ORCA_PROGRAM_ID,
    )
    .0
}

/// Derive the three consecutive tick array PDAs for the swap direction.
/// Tick arrays must be ordered in the direction of price travel:
///   a_to_b (price falls) → [current, current−span, current−2·span]
///   b_to_a (price rises) → [current, current+span, current+2·span]
fn swap_tick_arrays(pool_id: &Pubkey, price_bits: u64, tick_spacing: u16, a_to_b: bool) -> [Pubkey; 3] {
    let price = f64::from_bits(price_bits);
    let tick = if price > 0.0 && price.is_finite() {
        // Must floor, not truncate: for negative ticks, `as i32` truncates toward zero
        // (gives T+1 instead of T), selecting the wrong tick array at array boundaries.
        f64::floor(price.ln() / 1.0001_f64.ln()) as i32
    } else {
        0
    };
    let span = tick_spacing as i32 * TICK_ARRAY_SIZE;
    let start0 = tick_array_start(tick, tick_spacing);
    let (start1, start2) = if a_to_b {
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

/// Build a swap instruction for Orca Whirlpool.
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
    use std::sync::atomic::Ordering;
    let extra = &pool.extra;
    let oracle = extra.oracle.ok_or_else(|| anyhow::anyhow!("missing oracle"))?;

    // Prefer dynamic derivation: tick arrays depend on the swap direction and live price.
    // Static tick_array_0/1/2 baked into the JSON at fetch-time drift invalid as price
    // moves, causing ConstraintSeeds (2006) or InvalidTickArraySequence (6023) on-chain.
    // clmm_tick_spacing is written by fetch_orca_pools.js alongside the pool config.
    let [tick_array_0, tick_array_1, tick_array_2] =
        if let Some(tick_spacing) = extra.clmm_tick_spacing {
            let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
            if price_bits != 0 {
                swap_tick_arrays(&pool.id, price_bits, tick_spacing, a_to_b)
            } else {
                // Price not yet initialised from gRPC; fall back to static arrays.
                [
                    extra.tick_array_0.ok_or_else(|| anyhow::anyhow!("missing tick_array_0"))?,
                    extra.tick_array_1.ok_or_else(|| anyhow::anyhow!("missing tick_array_1"))?,
                    extra.tick_array_2.ok_or_else(|| anyhow::anyhow!("missing tick_array_2"))?,
                ]
            }
        } else {
            // tick_spacing absent (old JSON without clmm_tick_spacing); use static arrays.
            [
                extra.tick_array_0.ok_or_else(|| anyhow::anyhow!("missing tick_array_0"))?,
                extra.tick_array_1.ok_or_else(|| anyhow::anyhow!("missing tick_array_1"))?,
                extra.tick_array_2.ok_or_else(|| anyhow::anyhow!("missing tick_array_2"))?,
            ]
        };

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
