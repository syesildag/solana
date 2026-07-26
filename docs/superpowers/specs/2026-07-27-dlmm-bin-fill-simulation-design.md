# DLMM bin-array fill simulation — design

**Date:** 2026-07-27
**Status:** approved (transport + rollout chosen by user)
**Motivation:** the 2026-07-26 raw-RPC preflight reject (`ExceededAmountSlippageTolerance`
on USDC→PUMP→USDC, closing hop on 80 bps-bin-step pool `88LoXa6p`) exposed that the DLMM
quote is bin-depth-blind: it prices the whole fill at the active-bin mid with a
total-vault CP haircut, so any fill that drains the active bin over-quotes by up to
`bin_step` per bin crossed. A second, independent over-quote: the static
`fee_bps` from pools.json ignores DLMM's dynamic (volatility-driven) fee.

## Goal

Quote DLMM hops with a real staircase fill walk over live per-bin liquidity plus the
real dynamic fee, so the evaluator sizes into fills that actually exist on-chain and
raw-path preflight rejects stop being systematic on DLMM↔DLMM cycles.

Non-goals: Orca/Raydium CLMM tick-walk (same disease, future work); modeling DLMM
limit-order liquidity (skipping it under-fills — safe); changing graph edge weights
(active-bin marginal price is correct for edge detection).

## Verified on-chain facts (Meteora `dlmm-sdk`, commons crate + `idls/dlmm.json`)

- `BinArray` account: 8-byte Anchor discriminator + `index: i64` @8 + `version: u8` @16
  + 7 pad + `lb_pair: Pubkey` @24..56 + `bins: [Bin; 70]` @56 → **10,136 bytes total**.
- `Bin` = **144 bytes**; the walk needs only `amount_x: u64` @+0 and `amount_y: u64` @+8.
  (Current layout has limit-order fields where rewards used to be; size unchanged.)
- Bin price from id: `(1 + bin_step/10_000)^bin_id`, Q64.64. In-bin math (Meteora
  `commons/src/extensions/bin.rs`): selling X for Y → `out = mul_shr(price, in, 64)`
  capped at the bin's `amount_y`; selling Y for X → `out = shl_div(in, price, 64)`
  capped at `amount_x`. Rounding::Down on outputs, ::Up on capacity-in.
- Fees (`extensions/lb_pair.rs`): `base_fee = base_factor × bin_step × 10 ×
  10^base_fee_power_factor` (1e9 scale); `variable_fee = ceil(variable_fee_control ×
  (volatility_accumulator × bin_step)² / 1e11)` when `variable_fee_control > 0`.
  Fee is charged on input: `fee = ceil(amount × rate / 1e9)` (`compute_fee_from_amount`),
  and bin capacity-in is grossed up by `compute_fee`.
- The volatility accumulator updates per swap: `update_references` (time decay via
  `filter_period`/`decay_period`/`reduction_factor`) then `update_volatility_accumulator`
  per bin crossed, capped at `max_volatility_accumulator`. The quote simulates this
  exactly as Meteora's `quote_exact_in` does.
- `LbPair` account: `StaticParameters` @8..40 (`base_factor` u16 @8,
  `filter_period` u16 @10, `decay_period` u16 @12, `reduction_factor` u16 @14,
  `variable_fee_control` u32 @16, `max_volatility_accumulator` u32 @20,
  `min_bin_id`/`max_bin_id` i32 @24/@28, `protocol_share` u16 @32,
  `base_fee_power_factor` u8 @34), `VariableParameters` @40..72
  (`volatility_accumulator` u32 @40, `volatility_reference` u32 @44,
  `index_reference` i32 @48, `last_update_timestamp` i64 @56). Both live in the
  lb_pair account the bot ALREADY subscribes to (`state_account`); `active_id` @76 and
  `token_x_mint` @88 reads are unchanged.
- Bin-array PDA: `["bin_array", lb_pair, index_i64_le]`, index =
  `active_id.div_euclid(70)` (already implemented in `build_swap_instruction`).

## Design

### 1. Data model — per-pool bin cache on `Pool`

New field on `Pool` (`src/dex/types.rs`):

```rust
pub dlmm_bins: std::sync::RwLock<DlmmBinCache>,
```

```rust
pub struct DlmmBinCache {
    /// array index → (amount_x, amount_y) per bin slot
    pub arrays: BTreeMap<i64, [(u64, u64); 70]>,
    pub fee: DlmmFeeParams,   // static params + vol accumulator snapshot from lb_pair
    pub stamped_ns: u64,      // 0 = never populated
}
```

- Bins can't be atomics: a walk must see one array consistently. Writes are rare
  (one per DLMM swap/liquidity event per pool); the hot path uses `try_read()` and
  **falls back to the legacy haircut quote on contention or missing/stale data** —
  it never blocks.
- Pruned to active-array ±2 on every write (~62 KB across 11 DLMM pools).
- `Pool` struct literals in tests gain one field each (mechanical; no `Default`).

### 2. Fee params — decode from the already-subscribed lb_pair

Extend the DLMM branch of `parse_state` (`src/dex/dlmm.rs`) to decode the static +
variable parameter blocks (offsets above) and write them into `dlmm_bins.fee`
alongside the existing `active_bin_id` / orientation stores. Zero new subscriptions.
The walk computes the real per-swap fee (base + variable, with per-bin volatility
accumulation simulated at quote time using wall clock vs `last_update_timestamp`).

### 3. The walk — `dlmm::walk_quote`

`fn walk_quote(pool, amount_in, a_to_b) -> Option<SwapQuote>`, a port of Meteora's
`quote_exact_in` minus limit orders and transfer fees:

1. `try_read()` the cache; `None` if contended, `stamped_ns` stale (reuse the pool
   staleness threshold), or the active bin's array is absent.
2. Resolve direction: `swap_for_y = (token_a_is_x == a_to_b)` (same rule as the
   builder). Walk from `active_id`, per bin: capacity-out = opposing side amount;
   capacity-in grossed up by fee; fill `min(remaining, capacity)`, accumulate out with
   Q64.64 rounding-down; advance bin (down for `swap_for_y`, up otherwise), bumping
   the simulated volatility accumulator.
3. If input remains when the cached window is exhausted → `None` (fall back rather
   than fabricate depth).
4. Returns `SwapQuote` with real `fee_amount` and `price_impact` =
   `1 − avg_fill_price/active_bin_price`.

`get_quote` becomes mode-dependent (§5): `live` → `walk_quote(...).unwrap_or(haircut)`;
`shadow`/`off` → haircut (unchanged hot path).

Token-2022 transfer-fee guard: at startup, any DLMM pool whose mint carries the
transfer-fee extension is pinned to the haircut path with a warning (walk would
over-quote; none of the current book's mints use it, but PUMP-style Token-2022 mints
make the check mandatory).

### 4. Transport — per-pool gRPC memcmp filter (user-chosen)

`build_subscription` (`src/streamer/subscription.rs`) gains one named account filter
per DLMM pool: `owner = [LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo]`,
`filters = [memcmp(offset 24, lb_pair_bytes)]`. Every bin array of the pool streams
automatically — active-bin migration across array boundaries needs no resubscribe and
no PDA bookkeeping.

Callback dispatch (`main.rs` callback + `backfill::apply_polled_account`): an account
update with `data.len() == 10_136` and the `BinArray` discriminator resolves its pool
via bytes @24..56 → `get_by_pool_id` → decode `index` @8 + 70 amount pairs → store,
prune window, `stamped_ns = now`. **No `graph.update_pool`, no BF poke** (bins don't
move the marginal rate; the same tx's lb_pair/vault updates already poke).

Guards: length + discriminator asserted before decode; mismatch → skip + rate-limited
warn (future layout change degrades to haircut, never corrupts quotes).

Startup seeding: one `getMultipleAccounts` for active±1 arrays per DLMM pool so the
walk works before the first gRPC bin update. Backfill: DLMM pools' `accounts_for`
target list gains the same 3 derived PDAs so an outage doesn't freeze bins.

### 5. Rollout — `DLMM_BIN_QUOTE = off | shadow | live` (default `shadow`)

Shadow mode: hot quote unchanged; the **evaluator** (not `get_quote` — ternary search
calls it dozens of times per cycle) runs one walk per DLMM hop at the final chosen
size and appends `walk_out` vs `haircut_out` vs realized margin to the existing
near-miss / opportunity log lines. One live session answers "would the walk have
predicted the preflight rejects?"; then flip `live` in `.env`.

### 6. Side fix — swap-builder bin-array coverage

`build_swap_instruction` currently hardcodes 2 bin arrays (current + 1 neighbor); a
fill crossing into the 2nd neighbor reverts with an account-not-found. With the cache
present, compute actual coverage from the walk's terminal bin and pass up to 3 arrays;
without cache, keep today's 2-array behavior.

### 7. Testing

- Layout round-trip against a vendored mainnet fixture from Meteora's repo
  (`commons/tests/fixtures/*/bin_array_*.bin` + `lb_pair.bin`).
- Synthetic unit tests in `dlmm.rs` `#[cfg(test)]`: single-bin fill; multi-bin
  staircase (assert out < active-bin-linear out); drained-bin skip; both orientations
  (`dlmm_token_a_is_x` 1 and 2); dynamic fee growth with `variable_fee_control > 0`;
  stale/absent-cache fallback to haircut; builder 3-array coverage.
- `cargo test --bin solana-mev dlmm` per repo convention.

## Risks / accepted limitations

- **Skipped limit-order liquidity** under-fills → occasional missed sizing headroom,
  never an over-quote.
- **memcmp filter support** depends on the gRPC provider honoring standard Yellowstone
  filters; if rejected, the stream error surfaces at startup and the fallback is the
  static-PDA-list transport (not built now — revisit only if it bites).
- **Volatility-accumulator drift**: quote-time simulation uses wall clock vs the
  chain's `last_update_timestamp`; sub-second skew slightly under/over-states the
  variable fee. Bounded by `max_volatility_accumulator`; direction is symmetric and
  small next to the bps-scale errors this design removes.
- **CLMM pools keep the haircut** — DLMM↔CLMM cycles improve on one leg only.
