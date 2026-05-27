# Design: Bollinger Band Reversion (Trading Signal Engine #6)

**Date:** 2026-05-27
**Status:** Approved design — pending spec review
**Scope:** Add a sixth signal engine to the portfolio watcher that reacts when an
asset's live price pierces its 30-day Bollinger band.

## Goal

The portfolio watcher (`src/portfolio/suggestions.rs`) generates trading insights
from five independent signal engines. None of them act on actual Bollinger band
*position* — engine #5 ("Volatility Squeeze") only measures band *width*
contracting. This adds a sixth engine, **Bollinger Band Reversion**, that flags
assets whose current price has moved outside their 30-day ±2σ envelope, on a
mean-reversion basis.

## Decisions (from brainstorming)

| Decision | Choice | Rationale |
|---|---|---|
| Integration | New standalone engine #6 | Matches the existing one-engine-per-signal pattern and the docs structure. |
| Semantics | Mean-reversion | Price ≥ upper band → SELL candidate; price ≤ lower band → BUY candidate. Consistent with the suite's RSI/pairs bias. |
| Window / data source | Reuse 30-day daily bands | Mirrors the CLI chart's bands (30d, 2σ). σ comes from the same daily series the SMA is already computed from. |
| No-pair behavior | SWAP when paired, else standalone WATCH | When a sell and buy candidate co-exist → `SWAP sell FOR buy`. A lone candidate emits a standalone `WATCH` (like the squeeze engine). |

## Signal computation

```
For each portfolio asset (SOL + tokens):
    bands = DailyBands { sma, sigma, n }      // 30-day daily series stats
    skip if n < BOLLINGER_MIN_DAYS (14)       // σ not meaningful on too few points
    upper = sma + K · sigma                    // K = BOLLINGER_K = 2.0
    lower = sma − K · sigma
    skip if (upper − lower) < ε                // flat series (e.g. USDY stablecoin)

    price = latest minute tick from history deque
    pctB  = (price − lower) / (upper − lower)  // 0 = lower band, 1 = upper band

    price ≥ upper  (pctB ≥ 1) → SELL candidate
    price ≤ lower  (pctB ≤ 0) → BUY candidate
```

**Emission:**
- If both sell and buy candidates exist → emit `SWAP {sell} FOR {buy}` for each
  (sell, buy) combination (same cross-product pairing as `generate_rsi_suggestions`).
- If only sell candidates exist (no buy partners) → emit standalone
  `WATCH {sym} — above upper band` for each.
- If only buy candidates exist → emit standalone `WATCH {sym} — below lower band`
  for each.

This guarantees a lone band-pierce is never silently dropped, while avoiding
double-signalling when a natural swap pair is available.

## Data plumbing (Approach A — stats struct)

Both SMA producers already build a daily price vector and collapse it to a mean,
discarding σ. Promote the return type to carry the full stats:

```rust
// src/portfolio/pricer.rs
pub struct DailyBands {
    pub sma:   f64,   // mean of the daily series
    pub sigma: f64,   // sample standard deviation (Bessel, /(n-1)) of the daily series
    pub n:     usize, // number of daily data points
}
```

- `fetch_monthly_sma(...) -> HashMap<String, DailyBands>` — compute σ from the
  same `prices` vector it already builds from Birdeye 1D candles.
- `compute_sma_from_history(...) -> HashMap<String, DailyBands>` — compute σ from
  the same per-UTC-day `values` vector.
- Both keep the dual-key (mint + symbol) map shape.

**Why Approach A over a parallel `monthly_std` map:** we now consume more than the
mean, so a single struct keeps σ and the mean provably derived from the same
series. A parallel map would risk silent desync if one producer path is later
edited.

### Consumers updated (mechanical `.copied()` → `.map(|b| b.sma)`)

| File | Function | Change |
|---|---|---|
| `src/portfolio/watcher.rs` | run loop | `monthly_sma` becomes `HashMap<String, DailyBands>`; pass to engine #6. |
| `src/portfolio/suggestions.rs` | `generate_rsi_suggestions` | read `.sma` for the SMA-confirmation filter. |
| `src/portfolio/suggestions.rs` | `generate_vol_squeeze_suggestions` | read `.sma` for the direction bias. |
| `src/portfolio/analyzer.rs` | `generate_swap_suggestions` | read `.sma`. |

The signature of these existing functions stays `&HashMap<String, DailyBands>`
(type of the value changes; the lookup-by-symbol contract is unchanged).

## New engine

```rust
// src/portfolio/suggestions.rs

const BOLLINGER_K: f64 = 2.0;          // matches the CLI chart's ±2σ envelope
const BOLLINGER_MIN_DAYS: usize = 14;  // minimum daily points for a meaningful σ

pub fn generate_bollinger_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    bands: &HashMap<String, DailyBands>,
) -> Vec<Suggestion>
```

Registered in `generate_all_suggestions` after the squeeze engine. Note:
`generate_all_suggestions`'s `monthly_sma` parameter type changes to
`&HashMap<String, DailyBands>` and is forwarded to the new engine.

`signal_name = "Bollinger Reversion"`.

### Example output

```
[Bollinger Reversion]
SWAP NVDAx FOR GOOGLx
  • NVDAx €201.62 ≥ upper band €198.40 (30d, 2σ) — %B=1.08, stretched above envelope
  • GOOGLx €340.22 ≤ lower band €344.10 (30d, 2σ) — %B=−0.04, stretched below envelope
  • Price outside ±2σ Bollinger band — mean reversion expected (Bollinger, 2002)
```

```
[Bollinger Reversion]
WATCH NVDAx — above upper band
  • NVDAx €201.62 ≥ upper band €198.40 (30d, 2σ) — %B=1.08
  • Price above +2σ Bollinger band, no buy candidate to pair — mean reversion expected (Bollinger, 2002)
```

## Configuration

New constants at the top of `src/portfolio/suggestions.rs`:

| Parameter | Constant | Default | Notes |
|---|---|---|---|
| Band σ multiplier | `BOLLINGER_K` | 2.0 | Matches the CLI chart. |
| Band minimum daily points | `BOLLINGER_MIN_DAYS` | 14 | Below this, σ is too noisy to act on. |

## Error handling / edge cases

- **Too few daily points** (`n < 14`): asset skipped, no suggestion.
- **Flat series** (band width `< ε`, e.g. USDY stablecoin): skipped to avoid
  %B division blow-up.
- **No bands for an asset** (not in the map — Birdeye/local SMA unavailable):
  asset skipped. Engine degrades to silence, never panics.
- **No price in history**: `price_series` returns empty → asset skipped.

## Testing

Unit tests in the `#[cfg(test)]` block of `suggestions.rs` (existing style,
synthetic deque + `DailyBands` map):

1. `test_bollinger_sell_and_buy_pair` — one asset above upper, one below lower →
   one `SWAP sell FOR buy` suggestion.
2. `test_bollinger_standalone_watch_when_unpaired` — only an above-upper asset →
   standalone `WATCH … above upper band`, no SWAP.
3. `test_bollinger_skips_flat_series` — σ≈0 asset (USDY-like) produces nothing.
4. `test_bollinger_skips_insufficient_days` — `n < BOLLINGER_MIN_DAYS` → nothing.
5. `test_daily_bands_sigma` (in `pricer.rs`) — `compute_sma_from_history` returns
   the expected sample σ for a known daily series.

## Documentation

Add engine #6 to `docs/portfolio/trading-signals.md`:
- New section "### 6. Bollinger Band Reversion" mirroring the existing sections
  (academic basis: Bollinger 2002; computation; min data; example output).
- New config rows for `BOLLINGER_K` and `BOLLINGER_MIN_DAYS`.
- New row in the Data Dependencies table (needs 30d daily bands; degrades silently
  without them).

## Out of scope

- Changing the CLI chart's band logic (already correct; different data path).
- Band-width / %B-based position sizing in the rebalancer.
- Making `K` or the window runtime-configurable via env vars (constants for now,
  consistent with the other engines).
