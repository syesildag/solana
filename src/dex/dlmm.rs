use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use crate::dex::types::{self, Pool, SwapQuote, METEORA_DLMM_PUBKEY};
use std::sync::atomic::Ordering;

/// Parse a Meteora DLMM LbPair state account and return (price_as_f64, fee_bps).
///
/// The price returned is "token_b raw per token_a raw" — the same convention used by
/// Raydium CLMM and Orca Whirlpool so that exchange_graph::update_pool can reuse the
/// same sqrt_price_x64 hot path without modification.
///
/// Formula: raw_price_y_per_x = (1 + binStep/10_000)^active_id
///
/// Where active_id (i32 at offset 76) encodes the current active bin, and the result
/// is the raw (lamport) price — token_y raw per token_x raw.  No decimal scaling is
/// needed because the DLMM on-chain price is already expressed in raw (lamport) units.
///
/// Direction is resolved by reading token_x_mint (offset 88, 32 bytes) and comparing
/// to pool.token_a.  If they match, token_b == token_y, and raw_price_y_per_x is the
/// answer.  Otherwise the pool is stored reversed, and we return 1 / raw_price.
///
/// We return fee_bps = 0 to preserve the value loaded at startup (pool.fee_bps is set
/// from pools.json and remains constant for the lifetime of the pool).
pub fn parse_state(data: &[u8], pool: &types::Pool) -> Option<(f64, u64)> {
    if data.len() < 120 {
        return None;
    }

    let active_id = i32::from_le_bytes(data[76..80].try_into().ok()?);
    pool.active_bin_id.store(active_id, Ordering::Relaxed);
    let bin_step   = pool.extra.dlmm_bin_step? as f64;

    let raw_price_y_per_x = (1.0_f64 + bin_step / 10_000.0).powi(active_id);

    if !raw_price_y_per_x.is_finite() || raw_price_y_per_x <= 0.0 {
        return None;
    }

    // token_x_mint is at offset 88 in the LbPair account (32 bytes).
    let token_x_in_state = &data[88..120];
    let is_a_token_x     = token_x_in_state == pool.token_a.as_ref();

    let price = if is_a_token_x {
        raw_price_y_per_x          // token_b == token_y
    } else {
        1.0 / raw_price_y_per_x    // pool stored reversed; token_b == token_x
    };

    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    Some((price, 0))
}

/// Quote a DLMM swap using the active-bin mid-price stored in sqrt_price_x64.
/// DLMM is a concentrated liquidity market maker (bin model); for routing purposes
/// the active-bin mid-price gives a good approximation.  Price impact is treated
/// as 0.0 (same as other CLMM-style pools) because the per-bin constant-sum model
/// cannot be approximated with a simple impact formula without full bin-array data.
pub fn get_quote(pool: &types::Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    let fee_bps    = pool.fee_bps.load(Ordering::Relaxed);
    let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);

    let amount_out = if price_bits == 0 || amount_in == 0 {
        0
    } else {
        let price = f64::from_bits(price_bits); // token_b per token_a, raw units
        let fee   = 1.0 - (fee_bps as f64 / 10_000.0);
        let raw   = if a_to_b { amount_in as f64 * price * fee }
                    else      { amount_in as f64 / price * fee };
        raw as u64
    };

    let fee_amount = amount_in * fee_bps / 10_000;
    SwapQuote { amount_in, amount_out, fee_amount, price_impact: 0.0, a_to_b }
}

// ── Meteora DLMM swap instruction ────────────────────────────────────────────
// Seeds:
//   oracle:                  ["oracle", lb_pair]
//   bin_array_bitmap_ext:    ["bitmap", lb_pair]
//   event_authority:         ["__event_authority"]
//   bin_array PDA:           ["bin_array", lb_pair, index_i64_le]  (index = active_id.div_euclid(70))
const MAX_BIN_PER_ARRAY: i32 = 70;

fn derive_pda(seeds: &[&[u8]], program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(seeds, program_id).0
}

/// Build a Meteora DLMM swap instruction (exact-in, SPL Token pools).
///
/// Accounts (fixed order per swap IDL):
///   lb_pair, bin_array_bitmap_extension, reserve_x, reserve_y,
///   user_token_in, user_token_out, token_x_mint, token_y_mint,
///   oracle, host_fee_in, user, token_x_program, token_y_program,
///   event_authority, program
///   + remaining: bin_array PDAs (current + neighbour toward swap direction)
///
/// token_x = min(token_a, token_b) — Meteora always sorts mints when creating pairs.
/// swap_for_y = a_to_b XOR (token_a > token_b)
pub fn build_swap_instruction(
    pool: &Pool,
    user_src: Pubkey,
    user_dst: Pubkey,
    user: Pubkey,
    amount_in: u64,
    min_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let lb_pair = pool.id;

    // Determine DLMM orientation: token_x = min(token_a, token_b)
    let token_a_is_x = pool.token_a < pool.token_b;
    let (token_x_mint, token_y_mint, reserve_x, reserve_y) = if token_a_is_x {
        (pool.token_a, pool.token_b, pool.vault_a, pool.vault_b)
    } else {
        (pool.token_b, pool.token_a, pool.vault_b, pool.vault_a)
    };

    // swap_for_y = true means selling X to get Y
    let swap_for_y = token_a_is_x == a_to_b;

    let oracle        = derive_pda(&[b"oracle",           lb_pair.as_ref()], &METEORA_DLMM_PUBKEY);
    let bitmap_ext    = derive_pda(&[b"bitmap",           lb_pair.as_ref()], &METEORA_DLMM_PUBKEY);
    let event_auth    = derive_pda(&[b"__event_authority"                 ], &METEORA_DLMM_PUBKEY);

    // Active bin's array index + neighbour in the swap direction
    let active_id = pool.active_bin_id.load(Ordering::Relaxed);
    let cur_idx = active_id.div_euclid(MAX_BIN_PER_ARRAY) as i64;
    let adj_idx = if swap_for_y { cur_idx + 1 } else { cur_idx - 1 };

    let bin_array_0 = derive_pda(
        &[b"bin_array", lb_pair.as_ref(), &cur_idx.to_le_bytes()],
        &METEORA_DLMM_PUBKEY,
    );
    let bin_array_1 = derive_pda(
        &[b"bin_array", lb_pair.as_ref(), &adj_idx.to_le_bytes()],
        &METEORA_DLMM_PUBKEY,
    );

    // Instruction data: swap discriminant = sha256("global:swap")[0..8] + borsh fields
    // Fields (borsh LE): amount_in: u64, min_amount_out: u64
    // swap (not swap2) — supports SPL Token only; swap2 requires an extra memo_program account
    // and a RemainingAccountsInfo arg. All our DLMM pools use standard SPL Token.
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&[0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8]); // sha256("global:swap")[0..8]
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(lb_pair,       false), // lb_pair (writable)
        AccountMeta::new(bitmap_ext,    false), // bin_array_bitmap_extension (writable)
        AccountMeta::new(reserve_x,     false), // reserve_x (writable)
        AccountMeta::new(reserve_y,     false), // reserve_y (writable)
        AccountMeta::new(user_src,      false), // user_token_in (writable)
        AccountMeta::new(user_dst,      false), // user_token_out (writable)
        AccountMeta::new_readonly(token_x_mint, false),
        AccountMeta::new_readonly(token_y_mint, false),
        AccountMeta::new(oracle,        false), // oracle (writable)
        AccountMeta::new_readonly(METEORA_DLMM_PUBKEY, false), // host_fee_in = program (no-op)
        AccountMeta::new_readonly(user,         true),  // user (signer)
        AccountMeta::new_readonly(spl_token::id(), false), // token_x_program
        AccountMeta::new_readonly(spl_token::id(), false), // token_y_program
        AccountMeta::new_readonly(event_auth,   false), // event_authority
        AccountMeta::new_readonly(METEORA_DLMM_PUBKEY, false), // program (self-ref CPI guard)
        // remaining accounts: bin arrays
        AccountMeta::new(bin_array_0,   false),
        AccountMeta::new(bin_array_1,   false),
    ];

    Ok(Instruction { program_id: METEORA_DLMM_PUBKEY, accounts, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, Pool, PoolExtra};
    use std::sync::atomic::{AtomicI32, AtomicU64};
    use std::sync::Arc;
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    const POOL_ID: &str = "HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR";
    const VAULT_A: &str = "H7j5NPopj3tQvDg4N8CxwtYciTn3e8AEV6wSVrxpyDUc";
    const VAULT_B: &str = "HbYjRzx7teCxqW3unpXBEcNHhfVZvW2vW9MQ99TkizWt";
    const TOKEN_A: &str = "So11111111111111111111111111111111111111112";
    const TOKEN_B: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const BIN_STEP: u16 = 1;
    // active_id=0 → bin_array_index=0 for cur and 1 (a_to_b) or -1 (b_to_a) for adjacent
    const ACTIVE_ID: i32 = 0;

    fn sol_usdc_dlmm_pool() -> Arc<Pool> {
        Arc::new(Pool {
            id:      Pubkey::from_str(POOL_ID).unwrap(),
            dex:     DexKind::MeteoraDlmm,
            token_a: Pubkey::from_str(TOKEN_A).unwrap(),
            token_b: Pubkey::from_str(TOKEN_B).unwrap(),
            vault_a: Pubkey::from_str(VAULT_A).unwrap(),
            vault_b: Pubkey::from_str(VAULT_B).unwrap(),
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(1),
            sqrt_price_x64: AtomicU64::new(1),
            active_bin_id: AtomicI32::new(ACTIVE_ID),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            stable: false,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra {
                dlmm_bin_step: Some(BIN_STEP),
                ..PoolExtra::default()
            },
        })
    }

    #[test]
    fn swap_ix_has_exactly_17_accounts() {
        // 15 fixed + 2 bin array PDAs (current + neighbour)
        let pool = sol_usdc_dlmm_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, true,
        ).unwrap();
        assert_eq!(ix.accounts.len(), 17, "DLMM swap requires 15 fixed + 2 bin array accounts");
    }

    #[test]
    fn swap_ix_targets_dlmm_program() {
        let pool = sol_usdc_dlmm_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, true,
        ).unwrap();
        assert_eq!(ix.program_id, METEORA_DLMM_PUBKEY);
    }

    #[test]
    fn swap_ix_data_encodes_amount_at_byte_8() {
        // [disc(8)] [amount_in(8)] [min_out(8)] = 24 bytes
        let pool = sol_usdc_dlmm_pool();
        let amount: u64 = 123_456_789;
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            amount, 0, true,
        ).unwrap();
        assert_eq!(ix.data.len(), 24);
        let decoded = u64::from_le_bytes(ix.data[8..16].try_into().unwrap());
        assert_eq!(decoded, amount);
    }

    #[test]
    fn swap_ix_no_zero_pubkey_in_accounts() {
        let pool = sol_usdc_dlmm_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, true,
        ).unwrap();
        for (i, acct) in ix.accounts.iter().enumerate() {
            // host_fee_in (index 9) and program self-ref (index 14) use METEORA_DLMM_PUBKEY — that's fine
            assert_ne!(acct.pubkey, Pubkey::default(), "account[{i}] is zero pubkey");
        }
    }

    #[test]
    fn swap_ix_user_is_signer() {
        let pool = sol_usdc_dlmm_pool();
        let user = Pubkey::new_unique();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), user,
            1_000_000, 0, true,
        ).unwrap();
        // account[10] = user (signer)
        assert_eq!(ix.accounts[10].pubkey, user, "account[10] must be user");
        assert!(ix.accounts[10].is_signer, "user must be signer");
    }

    #[test]
    fn swap_ix_a_to_b_and_b_to_a_yield_different_bin_arrays() {
        // The adjacent bin array flips direction based on swap_for_y.
        let pool = sol_usdc_dlmm_pool();
        let ix_atob = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, true,
        ).unwrap();
        let ix_btoa = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, false,
        ).unwrap();
        // accounts[15] = bin_array_0 (current), accounts[16] = bin_array_1 (adjacent)
        assert_eq!(ix_atob.accounts[15].pubkey, ix_btoa.accounts[15].pubkey, "current bin array same for both");
        assert_ne!(ix_atob.accounts[16].pubkey, ix_btoa.accounts[16].pubkey, "adjacent bin array differs by direction");
    }

    #[test]
    fn swap_ix_lb_pair_is_first_account_and_writable() {
        let pool = sol_usdc_dlmm_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, true,
        ).unwrap();
        assert_eq!(ix.accounts[0].pubkey, Pubkey::from_str(POOL_ID).unwrap(), "account[0] must be lb_pair");
        assert!(ix.accounts[0].is_writable, "lb_pair must be writable");
    }
}
