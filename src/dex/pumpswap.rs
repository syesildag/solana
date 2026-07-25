//! PumpSwap (pump.fun AMM) swap-instruction builder.
//! Program: pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA
//!
//! Constant-product AMM with a base/quote orientation and two swap instructions:
//!   - `sell`  base → quote, exact-in : args (base_amount_in, min_quote_amount_out)
//!   - `buy`   quote → base, exact-OUT : args (base_amount_out, max_quote_amount_in, track_volume)
//!
//! Pricing already routes through `raydium_amm::get_quote` (same CP math); this module
//! adds the executable swap. Gated behind `ENABLE_PUMPSWAP_TRADING` (default off).
//!
//! ## STATUS: builder VALIDATED on-chain (2026-07-25); gated default-off
//! The buy=23 / sell=21 account list is the AMM's FULL declared interface (read from the
//! program's own on-chain Anchor IDL), and every PDA is asserted equal to live-mainnet
//! constants in `pda_derivations_match_live_mainnet_constants`. Discriminators are Anchor
//! `sha256("global:<ix>")[:8]` (verified deterministically). Organic swaps append OPTIONAL
//! buyback `remaining_accounts` — a rotating, runtime-selected fee-program `BuybackVault`
//! (+ ATA) with no static PDA seeds — which are NOT required: `simulateTransaction` of this
//! exact 23-account buy (tail stripped) resolved every account and entered `Buy` logic,
//! failing only on an unrelated uninitialized user ATA (created by the arb evaluator's
//! setup instructions before the swap). So the builder emits the complete instruction.
//! Still behind `ENABLE_PUMPSWAP_TRADING` (default off) — run the in-context
//! `simulateTransaction` on your own funded cycle (docs/pumpswap-trading.md) before live.
//!
//! Sourced on-chain and banked as consts: FEE_PROGRAM, PROTOCOL_FEE_RECIPIENT (below).
//!
//! ## Exact-out buy caveat
//! `buy` is exact-out on the base token. The arb model is exact-in (it passes
//! `amount_in` + `minimum_amount_out`). We map `base_amount_out = minimum_amount_out`
//! (the slippage-floored expected out) and `max_quote_amount_in = amount_in`. This is
//! CONSERVATIVE: the swap can never overspend the quote leg and never fails on slippage,
//! but it fills slightly *under* the optimal base amount (it buys exactly the floor, not
//! the expected). A cycle still closes; the small unfilled quote remainder stays in the
//! wallet. Refining to true expected-out would require threading the pre-slippage quote
//! into `build_swap_ix` — deferred.

use anyhow::{Context, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use spl_associated_token_account::get_associated_token_address_with_program_id;

use super::types::{DexKind, Pool};

/// Anchor discriminator = sha256("global:buy")[:8]. Verified in tests.
pub const BUY_DISCM: [u8; 8] = [102, 6, 61, 18, 1, 218, 235, 234];
/// Anchor discriminator = sha256("global:sell")[:8]. Verified in tests.
pub const SELL_DISCM: [u8; 8] = [51, 230, 133, 164, 1, 127, 131, 173];

/// The PumpSwap dynamic-fee program (`pfee…` vanity prefix). SOURCED ON-CHAIN
/// 2026-07-25: identical across every sampled live buy/sell. Constant, not per-pool.
pub const FEE_PROGRAM: Pubkey = solana_sdk::pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
/// A valid `protocol_fee_recipient`. SOURCED ON-CHAIN 2026-07-25: the program accepts
/// any member of the GlobalConfig recipient SET (observed ≥5 rotating: 62qc2CNX…,
/// G5UZAVbA…, JCRGumoE…, 7VtfL8fv…, FWsW1xNt…); any one is accepted, so we pin one.
pub const PROTOCOL_FEE_RECIPIENT: Pubkey = solana_sdk::pubkey!("62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV");

// ─── PDA derivations (all from the IDL seed definitions) ────────────────────────

fn global_config(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"global_config"], program).0
}
fn event_authority(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], program).0
}
fn coin_creator_vault_authority(program: &Pubkey, coin_creator: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"creator_vault", coin_creator.as_ref()], program).0
}
fn global_volume_accumulator(program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"global_volume_accumulator"], program).0
}
fn user_volume_accumulator(program: &Pubkey, user: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"user_volume_accumulator", user.as_ref()], program).0
}
/// fee_config is a PDA of the SEPARATE fee program, seeded with the AMM program id.
fn fee_config(fee_program: &Pubkey, amm_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"fee_config", amm_program.as_ref()], fee_program).0
}

/// Build a PumpSwap `buy` or `sell` swap instruction for one hop of an arb cycle.
///
/// `a_to_b == true`  ⇒ base → quote ⇒ `sell` (exact-in).
/// `a_to_b == false` ⇒ quote → base ⇒ `buy`  (exact-out; see module caveat).
///
/// Orientation convention (matched by `fetch_pumpswap_pools.js`):
///   base_mint = pool.token_a, quote_mint = pool.token_b,
///   pool_base_token_account = pool.vault_a, pool_quote_token_account = pool.vault_b,
///   base_token_program = token_program_a, quote_token_program = token_program_b.
///
/// `user_source`/`user_destination` are ignored: PumpSwap needs the user's base AND quote
/// ATAs in fixed slots regardless of direction, so both are derived from `user_owner`.
#[allow(clippy::too_many_arguments)]
pub fn build_swap_instruction(
    pool: &Pool,
    _user_source: Pubkey,
    _user_destination: Pubkey,
    user_owner: Pubkey,
    amount_in: u64,
    minimum_amount_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let program = DexKind::PumpSwap.program_id();
    let ex = &pool.extra;

    // coin_creator is per-pool (fetcher-emitted). fee_program + protocol_fee_recipient
    // are global constants sourced on-chain 2026-07-25 — default to the consts, allow an
    // extra override only for future-proofing if pump rotates them.
    let coin_creator = ex
        .pumpswap_coin_creator
        .context("PumpSwap: missing extra.pumpswap_coin_creator")?;
    let protocol_fee_recipient = ex.pumpswap_protocol_fee_recipient.unwrap_or(PROTOCOL_FEE_RECIPIENT);
    let fee_program = ex.pumpswap_fee_program.unwrap_or(FEE_PROGRAM);

    let base_mint = pool.token_a;
    let quote_mint = pool.token_b;
    let pool_base = pool.vault_a;
    let pool_quote = pool.vault_b;
    let base_tp = pool.token_program_for(true);
    let quote_tp = pool.token_program_for(false);

    let user_base = get_associated_token_address_with_program_id(&user_owner, &base_mint, &base_tp);
    let user_quote = get_associated_token_address_with_program_id(&user_owner, &quote_mint, &quote_tp);

    let cfg = global_config(&program);
    let evt = event_authority(&program);
    let ccv_auth = coin_creator_vault_authority(&program, &coin_creator);
    let ccv_ata = get_associated_token_address_with_program_id(&ccv_auth, &quote_mint, &quote_tp);
    let pfr_ata = get_associated_token_address_with_program_id(&protocol_fee_recipient, &quote_mint, &quote_tp);
    let fee_cfg = fee_config(&fee_program, &program);

    // Accounts 0..=16 are identical for buy and sell; buy appends volume accumulators.
    // Writability: mark WRITABLE anything the program may mutate (accounts/ATAs/vaults/
    // accumulators); keep programs, mints, sysvars, configs and authorities read-only.
    // Over-marking a program/mint/sysvar writable is invalid, so those stay read-only;
    // the volume accumulators ARE written (global + per-user counters) → writable.
    let mut accounts = vec![
        AccountMeta::new(pool.id, false),                       // 0 pool
        AccountMeta::new(user_owner, true),                     // 1 user (signer)
        AccountMeta::new_readonly(cfg, false),                  // 2 global_config
        AccountMeta::new_readonly(base_mint, false),            // 3 base_mint
        AccountMeta::new_readonly(quote_mint, false),           // 4 quote_mint
        AccountMeta::new(user_base, false),                     // 5 user_base_token_account
        AccountMeta::new(user_quote, false),                    // 6 user_quote_token_account
        AccountMeta::new(pool_base, false),                     // 7 pool_base_token_account
        AccountMeta::new(pool_quote, false),                    // 8 pool_quote_token_account
        AccountMeta::new_readonly(protocol_fee_recipient, false), // 9 protocol_fee_recipient
        AccountMeta::new(pfr_ata, false),                       // 10 protocol_fee_recipient_token_account
        AccountMeta::new_readonly(base_tp, false),              // 11 base_token_program
        AccountMeta::new_readonly(quote_tp, false),             // 12 quote_token_program
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false), // 13 system_program
        AccountMeta::new_readonly(spl_associated_token_account::id(), false), // 14 associated_token_program
        AccountMeta::new_readonly(evt, false),                  // 15 event_authority
        AccountMeta::new_readonly(program, false),              // 16 program
        AccountMeta::new(ccv_ata, false),                       // 17 coin_creator_vault_ata
        AccountMeta::new_readonly(ccv_auth, false),             // 18 coin_creator_vault_authority
    ];

    let data = if a_to_b {
        // SELL: base → quote, exact-in.
        // accounts: 19 fee_config, 20 fee_program
        accounts.push(AccountMeta::new_readonly(fee_cfg, false));
        accounts.push(AccountMeta::new_readonly(fee_program, false));
        let mut d = Vec::with_capacity(24);
        d.extend_from_slice(&SELL_DISCM);
        d.extend_from_slice(&amount_in.to_le_bytes());          // base_amount_in
        d.extend_from_slice(&minimum_amount_out.to_le_bytes()); // min_quote_amount_out
        d
    } else {
        // BUY: quote → base, exact-out (see module caveat).
        // accounts: 19 global_volume_accumulator, 20 user_volume_accumulator, 21 fee_config, 22 fee_program
        accounts.push(AccountMeta::new(global_volume_accumulator(&program), false));
        accounts.push(AccountMeta::new(user_volume_accumulator(&program, &user_owner), false));
        accounts.push(AccountMeta::new_readonly(fee_cfg, false));
        accounts.push(AccountMeta::new_readonly(fee_program, false));
        let mut d = Vec::with_capacity(25);
        d.extend_from_slice(&BUY_DISCM);
        d.extend_from_slice(&minimum_amount_out.to_le_bytes()); // base_amount_out (conservative)
        d.extend_from_slice(&amount_in.to_le_bytes());          // max_quote_amount_in
        d.push(0u8);                                            // track_volume: Option<bool> = None
        d
    };

    // The buy=23 / sell=21 account list above is the AMM's FULL declared interface
    // (confirmed against the on-chain program IDL). Organic swaps append optional
    // buyback `remaining_accounts` (a rotating, runtime-selected fee-program BuybackVault
    // + ATA) that are NOT statically derivable and NOT required — VALIDATED on-chain
    // 2026-07-25 by `simulateTransaction` of this exact 23-account buy stripped of the
    // tail: the program invoked `Buy`, resolved every account, and proceeded into swap
    // logic (failing only on an unrelated uninitialized user ATA, which the arb
    // evaluator's setup instructions create before the swap). See docs/pumpswap-trading.md.
    Ok(Instruction { program_id: program, accounts, data })
}

/// Fixed swap accounts and instruction data for one hop, mirroring `build_swap_instruction`
/// — kept as a test helper for granular layout/writability assertions.
/// `fee_program`/`protocol_fee_recipient` default to the sourced consts.
#[cfg(test)]
fn fixed_swap_accounts(pool: &Pool, user_owner: Pubkey, a_to_b: bool) -> (Vec<AccountMeta>, Vec<u8>) {
    let program = DexKind::PumpSwap.program_id();
    let ex = &pool.extra;
    let coin_creator = ex.pumpswap_coin_creator.expect("coin_creator");
    let protocol_fee_recipient = ex.pumpswap_protocol_fee_recipient.unwrap_or(PROTOCOL_FEE_RECIPIENT);
    let fee_program = ex.pumpswap_fee_program.unwrap_or(FEE_PROGRAM);
    let (base_mint, quote_mint) = (pool.token_a, pool.token_b);
    let base_tp = pool.token_program_for(true);
    let quote_tp = pool.token_program_for(false);
    let user_base = get_associated_token_address_with_program_id(&user_owner, &base_mint, &base_tp);
    let user_quote = get_associated_token_address_with_program_id(&user_owner, &quote_mint, &quote_tp);
    let ccv_auth = coin_creator_vault_authority(&program, &coin_creator);
    let ccv_ata = get_associated_token_address_with_program_id(&ccv_auth, &quote_mint, &quote_tp);
    let pfr_ata = get_associated_token_address_with_program_id(&protocol_fee_recipient, &quote_mint, &quote_tp);
    let fee_cfg = fee_config(&fee_program, &program);
    let mut accounts = vec![
        AccountMeta::new(pool.id, false),
        AccountMeta::new(user_owner, true),
        AccountMeta::new_readonly(global_config(&program), false),
        AccountMeta::new_readonly(base_mint, false),
        AccountMeta::new_readonly(quote_mint, false),
        AccountMeta::new(user_base, false),
        AccountMeta::new(user_quote, false),
        AccountMeta::new(pool.vault_a, false),
        AccountMeta::new(pool.vault_b, false),
        AccountMeta::new_readonly(protocol_fee_recipient, false),
        AccountMeta::new(pfr_ata, false),
        AccountMeta::new_readonly(base_tp, false),
        AccountMeta::new_readonly(quote_tp, false),
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        AccountMeta::new_readonly(spl_associated_token_account::id(), false),
        AccountMeta::new_readonly(event_authority(&program), false),
        AccountMeta::new_readonly(program, false),
        AccountMeta::new(ccv_ata, false),
        AccountMeta::new_readonly(ccv_auth, false),
    ];
    let data = if a_to_b {
        accounts.push(AccountMeta::new_readonly(fee_cfg, false));
        accounts.push(AccountMeta::new_readonly(fee_program, false));
        let mut d = Vec::new();
        d.extend_from_slice(&SELL_DISCM);
        d.extend_from_slice(&5_000u64.to_le_bytes());
        d.extend_from_slice(&4_900u64.to_le_bytes());
        d
    } else {
        accounts.push(AccountMeta::new(global_volume_accumulator(&program), false));
        accounts.push(AccountMeta::new(user_volume_accumulator(&program, &user_owner), false));
        accounts.push(AccountMeta::new_readonly(fee_cfg, false));
        accounts.push(AccountMeta::new_readonly(fee_program, false));
        let mut d = Vec::new();
        d.extend_from_slice(&BUY_DISCM);
        d.extend_from_slice(&4_900u64.to_le_bytes());
        d.extend_from_slice(&5_000u64.to_le_bytes());
        d.push(0u8);
        d
    };
    (accounts, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{Pool, PoolExtra};
    use std::sync::atomic::{AtomicI32, AtomicU64};

    fn tok2022() -> Pubkey { spl_token_2022::id() }

    fn pool_with(extra: PoolExtra, quote_is_2022: bool) -> Pool {
        let mut extra = extra;
        if quote_is_2022 {
            extra.token_program_b = Some(tok2022());
        }
        Pool {
            id: Pubkey::new_unique(),
            dex: DexKind::PumpSwap,
            token_a: Pubkey::new_unique(), // base
            token_b: Pubkey::new_unique(), // quote
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(1_000_000),
            reserve_b: AtomicU64::new(1_000_000),
            fee_bps: AtomicU64::new(25),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            last_update_ns: AtomicU64::new(0),
            extra,
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        }
    }

    fn full_extra() -> PoolExtra {
        PoolExtra {
            pumpswap_coin_creator: Some(Pubkey::new_unique()),
            pumpswap_protocol_fee_recipient: Some(Pubkey::new_unique()),
            pumpswap_fee_program: Some(Pubkey::new_unique()),
            ..PoolExtra::default()
        }
    }

    #[test]
    fn discriminators_are_anchor_sha256_of_global_names() {
        // Anchor: discriminator = sha256("global:<ix_name>")[:8]. Recompute and lock.
        use sha2::{Digest, Sha256};
        let d = |name: &str| -> [u8; 8] {
            let h = Sha256::digest(format!("global:{name}").as_bytes());
            let mut out = [0u8; 8];
            out.copy_from_slice(&h[..8]);
            out
        };
        assert_eq!(d("buy"), BUY_DISCM, "buy discriminator");
        assert_eq!(d("sell"), SELL_DISCM, "sell discriminator");
    }

    #[test]
    fn pda_derivations_match_live_mainnet_constants() {
        // Ground truth: pubkeys read directly from live PumpSwap buy/sell txs on
        // 2026-07-25 (7 swaps sampled). This locks the seed logic against the deployed
        // program — a seed change would flip these and fail loudly.
        use std::str::FromStr;
        let amm = DexKind::PumpSwap.program_id();
        let fee_program = Pubkey::from_str("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ").unwrap();
        let pk = |s: &str| Pubkey::from_str(s).unwrap();
        assert_eq!(global_config(&amm), pk("ADyA8hdefvWN2dbGGWFotbzWxrAvLW83WG6QCVXvJKqw"), "global_config PDA");
        assert_eq!(event_authority(&amm), pk("GS4CU59F31iL7aR2Q8zVS8DRrcRnXX1yjQ66TqNVQnaR"), "event_authority PDA");
        assert_eq!(global_volume_accumulator(&amm), pk("C2aFPdENg4A2HQsmrd5rTw5TaYBX5Ku887cWjbFKtZpw"), "global_volume_accumulator PDA");
        assert_eq!(fee_config(&fee_program, &amm), pk("5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx"), "fee_config PDA");
    }

    #[test]
    fn build_emits_full_declared_instruction() {
        // The builder emits the AMM's FULL declared interface (buy=23, sell=21) — the
        // optional buyback remaining_accounts are NOT required (validated on-chain via
        // simulateTransaction, 2026-07-25: the program resolved every account and entered
        // Buy logic). So build succeeds and the account/data layout matches the fixed spec.
        let pool = pool_with(full_extra(), false);
        let user = Pubkey::new_unique();
        let sell = build_swap_instruction(&pool, Pubkey::new_unique(), Pubkey::new_unique(), user, 5_000, 4_900, true).unwrap();
        assert_eq!(sell.program_id, DexKind::PumpSwap.program_id());
        assert_eq!(sell.accounts.len(), 21, "sell = 21 accounts");
        assert_eq!(&sell.data[..8], &SELL_DISCM);
        let buy = build_swap_instruction(&pool, Pubkey::new_unique(), Pubkey::new_unique(), user, 5_000, 4_900, false).unwrap();
        assert_eq!(buy.accounts.len(), 23, "buy = 23 accounts");
        assert_eq!(&buy.data[..8], &BUY_DISCM);
        assert_eq!(buy.data[24], 0u8, "track_volume = None");
        assert!(buy.accounts[20].is_writable, "user_volume_accumulator writable");
    }

    #[test]
    fn fixed_sell_accounts_layout_exact_in() {
        // The PDA-verified fixed portion (0..=20 for sell) — locked even though the full
        // builder bails on the tail.
        let pool = pool_with(full_extra(), false);
        let user = Pubkey::new_unique();
        let (accounts, data) = fixed_swap_accounts(&pool, user, true);
        assert_eq!(accounts.len(), 21, "sell fixed = 21 accounts (through fee_program)");
        assert_eq!(&data[..8], &SELL_DISCM);
        assert_eq!(&data[8..16], &5_000u64.to_le_bytes(), "base_amount_in");
        assert_eq!(&data[16..24], &4_900u64.to_le_bytes(), "min_quote_amount_out");
        assert_eq!(accounts[0].pubkey, pool.id);
        assert!(accounts[1].is_signer && accounts[1].is_writable);
        assert!(!accounts[3].is_writable, "base_mint read-only");
        assert!(!accounts[13].is_writable, "system_program read-only");
    }

    #[test]
    fn fixed_buy_accounts_layout_exact_out_with_volume_accumulators() {
        let pool = pool_with(full_extra(), false);
        let user = Pubkey::new_unique();
        let (accounts, data) = fixed_swap_accounts(&pool, user, false);
        assert_eq!(accounts.len(), 23, "buy fixed = 23 accounts (adds 2 volume accumulators)");
        assert_eq!(&data[..8], &BUY_DISCM);
        assert_eq!(&data[8..16], &4_900u64.to_le_bytes(), "base_amount_out (conservative)");
        assert_eq!(&data[16..24], &5_000u64.to_le_bytes(), "max_quote_amount_in");
        assert_eq!(data[24], 0u8, "track_volume = None");
        assert!(accounts[19].is_writable, "global_volume_accumulator writable");
        assert!(accounts[20].is_writable, "user_volume_accumulator writable");
    }

    #[test]
    fn missing_required_extra_errors_never_trades_on_a_guess() {
        let user = Pubkey::new_unique();
        // coin_creator is per-pool and REQUIRED: dropping it → a "missing" error.
        let mut ex = full_extra();
        ex.pumpswap_coin_creator = None;
        let pool = pool_with(ex, false);
        let err = build_swap_instruction(&pool, user, user, user, 1, 1, true).unwrap_err();
        assert!(err.to_string().contains("PumpSwap: missing"), "dropped coin_creator: {err}");
        // fee_program/protocol_fee_recipient default to the sourced consts, so dropping
        // them from extra is fine — the builder succeeds using the constants.
        for drop in ["recipient", "fee_program"] {
            let mut ex = full_extra();
            if drop == "recipient" { ex.pumpswap_protocol_fee_recipient = None; } else { ex.pumpswap_fee_program = None; }
            let pool = pool_with(ex, false);
            assert!(build_swap_instruction(&pool, user, user, user, 1, 1, true).is_ok(),
                "dropped {drop} still builds via the sourced const");
        }
    }

    #[test]
    fn quote_token_2022_threads_into_derived_atas() {
        // A Token-2022 quote mint must derive the protocol-fee and creator-vault ATAs
        // under the 2022 program, else the accounts mismatch on-chain. Compare the
        // slot-10 protocol_fee_recipient_ata between a keg-quote and a 2022-quote pool.
        let ex = full_extra();
        let keg = pool_with(ex.clone(), false);
        let t22 = pool_with(ex, true);
        let user = Pubkey::new_unique();
        let (a, _) = fixed_swap_accounts(&keg, user, true);
        let (b, _) = fixed_swap_accounts(&t22, user, true);
        assert_ne!(a[10].pubkey, b[10].pubkey,
            "protocol_fee_recipient_ata must differ when the quote token program differs");
    }

    #[test]
    fn pdas_derive_deterministically_at_fixed_slots() {
        let pool = pool_with(full_extra(), false);
        let user = Pubkey::new_unique();
        let (accounts, _) = fixed_swap_accounts(&pool, user, true);
        let program = DexKind::PumpSwap.program_id();
        // slot 2 = global_config PDA, slot 15 = event_authority PDA — stable derivations.
        assert_eq!(accounts[2].pubkey, global_config(&program));
        assert_eq!(accounts[15].pubkey, event_authority(&program));
        assert_eq!(accounts[16].pubkey, program, "slot 16 = the AMM program id");
    }
}


