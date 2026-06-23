# klend-builder

A thin Node sidecar that builds **Kamino `klend`** lending instructions and reads
market/obligation state for the Rust pairs trader (Phase 2b). The bot calls it over
HTTP; the SDK does the hard part (deriving accounts, PDAs and refresh ordering); the
**bot signs and submits** the returned instructions.

This is the **BUY** half of the build-vs-buy decision (see
`docs/superpowers/plans/2026-06-21-onchain-pairs-trader.md`). Hand-rolling klend's long,
version-specific Anchor account lists in Rust was rejected as error-prone and
unverifiable; the official [`@kamino-finance/klend-sdk`](https://github.com/Kamino-Finance/klend-sdk)
does it correctly. Same pattern the bot already uses for the Jupiter Metis swap-api and
`scripts/fetch_all.js`.

## Why a sidecar (not a Rust crate)

`klend-sdk` is `@solana/kit`-native (web3.js v2) TypeScript with no maintained Rust
equivalent. Pulling a klend Rust crate would also fight this repo's pinned `solana-sdk`
version (the same reason `dex::jupiter` is hand-rolled REST rather than the Jupiter
client crate). A small HTTP sidecar keeps the SDK isolated in Node.

## Endpoints

| method | path | body / query | returns |
|---|---|---|---|
| GET | `/health` | — | `{ ok, market, rpc }` |
| GET | `/market` | — | `{ reserves: { SYMBOL: { address, mint, borrowApy, liqThreshold, availableLiquidityRaw, decimals } } }` |
| GET | `/obligation` | `?owner=<pubkey>` | `{ exists, address, userTotalDeposit, userTotalBorrow, borrowLimit, loanToValue, liquidationLtv, netAccountValue }` |
| POST | `/build/:action` | `{ owner, symbol, amount }` (`action` ∈ `deposit｜borrow｜repay｜withdraw`; `amount` = **raw base units** string) | grouped instruction JSON: `{ computeBudgetIxs, setupIxs, inBetweenIxs, lendingIxs, cleanupIxs }` |

Each instruction is `{ programId, accounts: [{ pubkey, isSigner, isWritable }], data: <base64> }`.
The Rust client (`src/portfolio/kamino.rs::KlendClient`) flattens `setup → inBetween →
lending → cleanup` (dropping `computeBudgetIxs`; the bot adds its own) into
`Vec<Instruction>`.

## Setup

```bash
cd klend-builder
npm install

export RPC_URL="https://<your-helius-or-rpc>"      # same RPC the bot uses
export KLEND_MARKET="<lending market pubkey>"       # see "Finding the market" below
# optional: KLEND_BUILDER_PORT (default 8181), KLEND_SLOT_DURATION_MS (default 450)

npm run typecheck      # ← DO THIS FIRST (see "First-run verification")
npm start              # → klend-builder on :8181
```

### Finding the market pubkey

`KLEND_MARKET` is the klend **lending market** that holds the xStocks reserves. Find it
on [app.kamino.finance](https://app.kamino.finance) (the market's address) or by listing
markets with the SDK. The main market and any xStocks-specific market have different
pubkeys — confirm the one whose `/market` output actually lists your pair symbols
(NVDAx, SPYx, GOOGLx, QQQx) and USDC.

## First-run verification (important — this code is not yet run-verified)

The SDK *shapes* were confirmed against `klend-sdk@7.3.22` source, but this sidecar has
**never been executed** here. Anything marked `VERIFY:` in `src/index.ts` is a
convention to confirm against the version `npm install` actually pulls:

1. **`npm run typecheck`** — fixes any `KaminoAction.build*Txns` signature / arg-order
   drift and accessor names (`reserve.address`, `reserve.stats`, `obligation.refreshedStats`).
2. **`curl localhost:8181/health`** then **`/market`** — confirms `KaminoMarket.load`
   args + reserve accessors against a live RPC; check `borrowApy` units (fraction vs
   percent) and `liqThreshold` (should be 0–1).
3. **`/obligation?owner=<your pubkey>`** — confirms the obligation read (returns
   `{exists:false}` before your first deposit).
4. **`/build/deposit`** with a tiny `amount` — confirms the builder + `createNoopSigner`
   path produces instructions. **Do not submit on mainnet yet** — that's Phase 2b.3
   (devnet / tiny real funds, needs the wallet).

The Rust side's JSON contract (`KlendClient`) **is** unit-tested offline:
`cargo test --lib kamino::`.

## Key gotchas (from the SDK source audit)

- **`@solana/kit` v2**, not web3.js v1: `Address`/`Rpc`/bigint `Slot`/`Option`. No
  `PublicKey`/`Connection`.
- **`createNoopSigner(owner)`** is how we build *unsigned* txns: it marks the owner as a
  required signer in the instructions without a key; the bot signs.
- **`buildRepayTxns` puts `currentSlot` as a required positional before `payer`** — the
  other builders trail it with a `0n` default.
- Builders need **`useV2Ixs: true`** and an **`initUserMetadata` object**
  (`{ skipInitialization, skipLutCreation }`) — both `false` so a fresh wallet
  self-initializes its obligation + LUT on first use.
- Obligation health is on **`refreshedStats`** (there is no `.stats`); derive the health
  factor from `liquidationLtv / loanToValue`.
