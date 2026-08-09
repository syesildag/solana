# Auto-Adopt Unwatched Wallet Tokens (Trail-Only) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A second, `MOMENTUM_ADOPT_ALL_TOKENS`-gated (default `false`) adoption pass that adopts unwatched wallet SPL tokens into free momentum slots and manages them with a trailing stop only (no fade exit, no rotation eviction; stagnation eviction allowed).

**Architecture:** Approach B from the spec (`docs/superpowers/specs/2026-08-09-adopt-all-tokens-design.md`): a separate `adopt_unwatched_holdings` async function in `src/portfolio/momentum.rs`, called right after the existing watched adoption at both watcher call sites. Selection is a pure, unit-tested function mirroring `choose_adoption`. Positions carry a new `adopted_unwatched: bool` flag (serde-default false) that gates the exit paths.

**Tech Stack:** Rust (tokio, serde, reqwest), existing helpers: `scanner::fetch_token_balance_raw`, `scanner::load_pubkey`, `jupiter::quote`, `momentum_state`.

## Global Constraints

- `MOMENTUM_ADOPT_ALL_TOKENS` defaults to **false**; an unset `.env` must behave byte-identically to today.
- Built-in, non-configurable exclusions: WSOL `So11111111111111111111111111111111111111112`, USDC `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`, USDT `Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB`. (Native SOL never appears in `portfolio.tokens` — it lives in `sol_amount`.)
- NEVER run `cargo fmt` or whole-file rustfmt (repo is not rustfmt-clean; user rule).
- All tests live in `#[cfg(test)]` blocks at the bottom of each source file (repo convention).
- Test command: `cargo test --bin solana-mev <filter>`.
- Raw token amounts for Jupiter quotes come from `scanner::fetch_token_balance_raw` — never `ui_amount × 10^decimals` (Token-2022 scaledUiAmount rule).

---

### Task 1: Config knobs

**Files:**
- Modify: `src/portfolio/mod.rs` (struct `PortfolioConfig` ~line 161 area; `from_env` literal ~line 496-510)
- Test: bottom of `src/portfolio/mod.rs`

**Interfaces:**
- Produces: `cfg.momentum_adopt_all_tokens: bool`, `cfg.momentum_adopt_exclude_mints: Vec<String>`, `cfg.momentum_adopt_trail_pct: f64`, and `pub(crate) fn parse_csv_list(raw: &str) -> Vec<String>`.

- [ ] **Step 1: Write the failing test** (bottom of `src/portfolio/mod.rs`, inside the existing `#[cfg(test)] mod tests`):

```rust
#[test]
fn parse_csv_list_trims_and_drops_empties() {
    assert_eq!(
        parse_csv_list(" mintA , mintB ,, "),
        vec!["mintA".to_string(), "mintB".to_string()]
    );
    assert!(parse_csv_list("").is_empty());
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev parse_csv_list -- --nocapture`
Expected: FAIL — `parse_csv_list` not found.

- [ ] **Step 3: Implement.** Add near `parse_price_thresholds` in `src/portfolio/mod.rs`:

```rust
/// Parse a comma-separated list ("a, b,c") into trimmed non-empty strings.
pub(crate) fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}
```

Add fields to `PortfolioConfig` next to `momentum_adopt_wallet_position` (~line 161), with doc comments:

```rust
/// Master gate for the unwatched-adoption pass (spec 2026-08-09). Default false —
/// an unset .env behaves byte-identically to before the feature existed.
pub momentum_adopt_all_tokens: bool,
/// Operator exclusions on TOP of the built-in set (WSOL/USDC/USDT). Mints.
pub momentum_adopt_exclude_mints: Vec<String>,
/// Trail width for adopted-unwatched positions. Defaults to MOMENTUM_TRAIL_PCT.
pub momentum_adopt_trail_pct: f64,
```

In `from_env`, hoist the existing trail parse above the struct literal so it can be the default for the new knob, then use both:

```rust
let momentum_trail_pct = parse_env("MOMENTUM_TRAIL_PCT", 5.0_f64)?;
```

and inside the literal replace the old line with / add:

```rust
momentum_trail_pct,
momentum_adopt_all_tokens: parse_env("MOMENTUM_ADOPT_ALL_TOKENS", false)?,
momentum_adopt_exclude_mints: parse_csv_list(
    &std::env::var("MOMENTUM_ADOPT_EXCLUDE_MINTS").unwrap_or_default(),
),
momentum_adopt_trail_pct: parse_env("MOMENTUM_ADOPT_TRAIL_PCT", momentum_trail_pct)?,
```

- [ ] **Step 4: Run tests**

Run: `cargo test --bin solana-mev parse_csv_list`
Expected: PASS. Also `cargo build --release 2>&1 | head -30` — expect errors ONLY where `PortfolioConfig` literals exist outside `from_env` (tests/sim); add the three fields there with defaults (`false`, `vec![]`, and the literal's trail value).

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/mod.rs
git commit -m "feat: MOMENTUM_ADOPT_ALL_TOKENS config knobs (default off)"
```

---

### Task 2: Position flag + audit field

**Files:**
- Modify: `src/portfolio/momentum_state.rs` (struct `Position`, ~line 20)
- Modify: `src/portfolio/momentum_actions.rs` (`ActionKind::Adopted`, ~line 36)
- Modify: every `Position { … }` literal the compiler flags (at minimum `momentum.rs` `adopt_wallet_position` ~line 1785 and `try_open_position`) — add `adopted_unwatched: false`.
- Test: bottom of `src/portfolio/momentum_state.rs`

**Interfaces:**
- Produces: `Position.adopted_unwatched: bool` (serde-default), `ActionKind::Adopted { …, unwatched: bool }` (serde-default).

- [ ] **Step 1: Write the failing tests** (in `momentum_state.rs` tests, following the existing legacy-JSON pattern at ~line 348):

```rust
#[test]
fn adopted_unwatched_defaults_false_on_legacy_state() {
    let legacy = r#"{"position":null,"last_exit_ts_per_mint":{},"trades":[]}"#;
    // Reuse the existing legacy-load pattern in this module to obtain a state,
    // then push a position WITHOUT the field via JSON:
    let pos_json = r#"{
        "mint":"M","symbol":"S","entry_ts":"2026-08-09T00:00:00Z",
        "entry_price_usd":1.0,"token_amount":1.0,"usdc_spent":1.0,
        "peak_price_usd":1.0
    }"#;
    let pos: Position = serde_json::from_str(pos_json).unwrap();
    assert!(!pos.adopted_unwatched);
    let _ = legacy; // keep the doc-example visible
}

#[test]
fn adopted_unwatched_round_trips() {
    let mut pos: Position = serde_json::from_str(r#"{
        "mint":"M","symbol":"S","entry_ts":"2026-08-09T00:00:00Z",
        "entry_price_usd":1.0,"token_amount":1.0,"usdc_spent":1.0,
        "peak_price_usd":1.0
    }"#).unwrap();
    pos.adopted_unwatched = true;
    let s = serde_json::to_string(&pos).unwrap();
    let back: Position = serde_json::from_str(&s).unwrap();
    assert!(back.adopted_unwatched);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --bin solana-mev adopted_unwatched`
Expected: FAIL — no field `adopted_unwatched`.

- [ ] **Step 3: Implement.** In `Position` (after `entry_sig`/`dry_run` group):

```rust
/// True for a position auto-adopted from an UNWATCHED wallet holding
/// (MOMENTUM_ADOPT_ALL_TOKENS, spec 2026-08-09). Gates the exits: trail at
/// MOMENTUM_ADOPT_TRAIL_PCT, fade exit and rotation eviction skipped,
/// stagnation eviction allowed. `serde(default)` = false so pre-upgrade
/// state files never re-classify an existing position.
#[serde(default)]
pub adopted_unwatched: bool,
```

In `ActionKind::Adopted` add:

```rust
/// True when the adoption came from the unwatched-holdings pass.
#[serde(default)]
unwatched: bool,
```

Fix every `Position { … }` and `ActionKind::Adopted { … }` literal the compiler flags: `adopted_unwatched: false` / `unwatched: false` (the watched pass keeps `false`).

- [ ] **Step 4: Run tests**

Run: `cargo test --bin solana-mev adopted_unwatched && cargo test --bin solana-mev momentum_state`
Expected: PASS, including the existing RFC3339 state tests.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum_state.rs src/portfolio/momentum_actions.rs src/portfolio/momentum.rs
git commit -m "feat: adopted_unwatched flag on Position and Adopted audit record"
```

---

### Task 3: Pure selection function

**Files:**
- Modify: `src/portfolio/momentum.rs` (next to `choose_adoption`, ~line 1665)
- Test: bottom of `src/portfolio/momentum.rs`

**Interfaces:**
- Consumes: `TokenEntry` (`crate::portfolio::TokenEntry`: `mint`, `symbol`, `amount`), `AdoptCandidate` (same file).
- Produces:

```rust
pub const ADOPT_ALWAYS_EXCLUDED: [&str; 3] = [
    "So11111111111111111111111111111111111111112",                 // WSOL
    crate::portfolio::momentum_universe::USDC_MINT,                 // USDC
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",                 // USDT
];

/// Pure selection for the unwatched-adoption pass (spec 2026-08-09).
/// Returns candidates sorted USD-value descending, truncated to `cap`.
pub fn choose_unwatched_adoption(
    wallet: &[crate::portfolio::TokenEntry],
    prices: &std::collections::HashMap<String, f64>,
    watched_mints: &std::collections::HashSet<String>,
    held_mints: &std::collections::HashSet<String>,
    extra_excluded: &[String],
    last_exit_ts_per_mint: &std::collections::HashMap<String, i64>,
    now: i64,
    cooldown_secs: i64,
    min_usd: f64,
    cap: usize,
) -> Vec<AdoptCandidate>
```

- [ ] **Step 1: Write the failing tests** (bottom of `momentum.rs`, existing tests module; use small helpers):

```rust
fn te(mint: &str, amount: f64) -> crate::portfolio::TokenEntry {
    crate::portfolio::TokenEntry { mint: mint.into(), symbol: mint.into(), amount }
}

#[test]
fn unwatched_adoption_excludes_builtins_watched_held_and_configured() {
    use std::collections::{HashMap, HashSet};
    let wallet = vec![
        te("So11111111111111111111111111111111111111112", 5.0), // WSOL: built-in
        te(crate::portfolio::momentum_universe::USDC_MINT, 100.0), // USDC: built-in
        te("WATCHED", 100.0),
        te("HELD", 100.0),
        te("CFG_EXCLUDED", 100.0),
        te("GOOD", 100.0),
    ];
    let prices: HashMap<String, f64> =
        wallet.iter().map(|t| (t.mint.clone(), 1.0)).collect();
    let watched: HashSet<String> = ["WATCHED".to_string()].into();
    let held: HashSet<String> = ["HELD".to_string()].into();
    let got = choose_unwatched_adoption(
        &wallet, &prices, &watched, &held,
        &["CFG_EXCLUDED".to_string()],
        &HashMap::new(), 1_000, 3_600, 5.0, 8,
    );
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].mint, "GOOD");
}

#[test]
fn unwatched_adoption_floor_cooldown_order_and_cap() {
    use std::collections::{HashMap, HashSet};
    let wallet = vec![
        te("DUST", 1.0),      // $1 < $5 floor
        te("COOLING", 100.0), // exited 100s ago, cooldown 3600
        te("SMALL", 10.0),
        te("BIG", 500.0),
        te("MID", 50.0),
    ];
    let prices: HashMap<String, f64> =
        wallet.iter().map(|t| (t.mint.clone(), 1.0)).collect();
    let mut last_exit = HashMap::new();
    last_exit.insert("COOLING".to_string(), 900_i64);
    let got = choose_unwatched_adoption(
        &wallet, &prices, &HashSet::new(), &HashSet::new(), &[],
        &last_exit, 1_000, 3_600, 5.0, 2,
    );
    // BIG, MID (USD desc), capped at 2; DUST under floor; COOLING inside window.
    let mints: Vec<&str> = got.iter().map(|c| c.mint.as_str()).collect();
    assert_eq!(mints, vec!["BIG", "MID"]);
}

#[test]
fn unwatched_adoption_skips_zero_amount_and_missing_price() {
    use std::collections::{HashMap, HashSet};
    let wallet = vec![te("ZERO", 0.0), te("NOPRICE", 10.0)];
    let mut prices = HashMap::new();
    prices.insert("ZERO".to_string(), 1.0);
    // NOPRICE deliberately absent from the map.
    let got = choose_unwatched_adoption(
        &wallet, &prices, &HashSet::new(), &HashSet::new(), &[],
        &HashMap::new(), 1_000, 3_600, 5.0, 8,
    );
    assert!(got.is_empty());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --bin solana-mev unwatched_adoption`
Expected: FAIL — function not found.

- [ ] **Step 3: Implement** (next to `choose_adoption`):

```rust
pub fn choose_unwatched_adoption(
    wallet: &[crate::portfolio::TokenEntry],
    prices: &std::collections::HashMap<String, f64>,
    watched_mints: &std::collections::HashSet<String>,
    held_mints: &std::collections::HashSet<String>,
    extra_excluded: &[String],
    last_exit_ts_per_mint: &std::collections::HashMap<String, i64>,
    now: i64,
    cooldown_secs: i64,
    min_usd: f64,
    cap: usize,
) -> Vec<AdoptCandidate> {
    let mut cands: Vec<AdoptCandidate> = Vec::new();
    for t in wallet {
        if t.amount <= 0.0
            || ADOPT_ALWAYS_EXCLUDED.contains(&t.mint.as_str())
            || extra_excluded.iter().any(|m| m == &t.mint)
            || watched_mints.contains(&t.mint)
            || held_mints.contains(&t.mint)
        {
            continue;
        }
        if let Some(exit_ts) = last_exit_ts_per_mint.get(&t.mint) {
            if now - exit_ts < cooldown_secs {
                continue; // adopt → stop-out → re-adopt churn guard
            }
        }
        let Some(price) = prices.get(&t.mint).copied().filter(|p| *p > 0.0) else {
            continue;
        };
        if t.amount * price < min_usd {
            continue;
        }
        cands.push(AdoptCandidate {
            mint: t.mint.clone(),
            symbol: t.symbol.clone(),
            amount: t.amount,
            price_usd: price,
        });
    }
    cands.sort_by(|a, b| {
        let (va, vb) = (a.amount * a.price_usd, b.amount * b.price_usd);
        vb.partial_cmp(&va).unwrap_or(std::cmp::Ordering::Equal)
    });
    cands.truncate(cap);
    cands
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --bin solana-mev unwatched_adoption`
Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum.rs
git commit -m "feat: pure selection for unwatched wallet adoption"
```

---

### Task 4: `adopt_unwatched_holdings` pass + watcher call sites

**Files:**
- Modify: `src/portfolio/momentum.rs` (new async fn after `adopt_wallet_position`, ~line 1818)
- Modify: `src/portfolio/watcher.rs` (startup call ~line 288; slow-tick Step 0 ~line 987)

**Interfaces:**
- Consumes: `choose_unwatched_adoption` (Task 3), `Position.adopted_unwatched` (Task 2), config knobs (Task 1), `scanner::load_pubkey(&cfg.wallet_keypair_path)`, `scanner::fetch_token_balance_raw(rpc_url, owner, mint) -> Result<u64>`, `jupiter::quote(http, base_url, input_mint, output_mint, amount_raw, slippage_bps)`, `momentum_universe::USDC_MINT`.
- Produces: `pub async fn adopt_unwatched_holdings(cfg, portfolio, prices, watched, http) -> bool`.

- [ ] **Step 1: Implement the function** (no direct unit test — the selection core is Task 3-tested; this is a thin I/O shell, validated by build + the paper-mode log lines in rollout):

```rust
/// Second adoption pass (spec 2026-08-09): adopt UNWATCHED wallet holdings into
/// free slots, gated by MOMENTUM_ADOPT_ALL_TOKENS (default false). Runs AFTER the
/// watched pass so curated tokens keep priority. Trail-only management — the
/// `adopted_unwatched` flag gates the exit paths.
///
/// Paper mode (`DRY_RUN_MOMENTUM_TRADER=true`): adoption is skipped (nothing
/// wallet-backed to adopt) but selection still LOGS "would adopt" lines so the
/// rollout can be validated before the live flip.
pub async fn adopt_unwatched_holdings(
    cfg: &PortfolioConfig,
    portfolio: &Portfolio,
    prices: &HashMap<String, f64>,
    watched: &[WatchedToken],
    http: &reqwest::Client,
) -> bool {
    if !cfg.enable_momentum_trader
        || !cfg.momentum_adopt_wallet_position
        || !cfg.momentum_adopt_all_tokens
    {
        return false;
    }
    let path = Path::new(&cfg.momentum_state_path);
    let state = match momentum_state::load(path) {
        Ok(s) => s,
        Err(e) => {
            warn!("momentum: could not load state for unwatched adoption: {e}");
            return false;
        }
    };
    let cap = state.capacity(cfg.momentum_max_positions);
    if cap == 0 {
        return false;
    }
    let watched_mints: std::collections::HashSet<String> =
        watched.iter().map(|w| w.mint.clone()).collect();
    let held_mints = state.held_mints();
    let now = now_ts();
    let cands = choose_unwatched_adoption(
        &portfolio.tokens,
        prices,
        &watched_mints,
        &held_mints,
        &cfg.momentum_adopt_exclude_mints,
        &state.last_exit_ts_per_mint,
        now,
        cfg.momentum_reentry_cooldown_secs,
        cfg.momentum_trade_usdc * 0.5,
        cap,
    );
    if cands.is_empty() {
        return false;
    }
    if cfg.momentum_dry_run {
        for c in &cands {
            info!(
                "momentum: would adopt UNWATCHED {} (paper) — {:.6} tokens @ ${:.6} (${:.2})",
                c.symbol, c.amount, c.price_usd, c.amount * c.price_usd
            );
        }
        return false;
    }
    let owner = match scanner::load_pubkey(&cfg.wallet_keypair_path) {
        Ok(p) => p.to_string(),
        Err(e) => {
            warn!("momentum: unwatched adoption — cannot load wallet pubkey: {e}");
            return false;
        }
    };
    // Re-load state mutably right before writing (same pattern as the watched pass).
    let mut state = match momentum_state::load(path) {
        Ok(s) => s,
        Err(e) => {
            warn!("momentum: could not reload state for unwatched adoption: {e}");
            return false;
        }
    };
    let mut adopted_any = false;
    for c in cands {
        if state.positions.iter().any(|p| p.mint == c.mint) {
            continue; // race-safe dedup
        }
        // Sellability gate: a Jupiter sell-quote for the FULL RAW balance must
        // succeed before the token may take a slot (unsellable airdrop guard).
        let raw = match scanner::fetch_token_balance_raw(&cfg.rpc_url, &owner, &c.mint).await {
            Ok(r) if r > 0 => r,
            Ok(_) => {
                info!("momentum: unwatched adoption skip {} — zero raw balance (stale scan)", c.symbol);
                continue;
            }
            Err(e) => {
                info!("momentum: unwatched adoption skip {} — raw balance fetch failed: {e}", c.symbol);
                continue;
            }
        };
        if let Err(e) = crate::portfolio::jupiter::quote(
            http,
            &cfg.momentum_jupiter_api_url,
            &c.mint,
            crate::portfolio::momentum_universe::USDC_MINT,
            raw,
            cfg.momentum_slippage_bps,
        )
        .await
        {
            info!("momentum: unwatched adoption skip {} — UNSELLABLE (quote failed: {e})", c.symbol);
            continue;
        }
        let ts = now_ts();
        let usdc_basis = c.amount * c.price_usd;
        state.positions.push(Position {
            mint: c.mint.clone(),
            symbol: c.symbol.clone(),
            entry_ts: ts,
            entry_price_usd: c.price_usd,
            token_amount: c.amount,
            usdc_spent: usdc_basis,
            peak_price_usd: c.price_usd,
            peak_ts: ts,
            topup_usdc: 0.0,
            entry_sig: "adopted-unwatched".to_string(),
            dry_run: false,
            adopted_unwatched: true,
        });
        audit(cfg, ts, ActionKind::Adopted {
            symbol: c.symbol.clone(),
            mint: c.mint.clone(),
            token_amount: c.amount,
            entry_price_usd: c.price_usd,
            unwatched: true,
        });
        info!(
            "momentum: ADOPTED unwatched holding {} — {:.6} tokens @ ${:.6} (basis ${:.2}); \
             trail-only at {:.1}% (no fade exit). Real cost basis unknown — PnL from adoption.",
            c.symbol, c.amount, c.price_usd, usdc_basis, cfg.momentum_adopt_trail_pct
        );
        adopted_any = true;
    }
    if adopted_any {
        if let Err(e) = momentum_state::save(path, &state) {
            warn!("momentum: failed to persist unwatched adoption(s): {e}");
            return false;
        }
    }
    adopted_any
}
```

Note: if `state.capacity(…)` / `state.held_mints()` require `&mut` or differ in name, mirror exactly what `adopt_wallet_position` calls at `momentum.rs:1721-1725`.

- [ ] **Step 2: Wire the watcher call sites.** In `src/portfolio/watcher.rs`:

Startup (~line 288), directly after the existing call:

```rust
if cfg.enable_momentum_trader {
    momentum::adopt_wallet_position(&cfg, &portfolio, &last_prices, &watched);
    momentum::adopt_unwatched_holdings(&cfg, &portfolio, &last_prices, &watched, &http).await;
}
```

Slow-tick Step 0 (~line 987), directly after the existing call:

```rust
momentum::adopt_wallet_position(&cfg, &portfolio, &prices, &watched);
momentum::adopt_unwatched_holdings(&cfg, &portfolio, &prices, &watched, &http).await;
```

(Both maps already contain unwatched wallet mints — `token_mints` is seeded from `portfolio.tokens` at `watcher.rs:203` and refreshed on every wallet re-scan at `watcher.rs:577`.)

- [ ] **Step 3: Build and test**

Run: `cargo build --release 2>&1 | tail -5 && cargo test --bin solana-mev momentum`
Expected: clean build, all momentum tests PASS.

- [ ] **Step 4: Commit**

```bash
git add src/portfolio/momentum.rs src/portfolio/watcher.rs
git commit -m "feat: adopt_unwatched_holdings pass behind MOMENTUM_ADOPT_ALL_TOKENS"
```

---

### Task 5: Exit-side flag handling

**Files:**
- Modify: `src/portfolio/momentum.rs`:
  - trail resolution in the exit loop (~line 3601)
  - `maybe_take_profit_on_fade` (~line 3325, guard at top)
  - `weakest_green` (~line 3144, skip flagged)
  - `weakest_stalled` (~line 3180, NO change — stagnation stays allowed; add a locking test)
- Test: bottom of `src/portfolio/momentum.rs`

**Interfaces:**
- Produces: `pub fn effective_trail_pct(adopted_unwatched: bool, watched: &[WatchedToken], mint: &str, global: f64, adopt_trail: f64) -> f64`.

- [ ] **Step 1: Write the failing tests** (reuse the existing `trail_for`-test fixtures at ~line 5201 for the `watched` vector shape; reuse existing `weakest_green` test fixtures for `Candidate`/`Position` construction, setting `adopted_unwatched` explicitly):

```rust
#[test]
fn effective_trail_uses_adopt_trail_only_when_flagged() {
    let watched: Vec<WatchedToken> = vec![]; // no overrides in play
    assert_eq!(effective_trail_pct(true, &watched, "X", 30.0, 12.0), 12.0);
    assert_eq!(effective_trail_pct(false, &watched, "X", 30.0, 12.0), 30.0);
}

#[test]
fn weakest_green_never_selects_adopted_unwatched() {
    // Build ONE green, rankable position exactly like the existing
    // weakest_green tests in this module, then set:
    //   pos.adopted_unwatched = true;
    // and assert weakest_green(&[pos], &ranked, &prices) == None.
    // With the flag false the same fixture must return Some(0) —
    // proving the flag (not the fixture) is what excludes it.
}

#[test]
fn weakest_stalled_still_selects_adopted_unwatched() {
    // Same fixture pattern as the existing weakest_stalled tests: a stalled,
    // rankable position with adopted_unwatched = true must still be returned
    // (stagnation eviction is allowed by the spec).
}
```

(Write the two eviction tests as real code by copying the nearest existing fixture in the tests module — the exact `Candidate` field set lives there; do not invent field names.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --bin solana-mev effective_trail -- --nocapture`
Expected: FAIL — function not found. (`weakest_green_never_selects…` fails to compile until the skip is added only if fixtures compile; fine either way.)

- [ ] **Step 3: Implement.**

New pure fn next to `trail_for` (~line 3413):

```rust
/// Trail width for a position: adopted-unwatched positions use the dedicated
/// MOMENTUM_ADOPT_TRAIL_PCT (no per-token params exist for them); everything
/// else keeps the per-token override → global fallback.
pub fn effective_trail_pct(
    adopted_unwatched: bool,
    watched: &[WatchedToken],
    mint: &str,
    global: f64,
    adopt_trail: f64,
) -> f64 {
    if adopted_unwatched { adopt_trail } else { trail_for(watched, mint, global) }
}
```

Replace the exit-loop line (~3601):

```rust
let trail_pct = effective_trail_pct(
    pos.adopted_unwatched,
    ctx.watched,
    &pos.mint,
    cfg.momentum_trail_pct,
    cfg.momentum_adopt_trail_pct,
);
```

Guard at the very top of `maybe_take_profit_on_fade` (before the `exit_on_fade_for` check at ~3334), returning the same "did not exit" value that check returns:

```rust
if pos.adopted_unwatched {
    return Ok(false); // trail/stagnation only for adopted-unwatched (spec 2026-08-09)
}
```

Skip in `weakest_green`'s loop, first line of the `for (idx, pos)` body:

```rust
if pos.adopted_unwatched {
    continue; // rotation-exempt: adopted-unwatched positions leave via trail/stagnation
}
```

`weakest_stalled`: no code change (test locks the behavior in).

- [ ] **Step 4: Run tests**

Run: `cargo test --bin solana-mev effective_trail && cargo test --bin solana-mev weakest_`
Expected: new tests PASS, existing `weakest_green`/`weakest_stalled` tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum.rs
git commit -m "feat: trail-only exit semantics for adopted-unwatched positions"
```

---

### Task 6: Docs + .env.example

**Files:**
- Modify: `.env.example` (momentum section)
- Modify: `CLAUDE.md` (one bullet in the momentum subsystem list)

- [ ] **Step 1: `.env.example`** — add next to the other MOMENTUM_ADOPT var:

```bash
# Auto-adopt UNWATCHED wallet tokens (spec 2026-08-09). Default false. Requires
# MOMENTUM_ADOPT_WALLET_POSITION=true. Trail-only management (no fade exit).
# Built-in exclusions: WSOL, USDC, USDT. Sellability-gated (Jupiter quote).
MOMENTUM_ADOPT_ALL_TOKENS=false
# Extra excluded mints, comma-separated (on top of the built-ins).
MOMENTUM_ADOPT_EXCLUDE_MINTS=
# Trail width for adopted-unwatched positions; unset = MOMENTUM_TRAIL_PCT.
MOMENTUM_ADOPT_TRAIL_PCT=
```

(If empty-string values would fail `parse_env`, leave the knobs as comments only — `#MOMENTUM_ADOPT_TRAIL_PCT=20` — matching how other optional knobs are documented in the file. Check the file's own convention first. Note `parse_env` treats an UNSET var as default but an empty string as a parse error, so the commented form is correct for `MOMENTUM_ADOPT_TRAIL_PCT`; `MOMENTUM_ADOPT_EXCLUDE_MINTS` uses plain `env::var` + `parse_csv_list`, where empty is fine.)

- [ ] **Step 2: `CLAUDE.md`** — add one bullet to the `src/portfolio/` subsystem list (after the stagnation-eviction bullet), ~5 lines:

```markdown
- **Unwatched-holdings adoption** (opt-in, `MOMENTUM_ADOPT_ALL_TOKENS`, default off;
  spec: `docs/superpowers/specs/2026-08-09-adopt-all-tokens-design.md`) — a second
  adoption pass adopts NON-curated wallet tokens (minus WSOL/USDC/USDT + configured
  excludes, Jupiter-sellability-gated) into free slots, trail-only at
  `MOMENTUM_ADOPT_TRAIL_PCT` (no fade exit, rotation-exempt; stagnation eviction
  applies). Paper mode logs "would adopt" lines only.
```

- [ ] **Step 3: Final verification**

Run: `cargo build --release && cargo test --bin solana-mev momentum && cargo clippy 2>&1 | grep -c warning`
Expected: build clean, tests PASS, no NEW clippy warnings vs main.

- [ ] **Step 4: Commit**

```bash
git add .env.example CLAUDE.md
git commit -m "docs: MOMENTUM_ADOPT_ALL_TOKENS knobs in .env.example and CLAUDE.md"
```

---

### Task 7: Adoption email notifications (user requirement added 2026-08-09 22:39)

**Files:**
- Modify: `src/portfolio/momentum.rs` (`adopt_wallet_position` ~line 1703, `adopt_unwatched_holdings` from Task 4, new pure fn near `email_trade` ~line 1344)
- Modify: `src/portfolio/watcher.rs` (the two `adopt_wallet_position` call sites gain `.await`)
- Test: bottom of `src/portfolio/momentum.rs`

**Interfaces:**
- Consumes: `email_trade(cfg: &PortfolioConfig, subject: &str, body: &str)` (async, private, already labels `[PAPER]` in dry-run — momentum.rs ~1344), `AdoptCandidate`.
- Produces: `pub fn adoption_email(c: &AdoptCandidate, unwatched: bool, trail_pct: f64) -> (String, String)` (subject, body); `adopt_wallet_position` becomes `pub async fn`.

- [ ] **Step 1: Write the failing test** (bottom of `momentum.rs`):

```rust
#[test]
fn adoption_email_labels_unwatched_and_carries_numbers() {
    let c = AdoptCandidate {
        mint: "M".into(), symbol: "CATE".into(), amount: 2.5, price_usd: 4.0,
    };
    let (subj_w, body_w) = adoption_email(&c, false, 30.0);
    assert!(subj_w.contains("ADOPTED CATE"));
    assert!(body_w.contains("$10.00")); // 2.5 × 4.0 basis
    assert!(body_w.contains("trail 30.0%"));
    assert!(!subj_w.contains("unwatched"));
    let (subj_u, body_u) = adoption_email(&c, true, 12.0);
    assert!(subj_u.contains("ADOPTED CATE (unwatched)"));
    assert!(body_u.contains("trail-only"));
    assert!(body_u.contains("trail 12.0%"));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib adoption_email`
Expected: FAIL — function not found.

- [ ] **Step 3: Implement.**

Pure fn near `email_trade`:

```rust
/// Subject + body for an adoption notification (watched or unwatched pass).
/// Kept pure so the wording is unit-tested; `email_trade` adds [PAPER] labeling.
pub fn adoption_email(c: &AdoptCandidate, unwatched: bool, trail_pct: f64) -> (String, String) {
    let basis = c.amount * c.price_usd;
    let subject = if unwatched {
        format!("ADOPTED {} (unwatched)", c.symbol)
    } else {
        format!("ADOPTED {}", c.symbol)
    };
    let body = format!(
        "Momentum trader adopted a wallet holding:\n\n\
         token:  {} ({})\n\
         amount: {:.6}\n\
         price:  ${:.6}\n\
         basis:  ${:.2} (PnL measured from adoption; real cost basis unknown)\n\
         mgmt:   {}trail {:.1}%\n",
        c.symbol, c.mint, c.amount, c.price_usd, basis,
        if unwatched { "trail-only (no fade exit), " } else { "" },
        trail_pct,
    );
    (subject, body)
}
```

Make `adopt_wallet_position` async (`pub async fn`) and, in its adoption loop directly after the existing `info!("momentum: ADOPTED wallet position …")`, add:

```rust
let (subject, body) = adoption_email(&c, false, trail_for(watched, &c.mint, cfg.momentum_trail_pct));
email_trade(cfg, &subject, &body).await;
```

(Note: `c` is consumed into the Position literal in the existing loop — either email BEFORE building the Position, or clone the needed fields; keep the existing state-save semantics untouched.)

In `adopt_unwatched_holdings` (Task 4), directly after its `info!("momentum: ADOPTED unwatched holding …")`, add:

```rust
let (subject, body) = adoption_email(&c, true, cfg.momentum_adopt_trail_pct);
email_trade(cfg, &subject, &body).await;
```

In `src/portfolio/watcher.rs`, add `.await` to both `adopt_wallet_position(…)` call sites (startup ~line 288, slow-tick Step 0 ~line 987).

Emails fire only on REAL adoptions — the watched pass's dry-run early-return and the unwatched pass's "would adopt" paper branch send nothing.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib adoption_email && cargo test --lib momentum && cargo build --release 2>&1 | tail -3`
Expected: PASS, clean build.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum.rs src/portfolio/watcher.rs
git commit -m "feat: email notification on every momentum adoption (watched + unwatched)"
```

---

### Task 8: gRPC pricing for adopted unwatched tokens (user requirement added 2026-08-09 22:47)

**Files:**
- Modify: `src/portfolio/pricer.rs` (new `resolve_best_pool`)
- Modify: `src/portfolio/watcher.rs` (adopted-pool map, universe overlay, hoist the dynamic-wiring block out of the scan match ~line 645)
- Test: bottom of `src/portfolio/pricer.rs` and `src/portfolio/watcher.rs`

**Interfaces:**
- Consumes: `Position.adopted_unwatched` (Task 2), the existing dynamic-wiring machinery: `dynamic_pool_set` (watcher.rs:1488), `dex_to_decode_script` (1353), `run_pool_decode` (1372), `effective_universe` (1446), `feed_setup::spawn_grpc_feed(cfg, watched, extra_pools)` — which wires a token ONLY via `w.pool_refs()`, so the held entry must carry `pool`+`quote`.
- Produces:

```rust
/// pricer.rs — DexScreener best-venue resolution for one mint.
pub struct ResolvedPool {
    pub pool: String,   // pairAddress
    pub dex: String,    // dexId: pumpswap|raydium|orca|meteora
    pub quote: String,  // "SOL" | "USDC" (PoolRef convention)
}
/// Pure ranking half (unit-tested): pick the highest-volume.h24 pair whose
/// quoteToken.address is WSOL or USDC and whose dexId is a supported venue.
pub fn pick_best_pool(pairs_json: &serde_json::Value) -> Option<ResolvedPool>;
/// I/O wrapper: GET {DEXSCREENER_URL}/{mint} (same endpoint fetch_prices uses),
/// then pick_best_pool. Errors → None (fail open to REST), logged by caller.
pub async fn resolve_best_pool(http: &Client, mint: &str) -> Option<ResolvedPool>;

/// watcher.rs — pure overlay (unit-tested): for each universe entry whose mint is
/// in `adopted` and whose pool_refs() is empty, set `pool`/`quote` from the map.
fn overlay_adopted_pools(
    universe: &mut [WatchedToken],
    adopted: &HashMap<String, crate::portfolio::pricer::ResolvedPool>,
);
```

- [ ] **Step 1: Write the failing tests.**

In `pricer.rs` tests (construct DexScreener-shaped JSON inline):

```rust
#[test]
fn pick_best_pool_ranks_by_volume_filters_quote_and_dex() {
    let j: serde_json::Value = serde_json::json!({ "pairs": [
        { "dexId": "meteora", "pairAddress": "LOW",
          "quoteToken": { "address": "So11111111111111111111111111111111111111112" },
          "volume": { "h24": 100.0 } },
        { "dexId": "pumpswap", "pairAddress": "BEST",
          "quoteToken": { "address": "So11111111111111111111111111111111111111112" },
          "volume": { "h24": 900.0 } },
        { "dexId": "pumpswap", "pairAddress": "EXOTIC_QUOTE",
          "quoteToken": { "address": "SomeRandomQuoteMint111111111111111111111111" },
          "volume": { "h24": 5000.0 } },
        { "dexId": "unknown_dex", "pairAddress": "UNSUPPORTED",
          "quoteToken": { "address": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" },
          "volume": { "h24": 9000.0 } }
    ]});
    let got = pick_best_pool(&j).expect("one eligible pool");
    assert_eq!(got.pool, "BEST");
    assert_eq!(got.dex, "pumpswap");
    assert_eq!(got.quote, "SOL");
}

#[test]
fn pick_best_pool_none_when_no_eligible_pairs() {
    let j = serde_json::json!({ "pairs": [] });
    assert!(pick_best_pool(&j).is_none());
}
```

In `watcher.rs` tests (mirror the existing `effective_universe` test fixtures for `WatchedToken` construction):

```rust
#[test]
fn overlay_adopted_pools_fills_only_empty_refs() {
    let mut universe = vec![
        WatchedToken { symbol: "A".into(), mint: "MA".into(), name: None, equity: None,
                       params: None, pool: None, quote: None, pools: None },
        WatchedToken { symbol: "B".into(), mint: "MB".into(), name: None, equity: None,
                       params: None, pool: Some("EXISTING".into()), quote: Some("SOL".into()), pools: None },
    ];
    let mut adopted = std::collections::HashMap::new();
    adopted.insert("MA".to_string(), crate::portfolio::pricer::ResolvedPool {
        pool: "PA".into(), dex: "pumpswap".into(), quote: "SOL".into() });
    adopted.insert("MB".to_string(), crate::portfolio::pricer::ResolvedPool {
        pool: "PB".into(), dex: "pumpswap".into(), quote: "SOL".into() });
    overlay_adopted_pools(&mut universe, &adopted);
    assert_eq!(universe[0].pool.as_deref(), Some("PA")); // filled
    assert_eq!(universe[0].quote.as_deref(), Some("SOL"));
    assert_eq!(universe[1].pool.as_deref(), Some("EXISTING")); // curated ref untouched
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib pick_best_pool && cargo test --lib overlay_adopted`
Expected: FAIL — functions not found.

- [ ] **Step 3: Implement `pricer.rs` half.** `pick_best_pool` filters `pairs[]` to `dexId ∈ {pumpswap, raydium, orca, meteora}` AND `quoteToken.address ∈ {WSOL, USDC_MINT}` (the file's existing consts; USDT quotes are NOT wireable — PoolRef quote is SOL|USDC only), maps quote WSOL→"SOL" / USDC→"USDC", and returns the max by `volume.h24` (missing volume = 0.0). `resolve_best_pool` GETs `{DEXSCREENER_URL}/{mint}` with the file's existing reqwest patterns, parses to `serde_json::Value`, returns `pick_best_pool(&v)` (any error → `None`).

- [ ] **Step 4: Implement `watcher.rs` half.**

1. `overlay_adopted_pools` exactly per the interface block (skip entries with non-empty `pool_refs()`).
2. Watcher loop state: `let mut adopted_pools: HashMap<String, pricer::ResolvedPool> = HashMap::new();`
3. Each slow tick (before the wiring block): load momentum state; for each position with `adopted_unwatched == true` whose mint is not in `adopted_pools`, call `pricer::resolve_best_pool(&http, &mint).await` — `Some` → insert + `info!("momentum: adopted {} → gRPC pool {} ({}, quote {})", …)`; `None` → `info!` once per streak (a simple `HashSet<String>` of already-logged failures, cleared when the mint resolves or is dropped). Remove `adopted_pools`/failure-log entries whose mint is no longer held with the flag.
4. **Hoist the dynamic-wiring block** (currently inside the scan `match` at ~line 645-720) into the main tick body so it runs every slow tick regardless of `MOMENTUM_SCAN_ENABLE`: compute `let mut want = dynamic_pool_set(&discovered); want.extend(adopted_pools.values().map(|r| r.pool.clone()));` and in the `by_script` grouping, look up each pool's dex from `pool_dex` first, then from `adopted_pools` (match on `r.pool == *pool`), falling back to `POOL_DECODE_SCRIPT` as today. Guard the whole block with `cfg.momentum_grpc_pricing` (as now) and the same `want != wired_dynamic` change-gate so an unchanged set does zero work per tick. The universe for `spawn_grpc_feed` gains `overlay_adopted_pools(&mut universe, &adopted_pools)` right before the call.
5. The scan match keeps its discovery/backfill work but loses the wiring block (it now falls through to the hoisted one).

- [ ] **Step 5: Run tests + build**

Run: `cargo test --lib pick_best_pool && cargo test --lib overlay_adopted && cargo test --lib watcher && cargo build --release 2>&1 | tail -3`
Expected: PASS, clean build.

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/pricer.rs src/portfolio/watcher.rs
git commit -m "feat: dynamic gRPC pool wiring for adopted unwatched tokens"
```

---

## Self-review notes

- Spec coverage: env knobs (Task 1), flag + audit (Task 2), selection incl. exclusions/floor/cooldown/USD-order/cap (Task 3), pass + call sites + sellability gate + paper logging (Task 4), exit semantics incl. stagnation lock-in test (Task 5), docs/rollout (Task 6). Liquidity-drain/fast-arm items need no code: unwatched mints have no depth feed and no gRPC price, so both fail open already.
- The watched pass keeps priority by ORDER (it runs first and consumes capacity before the unwatched pass reloads state).
- Type consistency: `AdoptCandidate` reused from the watched pass; `choose_unwatched_adoption` signature identical between Task 3 (definition) and Task 4 (call).
