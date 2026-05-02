/// Raydium AMM V4 (constant-product, OpenBook-backed)
/// Program: 675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8
///
/// Fee: 25 bps (trade_fee_numerator = 25, trade_fee_denominator = 10000)
/// Reserves: read from SPL vault token accounts (vault_a, vault_b)
///
/// Swap instruction:
///   discriminator byte: 9 (SwapBaseIn) or 10 (SwapBaseOut)
///   data layout: [u8; 1] ++ [u64 amount_in] ++ [u64 minimum_amount_out]
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::dex::types::{Pool, SwapQuote};

pub const FEE_NUMERATOR: u64 = 25;
pub const FEE_DENOMINATOR: u64 = 10_000;

/// Compute swap output for a constant-product pool with 0.25% fee.
pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    use std::sync::atomic::Ordering;
    let (reserve_in, reserve_out) = if a_to_b {
        (
            pool.reserve_a.load(Ordering::Relaxed),
            pool.reserve_b.load(Ordering::Relaxed),
        )
    } else {
        (
            pool.reserve_b.load(Ordering::Relaxed),
            pool.reserve_a.load(Ordering::Relaxed),
        )
    };

    let fee_amount = amount_in * FEE_NUMERATOR / FEE_DENOMINATOR;
    let amount_in_with_fee = amount_in - fee_amount;

    if reserve_in == 0 {
        return SwapQuote { amount_in, amount_out: 0, fee_amount, price_impact: 1.0, a_to_b };
    }

    let numerator = (reserve_out as u128) * (amount_in_with_fee as u128);
    let denominator = (reserve_in as u128) + (amount_in_with_fee as u128);
    let amount_out = if denominator == 0 {
        0
    } else {
        (numerator / denominator) as u64
    };

    let price_impact = if reserve_in == 0 {
        1.0
    } else {
        amount_in as f64 / (reserve_in as f64 + amount_in as f64)
    };

    SwapQuote { amount_in, amount_out, fee_amount, price_impact, a_to_b }
}

/// Build a SwapBaseIn instruction for Raydium AMM V4.
///
/// Account ordering (17 accounts) matches the on-chain program IDL.
/// All pubkeys are pool-specific and must be populated in Pool.extra.
pub fn build_swap_instruction(
    pool: &Pool,
    user_source: Pubkey,
    user_destination: Pubkey,
    user_owner: Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let extra = &pool.extra;
    let amm_authority = extra.amm_authority.ok_or_else(|| anyhow::anyhow!("missing amm_authority"))?;
    let open_orders = extra.open_orders.ok_or_else(|| anyhow::anyhow!("missing open_orders"))?;
    let target_orders = extra.target_orders.ok_or_else(|| anyhow::anyhow!("missing target_orders"))?;
    let market_program = extra.market_program.ok_or_else(|| anyhow::anyhow!("missing market_program"))?;
    let market = extra.market.ok_or_else(|| anyhow::anyhow!("missing market"))?;
    let market_bids = extra.market_bids.ok_or_else(|| anyhow::anyhow!("missing market_bids"))?;
    let market_asks = extra.market_asks.ok_or_else(|| anyhow::anyhow!("missing market_asks"))?;
    let market_event_queue = extra.market_event_queue.ok_or_else(|| anyhow::anyhow!("missing market_event_queue"))?;
    let market_coin_vault = extra.market_coin_vault.ok_or_else(|| anyhow::anyhow!("missing market_coin_vault"))?;
    let market_pc_vault = extra.market_pc_vault.ok_or_else(|| anyhow::anyhow!("missing market_pc_vault"))?;
    let market_vault_signer = extra.market_vault_signer.ok_or_else(|| anyhow::anyhow!("missing market_vault_signer"))?;

    let (pool_coin_vault, pool_pc_vault) = if a_to_b {
        (pool.vault_a, pool.vault_b)
    } else {
        (pool.vault_b, pool.vault_a)
    };

    // instruction data: [9u8, amount_in as LE u64, minimum_amount_out as LE u64]
    let mut data = vec![9u8]; // SwapBaseIn discriminator
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&minimum_amount_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(spl_token::id(), false),
        AccountMeta::new(pool.id, false),
        AccountMeta::new_readonly(amm_authority, false),
        AccountMeta::new(open_orders, false),
        AccountMeta::new(target_orders, false),
        AccountMeta::new(pool_coin_vault, false),
        AccountMeta::new(pool_pc_vault, false),
        AccountMeta::new_readonly(market_program, false),
        AccountMeta::new(market, false),
        AccountMeta::new(market_bids, false),
        AccountMeta::new(market_asks, false),
        AccountMeta::new(market_event_queue, false),
        AccountMeta::new(market_coin_vault, false),
        AccountMeta::new(market_pc_vault, false),
        AccountMeta::new_readonly(market_vault_signer, false),
        AccountMeta::new(user_source, false),
        AccountMeta::new(user_destination, false),
        AccountMeta::new_readonly(user_owner, true),
    ];

    Ok(Instruction {
        program_id: pool.dex.program_id(),
        accounts,
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, PoolExtra};
    use solana_sdk::pubkey::Pubkey;
    use std::sync::atomic::{AtomicI32, AtomicU64};
    use std::sync::Arc;

    fn mock_pool(reserve_a: u64, reserve_b: u64) -> Arc<Pool> {
        Arc::new(Pool {
            id: Pubkey::new_unique(),
            dex: DexKind::RaydiumAmmV4,
            token_a: Pubkey::new_unique(),
            token_b: Pubkey::new_unique(),
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(reserve_a),
            reserve_b: AtomicU64::new(reserve_b),
            fee_bps: AtomicU64::new(25),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
            stable: false,
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
        })
    }

    // ─── fee behaviour ────────────────────────────────────────────────────────

    #[test]
    fn fee_amount_is_25_bps_of_input() {
        let q = get_quote(&mock_pool(1_000_000, 1_000_000), 10_000, true);
        // 10_000 * 25 / 10_000 = 25
        assert_eq!(q.fee_amount, 25);
    }

    #[test]
    fn output_is_less_than_input_on_equal_pool() {
        // Even with a 1:1 pool, fee makes output < input
        let q = get_quote(&mock_pool(10_000_000, 10_000_000), 100_000, true);
        assert!(q.amount_out < q.amount_in);
    }

    // ─── edge cases ───────────────────────────────────────────────────────────

    #[test]
    fn zero_input_gives_zero_output_and_zero_fee() {
        let q = get_quote(&mock_pool(1_000_000, 1_000_000), 0, true);
        assert_eq!(q.amount_out, 0);
        assert_eq!(q.fee_amount, 0);
    }

    #[test]
    fn zero_reserve_in_gives_zero_output() {
        let q = get_quote(&mock_pool(0, 1_000_000), 10_000, true);
        assert_eq!(q.amount_out, 0);
    }

    #[test]
    fn zero_reserve_out_gives_zero_output() {
        let q = get_quote(&mock_pool(1_000_000, 0), 10_000, true);
        assert_eq!(q.amount_out, 0);
    }

    // ─── directional symmetry ─────────────────────────────────────────────────

    #[test]
    fn equal_reserves_give_same_quote_both_directions() {
        let pool = mock_pool(1_000_000, 1_000_000);
        assert_eq!(
            get_quote(&pool, 50_000, true).amount_out,
            get_quote(&pool, 50_000, false).amount_out,
        );
    }

    // ─── round-trip invariant ─────────────────────────────────────────────────

    #[test]
    fn round_trip_always_returns_less_than_input() {
        let pool = mock_pool(50_000_000, 50_000_000);
        let q1 = get_quote(&pool, 1_000_000, true);
        let q2 = get_quote(&pool, q1.amount_out, false);
        assert!(
            q2.amount_out < 1_000_000,
            "round-trip returned {}, expected < 1_000_000",
            q2.amount_out
        );
    }

    // ─── price impact ─────────────────────────────────────────────────────────

    #[test]
    fn larger_trade_has_higher_price_impact() {
        let pool = mock_pool(10_000_000, 10_000_000);
        let small = get_quote(&pool, 10_000, true);
        let large = get_quote(&pool, 1_000_000, true);
        assert!(large.price_impact > small.price_impact);
    }
}
