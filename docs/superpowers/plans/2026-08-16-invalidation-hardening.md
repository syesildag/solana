# Invalidation Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every confirmed finding from the max-effort review of commit `ecf5669`: a live momentum position must never be written off (no sell) without genuinely independent on-chain confirmation, every drop site must share one audited close path that keeps all bookkeeping consistent, and the machinery must not stall the exit loop or lie in its logs.

**Architecture:** One new evidence primitive in `scanner.rs` (`confirm_zero_balance`: owner-indexed query cross-checked against direct dual-ATA lookups at confirmed commitment — direct account lookup does not use the secondary owner index, so it is a different evidence class), one unified close helper on `TraderState` (`close_without_sell`: retain + bench + escalation clear + TradeRecord push), and all four drop sites (mid-run invalidation, exit-path zero, rotation zero, startup reconcile) routed through both, with concurrency-bounded confirms, an entry-age guard, audit-after-save ordering, and KEEP-verdict audit records.

**Tech Stack:** Rust (tokio, serde, futures::future::join_all, spl-token/spl-token-2022 + spl-associated-token-account for ATA derivation — check Cargo.toml; if `spl-associated-token-account` is absent, derive the ATA with `Pubkey::find_program_address(&[owner, token_program, mint], &ATA_PROGRAM_ID)` which needs no new dependency).

## Global Constraints

- NEVER run `cargo fmt` or whole-file rustfmt (repo is not rustfmt-clean; hard user rule). Match surrounding style by hand.
- Tests live in the existing `#[cfg(test)]` blocks at the bottom of each source file; run with `cargo test --lib <filter>` (NOT `--bin solana-mev` — the bin target has no portfolio module).
- Fail-closed direction is LAW: a failed/ambiguous read KEEPS the position. Only a positively-confirmed zero (owner query empty AND both ATA lookups answering "absent or zero-amount") drops it.
- The watcher loop is one task; any new `.await` in the loop body must be bounded (`tokio::time::timeout`) and batched (`join_all`) — never serial unbounded RPCs.
- Behavior for positions/mints NOT in a drop scenario must be byte-identical.
- Commit only — NEVER `git push`.
- Bash timeout 600000 ms for cargo commands (fresh worktree = cold build).

---

### Task 1: Evidence primitive — `confirm_zero_balance` + scanner integrity

**Files:**
- Modify: `src/portfolio/scanner.rs`
- Test: bottom of `src/portfolio/scanner.rs`

**Interfaces:**
- Produces (in `scanner.rs`):

```rust
/// One ATA lookup outcome, normalized for the zero-confirmation verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AtaLookup {
    /// RPC answered positively: no account at that address.
    Absent,
    /// Account exists and holds this raw amount.
    Amount(u64),
    /// Account exists but its data did not parse as a token account.
    Unparseable,
}

/// Pure verdict (unit-tested): is the balance CONFIRMED zero?
/// `owner_total` = the owner-indexed query result; `ata_spl`/`ata_2022` = direct lookups.
/// Confirmed zero requires the owner query to have found nothing AND both direct
/// lookups to answer Absent or Amount(0). Anything else keeps the position:
/// any Amount(>0) anywhere is non-zero; Unparseable is ambiguous ⇒ NOT confirmed.
pub fn zero_verdict(owner_total: u64, ata_spl: AtaLookup, ata_2022: AtaLookup) -> ZeroVerdict;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZeroVerdict {
    ConfirmedZero,
    /// Positive evidence of a balance (raw amount, from whichever source saw it).
    NonZero(u64),
    /// Ambiguous (unparseable account data) — treat like a failed read.
    Unconfirmed,
}

/// I/O wrapper: owner-indexed get_token_accounts_by_owner(Mint) at CONFIRMED
/// commitment, then get_multiple_accounts on the two derived ATAs (spl-token +
/// token-2022) at CONFIRMED commitment, combined via `zero_verdict`.
/// Transport error anywhere ⇒ Err (caller keeps the position).
pub async fn confirm_zero_balance(rpc_url: &str, owner: &str, mint: &str) -> Result<ZeroVerdict>;
```

- [ ] **Step 1: Write the failing tests** (bottom of `scanner.rs`):

```rust
#[test]
fn zero_verdict_requires_positive_absence_everywhere() {
    use AtaLookup::*;
    // Confirmed zero: owner query empty + both ATAs positively absent/zero.
    assert_eq!(zero_verdict(0, Absent, Absent), ZeroVerdict::ConfirmedZero);
    assert_eq!(zero_verdict(0, Amount(0), Absent), ZeroVerdict::ConfirmedZero);
    // Any positive balance anywhere wins.
    assert_eq!(zero_verdict(5, Absent, Absent), ZeroVerdict::NonZero(5));
    assert_eq!(zero_verdict(0, Amount(7), Absent), ZeroVerdict::NonZero(7));
    assert_eq!(zero_verdict(0, Absent, Amount(9)), ZeroVerdict::NonZero(9));
    // Ambiguity never confirms zero.
    assert_eq!(zero_verdict(0, Unparseable, Absent), ZeroVerdict::Unconfirmed);
    assert_eq!(zero_verdict(0, Absent, Unparseable), ZeroVerdict::Unconfirmed);
}

#[test]
fn wallet_scan_falls_back_to_raw_amount_when_ui_amount_is_null() {
    // Simulate the jsonParsed tokenAmount payload of a scaledUiAmount mint whose
    // uiAmount is null: the scan must derive amount/10^decimals instead of skipping.
    let info = serde_json::json!({
        "mint": "M",
        "tokenAmount": { "uiAmount": null, "amount": "2500000", "decimals": 6 }
    });
    assert_eq!(parse_token_amount(&info), Some(("M".to_string(), 2.5)));
    // And the normal path still works.
    let info2 = serde_json::json!({
        "mint": "M2",
        "tokenAmount": { "uiAmount": 1.25, "amount": "1250000", "decimals": 6 }
    });
    assert_eq!(parse_token_amount(&info2), Some(("M2".to_string(), 1.25)));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib zero_verdict && cargo test --lib wallet_scan_falls_back`
Expected: FAIL — types/fns not found.

- [ ] **Step 3: Implement.**

1. `zero_verdict` exactly per the doc above (order of checks: any `Amount(n) if n > 0` or `owner_total > 0` → `NonZero(max)`; any `Unparseable` → `Unconfirmed`; else `ConfirmedZero`).
2. `confirm_zero_balance`: derive both ATAs via `Pubkey::find_program_address(&[owner.as_ref(), token_program.as_ref(), mint.as_ref()], &ATA_PROGRAM)` where `ATA_PROGRAM = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"` (add as a const; use the `spl-associated-token-account` crate helper instead if it is already a dependency). Use `spawn_blocking` + `RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed())`. Owner query = the existing `get_token_accounts_by_owner(Mint)` sum, but a PARSE FAILURE inside the sum must produce `Err` (not silently contribute 0 — fix the existing swallow in the copied logic; leave `fetch_token_balance_raw` itself untouched for its other callers this task). ATA lookups via one `get_multiple_accounts(&[ata_spl, ata_2022])`: `None` → `Absent`; `Some(acct)` → unpack with `spl_token::state::Account::unpack` (or byte-offset 64..72 LE like the existing code) → `Amount(raw)`, unparseable → `Unparseable`.
3. Wallet-scan integrity, same file: (a) extract the per-account `(mint, ui_amount)` parse in `fetch_wallet_balances` into `fn parse_token_amount(info: &serde_json::Value) -> Option<(String, f64)>` with the null-uiAmount fallback (`amount` string parsed as u64 / 10^`decimals`); (b) the Token-2022 `get_token_accounts_by_owner` failure at ~line 101 becomes `?` (propagate Err) instead of warn-and-continue — a partial scan must not be presented as a successful one (`scan_and_save` then keeps the previous portfolio.json for that tick). Update the comment accordingly.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib zero_verdict && cargo test --lib wallet_scan_falls_back && cargo test --lib scanner && cargo build --release 2>&1 | tail -3`
Expected: PASS, clean build.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/scanner.rs
git commit -m "feat: confirm_zero_balance evidence primitive + partial-scan integrity"
```

---

### Task 2: Unified close — `TraderState::close_without_sell` + richer `Invalidated` record

**Files:**
- Modify: `src/portfolio/momentum_state.rs`
- Modify: `src/portfolio/momentum_actions.rs`
- Test: bottom of `src/portfolio/momentum_state.rs`

**Interfaces:**
- Produces:

```rust
// momentum_state.rs
impl TraderState {
    /// Close a position WITHOUT a sell (balance confirmed gone / externally sold):
    /// removes it, benches the mint, clears its exit-escalation counter, and pushes
    /// a TradeRecord (exit_sig "invalidated") so the daily-trade-cap count and the
    /// realized-P&L sidecar stay consistent. `last_price_usd` = best-known price
    /// (0.0 if none). Returns the removed Position for the caller's audit record.
    pub fn close_without_sell(&mut self, mint: &str, ts: i64, last_price_usd: f64) -> Option<Position>;
}
```

```rust
// momentum_actions.rs — extend the variant (all new fields #[serde(default)] so old lines parse):
Invalidated {
    symbol: String,
    mint: String,
    #[serde(default)]
    token_amount: f64,
    #[serde(default)]
    entry_price_usd: f64,
    #[serde(default)]
    peak_price_usd: f64,
    #[serde(default)]
    last_price_usd: f64,
    #[serde(default)]
    dry_run: bool,
},
```

- [ ] **Step 1: Write the failing tests** (momentum_state.rs tests; reuse this module's existing Position JSON fixtures):

```rust
#[test]
fn close_without_sell_keeps_all_bookkeeping_consistent() {
    let mut state = TraderState::default();
    let mut pos: Position = serde_json::from_str(r#"{
        "mint":"M","symbol":"S","entry_ts":"2026-08-16T00:00:00Z",
        "entry_price_usd":1.0,"token_amount":100.0,"usdc_spent":100.0,
        "peak_price_usd":2.0
    }"#).unwrap();
    pos.dry_run = false;
    state.positions.push(pos);
    state.exit_attempts_per_mint.insert("M".to_string(), 4);

    let n_trades_before = state.trades.len();
    let removed = state.close_without_sell("M", 1_755_300_000, 1.5).expect("position removed");

    assert_eq!(removed.mint, "M");
    assert!(state.positions.is_empty());
    assert_eq!(state.last_exit_ts_per_mint.get("M"), Some(&1_755_300_000)); // benched
    assert!(state.exit_attempts_per_mint.get("M").is_none()); // escalation reset
    // TradeRecord pushed: daily cap + P&L sidecar stay consistent.
    assert_eq!(state.trades.len(), n_trades_before + 1);
    let t = state.trades.last().unwrap();
    assert_eq!(t.exit_sig, "invalidated");
    assert_eq!(t.usdc_out, 150.0); // 100 tokens × $1.5 best-known
    assert!(!t.dry_run);
    // Unknown mint → None, state untouched.
    assert!(state.close_without_sell("NOPE", 1, 1.0).is_none());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib close_without_sell`
Expected: FAIL — method not found.

- [ ] **Step 3: Implement.** `close_without_sell`: find + remove the position by mint (`positions.iter().position(...)` + `remove(idx)`), insert bench ts, `exit_attempts_per_mint.remove(mint)`, push `TradeRecord { entry_ts: p.entry_ts, exit_ts: ts, mint, symbol, entry_price_usd: p.entry_price_usd, exit_price_usd: last_price_usd, peak_price_usd: p.peak_price_usd, usdc_in: p.usdc_spent, usdc_out: p.token_amount * last_price_usd, pnl_pct: computed from usdc_in/usdc_out (0.0 if usdc_in == 0.0), entry_sig: p.entry_sig.clone(), exit_sig: "invalidated".into(), dry_run: p.dry_run }`, return `Some(p)`. Extend `ActionKind::Invalidated` with the five defaulted fields; the compiler flags the single construction site in `momentum.rs` — fill the new fields from the removed Position there (`last_price_usd` can be 0.0 for now; Task 3 threads the real price).

- [ ] **Step 4: Run tests**

Run: `cargo test --lib close_without_sell && cargo test --lib momentum_state && cargo test --lib momentum && cargo build --release 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum_state.rs src/portfolio/momentum_actions.rs src/portfolio/momentum.rs
git commit -m "feat: unified close_without_sell + full-fidelity Invalidated audit record"
```

---

### Task 3: Rewire mid-run invalidation — confirmed evidence, bounded concurrency, every-tick retry, honest audit

**Files:**
- Modify: `src/portfolio/momentum.rs` (`invalidate_unbacked_position` + the new-test symbol fix)
- Modify: `src/portfolio/momentum_actions.rs` (one new KEEP-verdict variant)
- Modify: `src/portfolio/watcher.rs` (call site: every slow tick; pass prices + stop_armed)
- Test: bottom of `src/portfolio/momentum.rs`

**Interfaces:**
- `invalidate_unbacked_position` new signature (single call site, watcher.rs):

```rust
pub async fn invalidate_unbacked_position(
    cfg: &PortfolioConfig,
    portfolio: &Portfolio,
    prices: &HashMap<String, f64>,                                  // for last_price_usd
    stop_armed: Option<&dashmap::DashMap<String, std::time::Instant>>, // clear on close
) -> bool
```

- New audit variant:

```rust
/// A nominated (scan-missing) live position was KEPT: the on-chain confirmation
/// did not return a confirmed zero. reason ∈ {"non-zero", "unconfirmed", "read-failed", "too-young"}.
InvalidateSkipped { symbol: String, mint: String, reason: String },
```

- [ ] **Step 1: Behavior changes to implement (all in `invalidate_unbacked_position`):**

1. **Entry-age guard:** skip (and audit `InvalidateSkipped{reason:"too-young"}`) any candidate whose `entry_ts` is within 180 s of `now` — a just-filled entry may not be visible at the scan's commitment yet. Constant `const INVALIDATE_MIN_AGE_SECS: i64 = 180;` with a doc comment naming the confirmed→finalized race.
2. **Confirmation:** replace the serial `fetch_token_balance_raw` loop with concurrent, bounded confirms:

```rust
let verdicts = futures::future::join_all(candidates.iter().map(|mint| async {
    let v = tokio::time::timeout(
        std::time::Duration::from_secs(8),
        scanner::confirm_zero_balance(&cfg.rpc_url, &owner, mint),
    )
    .await;
    (mint.clone(), v)
})).await;
```

(`futures` is already a dependency — `join_all` is used in `src/jito/client.rs`.) Verdict handling per candidate: `Ok(Ok(ConfirmedZero))` → drop list; `Ok(Ok(NonZero(raw)))` → keep, `info!` + audit `InvalidateSkipped{reason:"non-zero"}`; `Ok(Ok(Unconfirmed))` → keep, `warn!` + audit `InvalidateSkipped{reason:"unconfirmed"}`; `Ok(Err(_))` or `Err(_elapsed)` → keep, `warn!` + audit `InvalidateSkipped{reason:"read-failed"}`.
3. **Close:** for each confirmed-zero mint, `state.close_without_sell(&mint, ts, prices.get(&mint).copied().unwrap_or(0.0))`; clear the dwell entry `if let Some(sa) = stop_armed { sa.remove(&mint); }`; build the full `Invalidated` audit record from the returned Position **but do not emit it yet**.
4. **Audit-after-save ordering:** `momentum_state::save` FIRST; on save Err → `warn!` and `return false` WITHOUT emitting the Invalidated records (in-memory mutations are discarded with the function; next tick retries — no phantom audit lines, no duplicates). Only after a successful save, emit the buffered `Invalidated` audit records and the warn! lines.
5. **Doc fix:** the fn doc's "~a minute later" becomes "after `MOMENTUM_ADOPT_COOLDOWN_SECS` (this deployment: 60 s; default: the 1 h reentry cooldown)".
6. **Test fix:** in `unbacked_candidates_nominates_missing_and_zero_live_positions_only`, give positions distinct `symbol` vs `mint` (e.g. mint "GONE_MINT", symbol "GONE") and assert the returned values are the MINTS — killing the symbol==mint tautology.

- [ ] **Step 2: Watcher call site** (watcher.rs): move the call OUT of the `changed` branch so it runs every slow tick right after the re-scan block (nomination is a cheap set-diff; candidates are usually empty), passing `&prices`... note `prices` is built later in the tick — so place the call AFTER the price-merge (`let mut prices = last_prices.clone(); prices.extend(fresh);`) and BEFORE the momentum adoption/tick block, i.e. next to the existing Step-0 adoption calls:

```rust
momentum::invalidate_unbacked_position(&cfg, &portfolio, &prices, Some(&stop_armed)).await;
momentum::adopt_wallet_position(&cfg, &portfolio, &prices, &watched).await;
momentum::adopt_unwatched_holdings(&cfg, &portfolio, &prices, &watched, &http).await;
```

Remove the old call inside the `changed` branch. Update the stale comment block at watcher.rs ~619-627: the wallet-scan `.await` note stays true, but add one sentence: "the invalidation confirm reads below are batched via join_all and hard-capped at 8 s each, so the worst-case loop stall is one timeout window, not N×30 s."
Also fix the stale comment at watcher.rs ~350 ("When scanning is off, `effective` stays equal to `watched`") — held mints join `effective` unconditionally since c415ea0.

- [ ] **Step 3: Tests.** Add to momentum.rs tests:

```rust
#[test]
fn invalidation_verdict_routing_is_fail_closed() {
    // Pure-routing check via zero_verdict re-export semantics: this test pins the
    // MAPPING contract in one place so a refactor can't silently flip an arm.
    use crate::portfolio::scanner::{ZeroVerdict, AtaLookup, zero_verdict};
    assert_eq!(zero_verdict(0, AtaLookup::Absent, AtaLookup::Absent), ZeroVerdict::ConfirmedZero);
    assert_eq!(zero_verdict(0, AtaLookup::Unparseable, AtaLookup::Absent), ZeroVerdict::Unconfirmed);
    // The routing rule: ONLY ConfirmedZero may drop. Encoded as a const fn used by the impl:
    assert!(verdict_drops(ZeroVerdict::ConfirmedZero));
    assert!(!verdict_drops(ZeroVerdict::NonZero(1)));
    assert!(!verdict_drops(ZeroVerdict::Unconfirmed));
}
```

with `fn verdict_drops(v: ZeroVerdict) -> bool { matches!(v, ZeroVerdict::ConfirmedZero) }` used by the real handling (match arms call it or are structured so the test is honest — implementer's judgment, but the drop condition must flow through ONE testable predicate).

- [ ] **Step 4: Run tests + build**

Run: `cargo test --lib invalidat && cargo test --lib unbacked && cargo test --lib momentum && cargo test --lib watcher && cargo build --release 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/momentum.rs src/portfolio/momentum_actions.rs src/portfolio/watcher.rs
git commit -m "fix: confirmed-evidence invalidation — bounded confirms, every-tick retry, honest audit"
```

---

### Task 4: Route the exit/rotation zero-clears through the same evidence + close path

**Files:**
- Modify: `src/portfolio/momentum.rs` (exit-path zero-clear ~4155-4170; rotation zero-clear ~3262-3278; `stop_decision` stale-arm guard ~4387)
- Test: bottom of `src/portfolio/momentum.rs`

- [ ] **Step 1: Exit path** (inside `maybe_exit`'s sell-sizing block): today `Ok(0)` from `fetch_token_balance_raw` immediately clears the position. New behavior:

```rust
Ok(0) => {
    // Owner-indexed zero — confirm against direct ATA lookups before writing
    // the position off (a lagging owner index returns empty on healthy wallets).
    match scanner::confirm_zero_balance(&cfg.rpc_url, &owner, &pos.mint).await {
        Ok(scanner::ZeroVerdict::ConfirmedZero) => {
            warn!("momentum: on-chain balance of {} confirmed zero — position closed externally", pos.symbol);
            if let Some(p) = state.close_without_sell(&pos.mint, ts, price) {
                audit(cfg, ts, ActionKind::Invalidated {
                    symbol: p.symbol.clone(), mint: p.mint.clone(),
                    token_amount: p.token_amount, entry_price_usd: p.entry_price_usd,
                    peak_price_usd: p.peak_price_usd, last_price_usd: price, dry_run: p.dry_run,
                });
            }
            if let Some(sa) = ctx.stop_armed { sa.remove(&pos.mint); }
            momentum_state::save(state_path, &state)?;
            return Ok(None);
        }
        Ok(scanner::ZeroVerdict::NonZero(raw)) => raw, // the index lied — sell the real balance
        _ => {
            warn!("momentum: zero balance for {} UNCONFIRMED — keeping position, retrying next tick", pos.symbol);
            return Ok(None); // no sell this tick; stop stays armed
        }
    }
}
```

Adapt variable names (`price`, `ts`, `state_path`, `ctx`) to the function's actuals — read the surrounding code first; the rotation site (~3262) gets the same transformation with its own actuals (it sizes `pos` → sell for rotation; NonZero(raw) feeds the same variable the old `Ok(raw) if raw > 0` arm fed).

- [ ] **Step 2: `stop_decision` stale-arm guard.** A dwell arm left over from a closed position must not instantly fire the next position's stop. In `stop_decision` (pure fn, ~4387): if `armed_since` is older than `STALE_ARM_SECS: u64 = 600` (10 min — vastly longer than any real dwell), treat as un-armed (re-arm now). Read the fn body first and mirror its return semantics. Test:

```rust
#[test]
fn stop_decision_ignores_prehistoric_arm_timestamps() {
    // An arm from a position closed hours ago must re-arm, not instant-sell.
    // Build Instants via Instant::now() - Duration (mirror the existing
    // stop_decision_dwell_lifecycle test's construction).
    let now = std::time::Instant::now();
    let ancient = now - std::time::Duration::from_secs(3 * 3600);
    let d = stop_decision(true, Some(ancient), now, 3);
    assert!(matches!(d, StopDecision::Arm), "stale arm must re-arm, not sell");
}
```

(Adapt names to the real enum/signature after reading it — the contract is the assertion message.)

- [ ] **Step 3: Run tests + build**

Run: `cargo test --lib stop_decision && cargo test --lib momentum && cargo build --release 2>&1 | tail -3`
Expected: PASS, including the pre-existing `stop_decision_dwell_lifecycle`.

- [ ] **Step 4: Commit**

```bash
git add src/portfolio/momentum.rs
git commit -m "fix: exit/rotation zero-clears require confirmed evidence; stale stop-arms re-arm"
```

---

### Task 5: Startup reconcile + watched-adoption bench

**Files:**
- Modify: `src/portfolio/momentum.rs` (`reconcile_startup_position` ~1546; `adopt_wallet_position` candidate loop ~1849)
- Modify: `src/portfolio/watcher.rs` (startup call site ~281 gains `.await` and args)
- Test: bottom of `src/portfolio/momentum.rs`

- [ ] **Step 1: Startup reconcile.** Make `reconcile_startup_position` async and route its Step-3 unbacked handling through the SAME machinery as Task 3: nominate via `unbacked_candidates` (delete the inline duplicate predicate), apply the entry-age guard, confirm via `confirm_zero_balance` (join_all + 8 s timeouts), close via `close_without_sell`, audit `Invalidated` after a successful save, audit `InvalidateSkipped` on keeps. Startup has no prices map yet — pass the history-seeded `last_prices` from the watcher (it exists before the call site; if genuinely empty, `last_price_usd` = 0.0 is acceptable). Keep the function's OTHER steps (mode-mismatch purge etc.) untouched. Watcher startup call gains `.await` and the two new args (prices + `Some(&stop_armed)` — `stop_armed` is created at watcher.rs ~72, before this call site; verify and reorder declarations only if needed).
   Structural note: if extracting a shared `confirm_and_close(cfg, state, candidates, prices, stop_armed) -> bool` helper used by BOTH Task 3's fn and this one keeps the diff smaller and the logic single-sourced, do that — it is the preferred shape; Task 3's implementation should anticipate it (the reviewer will check the two paths cannot drift).

- [ ] **Step 2: Watched-adoption bench.** In `adopt_wallet_position`'s candidate loop, skip a mint whose `last_exit_ts_per_mint` entry is within `cfg.momentum_adopt_cooldown_secs` of now, logging the skip (mirror the unwatched pass's cooldown semantics: `now - exit_ts < cooldown`). The state is already loaded in this function (it loads for capacity) — use that load's map. Test (pure, via `choose_adoption`? — no: the bench check must live where the state is in scope; add it as a filter BEFORE building `cands`, and test via a new pure helper if trivial, else document in the fn doc that the unwatched pass's `choose_unwatched_adoption` tests cover the shared `now - exit_ts < cooldown` semantics and add an integration-shaped unit test only if it doesn't require I/O scaffolding).

- [ ] **Step 3: Run tests + build**

Run: `cargo test --lib momentum && cargo test --lib watcher && cargo build --release 2>&1 | tail -3`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add src/portfolio/momentum.rs src/portfolio/watcher.rs
git commit -m "fix: startup reconcile uses confirmed evidence; watched adoption honors the bench"
```

---

### Task 6: Consumers + docs

**Files:**
- Modify: `src/portfolio/forward_report.rs` (~89-92)
- Modify: `CLAUDE.md` (adopt-all bullet), `src/portfolio/momentum.rs` (any remaining stale comment), `.env.example` (no knob changes — only if a comment references old behavior)
- Test: bottom of `src/portfolio/forward_report.rs`

- [ ] **Step 1: forward_report.** Add an explicit `"Invalidated"` arm that CLOSES the leg like `Exited` does: `usdc_out` = the record's `token_amount × last_price_usd` fields (0.0 when absent — old thin records), `reason: "invalidated"`, and `dry` from the record's own `dry_run` field — NOTE the parser's shared `dry` default is `unwrap_or(true)`; Invalidated records written before this plan lack the field, so old lines close as paper (documented in an arm comment). Follow the file's existing explicit-arm-with-rationale-comment convention. Test: feed `parse_actions` a small JSONL string with live `Entered` then `Invalidated` (with the new fields, `dry_run: false`) and assert the trip lands in `closed` with reason "invalidated" and `open` is empty; mirror the file's existing test style.

- [ ] **Step 2: Docs.**
- CLAUDE.md adopt-all bullet: extend the adoption-latency sentence to "re-adoption after the trader's own exit — or after an invalidation (a position written off without a sell once its balance is CONFIRMED zero on-chain) — waits `MOMENTUM_ADOPT_COOLDOWN_SECS` …; the watched pass honors the same bench."
- Sweep for stale comments this plan's earlier tasks may have missed: watcher.rs ~350 and ~619-627 (Task 3 owns them — verify done), any "retrying next tick"-style text that no longer matches.

- [ ] **Step 3: Full gate**

Run: `cargo test --lib && cargo test --bin solana-mev 2>&1 | tail -2 && cargo build --release 2>&1 | tail -1 && cargo clippy --lib 2>&1 | grep -c warning`
Expected: all green; clippy warning count not above the pre-plan baseline (5 pre-existing).

- [ ] **Step 4: Commit**

```bash
git add src/portfolio/forward_report.rs CLAUDE.md
git commit -m "fix: forward_report closes Invalidated trips; docs match invalidation semantics"
```

---

## Deliberately out of scope (state in the final report)

- `merge()`-level partial-scan thresholds beyond the Token-2022 Err propagation (bigger design; the Err propagation already stops the biggest partial class from persisting).
- Shared/cached RpcClient and OnceLock pubkey (perf minors; unchanged behavior).
- The `-> bool` dead return of `invalidate_unbacked_position` (cosmetic; callers may appear later).
- Simplification minors from the review (duplicate symbol lookups collapse naturally via `close_without_sell`; anything left goes to the ledger).

## Self-review notes

- Every Critical/Important finding from the ecf5669 review maps to a task: startup twin (T5), Ok(0)≠confirmed (T1+T3+T4), curated bench (T5), finalized race (T1 commitment + T3 age guard), serial stall (T3 join_all+timeout), false re-check promise (T3 every-tick call), silent twins (T4), stop_armed leak (T3+T4 clears + T4 stale-arm guard), daily-cap/P&L/escalation leaks (T2 close_without_sell), audit-before-save (T3 ordering), forward_report (T6), KEEP-verdict audit (T3), thin record (T2), scanner fail-open + uiAmount-null (T1), test tautology (T3), untested verdict routing (T1 zero_verdict matrix + T3 verdict_drops).
- Type consistency: `ZeroVerdict`/`AtaLookup` defined in T1, consumed T3/T4/T5; `close_without_sell` defined T2, consumed T3/T4/T5; `Invalidated` fields defined T2, emitted T3/T4/T5, parsed T6.
