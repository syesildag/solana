/// Phoenix v1 CLOB swap support.
///
/// Price model: uses vault balance ratio as a first-order approximation of the
/// mid-price. Phoenix vaults hold all resting-order collateral, so their ratio
/// is the book VWAP and closely tracks the actual mid-price.
///
/// Swap instruction: IOC (ImmediateOrCancel) market order via PhoenixInstruction::Swap.
/// Borsh layout of instruction data:
///   [0u8]                          PhoenixInstruction::Swap discriminant
///   [2u8]                          OrderPacket::ImmediateOrCancel variant index
///   [side u8]                      0=Bid (buy base), 1=Ask (sell base)
///   [0u8]                          price_in_ticks: Option<u64> = None
///   [num_base_lots   u64 LE]
///   [num_quote_lots  u64 LE]
///   [min_base_lots   u64 LE]
///   [min_quote_lots  u64 LE]
///   [2u8]                          SelfTradeBehavior::DecrementTake
///   [0u8]                          match_limit: Option<u64> = None
///   [0u128 LE]                     client_order_id
///   [0u8]                          use_only_deposited_funds: false
///   [0u8]                          last_valid_slot: None
///   [0u8]                          last_valid_unix_timestamp: None
use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use std::sync::atomic::Ordering;

use crate::dex::types::{Pool, SwapQuote, PHOENIX_PUBKEY};

const SIDE_BID: u8 = 0;
const SIDE_ASK: u8 = 1;
const SELF_TRADE_DECREMENT_TAKE: u8 = 2;

/// CP-formula quote using vault reserves.
/// Phoenix is a CLOB so this is approximate, but correct for detecting divergences.
pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    let fee_bps = pool.fee_bps.load(Ordering::Relaxed);
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

    let fee_amount = amount_in * fee_bps / 10_000;
    let amount_in_with_fee = amount_in.saturating_sub(fee_amount);

    if reserve_in == 0 || amount_in == 0 {
        return SwapQuote { amount_in, amount_out: 0, fee_amount, price_impact: 1.0, a_to_b };
    }

    let numerator   = (reserve_out as u128) * (amount_in_with_fee as u128);
    let denominator = (reserve_in  as u128) + (amount_in_with_fee as u128);
    let amount_out  = (numerator / denominator) as u64;
    let price_impact = amount_in as f64 / (reserve_in as f64 + amount_in as f64);

    SwapQuote { amount_in, amount_out, fee_amount, price_impact, a_to_b }
}

/// Build a Phoenix v1 Swap instruction (IOC market order).
///
/// a_to_b=true  → token_a (base) → token_b (quote): Side::Ask  (sell base)
/// a_to_b=false → token_b (quote) → token_a (base): Side::Bid  (buy base)
pub fn build_swap_instruction(
    pool: &Pool,
    user_src: Pubkey,
    user_dst: Pubkey,
    user: Pubkey,
    amount_in: u64,
    min_out: u64,
    a_to_b: bool,
) -> Result<Instruction> {
    let base_lot  = pool.extra.phoenix_base_lot_size
        .ok_or_else(|| anyhow::anyhow!("missing phoenix_base_lot_size"))?;
    let quote_lot = pool.extra.phoenix_quote_lot_size
        .ok_or_else(|| anyhow::anyhow!("missing phoenix_quote_lot_size"))?;

    if base_lot == 0 || quote_lot == 0 {
        anyhow::bail!("phoenix lot sizes must be > 0");
    }

    // Convert raw token amounts to lots; reject if below the minimum lot size.
    let (side, num_base_lots, num_quote_lots, min_base_lots, min_quote_lots) = if a_to_b {
        // Selling base for quote: specify base lots in, floor min quote lots out.
        let base_lots = amount_in / base_lot;
        if base_lots == 0 { anyhow::bail!("amount_in below phoenix base_lot_size"); }
        (SIDE_ASK, base_lots, u64::MAX, 0u64, min_out / quote_lot)
    } else {
        // Buying base with quote: specify quote lots in, floor min base lots out.
        let quote_lots = amount_in / quote_lot;
        if quote_lots == 0 { anyhow::bail!("amount_in below phoenix quote_lot_size"); }
        (SIDE_BID, u64::MAX, quote_lots, min_out / base_lot, 0u64)
    };

    // Instruction data: PhoenixInstruction::Swap (u8=0) + borsh(OrderPacket::ImmediateOrCancel)
    let mut data: Vec<u8> = Vec::with_capacity(58);
    data.push(0u8);                                   // PhoenixInstruction::Swap = 0
    data.push(2u8);                                   // OrderPacket::ImmediateOrCancel = 2
    data.push(side);                                  // Side (u8)
    data.push(0u8);                                   // price_in_ticks: None
    data.extend_from_slice(&num_base_lots.to_le_bytes());
    data.extend_from_slice(&num_quote_lots.to_le_bytes());
    data.extend_from_slice(&min_base_lots.to_le_bytes());
    data.extend_from_slice(&min_quote_lots.to_le_bytes());
    data.push(SELF_TRADE_DECREMENT_TAKE);             // SelfTradeBehavior::DecrementTake = 2
    data.push(0u8);                                   // match_limit: None
    data.extend_from_slice(&0u128.to_le_bytes());     // client_order_id = 0
    data.push(0u8);                                   // use_only_deposited_funds: false
    data.push(0u8);                                   // last_valid_slot: None
    data.push(0u8);                                   // last_valid_unix_timestamp: None

    // Log authority PDA: seeds=[b"log"], program=phoenix
    let (log_authority, _) = Pubkey::find_program_address(&[b"log"], &PHOENIX_PUBKEY);

    // base_account = user ATA for token_a (base); quote_account = user ATA for token_b (quote)
    let (base_account, quote_account) = if a_to_b {
        (user_src, user_dst) // selling base: src=base, dst=quote
    } else {
        (user_dst, user_src) // buying base: src=quote, dst=base
    };

    let accounts = vec![
        AccountMeta::new_readonly(PHOENIX_PUBKEY, false),   // Phoenix program (self-ref CPI check)
        AccountMeta::new_readonly(log_authority, false),     // log authority PDA
        AccountMeta::new(pool.id, false),                    // market account (writable)
        AccountMeta::new_readonly(user, true),               // trader (signer)
        AccountMeta::new(base_account, false),               // user base ATA
        AccountMeta::new(quote_account, false),              // user quote ATA
        AccountMeta::new(pool.vault_a, false),               // market base vault
        AccountMeta::new(pool.vault_b, false),               // market quote vault
        AccountMeta::new_readonly(spl_token::id(), false),   // SPL token program
    ];

    Ok(Instruction {
        program_id: PHOENIX_PUBKEY,
        accounts,
        data,
    })
}
