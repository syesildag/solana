/// Saber — Curve StableSwap AMM for stablecoin and LST pairs.
///
/// Saber uses the same StableSwap invariant as Meteora DAMM stable pools.
/// Reserves are read from SPL token vault accounts (byte offset 64), same
/// as Raydium AMM V4 — no state account updates needed for pricing.
///
/// Typical amp parameters: 100 for LST pairs (jitoSOL/SOL), 500–2000 for
/// stablecoin pairs (USDC/USDT). Store per-pool as `extra.damm_amp`.
///
/// Pool state (SwapInfo) layout (after 1-byte tag):
///   is_initialized: bool     offset 1
///   token_a_mint:   Pubkey   offset 2   (+32)
///   token_b_mint:   Pubkey   offset 34  (+32)
///   token_a_vault:  Pubkey   offset 66  (+32)
///   token_b_vault:  Pubkey   offset 98  (+32)
///   swap_authority: Pubkey   offset 131 (+32)  (amm_authority in extra)
///
/// Reserves come from vault SPL token accounts (offset 64) — same subscription
/// path as Raydium AMM V4. No `state_account` subscription is needed.
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::sync::atomic::Ordering;

use crate::dex::types::{Pool, SwapQuote};

pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    use std::sync::atomic::Ordering;
    let fee_bps = pool.fee_bps.load(Ordering::Relaxed).max(4);
    let amp = pool.extra.damm_amp.unwrap_or(100);
    let ra = pool.reserve_a.load(Ordering::Relaxed);
    let rb = pool.reserve_b.load(Ordering::Relaxed);

    // Saber pools: use damm_virtual_price if set (LST/SOL pairs), else 1:1
    let vpr = pool.damm_virtual_price.load(Ordering::Relaxed);
    let price_scale = if vpr == 0 { crate::dex::stable_math::PRICE_SCALE } else { vpr };

    let amount_out = crate::dex::stable_math::get_amount_out(
        amount_in, ra, rb, amp, fee_bps, price_scale, a_to_b,
    );
    let reserve_in = if a_to_b { ra } else { rb };
    let price_impact = if reserve_in == 0 { 1.0 } else {
        amount_in as f64 / (reserve_in as f64 + amount_in as f64)
    };
    SwapQuote {
        amount_in,
        amount_out,
        fee_amount: amount_in * fee_bps / 10_000,
        price_impact,
        a_to_b,
    }
}

/// Anchor discriminator for Saber swap instruction.
/// ⚠️ Placeholder — verify against the Saber stable-swap IDL before enabling live pools.
/// See: https://github.com/saber-hq/stable-swap-program
const SWAP_DISCRIMINATOR: [u8; 8] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01];

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
    let swap_authority = ex.amm_authority
        .ok_or_else(|| anyhow::anyhow!("Saber missing swap_authority (amm_authority)"))?;
    let admin_fee_dst = if a_to_b {
        ex.admin_token_fee_a.ok_or_else(|| anyhow::anyhow!("Saber missing admin_token_fee_a"))?
    } else {
        ex.admin_token_fee_b.ok_or_else(|| anyhow::anyhow!("Saber missing admin_token_fee_b"))?
    };

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&in_amount.to_le_bytes());
    data.extend_from_slice(&minimum_out_amount.to_le_bytes());
    data.extend_from_slice(&u64::MAX.to_le_bytes()); // deadline

    // Saber swap accounts:
    //  0. swap_state          — readonly
    //  1. swap_authority      — readonly
    //  2. user                — signer
    //  3. user_source_token   — writable
    //  4. vault_a             — writable
    //  5. vault_b             — writable
    //  6. user_dest_token     — writable
    //  7. admin_fee_dst       — writable
    //  8. token_program       — readonly
    let accounts = vec![
        AccountMeta::new_readonly(pool.id, false),
        AccountMeta::new_readonly(swap_authority, false),
        AccountMeta::new_readonly(user, true),
        AccountMeta::new(user_source_token, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new(user_destination_token, false),
        AccountMeta::new(admin_fee_dst, false),
        AccountMeta::new_readonly(spl_token::id(), false),
    ];

    Ok(Instruction { program_id: pool.dex.program_id(), accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, PoolExtra};
    use std::sync::atomic::{AtomicI32, AtomicU64};
    use std::sync::Arc;

    fn mock_saber_pool(ra: u64, rb: u64, amp: u64) -> Arc<Pool> {
        Arc::new(Pool {
            id: solana_sdk::pubkey!("11111111111111111111111111111111"),
            dex: DexKind::Saber,
            token_a: solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            token_b: solana_sdk::pubkey!("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
            vault_a: solana_sdk::pubkey!("11111111111111111111111111111111"),
            vault_b: solana_sdk::pubkey!("11111111111111111111111111111111"),
            reserve_a: AtomicU64::new(ra),
            reserve_b: AtomicU64::new(rb),
            fee_bps: AtomicU64::new(4),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            stable: true,
            damm_virtual_price: AtomicU64::new(0),
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra { damm_amp: Some(amp), ..Default::default() },
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        })
    }

    #[test]
    fn get_quote_near_peg_usdc_usdt() {
        let pool = mock_saber_pool(50_000_000_000_000, 50_000_000_000_000, 100);
        let out = get_quote(&pool, 1_000_000, true);
        assert!(out.amount_out > 999_000 && out.amount_out < 1_000_000,
            "expected near 1:1 minus fee, got {}", out.amount_out);
    }

    #[test]
    fn get_quote_zero_reserves_returns_zero() {
        let pool = mock_saber_pool(0, 0, 100);
        let out = get_quote(&pool, 1_000_000, true);
        assert_eq!(out.amount_out, 0);
    }
}
