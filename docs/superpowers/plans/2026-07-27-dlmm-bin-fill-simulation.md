# DLMM Bin-Array Fill Simulation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the bin-depth-blind DLMM quote with a real staircase fill walk over live per-bin liquidity plus the real dynamic fee, streamed via per-pool gRPC memcmp filters, rolled out shadow-first.

**Architecture:** A `RwLock<DlmmBinCache>` on `Pool` holds a ±2-array window of `(amount_x, amount_y)` bins plus fee params decoded from the already-subscribed lb_pair. `dlmm::walk_quote` ports Meteora's `quote_exact_in` (minus limit orders / transfer fees) and is consulted by `get_quote` only in `live` mode; `shadow` mode logs walk-vs-haircut divergence from the evaluator. Bin arrays stream via one owner+memcmp filter per DLMM pool; startup seeds via RPC; the backfill poller covers stream gaps.

**Tech Stack:** Rust, tokio, yellowstone-grpc-proto, solana-sdk. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-27-dlmm-bin-fill-simulation-design.md`

## Global Constraints

- **NEVER run `cargo fmt` or `rustfmt`** — the repo is not rustfmt-clean; whole-file formatting causes huge diff churn.
- Tests live in `#[cfg(test)]` blocks at the bottom of each source file (repo convention).
- Test command: `cargo test --bin solana-mev <filter>`; full build check: `cargo build --release`.
- Commit after each task. **Commit only — do NOT `git push`.**
- Hot path must never block: bin cache reads use `try_read()`, fall back to the existing haircut quote on any failure.
- All Q64.64 rounding: outputs round **down**, input-capacity and fees round **up** (matches `lb_clmm`).
- `MAX_BIN_PER_ARRAY = 70` (already a const in `src/dex/dlmm.rs`).
- BinArray account: **10,136 bytes**, discriminator `[92, 142, 92, 220, 5, 148, 70, 181]`, `index: i64` @8, `lb_pair: Pubkey` @24..56, bins @56, each `Bin` 144 B with `amount_x: u64` @+0, `amount_y: u64` @+8.
- LbPair: `StaticParameters` @8..40, `VariableParameters` @40..72, `active_id` @76 (existing read), `token_x_mint` @88 (existing read).

---

### Task 1: `DlmmBinCache` types + `Pool.dlmm_bins` field

**Files:**
- Modify: `src/dex/types.rs` (Pool struct ~line 400, plus the 2 Pool literals in this file ~lines 483, 732)
- Modify (one line each — add `dlmm_bins: Default::default(),` to the Pool literal):
  `src/arbitrage/evaluator.rs:802`, `src/graph/bellman_ford.rs:197`, `src/graph/exchange_graph.rs:379`, `src/streamer/backfill.rs:288`, `src/dex/raydium_clmm.rs:408`, `src/portfolio/feed_setup.rs:581`, `src/dex/orca.rs:239`, `src/dex/orca.rs:370`, `src/dex/dlmm.rs:262`, `src/dex/lifinity.rs:136`, `src/dex/raydium_amm.rs:156`, `src/dex/saber.rs:138`, `src/dex/phoenix.rs:224`, `src/dex/pumpswap.rs:292`
  (line numbers are pre-change anchors — locate each by searching for `dlmm_token_a_is_x: AtomicU64::new(`)
- Test: `src/dex/types.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `pub struct DlmmFeeParams` (all fields `pub`, `Clone, Copy, Debug, Default, PartialEq`), `pub struct DlmmBinCache { pub arrays: BTreeMap<i64, [(u64, u64); 70]>, pub fee: DlmmFeeParams, pub stamped_ns: u64 }` (`Default`), `Pool.dlmm_bins: std::sync::RwLock<DlmmBinCache>`.

- [ ] **Step 1: Add the types and field**

In `src/dex/types.rs`, above `pub struct Pool`:

```rust
/// Meteora DLMM dynamic-fee parameters, decoded from the lb_pair account
/// (StaticParameters @8..40, VariableParameters @40..72). Defaults (all zero)
/// mean "not yet decoded" — walk_quote treats a zero base_factor as unusable.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DlmmFeeParams {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub base_fee_power_factor: u8,
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
}

/// Live per-bin liquidity window for one DLMM pool: bin-array index →
/// (amount_x, amount_y) per bin slot. Kept to active-array ±2 by
/// `dlmm::store_bin_array`. Bins are event-sourced (they only change when a
/// tx touches them), so freshness is "seeded at least once" (stamped_ns != 0),
/// not a time threshold — the gRPC memcmp stream + backfill keep it current.
#[derive(Default)]
pub struct DlmmBinCache {
    pub arrays: std::collections::BTreeMap<i64, [(u64, u64); 70]>,
    pub fee: DlmmFeeParams,
    pub stamped_ns: u64,
}
```

In `pub struct Pool`, after `dlmm_token_a_is_x`:

```rust
    /// Meteora DLMM only: live bin-liquidity window + fee params for the fill
    /// walk. Hot path uses try_read() and falls back to the haircut quote —
    /// never blocks. Always Default (empty) for non-DLMM pools.
    pub dlmm_bins: std::sync::RwLock<DlmmBinCache>,
```

Then add `dlmm_bins: Default::default(),` to **every** Pool struct literal listed above (16 sites; find them with `grep -rn "dlmm_token_a_is_x: AtomicU64::new(" src/`).

- [ ] **Step 2: Write the test**

In `src/dex/types.rs` `#[cfg(test)]` block:

```rust
#[test]
fn dlmm_bin_cache_default_is_empty_and_unstamped() {
    let c = DlmmBinCache::default();
    assert!(c.arrays.is_empty());
    assert_eq!(c.stamped_ns, 0);
    assert_eq!(c.fee, DlmmFeeParams::default());
}
```

- [ ] **Step 3: Build + run tests**

Run: `cargo build --release 2>&1 | tail -5` — expect success (this catches any missed literal site).
Run: `cargo test --bin solana-mev dlmm_bin_cache_default -v` — expect PASS.
Run: `cargo test --bin solana-mev 2>&1 | tail -3` — expect all existing tests still pass.

- [ ] **Step 4: Commit**

```bash
git add -A src/
git commit -m "feat(dlmm): add DlmmBinCache + fee params storage on Pool"
```

---

### Task 2: BinArray decode + vendored mainnet fixture

**Files:**
- Modify: `src/dex/dlmm.rs`
- Create: `assets/test_fixtures/dlmm/bin_array_1.bin`, `assets/test_fixtures/dlmm/lb_pair.bin`
- Test: `src/dex/dlmm.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `pub const BIN_ARRAY_LEN: usize = 10_136`, `pub const BIN_ARRAY_DISCRIMINATOR: [u8; 8]`, `pub fn decode_bin_array(data: &[u8]) -> Option<(i64, Pubkey, [(u64, u64); 70])>` (returns index, lb_pair, bins).

- [ ] **Step 1: Vendor the fixtures**

```bash
mkdir -p assets/test_fixtures/dlmm
BASE="https://raw.githubusercontent.com/MeteoraAg/dlmm-sdk/main/commons/tests/fixtures/9t3EyC9FweyL7PBWvKz3mrXg8B9fwFc9SK3QxM4ENqhd"
curl -sL "$BASE/bin_array_1.bin" -o assets/test_fixtures/dlmm/bin_array_1.bin
curl -sL "$BASE/lb_pair.bin"     -o assets/test_fixtures/dlmm/lb_pair.bin
ls -la assets/test_fixtures/dlmm/   # bin_array_1.bin MUST be exactly 10136 bytes
```

If the download fails or sizes are wrong, STOP and report — do not fabricate fixtures.

- [ ] **Step 2: Write the failing tests**

In `src/dex/dlmm.rs` tests:

```rust
const FIXTURE_LB_PAIR: &str = "9t3EyC9FweyL7PBWvKz3mrXg8B9fwFc9SK3QxM4ENqhd";

#[test]
fn decode_bin_array_fixture_roundtrip() {
    let data: &[u8] = include_bytes!("../../assets/test_fixtures/dlmm/bin_array_1.bin");
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
    let mut data = include_bytes!("../../assets/test_fixtures/dlmm/bin_array_1.bin").to_vec();
    data[0] ^= 0xFF;
    assert!(decode_bin_array(&data).is_none(), "wrong discriminator");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --bin solana-mev decode_bin_array -v`
Expected: FAIL — `decode_bin_array`/`BIN_ARRAY_LEN` not defined.

- [ ] **Step 4: Implement the decoder**

In `src/dex/dlmm.rs` (near the top, after the existing consts):

```rust
/// BinArray account: 8-byte Anchor discriminator + index i64 @8 + version u8 @16
/// + 7 pad + lb_pair Pubkey @24..56 + [Bin; 70] @56. Each Bin is 144 bytes with
/// amount_x u64 @+0 and amount_y u64 @+8 (the only fields the fill walk needs).
/// Verified against MeteoraAg/dlmm-sdk idls/dlmm.json (2026-07-27).
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
```

Add `use std::str::FromStr;` to the test module if not present (it is — existing tests use it).

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --bin solana-mev decode_bin_array -v` — expect 2 PASS.

- [ ] **Step 6: Commit**

```bash
git add src/dex/dlmm.rs assets/test_fixtures/dlmm/
git commit -m "feat(dlmm): BinArray account decoder + vendored mainnet fixture"
```

---

### Task 3: `store_bin_array` cache write path with window pruning

**Files:**
- Modify: `src/dex/dlmm.rs`
- Test: `src/dex/dlmm.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `decode_bin_array` (Task 2), `Pool.dlmm_bins` (Task 1), `types::monotonic_now_ns()`.
- Produces: `pub fn store_bin_array(pool: &Pool, data: &[u8]) -> bool` (true = stored; false = decode failure or foreign lb_pair). Prunes cached arrays to active-array ±2.

- [ ] **Step 1: Write the failing test**

The existing test helper `sol_usdc_dlmm_pool()` builds a DLMM pool (POOL_ID `HTvjzsfX…`). Add a helper that fabricates a valid BinArray account for it:

```rust
/// Build a synthetic BinArray account for `lb_pair` with the given index;
/// every bin gets (amount_x, amount_y) = amounts.
fn synth_bin_array(lb_pair: &Pubkey, index: i64, amounts: (u64, u64)) -> Vec<u8> {
    let mut data = vec![0u8; BIN_ARRAY_LEN];
    data[0..8].copy_from_slice(&BIN_ARRAY_DISCRIMINATOR);
    data[8..16].copy_from_slice(&index.to_le_bytes());
    data[24..56].copy_from_slice(lb_pair.as_ref());
    for i in 0..70 {
        let off = 56 + i * 144;
        data[off..off + 8].copy_from_slice(&amounts.0.to_le_bytes());
        data[off + 8..off + 16].copy_from_slice(&amounts.1.to_le_bytes());
    }
    data
}

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

#[test]
fn store_bin_array_rejects_foreign_lb_pair() {
    let pool = sol_usdc_dlmm_pool();
    let foreign = Pubkey::new_unique();
    assert!(!store_bin_array(&pool, &synth_bin_array(&foreign, 0, (1, 1))));
    assert!(pool.dlmm_bins.read().unwrap().arrays.is_empty());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev store_bin_array -v`
Expected: FAIL — `store_bin_array` not defined.

- [ ] **Step 3: Implement**

```rust
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
```

(Confirm the exact path of `monotonic_now_ns` — it is referenced as `types::monotonic_now_ns()` from `src/dex/mod.rs`; `dlmm.rs` already imports `crate::dex::types::{self, ...}`.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin solana-mev store_bin_array -v` — expect 2 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dex/dlmm.rs
git commit -m "feat(dlmm): store_bin_array cache write with ±2-array window prune"
```

---

### Task 4: lb_pair fee-parameter decode in `parse_state`

**Files:**
- Modify: `src/dex/dlmm.rs` (`parse_state`, ~line 27)
- Test: `src/dex/dlmm.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `DlmmFeeParams` (Task 1).
- Produces: `parse_state` additionally writes `pool.dlmm_bins.fee` on every lb_pair update. Signature unchanged.

- [ ] **Step 1: Write the failing test**

```rust
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
    let data: &[u8] = include_bytes!("../../assets/test_fixtures/dlmm/lb_pair.bin");
    assert_eq!(&data[0..8], &[33, 11, 49, 98, 181, 101, 177, 13], "LbPair discriminator");
    let base_factor = u16::from_le_bytes(data[8..10].try_into().unwrap());
    let decay = u16::from_le_bytes(data[12..14].try_into().unwrap());
    let filter = u16::from_le_bytes(data[10..12].try_into().unwrap());
    assert!(base_factor > 0, "fixture base_factor zero — offsets wrong");
    assert!(decay > filter, "decay_period must exceed filter_period");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev parse_state_decodes_fee -v`
Expected: FAIL — fee params all zero (not yet decoded).

- [ ] **Step 3: Implement**

In `parse_state`, after the existing `dlmm_token_a_is_x` store and before the final `Some((price, 0))`:

```rust
    // Dynamic-fee parameters ride the same account (StaticParameters @8..40,
    // VariableParameters @40..72) — decode on every lb_pair update so the fill
    // walk always sees the current volatility accumulator.
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
```

(`try_write` — parse_state runs on the stream callback; skipping one fee refresh under contention is harmless, blocking the callback is not.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin solana-mev parse_state -v` — expect the 2 new tests PASS and any existing parse_state tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dex/dlmm.rs
git commit -m "feat(dlmm): decode dynamic-fee params from lb_pair into bin cache"
```

---

### Task 5: the fill walk — `walk_quote`

**Files:**
- Modify: `src/dex/dlmm.rs`
- Test: `src/dex/dlmm.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `DlmmBinCache` (Task 1), `store_bin_array`/`synth_bin_array` (Task 3), fee params (Task 4).
- Produces:
  - `pub(crate) struct WalkFill { pub amount_out: u64, pub fee_amount: u64, pub terminal_bin: i32 }`
  - `pub(crate) fn walk_fill(pool: &Pool, amount_in: u64, swap_for_y: bool, now_unix_ts: i64) -> Option<WalkFill>`
  - `pub fn walk_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> Option<SwapQuote>` (wraps walk_fill with wall-clock time; `None` = fall back to haircut)
  - `fn total_fee_rate(fee: &DlmmFeeParams, bin_step: u16, vol_acc: u32) -> u128` and `fn simulated_vol_state(fee: &DlmmFeeParams, active_id: i32, now_ts: i64) -> (u32, i32)` (unit-tested directly)
- Task 6 will call `walk_quote` from `get_quote`; Task 9 will call `walk_fill` for builder coverage.

- [ ] **Step 1: Write the failing fee-math tests**

```rust
#[test]
fn total_fee_rate_base_and_variable() {
    // base = base_factor × bin_step × 10 × 10^power (1e9 scale)
    let mut fee = crate::dex::types::DlmmFeeParams {
        base_factor: 10_000, base_fee_power_factor: 0, variable_fee_control: 0,
        ..Default::default()
    };
    // 10_000 × 100 × 10 = 1e7 = 1% of 1e9
    assert_eq!(total_fee_rate(&fee, 100, 0), 10_000_000);
    // variable: ceil(vfc × (vol_acc × bin_step)² / 1e11)
    fee.variable_fee_control = 7_500_000;
    // vol_acc=10_000 (one bin), bin_step=80: (10_000×80)² = 6.4e11
    // 7.5e6 × 6.4e11 / 1e11 = 4.8e7 → base(10_000×80×10=8e6) + 4.8e7 = 5.6e7
    assert_eq!(total_fee_rate(&fee, 80, 10_000), 8_000_000 + 48_000_000);
    // cap at MAX_FEE_RATE (10%)
    assert_eq!(total_fee_rate(&fee, 80, 300_000), 100_000_000);
}

#[test]
fn simulated_vol_state_reference_decay() {
    let fee = crate::dex::types::DlmmFeeParams {
        filter_period: 30, decay_period: 600, reduction_factor: 5_000,
        volatility_accumulator: 100_000, volatility_reference: 40_000,
        index_reference: 5, last_update_timestamp: 1_000,
        ..Default::default()
    };
    // elapsed < filter_period → stored references unchanged
    assert_eq!(simulated_vol_state(&fee, 9, 1_020), (40_000, 5));
    // filter ≤ elapsed < decay → vol_ref = acc × reduction / 10_000, index_ref = active
    assert_eq!(simulated_vol_state(&fee, 9, 1_100), (50_000, 9));
    // elapsed ≥ decay → full reset
    assert_eq!(simulated_vol_state(&fee, 9, 1_700), (0, 9));
}
```

- [ ] **Step 2: Write the failing walk tests**

```rust
/// Pool with bin_step=100 (1%), active_id=0 (price 1.0), 1% base fee, no
/// variable fee, orientation token_a=X, and one seeded bin array.
fn walk_test_pool(bins0: [(u64, u64); 70]) -> Arc<Pool> {
    let pool = sol_usdc_dlmm_pool();
    pool.extra_bin_step_override(); // see note below — set bin_step 100
    pool.active_bin_id.store(0, Ordering::Relaxed);
    {
        let mut cache = pool.dlmm_bins.write().unwrap();
        cache.arrays.insert(0, bins0);
        cache.fee = crate::dex::types::DlmmFeeParams {
            base_factor: 10_000, // ×100×10 = 1% of 1e9
            filter_period: 30, decay_period: 600, reduction_factor: 0,
            max_volatility_accumulator: 350_000,
            last_update_timestamp: i64::MAX - 1_000, // elapsed<filter → refs unchanged, vol=0
            ..Default::default()
        };
        cache.stamped_ns = 1;
    }
    pool
}
```

**Note:** `sol_usdc_dlmm_pool()` hardcodes `BIN_STEP=1`; instead of an override helper, change the test helper to take `bin_step: u16` as a parameter and update its existing callers (`build_swap_instruction` tests) to pass `1`. active_id=0 sits in the MIDDLE of array 0? No — bin 0 is slot 0 of array 0; walking down from bin 0 leaves array 0 immediately. For multi-bin-down tests use `active_id = 35` (slot 35 of array 0) and adjust expected prices accordingly, or seed array −1 too. Keep tests explicit about this.

```rust
#[test]
fn walk_single_bin_fill_exact() {
    // active_id=0 → price=1.0. Sell X for Y (a_to_b with orientation 1 → swap_for_y).
    let mut bins = [(0u64, 0u64); 70];
    bins[0] = (0, 1_000_000); // bin 0: 1e6 Y available
    let pool = walk_test_pool(bins);
    let q = walk_quote(&pool, 500_000, true).expect("must quote");
    // fee = ceil(500_000 × 1e7 / 1e9) = 5_000; out = 495_000 × 1.0
    assert_eq!(q.fee_amount, 5_000);
    assert_eq!(q.amount_out, 495_000);
}

#[test]
fn walk_multi_bin_staircase_beats_linear_illusion() {
    // active_id=35 (slot 35). Selling X: bin 35 has 300k Y, bin 34 has 1e6 Y
    // at price 1/1.01. A depth-blind quote would price everything at bin-35 price.
    let mut bins = [(0u64, 0u64); 70];
    bins[35] = (0, 300_000);
    bins[34] = (0, 1_000_000);
    let pool = walk_test_pool(bins);
    pool.active_bin_id.store(35, Ordering::Relaxed);
    let amount_in = 500_000u64;
    let q = walk_quote(&pool, amount_in, true).expect("must quote");
    let p35 = 1.01f64.powi(35);
    let linear_out = (amount_in as f64 * 0.99 * p35) as u64; // 1% fee, all at bin-35 price
    assert!(q.amount_out < linear_out, "staircase must under-fill the linear illusion");
    // and it must beat pricing everything at the NEXT bin down (sanity lower bound)
    let lower = (amount_in as f64 * 0.98 * p35 / 1.01) as u64;
    assert!(q.amount_out > lower);
}

#[test]
fn walk_skips_drained_bins() {
    let mut bins = [(0u64, 0u64); 70];
    bins[35] = (0, 100_000);
    bins[34] = (0, 0);          // drained — must be skipped, not terminate
    bins[33] = (0, 1_000_000);
    let pool = walk_test_pool(bins);
    pool.active_bin_id.store(35, Ordering::Relaxed);
    let q = walk_quote(&pool, 300_000, true).expect("must quote");
    assert!(q.amount_out > 100_000, "must fill past the drained bin");
}

#[test]
fn walk_orientation_reversed_pool() {
    // orientation 2 = token_b is X. a_to_b (sell token_a=Y for token_b=X) → swap_for_y=false → walk UP.
    let mut bins = [(0u64, 0u64); 70];
    bins[35] = (400_000, 0);   // X liquidity at bin 35
    bins[36] = (400_000, 0);   // X at bin 36 (price higher)
    let pool = walk_test_pool(bins);
    pool.active_bin_id.store(35, Ordering::Relaxed);
    pool.dlmm_token_a_is_x.store(2, Ordering::Relaxed);
    let q = walk_quote(&pool, 500_000, true).expect("must quote");
    assert!(q.amount_out > 0);
    // selling Y for X at price ~1.01^35 y-per-x: out_x ≈ in/1.01^35 spread over 2 bins
}

#[test]
fn walk_returns_none_when_window_exhausted_or_unseeded() {
    // Unseeded cache → None (fall back to haircut).
    let pool = sol_usdc_dlmm_pool();
    assert!(walk_quote(&pool, 1_000, true).is_none());
    // Window exhausted: huge input vs tiny liquidity, missing next array → None.
    let mut bins = [(0u64, 0u64); 70];
    bins[0] = (0, 10); // active bin 0 = slot 0; next array (-1) not cached
    let pool = walk_test_pool(bins);
    assert!(walk_quote(&pool, 1_000_000, true).is_none(),
        "must refuse to fabricate depth beyond the cached window");
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test --bin solana-mev walk_ -v` and `cargo test --bin solana-mev total_fee_rate -v`
Expected: FAIL — functions not defined.

- [ ] **Step 4: Implement**

In `src/dex/dlmm.rs`:

```rust
// ── DLMM fill walk (real per-bin liquidity + dynamic fee) ────────────────────
// Port of Meteora's commons quote_exact_in (MeteoraAg/dlmm-sdk), minus limit
// orders (skipping them under-fills — safe) and transfer fees (pools with a
// transfer-fee mint are pinned to the haircut path at startup, Task 6).

const FEE_PRECISION: u128 = 1_000_000_000;   // 1e9, lb_clmm fee scale
const MAX_FEE_RATE: u128 = 100_000_000;      // 10% cap, lb_clmm constant
const BASIS_POINT_MAX_U128: u128 = 10_000;

/// total swap fee rate (1e9 scale): base + variable, capped at 10%.
fn total_fee_rate(fee: &types::DlmmFeeParams, bin_step: u16, vol_acc: u32) -> u128 {
    let base = (fee.base_factor as u128)
        * (bin_step as u128)
        * 10u128
        * 10u128.pow(fee.base_fee_power_factor as u32);
    let variable = if fee.variable_fee_control > 0 {
        let sq = ((vol_acc as u128) * (bin_step as u128)).pow(2);
        ((fee.variable_fee_control as u128) * sq + 99_999_999_999) / 100_000_000_000
    } else {
        0
    };
    (base + variable).min(MAX_FEE_RATE)
}

/// lb_clmm update_references, simulated at quote time: returns the
/// (volatility_reference, index_reference) the program would use for a swap
/// happening `now_ts`.
fn simulated_vol_state(fee: &types::DlmmFeeParams, active_id: i32, now_ts: i64) -> (u32, i32) {
    let elapsed = now_ts.saturating_sub(fee.last_update_timestamp);
    if elapsed >= fee.filter_period as i64 {
        let vol_ref = if elapsed < fee.decay_period as i64 {
            ((fee.volatility_accumulator as u64 * fee.reduction_factor as u64)
                / BASIS_POINT_MAX_U128 as u64) as u32
        } else {
            0
        };
        (vol_ref, active_id)
    } else {
        (fee.volatility_reference, fee.index_reference)
    }
}

pub(crate) struct WalkFill {
    pub amount_out: u64,
    pub fee_amount: u64,
    /// last bin id touched by the fill — Task 9 derives bin-array coverage from it
    pub terminal_bin: i32,
}

/// Staircase fill over cached bins. `None` means "can't walk" (unseeded cache,
/// lock contention, window exhausted, overflow) — the caller falls back to the
/// haircut quote. Never blocks: try_read only.
pub(crate) fn walk_fill(
    pool: &Pool,
    amount_in: u64,
    swap_for_y: bool,
    now_unix_ts: i64,
) -> Option<WalkFill> {
    if amount_in == 0 {
        return None;
    }
    let bin_step = pool.extra.dlmm_bin_step?;
    let active_id = pool.active_bin_id.load(Ordering::Relaxed);
    let cache = pool.dlmm_bins.try_read().ok()?;
    if cache.stamped_ns == 0 || cache.fee.base_factor == 0 {
        return None;
    }
    let (vol_ref, index_ref) = simulated_vol_state(&cache.fee, active_id, now_unix_ts);

    let step = 1.0 + bin_step as f64 / 10_000.0;
    let mut price_f = step.powi(active_id); // y per x, raw units
    let mut bin_id = active_id;
    let mut remaining = amount_in as u128;
    let mut total_out: u128 = 0;
    let mut total_fee: u128 = 0;

    while remaining > 0 {
        let arr_idx = bin_id.div_euclid(MAX_BIN_PER_ARRAY) as i64;
        // Window exhausted → refuse to fabricate depth beyond what we can see.
        let bins = cache.arrays.get(&arr_idx)?;
        let slot = bin_id.rem_euclid(MAX_BIN_PER_ARRAY) as usize;
        let (amount_x, amount_y) = bins[slot];
        let cap_out = if swap_for_y { amount_y } else { amount_x } as u128;
        if cap_out > 0 {
            // volatility accumulator the program would reach at this bin
            let vol_acc = ((vol_ref as u128)
                + ((index_ref as i64 - bin_id as i64).unsigned_abs() as u128)
                    * BASIS_POINT_MAX_U128)
                .min(cache.fee.max_volatility_accumulator as u128) as u32;
            let rate = total_fee_rate(&cache.fee, bin_step, vol_acc);
            let price_q64 = (price_f * (2f64).powi(64)) as u128;
            if price_q64 == 0 {
                return None;
            }
            // input to drain the bin (round UP, like get_max_amount_in) …
            let cap_in_no_fee: u128 = if swap_for_y {
                (cap_out.checked_shl(64)?).div_ceil(price_q64)
            } else {
                (cap_out.checked_mul(price_q64)? + (1u128 << 64) - 1) >> 64
            };
            // … grossed up by the fee (compute_fee: rate/(1e9−rate), round UP)
            let max_fee = (cap_in_no_fee * rate).div_ceil(FEE_PRECISION - rate);
            let cap_in = cap_in_no_fee + max_fee;
            if remaining >= cap_in {
                remaining -= cap_in;
                total_out += cap_out;
                total_fee += max_fee;
            } else {
                // partial fill: fee on input (compute_fee_from_amount, round UP),
                // output rounds DOWN (get_amount_out)
                let fee = (remaining * rate).div_ceil(FEE_PRECISION);
                let in_after_fee = remaining - fee;
                let out = if swap_for_y {
                    (in_after_fee.checked_mul(price_q64)?) >> 64
                } else {
                    (in_after_fee.checked_shl(64)?) / price_q64
                };
                total_out += out.min(cap_out);
                total_fee += fee;
                remaining = 0;
            }
        }
        if remaining > 0 {
            if swap_for_y {
                bin_id = bin_id.checked_sub(1)?;
                price_f /= step;
            } else {
                bin_id = bin_id.checked_add(1)?;
                price_f *= step;
            }
        }
    }

    Some(WalkFill {
        amount_out: u64::try_from(total_out).ok()?,
        fee_amount: u64::try_from(total_fee).ok()?,
        terminal_bin: bin_id,
    })
}

/// Bin-walk quote in SwapQuote form. `None` → caller uses the haircut quote.
pub fn walk_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> Option<SwapQuote> {
    let orientation = pool.dlmm_token_a_is_x.load(Ordering::Relaxed);
    if orientation == 0 {
        return None;
    }
    let swap_for_y = (orientation == 1) == a_to_b;
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let fill = walk_fill(pool, amount_in, swap_for_y, now_ts)?;
    if fill.amount_out == 0 {
        return None;
    }
    // Impact isolates the pool-level price shift (fees excluded, matching
    // Pool::price_impact semantics): 1 − out / (in_after_fee × active price).
    let bin_step = pool.extra.dlmm_bin_step? as f64;
    let active_price = (1.0 + bin_step / 10_000.0).powi(pool.active_bin_id.load(Ordering::Relaxed));
    let in_after_fee = (amount_in - fill.fee_amount) as f64;
    let ideal_out = if swap_for_y { in_after_fee * active_price } else { in_after_fee / active_price };
    let price_impact = if ideal_out > 0.0 {
        (1.0 - fill.amount_out as f64 / ideal_out).max(0.0)
    } else {
        0.0
    };
    Some(SwapQuote {
        amount_in,
        amount_out: fill.amount_out,
        fee_amount: fill.fee_amount,
        price_impact,
        a_to_b,
    })
}
```

Adjust the test helper signature change (`sol_usdc_dlmm_pool` → parameterized bin_step) as described in Step 2's note. `u128::div_ceil` is stable (Rust ≥1.73); `checked_shl(64)` returns `Option` — the `?` propagates overflow to fallback.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --bin solana-mev dlmm -v` — expect ALL dlmm tests (new + existing) PASS.

- [ ] **Step 6: Commit**

```bash
git add src/dex/dlmm.rs
git commit -m "feat(dlmm): staircase fill walk over cached bins with dynamic fee"
```

---

### Task 6: quote mode config + `get_quote` dispatch + transfer-fee guard

**Files:**
- Modify: `src/config.rs` (Config struct ~line 20 + `from_env` ~line 180)
- Modify: `src/dex/dlmm.rs` (`get_quote` ~line 67)
- Modify: `src/dex/types.rs` (transfer-fee mint registry, next to `publish_mint_token_programs`)
- Modify: `src/main.rs` (mint-resolution block ~line 602: TLV scan; after config load: publish mode)
- Test: `src/dex/dlmm.rs`, `src/dex/types.rs`, `src/config.rs` `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `pub enum DlmmBinQuoteMode { Off, Shadow, Live }` (in `config.rs`, `Clone, Copy, PartialEq, Eq, Debug`), `Config.dlmm_bin_quote: DlmmBinQuoteMode` (env `DLMM_BIN_QUOTE`, default Shadow)
  - `dlmm::set_bin_quote_mode(mode: u8)` (0=off, 1=shadow, 2=live) + `dlmm::haircut_quote` (the renamed current `get_quote` body, `pub(crate)`)
  - `types::publish_transfer_fee_mints(HashSet<Pubkey>)`, `types::mint_has_transfer_fee(&Pubkey) -> bool`, `types::mint_data_has_transfer_fee(&[u8]) -> bool`
- Task 8 consumes `Config.dlmm_bin_quote` and `dlmm::haircut_quote`.

- [ ] **Step 1: Write the failing tests**

`src/dex/types.rs`:

```rust
#[test]
fn mint_data_transfer_fee_tlv_scan() {
    // Token-2022 mint: base 82 bytes, padded to 165, account_type=1 @165,
    // TLV entries from 166: [type u16][len u16][payload]. TransferFeeConfig = 1.
    let mut with_fee = vec![0u8; 200];
    with_fee[165] = 1;                                   // AccountType::Mint
    with_fee[166..168].copy_from_slice(&1u16.to_le_bytes());  // ext type 1
    with_fee[168..170].copy_from_slice(&8u16.to_le_bytes());  // len
    assert!(mint_data_has_transfer_fee(&with_fee));

    let mut other_ext = vec![0u8; 200];
    other_ext[165] = 1;
    other_ext[166..168].copy_from_slice(&3u16.to_le_bytes()); // some other ext
    other_ext[168..170].copy_from_slice(&4u16.to_le_bytes());
    assert!(!mint_data_has_transfer_fee(&other_ext));

    assert!(!mint_data_has_transfer_fee(&[0u8; 82]), "classic SPL mint");
}
```

`src/dex/dlmm.rs`:

```rust
#[test]
fn get_quote_live_mode_prefers_walk_and_falls_back() {
    let mut bins = [(0u64, 0u64); 70];
    bins[0] = (0, 1_000_000);
    let pool = walk_test_pool(bins);
    pool.sqrt_price_x64.store(1.0f64.to_bits(), Ordering::Relaxed);
    pool.fee_bps.store(100, Ordering::Relaxed);
    pool.reserve_a.store(u64::MAX / 4, Ordering::Relaxed); // haircut impact ≈ 0

    set_bin_quote_mode(2); // live
    let live = get_quote(&pool, 500_000, true);
    assert_eq!(live.amount_out, 495_000, "live mode must use the walk (1% real fee)");

    set_bin_quote_mode(0); // off
    let off = get_quote(&pool, 500_000, true);
    assert_eq!(off.amount_out, 495_000, "haircut: 1% fee_bps × ~zero impact");

    // live mode with unseeded cache falls back to haircut
    let bare = sol_usdc_dlmm_pool(1);
    bare.sqrt_price_x64.store(1.0f64.to_bits(), Ordering::Relaxed);
    set_bin_quote_mode(2);
    let fb = get_quote(&bare, 1_000, true);
    set_bin_quote_mode(0); // restore for other tests
    assert!(fb.amount_out > 0, "fallback haircut must still quote");
}
```

(Note: `walk_test_pool` uses fee params equal to the pool's static 1% so live and off agree in this construction — the assertion is that both paths produce a quote and live uses the walk; keep `set_bin_quote_mode(0)` restores so test order can't leak state.)

`src/config.rs`:

```rust
#[test]
fn dlmm_bin_quote_mode_parses() {
    assert_eq!(parse_dlmm_bin_quote_mode("off"), DlmmBinQuoteMode::Off);
    assert_eq!(parse_dlmm_bin_quote_mode("live"), DlmmBinQuoteMode::Live);
    assert_eq!(parse_dlmm_bin_quote_mode("shadow"), DlmmBinQuoteMode::Shadow);
    assert_eq!(parse_dlmm_bin_quote_mode("bogus"), DlmmBinQuoteMode::Shadow, "default");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev mint_data_transfer_fee -v && cargo test --bin solana-mev dlmm_bin_quote_mode -v`
Expected: FAIL — functions/types not defined.

- [ ] **Step 3: Implement**

`src/config.rs` — enum + free parse fn + field + from_env wiring:

```rust
/// DLMM bin-walk quote rollout mode (DLMM_BIN_QUOTE env).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DlmmBinQuoteMode { Off, Shadow, Live }

pub fn parse_dlmm_bin_quote_mode(s: &str) -> DlmmBinQuoteMode {
    match s.to_ascii_lowercase().as_str() {
        "off" => DlmmBinQuoteMode::Off,
        "live" => DlmmBinQuoteMode::Live,
        _ => DlmmBinQuoteMode::Shadow,
    }
}
```

Field on Config: `pub dlmm_bin_quote: DlmmBinQuoteMode,` and in `from_env`'s `Ok(Self { ... })`:

```rust
            dlmm_bin_quote: parse_dlmm_bin_quote_mode(
                &env::var("DLMM_BIN_QUOTE").unwrap_or_else(|_| "shadow".to_string()),
            ),
```

`src/dex/types.rs` — mirror the `publish_mint_token_programs` OnceLock pattern:

```rust
static TRANSFER_FEE_MINTS: OnceLock<HashSet<Pubkey>> = OnceLock::new();

pub fn publish_transfer_fee_mints(mints: HashSet<Pubkey>) {
    let _ = TRANSFER_FEE_MINTS.set(mints);
}

pub fn mint_has_transfer_fee(mint: &Pubkey) -> bool {
    TRANSFER_FEE_MINTS.get().map_or(false, |s| s.contains(mint))
}

/// Token-2022 mint TLV scan for the TransferFeeConfig extension (type 1).
/// Layout: base mint 82 bytes, zero-padded to 165, account_type u8 @165
/// (1 = Mint), then TLV entries [ext_type u16 LE][len u16 LE][payload].
pub fn mint_data_has_transfer_fee(data: &[u8]) -> bool {
    if data.len() <= 166 || data[165] != 1 {
        return false;
    }
    let mut off = 166;
    while off + 4 <= data.len() {
        let ext = u16::from_le_bytes([data[off], data[off + 1]]);
        let len = u16::from_le_bytes([data[off + 2], data[off + 3]]) as usize;
        if ext == 1 {
            return true;
        }
        off += 4 + len;
    }
    false
}
```

`src/dex/dlmm.rs` — rename the existing `get_quote` body to `pub(crate) fn haircut_quote(...)` (same signature) and add:

```rust
use std::sync::atomic::AtomicU8;

/// 0 = off, 1 = shadow, 2 = live — set once at startup from Config.dlmm_bin_quote.
/// A process-wide static (same pattern as sol_price / mint token programs)
/// because get_quote has no Config access on the hot path.
static BIN_QUOTE_MODE: AtomicU8 = AtomicU8::new(0);

pub fn set_bin_quote_mode(mode: u8) {
    BIN_QUOTE_MODE.store(mode, Ordering::Relaxed);
}

pub fn get_quote(pool: &types::Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    if BIN_QUOTE_MODE.load(Ordering::Relaxed) == 2
        && !types::mint_has_transfer_fee(&pool.token_a)
        && !types::mint_has_transfer_fee(&pool.token_b)
    {
        if let Some(q) = walk_quote(pool, amount_in, a_to_b) {
            return q;
        }
    }
    haircut_quote(pool, amount_in, a_to_b)
}
```

`src/main.rs` — in the mint-resolution block (~line 610), the fetched account data is already in hand; collect transfer-fee mints alongside `mint_programs`:

```rust
        let mut transfer_fee_mints = std::collections::HashSet::new();
        // (inside the existing `for (mint, acc) in chunk.iter().zip(accounts)` loop)
                        if let Some(a) = acc {
                            mint_programs.insert(*mint, a.owner);
                            if dex::types::mint_data_has_transfer_fee(&a.data) {
                                transfer_fee_mints.insert(*mint);
                            }
                        }
        // (after the loop, next to publish_mint_token_programs)
        for p in registry.all_pools().iter().filter(|p| p.dex == dex::types::DexKind::MeteoraDlmm) {
            if transfer_fee_mints.contains(&p.token_a) || transfer_fee_mints.contains(&p.token_b) {
                warn!("DLMM pool {} has a transfer-fee mint — bin walk pinned to haircut quote", p.id);
            }
        }
        dex::types::publish_transfer_fee_mints(transfer_fee_mints);
```

And right after `Config` is loaded (near the existing `Config loaded` log):

```rust
    dex::dlmm::set_bin_quote_mode(match config.dlmm_bin_quote {
        config::DlmmBinQuoteMode::Off => 0,
        config::DlmmBinQuoteMode::Shadow => 1,
        config::DlmmBinQuoteMode::Live => 2,
    });
```

Also update the 3 evaluator call sites? **No** — they already call `dlmm::get_quote`, which now dispatches internally. No evaluator change in this task.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin solana-mev dlmm -v && cargo test --bin solana-mev mint_data -v && cargo test --bin solana-mev dlmm_bin_quote_mode -v && cargo build --release 2>&1 | tail -3`
Expected: all PASS, clean build.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/dex/dlmm.rs src/dex/types.rs src/main.rs
git commit -m "feat(dlmm): DLMM_BIN_QUOTE mode dispatch + transfer-fee mint guard"
```

---

### Task 7: gRPC memcmp transport + startup seeding + backfill coverage

**Files:**
- Modify: `src/streamer/subscription.rs`
- Modify: `src/main.rs` (callback ~line 996-1020; subscription build ~line 1897; startup seeding after the CL-state prefetch ~line 600)
- Modify: `src/streamer/backfill.rs` (`accounts_for` ~line 59, `apply_polled_account` ~line 118)
- Modify: `src/dex/dlmm.rs` (extract `derive_bin_array_pda`)
- Test: `src/streamer/subscription.rs`, `src/streamer/backfill.rs`, `src/dex/dlmm.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `store_bin_array`, `decode_bin_array`, `BIN_ARRAY_LEN`, `BIN_ARRAY_DISCRIMINATOR` (Tasks 2-3).
- Produces:
  - `dlmm::derive_bin_array_pda(lb_pair: &Pubkey, index: i64) -> Pubkey` (pub; refactored out of `build_swap_instruction`)
  - `dlmm::seed_bin_array_keys(pool: &Pool) -> Vec<Pubkey>` (active-array −1, 0, +1 PDAs)
  - `subscription::build_account_subscription(accounts: &[Pubkey], dlmm_lb_pairs: &[Pubkey])` — signature gains the second param (`&[]` = no bin filters)

- [ ] **Step 1: Write the failing tests**

`src/streamer/subscription.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn bin_filters_added_per_dlmm_pool() {
        let acc = Pubkey::new_unique();
        let lb_pair = Pubkey::from_str("HTvjzsfX3yU6BUodCjZ5vZkUrAxMDTrBs3CJaq43ashR").unwrap();
        let req = build_account_subscription(&[acc], &[lb_pair]);
        assert!(req.accounts.contains_key("pools"));
        let key = format!("bins:{lb_pair}");
        let f = req.accounts.get(&key).expect("bin filter present");
        assert_eq!(f.owner, vec!["LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo".to_string()]);
        assert_eq!(f.filters.len(), 1);
        // no-DLMM call sites keep the old shape
        let req2 = build_account_subscription(&[acc], &[]);
        assert_eq!(req2.accounts.len(), 1);
    }
}
```

`src/dex/dlmm.rs`:

```rust
#[test]
fn seed_bin_array_keys_covers_active_plus_minus_one() {
    let pool = sol_usdc_dlmm_pool(1);
    pool.active_bin_id.store(-71, Ordering::Relaxed); // array -2 (div_euclid)
    let keys = seed_bin_array_keys(&pool);
    assert_eq!(keys.len(), 3);
    assert_eq!(keys[0], derive_bin_array_pda(&pool.id, -3));
    assert_eq!(keys[1], derive_bin_array_pda(&pool.id, -2));
    assert_eq!(keys[2], derive_bin_array_pda(&pool.id, -1));
}
```

`src/streamer/backfill.rs` (extend the existing test module, which already builds pools):

```rust
#[test]
fn apply_polled_bin_array_stores_into_cache() {
    // build a MeteoraDlmm pool the way this module's existing tests do,
    // with state_account = Some(id) and active_bin_id = 0
    let pool = dlmm_test_pool(); // adapt to this module's existing helper naming
    let data = crate::dex::dlmm::synth_bin_array_pub(&pool.id, 0, (9, 9)); // see note
    let graph = test_graph();   // module's existing graph helper (or construct ExchangeGraph::new)
    let key = crate::dex::dlmm::derive_bin_array_pda(&pool.id, 0);
    assert!(apply_polled_account(&pool, &key, &data, &graph));
    assert_eq!(pool.dlmm_bins.read().unwrap().arrays[&0][0], (9, 9));
}
```

**Note:** promote the Task 3 test helper `synth_bin_array` to `#[cfg(test)] pub(crate) fn synth_bin_array_pub` in `dlmm.rs` (or move it out of the test module behind `#[cfg(test)]`) so backfill tests can reuse it. Inspect `backfill.rs`'s existing `#[cfg(test)]` helpers (~line 280) and reuse its pool/graph construction idioms rather than inventing new ones.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev bin_filters_added -v` — FAIL (signature has one param).
Run: `cargo test --bin solana-mev seed_bin_array_keys -v` — FAIL (fn not defined).

- [ ] **Step 3: Implement**

`src/dex/dlmm.rs` — extract the PDA derivation (replace the two inline `derive_pda(&[b"bin_array", ...])` calls in `build_swap_instruction` with calls to this):

```rust
/// bin_array PDA: ["bin_array", lb_pair, index_i64_le]
pub fn derive_bin_array_pda(lb_pair: &Pubkey, index: i64) -> Pubkey {
    derive_pda(&[b"bin_array", lb_pair.as_ref(), &index.to_le_bytes()], &METEORA_DLMM_PUBKEY)
}

/// The 3 bin-array PDAs (active−1, active, active+1) used for startup seeding
/// and backfill polling.
pub fn seed_bin_array_keys(pool: &Pool) -> Vec<Pubkey> {
    let arr = pool.active_bin_id.load(Ordering::Relaxed).div_euclid(MAX_BIN_PER_ARRAY) as i64;
    (arr - 1..=arr + 1).map(|i| derive_bin_array_pda(&pool.id, i)).collect()
}
```

`src/streamer/subscription.rs`:

```rust
use yellowstone_grpc_proto::geyser::{
    subscribe_request_filter_accounts_filter::Filter as AccFilter,
    subscribe_request_filter_accounts_filter_memcmp::Data as MemcmpData,
    SubscribeRequestFilterAccountsFilter, SubscribeRequestFilterAccountsFilterMemcmp,
};

const METEORA_DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

pub fn build_account_subscription(accounts: &[Pubkey], dlmm_lb_pairs: &[Pubkey]) -> SubscribeRequest {
    build_subscription(accounts, accounts, dlmm_lb_pairs)
}

pub fn build_subscription(
    accounts: &[Pubkey],
    watch_vaults: &[Pubkey],
    dlmm_lb_pairs: &[Pubkey],
) -> SubscribeRequest {
    // ... existing body; after inserting the "pools" filter add:

    // One owner+memcmp filter per DLMM pool: every BinArray account carries its
    // lb_pair at offset 24, so this streams ALL bin arrays of the pool — the
    // active bin migrating across array boundaries needs no resubscribe.
    for lb_pair in dlmm_lb_pairs {
        account_filters.insert(
            format!("bins:{lb_pair}"),
            SubscribeRequestFilterAccounts {
                account: vec![],
                owner: vec![METEORA_DLMM_PROGRAM.to_string()],
                filters: vec![SubscribeRequestFilterAccountsFilter {
                    filter: Some(AccFilter::Memcmp(SubscribeRequestFilterAccountsFilterMemcmp {
                        offset: 24,
                        data: Some(MemcmpData::Bytes(lb_pair.to_bytes().to_vec())),
                    })),
                }],
                ..Default::default()
            },
        );
    }
```

(If the proto field/enum names differ in the pinned yellowstone version, check `cargo doc` or the crate source in `~/.cargo` — the memcmp filter is a standard Yellowstone feature; adjust names, not structure. If `build_subscription` has other callers — grep first — add the parameter there too, passing `&[]`.)

`src/main.rs`:

1. Subscription call site (~line 1897):

```rust
    let dlmm_lb_pairs: Vec<Pubkey> = registry.all_pools().iter()
        .filter(|p| p.dex == dex::types::DexKind::MeteoraDlmm)
        .map(|p| p.id)
        .collect();
    let initial_subscription = build_account_subscription(&account_keys, &dlmm_lb_pairs);
```

2. Callback: in the dispatch chain (after the `get_by_state_account` branch, before the final else), insert:

```rust
        } else if data.len() == dex::dlmm::BIN_ARRAY_LEN
            && data[0..8] == dex::dlmm::BIN_ARRAY_DISCRIMINATOR
        {
            // BinArray from the per-pool memcmp filter — not in any index.
            // Store bins; return false: bin contents don't move the marginal
            // rate, and the same tx's lb_pair/vault updates already poke BF.
            Pubkey::try_from(&data[24..56]).ok()
                .and_then(|lb| registry_cb.get_by_pool_id(&lb))
                .map(|pool| { dex::dlmm::store_bin_array(&pool, &data); })
                .is_some() && false
        } else {
```

(Match the surrounding style — an explicit `{ ...; false }` block is clearer than `&& false`; write it as a block.)

3. Startup seeding, immediately after the CL-state prefetch block (~line 600):

```rust
    // ── Seed DLMM bin arrays (active ±1 per pool) so the fill walk works
    // before the first gRPC bin update ────────────────────────────────────
    {
        let dlmm_pools: Vec<Arc<dex::types::Pool>> = registry.all_pools().iter()
            .filter(|p| p.dex == dex::types::DexKind::MeteoraDlmm)
            .map(Arc::clone)
            .collect();
        if !dlmm_pools.is_empty() {
            let mut keys = Vec::new();
            let mut owners = Vec::new();
            for p in &dlmm_pools {
                for k in dex::dlmm::seed_bin_array_keys(p) {
                    keys.push(k);
                    owners.push(Arc::clone(p));
                }
            }
            let mut seeded = 0usize;
            for (chunk_keys, chunk_owners) in keys.chunks(100).zip(owners.chunks(100)) {
                match rpc.get_multiple_accounts(chunk_keys).await {
                    Ok(accounts) => {
                        for (pool, acc) in chunk_owners.iter().zip(accounts) {
                            if let Some(a) = acc {
                                if dex::dlmm::store_bin_array(pool, &a.data) {
                                    seeded += 1;
                                }
                            }
                        }
                    }
                    Err(e) => warn!("DLMM bin-array seed fetch failed: {e}"),
                }
            }
            info!("Seeded {} DLMM bin arrays across {} pools", seeded, dlmm_pools.len());
        }
    }
```

(This must run AFTER the CL prefetch so `active_bin_id` is populated. An empty-account result — array not initialized on-chain — is normal, not an error.)

`src/streamer/backfill.rs`:

1. `accounts_for`: DLMM pools currently fall into the generic `state_account` arm. Add an explicit arm ABOVE the generic `_` match:

```rust
        // DLMM prices off lb_pair state AND fills off bin arrays: poll both so
        // a stream gap can't leave the fill walk on frozen bins.
        DexKind::MeteoraDlmm => pool.state_account.map(|s| {
            let mut v = vec![s];
            v.extend(crate::dex::dlmm::seed_bin_array_keys(pool));
            v
        }),
```

2. `apply_polled_account`: before the final `else { false }` arm, add:

```rust
    } else if data.len() == crate::dex::dlmm::BIN_ARRAY_LEN
        && data[0..8] == crate::dex::dlmm::BIN_ARRAY_DISCRIMINATOR
    {
        crate::dex::dlmm::store_bin_array(pool, data)
```

(Returning true stamps the pool + refreshes the graph edge from unchanged price atomics — a no-op semantically, and it correctly marks the pool fresh.)

- [ ] **Step 4: Run tests + build**

Run: `cargo test --bin solana-mev bin_filters_added -v && cargo test --bin solana-mev seed_bin_array_keys -v && cargo test --bin solana-mev apply_polled_bin_array -v && cargo test --bin solana-mev 2>&1 | tail -3 && cargo build --release 2>&1 | tail -3`
Expected: all PASS, clean build.

- [ ] **Step 5: Live smoke test (DRY_RUN)**

Run: `DRY_RUN=true timeout 30 cargo run --release --bin solana-mev 2>&1 | grep -E "Seeded|bins:|gRPC stream|error" | head -20`
Expected: `Seeded N DLMM bin arrays across 11 pools` with N ≥ 20, gRPC stream starts without filter-rejection errors. If the provider rejects memcmp filters, STOP and report (spec names static-PDA transport as the fallback — do not improvise it without asking).

- [ ] **Step 6: Commit**

```bash
git add src/streamer/subscription.rs src/streamer/backfill.rs src/main.rs src/dex/dlmm.rs
git commit -m "feat(dlmm): stream bin arrays via per-pool memcmp filters + seed & backfill"
```

---

### Task 8: shadow-mode divergence logging in the evaluator

**Files:**
- Modify: `src/arbitrage/evaluator.rs` (dedupe the 3 quote dispatch matches ~lines 80-92/123-135/165-177; near-miss block ~line 1580; post-search success path ~line 1596)
- Test: `src/arbitrage/evaluator.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `dlmm::walk_quote`, `dlmm::haircut_quote` (Tasks 5-6), `Config.dlmm_bin_quote` (Task 6).
- Produces: `fn quote_hop(pool: &Arc<Pool>, amount: u64, a_to_b: bool) -> SwapQuote` (the deduped dispatch), `fn dlmm_shadow_report(cycle: &ArbCycle, pools: &[Arc<Pool>], amount_in: u64) -> Option<String>`.

- [ ] **Step 1: Dedupe the quote dispatch**

Extract the identical 11-arm `match pool.dex { ... get_quote ... }` from `diagnose_quote_failure`, `probe_gross_ratio`, and `evaluate_quotes` into:

```rust
/// Single quote dispatch — keep the PumpSwap CP-math note from the originals.
fn quote_hop(pool: &Pool, amount: u64, a_to_b: bool) -> SwapQuote {
    match pool.dex {
        DexKind::RaydiumAmmV4  => raydium_amm::get_quote(pool, amount, a_to_b),
        DexKind::RaydiumClmm   => raydium_clmm::get_quote(pool, amount, a_to_b),
        DexKind::OrcaWhirlpool => orca::get_quote(pool, amount, a_to_b),
        DexKind::MeteoraDamm   => meteora::get_quote(pool, amount, a_to_b),
        DexKind::MeteoraDlmm   => dlmm::get_quote(pool, amount, a_to_b),
        DexKind::Phoenix       => phoenix::get_quote(pool, amount, a_to_b),
        DexKind::Lifinity      => lifinity::get_quote(pool, amount, a_to_b),
        DexKind::Invariant     => invariant::get_quote(pool, amount, a_to_b),
        DexKind::Saber         => saber::get_quote(pool, amount, a_to_b),
        DexKind::Jupiter       => jupiter::get_quote(pool, amount, a_to_b),
        DexKind::PumpSwap      => raydium_amm::get_quote(pool, amount, a_to_b), // same CP math; pricing-only, never in the registry
    }
}
```

Replace all 3 match blocks with `quote_hop(pool, current, edge.a_to_b)` (keep each function's surrounding gating logic byte-identical). Run `cargo test --bin solana-mev evaluator` — must still pass before proceeding.

- [ ] **Step 2: Write the failing test for the shadow report**

The evaluator test module (~line 780) already constructs pools and cycles — reuse its helpers. Test the pure helper, not the logging:

```rust
#[test]
fn dlmm_shadow_report_covers_dlmm_hops_only() {
    // Build a 2-hop cycle where hop0 is a DLMM pool with a seeded bin cache
    // (reuse walk-style setup: bin_step=100, active_id=0, bins[0]=(0,1_000_000),
    // fee base_factor=10_000, stamped) and hop1 is a Raydium AMM pool.
    // Assert: report is Some, mentions "hop0", does NOT mention "hop1",
    // and contains "walk=" and "haircut=".
    // Assert: a cycle with no DLMM hop returns None.
}
```

(Write the real construction against the module's existing helpers — `bellman_ford::ArbCycle` construction exists in evaluator tests; follow the closest existing test's idiom.)

- [ ] **Step 3: Implement the report + call sites**

```rust
/// Shadow-mode diagnostic: for each DLMM hop, compare the bin walk against the
/// haircut quote at this cycle's actual hop input. Chains hop inputs with the
/// live get_quote (whatever mode is active) exactly like evaluate_quotes.
/// Returns None when the cycle has no DLMM hop. Not called on the hot path —
/// only from the (rate-limited) near-miss block and the per-submission path.
fn dlmm_shadow_report(cycle: &ArbCycle, pools: &[Arc<Pool>], amount_in: u64) -> Option<String> {
    let mut current = amount_in;
    let mut parts: Vec<String> = Vec::new();
    for (i, (edge, pool)) in cycle.edges.iter().zip(pools.iter()).enumerate() {
        if pool.dex == DexKind::MeteoraDlmm {
            let haircut = dlmm::haircut_quote(pool, current, edge.a_to_b);
            let part = match dlmm::walk_quote(pool, current, edge.a_to_b) {
                Some(w) => {
                    let delta_bps =
                        (w.amount_out as f64 / haircut.amount_out.max(1) as f64 - 1.0) * 10_000.0;
                    format!(
                        "hop{i} {}: in={current} walk={} haircut={} Δ={delta_bps:+.1}bps walk_fee={}",
                        &pool.id.to_string()[..8], w.amount_out, haircut.amount_out, w.fee_amount,
                    )
                }
                None => format!("hop{i} {}: in={current} walk=unavailable", &pool.id.to_string()[..8]),
            };
            parts.push(part);
        }
        let q = quote_hop(pool, current, edge.a_to_b);
        if q.amount_out == 0 { break; }
        current = q.amount_out;
    }
    (!parts.is_empty()).then(|| parts.join(" | "))
}
```

Call site 1 — near-miss diagnosed branch (directly after the `info!("near-miss [...] reason={reason}")` at ~line 1583):

```rust
                        if config.dlmm_bin_quote == crate::config::DlmmBinQuoteMode::Shadow {
                            if let Some(s) = dlmm_shadow_report(cycle, &pools, probe) {
                                info!("dlmm-shadow [{path}] {s}");
                            }
                        }
```

Call site 2 — right after `let (best_amount_in, mut best_quote) = ...` resolves to Some (~line 1596), before the routing-correction logic:

```rust
    if config.dlmm_bin_quote == crate::config::DlmmBinQuoteMode::Shadow {
        if let Some(s) = dlmm_shadow_report(cycle, &pools, best_amount_in) {
            let path: String = cycle.path.iter()
                .map(crate::dex::types::mint_symbol)
                .collect::<Vec<_>>().join("→");
            info!("dlmm-shadow [{path}] in={best_amount_in} {s}");
        }
    }
```

(Frequency: this fires once per cycle that survives ternary search — the same cadence as the `Cycle:` submission log, i.e. rare. The near-miss site is already rate-limited per path.)

- [ ] **Step 4: Run tests**

Run: `cargo test --bin solana-mev evaluator -v 2>&1 | tail -5` — all PASS including the new shadow-report test.

- [ ] **Step 5: Commit**

```bash
git add src/arbitrage/evaluator.rs
git commit -m "feat(dlmm): shadow-mode walk-vs-haircut divergence logging + quote_hop dedupe"
```

---

### Task 9: swap-builder bin-array direction fix + walk-derived coverage

**Files:**
- Modify: `src/dex/dlmm.rs` (`build_swap_instruction` ~lines 164-212)
- Test: `src/dex/dlmm.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `walk_fill` (Task 5), `derive_bin_array_pda` (Task 7).
- Produces: `fn bin_array_indexes_for_swap(pool: &Pool, amount_in: u64, swap_for_y: bool) -> Vec<i64>` (1-3 indexes, walk-derived with directional fallback).

**Background — the latent direction bug:** the current code sets `adj_idx = cur + 1` when `swap_for_y`. Meteora's reference (`get_bin_array_pubkeys_for_swap`) uses `increment = -1` for `swap_for_y`: selling X moves the active bin DOWN, so the neighbor that matters is `cur − 1`. The current builder passes the wrong-direction neighbor; any fill crossing an array boundary reverts with an account-not-found. This task fixes the direction AND sizes coverage from the walk.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn bin_array_indexes_direction_follows_meteora_reference() {
    // No cache → fallback [cur, cur-1] for swap_for_y (was cur+1: the bug).
    let pool = sol_usdc_dlmm_pool(1);
    pool.active_bin_id.store(0, Ordering::Relaxed); // array 0
    assert_eq!(bin_array_indexes_for_swap(&pool, 1_000, true), vec![0, -1]);
    assert_eq!(bin_array_indexes_for_swap(&pool, 1_000, false), vec![0, 1]);
}

#[test]
fn bin_array_indexes_walk_extends_to_three_arrays() {
    // active bin 0 (slot 0 of array 0): fill drains bin 0 and continues into
    // array -1 and array -2 → coverage [0, -1, -2].
    let mut bins0 = [(0u64, 0u64); 70];
    bins0[0] = (0, 100);
    let pool = walk_test_pool(bins0);
    let mut bins_m1 = [(0u64, 0u64); 70];
    for b in bins_m1.iter_mut() { *b = (0, 100); }
    let mut bins_m2 = [(0u64, 0u64); 70];
    for b in bins_m2.iter_mut() { *b = (0, 1_000_000_000); }
    {
        let mut c = pool.dlmm_bins.write().unwrap();
        c.arrays.insert(-1, bins_m1);
        c.arrays.insert(-2, bins_m2);
    }
    let idx = bin_array_indexes_for_swap(&pool, 50_000, true);
    assert_eq!(idx, vec![0, -1, -2]);
}

#[test]
fn build_swap_instruction_passes_walk_derived_arrays() {
    // Same setup as above; assert accounts [16..] equal the PDAs of [0,-1,-2].
    // (Extend the existing build_swap_instruction test module style.)
}
```

Also UPDATE the existing builder tests (they assert `bin_array_1` derived with the old inverted direction — flip the expected neighbor: `a_to_b`/`swap_for_y=true` at `ACTIVE_ID=0` now expects array index **-1**, not 1).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --bin solana-mev bin_array_indexes -v` — FAIL (fn not defined).

- [ ] **Step 3: Implement**

```rust
/// Bin-array indexes the swap will touch, most-likely-first, capped at 3
/// (the wire-size probe re-runs after instruction build, so a 3rd array is
/// size-checked like everything else). Directional fallback matches Meteora's
/// get_bin_array_pubkeys_for_swap: swap_for_y walks DOWN (−1), else UP (+1).
fn bin_array_indexes_for_swap(pool: &Pool, amount_in: u64, swap_for_y: bool) -> Vec<i64> {
    let cur = pool.active_bin_id.load(Ordering::Relaxed).div_euclid(MAX_BIN_PER_ARRAY) as i64;
    let step: i64 = if swap_for_y { -1 } else { 1 };
    let now_ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let terminal = walk_fill(pool, amount_in, swap_for_y, now_ts)
        .map(|f| f.terminal_bin.div_euclid(MAX_BIN_PER_ARRAY) as i64);
    match terminal {
        // walk unavailable or fill stays in the current array: current + directional neighbour
        None => vec![cur, cur + step],
        Some(t) if t == cur => vec![cur, cur + step],
        Some(t) => {
            let mut v = vec![cur];
            let mut i = cur;
            while i != t && v.len() < 3 {
                i += step;
                v.push(i);
            }
            v
        }
    }
}
```

In `build_swap_instruction`, replace the `cur_idx`/`adj_idx`/`bin_array_0`/`bin_array_1` block (~lines 164-176) and the two trailing AccountMetas with:

```rust
    let bin_array_metas: Vec<AccountMeta> = bin_array_indexes_for_swap(pool, amount_in, swap_for_y)
        .into_iter()
        .map(|idx| AccountMeta::new(derive_bin_array_pda(&lb_pair, idx), false))
        .collect();
    // ... accounts vec unchanged through index [15], then:
    accounts.extend(bin_array_metas);
```

- [ ] **Step 4: Run tests**

Run: `cargo test --bin solana-mev dlmm -v 2>&1 | tail -5` — all PASS (including the updated legacy builder tests).

- [ ] **Step 5: Commit**

```bash
git add src/dex/dlmm.rs
git commit -m "fix(dlmm): bin-array neighbour direction (Meteora reference: swap_for_y walks DOWN) + walk-derived 3-array coverage"
```

---

### Task 10: docs, env template, full verification

**Files:**
- Modify: `CLAUDE.md` (DEX-specific notes → Meteora DLMM entry; env-var table if a routing-relevant var), `.env.example`
- No code changes.

- [ ] **Step 1: Update `.env.example`**

Add near the other arb tuning vars:

```bash
# DLMM bin-walk quote: off | shadow (log walk-vs-haircut divergence, default) | live
DLMM_BIN_QUOTE=shadow
```

- [ ] **Step 2: Update CLAUDE.md**

In the **Meteora DLMM** entry under "DEX-specific notes", append:

```markdown
Since 2026-07-27 DLMM quotes can use a **real bin fill walk** (`DLMM_BIN_QUOTE=off|shadow|live`,
default `shadow`): per-pool gRPC owner+memcmp filters (offset 24 = lb_pair) stream every
BinArray (10,136 B; amounts @+0/+8 of each 144-B bin) into `Pool.dlmm_bins`
(`RwLock<DlmmBinCache>`, active-array ±2 window, try_read + haircut fallback — never blocks);
`parse_state` also decodes the dynamic fee params (StaticParameters @8..40,
VariableParameters @40..72) so the walk charges the real base+variable fee.
`shadow` logs `dlmm-shadow` walk-vs-haircut divergence lines from the evaluator; flip to
`live` after a clean session. Pools with a transfer-fee (Token-2022) mint are pinned to the
haircut quote. The swap builder derives bin-array coverage from the walk (up to 3 arrays)
— and note the neighbour direction: `swap_for_y` walks bin ids DOWN (array −1), per
Meteora's reference implementation.
```

- [ ] **Step 3: Full verification**

Run: `cargo test --bin solana-mev 2>&1 | tail -5` — all tests pass.
Run: `cargo clippy 2>&1 | grep -E "^error|warning: unused" | head` — no new errors.
Run: `DRY_RUN=true timeout 45 cargo run --release --bin solana-mev 2>&1 | grep -E "Seeded|dlmm-shadow|ERROR" | head -20` — bin seeding works; if any DLMM cycle evaluates during the window, a `dlmm-shadow` line appears.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md .env.example
git commit -m "docs(dlmm): bin-walk quote rollout notes + DLMM_BIN_QUOTE env"
```
