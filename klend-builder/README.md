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
export KLEND_MARKET="5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua"  # "xStocks Market" (resolved — see below)
# optional: KLEND_BUILDER_PORT (default 8181), KLEND_SLOT_DURATION_MS (default 450)

npm run typecheck      # ← DO THIS FIRST (see "First-run verification")
npm start              # → klend-builder on :8181
```

### The market (resolved 2026-06-23)

`KLEND_MARKET = 5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua` — Kamino's **"xStocks
Market"** (NOT the Main Market `7u3HeHxY…`). Verified via the Kamino API
(`https://api.kamino.finance/v2/kamino-market`) with every pair mint byte-matched. It
holds NVDAx, SPYx, QQQx, GOOGLx **and** USDC (plus TSLAx, AAPLx, etc.) in one market, so
a cross-margin obligation (USDC + long xStock collateral, short xStock borrow) is
possible. Market ALT: `8ofreL6hKfEet1DnhHVGvCTnSdz4pg85PpbuCUHnEcKm`.

⚠️ **GOOGLx borrow cap = 0** in this market — it is collateral-only, **not borrowable**.
The pairs strategy can short NVDAx/SPYx/QQQx but **never GOOGLx**; the 2c execution layer
must check borrowability and skip/one-side any trade that would short GOOGLx. Still
confirm the live `/market` output on first run — caps and APYs move.

## Verification status

**Type contract — DONE.** `npm run typecheck` (`tsc --noEmit`) passes against the
installed `klend-sdk@7.3.22`, and `package-lock.json` pins that exact tree. Every builder
arg order + accessor in `index.ts` compiles against the real SDK types (a deliberate-error
probe confirmed tsc is genuinely checking the SDK calls, not resolving them to `any`). The
Rust JSON contract is unit-tested offline: `cargo test --lib kamino::`.

**Runtime — still needs a live RPC + wallet (Phase 2b.3).** A type-check can't see these;
walk them on first run:

1. **`npm run typecheck`** — confirm it still passes after any `npm install`/update.
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
