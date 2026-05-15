# Phoenix CLOB Directional Pricing

**Date:** 2026-05-15  
**Status:** Approved

## Problem

Phoenix pools are loaded and pass config validation, but contribute zero edges to the exchange graph. The root cause is in `parse_state` (src/dex/phoenix.rs): it averages best-bid and best-ask into a single mid-price, stores it in `sqrt_price_x64`, and returns price=0 when either side of the order book is empty. A zero price skips the pool in `update_pool` (exchange_graph.rs), so Phoenix never appears in Bellman-Ford cycles.

A secondary issue is correctness: a CLOB is not symmetric — selling base uses the bid price, buying base uses the ask price. Using mid-price overestimates the rate you'd actually receive.

## Goal

Phoenix pools produce valid graph edges whenever at least one side of the order book has resting orders. Cycles through Phoenix are evaluated using the price you'd actually transact at (bid for a→b, ask for b→a).

## Design

### Storage

Reuse two existing `AtomicU64` fields on `Pool` that are zero for Phoenix pools:

| Field | Stores | Used by |
|---|---|---|
| `sqrt_price_x64` | best-bid price as f64 bits | a→b edge, `get_quote` when `a_to_b=true` |
| `damm_virtual_price` | best-ask price as f64 bits | b→a edge, `get_quote` when `a_to_b=false` |

No new struct fields required.

### `src/dex/phoenix.rs` — `parse_state`

**Current:** computes `mid_ticks = (best_bid_ticks + best_ask_ticks) / 2`, stores one price in `sqrt_price_x64`.

**New:** compute bid and ask separately from their respective ticks:

```
bid_price = best_bid_ticks × tick_size × quote_lot / (base_lots_per_unit × base_lot)
ask_price = best_ask_ticks × tick_size × quote_lot / (base_lots_per_unit × base_lot)
```

Store bid as `f64::to_bits()` in `pool.sqrt_price_x64`.  
Store ask as `f64::to_bits()` in `pool.damm_virtual_price`.  
If a side is empty (no resting orders), its price stays 0 — the corresponding edge direction is disabled.

### `src/dex/phoenix.rs` — `get_quote`

**Current:** reads `sqrt_price_x64` for both directions.

**New:** direction-dependent read:

```rust
let price_bits = if a_to_b {
    pool.sqrt_price_x64.load(Ordering::Relaxed)      // bid price
} else {
    pool.damm_virtual_price.load(Ordering::Relaxed)  // ask price
};
```

Rest of the function (lot conversion, fee application, zero guard) is unchanged.

### `src/graph/exchange_graph.rs` — `update_pool` Phoenix branch

**Current:**
```rust
DexKind::Phoenix | ... => {
    let price_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
    if price_bits == 0 { return; }
    let price = f64::from_bits(price_bits);
    (price * fee, (1.0 / price) * fee)
}
```

**New:** read bid and ask independently; skip individual edge directions when their price is zero:

```rust
DexKind::Phoenix => {
    let bid_bits = pool.sqrt_price_x64.load(Ordering::Relaxed);
    let ask_bits = pool.damm_virtual_price.load(Ordering::Relaxed);
    if bid_bits == 0 && ask_bits == 0 { return; }
    let fee = 1.0 - (pool.fee_bps.load(Ordering::Relaxed) as f64 / 10_000.0);
    let rate_a_to_b = if bid_bits > 0 { f64::from_bits(bid_bits) * fee } else { 0.0 };
    let rate_b_to_a = if ask_bits > 0 { (1.0 / f64::from_bits(ask_bits)) * fee } else { 0.0 };
    (rate_a_to_b, rate_b_to_a)
}
```

Zero rates produce `+∞` edge weights (never negative cycles), so they effectively disable that direction without requiring special handling downstream.

Phoenix must also be removed from the `OrcaWhirlpool | RaydiumClmm | MeteoraDlmm | Phoenix | Lifinity | Invariant` group so it gets its own branch.

## Edge Cases

| Scenario | Behavior |
|---|---|
| Two-sided book, tight spread | Both edges valid; Phoenix may enter cycles |
| Asks only (no bids) | `sqrt_price_x64=0` → a→b edge disabled; b→a edge valid |
| Bids only (no asks) | `damm_virtual_price=0` → b→a edge disabled; a→b edge valid |
| Empty book | Both prices 0 → `update_pool` returns early (no edges) |
| ask_price < bid_price (crossed book) | Both edges technically valid but cycle would be arbitrageable in itself; BF will detect it |

## Files Changed

| File | Change |
|---|---|
| `src/dex/phoenix.rs` | `parse_state`: compute bid+ask separately, store in two atomics. `get_quote`: direction-dependent price read. |
| `src/graph/exchange_graph.rs` | Phoenix gets its own branch in `update_pool`; reads bid and ask independently. |

**Files NOT changed:** `src/dex/mod.rs`, `src/dex/types.rs`, `src/main.rs`, `src/streamer/subscription.rs`.

## Tests

Three new unit tests in `src/dex/phoenix.rs` tests module:

1. **`bid_lower_than_ask_on_symmetric_book`** — provide mock data with two-sided book; assert bid < ask and both > 0.
2. **`asks_only_book_gives_zero_bid`** — mock with no bids; assert `sqrt_price_x64 == 0`, `damm_virtual_price > 0`.
3. **`empty_book_gives_zero_both`** — mock with no orders; assert both fields == 0, no panic.

Note: no Phoenix tests exist today. Tests require constructing mock FIFOMarket byte arrays from scratch matching the documented layout constants in phoenix.rs (FIFO_PREFIX=880, TICK_SIZE_OFF=840, BASE_LOTS_OFF=832).

## Verification

```bash
cargo build --release

cargo test --bin solana-mev phoenix -- --nocapture

# Run the bot with RUST_LOG=solana_mev=debug to observe Phoenix edges.
# The 10s BF window stats line currently prints raydium/clmm/orca/damm/dlmm counts
# but not Phoenix separately. Add Phoenix to the stats format in main.rs or check
# by_dex[5] > 0 via debug log to confirm the integration is live.
```
