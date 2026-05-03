/// Lifinity v2 — oracle-anchored AMM.
///
/// Unlike reserve-ratio AMMs, Lifinity anchors its price to a Pyth oracle feed.
/// The pool state stores the oracle-derived price as f64 bits in sqrt_price_x64
/// (set by parse_state at startup and on state-account updates). Arb opportunities
/// arise when the oracle price has moved but the pool's on-chain price hasn't been
/// updated yet — these windows last seconds rather than milliseconds.
///
/// Pool state layout (key fields, after 8-byte Anchor discriminator):
///   amm_config:    Pubkey (32)      offset 9
///   oracle:        Pubkey (32)      offset 74  (Pyth price account)
///   token_0_vault: Pubkey (32)      offset 138
///   token_1_vault: Pubkey (32)      offset 170
///   price:         u64 (f64 bits)   offset 273 (oracle price, token_b per token_a)
///
/// NOTE: byte offsets above need on-chain verification before enabling live pools.
/// Use: solana account <pool_state> --output json | python3 -c "import base64,struct,json,sys;
///      d=base64.b64decode(json.load(sys.stdin)['account']['data'][0]);
///      [print(f'off {o}: {struct.unpack_from(\"<Q\",d,o)[0]}') for o in range(265,290,8)]"
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::sync::atomic::Ordering;

use crate::dex::types::{Pool, SwapQuote};

pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
    if price_bits == 0 {
        return SwapQuote { amount_in, amount_out: 0, fee_amount: 0, price_impact: 1.0, a_to_b };
    }
    // Price stored as f64 bits: token_b per token_a (raw decimal units)
    let price = f64::from_bits(price_bits);
    let fee_bps = pool.fee_bps.load(Ordering::Relaxed).max(10);
    let fee_mult = 1.0 - fee_bps as f64 / 10_000.0;

    let amount_out = if a_to_b {
        (amount_in as f64 * price * fee_mult) as u64
    } else if price > 0.0 {
        (amount_in as f64 / price * fee_mult) as u64
    } else {
        0
    };

    let reserve_in = if a_to_b {
        pool.reserve_a.load(Ordering::Relaxed)
    } else {
        pool.reserve_b.load(Ordering::Relaxed)
    };
    let price_impact = if reserve_in == 0 { 1.0 } else {
        amount_in as f64 / (reserve_in as f64 + amount_in as f64)
    };
    SwapQuote { amount_in, amount_out, fee_amount: amount_in * fee_bps / 10_000, price_impact, a_to_b }
}

/// Anchor discriminator for Lifinity v2 swap instruction.
/// ⚠️ Placeholder — verify against the Lifinity v2 IDL before enabling live pools.
const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xd0];

pub fn build_swap_instruction(
    pool: &Pool,
    user_source_token: Pubkey,
    user_destination_token: Pubkey,
    user: Pubkey,
    in_amount: u64,
    minimum_out_amount: u64,
    _a_to_b: bool,
) -> Result<Instruction> {
    let ex = &pool.extra;
    let amm_config = ex.clmm_amm_config
        .ok_or_else(|| anyhow::anyhow!("Lifinity missing amm_config"))?;
    let oracle = ex.oracle
        .ok_or_else(|| anyhow::anyhow!("Lifinity missing oracle"))?;

    let mut data = SWAP_DISCRIMINATOR.to_vec();
    data.extend_from_slice(&in_amount.to_le_bytes());
    data.extend_from_slice(&minimum_out_amount.to_le_bytes());

    // Lifinity v2 swap account order (verify against IDL):
    //  0. amm_config          — readonly
    //  1. pool_state          — writable
    //  2. user_source_token   — writable
    //  3. user_dest_token     — writable
    //  4. vault_a             — writable
    //  5. vault_b             — writable
    //  6. oracle              — readonly
    //  7. user                — signer
    //  8. token_program       — readonly
    let accounts = vec![
        AccountMeta::new_readonly(amm_config, false),
        AccountMeta::new(pool.id, false),
        AccountMeta::new(user_source_token, false),
        AccountMeta::new(user_destination_token, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new_readonly(oracle, false),
        AccountMeta::new_readonly(user, true),
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

    fn mock_pool(price_bits: u64, fee_bps: u64) -> Arc<Pool> {
        Arc::new(Pool {
            id: solana_sdk::pubkey!("11111111111111111111111111111111"),
            dex: DexKind::Lifinity,
            token_a: solana_sdk::pubkey!("So11111111111111111111111111111111111111112"),
            token_b: solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
            vault_a: solana_sdk::pubkey!("11111111111111111111111111111111"),
            vault_b: solana_sdk::pubkey!("11111111111111111111111111111111"),
            reserve_a: AtomicU64::new(1_000_000_000_000),
            reserve_b: AtomicU64::new(150_000_000_000),
            fee_bps: AtomicU64::new(fee_bps),
            sqrt_price_x64: AtomicU64::new(price_bits),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        })
    }

    #[test]
    fn get_quote_nonzero_price_returns_output() {
        // SOL/USDC ≈ 150: price = 0.15 (USDC per SOL in raw 6-dec/9-dec units)
        let price: f64 = 0.15;
        let pool = mock_pool(price.to_bits(), 10);
        let q = get_quote(&pool, 1_000_000_000, true); // 1 SOL
        assert!(q.amount_out > 0, "expected nonzero output, got {}", q.amount_out);
    }

    #[test]
    fn get_quote_zero_price_returns_zero() {
        let pool = mock_pool(0, 10);
        let q = get_quote(&pool, 1_000_000_000, true);
        assert_eq!(q.amount_out, 0);
    }

    #[test]
    fn get_quote_b_to_a_divides_by_price() {
        let price: f64 = 0.15;
        let pool = mock_pool(price.to_bits(), 0);
        // 150 USDC → should give ≈ 1 SOL (minus zero fee)
        let q = get_quote(&pool, 150_000_000, false);
        // 150e6 / 0.15 = 1e9
        assert!(q.amount_out > 990_000_000 && q.amount_out < 1_010_000_000,
            "expected ≈1 SOL, got {}", q.amount_out);
    }
}
