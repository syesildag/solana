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

Solana versioned transactions (`v0` message format) allow account references to be compressed from 32 bytes to 1 byte each by pointing into an on-chain Address Lookup Table (ALT). A pre-created ALT containing all pool/vault/program/MarginFi accounts reduces a 3-hop flash loan tx from ~1945 bytes to ~650 bytes.

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
  cargo run --bin alt-manager -- create
    ├── collect ~187 accounts from pools.json + MarginFi config + user ATAs
    ├── create_lookup_table tx
    ├── ~7 × extend_lookup_table txs (30 accounts per tx)
    ├── wait 2 slots for ALT to activate
    └── prints ALT address → paste into .env as ALT_ADDRESS
```

---

## Files Changed

| File | Change |
|---|---|
| `src/alt/mod.rs` | **new** — `collect_alt_accounts()` + `load_alt()` |
| `src/bin/alt_manager.rs` | **new** — CLI: create / extend / inspect / verify |
| `Cargo.toml` | add `[[bin]]` for alt-manager |
| `src/config.rs` | add `alt_address: Option<Pubkey>` |
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

### `src/alt/mod.rs`

```rust
pub fn collect_alt_accounts(
    registry: &PoolRegistry,
    flash: &FlashLoanConfig,
    user: Pubkey,
) -> Vec<Pubkey>

pub async fn load_alt(
    rpc: &RpcClient,
    address: Pubkey,
) -> Result<AddressLookupTableAccount>
```

`collect_alt_accounts` deduplicates with a `HashSet` before returning. Called by both `alt-manager create/extend` and `alt::load_alt` (for verify).

### `src/bin/alt_manager.rs`

```
USAGE: alt-manager <COMMAND>

Commands:
  create    Create a new ALT and populate from pools.json + .env
  extend    Add new accounts to an existing ALT (after pools.json changes)
  inspect   List all accounts currently in an ALT
  verify    Check that every flash-loan-required account is covered
```

All subcommands read the same `.env` as the bot. `create` batches `extend_lookup_table` calls at 30 accounts per transaction, waits 2 slots after the last extend, then prints the ALT address.

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

Startup (after RPC init):
```rust
let alt = Arc::new(
    alt::load_alt(&rpc, config.alt_address
        .context("ALT_ADDRESS required — run: cargo run --bin alt-manager -- create")?
    ).await?
);
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
# 1. Create and populate the ALT (~10 s)
cargo run --bin alt-manager -- create

# 2. Add the printed address to .env
echo "ALT_ADDRESS=<address>" >> .env

# 3. Verify coverage
cargo run --bin alt-manager -- verify --address <address>

# 4. Build and run
cargo build --release
cargo run --release
# Expected: flash loan txs measure ~650 bytes, 196 bps and 158 bps cycles execute
```

**When pools.json changes:**
```bash
cargo run --bin alt-manager -- extend --address <address>
cargo run --bin alt-manager -- verify --address <address>
# restart the bot to reload the updated ALT
```
