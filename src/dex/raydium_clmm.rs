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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, Pool, PoolExtra};
    use std::sync::atomic::{AtomicI32, AtomicU64};
    use std::sync::Arc;

    // Real on-chain values for the SOL/RAY CLMM pool that triggers the
    // Custom(3007) AccountOwnedByWrongProgram error in simulation.
    const POOL_ID:     &str = "2AXXcN6oN9bBT5owwmTH53C7QHUXvhLeu718Kqt8rvY2";
    const AMM_CONFIG:  &str = "HfERMT5DRA6C1TAqecrJQFpmkf3wsWTMncqnj3RDg5aw";
    const OBSERVATION: &str = "DCURDhS5do6w9EytNmFxUNp3kYqXxfkv61Gs7FtLcH5a";
    const VAULT_A:     &str = "9Jgp8NpqEDFd5d3RQPfuRY7gMgRFByTNFmi68Ph1yvVb";
    const VAULT_B:     &str = "Be1CFyoPAr8aBGxpvCPD2LD21hdz2vjYNq8EcypnmgGD";
    const TOKEN_SOL:   &str = "So11111111111111111111111111111111111111112";
    const TOKEN_RAY:   &str = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";
    const TICK_SPACING: u16 = 10;
    const CURRENT_TICK: i32 = -22939;

    fn sol_ray_pool() -> Arc<Pool> {
        sol_ray_pool_with(CURRENT_TICK, Some(TICK_SPACING), true)
    }

    fn sol_ray_pool_with(tick: i32, spacing: Option<u16>, has_all_extra: bool) -> Arc<Pool> {
        use solana_sdk::pubkey::Pubkey;
        use std::str::FromStr;
        Arc::new(Pool {
            id:      Pubkey::from_str(POOL_ID).unwrap(),
            dex:     DexKind::RaydiumClmm,
            token_a: Pubkey::from_str(TOKEN_SOL).unwrap(),
            token_b: Pubkey::from_str(TOKEN_RAY).unwrap(),
            vault_a: Pubkey::from_str(VAULT_A).unwrap(),
            vault_b: Pubkey::from_str(VAULT_B).unwrap(),
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(25),
            sqrt_price_x64: AtomicU64::new(1), // non-zero so instruction builder proceeds
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(tick),
            state_account: None,
            stable: false,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra {
                clmm_amm_config:   if has_all_extra { Some(Pubkey::from_str(AMM_CONFIG).unwrap()) } else { None },
                clmm_observation:  if has_all_extra { Some(Pubkey::from_str(OBSERVATION).unwrap()) } else { None },
                clmm_tick_spacing: spacing,
                ..PoolExtra::default()
            },
        })
    }

    // ─── tick_array_start_index ───────────────────────────────────────────────

    #[test]
    fn tick_start_positive() {
        // span = tick_spacing * 60 = 600
        assert_eq!(tick_array_start_index(0, 10), 0);
        assert_eq!(tick_array_start_index(1, 10), 0);
        assert_eq!(tick_array_start_index(599, 10), 0);
        assert_eq!(tick_array_start_index(600, 10), 600);
        assert_eq!(tick_array_start_index(1199, 10), 600);
        assert_eq!(tick_array_start_index(1200, 10), 1200);
    }

    #[test]
    fn tick_start_negative() {
        // Rust truncates division toward zero, so we must floor manually.
        assert_eq!(tick_array_start_index(-1, 10), -600);
        assert_eq!(tick_array_start_index(-600, 10), -600);  // exact boundary stays in same array
        assert_eq!(tick_array_start_index(-601, 10), -1200);
        assert_eq!(tick_array_start_index(-1200, 10), -1200);
        assert_eq!(tick_array_start_index(-1201, 10), -1800);
    }

    #[test]
    fn tick_start_known_sol_ray_pool() {
        // tick -22939, spacing 10 → span=600 → start = -23400
        assert_eq!(tick_array_start_index(CURRENT_TICK, TICK_SPACING), -23400);
    }

    #[test]
    fn tick_start_exact_negative_boundary_not_double_floored() {
        // -600 is exactly at a boundary and must give -600, not -1200.
        assert_eq!(tick_array_start_index(-600, 10), -600);
    }

    // ─── swap_tick_arrays ─────────────────────────────────────────────────────

    #[test]
    fn a_to_b_tick_arrays_decrease() {
        // SOL→RAY (a_to_b=true) is zero-for-one: tick falls → arrays go downward.
        let pool_id = solana_sdk::pubkey::Pubkey::from_str(POOL_ID).unwrap();
        use std::str::FromStr;
        let arrays = swap_tick_arrays(&pool_id, CURRENT_TICK, TICK_SPACING, true);
        let span = TICK_SPACING as i32 * TICK_ARRAY_SIZE; // 600
        let start = tick_array_start_index(CURRENT_TICK, TICK_SPACING); // -23400
        assert_eq!(arrays[0], tick_array_pda(&pool_id, start));
        assert_eq!(arrays[1], tick_array_pda(&pool_id, start - span));       // -24000
        assert_eq!(arrays[2], tick_array_pda(&pool_id, start - 2 * span));   // -24600
    }

    #[test]
    fn b_to_a_tick_arrays_increase() {
        // RAY→SOL (a_to_b=false): tick rises → arrays go upward.
        use std::str::FromStr;
        let pool_id = solana_sdk::pubkey::Pubkey::from_str(POOL_ID).unwrap();
        let arrays = swap_tick_arrays(&pool_id, CURRENT_TICK, TICK_SPACING, false);
        let span = TICK_SPACING as i32 * TICK_ARRAY_SIZE;
        let start = tick_array_start_index(CURRENT_TICK, TICK_SPACING); // -23400
        assert_eq!(arrays[0], tick_array_pda(&pool_id, start));
        assert_eq!(arrays[1], tick_array_pda(&pool_id, start + span));       // -22800
        assert_eq!(arrays[2], tick_array_pda(&pool_id, start + 2 * span));   // -22200
    }

    #[test]
    fn opposite_directions_share_first_array_diverge_on_next() {
        use std::str::FromStr;
        let pool_id = solana_sdk::pubkey::Pubkey::from_str(POOL_ID).unwrap();
        let down = swap_tick_arrays(&pool_id, CURRENT_TICK, TICK_SPACING, true);
        let up   = swap_tick_arrays(&pool_id, CURRENT_TICK, TICK_SPACING, false);
        assert_eq!(down[0], up[0], "both directions start in the same tick array");
        assert_ne!(down[1], up[1], "arrays[1] must diverge by direction");
        assert_ne!(down[2], up[2], "arrays[2] must diverge by direction");
    }

    // ─── build_swap_instruction — structure ───────────────────────────────────

    #[test]
    fn swap_ix_has_exactly_16_accounts() {
        let pool = sol_ray_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        ).unwrap();
        assert_eq!(ix.accounts.len(), 16, "swap_v2 requires exactly 16 accounts");
    }

    #[test]
    fn swap_ix_targets_clmm_program() {
        let pool = sol_ray_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        ).unwrap();
        assert_eq!(ix.program_id, RAYDIUM_CLMM_PROGRAM);
    }

    #[test]
    fn swap_ix_data_starts_with_discriminator() {
        let pool = sol_ray_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        ).unwrap();
        assert_eq!(&ix.data[..8], &SWAP_V2_DISCRIMINATOR);
    }

    #[test]
    fn swap_ix_data_encodes_amount_at_byte_8() {
        // data layout: [discriminator(8)] [amount(8)] [threshold(8)] [sqrt_limit(16)] [is_base(1)]
        let pool = sol_ray_pool();
        let amount: u64 = 246_071_004;
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            amount, 0, 0, true, true,
        ).unwrap();
        let decoded = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(decoded, amount);
        assert_eq!(ix.data.len(), 41, "8 discriminator + 8 amount + 8 threshold + 16 sqrt_limit + 1 bool");
    }

    #[test]
    fn swap_ix_no_zero_pubkey_in_accounts() {
        // A zero pubkey in any slot means a missing extra field silently became default.
        let pool = sol_ray_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        ).unwrap();
        for (i, acct) in ix.accounts.iter().enumerate() {
            assert_ne!(
                acct.pubkey,
                Pubkey::default(),
                "account[{i}] is zero pubkey — likely a missing pool extra field"
            );
        }
    }

    #[test]
    fn swap_ix_signer_and_writable_flags() {
        let pool = sol_ray_pool();
        let owner = Pubkey::new_unique();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), owner,
            1_000_000, 0, 0, true, true,
        ).unwrap();
        assert!(ix.accounts[0].is_signer,   "account[0] (payer) must be signer");
        assert!(!ix.accounts[0].is_writable, "account[0] (payer) must be read-only");
        assert!(!ix.accounts[1].is_writable, "account[1] (amm_config) must be read-only");
        assert!(ix.accounts[2].is_writable,  "account[2] (pool_state) must be writable");
        assert!(ix.accounts[12].is_writable, "account[12] (tick_array_0) must be writable");
        assert!(ix.accounts[15].is_writable, "account[15] (observation) must be writable");
    }

    #[test]
    fn swap_ix_tick_arrays_match_computed_pdas_a_to_b() {
        use std::str::FromStr;
        let pool = sol_ray_pool();
        let pool_id = Pubkey::from_str(POOL_ID).unwrap();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true, // a_to_b
        ).unwrap();
        let expected = swap_tick_arrays(&pool_id, CURRENT_TICK, TICK_SPACING, true);
        assert_eq!(ix.accounts[12].pubkey, expected[0], "tick_array_0 mismatch");
        assert_eq!(ix.accounts[13].pubkey, expected[1], "tick_array_1 mismatch");
        assert_eq!(ix.accounts[14].pubkey, expected[2], "tick_array_2 mismatch");
    }

    #[test]
    fn swap_ix_a_to_b_uses_correct_vaults_and_mints() {
        use std::str::FromStr;
        let pool = sol_ray_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true, // a_to_b: SOL→RAY
        ).unwrap();
        // input_vault = vault_a (SOL), output_vault = vault_b (RAY)
        assert_eq!(ix.accounts[5].pubkey, Pubkey::from_str(VAULT_A).unwrap(), "input_vault (a_to_b) must be vault_a");
        assert_eq!(ix.accounts[6].pubkey, Pubkey::from_str(VAULT_B).unwrap(), "output_vault (a_to_b) must be vault_b");
        assert_eq!(ix.accounts[7].pubkey, Pubkey::from_str(TOKEN_SOL).unwrap(), "input_mint (a_to_b) must be SOL");
        assert_eq!(ix.accounts[8].pubkey, Pubkey::from_str(TOKEN_RAY).unwrap(), "output_mint (a_to_b) must be RAY");
    }

    #[test]
    fn swap_ix_b_to_a_flips_vaults_and_mints() {
        use std::str::FromStr;
        let pool = sol_ray_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, false, // b_to_a: RAY→SOL
        ).unwrap();
        assert_eq!(ix.accounts[5].pubkey, Pubkey::from_str(VAULT_B).unwrap(), "input_vault (b_to_a) must be vault_b");
        assert_eq!(ix.accounts[6].pubkey, Pubkey::from_str(VAULT_A).unwrap(), "output_vault (b_to_a) must be vault_a");
        assert_eq!(ix.accounts[7].pubkey, Pubkey::from_str(TOKEN_RAY).unwrap(), "input_mint (b_to_a) must be RAY");
        assert_eq!(ix.accounts[8].pubkey, Pubkey::from_str(TOKEN_SOL).unwrap(), "output_mint (b_to_a) must be SOL");
    }

    // ─── build_swap_instruction — failure modes ───────────────────────────────

    #[test]
    fn swap_ix_fails_when_price_uninitialized() {
        use std::sync::atomic::Ordering;
        let pool = sol_ray_pool();
        pool.sqrt_price_x64.store(0, Ordering::Relaxed);
        let result = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        );
        assert!(result.is_err(), "must fail when sqrt_price_x64 is 0 — tick unknown");
    }

    #[test]
    fn swap_ix_fails_without_amm_config() {
        let pool = sol_ray_pool_with(CURRENT_TICK, Some(TICK_SPACING), false);
        let result = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        );
        assert!(result.is_err(), "must fail when clmm_amm_config is None");
    }

    #[test]
    fn swap_ix_fails_without_tick_spacing() {
        let pool = sol_ray_pool_with(CURRENT_TICK, None, true);
        let result = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, 0, true, true,
        );
        assert!(result.is_err(), "must fail when clmm_tick_spacing is None");
    }

}
