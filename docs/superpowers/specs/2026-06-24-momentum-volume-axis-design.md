# Record per-token volume + liquidity in price snapshots

**Date:** 2026-06-24
**Status:** design approved → implementation pending
**Scope:** data capture only (Phase 1 of a "most-traded / trending" momentum axis)

## Context

The momentum trader ranks watched tokens by a price-derived metric (`slope_r2`, etc.). The
operator proposed a new selection axis: **favor the most-traded (high 24h volume) or
trending (rising volume) tokens** — liquidity/activity as an edge dimension (better fills,
less noisy signals; thin/recent tokens behaved badly all session).

**Blocker:** the backtest history (`assets/price_history.jsonl`) records **price only** —
`PriceSnapshot { ts, prices }`. A volume/trending axis therefore **cannot be validated** on
existing data. But the live pricer (`pricer.rs`) **already fetches** 24h volume + liquidity
per token (it ranks DexScreener pools by `(24h volume, liquidity)` to choose the price
source) — it just **discards** them after picking the price.

**Decision (operator):** the disciplined *record-now, backtest-later* path. Start capturing
volume + liquidity per snapshot so the axis becomes backtestable in a few weeks. **This
sub-project is data capture only — the momentum trader's behavior is unchanged.**

## Goal

Capture **24h volume** and **pool liquidity** per token in every price snapshot, plumbed
from the pricer's existing pool ranking, backward-compatible with existing history.

## Scope

**In:** surface volume+liquidity from the pricer; add them to `PriceSnapshot`; write them
each tick; back-compat for old history lines; tests.

**Out (future sub-project, after ~2–4 weeks of recorded data):** the most-traded/trending
filter-or-tilt in the ranking, and its walk-forward backtest. A new spec covers that once
volume history exists.

## Design

### `pricer.rs`

- `select_base_pair_price(pairs, mint) -> Option<f64>` → **`-> Option<(f64, f64, f64)>`**
  = `(price, volume_24h, liquidity)`. The fn already computes the chosen pool's
  `(price, volume_24h, liquidity)` tuple internally (the `best` candidate) — return all three.
- `best_base_pair_price(...) -> Result<Option<f64>>` → **`-> Result<Option<(f64, f64, f64)>>`**.
- New **`pub async fn fetch_prices_and_volumes(client, mints, api_key)
  -> Result<(HashMap<String,f64>, HashMap<String,f64>, HashMap<String,f64>)>`**
  = `(prices, volumes, liquidity)`; the per-mint loop fills all three maps. A mint with no
  base pool contributes to none of them.
- **`fetch_prices(...)` is retained** for existing callers (history backfill, etc.) as a
  thin wrapper returning the prices map only — so those call sites don't change.
- SOL price (Kraken path) carries no DexScreener volume/liquidity → SOL is simply **absent**
  from the volumes/liquidity maps (best-effort; consumers treat missing as unknown).

### `history.rs` — `PriceSnapshot`

```rust
pub struct PriceSnapshot {
    pub ts: u64,
    pub prices: HashMap<String, f64>,
    #[serde(default)] pub volumes: HashMap<String, f64>,   // 24h USD volume per token
    #[serde(default)] pub liquidity: HashMap<String, f64>, // pool USD liquidity per token
}
```

`#[serde(default)]` keeps existing JSONL lines (which lack the fields) **loadable** → empty
maps. New snapshots serialize the fields. `append_snapshot` / `load` need no change (serde
handles it).

### `watcher.rs` (monitoring tick)

The tick calls `fetch_prices_and_volumes` instead of `fetch_prices`, and builds
`PriceSnapshot { ts, prices, volumes, liquidity }` before `append_snapshot`. `last_prices`
continues to derive from `prices` only — no other tick logic changes.

### Other `PriceSnapshot` literals

The pricer history-backfill fns (CoinGecko/Birdeye) and the sim test helpers construct
`PriceSnapshot`; add `volumes: HashMap::new(), liquidity: HashMap::new()` (those sources
provide no per-snapshot volume, so empty is correct).

## Data flow

DexScreener pool list → `select_base_pair_price` picks the top pool by `(24h vol, liq)` and
returns `(price, vol, liq)` → `fetch_prices_and_volumes` aggregates per mint → the watcher
writes them into the snapshot → `price_history.jsonl` accumulates volume + liquidity
alongside price.

## Backward compatibility

- **Reading:** old lines (no `volumes`/`liquidity`) → serde default empty maps. No migration.
- **Writing:** new lines include the maps; the file grows modestly.
- **Consumers** (momentum sim, analyzer) ignore the new fields → **no behavior change**.

## Testing

- **pricer:** `select_base_pair_price` returns the top pool's `(price, volume, liquidity)`
  for sample DexScreener JSON (extend the existing selection test).
- **history:** `PriceSnapshot` serde round-trips with the new maps; **and** an old-format
  line `{"ts":..,"prices":{..}}` deserializes with empty `volumes`/`liquidity` (back-compat).

## Files

- `src/portfolio/pricer.rs` — return tuples + `fetch_prices_and_volumes` + `fetch_prices` wrapper + test.
- `src/portfolio/history.rs` — `PriceSnapshot` fields + serde back-compat test.
- `src/portfolio/watcher.rs` — tick builds the snapshot with volume+liquidity.
- `src/portfolio/sim.rs` (test `PriceSnapshot` literals) + `src/portfolio/pricer.rs` history fns (literals).

## Future (separate spec, gated on accumulated data)

Design + backtest the most-traded/trending axis: a volume floor (filter) and/or a rank tilt
that blends a volume-z (level for "most-traded", rate-of-change for "trending") into the
momentum score. Validate walk-forward; wire live only if robust — same bar as every other knob.
