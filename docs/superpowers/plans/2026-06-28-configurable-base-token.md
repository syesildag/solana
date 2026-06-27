# Configurable Base Token Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the arbitrage engine's base/starting token configurable (SOL default, USDC opt-in, wallet-funded for USDC), in service of stable-denominated P&L, without changing the proven SOL path.

**Architecture:** A thin `BaseToken` value object (mint, decimals, symbol, `is_native`) is resolved from `BASE_MINT` at startup and stored on `Config`. The single real branch point is native-vs-SPL: a native base (WSOL) keeps today's wrap/unwrap byte-for-byte; a plain SPL base (USDC) funds directly from its wallet ATA with no wrap. A process-wide lock-free SOL/USD price cache (published by the portfolio watcher, read in the hot loop) converts USDC profit to a SOL-equivalent so the SOL-denominated Jito tip stays correctly sized. Wallet accounting becomes dual-guard: P&L drawdown in base units plus an independent SOL gas floor.

**Tech Stack:** Rust, solana-sdk, spl-token, tokio, anyhow, tracing. Tests are inline `#[cfg(test)]` modules at the bottom of each source file (project convention).

## Global Constraints

- **NEVER run `cargo fmt` or `rustfmt`** on any file — this repo is not rustfmt-clean and it produces massive diff churn. Hand-match surrounding style only.
- Tests live in `#[cfg(test)]` blocks at the **bottom of each source file**, not in a separate `tests/` tree.
- Build/test with `cargo test --bin solana-mev` (filter by name as shown). Lint with `cargo clippy`.
- **Backwards compatibility is mandatory:** `BASE_MINT` defaults to the WSOL mint, so an unchanged `.env` must behave exactly as today (`is_native == true` everywhere).
- WSOL mint: `So11111111111111111111111111111111111111112`. USDC mint: `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`.
- **Out of scope (do not implement):** MarginFi USDC-bank flash loan (USDC is wallet-funded only); changing how Jito tips are *paid* (always SOL); curating USDC pools in `pools.json`.
- Commit after every task. Do NOT `git push` (publishing is the user's call).

---

## File Structure

| File | Responsibility |
|---|---|
| `src/dex/types.rs` | Add `BaseToken` struct, `USDC_MINT`/`USDC_PUBKEY` consts, `resolve_base_token` resolver. |
| `src/arbitrage/sol_price.rs` (new) | Process-wide SOL/USD price cache + pure conversion helpers for tip sizing. |
| `src/arbitrage/capital.rs` (new) | Pure wallet-capital helpers: `spendable_base`, `evaluate_halt`. |
| `src/arbitrage/mod.rs` | Register the two new modules. |
| `src/config.rs` | Parse `BASE_MINT` → `base_token`; force-disable flash loan for non-native; add `min_sol_gas_lamports`; base-neutral threshold aliases. |
| `src/arbitrage/evaluator.rs` | `is_native`-aware setup/teardown; convert gross profit before `compute_jito_tip`. |
| `src/portfolio/watcher.rs` | Publish the fetched SOL/USD price into the price cache each tick. |
| `src/main.rs` | Use `base_token.mint` as cycle source; startup base logging; dual-guard halt + base-balance capital cap. |
| `.env.example` | Document `BASE_MINT`, `MIN_SOL_GAS_LAMPORTS`, base-unit thresholds, USDC values. |

---

## Task 1: `BaseToken` value object + resolver

**Files:**
- Modify: `src/dex/types.rs` (add consts + struct + resolver near the existing `WSOL_MINT`/`WSOL_PUBKEY` block, ~lines 7–18; add tests to the file's `#[cfg(test)]` block)

**Interfaces:**
- Produces:
  - `pub const USDC_MINT: &str`
  - `pub const USDC_PUBKEY: Pubkey`
  - `pub struct BaseToken { pub mint: Pubkey, pub decimals: u8, pub symbol: &'static str, pub is_native: bool }` (derives `Debug, Clone, Copy`)
  - `pub fn resolve_base_token(mint: &str) -> Result<BaseToken, String>`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module at the bottom of `src/dex/types.rs`:

```rust
#[test]
fn resolve_base_token_sol_is_native() {
    let bt = super::resolve_base_token(super::WSOL_MINT).unwrap();
    assert_eq!(bt.symbol, "SOL");
    assert_eq!(bt.decimals, 9);
    assert!(bt.is_native);
    assert_eq!(bt.mint, super::WSOL_PUBKEY);
}

#[test]
fn resolve_base_token_usdc_is_spl() {
    let bt = super::resolve_base_token(super::USDC_MINT).unwrap();
    assert_eq!(bt.symbol, "USDC");
    assert_eq!(bt.decimals, 6);
    assert!(!bt.is_native);
    assert_eq!(bt.mint, super::USDC_PUBKEY);
}

#[test]
fn resolve_base_token_unknown_errors() {
    let err = super::resolve_base_token("NotAMint111").unwrap_err();
    assert!(err.contains("Unsupported BASE_MINT"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev resolve_base_token`
Expected: FAIL — `cannot find function resolve_base_token` / `USDC_MINT` not found.

- [ ] **Step 3: Write minimal implementation**

In `src/dex/types.rs`, immediately after the `WSOL_PUBKEY` const (line 12):

```rust
pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const USDC_PUBKEY: Pubkey = solana_sdk::pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

/// The arbitrage engine's base/starting token. Cycles begin and end at `mint`.
/// `is_native` is the one real branch point: native (WSOL) needs SOL wrap/unwrap;
/// a plain SPL base (USDC) funds directly from its wallet ATA with no wrap.
#[derive(Debug, Clone, Copy)]
pub struct BaseToken {
    pub mint: Pubkey,
    pub decimals: u8,
    pub symbol: &'static str,
    pub is_native: bool,
}

/// Resolve a base-token mint string into its metadata. Only vetted bases are allowed,
/// so thresholds and wrap behavior are never guessed from an unknown mint.
pub fn resolve_base_token(mint: &str) -> Result<BaseToken, String> {
    match mint {
        WSOL_MINT => Ok(BaseToken { mint: WSOL_PUBKEY, decimals: 9, symbol: "SOL",  is_native: true }),
        USDC_MINT => Ok(BaseToken { mint: USDC_PUBKEY, decimals: 6, symbol: "USDC", is_native: false }),
        other => Err(format!(
            "Unsupported BASE_MINT '{other}'. Supported: SOL ({WSOL_MINT}), USDC ({USDC_MINT})"
        )),
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin solana-mev resolve_base_token`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/dex/types.rs
git commit -m "feat(arb): BaseToken value object + resolve_base_token (SOL/USDC)"
```

---

## Task 2: SOL price cache + tip conversion helpers

**Files:**
- Create: `src/arbitrage/sol_price.rs`
- Modify: `src/arbitrage/mod.rs` (add `pub mod sol_price;`)
- Test: inline `#[cfg(test)]` in `src/arbitrage/sol_price.rs`

**Interfaces:**
- Consumes: `crate::dex::types::BaseToken` (Task 1)
- Produces:
  - `pub fn publish(price_usd: f64)` — store latest SOL/USD with current timestamp
  - `pub fn get_fresh(max_age_secs: u64) -> Option<f64>` — latest price if not stale
  - `pub const PRICE_MAX_AGE_SECS: u64` — staleness ceiling for tip sizing
  - `pub fn gross_profit_for_tip(gross_base_units: u64, base: &BaseToken, sol_price_usd: Option<f64>) -> u64` — SOL-equivalent lamports for tip sizing (identity for native; `0` when price missing → caller falls back to floor tip)
  - (pure internal, exposed for tests) `pub(crate) fn base_units_to_lamports(units: u64, decimals: u8, sol_price_usd: f64) -> u64`, `pub(crate) fn fresh_price(price_bits: u64, ts: u64, now: u64, max_age: u64) -> Option<f64>`

- [ ] **Step 1: Write the failing test**

Create `src/arbitrage/sol_price.rs` with ONLY the test module first (implementation comes in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::resolve_base_token;
    use crate::dex::types::{WSOL_MINT, USDC_MINT};

    #[test]
    fn base_units_to_lamports_usdc() {
        // 10 USDC (6dp) at $200/SOL = 0.05 SOL = 50_000_000 lamports
        assert_eq!(base_units_to_lamports(10_000_000, 6, 200.0), 50_000_000);
    }

    #[test]
    fn gross_for_tip_native_is_identity() {
        let sol = resolve_base_token(WSOL_MINT).unwrap();
        // Price is ignored for native base.
        assert_eq!(gross_profit_for_tip(400_000, &sol, None), 400_000);
        assert_eq!(gross_profit_for_tip(400_000, &sol, Some(200.0)), 400_000);
    }

    #[test]
    fn gross_for_tip_usdc_converts() {
        let usdc = resolve_base_token(USDC_MINT).unwrap();
        assert_eq!(gross_profit_for_tip(10_000_000, &usdc, Some(200.0)), 50_000_000);
    }

    #[test]
    fn gross_for_tip_usdc_stale_price_is_zero() {
        let usdc = resolve_base_token(USDC_MINT).unwrap();
        // None price → 0 so the caller uses the floor tip rather than bidding blind.
        assert_eq!(gross_profit_for_tip(10_000_000, &usdc, None), 0);
        assert_eq!(gross_profit_for_tip(10_000_000, &usdc, Some(0.0)), 0);
    }

    #[test]
    fn fresh_price_respects_staleness() {
        let bits = 200.0_f64.to_bits();
        assert_eq!(fresh_price(bits, 100, 150, 60), Some(200.0)); // 50s old, max 60 → fresh
        assert_eq!(fresh_price(bits, 100, 200, 60), None);        // 100s old, max 60 → stale
        assert_eq!(fresh_price(bits, 0,   200, 60), None);        // never published
        assert_eq!(fresh_price(0,    100, 100, 60), None);        // price 0.0 → invalid
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev sol_price`
Expected: FAIL — module `sol_price` not declared / functions not found.

- [ ] **Step 3: Write minimal implementation**

Prepend the implementation above the test module in `src/arbitrage/sol_price.rs`:

```rust
//! Process-wide SOL/USD price cache.
//!
//! The portfolio watcher (async, ~300s cadence) publishes the latest SOL/USD price;
//! the arbitrage hot loop reads it lock-free to convert a non-native base's profit into
//! a SOL-equivalent lamport value for Jito-tip sizing. When no fresh price is available
//! the conversion yields 0, which makes the tip logic fall back to the floor tip rather
//! than bid on a stale rate.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::dex::types::BaseToken;

/// Tip sizing treats a price older than this (seconds) as missing. ~2× the watcher's
/// 300s refresh cadence.
pub const PRICE_MAX_AGE_SECS: u64 = 600;

static SOL_PRICE_USD_BITS: AtomicU64 = AtomicU64::new(0);
static SOL_PRICE_TS_SECS: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Publish the latest SOL/USD price (called by the portfolio watcher).
pub fn publish(price_usd: f64) {
    SOL_PRICE_USD_BITS.store(price_usd.to_bits(), Ordering::Relaxed);
    SOL_PRICE_TS_SECS.store(now_secs(), Ordering::Relaxed);
}

/// Latest SOL/USD price if published within `max_age_secs`, else None.
pub fn get_fresh(max_age_secs: u64) -> Option<f64> {
    fresh_price(
        SOL_PRICE_USD_BITS.load(Ordering::Relaxed),
        SOL_PRICE_TS_SECS.load(Ordering::Relaxed),
        now_secs(),
        max_age_secs,
    )
}

/// Pure staleness/validity check (testable without touching the statics or the clock).
pub(crate) fn fresh_price(price_bits: u64, ts: u64, now: u64, max_age: u64) -> Option<f64> {
    if ts == 0 || now.saturating_sub(ts) > max_age {
        return None;
    }
    let px = f64::from_bits(price_bits);
    if px > 0.0 { Some(px) } else { None }
}

/// Convert `units` of a token with `decimals` to SOL-equivalent lamports at `sol_price_usd`
/// (USD per 1 SOL). Pure.
pub(crate) fn base_units_to_lamports(units: u64, decimals: u8, sol_price_usd: f64) -> u64 {
    if sol_price_usd <= 0.0 {
        return 0;
    }
    let usd_value = units as f64 / 10f64.powi(decimals as i32);
    let sol_value = usd_value / sol_price_usd;
    (sol_value * 1e9) as u64
}

/// SOL-equivalent lamports for the gross profit, used only for Jito-tip sizing.
/// Native base: identity (already lamports). SPL base: convert via the cached price,
/// returning 0 when no fresh price is available so the caller uses the floor tip.
pub fn gross_profit_for_tip(gross_base_units: u64, base: &BaseToken, sol_price_usd: Option<f64>) -> u64 {
    if base.is_native {
        return gross_base_units;
    }
    match sol_price_usd {
        Some(px) if px > 0.0 => base_units_to_lamports(gross_base_units, base.decimals, px),
        _ => 0,
    }
}
```

Add to `src/arbitrage/mod.rs`:

```rust
pub mod sol_price;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin solana-mev sol_price`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/arbitrage/sol_price.rs src/arbitrage/mod.rs
git commit -m "feat(arb): SOL/USD price cache + tip conversion helpers"
```

---

## Task 3: Config — base token, force-disable flash loan, gas floor, aliases

**Files:**
- Modify: `src/config.rs` (struct ~lines 19–123; `from_env` ~lines 125–260)
- Test: inline `#[cfg(test)]` at bottom of `src/config.rs` (create the module if absent)

**Interfaces:**
- Consumes: `crate::dex::types::{BaseToken, resolve_base_token, WSOL_MINT}` (Task 1)
- Produces (new `Config` fields):
  - `pub base_token: BaseToken`
  - `pub min_sol_gas_lamports: u64`
- Behavior: when `!base_token.is_native`, `enable_flash_loan` is forced `false` (with a warning) and `flash_loan` is `None`. `INPUT_SOL_LAMPORTS` also accepts the alias `INPUT_BASE_UNITS`; `MIN_PROFIT_LAMPORTS` also accepts `MIN_PROFIT_BASE_UNITS` (env value still parsed into the same fields, now interpreted in base units).

- [ ] **Step 1: Write the failing test**

Add a `#[cfg(test)]` module at the bottom of `src/config.rs`. These tests exercise the pure decision rules (no env mutation), so add small helper fns alongside and test those:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flash_loan_forced_off_for_non_native_base() {
        // requested=true but base is SPL → must be disabled
        assert!(!super::resolve_flash_loan_enabled(true, false));
        // requested=true and base native → stays on
        assert!(super::resolve_flash_loan_enabled(true, true));
        // requested=false → stays off
        assert!(!super::resolve_flash_loan_enabled(false, true));
    }

    #[test]
    fn first_env_present_prefers_primary_then_alias() {
        assert_eq!(super::first_present(Some("5".into()), None, "9"), "5");
        assert_eq!(super::first_present(None, Some("7".into()), "9"), "7");
        assert_eq!(super::first_present(None, None, "9"), "9");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev config::tests`
Expected: FAIL — `resolve_flash_loan_enabled` / `first_present` not found.

- [ ] **Step 3: Write minimal implementation**

Add these free helpers above `impl Config` in `src/config.rs`:

```rust
/// Flash loan is only valid when the base token is native (WSOL). A non-native base
/// (USDC) is wallet-funded, so a requested flash loan is force-disabled.
pub(crate) fn resolve_flash_loan_enabled(requested: bool, base_is_native: bool) -> bool {
    requested && base_is_native
}

/// Return the primary env value if present, else the alias, else the default.
pub(crate) fn first_present(primary: Option<String>, alias: Option<String>, default: &str) -> String {
    primary.or(alias).unwrap_or_else(|| default.to_string())
}
```

Add the two fields to the `Config` struct (place `base_token` near the top, after `pools_config_path`; `min_sol_gas_lamports` near the other lamport fields):

```rust
    /// The arbitrage base/starting token. Defaults to SOL (`BASE_MINT` unset).
    pub base_token: crate::dex::types::BaseToken,
    /// Halt if native SOL falls below this (can't pay tips/fees). Only enforced when the
    /// base token is non-native; for a SOL base the P&L guard already covers it.
    pub min_sol_gas_lamports: u64,
```

In `from_env`, BEFORE the `Ok(Self { ... })` literal, resolve the base token and compute the effective flash-loan flag. Find where `enable_flash_loan` and `flash_loan` are currently built (struct field at ~line 201 and the `flash_loan:` block at ~line 257). Restructure so they consult the base token:

```rust
        let base_mint = env::var("BASE_MINT")
            .unwrap_or_else(|_| crate::dex::types::WSOL_MINT.to_string());
        let base_token = crate::dex::types::resolve_base_token(&base_mint)
            .map_err(|e| anyhow::anyhow!(e))?;

        let flash_loan_requested = env::var("ENABLE_FLASH_LOAN")
            .unwrap_or_else(|_| "false".to_string())
            .parse()
            .unwrap_or(false);
        let enable_flash_loan = resolve_flash_loan_enabled(flash_loan_requested, base_token.is_native);
        if flash_loan_requested && !enable_flash_loan {
            tracing::warn!(
                "ENABLE_FLASH_LOAN=true ignored: base token {} is not native (wallet-funded only).",
                base_token.symbol
            );
        }
```

Then in the `Ok(Self { ... })` literal:

1. Replace the inline `enable_flash_loan: env::var("ENABLE_FLASH_LOAN")…` field (lines 201–204) with:
   ```rust
            enable_flash_loan,
   ```
2. Replace the `input_sol_lamports` field (lines 145–148) with the alias-aware form:
   ```rust
            input_sol_lamports: first_present(
                env::var("INPUT_SOL_LAMPORTS").ok(),
                env::var("INPUT_BASE_UNITS").ok(),
                "1000000000",
            ).parse().context("INPUT_SOL_LAMPORTS/INPUT_BASE_UNITS must be a number")?,
   ```
3. Replace the `min_profit_lamports` field (lines 141–144) with:
   ```rust
            min_profit_lamports: first_present(
                env::var("MIN_PROFIT_LAMPORTS").ok(),
                env::var("MIN_PROFIT_BASE_UNITS").ok(),
                "10000",
            ).parse().context("MIN_PROFIT_LAMPORTS/MIN_PROFIT_BASE_UNITS must be a number")?,
   ```
4. Add the two new fields anywhere in the literal:
   ```rust
            base_token,
            min_sol_gas_lamports: env::var("MIN_SOL_GAS_LAMPORTS")
                .unwrap_or_else(|_| "100000000".to_string()) // 0.1 SOL
                .parse()
                .context("MIN_SOL_GAS_LAMPORTS must be a number")?,
   ```
5. In the existing `flash_loan:` block (~line 257), change the guard that decides whether to populate `FlashLoanConfig` so it uses the already-computed `enable_flash_loan` (which is now base-aware) instead of re-reading the env var. Replace the inner `let enabled = env::var("ENABLE_FLASH_LOAN")…` with:
   ```rust
                let enabled = enable_flash_loan;
   ```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin solana-mev config::tests`
Expected: PASS (2 tests).

- [ ] **Step 5: Verify the whole crate still compiles**

Run: `cargo build --bin solana-mev`
Expected: compiles (other call sites still use `config.base_token` in later tasks; nothing references it yet, which is fine).

- [ ] **Step 6: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): BASE_MINT base_token, force-disable flash loan for non-native, gas floor, base-unit aliases"
```

---

## Task 4: `is_native`-aware funding & settlement

**Files:**
- Modify: `src/arbitrage/evaluator.rs` — `build_setup_instructions` (lines 450–480), `build_teardown_instructions` (lines 484–490), and their wallet-funded callers (lines 422–423)
- Test: inline `#[cfg(test)]` in `src/arbitrage/evaluator.rs`

**Interfaces:**
- Consumes: `crate::dex::types::BaseToken` (Task 1)
- Produces (changed signatures):
  - `fn build_setup_instructions(user: Pubkey, amount_in: u64, path: &[Pubkey], base: &BaseToken) -> Vec<Instruction>`
  - `fn build_teardown_instructions(user: Pubkey, base: &BaseToken) -> Vec<Instruction>`
- Behavior: `is_native` → today's WSOL wrap (transfer + sync_native) and teardown close; `!is_native` → create the base ATA (idempotent), no transfer, no sync_native, empty teardown. Intermediate-ATA creation skips `base.mint` instead of hardcoded `WSOL_PUBKEY`.

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)]` module at the bottom of `src/arbitrage/evaluator.rs`:

```rust
#[test]
fn setup_native_wraps_and_teardown_closes() {
    use crate::dex::types::{resolve_base_token, WSOL_MINT};
    let user = solana_sdk::pubkey::Pubkey::new_unique();
    let mint_x = solana_sdk::pubkey::Pubkey::new_unique();
    let sol = resolve_base_token(WSOL_MINT).unwrap();
    let path = vec![sol.mint, mint_x, sol.mint];

    let setup = super::build_setup_instructions(user, 1_000_000, &path, &sol);
    // native: must contain a system transfer (wrap) and a sync_native
    let has_transfer = setup.iter().any(|ix| ix.program_id == solana_sdk::system_program::id());
    let has_token_ix = setup.iter().any(|ix| ix.program_id == spl_token::id());
    assert!(has_transfer, "native setup must fund the WSOL ATA");
    assert!(has_token_ix, "native setup must include token-program ix (ATA/sync_native)");

    let teardown = super::build_teardown_instructions(user, &sol);
    assert_eq!(teardown.len(), 1, "native teardown closes the WSOL ATA");
}

#[test]
fn setup_spl_base_does_not_wrap_and_teardown_empty() {
    use crate::dex::types::{resolve_base_token, USDC_MINT};
    let user = solana_sdk::pubkey::Pubkey::new_unique();
    let mint_x = solana_sdk::pubkey::Pubkey::new_unique();
    let usdc = resolve_base_token(USDC_MINT).unwrap();
    let path = vec![usdc.mint, mint_x, usdc.mint];

    let setup = super::build_setup_instructions(user, 1_000_000, &path, &usdc);
    // SPL base: NO system transfer (no wrap)
    let has_transfer = setup.iter().any(|ix| ix.program_id == solana_sdk::system_program::id());
    assert!(!has_transfer, "SPL base must not wrap (no system transfer)");

    let teardown = super::build_teardown_instructions(user, &usdc);
    assert!(teardown.is_empty(), "SPL base teardown is a no-op (keep USDC in the ATA)");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev -- setup_native setup_spl`
Expected: FAIL — `build_setup_instructions` arity mismatch (4 args expected, 3 defined).

- [ ] **Step 3: Write minimal implementation**

Replace `build_setup_instructions` (lines 450–480) and `build_teardown_instructions` (lines 484–490) with:

```rust
/// Build setup instructions for tx[0]. For a native base (WSOL): create intermediate +
/// WSOL ATAs, fund the WSOL ATA, sync_native. For an SPL base (e.g. USDC): create
/// intermediate + base ATAs only — the wallet's base ATA already holds the capital,
/// so there is no wrap step.
fn build_setup_instructions(user: Pubkey, amount_in: u64, path: &[Pubkey], base: &BaseToken) -> Vec<Instruction> {
    let base_ata = get_associated_token_address(&user, &base.mint);
    let mut ixs: Vec<Instruction> = Vec::new();

    // Create ATAs for all non-base intermediate mints (idempotent — no-op if exists)
    let mut seen = std::collections::HashSet::new();
    for &mint in path {
        if mint != base.mint && seen.insert(mint) {
            ixs.push(create_associated_token_account_idempotent(
                &user, &user, &mint, &spl_token::id(),
            ));
        }
    }

    // Create (or verify) the base ATA
    ixs.push(create_associated_token_account_idempotent(
        &user, &user, &base.mint, &spl_token::id(),
    ));

    if base.is_native {
        // Fund the WSOL ATA with the arb input amount, then sync so the token program
        // sees the deposited lamports as WSOL.
        ixs.push(system_instruction::transfer(&user, &base_ata, amount_in));
        ixs.push(
            spl_token::instruction::sync_native(&spl_token::id(), &base_ata)
                .expect("sync_native is always valid"),
        );
    }

    ixs
}

/// Teardown appended to the last swap tx. Native base: close the WSOL ATA (unwrap
/// principal+profit back to SOL). SPL base: no-op — principal+profit stay in the base ATA.
fn build_teardown_instructions(user: Pubkey, base: &BaseToken) -> Vec<Instruction> {
    if !base.is_native {
        return Vec::new();
    }
    let base_ata = get_associated_token_address(&user, &base.mint);
    vec![
        spl_token::instruction::close_account(&spl_token::id(), &base_ata, &user, &user, &[])
            .expect("close_account is always valid"),
    ]
}
```

Update the wallet-funded callers (lines 422–423) to pass the base token (the function already has `config` in scope):

```rust
            let setup = build_setup_instructions(user, amount_in, &cycle.path, &config.base_token);
            let teardown = build_teardown_instructions(user, &config.base_token);
```

Ensure `BaseToken` is imported in `evaluator.rs` (add to the existing `use crate::dex::types::...` line if not present):

```rust
use crate::dex::types::BaseToken;
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --bin solana-mev -- setup_native setup_spl`
Expected: PASS (2 tests).

- [ ] **Step 5: Confirm the SOL path is unchanged (regression)**

Run: `cargo test --bin solana-mev evaluator`
Expected: PASS — all pre-existing evaluator tests (including the `compute_jito_tip` suite) still pass.

- [ ] **Step 6: Commit**

```bash
git add src/arbitrage/evaluator.rs
git commit -m "feat(arb): is_native-aware setup/teardown (USDC funds from ATA, no wrap)"
```

---

## Task 5: Wire tip conversion + publish SOL price

**Files:**
- Modify: `src/arbitrage/evaluator.rs` — the two `compute_jito_tip` call sites (line 247, line 925)
- Modify: `src/portfolio/watcher.rs` — publish SOL price each tick (after the price merge, ~line 460)

**Interfaces:**
- Consumes: `crate::arbitrage::sol_price::{gross_profit_for_tip, get_fresh, PRICE_MAX_AGE_SECS}` (Task 2), `config.base_token` (Task 3)
- Produces: no new public API; behavior change only (tip sizing respects base token + cached price).

- [ ] **Step 1: Write the failing test**

The conversion math is already unit-tested in Task 2. Here, add a focused integration-style test in `src/arbitrage/evaluator.rs`'s test module proving the call-site decision matches the helper (guards against future drift between the two sites):

```rust
#[test]
fn tip_input_uses_gross_for_tip_helper() {
    use crate::arbitrage::sol_price::gross_profit_for_tip;
    use crate::dex::types::{resolve_base_token, USDC_MINT, WSOL_MINT};

    let sol = resolve_base_token(WSOL_MINT).unwrap();
    let usdc = resolve_base_token(USDC_MINT).unwrap();

    // Native: tip input equals raw gross.
    assert_eq!(gross_profit_for_tip(400_000, &sol, Some(200.0)), 400_000);
    // USDC at $200/SOL: 10 USDC → 0.05 SOL → 50_000_000 lamports tip input.
    assert_eq!(gross_profit_for_tip(10_000_000, &usdc, Some(200.0)), 50_000_000);
    // USDC stale price → 0 → floor tip path.
    assert_eq!(gross_profit_for_tip(10_000_000, &usdc, None), 0);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev tip_input_uses_gross_for_tip_helper`
Expected: FAIL — unresolved import `crate::arbitrage::sol_price::gross_profit_for_tip` only if Task 2 not built; otherwise it passes trivially. If it passes here, that's acceptable — the real change is the wiring in Step 3, verified by the regression run in Step 4.

- [ ] **Step 3: Write the wiring**

At call site **line 247**, replace:

```rust
        let tip = compute_jito_tip(gross_profit as u64, config, tip_floor);
```

with:

```rust
        let gross_for_tip = crate::arbitrage::sol_price::gross_profit_for_tip(
            gross_profit as u64,
            &config.base_token,
            crate::arbitrage::sol_price::get_fresh(crate::arbitrage::sol_price::PRICE_MAX_AGE_SECS),
        );
        let tip = compute_jito_tip(gross_for_tip, config, tip_floor);
```

At call site **line 925**, replace:

```rust
        compute_jito_tip(gross_profit as u64, config, tip_floor)
```

with:

```rust
        {
            let gross_for_tip = crate::arbitrage::sol_price::gross_profit_for_tip(
                gross_profit as u64,
                &config.base_token,
                crate::arbitrage::sol_price::get_fresh(crate::arbitrage::sol_price::PRICE_MAX_AGE_SECS),
            );
            compute_jito_tip(gross_for_tip, config, tip_floor)
        }
```

In `src/portfolio/watcher.rs`, immediately after the price merge (after `prices.extend(fresh);` at ~line 460), add:

```rust
        // Feed the arbitrage hot loop's SOL/USD price cache (used to size SOL-denominated
        // Jito tips when the arb base token is non-native).
        if let Some(&sol_usd) = prices.get("SOL") {
            if sol_usd > 0.0 {
                crate::arbitrage::sol_price::publish(sol_usd);
            }
        }
```

- [ ] **Step 4: Run tests + regression**

Run: `cargo test --bin solana-mev evaluator`
Expected: PASS — existing `compute_jito_tip` tests unaffected (native base → identity conversion), new test passes.

- [ ] **Step 5: Build the whole binary**

Run: `cargo build --bin solana-mev`
Expected: compiles.

- [ ] **Step 6: Commit**

```bash
git add src/arbitrage/evaluator.rs src/portfolio/watcher.rs
git commit -m "feat(arb): size SOL tip from base profit via price cache; watcher publishes SOL/USD"
```

---

## Task 6: Cycle source + startup base logging (main.rs)

**Files:**
- Modify: `src/main.rs` — line 552–553 (cycle source + log_rates), line 897 (Bellman-Ford source), import line 34

**Interfaces:**
- Consumes: `config.base_token` (Task 3)
- Produces: cycle enumeration now starts/ends at the configured base mint; startup logs the active base symbol/decimals.

- [ ] **Step 1: Make the change**

Replace lines 552–553:

```rust
    let sol_mint = Pubkey::from_str(WSOL_MINT)?;
    graph.log_rates(&sol_mint);
```

with:

```rust
    let base_mint = config.base_token.mint;
    info!(
        "Arbitrage base token: {} ({}, {} decimals, native={})",
        config.base_token.symbol, base_mint, config.base_token.decimals, config.base_token.is_native,
    );
    graph.log_rates(&base_mint);
```

Replace line 897:

```rust
                let search = bellman_ford::find_negative_cycles_with_diag(&graph_bf, sol_mint);
```

with:

```rust
                let search = bellman_ford::find_negative_cycles_with_diag(&graph_bf, base_mint);
```

> Note: `base_mint` is captured by the spawned Bellman-Ford task. `Pubkey` is `Copy`, so `move` closures copy it; if the closure capturing the loop is non-`move`, copy it into a local (`let base_mint = base_mint;`) before the spawn, mirroring how `sol_mint` was previously captured. Keep the existing capture style.

If `WSOL_MINT` (and `Pubkey::from_str` / `use std::str::FromStr`) become unused after this change, remove them from the imports to avoid `unused_import` warnings — but only if the compiler flags them (`mint_symbol` and `Pool` from the same `use` line stay).

- [ ] **Step 2: Build**

Run: `cargo build --bin solana-mev`
Expected: compiles with no `unused`/`unresolved` warnings related to this change.

- [ ] **Step 3: Commit**

```bash
git add src/main.rs
git commit -m "feat(arb): enumerate cycles from configured base token; log active base at startup"
```

---

## Task 7: Dual-guard halt + base-balance capital cap (main.rs)

**Files:**
- Create: `src/arbitrage/capital.rs`
- Modify: `src/arbitrage/mod.rs` (`pub mod capital;`)
- Modify: `src/main.rs` — startup balance (~line 516), balance cache + halt loop (lines 616–669), wallet-funded capital cap (lines 960–977)
- Test: inline `#[cfg(test)]` in `src/arbitrage/capital.rs`

**Interfaces:**
- Consumes: nothing from other tasks (pure helpers); main.rs consumes `config.base_token` + `config.min_sol_gas_lamports`
- Produces:
  - `pub enum HaltDecision { Continue, WarnPnl, HaltGas }`
  - `pub fn spendable_base(balance: u64, overhead: u64, input_cap: u64) -> u64`
  - `pub fn evaluate_halt(b_base: u64, pnl_threshold: u64, b_sol: u64, gas_floor: u64, is_native: bool) -> HaltDecision`

- [ ] **Step 1: Write the failing test**

Create `src/arbitrage/capital.rs` with the test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spendable_subtracts_overhead_then_caps() {
        // 10 capital, 2 overhead, cap 100 → 8
        assert_eq!(spendable_base(10, 2, 100), 8);
        // cap binds
        assert_eq!(spendable_base(1_000, 0, 50), 50);
        // overhead exceeds balance → 0
        assert_eq!(spendable_base(1, 5, 100), 0);
    }

    #[test]
    fn halt_gas_only_for_non_native() {
        // non-native, SOL below gas floor → HaltGas regardless of base balance
        assert_eq!(evaluate_halt(1_000, 0, 10, 100, false), HaltDecision::HaltGas);
        // native: gas floor not separately enforced
        assert_eq!(evaluate_halt(1_000, 2_000, 10, 100, true), HaltDecision::WarnPnl);
    }

    #[test]
    fn halt_pnl_when_base_below_threshold() {
        assert_eq!(evaluate_halt(50, 100, 1_000, 100, false), HaltDecision::WarnPnl);
        assert_eq!(evaluate_halt(150, 100, 1_000, 100, false), HaltDecision::Continue);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev capital`
Expected: FAIL — module not declared / fns missing.

- [ ] **Step 3: Write the implementation**

Prepend to `src/arbitrage/capital.rs`:

```rust
//! Pure wallet-capital accounting for the arbitrage loop: how much base-token capital is
//! spendable, and when to halt. Native and non-native bases differ — native pays gas from
//! the same balance it trades; a non-native base trades USDC but still pays SOL gas.

#[derive(Debug, PartialEq, Eq)]
pub enum HaltDecision {
    Continue,
    /// Base-token P&L dropped below the drawdown threshold (caller debounces to a halt).
    WarnPnl,
    /// Native SOL gas balance exhausted — cannot pay tips/fees (immediate halt).
    HaltGas,
}

/// Spendable base capital after reserving `overhead` and applying `input_cap`.
pub fn spendable_base(balance: u64, overhead: u64, input_cap: u64) -> u64 {
    balance.saturating_sub(overhead).min(input_cap)
}

/// Decide halt state from the latest balances. Gas guard applies only to a non-native
/// base (a native base's gas == its base balance, covered by the P&L guard).
pub fn evaluate_halt(b_base: u64, pnl_threshold: u64, b_sol: u64, gas_floor: u64, is_native: bool) -> HaltDecision {
    if !is_native && b_sol < gas_floor {
        return HaltDecision::HaltGas;
    }
    if b_base < pnl_threshold {
        return HaltDecision::WarnPnl;
    }
    HaltDecision::Continue
}
```

Add to `src/arbitrage/mod.rs`:

```rust
pub mod capital;
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --bin solana-mev capital`
Expected: PASS (3 tests).

- [ ] **Step 5: Wire helpers into main.rs**

(a) After the native `start_balance` fetch (~line 532), add the startup base-token balance (used as the P&L baseline). For a native base it equals `start_balance`:

```rust
    // P&L baseline in base-token units. Native base: same as the SOL balance above.
    // SPL base: the wallet's base-token ATA balance at startup.
    let start_base_balance: u64 = if config.base_token.is_native {
        start_balance
    } else {
        let base_ata = spl_associated_token_account::get_associated_token_address(&user, &config.base_token.mint);
        match rpc.get_token_account_balance(&base_ata).await {
            Ok(ui) => ui.amount.parse::<u64>().unwrap_or(0),
            Err(e) => { warn!("Could not fetch base-token ({}) balance: {e}", config.base_token.symbol); 0 }
        }
    };
```

> The crate already constructs ATAs via `get_associated_token_address`; reuse whatever path the file already imports (in `main.rs` it may be `spl_associated_token_account::get_associated_token_address`). Match the existing import.

(b) Replace the balance-cache/halt loop body (lines 633–668) so it stores the **base** balance into `cached_balance` and applies the dual guard. Capture the needed config values before the spawn:

```rust
        let base_is_native = config.base_token.is_native;
        let base_mint_for_cache = config.base_token.mint;
        let base_symbol = config.base_token.symbol;
        let gas_floor = config.min_sol_gas_lamports;
        tokio::spawn(async move {
            let mut below_start_count = 0u32;
            // Overhead is SOL-rent for the native wrap path; an SPL base reserves nothing
            // from its trading capital (gas comes from the separate SOL balance).
            let base_overhead = if base_is_native { BALANCE_OVERHEAD_LAMPORTS } else { 0 };
            loop {
                // Native SOL balance — always needed (gas guard + native P&L).
                let b_sol = rpc.get_balance(&wallet).await.unwrap_or_else(|e| {
                    warn!("Balance cache refresh failed: {e}"); 0
                });
                // Base-token capital balance.
                let b_base = if base_is_native {
                    b_sol
                } else {
                    let ata = spl_associated_token_account::get_associated_token_address(&wallet, &base_mint_for_cache);
                    rpc.get_token_account_balance(&ata).await
                        .ok().and_then(|ui| ui.amount.parse::<u64>().ok()).unwrap_or(0)
                };
                // Publish spendable capital for the hot loop's amount_in cap.
                cache.store(b_base, Ordering::Relaxed);

                if !dry_run && start_base_balance > 0 {
                    let pnl_threshold = start_base_balance.saturating_sub(base_overhead);
                    match crate::arbitrage::capital::evaluate_halt(
                        b_base, pnl_threshold, b_sol, gas_floor, base_is_native,
                    ) {
                        crate::arbitrage::capital::HaltDecision::HaltGas => {
                            error!(
                                "HALT: native SOL {:.6} below gas floor {:.6} — cannot pay tips/fees.",
                                b_sol as f64 / 1e9, gas_floor as f64 / 1e9,
                            );
                            std::process::exit(1);
                        }
                        crate::arbitrage::capital::HaltDecision::WarnPnl => {
                            below_start_count += 1;
                            if below_start_count >= 2 {
                                error!(
                                    "HALT: base {} {} below threshold {} (start {}) — stopping to prevent further losses.",
                                    base_symbol, b_base, pnl_threshold, start_base_balance,
                                );
                                std::process::exit(1);
                            }
                            warn!(
                                "Base {} balance {} below P&L threshold {} (start {}) — will halt if still low next poll",
                                base_symbol, b_base, pnl_threshold, start_base_balance,
                            );
                        }
                        crate::arbitrage::capital::HaltDecision::Continue => {
                            below_start_count = 0;
                        }
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
```

> You will need `start_base_balance` moved into the closure — add `let start_base_balance = start_base_balance;` to the capture block alongside the existing `let wallet = user;` etc. Remove the now-unused `start_balance`-based `halt_threshold` logic that this block replaces. Keep the existing `cached_balance`/`rpc`/`cache`/`wallet`/`dry_run` clones above the spawn.

(c) Update the wallet-funded capital cap (lines 967–976) so overhead is base-aware and the cap is in base units:

```rust
                    let wallet_balance = balance_bf.load(Ordering::Relaxed);
                    let base_overhead = if config_bf.base_token.is_native { BALANCE_OVERHEAD_LAMPORTS } else { 0 };
                    let spendable = crate::arbitrage::capital::spendable_base(
                        wallet_balance, base_overhead, config_bf.input_sol_lamports,
                    );
                    if spendable == 0 {
                        debug!("Base-token balance ({wallet_balance}) too low for overhead reserve — skipping");
                        in_flight_bf.store(false, Ordering::Release);
                        continue;
                    }
                    spendable
```

- [ ] **Step 6: Build**

Run: `cargo build --bin solana-mev`
Expected: compiles. Resolve any import path for `get_token_account_balance` (it's on `RpcClient` / the nonblocking client already used as `rpc`) and `get_associated_token_address` to match existing usage in `main.rs`.

- [ ] **Step 7: Regression test**

Run: `cargo test --bin solana-mev`
Expected: PASS — full suite. The native path is unchanged (overhead + thresholds identical when `is_native`).

- [ ] **Step 8: Commit**

```bash
git add src/arbitrage/capital.rs src/arbitrage/mod.rs src/main.rs
git commit -m "feat(arb): dual-guard halt (base P&L + SOL gas) and base-unit capital cap"
```

---

## Task 8: Document config + final sweep

**Files:**
- Modify: `.env.example`
- Verify: whole crate

**Interfaces:** none (docs + verification).

- [ ] **Step 1: Document the new env vars**

Add a section to `.env.example` near the existing `MIN_PROFIT_LAMPORTS` / `INPUT_SOL_LAMPORTS` entries (lines ~8–9):

```bash
# ── Base token ────────────────────────────────────────────────────────────────
# Starting/closing token of every arbitrage cycle. Default = wrapped SOL.
# Supported: SOL (So11111111111111111111111111111111111111112)
#            USDC (EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v)
# When set to a non-native base (USDC), flash loan is force-disabled (wallet-funded
# only) and the wallet must hold USDC capital AND SOL for gas/tips.
# BASE_MINT=So11111111111111111111111111111111111111112

# Thresholds are in the BASE TOKEN's smallest unit (SOL=9 decimals, USDC=6 decimals).
# The historical SOL-named vars still work; the *_BASE_UNITS aliases are base-neutral.
MIN_PROFIT_LAMPORTS=10000          # alias: MIN_PROFIT_BASE_UNITS
INPUT_SOL_LAMPORTS=100000000       # alias: INPUT_BASE_UNITS
# USDC examples (6 decimals): MIN_PROFIT_BASE_UNITS=20000 (0.02 USDC), INPUT_BASE_UNITS=100000000 (100 USDC)

# Halt if native SOL falls below this floor (can't pay tips/fees). Enforced only when
# BASE_MINT is non-native; for a SOL base the P&L guard already covers gas. Default 0.1 SOL.
MIN_SOL_GAS_LAMPORTS=100000000
```

- [ ] **Step 2: Full build, lint, test**

Run:
```bash
cargo build --bin solana-mev
cargo clippy --bin solana-mev 2>&1 | tail -30
cargo test --bin solana-mev
```
Expected: build OK; no NEW clippy warnings attributable to the new code; all tests pass.

- [ ] **Step 3: Sanity-check both config paths load**

Run (proves the resolver + force-disable path works end to end without starting the bot):
```bash
BASE_MINT=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v ENABLE_FLASH_LOAN=true CHECK_POOLS=true \
  cargo run --bin solana-mev 2>&1 | grep -i "base token\|ignored\|flash"
```
Expected: logs `Arbitrage base token: USDC ...` and the `ENABLE_FLASH_LOAN=true ignored` warning. (CHECK_POOLS short-circuits before the gRPC stream.)

- [ ] **Step 4: Commit**

```bash
git add .env.example
git commit -m "docs(env): document BASE_MINT, base-unit thresholds, MIN_SOL_GAS_LAMPORTS"
```

---

## Self-Review (completed during plan authoring)

**Spec coverage:**
- §1 BaseToken/config → Tasks 1, 3 ✓
- §2 Funding native vs SPL → Task 4 ✓
- §3 Tip sizing + price cache → Tasks 2, 5 ✓
- §4 Dual-guard halt → Task 7 ✓
- §5 Cycle source + thresholds + display → Tasks 6 (source/display), 3 (thresholds/aliases) ✓
- §6 Out-of-scope honored (no USDC flash loan; tips still SOL); testing per task; operational prerequisites are documented, not coded ✓

**Type consistency:** `BaseToken` fields (`mint`, `decimals`, `symbol`, `is_native`) used identically across Tasks 1/3/4/5/6/7. `gross_profit_for_tip`/`get_fresh`/`PRICE_MAX_AGE_SECS` signatures match between Task 2 (def) and Task 5 (use). `spendable_base`/`evaluate_halt`/`HaltDecision` match between Task 7 def and main.rs use.

**Placeholder scan:** every code step contains concrete code; no TBD/TODO. Import-path caveats (`get_token_account_balance`, `get_associated_token_address`) are flagged with the rule "match existing usage in the file" rather than left vague.
