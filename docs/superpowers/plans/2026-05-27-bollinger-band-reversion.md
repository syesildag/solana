# Bollinger Band Reversion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a sixth portfolio-watcher signal engine that flags assets whose live price has pierced their 30-day ±2σ Bollinger band, on a mean-reversion basis.

**Architecture:** Promote the watcher's `monthly_sma: HashMap<String, f64>` to `HashMap<String, DailyBands>` (a `{ sma, sigma, n }` struct computed from the same daily series), so the new engine reads mean and σ from a single source of truth. The new `generate_bollinger_suggestions` engine pairs above-upper (SELL) and below-lower (BUY) candidates into SWAPs, falling back to standalone WATCH when only one side fires.

**Tech Stack:** Rust, Tokio. Tests live in `#[cfg(test)]` blocks at the bottom of each source file. Run with `cargo test --bin solana-mev`.

**Spec:** `docs/superpowers/specs/2026-05-27-bollinger-band-reversion-design.md`

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/pricer.rs` | Produces the 30d daily stats | Add `DailyBands` struct; both SMA producers return `HashMap<String, DailyBands>` (compute σ + n). |
| `src/portfolio/suggestions.rs` | Signal engines | New `generate_bollinger_suggestions`; RSI + squeeze read `.sma`; aggregator forwards bands. |
| `src/portfolio/analyzer.rs` | Swap suggestions | `generate_swap_suggestions` reads `.sma`; test maps build `DailyBands`. |
| `src/portfolio/watcher.rs` | Run loop | `monthly_sma` becomes the stats map (type inference handles it; no logic change). |
| `docs/portfolio/trading-signals.md` | Signal docs | Add section #6, config rows, data-dependency row. |

---

## Task 1: Promote `monthly_sma` to `DailyBands` (mean + σ + n)

This task changes a type that ripples across four files. Everything must still compile and all existing tests must still pass with **identical behavior** — only the carrier type changes, plus a new σ field that nothing reads yet.

**Files:**
- Modify: `src/portfolio/pricer.rs` (add struct; update `fetch_monthly_sma` ~341-414 and `compute_sma_from_history` ~423-468)
- Modify: `src/portfolio/suggestions.rs` (RSI ~193, squeeze ~378, aggregator ~416-429)
- Modify: `src/portfolio/analyzer.rs` (`generate_swap_suggestions` ~426-480; tests ~887-931, ~1134-1150)
- Modify: `src/portfolio/watcher.rs` (no code change expected — verify it compiles)

- [ ] **Step 1: Add the `DailyBands` struct in `pricer.rs`**

Add near the top of `src/portfolio/pricer.rs`, after the existing `use`/const block:

```rust
/// 30-day daily statistics for one asset, keyed by both mint and symbol.
/// `sma` is the mean of the daily series; `sigma` is its sample standard
/// deviation (Bessel's correction, /(n-1)); `n` is the number of daily points.
/// Bollinger bands are `sma ± k·sigma`.
#[derive(Debug, Clone, Copy)]
pub struct DailyBands {
    pub sma: f64,
    pub sigma: f64,
    pub n: usize,
}
```

- [ ] **Step 2: Update `fetch_monthly_sma` to compute σ and return `DailyBands`**

In `src/portfolio/pricer.rs`, change the signature return type:

```rust
pub async fn fetch_monthly_sma(
    client: &Client,
    api_key: &str,
    portfolio: &super::Portfolio,
) -> HashMap<String, DailyBands> {
```

Change the map declaration:

```rust
    let mut sma_map: HashMap<String, DailyBands> = HashMap::new();
```

Replace the mean computation + inserts (the current lines computing `sma` and the two `sma_map.insert(...)`) with:

```rust
        let n = prices.len();
        let sma = prices.iter().sum::<f64>() / n as f64;
        let sigma = (prices.iter().map(|p| (p - sma).powi(2)).sum::<f64>()
            / (n - 1) as f64)
            .sqrt();
        tracing::info!(
            "portfolio: 30d SMA {symbol} = ${sma:.4} σ=${sigma:.4} ({n} candles)"
        );
        let bands = DailyBands { sma, sigma, n };
        sma_map.insert(mint.clone(), bands);
        sma_map.insert(symbol.clone(), bands);
```

(The existing `if prices.len() < 7 { continue; }` guard stays, so `n >= 7` here and `n - 1` is safe.)

- [ ] **Step 3: Update `compute_sma_from_history` to compute σ and return `DailyBands`**

In `src/portfolio/pricer.rs`, change the signature return type:

```rust
pub fn compute_sma_from_history(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &super::Portfolio,
) -> HashMap<String, DailyBands> {
```

Change the map declaration:

```rust
    let mut sma_map: HashMap<String, DailyBands> = HashMap::new();
```

Replace the mean computation + inserts (current `let values...`, `let sma...`, log, two inserts) with:

```rust
        let values: Vec<f64> = daily.values().cloned().collect();
        let n = values.len();
        let sma = values.iter().sum::<f64>() / n as f64;
        let sigma = (values.iter().map(|v| (v - sma).powi(2)).sum::<f64>()
            / (n - 1) as f64)
            .sqrt();
        tracing::info!(
            "portfolio: {n}-day SMA {symbol} = ${sma:.4} σ=${sigma:.4} (local history)"
        );
        let bands = DailyBands { sma, sigma, n };
        sma_map.insert(mint.clone(), bands);
        sma_map.insert(symbol.clone(), bands);
```

(The existing `if daily.len() < 2 { continue; }` guard stays, so `n >= 2` and `n - 1` is safe.)

- [ ] **Step 4: Update the suggestion-engine consumers in `suggestions.rs`**

Add to the imports at the top of `src/portfolio/suggestions.rs` (alongside the existing `use super::...` lines):

```rust
use super::pricer::DailyBands;
```

In `generate_rsi_suggestions`, change the parameter type:

```rust
    monthly_sma: &HashMap<String, DailyBands>,
```

and change the lookup (currently `let sma = monthly_sma.get(sym).copied();`):

```rust
        let sma = monthly_sma.get(sym).map(|b| b.sma);
```

and the two SMA-fallback lookups in the pairing loop (currently `monthly_sma.get(...).copied().unwrap_or(*price)`):

```rust
            let sell_sma = monthly_sma.get(sell_sym.as_str()).map(|b| b.sma).unwrap_or(*sell_price);
```
```rust
            let buy_sma = monthly_sma.get(buy_sym.as_str()).map(|b| b.sma).unwrap_or(*buy_price);
```

In `generate_vol_squeeze_suggestions`, change the parameter type:

```rust
    monthly_sma: &HashMap<String, DailyBands>,
```

and change the lookup (currently `let sma = monthly_sma.get(sym).copied();`):

```rust
        let sma = monthly_sma.get(sym).map(|b| b.sma);
```

In `generate_all_suggestions`, change the parameter type:

```rust
    monthly_sma: &HashMap<String, DailyBands>,
```

(Body unchanged in this task — the new engine is wired in Task 3.)

- [ ] **Step 5: Update `generate_swap_suggestions` in `analyzer.rs`**

Add to the imports at the top of `src/portfolio/analyzer.rs`:

```rust
use super::pricer::DailyBands;
```

Change the parameter type (currently `monthly_sma: &HashMap<String, f64>`):

```rust
    monthly_sma: &HashMap<String, DailyBands>,
```

The lookup `let Some(sma) = monthly_sma.get(symbol) else { continue; };` now binds `&DailyBands`. Update the two comparisons that deref it (currently `current_price > *sma` and `current_price < *sma`):

```rust
            AlertKind::New7dHigh { .. } if current_price > sma.sma => {
```
```rust
            AlertKind::New7dLow { .. } if current_price < sma.sma => {
```

Update the two indexed lookups in the pairing loop (currently `monthly_sma[sell_alert.symbol.as_str()]` and `monthly_sma[buy_alert.symbol.as_str()]`):

```rust
        let sell_sma = monthly_sma[sell_alert.symbol.as_str()].sma;
```
```rust
            let buy_sma = monthly_sma[buy_alert.symbol.as_str()].sma;
```

- [ ] **Step 6: Update the `analyzer.rs` tests that build SMA maps**

In the `#[cfg(test)] mod tests` block of `src/portfolio/analyzer.rs`, add the import:

```rust
    use crate::portfolio::pricer::DailyBands;
```

Replace every `sma.insert("SYM".to_string(), VALUE);` with a `DailyBands`. The four affected tests are `test_swap_suggestion_generated` (~887-889), `test_no_swap_7dhigh_below_sma` (~916-917), `test_no_swap_7dlow_above_sma` (~929-930), and `test_no_swap_missing_asset_in_sma` (~1148-1149). For each, wrap the float:

```rust
        sma.insert("NVDAx".to_string(), DailyBands { sma: 185.0, sigma: 1.0, n: 30 });
```
```rust
        sma.insert("GOOGLx".to_string(), DailyBands { sma: 360.0, sigma: 1.0, n: 30 });
```

(Use the same numeric mean each test used before; `sigma`/`n` are arbitrary positive values — these tests don't exercise σ. The `test_no_swap_without_sma` test passes an empty `HashMap::new()` and needs no change — its element type is inferred from the call.)

- [ ] **Step 7: Add a σ unit test to `pricer.rs`**

In (or create) the `#[cfg(test)] mod tests` block at the bottom of `src/portfolio/pricer.rs`, add:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::history::PriceSnapshot;
    use crate::portfolio::{Portfolio, TokenEntry};
    use std::collections::{HashMap, VecDeque};

    #[test]
    fn test_daily_bands_sigma_from_history() {
        // Three distinct UTC days with closes 100, 110, 120.
        // mean = 110; sample σ = sqrt((100+0+100)/2) = 10.
        const DAY: u64 = 86_400;
        let mut history: VecDeque<PriceSnapshot> = VecDeque::new();
        for (i, p) in [100.0_f64, 110.0, 120.0].iter().enumerate() {
            let mut prices = HashMap::new();
            prices.insert("SOL".to_string(), *p);
            history.push_back(PriceSnapshot { ts: i as u64 * DAY, prices });
        }
        let portfolio = Portfolio { sol_amount: 1.0, tokens: Vec::<TokenEntry>::new() };

        let bands = compute_sma_from_history(&history, &portfolio);
        let sol = bands.get("SOL").expect("SOL bands present");
        assert!((sol.sma - 110.0).abs() < 1e-9, "sma was {}", sol.sma);
        assert!((sol.sigma - 10.0).abs() < 1e-9, "sigma was {}", sol.sigma);
        assert_eq!(sol.n, 3);
    }
}
```

If a `mod tests` block already exists in `pricer.rs`, add only the `#[test] fn test_daily_bands_sigma_from_history` (and any missing `use`) inside it instead of duplicating the module.

- [ ] **Step 8: Build and run the full suite to confirm no behavior regression**

Run: `cargo test --bin solana-mev 2>&1 | tail -30`
Expected: compiles cleanly; all existing tests pass; `test_daily_bands_sigma_from_history` passes. `watcher.rs` needs no edit (the `monthly_sma` binding infers `HashMap<String, DailyBands>` from the producer return types; `monthly_sma.len() / 2` logging is still valid).

- [ ] **Step 9: Commit**

```bash
git add src/portfolio/pricer.rs src/portfolio/suggestions.rs src/portfolio/analyzer.rs
git commit -m "refactor(portfolio): carry 30d daily σ alongside SMA via DailyBands

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 2: Implement `generate_bollinger_suggestions` (engine #6)

**Files:**
- Modify: `src/portfolio/suggestions.rs` (new constants + function; tests in the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/suggestions.rs`. These reuse the existing `make_snap`, `single_token_portfolio`, and `two_token_portfolio` helpers already defined in that module. Add `use crate::portfolio::pricer::DailyBands;` to the test module's imports.

```rust
    fn bands_map(entries: &[(&str, f64, f64, usize)]) -> HashMap<String, DailyBands> {
        let mut m = HashMap::new();
        for &(sym, sma, sigma, n) in entries {
            m.insert(sym.to_string(), DailyBands { sma, sigma, n });
        }
        m
    }

    #[test]
    fn test_bollinger_sell_and_buy_pair() {
        // AAPLx upper=110, lower=90; price 115 → SELL.
        // QQQx  upper=210, lower=190; price 185 → BUY.
        let portfolio = two_token_portfolio();
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintA", 115.0), ("mintB", 185.0)]));
        let bands = bands_map(&[("AAPLx", 100.0, 5.0, 30), ("QQQx", 200.0, 5.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert_eq!(s.len(), 1, "expected one SWAP suggestion");
        assert!(s[0].action.contains("SWAP AAPLx FOR QQQx"), "got {}", s[0].action);
        assert_eq!(s[0].signal_name, "Bollinger Reversion");
    }

    #[test]
    fn test_bollinger_standalone_watch_when_unpaired() {
        // Only AAPLx pierces (above upper); QQQx sits inside its band → WATCH, no SWAP.
        let portfolio = two_token_portfolio();
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintA", 115.0), ("mintB", 200.0)]));
        let bands = bands_map(&[("AAPLx", 100.0, 5.0, 30), ("QQQx", 200.0, 5.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert_eq!(s.len(), 1, "expected one standalone WATCH");
        assert!(s[0].action.contains("WATCH AAPLx — above upper band"), "got {}", s[0].action);
        assert!(!s[0].action.contains("SWAP"));
    }

    #[test]
    fn test_bollinger_skips_flat_series() {
        // USDY-like: σ=0 → band width 0 → skipped (no %B blow-up).
        let portfolio = single_token_portfolio("USDY", "mintU");
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintU", 1.5)]));
        let bands = bands_map(&[("USDY", 1.0, 0.0, 30)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert!(s.is_empty(), "flat series must produce no suggestion");
    }

    #[test]
    fn test_bollinger_skips_insufficient_days() {
        // n=5 < BOLLINGER_MIN_DAYS(14) → skipped even though price pierces.
        let portfolio = single_token_portfolio("NVDAx", "mintN");
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintN", 999.0)]));
        let bands = bands_map(&[("NVDAx", 100.0, 5.0, 5)]);

        let s = generate_bollinger_suggestions(&history, &portfolio, &bands);
        assert!(s.is_empty(), "insufficient daily points must produce no suggestion");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin solana-mev bollinger 2>&1 | tail -20`
Expected: FAIL — `cannot find function generate_bollinger_suggestions in this scope`.

- [ ] **Step 3: Implement the engine**

Add to `src/portfolio/suggestions.rs` after `generate_vol_squeeze_suggestions` (before the `// ── Aggregator ──` section):

```rust
// ── 6. Bollinger Band Reversion ───────────────────────────────────────────────
// Reference: Bollinger (2002) "Bollinger on Bollinger Bands". A price piercing
// the ±Kσ envelope around the 30-day moving average is statistically stretched;
// on a mean-reversion basis it is expected to revert toward the band.

const BOLLINGER_K: f64 = 2.0;          // matches the CLI chart's ±2σ envelope
const BOLLINGER_MIN_DAYS: usize = 14;  // minimum daily points for a meaningful σ

pub fn generate_bollinger_suggestions(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    bands: &HashMap<String, DailyBands>,
) -> Vec<Suggestion> {
    let symbols: Vec<&str> = std::iter::once("SOL")
        .chain(portfolio.tokens.iter().map(|t| t.symbol.as_str()))
        .collect();

    // (symbol, price, band_edge, pct_b)
    let mut sell_cands: Vec<(String, f64, f64, f64)> = vec![];
    let mut buy_cands: Vec<(String, f64, f64, f64)> = vec![];

    for &sym in &symbols {
        let Some(b) = bands.get(sym) else { continue; };
        if b.n < BOLLINGER_MIN_DAYS { continue; }

        let upper = b.sma + BOLLINGER_K * b.sigma;
        let lower = b.sma - BOLLINGER_K * b.sigma;
        let width = upper - lower;
        if width < 1e-9 { continue; } // flat series (e.g. USDY stablecoin)

        let prices = price_series(sym, portfolio, history);
        let Some(&price) = prices.last() else { continue; };
        let pct_b = (price - lower) / width;

        if price >= upper {
            sell_cands.push((sym.to_string(), price, upper, pct_b));
        } else if price <= lower {
            buy_cands.push((sym.to_string(), price, lower, pct_b));
        }
    }

    let mut suggestions = Vec::new();

    if !sell_cands.is_empty() && !buy_cands.is_empty() {
        // Both sides fired → pair them into SWAPs (cross product, like RSI).
        for (ss, sp, su, sb) in &sell_cands {
            for (bs, bp, bl, bb) in &buy_cands {
                suggestions.push(Suggestion {
                    action: format!("SWAP {} FOR {}", ss, bs),
                    signal_name: "Bollinger Reversion".to_string(),
                    rationale: vec![
                        format!(
                            "{} €{:.2} ≥ upper band €{:.2} (30d, 2σ) — %B={:.2}, stretched above envelope",
                            ss, sp, su, sb
                        ),
                        format!(
                            "{} €{:.2} ≤ lower band €{:.2} (30d, 2σ) — %B={:.2}, stretched below envelope",
                            bs, bp, bl, bb
                        ),
                        "Price outside ±2σ Bollinger band — mean reversion expected (Bollinger, 2002)".to_string(),
                    ],
                });
            }
        }
    } else {
        // Only one side fired → standalone WATCH so the pierce is never dropped.
        for (ss, sp, su, sb) in &sell_cands {
            suggestions.push(Suggestion {
                action: format!("WATCH {} — above upper band", ss),
                signal_name: "Bollinger Reversion".to_string(),
                rationale: vec![
                    format!("{} €{:.2} ≥ upper band €{:.2} (30d, 2σ) — %B={:.2}", ss, sp, su, sb),
                    "Price above +2σ Bollinger band, no buy candidate to pair — mean reversion expected (Bollinger, 2002)".to_string(),
                ],
            });
        }
        for (bs, bp, bl, bb) in &buy_cands {
            suggestions.push(Suggestion {
                action: format!("WATCH {} — below lower band", bs),
                signal_name: "Bollinger Reversion".to_string(),
                rationale: vec![
                    format!("{} €{:.2} ≤ lower band €{:.2} (30d, 2σ) — %B={:.2}", bs, bp, bl, bb),
                    "Price below −2σ Bollinger band, no sell candidate to pair — mean reversion expected (Bollinger, 2002)".to_string(),
                ],
            });
        }
    }

    suggestions
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin solana-mev bollinger 2>&1 | tail -20`
Expected: PASS — all four `test_bollinger_*` tests green.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/suggestions.rs
git commit -m "feat(portfolio): add Bollinger Band Reversion signal engine

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 3: Wire engine #6 into the aggregator

**Files:**
- Modify: `src/portfolio/suggestions.rs` (`generate_all_suggestions` body + a test)

- [ ] **Step 1: Write the failing aggregator test**

Add to the `#[cfg(test)] mod tests` block in `src/portfolio/suggestions.rs`:

```rust
    #[test]
    fn test_aggregator_includes_bollinger() {
        // One asset pierces its upper band → the aggregate must surface a
        // Bollinger Reversion suggestion among its results.
        let portfolio = single_token_portfolio("NVDAx", "mintN");
        let mut history = VecDeque::new();
        history.push_back(make_snap(0, &[("mintN", 130.0)])); // above upper=110
        let bands = bands_map(&[("NVDAx", 100.0, 5.0, 30)]);
        let risk = empty_risk(&["NVDAx"]);

        let all = generate_all_suggestions(&history, &portfolio, &risk, &bands);
        assert!(
            all.iter().any(|s| s.signal_name == "Bollinger Reversion"),
            "aggregator must include the Bollinger engine"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin solana-mev test_aggregator_includes_bollinger 2>&1 | tail -20`
Expected: FAIL — assertion fails (no Bollinger suggestion yet; the engine isn't registered).

- [ ] **Step 3: Register the engine in `generate_all_suggestions`**

In `src/portfolio/suggestions.rs`, add one line to the body of `generate_all_suggestions`, after the `generate_vol_squeeze_suggestions` extend:

```rust
    all.extend(generate_bollinger_suggestions(history, portfolio, monthly_sma));
```

- [ ] **Step 4: Run the full suite to verify everything passes**

Run: `cargo test --bin solana-mev 2>&1 | tail -30`
Expected: PASS — `test_aggregator_includes_bollinger` and all prior tests green.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/suggestions.rs
git commit -m "feat(portfolio): register Bollinger Reversion in suggestion aggregator

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Task 4: Document engine #6

**Files:**
- Modify: `docs/portfolio/trading-signals.md`

- [ ] **Step 1: Add the engine #6 section**

In `docs/portfolio/trading-signals.md`, after the "### 5. Volatility Squeeze" section and before the "## Configuration" section (i.e. after the `---` that closes section 5), insert:

```markdown
### 6. Bollinger Band Reversion

**Academic basis:** Bollinger (2002) *"Bollinger on Bollinger Bands"*. A price trading outside the ±2σ envelope around its moving average is statistically stretched and tends to revert toward the band.

**What it detects:** Assets whose current price has pierced their 30-day Bollinger band — above the upper band (overextended, sell candidate) or below the lower band (oversold, buy candidate). Unlike the Volatility Squeeze (#5), which measures band *width* contracting, this engine measures band *position*.

**Signal computation:**
```
upper = 30d SMA + K·σ        K = 2.0
lower = 30d SMA − K·σ
%B    = (price − lower) / (upper − lower)    0 = lower band, 1 = upper band

price ≥ upper (%B ≥ 1) → SELL candidate
price ≤ lower (%B ≤ 0) → BUY candidate
```

When a sell and a buy candidate co-exist, they are paired into `SWAP sell FOR buy` (same pairing as the RSI engine). A lone candidate with no partner emits a standalone `WATCH`.

**Minimum data:** 14 daily points for a meaningful σ (`BOLLINGER_MIN_DAYS`). The bands reuse the same 30-day daily series as the 30d SMA (local history, or Birdeye `1D` candles). Flat series (σ≈0, e.g. USDY) are skipped.

**Example output:**
```
[Bollinger Reversion]
SWAP NVDAx FOR GOOGLx
  • NVDAx €201.62 ≥ upper band €198.40 (30d, 2σ) — %B=1.08, stretched above envelope
  • GOOGLx €340.22 ≤ lower band €344.10 (30d, 2σ) — %B=−0.04, stretched below envelope
  • Price outside ±2σ Bollinger band — mean reversion expected (Bollinger, 2002)
```

---
```

- [ ] **Step 2: Add the configuration rows**

In the "## Configuration" table, after the `SQUEEZE_RATIO_THRESHOLD` row, add:

```markdown
| Bollinger σ multiplier | `BOLLINGER_K` | 2.0 | Band = 30d SMA ± k·σ; matches the CLI chart |
| Bollinger min daily points | `BOLLINGER_MIN_DAYS` | 14 | Below this, σ is too noisy to act on |
```

- [ ] **Step 3: Add the data-dependency row**

In the "## Data Dependencies" table, after the `Volatility Squeeze` row, add:

```markdown
| Bollinger Reversion | 14 daily points | Yes (bands) | No |
```

- [ ] **Step 4: Commit**

```bash
git add docs/portfolio/trading-signals.md
git commit -m "docs(portfolio): document Bollinger Reversion signal engine

Co-Authored-By: Claude Opus 4.7 <noreply@anthropic.com>"
```

---

## Self-Review Notes

- **Spec coverage:** Stats-struct plumbing (Task 1), mean-reversion engine with SWAP/WATCH fallback (Task 2), aggregator wiring (Task 3), docs incl. config + data-dependency rows (Task 4). All five spec tests are present: pairing (T2 s1), standalone watch (T2 s1), flat-series skip (T2 s1), insufficient-days skip (T2 s1), pricer σ (T1 s7). Aggregator test added (T3) for integration confidence.
- **Type consistency:** `DailyBands { sma, sigma, n }` is defined once in `pricer.rs` and imported via `super::pricer::DailyBands` (non-test) / `crate::portfolio::pricer::DailyBands` (tests). `generate_bollinger_suggestions(history, portfolio, bands)` signature matches its call in `generate_all_suggestions`. The aggregator's `monthly_sma` param (type `&HashMap<String, DailyBands>`) is forwarded as `bands`.
- **No behavior change in Task 1:** every consumer reads `.sma`, reproducing the prior `f64` value exactly; σ is additive and unread until Task 2.
```
