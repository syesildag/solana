# PumpSwap trading (Phase 2) — VALIDATED on-chain, gated default-off

**Status: builder validated against the live program via `simulateTransaction`
(2026-07-25). `ENABLE_PUMPSWAP_TRADING` is default-off. Run the in-context sim on your
own funded cycle (below) before enabling on real funds.**

## What is proven (all from on-chain ground truth, 2026-07-25)

PumpSwap (pump.fun AMM, `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) was a
pricing-only venue. Phase 2 makes it tradeable:

- `src/dex/pumpswap.rs` — `buy`/`sell` builder. The **buy=23 / sell=21 account list is
  the AMM's FULL declared interface**, read from the program's own **on-chain Anchor
  IDL** (fetched + decompressed from the IDL PDA, `5fLnXNNo…`). Not a web guess.
- **Every PDA is asserted equal to live-mainnet constants** in
  `pda_derivations_match_live_mainnet_constants` (`global_config` `ADyA8hde…`,
  `event_authority` `GS4CU59F…`, `global_volume_accumulator` `C2aFPdEN…`, `fee_config`
  `5PHirr8j…`). Discriminators are Anchor `sha256("global:buy"|"sell")[:8]`, verified
  deterministically.
- `FEE_PROGRAM = pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ` and
  `PROTOCOL_FEE_RECIPIENT = 62qc2CNXwrYqQScmEdiZFFAnJR262PxWEuNQtxfafNgV` are sourced
  on-chain and banked as consts; `check_extra` needs only the per-pool
  `pumpswap_coin_creator` (fetcher-emitted).
- **On-chain `simulateTransaction` acceptance:** the exact 23-account `buy` above
  (no buyback tail) was simulated against live state — the program invoked `Buy`,
  resolved *every* account (incl. `fee_config`/`fee_program`), and proceeded into swap
  logic. It failed only on `user_base_token_account` `AccountNotInitialized` (3012) —
  because the probe reused a stranger's uninitialized ATA. In an arb cycle the
  evaluator's `build_setup_instructions` creates the user's base+intermediate ATAs
  before the swap, so that error does not occur.

## The buyback tail is OPTIONAL (why the 23/21 instruction is complete)

Organic swaps append 2–4 trailing `remaining_accounts` after `fee_program`: a rotating
fee-program **`BuybackVault`** (208 B, owned by `pfeeUxB…`) + its quote-mint ATA (and,
for a WSOL quote, a leading init slot). The fee IDL confirms `BuybackVault` has **no PDA
seeds** — it is an indexed account selected at runtime by pump's SDK to feed buyback.
It is NOT part of the declared instruction and NOT required: the simulation above passed
account resolution without it. A static builder correctly omits it.

## Two known limitations (handled; just weigh)

- **Exact-out buy.** PumpSwap `buy` is exact-out on base; the arb model is exact-in. The
  builder maps `base_amount_out = minimum_amount_out` (slippage floor) and
  `max_quote_amount_in = amount_in` — conservative (never overspends/slippage-fails), but
  under-fills buys slightly, leaving a small quote remainder. Fine for cycle closure.
- **Transaction size.** Buy is 23 accounts, sell 21. A SOL→token→SOL cycle through a pump
  leg needs ALT + Jito and will NOT fit the raw no-ALT path (Phase 1); oversized cycles
  are skipped by the 1232-byte guard. PumpSwap trades via flash+Jito+ALT on SOL-base
  cycles — expected.

## Before enabling on real funds (in-context sim gate)

The account structure is validated, but do a final in-context check with YOUR wallet:

1. `ENABLE_PUMPSWAP_TRADING=true`, `cargo run … -- --init-alt` (the 23 swap accounts must
   be in the ALT so a flash cycle fits).
2. `DRY_RUN=true`, find a candidate cycle through a pump pool. Confirm the pre-submission
   `simulateTransaction` returns **success** — the setup instructions create the user
   ATAs, so the 3012 seen in the probe must be gone. Watch for any Anchor constraint
   error (6000–6999) or an unexpected `AccountNotInitialized`.
3. If sim is clean on a real cycle: enable on tiny size, watch the first live outcome, scale.
4. If a token's swaps somehow require the buyback tail (not observed), the fix is to add
   the buyback `remaining_accounts` — but the sim above indicates they are not needed.

## Verified offline (`cargo test --lib dex::pumpswap`, 9 tests)

Discriminators (deterministic sha256), **PDA derivations == live-mainnet constants**, the
declared sell(21)/buy(23) layout + arg encoding + exact-out mapping + token-2022 ATA
threading, the full-instruction emit, and the coin_creator missing-extra guard. Plus the
registry gate parse and default-skip (flag off = byte-identical). The fetcher's
`coin_creator` + token-program decode is validated against the live MANIFEST pool.
