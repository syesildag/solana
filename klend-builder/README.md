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
installed `klend-sdk@7.3.22` (a deliberate-error probe confirmed tsc genuinely checks the
SDK calls, not `any`). Rust JSON contract unit-tested: `cargo test --lib kamino::`.

**Runtime against live mainnet — DONE for read + build (2026-06-23).** Ran the sidecar
against the real xStocks market and confirmed:
- `/health` ok; `KaminoMarket.load` succeeds.
- `/market` returns all 13 reserves. **`borrowApy` is a FRACTION** (NVDAx 0.034 = 3.4%,
  USDC 0.047) → the Rust `borrow_apy_pct = ×100` is correct. `liqThreshold` is 0–1
  (0.65/0.75/0.72/0.70/0.90); decimals 8 (xStocks) / 6 (USDC); `availableLiquidityRaw /
  1e8` matches Kamino's UI.
- `/obligation?owner=…` returns `{exists:false}` for a wallet with no obligation (handled).
- `/build/deposit` (USDC) and `/build/borrow` (NVDAx) each build correctly: 1 computeBudget
  + 6 setup + 1 lending ix, the lending ix being the 17-account klend instruction (exactly
  what we refused to hand-roll). Built unsigned via `createNoopSigner`; nothing submitted.

**Still needs real funds (the true Phase 2b.3):** signing + submitting an actual
deposit/borrow, confirming it lands, and that cross-margin health behaves under a rich-leg
move. That's the only remaining unverified step.

### ⚠️ Dependency pin (do not remove)

`package.json` has an `overrides` forcing `@kamino-finance/farms-sdk` to **exactly
`3.2.24`**. klend-sdk@7.3.22's compiled `seeds.js` requires
`@kamino-finance/farms-sdk/dist/@codegen/farms/programId`, which farms-sdk **removed in
3.2.25+** — but klend-sdk's range is `^3.2.24`, so npm otherwise installs 3.2.26 and the
server crashes at startup with `MODULE_NOT_FOUND`. The pin (+ committed `package-lock.json`)
is what makes it run. If you bump `klend-sdk`, re-check this.

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
