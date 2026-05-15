/// Phoenix v1 CLOB swap support.
///
/// Price model: parses the on-chain FIFOMarket order book to extract best bid and ask
/// ticks separately. Bid price is stored in `sqrt_price_x64`; ask price in
/// `damm_virtual_price`. Quotes use the direction-appropriate price:
///   a_to_b=true  (sell base for quote) → bid price
///   a_to_b=false (buy base with quote) → ask price
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

/// Quote using directional prices from the order book.
/// a_to_b=true  (sell base): uses bid price from sqrt_price_x64.
/// a_to_b=false (buy base):  uses ask price from damm_virtual_price.
pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    let fee_bps = pool.fee_bps.load(Ordering::Relaxed);
    let price_bits = if a_to_b {
        pool.sqrt_price_x64.load(Ordering::Relaxed)      // bid price
    } else {
        pool.damm_virtual_price.load(Ordering::Relaxed)  // ask price
    };

    let amount_out = if price_bits == 0 || amount_in == 0 {
        0
    } else {
        let price = f64::from_bits(price_bits);
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

/// Parse a Phoenix FIFOMarket state account, storing bid and ask prices separately.
///
/// - Bid price → returned as first tuple element (caller writes to `pool.sqrt_price_x64`).
/// - Ask price → stored directly in `pool.damm_virtual_price` as f64 bits.
///
/// Handles one-sided books: if bids are absent or only floor/sentinel orders exist,
/// bid_price is 0.0 (a→b edge disabled). If asks are absent, ask_price is 0.0 (b→a disabled).
/// Returns None only when both prices are zero (empty or fully invalid book).
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

    let price_factor = tick_size_lots as f64 * quote_lot as f64
                     / (base_lots_per_unit as f64 * base_lot as f64);

    // Best ask: stored as actual tick count; leftmost (min) = best ask.
    let ask_ticks_raw = navigate_rbt(data, asks_start, false);
    let ask_price: f64 = ask_ticks_raw
        .filter(|&t| t > 0)
        .map(|t| t as f64 * price_factor)
        .unwrap_or(0.0);

    // Best bid: stored as wrapping_neg(actual_ticks); leftmost stored (min) = max actual = best bid.
    // Floor/sentinel bids have stored≈1 → actual≈u64::MAX; filtered by comparing against ask.
    let bid_price: f64 = navigate_rbt(data, FIFO_PREFIX, false)
        .map(|stored| stored.wrapping_neg())
        .filter(|&t| {
            if t == 0 { return false; }
            if let Some(ask_t) = ask_ticks_raw {
                t < ask_t
            } else {
                t < u64::MAX / 1_000
            }
        })
        .map(|t| t as f64 * price_factor)
        .unwrap_or(0.0);

    if bid_price <= 0.0 && ask_price <= 0.0 {
        return None;
    }

    if (bid_price > 0.0 && !bid_price.is_finite())
        || (ask_price > 0.0 && !ask_price.is_finite())
    {
        return None;
    }

    pool.damm_virtual_price.store(ask_price.to_bits(), Ordering::Relaxed);

    Some((bid_price, 0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, Pool, PoolExtra};
    use solana_sdk::pubkey::Pubkey;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
    use std::sync::Arc;

    fn phoenix_pool() -> Arc<Pool> {
        Arc::new(Pool {
            id: Pubkey::new_unique(),
            dex: DexKind::Phoenix,
            token_a: Pubkey::new_unique(),
            token_b: Pubkey::new_unique(),
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(0),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra {
                phoenix_base_lot_size: Some(1),
                phoenix_quote_lot_size: Some(1),
                ..PoolExtra::default()
            },
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        })
    }

    // Builds a minimal FIFOMarket byte array for testing parse_state.
    // Uses bids_capacity=1, so asks_start = FIFO_PREFIX+TREE_HDR+NODE_SIZE = 976.
    // base_lots_per_unit=1000, tick_size_lots=100 → price_factor = 0.1
    // bid at B actual ticks → bid_price = B * 0.1; ask at A ticks → ask_price = A * 0.1
    fn make_market_data(bid_actual_ticks: Option<u64>, ask_ticks: Option<u64>) -> Vec<u8> {
        const BIDS_CAP: usize = 1;
        let asks_start = FIFO_PREFIX + TREE_HDR + BIDS_CAP * NODE_SIZE; // 976
        let buf_size = asks_start + TREE_HDR + NODE_SIZE;                // 1072
        let mut buf = vec![0u8; buf_size];

        buf[16..24].copy_from_slice(&(BIDS_CAP as u64).to_le_bytes());   // bids capacity
        buf[832..840].copy_from_slice(&1000u64.to_le_bytes());            // base_lots_per_unit
        buf[840..848].copy_from_slice(&100u64.to_le_bytes());             // tick_size_lots

        // Bids node 1 at FIFO_PREFIX+TREE_HDR=912; price at 912+PRICE_OFF=928
        if let Some(actual) = bid_actual_ticks {
            buf[880..884].copy_from_slice(&1u32.to_le_bytes()); // root = node 1
            buf[928..936].copy_from_slice(&actual.wrapping_neg().to_le_bytes());
        }

        // Asks node 1 at asks_start+TREE_HDR=1008; price at 1008+PRICE_OFF=1024
        if let Some(ticks) = ask_ticks {
            buf[976..980].copy_from_slice(&1u32.to_le_bytes()); // root = node 1
            buf[1024..1032].copy_from_slice(&ticks.to_le_bytes());
        }

        buf
    }

    #[test]
    fn get_quote_a_to_b_uses_bid_price() {
        let pool = phoenix_pool();
        pool.sqrt_price_x64.store(10.0f64.to_bits(), Ordering::Relaxed);      // bid = 10.0
        pool.damm_virtual_price.store(11.0f64.to_bits(), Ordering::Relaxed);  // ask = 11.0
        // a_to_b: sell base → bid (10.0) * fee (1.0) = 10.0; output = 1_000_000 * 10.0 = 10_000_000
        let q = get_quote(&pool, 1_000_000, true);
        assert_eq!(q.amount_out, 10_000_000);
    }

    #[test]
    fn get_quote_b_to_a_uses_ask_price() {
        let pool = phoenix_pool();
        pool.sqrt_price_x64.store(10.0f64.to_bits(), Ordering::Relaxed);      // bid = 10.0
        pool.damm_virtual_price.store(11.0f64.to_bits(), Ordering::Relaxed);  // ask = 11.0
        // b_to_a: buy base with quote → (1/ask) * fee = (1/11.0) * 1.0; output = floor(1_000_000 / 11.0) = 90_909
        let q = get_quote(&pool, 1_000_000, false);
        assert_eq!(q.amount_out, 90_909);
    }

    #[test]
    fn parse_state_two_sided_book_stores_bid_and_ask() {
        let pool = phoenix_pool();
        // bid at 100 ticks → bid_price = 100 * 0.1 = 10.0
        // ask at 110 ticks → ask_price = 110 * 0.1 = 11.0
        let data = make_market_data(Some(100), Some(110));
        let result = parse_state(&data, &pool);
        assert!(result.is_some(), "two-sided book must return Some");
        let (bid_price, fee) = result.unwrap();
        assert_eq!(fee, 0);
        assert!((bid_price - 10.0).abs() < 1e-9, "bid_price should be 10.0, got {bid_price}");
        let ask_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
        let ask_price = f64::from_bits(ask_bits);
        assert!((ask_price - 11.0).abs() < 1e-9, "ask_price should be 11.0, got {ask_price}");
        assert!(bid_price < ask_price, "bid must be less than ask");
    }

    #[test]
    fn parse_state_asks_only_book_stores_zero_bid() {
        let pool = phoenix_pool();
        let data = make_market_data(None, Some(110));
        let result = parse_state(&data, &pool);
        assert!(result.is_some(), "asks-only book must return Some");
        let (bid_price, _) = result.unwrap();
        assert_eq!(bid_price, 0.0, "bid_price must be 0.0 when no bids");
        let ask_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
        let ask_price = f64::from_bits(ask_bits);
        assert!((ask_price - 11.0).abs() < 1e-9, "ask_price should be 11.0");
    }

    #[test]
    fn parse_state_empty_book_returns_none() {
        let pool = phoenix_pool();
        let data = make_market_data(None, None);
        assert!(parse_state(&data, &pool).is_none(), "empty book must return None");
    }
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
