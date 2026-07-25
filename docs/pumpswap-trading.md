# PumpSwap trading (Phase 2) — status, the real blocker, and the on-chain gate

**Status: NOT tradeable yet. Fixed account layout VERIFIED against live mainnet; one
blocker remains (the dynamic-fee tail). `ENABLE_PUMPSWAP_TRADING` is default-off and the
builder currently REFUSES to emit an instruction, so nothing can trade PumpSwap.**

## What is done and PROVEN (on-chain, 2026-07-25)

PumpSwap (pump.fun AMM, `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) was a
**pricing-only** venue. Phase 2 builds toward making it tradeable:

- `src/dex/pumpswap.rs` — `buy`/`sell` builder for the **fixed accounts 0..=22**, all PDAs
  derived in-Rust, token-2022 threaded, exact-out buy mapped conservatively.
- **Every PDA derivation is asserted against live-mainnet constants** in
  `pda_derivations_match_live_mainnet_constants` — `global_config` (`ADyA8hde…`),
  `event_authority` (`GS4CU59F…`), `global_volume_accumulator` (`C2aFPdEN…`), and
  `fee_config` (`5PHirr8j…`) all match values read from real swaps. Discriminators are
  Anchor `sha256("global:buy"|"sell")[:8]`, verified deterministically. **This half is
  not a guess — it is confirmed against the deployed program.**
- The two previously-"source it yourself" constants are now **sourced and banked as
  Rust consts**: `FEE_PROGRAM = pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ` and
  `PROTOCOL_FEE_RECIPIENT = 62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV` (a member of the
  rotating GlobalConfig recipient set; the program accepts any member). `check_extra` now
  requires only the per-pool `pumpswap_coin_creator`.
- `ENABLE_PUMPSWAP_TRADING` (default **false**): off ⇒ `PoolRegistry::load` skips pump
  pools exactly as before (byte-identical; watcher unaffected).

## THE BLOCKER: the variable dynamic-fee tail (needs the fee program's data layout)

Sampling 7 live buy/sell txs showed the current program appends, **after** `fee_program`,
a variable-length dynamic-fee tail:

| ix | trailing accounts (beyond the fixed 0..=22 the builder emits) |
|----|---------------------------------------------------------------|
| buy | idx 23 = uninitialized slot; idx 24 = fee-recipient config PDA (owned by `pfeeUxB…`, 208 B); idx 25 = that PDA's quote-mint ATA — and MORE when >1 recipient (observed 26–27 total accounts) |
| sell | idx 21 = fee-recipient config PDA; idx 22 = its ATA — 23–26 total |

The count varies (2–4 trailing) because the set of active fee recipients varies, and the
recipient config PDA **differs per pool** (e.g. `9M4giFF…` vs `EHAAiTxc…`). Those addresses
come from decoding the `fee_config` account (`5PHirr8j…`, 4073 B, owned by the fee program)
— a layout that is **not in the AMM IDL**. Because of this, `build_swap_instruction`
deliberately **bails** (`"PumpSwap swap incomplete: the dynamic-fee remaining accounts…"`)
rather than emit a 21/23-account tx the program would reject (wasting base+priority fees).

**To finish Phase 2 (the one remaining task):**
1. Get the `pfeeUxB…` fee program's IDL / account layout (its Anchor IDL PDA, or a
   community SDK). Decode `fee_config` (`5PHirr8j…`) to enumerate active fee recipients.
2. For each active recipient, derive its config PDA + quote-mint ATA and append them (plus
   the buy's leading slot) to the instruction — matching the live per-pool account list.
   Cross-check a rebuilt instruction's account set byte-for-byte against a real recent swap
   for the same pool before trusting it.
3. Remove the `bail!` and add a builder test that reproduces a real swap's full account
   list.

## Two known limitations (already handled, just weigh them)

- **Exact-out buy.** PumpSwap `buy` is exact-out on base; the arb model is exact-in. The
  builder maps `base_amount_out = minimum_amount_out` (slippage floor) and
  `max_quote_amount_in = amount_in` — conservative (never overspends/slippage-fails) but
  under-fills buys slightly, leaving a small quote remainder. Fine for cycle closure.
- **Transaction size.** Buy is 26+ accounts, sell 23+. A SOL→token→SOL cycle through a pump
  leg needs ALT + Jito; it will NOT fit the raw no-ALT path (Phase 1), and oversized cycles
  are skipped by the 1232-byte guard. So "joins the raw path" is aspirational — realistically
  PumpSwap trades via flash+Jito+ALT on SOL-base cycles. Expected, not a bug.

## MANDATORY: on-chain simulation gate before enabling (after the blocker is cleared)

1. Finish the dynamic-fee tail (above); the builder must stop bailing.
2. `cargo run --release --bin solana-mev -- --init-alt` (the 26+ swap accounts must be in
   the ALT or the tx won't fit).
3. `DRY_RUN=true`, find a candidate cycle through a pump pool, confirm the pre-submission
   `simulateTransaction` returns **success** — not `ProgramAccountNotFound`, no Anchor
   constraint error (6000–6999), no `AccountNotInitialized`. That log line is the gate.
4. If sim fails, likeliest culprits: the dynamic-fee tail (wrong recipient config PDA / ATA
   / count), then account-order drift (re-check a live tx), then the exact-out buy sizing.
5. Only after a clean sim on a real cycle: enable on tiny size, watch the first outcome, scale.

## Verified offline (trust without the chain)

`cargo test --lib dex::pumpswap` (9 tests): discriminators (deterministic sha256),
**PDA derivations == live-mainnet constants**, the fixed sell(21)/buy(23) account
layout + arg encoding + exact-out mapping + token-2022 ATA threading, the
builder-bails-until-tail-implemented guard, and the coin_creator missing-extra guard.
Plus the registry gate parse and default-skip (flag off = unchanged). The fetcher's
`coin_creator` + token-program decode is validated against the live MANIFEST pool.
