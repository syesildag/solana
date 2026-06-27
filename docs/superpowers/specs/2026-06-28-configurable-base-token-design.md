# Configurable base token (SOL default, USDC opt-in) — Design

**Date:** 2026-06-28
**Status:** Design — approved, pending spec review
**Goal:** Allow the arbitrage engine's base/starting token to be configured (SOL by
default, USDC opt-in) in service of stable-denominated P&L, without disturbing the
proven SOL path.

## Summary

Today the bot hardcodes wrapped SOL as the starting currency of every arbitrage cycle:
Bellman-Ford enumerates only cycles that start and end at the SOL mint, the cycle is
funded by a MarginFi **SOL** flash loan (or wallet-funded with SOL wrapped into a WSOL
ATA), all profit/input thresholds are denominated in lamports, and the wallet-balance
halt guard watches native SOL.

This change introduces a thin `BaseToken` value object, threaded through the small set of
call sites that assume SOL, so the base token becomes a config value. The native-vs-SPL
distinction (not SOL-vs-USDC) is the single real branch point: a native base (WSOL) keeps
today's wrap/unwrap behavior byte-for-byte; a plain SPL base (USDC) funds directly from
its wallet ATA with no wrap step.

### Decisions locked during brainstorming

| Decision | Choice |
|---|---|
| Goal | Stable-denominated P&L |
| Scope | Configurable base token (SOL default, USDC opt-in) |
| Funding when base=USDC | **Wallet-funded** (own USDC balance). MarginFi USDC-bank flash loan is **out of scope**. |
| Jito tip sizing when base=USDC | Convert USDC profit → SOL via a cached price feed (reuse the portfolio pricer's Kraken SOL price), then apply existing ratio/floor logic. Fall back to floor tip when the price is stale. |
| Threshold denomination | Base token's smallest unit ("base-unit thresholds"); no hidden conversion on the hot path. |
| Halt/safety logic | **Dual guard**: P&L drawdown in base units **and** an independent SOL gas floor. |
| Structural approach | **A — thin `BaseToken` struct** threaded through (~6 call sites), `is_native` flag governs wrap/unwrap. No traits/generics. |

### Non-goals (explicitly out of scope)

- MarginFi (or any) **USDC flash loan** — USDC is wallet-funded only. SOL flash loan is
  untouched.
- Changing how Jito tips are **paid** — always SOL (hard Solana/Jito constraint).
- Sourcing/curating USDC-quoted pools in `pools.json` — an operational prerequisite
  (`fetch_all.js` + `--init-alt`), flagged here, not code in this change.
- Re-basing the portfolio/momentum/pairs subsystems — this design touches only the
  arbitrage engine.

## Backwards compatibility

`BASE_MINT` defaults to the WSOL mint. With no `.env` change, `base_token.is_native ==
true` and every code path takes its existing branch — the SOL behavior is unchanged. The
feature is strictly additive and opt-in.

## Architecture

### Section 1 — `BaseToken` value object & config

New struct in `src/dex/types.rs`:

```rust
pub struct BaseToken {
    pub mint: Pubkey,
    pub decimals: u8,
    pub symbol: &'static str,
    pub is_native: bool,   // true = WSOL (needs wrap/unwrap); false = plain SPL
}
```

Resolved at startup from a small static table (initially two entries):

| Symbol | Mint | Decimals | is_native |
|---|---|---|---|
| SOL  | `So11111111111111111111111111111111111111112` | 9 | true |
| USDC | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` | 6 | false |

- New env var `BASE_MINT`, default = WSOL mint. `Config` gains `base_token: BaseToken`.
- Unknown `BASE_MINT` → hard error at startup listing supported symbols.
- When `base_token.is_native == false`, flash loan is **force-disabled** with a warning
  (wallet-funded USDC), regardless of `ENABLE_FLASH_LOAN`.

### Section 2 — Funding & settlement (native vs SPL)

The wrap/unwrap logic in `src/arbitrage/evaluator.rs` (`build_setup_instructions` /
`build_teardown_instructions`, ~lines 450–490) becomes `is_native`-aware and takes the
`BaseToken`:

- **`is_native = true` (SOL — unchanged):** create WSOL ATA → `system_instruction::transfer(amount_in)`
  → `sync_native`; teardown `close_account` (unwraps principal+profit back to SOL).
  Byte-for-byte identical to today.
- **`is_native = false` (USDC):** **no** wrap, **no** `sync_native`, **no** `close_account`.
  The swap chain pulls `amount_in` from the existing base-token ATA and returns
  principal+profit to that same ATA. Setup still creates intermediate-mint ATAs as today.

The cycle still starts and ends at `base_token.mint`. Flash-loan setup/teardown
(`src/flash_loan/mod.rs`) is unchanged and only reachable when `is_native` (SOL).

### Section 3 — Tip sizing & the SOL price cache

Jito tips remain SOL-denominated. New seam between the async portfolio watcher and the
sync hot loop, because `src/portfolio/pricer.rs` is async-only (no synchronous hot-path
price exists today):

- A `SolPriceCache` holding an `AtomicU64` (SOL/USDC price as f64 bits) plus a freshness
  timestamp (`AtomicU64` unix secs).
- The portfolio watcher — which already fetches SOL/USD from Kraken (~300s cadence) —
  publishes into the cache on each refresh.
- `compute_jito_tip` (`src/arbitrage/evaluator.rs`): when `base_token.is_native`,
  unchanged. When not, convert base-unit gross profit → lamports via the cached price,
  then apply existing ratio/floor logic.
- **Staleness guard:** if the price is absent or older than a threshold (e.g. 2× the
  refresh interval), fall back to the floor tip — never bid on a stale rate. This makes a
  running portfolio pricer a soft prerequisite for base=USDC; document it.

### Section 4 — Dual-guard halt logic

Replaces the single native-balance check in `src/main.rs` (~lines 640–668):

- **P&L guard (base units):** track base-token balance vs startup; halt on drawdown
  beyond the configured floor (with the existing 2-strike debounce). For SOL this is the
  native balance (today's behavior); for USDC it reads the USDC ATA balance.
- **Gas guard (SOL):** independently halt if native SOL falls below
  `MIN_SOL_GAS_LAMPORTS` (can't pay tips/fees). For base=SOL this collapses into the
  existing single check; for base=USDC it is a new, separate read.

### Section 5 — Cycle source, thresholds & symbols

- **Cycle source:** `src/main.rs:897` passes `config.base_token.mint` to
  `bellman_ford::find_negative_cycles_with_diag` instead of `sol_mint`; the diagnostic
  `graph.log_rates` (line 553) likewise.
- **Thresholds (base-unit):** `MIN_PROFIT_LAMPORTS`, `INPUT_SOL_LAMPORTS`, probe sizes,
  etc. are interpreted in the base token's smallest unit. Defaults keep SOL values;
  `.env.example` documents USDC equivalents (6-dp). User-facing names get base-neutral
  aliases (e.g. `INPUT_BASE_UNITS`) with the old names still accepted for compat.
- **Display:** `mint_symbol` already maps mints→symbols; logs print the active base symbol
  so "profit: 0.02 SOL" vs "0.02 USDC" reads correctly.

## Affected files

| File | Change |
|---|---|
| `src/dex/types.rs` | Add `BaseToken` struct + static token table + resolver. |
| `src/config.rs` | Parse `BASE_MINT`; add `base_token`; force-disable flash loan for non-native; add `MIN_SOL_GAS_LAMPORTS`; base-neutral threshold aliases. |
| `src/arbitrage/evaluator.rs` | `is_native`-aware setup/teardown; tip conversion in `compute_jito_tip`. |
| `src/arbitrage/opportunity.rs` | Field/doc comments base-neutral (denomination semantics unchanged). |
| `src/main.rs` | Use `base_token.mint` for cycle source + log_rates; dual-guard halt; wire `SolPriceCache`. |
| `src/portfolio/pricer.rs` / `watcher.rs` | Publish SOL price into `SolPriceCache`. |
| `.env.example` | Document `BASE_MINT`, base-unit thresholds, `MIN_SOL_GAS_LAMPORTS`, USDC notes. |

## Testing

All in the existing `#[cfg(test)]` per-file style:

- **Token table / resolver:** known symbols resolve with correct decimals + `is_native`;
  unknown mint errors.
- **Funding branch:** `is_native=true` emits wrap (`transfer`+`sync_native`) and
  teardown `close_account`; `is_native=false` emits none of these and leaves the base ATA
  intact.
- **Tip conversion:** with a fixed cached SOL price, a known USDC gross profit produces
  the expected lamport tip; a stale/absent price falls back to the floor tip.
- **Dual guard:** USDC drawdown beyond floor halts; SOL below gas floor halts
  independently; base=SOL collapses to today's single-guard behavior.

## Operational prerequisites for running base=USDC (not code)

1. `pools.json` must contain USDC-quoted pools for USDC cycles to exist
   (`node scripts/fetch_all.js`), then `--init-alt` to extend the ALT.
2. Wallet must hold USDC working capital **and** a SOL balance above the gas floor.
3. The portfolio pricer must be running so the SOL price cache stays fresh (else tips
   fall back to floor).

## Risks

- **Tip mis-sizing on stale price** — mitigated by the staleness floor-tip fallback.
- **Threshold misconfiguration (9-dp vs 6-dp)** — mitigated by base-unit semantics +
  documented USDC values + startup logging of the resolved base symbol/decimals.
- **Thin USDC liquidity / few cycles** — operational, not a code risk; surfaced via the
  same diagnostics as SOL cycles.
