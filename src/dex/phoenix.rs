/// Phoenix v1 CLOB swap support.
///
/// Price model: parses the on-chain FIFOMarket order book to extract best bid/ask
/// ticks, computes mid-price in raw token units, and stores it in `sqrt_price_x64`
/// (as f64 bits) so the exchange graph and quote engine use a real market price.
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

const SIDE_BID:                u8 = 0; // Side::Bid  — buying base with quote
const SIDE_ASK:                u8 = 1; // Side::Ask  — selling base for quote
const SELF_TRADE_DECREMENT_TAKE: u8 = 2; // SelfTradeBehavior::DecrementTake

// Suppress dead_code: SIDE_BID/SIDE_ASK are used inside build_swap_instruction branches
// but the Rust lint fires on private constants that only appear in one arm each.
const _: () = assert!(SIDE_BID == 0 && SIDE_ASK == 1);

/// Quote using the mid-price derived from the order book (stored in sqrt_price_x64).
/// Price is token_b per token_a in raw units, consistent with other CLMM DEXs.
pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    let fee_bps   = pool.fee_bps.load(Ordering::Relaxed);
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

// ── FIFOMarket binary layout constants ───────────────────────────────────────
// Verified by on-chain probe across 6 markets (SOL/USDT, ETH/USDC, BTC/USDC, SOL/mSOL, …):
//
//   [0..576)   MarketHeader
//     [16..24)   MarketSizeParams.bids_size (u64) — MAX order-slot capacity of bids tree
//   [576..832) FIFOMarket._padding [u64; 32]
//   [832..840) FIFOMarket.base_lots_per_base_unit        (e.g. 1000 SOL/USDT, 10000 ETH/USDC)
//   [840..848) FIFOMarket.tick_size_in_quote_lots_per_base_unit  (== base_lots for all standard markets)
//   [848..856) FIFOMarket.sequence_number                (order count, hundreds of millions)
//   [856..864) FIFOMarket.num_trader_state_header_pages  (small constant, typically 1–5)
//   [864..872) FIFOMarket.accumulated_fee_field          (large, grows over time)
//   [872..880) FIFOMarket.another_field
//   [880..)    bids RedBlackTree (sokoban)
const BIDS_SIZE_OFF: usize = 16;  // MarketHeader.market_size_params.bids_size
const BASE_LOTS_OFF: usize = 832; // FIFOMarket.base_lots_per_base_unit
const TICK_SIZE_OFF: usize = 840; // FIFOMarket.tick_size_in_quote_lots_per_base_unit
const FIFO_PREFIX:   usize = 880; // byte offset of bids tree in account data
const TREE_HDR:      usize = 32;  // sokoban tree header before nodes: root(u32)+free_list(u32)+allocator_meta(24B)
const NODE_SIZE:     usize = 64;  // 4×u32 registers(16) + FIFOOrderId{price_in_ticks,order_seq}(16) + FIFORestingOrder(32)
const PRICE_OFF:     usize = 16;  // FIFOOrderId.price_in_ticks is the first field (confirmed via carbon-phoenix-v1-decoder)
const SENTINEL:      u32   = 0;   // sokoban null handle

/// Parse a Phoenix FIFOMarket state account to extract the mid-price.
///
/// Navigates the sokoban RedBlackTree order book. Bid prices are stored as
/// wrapping_neg(price) so both trees are traversed leftward for "best" price:
/// - Best bid = leftmost bids node (min stored ≈ max actual price); negate to recover real ticks.
/// - Best ask = leftmost asks node (min stored = min actual price).
///
/// Returns `(price_b_per_a_raw, 0)` where price is in raw token units
/// (quote atoms per base atom), stored as f64 bits in `sqrt_price_x64`.
/// Fee bps is returned as 0 to preserve the value already set from pools.json.
pub fn parse_state(data: &[u8], pool: &Pool) -> Option<(f64, u64)> {
    if data.len() < FIFO_PREFIX + TREE_HDR {
        return None;
    }

    let base_lots_per_unit = read_u64(data, BASE_LOTS_OFF)?;
    let tick_size_lots     = read_u64(data, TICK_SIZE_OFF)?;
    let base_lot  = pool.extra.phoenix_base_lot_size?;
    let quote_lot = pool.extra.phoenix_quote_lot_size?;

    if base_lots_per_unit == 0 || tick_size_lots == 0 || base_lot == 0 || quote_lot == 0 {
        return None;
    }

    let bids_capacity = read_u64(data, BIDS_SIZE_OFF)? as usize;
    let asks_start    = FIFO_PREFIX + TREE_HDR + bids_capacity * NODE_SIZE;

    if data.len() < asks_start + TREE_HDR {
        return None;
    }

    // Bids are stored with wrapping_neg(price) — the leftmost bid (minimum stored) is the
    // maximum actual price = best bid. Floor/sentinel bids at stored=1 (actual=u64::MAX)
    // indicate no real liquidity; the wrapping_neg check below filters them out.
    let bid_stored = navigate_rbt(data, FIFO_PREFIX, false)?;
    let ask_ticks  = navigate_rbt(data, asks_start,  false)?;

    let bid_ticks = bid_stored.wrapping_neg();

    if bid_ticks == 0 || ask_ticks == 0 || bid_ticks >= ask_ticks {
        return None;
    }

    let mid_ticks = (bid_ticks + ask_ticks) / 2;
    // Convert ticks → quote_atoms per base_atom
    let price = mid_ticks as f64 * tick_size_lots as f64 * quote_lot as f64
                / (base_lots_per_unit as f64 * base_lot as f64);

    if price <= 0.0 || !price.is_finite() {
        return None;
    }
    Some((price, 0))
}

/// Traverse a sokoban RedBlackTree to its rightmost (go_right=true) or leftmost leaf.
/// Returns the `price_in_ticks` stored in the FIFOOrderId key of that node.
fn navigate_rbt(data: &[u8], tree_start: usize, go_right: bool) -> Option<u64> {
    let root = read_u32(data, tree_start)?;
    if root == SENTINEL {
        return None;
    }
    // sokoban registers: [0]=parent (offset 0), [1]=left (offset 4), [2]=right (offset 8)
    let reg_off = if go_right { 8 } else { 4 };
    let nodes_start = tree_start + TREE_HDR;
    let mut current = root;
    loop {
        let node_off = nodes_start + (current as usize - 1) * NODE_SIZE;
        let next = read_u32(data, node_off + reg_off)?;
        if next == SENTINEL {
            return read_u64(data, node_off + PRICE_OFF);
        }
        current = next;
    }
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    let bytes = data.get(offset..offset + 8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
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
