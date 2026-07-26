use anyhow::{anyhow, Result};
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
    // Meteora does not enforce any mint ordering, so we read the orientation from on-chain data
    // and cache it for use by build_swap_instruction.
    let token_x_in_state = &data[88..120];
    let is_a_token_x     = token_x_in_state == pool.token_a.as_ref();
    pool.dlmm_token_a_is_x.store(if is_a_token_x { 1 } else { 2 }, Ordering::Relaxed);

    let price = if is_a_token_x {
        raw_price_y_per_x          // token_b == token_y
    } else {
        1.0 / raw_price_y_per_x    // pool stored reversed; token_b == token_x
    };

    if !price.is_finite() || price <= 0.0 {
        return None;
    }

    // Dynamic-fee parameters ride the same account (StaticParameters @8..40,
    // VariableParameters @40..72) — decode on every lb_pair update so the fill
    // walk always sees the current volatility accumulator. try_write: skipping
    // one refresh under contention is harmless, blocking the stream callback
    // is not.
    let fee = types::DlmmFeeParams {
        base_factor: u16::from_le_bytes(data[8..10].try_into().ok()?),
        filter_period: u16::from_le_bytes(data[10..12].try_into().ok()?),
        decay_period: u16::from_le_bytes(data[12..14].try_into().ok()?),
        reduction_factor: u16::from_le_bytes(data[14..16].try_into().ok()?),
        variable_fee_control: u32::from_le_bytes(data[16..20].try_into().ok()?),
        max_volatility_accumulator: u32::from_le_bytes(data[20..24].try_into().ok()?),
        base_fee_power_factor: data[34],
        volatility_accumulator: u32::from_le_bytes(data[40..44].try_into().ok()?),
        volatility_reference: u32::from_le_bytes(data[44..48].try_into().ok()?),
        index_reference: i32::from_le_bytes(data[48..52].try_into().ok()?),
        last_update_timestamp: i64::from_le_bytes(data[56..64].try_into().ok()?),
    };
    if let Ok(mut cache) = pool.dlmm_bins.try_write() {
        cache.fee = fee;
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

    if price_bits == 0 || amount_in == 0 {
        return SwapQuote { amount_in, amount_out: 0, fee_amount: 0, price_impact: 0.0, a_to_b };
    }

    let price = f64::from_bits(price_bits); // token_b per token_a, raw units
    let fee   = 1.0 - (fee_bps as f64 / 10_000.0);
    let linear_out = if a_to_b { amount_in as f64 * price * fee }
                     else      { amount_in as f64 / price * fee };

    // DLMM is binned concentrated liquidity; the raw active-bin price assumes infinite depth
    // and overestimates the fill for anything that crosses bins — the documented cause of the
    // live ExceededAmountSlippageTolerance reverts on large inputs. Apply the SAME conservative
    // CP-marginal impact haircut Raydium CLMM already uses: reserve_in = the input-side vault
    // balance (total pool liquidity, a rough floor on real depth), impact = in/(reserve_in+in).
    // Directionally this UNDER-fills rather than over-fills, so min_out becomes achievable and
    // the evaluator stops sizing into reverts. No live reserve → fall back to raw (unchanged).
    let reserve_in = if a_to_b { pool.reserve_a.load(Ordering::Relaxed) }
                     else      { pool.reserve_b.load(Ordering::Relaxed) };
    let (amount_out, price_impact) = if reserve_in > 0 {
        let impact = amount_in as f64 / (reserve_in as f64 + amount_in as f64);
        ((linear_out * (1.0 - impact)) as u64, impact)
    } else {
        (linear_out as u64, 0.0)
    };

    let fee_amount = amount_in * fee_bps / 10_000;
    SwapQuote { amount_in, amount_out, fee_amount, price_impact, a_to_b }
}

// ── BinArray account decode (fill-walk data source) ─────────────────────────

/// BinArray account: 8-byte Anchor discriminator + index i64 @8 + version u8 @16
/// + 7 pad + lb_pair Pubkey @24..56 + [Bin; 70] @56. Each Bin is 144 bytes with
/// amount_x u64 @+0 and amount_y u64 @+8 (the only fields the fill walk needs).
/// Verified against MeteoraAg/dlmm-sdk idls/dlmm.json + a mainnet fixture (2026-07-27).
pub const BIN_ARRAY_LEN: usize = 10_136;
/// sha256("account:BinArray")[0..8]
pub const BIN_ARRAY_DISCRIMINATOR: [u8; 8] = [92, 142, 92, 220, 5, 148, 70, 181];
const BIN_SIZE: usize = 144;

/// Strict decode: exact length + discriminator or None — a future on-chain
/// layout change must degrade to the haircut quote, never corrupt it.
pub fn decode_bin_array(data: &[u8]) -> Option<(i64, Pubkey, [(u64, u64); 70])> {
    if data.len() != BIN_ARRAY_LEN || data[0..8] != BIN_ARRAY_DISCRIMINATOR {
        return None;
    }
    let index = i64::from_le_bytes(data[8..16].try_into().ok()?);
    let lb_pair = Pubkey::try_from(&data[24..56]).ok()?;
    let mut bins = [(0u64, 0u64); 70];
    for (i, bin) in bins.iter_mut().enumerate() {
        let off = 56 + i * BIN_SIZE;
        bin.0 = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
        bin.1 = u64::from_le_bytes(data[off + 8..off + 16].try_into().ok()?);
    }
    Some((index, lb_pair, bins))
}

/// Decode a streamed/polled BinArray account and store it in the pool's bin
/// cache, pruning the window to active-array ±2. Returns false (no store) on
/// decode failure or an lb_pair mismatch. Poisoned lock recovers via
/// into_inner — writers only ever replace whole entries.
pub fn store_bin_array(pool: &Pool, data: &[u8]) -> bool {
    let Some((index, lb_pair, bins)) = decode_bin_array(data) else { return false };
    if lb_pair != pool.id {
        return false;
    }
    let active_arr = pool.active_bin_id.load(Ordering::Relaxed).div_euclid(MAX_BIN_PER_ARRAY) as i64;
    let mut cache = match pool.dlmm_bins.write() {
        Ok(c) => c,
        Err(poisoned) => poisoned.into_inner(),
    };
    cache.arrays.insert(index, bins);
    cache.arrays.retain(|&idx, _| (idx - active_arr).abs() <= 2);
    cache.stamped_ns = types::monotonic_now_ns();
    true
}

/// Build a synthetic BinArray account image (tests only): every bin gets
/// (amount_x, amount_y) = `amounts`. Shared with the backfill test module.
#[cfg(test)]
pub(crate) fn synth_bin_array(lb_pair: &Pubkey, index: i64, amounts: (u64, u64)) -> Vec<u8> {
    let mut data = vec![0u8; BIN_ARRAY_LEN];
    data[0..8].copy_from_slice(&BIN_ARRAY_DISCRIMINATOR);
    data[8..16].copy_from_slice(&index.to_le_bytes());
    data[24..56].copy_from_slice(lb_pair.as_ref());
    for i in 0..70 {
        let off = 56 + i * BIN_SIZE;
        data[off..off + 8].copy_from_slice(&amounts.0.to_le_bytes());
        data[off + 8..off + 16].copy_from_slice(&amounts.1.to_le_bytes());
    }
    data
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

/// Build a Meteora DLMM swap2 instruction (exact-in, SPL Token pools).
///
/// Accounts (fixed order per swap2 IDL, 16 fixed):
///   lb_pair, bin_array_bitmap_extension (optional — pass program id if absent),
///   reserve_x, reserve_y, user_token_in, user_token_out,
///   token_x_mint, token_y_mint, oracle, host_fee_in,
///   user, token_x_program, token_y_program, memo_program,
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

    // Determine DLMM orientation from cached on-chain state (set by parse_state).
    // Meteora does NOT enforce any mint ordering when creating lb_pairs, so byte
    // comparison is unreliable — the pool creator decides which token is X.
    let orientation = pool.dlmm_token_a_is_x.load(Ordering::Relaxed);
    if orientation == 0 {
        return Err(anyhow!("DLMM pool {} token orientation not yet loaded from lb_pair state", pool.id));
    }
    let token_a_is_x = orientation == 1;
    let (token_x_mint, token_y_mint, reserve_x, reserve_y) = if token_a_is_x {
        (pool.token_a, pool.token_b, pool.vault_a, pool.vault_b)
    } else {
        (pool.token_b, pool.token_a, pool.vault_b, pool.vault_a)
    };

    // Per-token programs (classic SPL or Token-2022) — a Token-2022 mint's transfer CPI
    // fails IncorrectProgramId if handed the classic program. token_x is token_a iff
    // token_a_is_x, so its program is token_program_for(token_a_is_x); token_y the inverse.
    let token_x_program = pool.token_program_for(token_a_is_x);
    let token_y_program = pool.token_program_for(!token_a_is_x);

    // swap_for_y = true means selling X to get Y
    let swap_for_y = token_a_is_x == a_to_b;

    let oracle      = derive_pda(&[b"oracle",           lb_pair.as_ref()], &METEORA_DLMM_PUBKEY);
    let event_auth  = derive_pda(&[b"__event_authority"                 ], &METEORA_DLMM_PUBKEY);
    // bin_array_bitmap_extension is optional (only initialized for pools spanning >70 bins);
    // pass program id as None sentinel per Anchor optional-account convention.
    let bitmap_ext  = METEORA_DLMM_PUBKEY;

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

    // Instruction data: swap2 discriminant = sha256("global:swap2")[0..8] + borsh fields
    // Fields (borsh LE): amount_in: u64, min_amount_out: u64,
    //   remaining_accounts_info: Vec<RemainingAccountsSlice> — empty for SPL Token (4 zero bytes)
    let mut data = Vec::with_capacity(28);
    data.extend_from_slice(&[0x41, 0x4b, 0x3f, 0x4c, 0xeb, 0x5b, 0x5b, 0x88]); // sha256("global:swap2")[0..8]
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data.extend_from_slice(&[0u8; 4]); // empty Vec<RemainingAccountsSlice> (no Token-2022 hooks)

    // MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr — required by swap2 for Token-2022 memo support
    let memo_program = solana_sdk::pubkey!(
        "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"
    );

    let accounts = vec![
        AccountMeta::new(lb_pair,       false), // [0]  lb_pair (writable)
        AccountMeta::new_readonly(bitmap_ext, false), // [1]  bin_array_bitmap_extension (optional → program id)
        AccountMeta::new(reserve_x,     false), // [2]  reserve_x (writable)
        AccountMeta::new(reserve_y,     false), // [3]  reserve_y (writable)
        AccountMeta::new(user_src,      false), // [4]  user_token_in (writable)
        AccountMeta::new(user_dst,      false), // [5]  user_token_out (writable)
        AccountMeta::new_readonly(token_x_mint, false), // [6]  token_x_mint
        AccountMeta::new_readonly(token_y_mint, false), // [7]  token_y_mint
        AccountMeta::new(oracle,        false), // [8]  oracle (writable)
        AccountMeta::new_readonly(METEORA_DLMM_PUBKEY, false), // [9]  host_fee_in = program (no-op)
        AccountMeta::new_readonly(user,         true),  // [10] user (signer)
        AccountMeta::new_readonly(token_x_program, false), // [11] token_x_program
        AccountMeta::new_readonly(token_y_program, false), // [12] token_y_program
        AccountMeta::new_readonly(memo_program, false), // [13] memo_program (new in swap2)
        AccountMeta::new_readonly(event_auth,   false), // [14] event_authority
        AccountMeta::new_readonly(METEORA_DLMM_PUBKEY, false), // [15] program (self-ref CPI guard)
        // remaining accounts: bin arrays
        AccountMeta::new(bin_array_0,   false), // [16]
        AccountMeta::new(bin_array_1,   false), // [17]
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
            damm_virtual_price: AtomicU64::new(0),
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            last_update_ns: AtomicU64::new(0),
            extra: PoolExtra {
                dlmm_bin_step: Some(BIN_STEP),
                ..PoolExtra::default()
            },
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            // SOL/USDC: SOL < USDC in byte order → token_a (SOL) is X; confirmed by on-chain lb_pair
            dlmm_token_a_is_x: AtomicU64::new(1),
            dlmm_bins: Default::default(),
        })
    }

    #[test]
    fn get_quote_applies_reserve_impact_haircut() {
        let pool = sol_usdc_dlmm_pool();
        pool.sqrt_price_x64.store(1.0_f64.to_bits(), Ordering::Relaxed); // 1 token_b per token_a
        pool.fee_bps.store(0, Ordering::Relaxed);                        // isolate the impact term

        // No live reserve → raw active-bin price (unchanged fallback behaviour).
        pool.reserve_a.store(0, Ordering::Relaxed);
        let q0 = get_quote(&pool, 1_000, true);
        assert_eq!(q0.amount_out, 1_000, "no reserve → raw price, zero impact");
        assert_eq!(q0.price_impact, 0.0);

        // amount_in == reserve_in → impact = in/(reserve+in) = 0.5 → half the linear fill.
        pool.reserve_a.store(10_000, Ordering::Relaxed);
        let q1 = get_quote(&pool, 10_000, true);
        assert!((q1.price_impact - 0.5).abs() < 1e-9, "impact = in/(reserve+in)");
        assert_eq!(q1.amount_out, 5_000, "linear 10_000 × (1 − 0.5)");

        // Tiny swap against the same reserve → negligible haircut (≈ linear).
        let q2 = get_quote(&pool, 1, true);
        assert!(q2.price_impact < 1e-3 && q2.amount_out <= 1, "small swap ≈ linear");
    }

    #[test]
    fn swap_ix_has_exactly_18_accounts() {
        // 16 fixed + 2 bin array PDAs (current + neighbour)
        let pool = sol_usdc_dlmm_pool();
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            1_000_000, 0, true,
        ).unwrap();
        assert_eq!(ix.accounts.len(), 18, "DLMM swap2 requires 16 fixed + 2 bin array accounts");
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
        // [disc(8)] [amount_in(8)] [min_out(8)] [remaining_accounts_info(4)] = 28 bytes
        let pool = sol_usdc_dlmm_pool();
        let amount: u64 = 123_456_789;
        let ix = build_swap_instruction(
            &pool, Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique(),
            amount, 0, true,
        ).unwrap();
        assert_eq!(ix.data.len(), 28);
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
            // bitmap_ext (index 1), host_fee_in (index 9), program self-ref (index 15) use METEORA_DLMM_PUBKEY — that's fine
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
        // account[10] = user (signer) — unchanged position in swap2
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
        // accounts[16] = bin_array_0 (current), accounts[17] = bin_array_1 (adjacent)
        assert_eq!(ix_atob.accounts[16].pubkey, ix_btoa.accounts[16].pubkey, "current bin array same for both");
        assert_ne!(ix_atob.accounts[17].pubkey, ix_btoa.accounts[17].pubkey, "adjacent bin array differs by direction");
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

    // ─── BinArray decode ──────────────────────────────────────────────────────

    const FIXTURE_LB_PAIR: &str = "9t3EyC9FweyL7PBWvKz3mrXg8B9fwFc9SK3QxM4ENqhd";

    #[test]
    fn decode_bin_array_fixture_roundtrip() {
        let data: &[u8] = include_bytes!("../../tests/fixtures/dlmm/bin_array_1.bin");
        assert_eq!(data.len(), BIN_ARRAY_LEN);
        let (index, lb_pair, bins) = decode_bin_array(data).expect("fixture must decode");
        assert_eq!(lb_pair, Pubkey::from_str(FIXTURE_LB_PAIR).unwrap(),
            "lb_pair @24..56 must match the fixture pool");
        // A real mainnet snapshot has liquidity somewhere in the array.
        let total: u128 = bins.iter().map(|(x, y)| *x as u128 + *y as u128).sum();
        assert!(total > 0, "fixture bins all empty — layout offsets are wrong");
        // index sanity: bin ids covered = index*70 ..= index*70+69 — must be i32-representable
        assert!(index.checked_mul(70).and_then(|v| i32::try_from(v).ok()).is_some());
    }

    #[test]
    fn decode_bin_array_rejects_bad_input() {
        assert!(decode_bin_array(&[0u8; 100]).is_none(), "wrong length");
        let mut data = include_bytes!("../../tests/fixtures/dlmm/bin_array_1.bin").to_vec();
        data[0] ^= 0xFF;
        assert!(decode_bin_array(&data).is_none(), "wrong discriminator");
    }

    // ─── store_bin_array ──────────────────────────────────────────────────────

    #[test]
    fn store_bin_array_stores_prunes_and_stamps() {
        let pool = sol_usdc_dlmm_pool();
        pool.active_bin_id.store(0, Ordering::Relaxed); // active array = 0
        let id = pool.id;
        // Arrays -3..=3: only -2..=2 must survive the ±2 prune.
        for idx in -3i64..=3 {
            assert!(store_bin_array(&pool, &synth_bin_array(&id, idx, (5, 7))));
        }
        let cache = pool.dlmm_bins.read().unwrap();
        let kept: Vec<i64> = cache.arrays.keys().copied().collect();
        assert_eq!(kept, vec![-2, -1, 0, 1, 2]);
        assert_eq!(cache.arrays[&0][35], (5, 7));
        assert!(cache.stamped_ns > 0);
    }

    // ─── lb_pair fee-param decode ─────────────────────────────────────────────

    /// Minimal lb_pair image: StaticParameters @8..40, VariableParameters @40..72,
    /// active_id @76, token_x_mint @88..120 (offsets verified against the IDL).
    fn synth_lb_pair(active_id: i32, token_x: &Pubkey) -> Vec<u8> {
        let mut d = vec![0u8; 120];
        d[8..10].copy_from_slice(&5_000u16.to_le_bytes());     // base_factor
        d[10..12].copy_from_slice(&30u16.to_le_bytes());       // filter_period
        d[12..14].copy_from_slice(&600u16.to_le_bytes());      // decay_period
        d[14..16].copy_from_slice(&5_000u16.to_le_bytes());    // reduction_factor
        d[16..20].copy_from_slice(&40_000u32.to_le_bytes());   // variable_fee_control
        d[20..24].copy_from_slice(&350_000u32.to_le_bytes());  // max_volatility_accumulator
        d[34] = 0;                                             // base_fee_power_factor
        d[40..44].copy_from_slice(&123_456u32.to_le_bytes());  // volatility_accumulator
        d[44..48].copy_from_slice(&23_456u32.to_le_bytes());   // volatility_reference
        d[48..52].copy_from_slice(&(-42i32).to_le_bytes());    // index_reference
        d[56..64].copy_from_slice(&1_753_500_000i64.to_le_bytes()); // last_update_timestamp
        d[76..80].copy_from_slice(&active_id.to_le_bytes());
        d[88..120].copy_from_slice(token_x.as_ref());
        d
    }

    #[test]
    fn parse_state_decodes_fee_params_into_cache() {
        let pool = sol_usdc_dlmm_pool();
        let token_x = pool.token_a;
        let data = synth_lb_pair(-7, &token_x);
        let (price, _) = parse_state(&data, &pool).expect("must parse");
        assert!(price > 0.0);
        assert_eq!(pool.active_bin_id.load(Ordering::Relaxed), -7);
        let fee = pool.dlmm_bins.read().unwrap().fee;
        assert_eq!(fee.base_factor, 5_000);
        assert_eq!(fee.filter_period, 30);
        assert_eq!(fee.decay_period, 600);
        assert_eq!(fee.reduction_factor, 5_000);
        assert_eq!(fee.variable_fee_control, 40_000);
        assert_eq!(fee.max_volatility_accumulator, 350_000);
        assert_eq!(fee.volatility_accumulator, 123_456);
        assert_eq!(fee.volatility_reference, 23_456);
        assert_eq!(fee.index_reference, -42);
        assert_eq!(fee.last_update_timestamp, 1_753_500_000);
    }

    #[test]
    fn parse_state_fixture_lb_pair_fee_params_sane() {
        let data: &[u8] = include_bytes!("../../tests/fixtures/dlmm/lb_pair.bin");
        assert_eq!(&data[0..8], &[33, 11, 49, 98, 181, 101, 177, 13], "LbPair discriminator");
        let base_factor = u16::from_le_bytes(data[8..10].try_into().unwrap());
        let decay = u16::from_le_bytes(data[12..14].try_into().unwrap());
        let filter = u16::from_le_bytes(data[10..12].try_into().unwrap());
        assert!(base_factor > 0, "fixture base_factor zero — offsets wrong");
        assert!(decay > filter, "decay_period must exceed filter_period");
    }

    #[test]
    fn store_bin_array_rejects_foreign_lb_pair() {
        let pool = sol_usdc_dlmm_pool();
        let foreign = Pubkey::new_unique();
        assert!(!store_bin_array(&pool, &synth_bin_array(&foreign, 0, (1, 1))));
        assert!(pool.dlmm_bins.read().unwrap().arrays.is_empty());
    }
}
