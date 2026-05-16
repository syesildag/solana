# Versioned Transactions + Address Lookup Table (ALT)

**Date:** 2026-05-16  
**Status:** Approved  
**Goal:** Fix the recurring `Flash loan tx too large` failures on the best cycles (196 bps, 158 bps) by migrating all Jito bundle transactions from legacy `Transaction` to versioned `VersionedTransaction` backed by a pre-created on-chain Address Lookup Table.

---

## Problem

Flash loan cycles pack all instructions into a single transaction:
- 2 compute budget instructions
- MarginFi borrow/repay/end (~12 accounts)
- 3 swap instructions (Raydium CLMM ~15 accounts, Orca ~15 accounts, Meteora ~10 accounts)

Total: 40+ unique accounts × 32 bytes = ~1280 bytes in the account section alone, exceeding Solana's 1232-byte wire limit. The two most profitable cycles (196 bps and 158 bps) are blocked on every BF run.

The wallet-funded fallback (one tx per hop) avoids this but caps input at `INPUT_SOL_LAMPORTS` (~0.1–1 SOL) instead of the flash-loan optimal ~7–8 SOL, leaving ~75% of the available profit on the table.

---

## Solution

Two complementary changes:

**1. Pre-create user ATAs via `fetch_all.js`**  
`scripts/fetch_all.js` gains a final step: a new `scripts/create_atas.js` script that reads `pools.json`, derives every user ATA for every unique mint, and creates any that are missing. This guarantees ATAs exist before the bot starts, allowing the flash loan setup to drop all intermediate `create_associated_token_account_idempotent` instructions from the hot path. WSOL ATA creation stays — it is closed at the end of each bundle and must be re-created each time.

**2. Versioned transactions + Address Lookup Table**  
Solana versioned transactions (`v0` message format) allow account references to be compressed from 32 bytes to 1 byte each by pointing into an on-chain Address Lookup Table (ALT). A pre-created ALT containing all pool/vault/program/MarginFi accounts reduces a 3-hop flash loan tx from ~1945 bytes to ~550 bytes (after also removing the intermediate ATA creation instructions).

All bundle transactions — flash loan and wallet-funded — are unified to `VersionedTransaction`. Legacy `Transaction` is removed from the hot path.

---

## Architecture

```
startup
  main.rs
    ├── Config::from_env()           // reads ALT_ADDRESS → Pubkey (hard error if missing)
    └── alt::load_alt(rpc, address)  // getAccountInfo → AddressLookupTableAccount
          └── Arc::new(alt) cloned into every spawned task

hot path (every BF cycle)
  optimize_input_and_tip(&alt)
    └── build_opportunity(&alt)
          └── estimate_v0_wire_size(ixs, payer, &alt)   // ~650 bytes now
                └── v0::Message::try_compile(...)

  JitoBundle::build(..., &alt)
    └── build_versioned_tx(ixs, keypair, blockhash, &alt)
          └── v0::Message::try_compile + VersionedTransaction::try_new
    └── JitoBundle { transactions: Vec<VersionedTransaction> }

  simulate_opportunity(..., swap_vtxs: &[VersionedTransaction])
    └── rpc.simulate_transaction_with_config(&vtx, cfg)
          // SerializableTransaction covers both Transaction and VersionedTransaction

one-time setup (run before starting the bot)
  node scripts/fetch_all.js          // fetches pools.json, then runs create_atas.js:
    └── create_atas.js
          ├── read pools.json → collect all unique mints
          ├── derive ATA for each mint (deterministic: wallet + mint)
          ├── getAccountInfo in batch → find missing ATAs
          └── send createAssociatedTokenAccount txs for any missing

  cargo run --release --bin solana-mev -- --init-alt
    ├── alt::collect_alt_accounts(registry, flash, user)   // ~187 accounts via PoolRegistry
    ├── ALT_ADDRESS set?
    │   ├── yes → load ALT, extend with any missing accounts, continue as normal bot
    │   └── no  → create_lookup_table + extend, write address to alt.json, continue
    └── Arc::new(alt) ready for the BF loop
```

---

## Files Changed

| File | Change |
|---|---|
| `scripts/create_atas.js` | **new** — check and create user ATAs for all mints in pools.json |
| `scripts/fetch_all.js` | add `create_atas.js` as the final step |
| `src/alt/mod.rs` | **new** — `load_alt()`, `collect_alt_accounts()`, `init_alt()` |
| `src/config.rs` | add `alt_address: Option<Pubkey>` |
| `src/flash_loan/mod.rs` | remove intermediate `create_associated_token_account_idempotent` from setup (ATAs guaranteed by fetch_all); keep WSOL ATA creation |
| `src/jito/bundle.rs` | `Vec<Transaction>` → `Vec<VersionedTransaction>`; `build()` takes `&AddressLookupTableAccount` |
| `src/arbitrage/evaluator.rs` | `estimate_tx_wire_size` → `estimate_v0_wire_size` using `v0::Message::try_compile` |
| `src/arbitrage/simulator.rs` | `&[Transaction]` → `&[VersionedTransaction]` |
| `src/main.rs` | load ALT at startup; thread `Arc<AddressLookupTableAccount>`; extract `Vec<VersionedTransaction>` for sim |
| `.env.example` | add `ALT_ADDRESS=` |

No new Cargo dependencies — `solana-sdk 2.x` already includes `AddressLookupTableAccount`, `v0::Message`, and `VersionedTransaction`.

---

## ALT Contents (~187 accounts, limit 256)

**Program IDs (~13)**
- Raydium AMM V4, Raydium CLMM, Orca Whirlpool, Meteora DAMM, DLMM, Phoenix (+ any future DEXes)
- `spl_token`, `token_2022`, `memo_program`, `associated_token_program`, `system_program`
- `sysvar::instructions`
- MarginFi program (`MFv2hWf31Z9kbCa1snEPdcgp8b3wL2KLJ95EAn3r4mJ`)

**Per-pool accounts (~160, from pools.json via `PoolRegistry`)**
- `pool.state_account` (all CL/CLOB pools)
- `pool.vault_a`, `pool.vault_b`
- All `pool.extra.*` fields (tick arrays, oracle, amm_config, a_vault_lp, b_vault_lp, etc.)

**MarginFi accounts (~6, from .env)**
- `marginfi_group`, `marginfi_account` (user's lending account)
- `marginfi_sol_bank`, `marginfi_sol_bank_oracle`
- `bank_liquidity_vault` (PDA — derived at collection time)
- `bank_liquidity_vault_authority` (PDA — derived at collection time)

**User ATAs (~15, derived from wallet pubkey + each unique mint)**
- WSOL ATA + one per intermediate token (RAY, USDC, USDT, BTC, ETH, mSOL, jitoSOL, EURC, POPCAT, …)

The fee payer / keypair signer is the only account that **cannot** go into an ALT (Solana requires signers to be static). Everything else compresses.

---

## Component Details

### `scripts/create_atas.js`

Runs as the final step of `fetch_all.js`. Uses `@solana/web3.js` (already a dependency):

1. Reads `WALLET_KEYPAIR_PATH` and `RPC_URL` from `.env`
2. Reads `pools.json`, collects all unique non-WSOL mints
3. Derives each ATA address: `getAssociatedTokenAddressSync(mint, wallet)`
4. Batch `getMultipleAccountsInfo` to find which are missing
5. Sends `createAssociatedTokenAccount` instructions for missing ones (batched, 10 per tx)
6. Prints a summary: `Created N ATAs, M already existed`

### `src/flash_loan/mod.rs`

`build_setup_instructions` removes the loop that emits `create_associated_token_account_idempotent` for intermediate mints. ATAs are guaranteed to exist (created by `fetch_all.js`). The WSOL ATA `create_associated_token_account_idempotent` call stays — WSOL ATA is closed at bundle teardown and must be re-created each time. This saves ~2 instructions (~200 bytes) from every 3-hop flash loan tx.

### `src/alt/mod.rs`

```rust
/// Load and deserialize an existing ALT from the chain.
pub async fn load_alt(rpc: &RpcClient, address: Pubkey) -> Result<AddressLookupTableAccount>

/// Collect all accounts that should appear in flash-loan transactions.
/// Uses PoolRegistry directly — all pool/vault/extra pubkeys, program IDs,
/// MarginFi PDAs, and user ATAs. Deduplicates and excludes the signer.
pub fn collect_alt_accounts(
    registry: &PoolRegistry,
    flash: Option<&FlashLoanConfig>,
    user: Pubkey,
) -> Vec<Pubkey>

/// Create-or-extend the ALT based on current config and pool registry.
///   - ALT_ADDRESS set   → load, extend with any missing accounts, return loaded ALT
///   - ALT_ADDRESS unset → create new ALT, save address to alt.json, return loaded ALT
pub async fn init_alt(
    rpc: &RpcClient,
    keypair: &Keypair,
    config: &Config,
    registry: &PoolRegistry,
    user: Pubkey,
) -> Result<AddressLookupTableAccount>
```

No separate `alt_manager` binary — both lifecycle operations are flags on the main binary:

### `src/jito/bundle.rs`

```rust
pub struct JitoBundle {
    pub transactions: Vec<VersionedTransaction>,  // was Vec<Transaction>
}

impl JitoBundle {
    pub fn build(
        opportunity: &ArbOpportunity,
        keypair: &Keypair,
        recent_blockhash: Hash,
        config: &Config,
        alt: &AddressLookupTableAccount,           // new
    ) -> Result<Self>
}

fn build_versioned_tx(
    ixs: &[Instruction],
    keypair: &Keypair,
    blockhash: Hash,
    alt: &AddressLookupTableAccount,
) -> Result<VersionedTransaction> {
    let message = v0::Message::try_compile(&keypair.pubkey(), ixs, &[alt.clone()], blockhash)?;
    Ok(VersionedTransaction::try_new(VersionedMessage::V0(message), &[keypair])?)
}
```

Both flash-loan and wallet-funded paths call `build_versioned_tx`. The tip transaction (2 accounts, trivially small) also uses it for uniformity. `encode()` is unchanged — `bincode::serialize` + `bs58::encode` works identically for `VersionedTransaction`.

### `src/arbitrage/evaluator.rs`

```rust
fn estimate_v0_wire_size(
    ixs: &[Instruction],
    payer: &Pubkey,
    alt: &AddressLookupTableAccount,
) -> usize {
    let Ok(message) = v0::Message::try_compile(payer, ixs, &[alt.clone()], &Hash::default())
        else { return usize::MAX };
    let num_sigs = message.header.num_required_signatures as usize;
    let tx = VersionedTransaction {
        signatures: vec![Signature::default(); num_sigs],
        message: VersionedMessage::V0(message),
    };
    bincode::serialized_size(&tx).unwrap_or(u64::MAX) as usize
}
```

`build_opportunity` receives `alt: &AddressLookupTableAccount` (threaded from `optimize_input_and_tip`). The `>1232` guard stays; 3-hop flash loan txs will now measure ~650 bytes.

### `src/arbitrage/simulator.rs`

Only the function signature changes:

```rust
pub async fn simulate_opportunity(
    opportunity: &ArbOpportunity,
    swap_txs: &[VersionedTransaction],   // was &[Transaction]
    rpc: &RpcClient,
) -> Result<SimOutcome>
```

`VersionedTransaction` implements `SerializableTransaction`, so `rpc.simulate_transaction_with_config` compiles without any other changes.

### `src/main.rs`

Parse both flags from `std::env::args()` before the main loop:
```rust
let args: Vec<String> = std::env::args().collect();
let init_alt_flag    = args.iter().any(|a| a == "--init-alt");
let inspect_alt_flag = args.iter().any(|a| a == "--inspect-alt");
```

Startup (after RPC + registry init):
```rust
// --inspect-alt: print ALT contents and exit
if inspect_alt_flag {
    let addr = config.alt_address
        .context("ALT_ADDRESS required for --inspect-alt")?;
    let alt = alt::load_alt(&rpc, addr).await?;
    println!("ALT: {addr}  ({} accounts)", alt.addresses.len());
    for (i, pk) in alt.addresses.iter().enumerate() {
        println!("  [{i:3}] {pk}");
    }
    return Ok(());
}

// --init-alt: create/extend, then start bot normally
let alt = Arc::new(if init_alt_flag {
    alt::init_alt(&rpc, &keypair, &config, &registry, user).await?
} else {
    let addr = config.alt_address
        .context("ALT_ADDRESS required — run with --init-alt to create")?;
    alt::load_alt(&rpc, addr).await?
});
```

Cloned per-task as `alt_bf`. Passed to:
- `optimize_input_and_tip(..., &alt_bf)`
- `JitoBundle::build(..., &alt_bf)`

Swap tx extraction:
```rust
let swap_txs: Vec<VersionedTransaction> = bundle.transactions[..n-1].to_vec();
```

---

## Error Handling

| Scenario | Behaviour |
|---|---|
| `ALT_ADDRESS` not set | Hard error at startup with message pointing to `alt-manager create` |
| ALT account not found on-chain | Hard error at startup — wrong address or ALT deleted |
| ALT missing an account for a flash loan tx | `v0::Message::try_compile` returns error; size estimate returns `usize::MAX`; opportunity skipped with `warn!` pointing to `alt-manager verify` |
| Extra stale accounts in ALT | Silent — unused entries are harmless |
| New pools added without extending ALT | Same as missing account — caught at size estimation, warn logged |

---

## Testing

**Existing tests** — all unit tests in `evaluator.rs` (slippage, tip math, profit identity) are unaffected; they use synthetic pools and do not touch transaction building.

**New unit tests**
- `estimate_v0_wire_size` with a synthetic ALT covering the probe's accounts returns < 1232 for a 3-hop flash loan instruction set
- `JitoBundle::encode` round-trips correctly for a `VersionedTransaction`

**Integration test**
- `alt-manager verify` — run after every `create` or `extend` to confirm all flash-loan-required accounts are covered before starting the bot

---

## Rollout

```bash
# 1. Fetch pools + create any missing user ATAs
node scripts/fetch_all.js
# prints: Created N ATAs, M already existed

# 2. Create ALT and start bot (first time — ALT_ADDRESS not yet in .env)
cargo build --release
cargo run --release --bin solana-mev -- --init-alt
# prints: ALT created: <PUBKEY> — saved to alt.json
# prints: ALT loaded: 187 accounts
# bot starts normally

# 3. Persist the address for future runs
echo "ALT_ADDRESS=$(jq -r .alt_address alt.json)" >> .env

# Subsequent runs (normal)
cargo run --release
# Expected: flash loan txs ~550 bytes — 196 bps and 158 bps cycles execute

# Inspect ALT contents at any time
cargo run --release --bin solana-mev -- --inspect-alt
```

**When pools.json changes:**
```bash
node scripts/fetch_all.js          # creates new ATAs for new mints
cargo run --release --bin solana-mev -- --init-alt  # extends ALT with new accounts, starts bot
```
