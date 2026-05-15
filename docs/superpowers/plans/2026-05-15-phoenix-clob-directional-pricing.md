# Phoenix CLOB Directional Pricing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Phoenix CLOB pools contribute real edges to the exchange graph by using bid price for a→b edges and ask price for b→a edges, instead of a mid-price that returns 0 when either side of the book is empty.

**Architecture:** `parse_state` computes bid and ask prices separately from the sokoban order book trees, storing bid in `sqrt_price_x64` (returned to caller) and ask in `damm_virtual_price` (written directly, unused by Phoenix otherwise). `get_quote` reads the direction-appropriate atomic. `update_pool` in `exchange_graph.rs` gets its own Phoenix branch that reads both atomics and inserts or removes each edge direction independently.

**Tech Stack:** Rust, `std::sync::atomic::AtomicU64`, existing `Pool` atomics (`sqrt_price_x64`, `damm_virtual_price`, `fee_bps`)

---

## Files

| File | Change |
|---|---|
| `src/dex/phoenix.rs` | `get_quote`: read direction-dependent atomic. `parse_state`: split mid into bid/ask, write ask to `damm_virtual_price`. New unit tests. |
| `src/graph/exchange_graph.rs` | Phoenix gets its own early-return branch in `update_pool`; handles one-sided books; removes stale edges. |
| `src/main.rs` | Add `phoenix={}` to BF window stats log format string. |

---

## Task 1: Directional price reads in `get_quote`

**Files:**
- Modify: `src/dex/phoenix.rs`

- [ ] **Step 1: Write failing tests**

Add inside the `#[cfg(test)] mod tests` block at the bottom of `src/dex/phoenix.rs`. There is no existing test module — create it:

```rust
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

    #[test]
    fn get_quote_a_to_b_uses_bid_price() {
        let pool = phoenix_pool();
        pool.sqrt_price_x64.store(10.0f64.to_bits(), Ordering::Relaxed);      // bid = 10.0
        pool.damm_virtual_price.store(11.0f64.to_bits(), Ordering::Relaxed);  // ask = 11.0
        // a_to_b: sell base → should use bid (10.0), output = 1_000_000 * 10.0 = 10_000_000
        let q = get_quote(&pool, 1_000_000, true);
        assert_eq!(q.amount_out, 10_000_000);
    }

    #[test]
    fn get_quote_b_to_a_uses_ask_price() {
        let pool = phoenix_pool();
        pool.sqrt_price_x64.store(10.0f64.to_bits(), Ordering::Relaxed);      // bid = 10.0
        pool.damm_virtual_price.store(11.0f64.to_bits(), Ordering::Relaxed);  // ask = 11.0
        // b_to_a: buy base with quote → should use ask (11.0), output = floor(1_000_000 / 11.0) = 90_909
        let q = get_quote(&pool, 1_000_000, false);
        assert_eq!(q.amount_out, 90_909);
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --bin solana-mev phoenix -- --nocapture 2>&1
```

Expected: `get_quote_b_to_a_uses_ask_price` FAILS because the current implementation reads `sqrt_price_x64` (bid=10.0) for both directions, producing `floor(1_000_000 / 10.0) = 100_000` instead of `90_909`.

- [ ] **Step 3: Implement `get_quote` change**

In `src/dex/phoenix.rs`, replace the body of `get_quote` (lines 42–58):

```rust
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
```

Also update the doc-comment at the top of the file (lines 1–8) to reflect the new model:

```rust
/// Phoenix v1 CLOB swap support.
///
/// Price model: parses the on-chain FIFOMarket order book to extract best bid and ask
/// ticks separately. Bid price is stored in `sqrt_price_x64`; ask price in
/// `damm_virtual_price`. Quotes use the direction-appropriate price:
///   a_to_b=true  (sell base for quote) → bid price
///   a_to_b=false (buy base with quote) → ask price
///
/// Swap instruction: IOC (ImmediateOrCancel) market order via PhoenixInstruction::Swap.
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
cargo test --bin solana-mev phoenix -- --nocapture 2>&1
```

Expected: both tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/dex/phoenix.rs
git commit -m "feat(phoenix): use directional bid/ask prices in get_quote"
```

---

## Task 2: Split mid-price into bid/ask in `parse_state`

**Files:**
- Modify: `src/dex/phoenix.rs`

- [ ] **Step 1: Write failing tests**

Add three tests to the `mod tests` block in `src/dex/phoenix.rs`. These require a helper that builds mock FIFOMarket byte buffers.

Add this helper function inside `mod tests` (after `phoenix_pool`):

```rust
// Builds a minimal FIFOMarket byte array for testing parse_state.
// Uses bids_capacity=1 (one slot), so asks_start = FIFO_PREFIX+TREE_HDR+NODE_SIZE = 976.
// base_lots_per_unit=1000, tick_size_lots=100, base_lot=1, quote_lot=1
// → price_factor = 100*1 / (1000*1) = 0.1
// → bid at B actual ticks → bid_price = B * 0.1
// → ask at A actual ticks → ask_price = A * 0.1
fn make_market_data(bid_actual_ticks: Option<u64>, ask_ticks: Option<u64>) -> Vec<u8> {
    const BIDS_CAP: usize = 1;
    // asks_start = FIFO_PREFIX + TREE_HDR + BIDS_CAP * NODE_SIZE = 880+32+64 = 976
    let asks_start = FIFO_PREFIX + TREE_HDR + BIDS_CAP * NODE_SIZE;
    // buffer needs to hold asks tree header + one ask node
    let buf_size = asks_start + TREE_HDR + NODE_SIZE; // 976+32+64 = 1072
    let mut buf = vec![0u8; buf_size];

    // BIDS_SIZE_OFF=16: bids tree capacity (u64)
    buf[16..24].copy_from_slice(&(BIDS_CAP as u64).to_le_bytes());
    // BASE_LOTS_OFF=832, TICK_SIZE_OFF=840 (u64 each)
    buf[832..840].copy_from_slice(&1000u64.to_le_bytes());
    buf[840..848].copy_from_slice(&100u64.to_le_bytes());

    // Bids tree: sokoban header at FIFO_PREFIX=880 (32 bytes), then nodes
    // Node 1 is at FIFO_PREFIX+TREE_HDR = 912; price_in_ticks at node_off+PRICE_OFF = 912+16 = 928
    if let Some(actual) = bid_actual_ticks {
        let stored = actual.wrapping_neg(); // bids stored as wrapping_neg(actual)
        buf[880..884].copy_from_slice(&1u32.to_le_bytes()); // root = node 1
        buf[928..936].copy_from_slice(&stored.to_le_bytes());
    }
    // else: root = 0 (SENTINEL) → navigate_rbt returns None

    // Asks tree: header at asks_start=976, node 1 at 976+TREE_HDR=1008, price at 1008+16=1024
    if let Some(ticks) = ask_ticks {
        buf[976..980].copy_from_slice(&1u32.to_le_bytes()); // root = node 1
        buf[1024..1032].copy_from_slice(&ticks.to_le_bytes());
    }

    buf
}
```

Add the three tests:

```rust
#[test]
fn parse_state_two_sided_book_stores_bid_and_ask() {
    let pool = phoenix_pool(); // base_lot=1, quote_lot=1
    // bid at 100 ticks → bid_price = 100 * 0.1 = 10.0
    // ask at 110 ticks → ask_price = 110 * 0.1 = 11.0
    let data = make_market_data(Some(100), Some(110));
    let result = parse_state(&data, &pool);
    assert!(result.is_some(), "two-sided book must return Some");
    let (bid_price, fee) = result.unwrap();
    assert_eq!(fee, 0);
    assert!((bid_price - 10.0).abs() < 1e-9, "bid_price should be 10.0, got {bid_price}");
    // ask must be stored in damm_virtual_price
    let ask_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
    let ask_price = f64::from_bits(ask_bits);
    assert!((ask_price - 11.0).abs() < 1e-9, "ask_price should be 11.0, got {ask_price}");
    // bid < ask
    assert!(bid_price < ask_price, "bid must be less than ask");
}

#[test]
fn parse_state_asks_only_book_stores_zero_bid() {
    let pool = phoenix_pool();
    // No bids tree root → bid = 0; ask at 110 ticks → ask_price = 11.0
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
    let result = parse_state(&data, &pool);
    assert!(result.is_none(), "empty book must return None");
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --bin solana-mev phoenix -- --nocapture 2>&1
```

Expected: `parse_state_asks_only_book_stores_zero_bid` and `parse_state_two_sided_book_stores_bid_and_ask` fail because the current `parse_state` returns `None` when either side is missing, and stores only a mid-price (not separate bid/ask).

- [ ] **Step 3: Implement `parse_state` change**

Replace the entire `parse_state` function in `src/dex/phoenix.rs` (lines 82–134):

```rust
/// Parse a Phoenix FIFOMarket state account, storing bid and ask prices separately.
///
/// - Bid price → stored as f64 bits in `pool.sqrt_price_x64` (via return value to caller).
/// - Ask price → stored as f64 bits in `pool.damm_virtual_price` (written directly here).
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

    // price_factor: converts tick count to raw price (quote atoms per base atom)
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
        .map(|stored| stored.wrapping_neg()) // recover actual tick count
        .filter(|&t| {
            if t == 0 { return false; }
            // Filter floor bids: bid ticks must be strictly below ask ticks (if an ask exists),
            // or below a practical upper bound when no ask is available.
            if let Some(ask_t) = ask_ticks_raw {
                t < ask_t
            } else {
                t < u64::MAX / 1_000 // no ask: reject astronomically large values (floor sentinel)
            }
        })
        .map(|t| t as f64 * price_factor)
        .unwrap_or(0.0);

    if bid_price <= 0.0 && ask_price <= 0.0 {
        return None;
    }

    // Validate computed prices
    if (bid_price > 0.0 && !bid_price.is_finite())
        || (ask_price > 0.0 && !ask_price.is_finite())
    {
        return None;
    }

    // Write ask into damm_virtual_price (unused by Phoenix for DAMM purposes).
    // The caller writes the returned bid into sqrt_price_x64.
    pool.damm_virtual_price.store(ask_price.to_bits(), Ordering::Relaxed);

    Some((bid_price, 0)) // 0 = preserve fee_bps already set from pools.json
}
```

Also update the doc comment block at the top of the file to match the new model (replace lines 1–8):

```rust
/// Phoenix v1 CLOB swap support.
///
/// Price model: parses the on-chain FIFOMarket order book to extract best bid and ask
/// ticks separately. Bid price is stored in `sqrt_price_x64`; ask price in
/// `damm_virtual_price`. This allows one-sided books to produce valid edges.
///
/// Swap instruction: IOC (ImmediateOrCancel) market order via PhoenixInstruction::Swap.
/// Borsh layout of instruction data:
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
cargo test --bin solana-mev phoenix -- --nocapture 2>&1
```

Expected: all 5 phoenix tests pass (`get_quote_a_to_b_uses_bid_price`, `get_quote_b_to_a_uses_ask_price`, `parse_state_two_sided_book_stores_bid_and_ask`, `parse_state_asks_only_book_stores_zero_bid`, `parse_state_empty_book_returns_none`).

- [ ] **Step 5: Commit**

```bash
git add src/dex/phoenix.rs
git commit -m "feat(phoenix): split parse_state into bid/ask prices, handle one-sided books"
```

---

## Task 3: Phoenix directional edges in `exchange_graph`

**Files:**
- Modify: `src/graph/exchange_graph.rs`

- [ ] **Step 1: Write failing test**

Add to the existing `#[cfg(test)] mod tests` block in `src/graph/exchange_graph.rs`. If no test module exists, create one. Add this import at the top of the file (if not already present):

```rust
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
```

Add this to the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::{DexKind, Pool, PoolExtra};
    use solana_sdk::pubkey::Pubkey;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
    use std::sync::Arc;

    fn phoenix_pool_with_prices(bid: f64, ask: f64) -> Arc<Pool> {
        let p = Arc::new(Pool {
            id: Pubkey::new_unique(),
            dex: DexKind::Phoenix,
            token_a: Pubkey::new_unique(),
            token_b: Pubkey::new_unique(),
            vault_a: Pubkey::new_unique(),
            vault_b: Pubkey::new_unique(),
            reserve_a: AtomicU64::new(0),
            reserve_b: AtomicU64::new(0),
            fee_bps: AtomicU64::new(0), // zero fee for simplicity
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            extra: PoolExtra::default(),
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        });
        p.sqrt_price_x64.store(bid.to_bits(), Ordering::Relaxed);
        p.damm_virtual_price.store(ask.to_bits(), Ordering::Relaxed);
        p
    }

    #[test]
    fn phoenix_two_sided_book_creates_two_edges() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 2, "two-sided book must produce 2 edges");
    }

    #[test]
    fn phoenix_asks_only_creates_one_b_to_a_edge() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(0.0, 11.0); // bid=0 (no bids), ask=11.0
        graph.update_pool(&pool);
        let edges = graph.snapshot_edges();
        assert_eq!(edges.len(), 1, "asks-only must produce exactly 1 edge");
        assert!(!edges[0].a_to_b, "the single edge must be b→a");
    }

    #[test]
    fn phoenix_bids_only_creates_one_a_to_b_edge() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 0.0); // bid=10.0, ask=0 (no asks)
        graph.update_pool(&pool);
        let edges = graph.snapshot_edges();
        assert_eq!(edges.len(), 1, "bids-only must produce exactly 1 edge");
        assert!(edges[0].a_to_b, "the single edge must be a→b");
    }

    #[test]
    fn phoenix_empty_book_creates_no_edges() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(0.0, 0.0);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 0, "empty book must produce 0 edges");
    }

    #[test]
    fn phoenix_a_to_b_weight_uses_bid_price() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0); // fee=0
        graph.update_pool(&pool);
        let edges = graph.snapshot_edges();
        let a_to_b = edges.iter().find(|e| e.a_to_b).expect("a→b edge must exist");
        // weight = -ln(bid_price * fee) = -ln(10.0 * 1.0) = -ln(10.0)
        let expected = -(10.0f64).ln();
        assert!((a_to_b.weight - expected).abs() < 1e-9,
            "a→b weight should be {expected}, got {}", a_to_b.weight);
    }

    #[test]
    fn phoenix_b_to_a_weight_uses_ask_price() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0); // fee=0
        graph.update_pool(&pool);
        let edges = graph.snapshot_edges();
        let b_to_a = edges.iter().find(|e| !e.a_to_b).expect("b→a edge must exist");
        // weight = -ln((1/ask_price) * fee) = -ln(1/11.0) = ln(11.0)
        let expected = -(1.0 / 11.0f64).ln();
        assert!((b_to_a.weight - expected).abs() < 1e-9,
            "b→a weight should be {expected}, got {}", b_to_a.weight);
    }

    #[test]
    fn phoenix_stale_edge_removed_when_price_drops_to_zero() {
        let graph = ExchangeGraph::new();
        let pool = phoenix_pool_with_prices(10.0, 11.0);
        graph.update_pool(&pool);
        assert_eq!(graph.edge_count(), 2);

        // Ask dries up — simulate by clearing damm_virtual_price
        pool.damm_virtual_price.store(0, Ordering::Relaxed);
        graph.update_pool(&pool);

        let edges = graph.snapshot_edges();
        assert_eq!(edges.len(), 1, "stale b→a edge must be removed when ask drops to 0");
        assert!(edges[0].a_to_b, "remaining edge must be a→b");
    }
}
```

- [ ] **Step 2: Run tests to confirm they fail**

```bash
cargo test --bin solana-mev exchange_graph -- --nocapture 2>&1
```

Expected: all phoenix tests fail (Phoenix currently shares the CLMM branch which reads only `sqrt_price_x64` and requires both edges to be valid simultaneously).

- [ ] **Step 3: Implement the Phoenix branch in `update_pool`**

In `src/graph/exchange_graph.rs`, add the Phoenix early-return block just after the stable-pool block (after line 83, before the `let (rate_a_to_b, rate_b_to_a) = match pool.dex {` line). Also remove `DexKind::Phoenix` from the CLMM match arm.

**A. Add the Phoenix block (insert before the existing `let (rate_a_to_b, rate_b_to_a) = match` line):**

```rust
        // Phoenix CLOB: bid and ask prices live in separate atomics. Edge directions
        // are independent — only insert edges with a valid non-zero price, and remove
        // stale edges when a price drops to zero (e.g., order book side dries up).
        if pool.dex == DexKind::Phoenix {
            let bid_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
            let ask_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
            if bid_bits == 0 && ask_bits == 0 {
                return;
            }
            let fee = 1.0 - (pool.fee_bps.load(Ordering::Relaxed) as f64 / 10_000.0);

            if bid_bits > 0 {
                let bid = f64::from_bits(bid_bits);
                let weight = -(bid * fee).ln();
                if bid > 0.0 && weight.is_finite() {
                    self.edges.insert(
                        (pool.token_a, pool.token_b, pool.id),
                        Edge { from: pool.token_a, to: pool.token_b, weight,
                               pool_id: pool.id, dex: pool.dex, a_to_b: true },
                    );
                }
            } else {
                self.edges.remove(&(pool.token_a, pool.token_b, pool.id));
            }

            if ask_bits > 0 {
                let ask = f64::from_bits(ask_bits);
                let weight = -(1.0 / ask * fee).ln();
                if ask > 0.0 && weight.is_finite() {
                    self.edges.insert(
                        (pool.token_b, pool.token_a, pool.id),
                        Edge { from: pool.token_b, to: pool.token_a, weight,
                               pool_id: pool.id, dex: pool.dex, a_to_b: false },
                    );
                }
            } else {
                self.edges.remove(&(pool.token_b, pool.token_a, pool.id));
            }

            self.generation.fetch_add(1, Ordering::Release);
            return;
        }
```

**B. Remove `DexKind::Phoenix` from the existing CLMM match arm and its preceding comment.**

Remove the three-line Phoenix comment block that precedes the match arm (lines 86–88):
```rust
        // Phoenix is a CLOB — vault balances are book collateral depth, not price.
        // `parse_state` navigates the on-chain order book to set sqrt_price_x64
        // (as the mid-price in raw token units), so we use the same path as CLMM pools.
```

Change the match arm from:
```rust
            DexKind::OrcaWhirlpool | DexKind::RaydiumClmm | DexKind::MeteoraDlmm | DexKind::Phoenix
            | DexKind::Lifinity | DexKind::Invariant => {
```
To:
```rust
            DexKind::OrcaWhirlpool | DexKind::RaydiumClmm | DexKind::MeteoraDlmm
            | DexKind::Lifinity | DexKind::Invariant => {
```

- [ ] **Step 4: Run tests to confirm they pass**

```bash
cargo test --bin solana-mev exchange_graph -- --nocapture 2>&1
```

Expected: all 7 exchange_graph tests pass.

- [ ] **Step 5: Run full test suite to check for regressions**

```bash
cargo test --bin solana-mev 2>&1
```

Expected: all 99 tests pass (plus the new ones = more than 99).

- [ ] **Step 6: Commit**

```bash
git add src/graph/exchange_graph.rs
git commit -m "feat(phoenix): directional edges in exchange_graph, remove stale edges on price drop"
```

---

## Task 4: Add Phoenix to BF stats log

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 1: Update the stats format string**

Find the `info!` macro in `src/main.rs` that contains `"BF window —"` (around line 627). Change the format string and argument list to include Phoenix:

Current:
```rust
info!(
    "BF window — runs={} neg_cycles={} evaluated={} profitable={} ({:.1} runs/s) \
     best_margin={:+.2}bps best_overall={} tip_floor_ema50={} | \
     edges={} (raydium={} clmm={} orca={} damm={} dlmm={}) avg_paths/run={:.0}",
    stat_bf_runs, stat_cycles, stat_eval_rejected + stat_profitable,
    stat_profitable, stat_bf_runs as f64 / secs, stat_best_gross_bps,
    best_overall_str, floor_str, edges,
    by_dex[0], by_dex[1], by_dex[2], by_dex[3], by_dex[4], avg_paths,
);
```

Replace with:
```rust
info!(
    "BF window — runs={} neg_cycles={} evaluated={} profitable={} ({:.1} runs/s) \
     best_margin={:+.2}bps best_overall={} tip_floor_ema50={} | \
     edges={} (raydium={} clmm={} orca={} damm={} dlmm={} phoenix={}) avg_paths/run={:.0}",
    stat_bf_runs, stat_cycles, stat_eval_rejected + stat_profitable,
    stat_profitable, stat_bf_runs as f64 / secs, stat_best_gross_bps,
    best_overall_str, floor_str, edges,
    by_dex[0], by_dex[1], by_dex[2], by_dex[3], by_dex[4], by_dex[5], avg_paths,
);
```

- [ ] **Step 2: Build to confirm it compiles**

```bash
cargo build --release 2>&1
```

Expected: `Finished` with no errors.

- [ ] **Step 3: Run full test suite**

```bash
cargo test --bin solana-mev 2>&1
```

Expected: all tests pass.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "feat(phoenix): add phoenix edge count to BF window stats log"
```

---

## Verification

After all tasks complete, the live bot should show Phoenix edges in the 10-second stats line:

```
BF window — ... edges=42 (raydium=12 clmm=8 orca=6 damm=4 dlmm=6 phoenix=6) ...
```

`phoenix=0` means Phoenix state accounts aren't being streamed yet (check that pools.json entries have `state_account` set — `fetch_phoenix.js` already generates this correctly). `phoenix>0` confirms the integration is live.
