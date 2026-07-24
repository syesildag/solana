# PumpSwap trading (Phase 2) — status, gate, and the on-chain verification you must run

**Status: implemented, tested, DEFAULT-OFF. NOT yet verified against the live program.**
Do not enable on real funds until the on-chain `simulateTransaction` gate below passes.

## What this adds

PumpSwap (pump.fun AMM, `pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) was a
**pricing-only** venue (the momentum watcher priced it; the arb bot skipped it). Phase 2
makes it a **tradeable** venue for the arb bot, behind a new opt-in flag:

- `src/dex/pumpswap.rs` — `build_swap_instruction` for `buy`/`sell`, full current account
  layout (buy = 23 accounts, sell = 21), all PDAs derived in-Rust, token-2022 threaded.
- `PoolExtra` gained `pumpswap_coin_creator` / `pumpswap_protocol_fee_recipient` /
  `pumpswap_fee_program`; `check_extra` requires all three for a loaded pump pool.
- `ENABLE_PUMPSWAP_TRADING` (default **false**): off ⇒ `PoolRegistry::load` skips pump
  pools exactly as before (byte-identical; the watcher is unaffected). On ⇒ they load
  and Bellman-Ford builds executable edges through them.
- `scripts/fetch_pumpswap_pools.js` now emits `token_program_a/b` (the vault owners —
  Token vs Token-2022, validated live) and `pumpswap_coin_creator` (pool acct +211).

Sourcing: account order + args from the official `pump-fun/pump-public-docs` IDL
(`idl/pump_amm.json`), cross-checked against a community IDL gist and Bitquery/Shyft
notes. Discriminators are Anchor `sha256("global:buy"|"global:sell")[:8]` and are
**verified deterministically** in a unit test (`discriminators_are_anchor_sha256…`).

## The two constants you must source on-chain (deliberately NOT hardcoded)

No public doc gives these; the builder treats them as **required data** and errors
(`PumpSwap: missing …`) rather than trading on a guess, so a half-configured pool can
never trade:

1. **`pumpswap_protocol_fee_recipient`** — one of `GlobalConfig.protocol_fee_recipients:
   [Pubkey; 8]`. Fetch the `global_config` PDA (`find_program_address([b"global_config"],
   program)`), decode the account, read a currently-valid recipient. The program
   validates it, so it must be one the program currently accepts.
2. **`pumpswap_fee_program`** — the separate fee program that owns the `fee_config` PDA
   (a 2025 addition). Find it from a recent successful PumpSwap buy/sell on Solscan
   (the account at buy-index 22 / sell-index 20), or from the current SDK.

Add both to each pump pool's `extra` in `pools.json` (via a merge step or by hand) once
sourced. `check_extra` will list them as missing at startup until then.

## Two known limitations to weigh

- **Exact-out buy.** PumpSwap `buy` is exact-out on base; the arb model is exact-in. The
  builder maps `base_amount_out = minimum_amount_out` (slippage floor) and
  `max_quote_amount_in = amount_in` — conservative (never overspends, never
  slippage-fails) but it under-fills buys slightly, leaving a small quote remainder. Fine
  for cycle closure; refine later by threading the pre-slippage expected-out.
- **Transaction size.** A `buy` is 23 accounts, `sell` 21. A SOL→token→SOL flash cycle
  through one pump leg needs ALT + Jito; it will NOT fit the raw no-ALT path (Phase 1),
  and cycles too large even with ALT are skipped by the existing 1232-byte guard. So
  "joins the raw path" is aspirational — realistically PumpSwap trades via the
  flash+Jito+ALT path on SOL-base cycles. This is expected, not a bug.

## MANDATORY: on-chain simulation gate before enabling

The account assembly has NOT been run against the live program in this environment. Before
`ENABLE_PUMPSWAP_TRADING=true` on real funds:

1. Source the two constants above; add them to a real pump pool in `pools.json`.
2. `cargo run --release --bin solana-mev -- --init-alt` (the buy/sell accounts must be in
   the ALT or the tx won't fit).
3. Run the bot in `DRY_RUN=true` and find a candidate cycle through the pump pool; confirm
   the pre-submission `simulateTransaction` returns **success** (not
   `ProgramAccountNotFound`, not an Anchor constraint error in 6000–6999, not
   `AccountNotInitialized`). The simulator log line is the gate.
4. If sim fails, the likely culprits in order: wrong `protocol_fee_recipient` (not
   currently accepted), wrong `fee_program`, an account-order drift since this was written
   (re-check against a live tx on Solscan), or the exact-out buy sizing. Fix and re-sim.
5. Only after a clean sim on a real cycle: enable on tiny size, watch the first live
   outcome, scale.

## Verified offline (what you can trust without the chain)

`cargo test --lib dex::pumpswap` — discriminators (deterministic sha256), account
counts/order (buy 23 / sell 21), buy/sell arg encoding, exact-out mapping, token-2022
ATA threading, all-PDA-at-fixed-slot derivation, and the missing-extra guard. Plus the
registry gate parse, the default-skip (flag off = unchanged), and the dispatch
missing-extra error. The fetcher's `coin_creator` + token-program decode is validated
against the live MANIFEST pool (base correctly detected as Token-2022).
