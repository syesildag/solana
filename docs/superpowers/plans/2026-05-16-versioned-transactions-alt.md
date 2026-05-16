# Versioned Transactions + ALT Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate `Flash loan tx too large` failures by migrating all Jito bundle transactions to `VersionedTransaction` backed by an on-chain Address Lookup Table, and add `scripts/create_atas.js` to pre-create user token accounts.

**Architecture:** A single `Arc<AddressLookupTableAccount>` is loaded at bot startup and threaded (alongside existing `keypair`/`rpc` Arcs) into each BF task. All transactions use `v0::Message::try_compile` with the ALT. Two optional CLI flags — `--init-alt` (create/extend ALT then start bot) and `--inspect-alt` (print ALT contents and exit) — replace any separate binary. `scripts/create_atas.js` (final step of `fetch_all.js`) ensures user ATAs exist before the bot starts, allowing flash loan setup to drop intermediate ATA creation instructions.

**Tech Stack:** Rust with solana-sdk 2.x (`AddressLookupTableAccount`, `v0::Message`, `VersionedTransaction`), Node.js + `@solana/web3.js` + `@solana/spl-token`.

---

## File Map

| File | Action | Responsibility |
|---|---|---|
| `scripts/package.json` | modify | add `@solana/spl-token` dependency |
| `scripts/create_atas.js` | **create** | check + create user ATAs for all mints in pools.json |
| `scripts/fetch_all.js` | modify | add `create_atas.js` as final step |
| `src/alt/mod.rs` | **create** | `load_alt()`, `collect_alt_accounts()`, `init_alt()` |
| `src/config.rs` | modify | add `alt_address: Option<Pubkey>` |
| `src/flash_loan/mod.rs` | modify | remove intermediate ATA creates; update `end_index` |
| `src/jito/bundle.rs` | modify | `Vec<Transaction>` → `Vec<VersionedTransaction>`; add `build_versioned_tx`; `build()` takes `&AddressLookupTableAccount` |
| `src/arbitrage/evaluator.rs` | modify | `estimate_tx_wire_size` → `estimate_v0_wire_size`; thread `alt` through `build_opportunity` and `optimize_input_and_tip` |
| `src/arbitrage/simulator.rs` | modify | `&[Transaction]` → `&[VersionedTransaction]` |
| `src/main.rs` | modify | `mod alt;`; `--init-alt` / `--inspect-alt` flags; load/init ALT; thread `Arc<AddressLookupTableAccount>` |
| `.env.example` | modify | add `ALT_ADDRESS=` |
| `docs/superpowers/specs/2026-05-16-versioned-transactions-alt-design.md` | modify | document `--init-alt` / `--inspect-alt`; mark implemented |

---

## Task 1: `scripts/create_atas.js` + `package.json`

**Files:**
- Create: `scripts/create_atas.js`
- Modify: `scripts/package.json`

- [ ] **Step 1.1: Add `@solana/spl-token` to package.json**

In `scripts/package.json`, add to `dependencies`:
```json
"@solana/spl-token": "^0.4.9"
```

Run `cd scripts && npm install` to install.

- [ ] **Step 1.2: Write `scripts/create_atas.js`**

```javascript
#!/usr/bin/env node
"use strict";

const { Connection, PublicKey, Keypair, Transaction } = require("@solana/web3.js");
const {
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountInstruction,
  TOKEN_PROGRAM_ID,
} = require("@solana/spl-token");
const fs   = require("fs");
const path = require("path");
const os   = require("os");

const WSOL = "So11111111111111111111111111111111111111112";

async function main() {
  const rpcUrl      = process.env.RPC_URL      || "https://api.mainnet-beta.solana.com";
  const keypairPath = (process.env.WALLET_KEYPAIR_PATH || "~/.config/solana/id.json")
                        .replace(/^~/, os.homedir());
  const poolsPath   = process.env.POOLS_CONFIG_PATH
                        || path.join(__dirname, "../pools.json");

  const wallet = Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(fs.readFileSync(keypairPath, "utf-8")))
  );
  const connection = new Connection(rpcUrl, "confirmed");
  const pools      = JSON.parse(fs.readFileSync(poolsPath, "utf-8"));

  // Collect unique non-WSOL mints from pools.json
  const mints = new Set();
  for (const pool of pools) {
    if (pool.token_a && pool.token_a !== WSOL) mints.add(pool.token_a);
    if (pool.token_b && pool.token_b !== WSOL) mints.add(pool.token_b);
  }
  console.log(`Found ${mints.size} unique non-WSOL mints across ${pools.length} pools`);

  // Derive ATA addresses
  const ataAccounts = [];
  for (const mintStr of mints) {
    const mint = new PublicKey(mintStr);
    const ata  = getAssociatedTokenAddressSync(mint, wallet.publicKey);
    ataAccounts.push({ mint, ata });
  }

  // Batch-check existence (100 per getMultipleAccountsInfo call)
  const missing = [];
  for (let i = 0; i < ataAccounts.length; i += 100) {
    const batch = ataAccounts.slice(i, i + 100);
    const infos = await connection.getMultipleAccountsInfo(batch.map(a => a.ata));
    for (let j = 0; j < batch.length; j++) {
      if (!infos[j]) missing.push(batch[j]);
    }
  }

  if (missing.length === 0) {
    console.log(`All ${ataAccounts.length} ATAs already exist — nothing to create.`);
    return;
  }

  console.log(`Creating ${missing.length} missing ATAs (${ataAccounts.length - missing.length} already exist)...`);

  // Create in batches of 10 per transaction
  for (let i = 0; i < missing.length; i += 10) {
    const batch = missing.slice(i, i + 10);
    const { blockhash } = await connection.getLatestBlockhash();
    const tx = new Transaction({ recentBlockhash: blockhash, feePayer: wallet.publicKey });
    for (const { mint, ata } of batch) {
      tx.add(createAssociatedTokenAccountInstruction(
        wallet.publicKey, ata, wallet.publicKey, mint, TOKEN_PROGRAM_ID,
      ));
    }
    const sig = await connection.sendAndConfirmTransaction(tx, [wallet]);
    console.log(`  Batch ${Math.floor(i / 10) + 1}: created ${batch.length} ATAs (sig: ${sig.slice(0, 8)}...)`);
  }

  console.log(`Done: ${missing.length} ATAs created.`);
}

main().catch(err => { console.error(err); process.exit(1); });
```

- [ ] **Step 1.3: Smoke-test (dry run)**

```bash
cd /Users/serkan/Workspace/solana
RPC_URL="$RPC_URL" WALLET_KEYPAIR_PATH="$WALLET_KEYPAIR_PATH" node scripts/create_atas.js
```

Expected output: `Found N unique non-WSOL mints ... All N ATAs already exist` (or creation lines if some are missing).

- [ ] **Step 1.4: Commit**

```bash
git add scripts/package.json scripts/package-lock.json scripts/create_atas.js
git commit -m "feat(scripts): add create_atas.js to pre-create user token accounts"
```

---

## Task 2: Update `scripts/fetch_all.js`

**Files:**
- Modify: `scripts/fetch_all.js`

- [ ] **Step 2.1: Add `create_atas.js` as the final step**

In `scripts/fetch_all.js`, add `"create_atas.js"` to the end of the `SCRIPTS` array:

```javascript
const SCRIPTS = [
  "fetch_raydium_pools.js",
  "fetch_orca_pools.js",
  "fetch_meteora_pools.js",
  "fetch_meteora_dlmm.js",
  "fetch_phoenix.js",
  "fetch_lifinity_pools.js",
  "fetch_invariant_pools.js",
  "fetch_saber_pools.js",
  "merge_pools.js",
  "create_atas.js",   // ← new: pre-create user ATAs after pools.json is written
];
```

- [ ] **Step 2.2: Verify fetch_all still runs clean**

```bash
cd /Users/serkan/Workspace/solana
RPC_URL="$RPC_URL" WALLET_KEYPAIR_PATH="$WALLET_KEYPAIR_PATH" node scripts/fetch_all.js 2>&1 | tail -20
```

Expected: all scripts succeed; final line is `✓  All fetchers complete — pools.json is up to date.`

- [ ] **Step 2.3: Commit**

```bash
git add scripts/fetch_all.js
git commit -m "feat(scripts): run create_atas.js as final step of fetch_all"
```

---

## Task 3: `src/alt/mod.rs`

**Files:**
- Create: `src/alt/mod.rs`

- [ ] **Step 3.1: Write `src/alt/mod.rs`**

```rust
use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{
        instruction::{create_lookup_table, extend_lookup_table},
        state::AddressLookupTable,
        AddressLookupTableAccount,
    },
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    sysvar,
    transaction::Transaction,
};
use spl_associated_token_account::get_associated_token_address;
use std::collections::HashSet;
use tracing::info;

use crate::config::{Config, FlashLoanConfig};
use crate::dex::PoolRegistry;
use crate::flash_loan::MARGINFI_PROGRAM_ID;

// ─── Load ─────────────────────────────────────────────────────────────────────

/// Fetch and deserialize an ALT from the chain.
pub async fn load_alt(rpc: &RpcClient, address: Pubkey) -> Result<AddressLookupTableAccount> {
    let account = rpc
        .get_account(&address)
        .await
        .with_context(|| format!(
            "ALT {address} not found on-chain — run with --init-alt to create"
        ))?;
    let addresses = AddressLookupTable::deserialize(&account.data)
        .map_err(|e| anyhow::anyhow!("Failed to deserialize ALT {address}: {e:?}"))?
        .addresses
        .to_vec();
    Ok(AddressLookupTableAccount { key: address, addresses })
}

// ─── Collect ──────────────────────────────────────────────────────────────────

fn bank_liquidity_vault(bank: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"liquidity_vault", bank.as_ref()], program_id).0
}

fn bank_liquidity_vault_authority(bank: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"liquidity_vault_authority", bank.as_ref()], program_id).0
}

/// Collect all accounts that should be in the ALT:
///   - Every program ID used by any DEX in the registry
///   - Fixed program IDs (token programs, system, sysvar, memo, MarginFi)
///   - Per-pool: state_account, vault_a, vault_b, all PoolExtra pubkeys
///   - MarginFi protocol accounts + derived PDAs (when flash loan is configured)
///   - User ATAs for every unique mint across all pools
///
/// The signer (user pubkey) is excluded — signers cannot be referenced via ALT.
pub fn collect_alt_accounts(
    registry: &PoolRegistry,
    flash: Option<&FlashLoanConfig>,
    user: Pubkey,
) -> Vec<Pubkey> {
    let marginfi_program: Pubkey = MARGINFI_PROGRAM_ID.parse().expect("valid pubkey");
    let mut accounts: HashSet<Pubkey> = HashSet::new();

    // Fixed program IDs
    for pk in [
        spl_token::id(),
        spl_token_2022::id(),
        spl_associated_token_account::id(),
        solana_sdk::system_program::id(),
        sysvar::instructions::id(),
        "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr".parse().expect("valid pubkey"),
        marginfi_program,
    ] {
        accounts.insert(pk);
    }

    // Per-pool accounts
    let mut mints: HashSet<Pubkey> = HashSet::new();
    for pool in registry.all_pools() {
        // DEX program ID
        accounts.insert(pool.dex.program_id());
        // Vaults + state
        accounts.insert(pool.vault_a);
        accounts.insert(pool.vault_b);
        if let Some(state) = pool.state_account {
            accounts.insert(state);
        }
        // All PoolExtra pubkeys (Raydium AMM V4, Orca, CLMM, DAMM, etc.)
        let e = &pool.extra;
        for opt in [
            e.amm_authority,   e.open_orders,   e.target_orders,   e.market_program,
            e.market,          e.market_bids,   e.market_asks,     e.market_event_queue,
            e.market_coin_vault, e.market_pc_vault, e.market_vault_signer,
            e.tick_array_0,    e.tick_array_1,  e.tick_array_2,    e.oracle,
            e.clmm_amm_config, e.clmm_observation,
            e.a_vault_lp,      e.b_vault_lp,    e.a_token_vault,   e.b_token_vault,
            e.a_vault_lp_mint, e.b_vault_lp_mint,
            e.admin_token_fee_a, e.admin_token_fee_b,
            e.token_program_a, e.token_program_b,
        ] {
            if let Some(pk) = opt {
                accounts.insert(pk);
            }
        }
        mints.insert(pool.token_a);
        mints.insert(pool.token_b);
    }

    // MarginFi accounts + PDAs
    if let Some(flash) = flash {
        let vault = bank_liquidity_vault(&flash.marginfi_sol_bank, &marginfi_program);
        let authority = bank_liquidity_vault_authority(&flash.marginfi_sol_bank, &marginfi_program);
        for pk in [
            flash.marginfi_account, flash.marginfi_group,
            flash.marginfi_sol_bank, flash.marginfi_sol_bank_oracle,
            vault, authority,
        ] {
            accounts.insert(pk);
        }
    }

    // User ATAs for every unique mint
    for mint in mints {
        accounts.insert(get_associated_token_address(&user, &mint));
    }

    accounts.remove(&user);
    accounts.into_iter().collect()
}

// ─── Init ─────────────────────────────────────────────────────────────────────

async fn send_tx(rpc: &RpcClient, keypair: &Keypair, ix: solana_sdk::instruction::Instruction) -> Result<()> {
    let blockhash = rpc.get_latest_blockhash().await?;
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&keypair.pubkey()), &[keypair], blockhash,
    );
    rpc.send_and_confirm_transaction(&tx).await?;
    Ok(())
}

async fn extend_with_accounts(
    rpc: &RpcClient,
    keypair: &Keypair,
    alt_address: Pubkey,
    accounts: Vec<Pubkey>,
) -> Result<()> {
    // Deduplicate and exclude accounts already in the ALT
    let existing: HashSet<Pubkey> = {
        let account = rpc.get_account(&alt_address).await
            .context("Failed to load ALT for extension")?;
        AddressLookupTable::deserialize(&account.data)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?
            .addresses
            .iter()
            .copied()
            .collect()
    };
    let to_add: Vec<Pubkey> = {
        let mut seen = HashSet::new();
        accounts.into_iter()
            .filter(|pk| !existing.contains(pk) && seen.insert(*pk))
            .collect()
    };
    if to_add.is_empty() {
        info!("ALT already up to date — no new accounts to add");
        return Ok(());
    }
    info!("Extending ALT with {} new accounts...", to_add.len());
    for (i, chunk) in to_add.chunks(30).enumerate() {
        let ix = extend_lookup_table(
            alt_address, keypair.pubkey(), Some(keypair.pubkey()), chunk.to_vec(),
        );
        send_tx(rpc, keypair, ix).await
            .with_context(|| format!("Failed to extend ALT (batch {i})"))?;
        info!("  Batch {}/{} confirmed", i + 1, (to_add.len() + 29) / 30);
    }
    Ok(())
}

/// Create-or-extend the ALT, then return the loaded account.
///
///   --init-alt + ALT_ADDRESS set   → extend with any missing accounts
///   --init-alt + ALT_ADDRESS unset → create new ALT, save address to alt.json
pub async fn init_alt(
    rpc: &RpcClient,
    keypair: &Keypair,
    config: &Config,
    registry: &PoolRegistry,
    user: Pubkey,
) -> Result<AddressLookupTableAccount> {
    let accounts = collect_alt_accounts(registry, config.flash_loan.as_ref(), user);
    info!("Collected {} accounts for ALT", accounts.len());

    let alt_address = if let Some(addr) = config.alt_address {
        // Extend existing ALT
        info!("Extending existing ALT {addr}...");
        extend_with_accounts(rpc, keypair, addr, accounts).await?;
        addr
    } else {
        // Create new ALT
        let recent_slot = rpc.get_slot().await.context("Failed to get slot")?;
        let (addr, create_ix) = create_lookup_table(keypair.pubkey(), keypair.pubkey(), recent_slot);
        info!("Creating new ALT {addr}...");
        send_tx(rpc, keypair, create_ix).await.context("Failed to create ALT")?;

        // Deduplicate before extending
        let unique: Vec<Pubkey> = {
            let mut seen = HashSet::new();
            accounts.into_iter().filter(|p| seen.insert(*p)).collect()
        };
        info!("Extending with {} accounts ({} batches)...", unique.len(), (unique.len() + 29) / 30);
        for (i, chunk) in unique.chunks(30).enumerate() {
            let ix = extend_lookup_table(addr, keypair.pubkey(), Some(keypair.pubkey()), chunk.to_vec());
            send_tx(rpc, keypair, ix).await
                .with_context(|| format!("Failed to extend ALT (batch {i})"))?;
            info!("  Batch {}/{} confirmed", i + 1, (unique.len() + 29) / 30);
        }

        // Save address for future runs
        std::fs::write("alt.json", format!("{{\"alt_address\":\"{addr}\"}}"))
            .context("Failed to write alt.json")?;
        info!("ALT address saved to alt.json — add to .env: ALT_ADDRESS={addr}");

        // Wait ~2 slots for ALT to activate
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        addr
    };

    load_alt(rpc, alt_address).await
}
```

- [ ] **Step 3.2: Declare the module in `src/main.rs`**

Add `mod alt;` at the top of `src/main.rs`:

```rust
mod alt;
mod arbitrage;
mod config;
mod dex;
mod flash_loan;
mod graph;
mod jito;
mod streamer;
```

- [ ] **Step 3.3: Verify it compiles (with expected errors elsewhere)**

```bash
cargo build --bin solana-mev 2>&1 | grep "error\[" | head -20
```

Expected: errors in `config.rs`, `bundle.rs`, `evaluator.rs`, `simulator.rs`, `main.rs` — `alt` itself error-free.

- [ ] **Step 3.4: Commit**

```bash
git add src/alt/mod.rs src/main.rs
git commit -m "feat(alt): add load_alt, collect_alt_accounts, init_alt"
```

---

## Task 4: `src/config.rs` — add `alt_address`

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 4.1: Add `alt_address` field to `Config` struct**

In `src/config.rs`, add after the `flash_loan` field:

```rust
/// On-chain Address Lookup Table address for versioned transaction compression.
/// Required at startup — create with `cargo run --bin alt-manager -- create`.
pub alt_address: Option<Pubkey>,
```

- [ ] **Step 4.2: Parse `ALT_ADDRESS` in `from_env()`**

In `Config::from_env()`, add after the `flash_loan` block:

```rust
alt_address: env::var("ALT_ADDRESS")
    .ok()
    .map(|s| s.parse::<Pubkey>().context("ALT_ADDRESS must be a valid pubkey"))
    .transpose()?,
```

- [ ] **Step 4.3: Add `alt_address: None` to the test `Config` in `evaluator.rs`**

In `src/arbitrage/evaluator.rs`, in the `test_config()` function, add the field to the `Config { ... }` literal:

```rust
alt_address: None,
```

- [ ] **Step 4.4: Verify it compiles**

```bash
cargo build --bin solana-mev 2>&1 | grep "error\[" | head -20
```

- [ ] **Step 4.5: Commit**

```bash
git add src/config.rs src/arbitrage/evaluator.rs
git commit -m "feat(config): add alt_address field parsed from ALT_ADDRESS env var"
```

---

## Task 5: `src/flash_loan/mod.rs` — remove intermediate ATA creates

**Files:**
- Modify: `src/flash_loan/mod.rs`

- [ ] **Step 5.1: Update the doc-comment transaction layout**

Replace the layout comment at the top of `build_setup_instructions`:

```rust
/// Build flash-loan setup instructions for tx[0]:
///   1. CreateATA (idempotent) for WSOL  (intermediate ATAs pre-created by create_atas.js)
///   2. LendingAccountStartFlashloan(end_index)
///   3. LendingAccountBorrow(amount_in)
///
/// Transaction layout (ComputeBudget instructions are tx[0] and tx[1]):
///   [0]      SetComputeUnitLimit
///   [1]      SetComputeUnitPrice
///   [2]      CreateATA(WSOL)
///   [3]      StartFlashloan
///   [4]      Borrow
///   [5..4+H] Swap × H
///   [5+H]    Repay
///   [6+H]    EndFlashloan  ← end_index = 6 + H
///   [7+H]    CloseAccount
```

- [ ] **Step 5.2: Remove intermediate ATA loop and update `end_index`**

In `build_setup_instructions`, remove:
```rust
// 1. CreateATA for each unique non-WSOL intermediate mint
let mut seen = std::collections::HashSet::new();
let n = count_unique_non_wsol_mints(path);
for &mint in path {
    if mint != WSOL_PUBKEY && seen.insert(mint) {
        ixs.push(create_associated_token_account_idempotent(
            &user, &user, &mint, &spl_token::id(),
        ));
    }
}
```

And replace the `end_index` line:
```rust
// was: let end_index: u64 = (n + 6 + hops) as u64;
let end_index: u64 = (6 + hops) as u64;
```

Update the comment on the WSOL ATA create:
```rust
// CreateATA for WSOL — closed at teardown, must be re-created each bundle.
// Intermediate ATAs are guaranteed by scripts/create_atas.js.
ixs.push(create_associated_token_account_idempotent(
    &user, &user, &WSOL_PUBKEY, &spl_token::id(),
));
```

- [ ] **Step 5.3: Remove (or make private) `count_unique_non_wsol_mints`**

Remove the `pub` from `count_unique_non_wsol_mints` (or delete the function if unused):

```bash
grep -rn "count_unique_non_wsol_mints" /Users/serkan/Workspace/solana/src/
```

If the function appears only in `flash_loan/mod.rs`, delete it entirely. If it's referenced elsewhere, make it `pub(crate)`.

- [ ] **Step 5.4: Verify it compiles**

```bash
cargo build --bin solana-mev 2>&1 | grep "error\[" | head -20
```

- [ ] **Step 5.5: Commit**

```bash
git add src/flash_loan/mod.rs
git commit -m "feat(flash_loan): remove intermediate ATA creates — pre-created by create_atas.js"
```

---

## Task 6: `src/jito/bundle.rs` — migrate to `VersionedTransaction`

**Files:**
- Modify: `src/jito/bundle.rs`

- [ ] **Step 6.1: Update imports**

Replace the existing `use solana_sdk` block with:

```rust
use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use tracing::debug;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::Instruction,
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};

use crate::arbitrage::opportunity::ArbOpportunity;
use crate::config::Config;
```

- [ ] **Step 6.2: Update `JitoBundle` struct**

```rust
pub struct JitoBundle {
    pub transactions: Vec<VersionedTransaction>,
}
```

- [ ] **Step 6.3: Add `build_versioned_tx` private helper**

Add before `impl JitoBundle`:

```rust
fn build_versioned_tx(
    ixs: &[Instruction],
    keypair: &Keypair,
    blockhash: Hash,
    alt: &AddressLookupTableAccount,
) -> Result<VersionedTransaction> {
    let message = v0::Message::try_compile(
        &keypair.pubkey(),
        ixs,
        &[alt.clone()],
        blockhash,
    )?;
    Ok(VersionedTransaction::try_new(VersionedMessage::V0(message), &[keypair])?)
}
```

- [ ] **Step 6.4: Update `JitoBundle::build` signature and body**

```rust
pub fn build(
    opportunity: &ArbOpportunity,
    keypair: &Keypair,
    recent_blockhash: Hash,
    config: &Config,
    alt: &AddressLookupTableAccount,
) -> Result<Self> {
    let payer = keypair.pubkey();
    let mut txs: Vec<VersionedTransaction> = Vec::new();

    let cu_price = config.compute_unit_price_micro_lamports;

    if config.enable_flash_loan {
        let cu_limit = config.compute_unit_limit.max(1_200_000) as u32;
        let mut ixs: Vec<Instruction> = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
            ComputeBudgetInstruction::set_compute_unit_price(cu_price),
        ];
        ixs.extend(opportunity.setup_instructions.iter().cloned());
        ixs.extend(opportunity.swap_instructions.iter().cloned());
        ixs.extend(opportunity.teardown_instructions.iter().cloned());
        txs.push(build_versioned_tx(&ixs, keypair, recent_blockhash, alt)?);
    } else {
        let cu_limit = config.compute_unit_limit as u32;
        let last_swap = opportunity.swap_instructions.len().saturating_sub(1);
        for (i, ix) in opportunity.swap_instructions.iter().enumerate() {
            let mut ixs: Vec<Instruction> = vec![
                ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
                ComputeBudgetInstruction::set_compute_unit_price(cu_price),
            ];
            if i == 0 {
                ixs.extend(opportunity.setup_instructions.iter().cloned());
            }
            ixs.push(ix.clone());
            if i == last_swap {
                ixs.extend(opportunity.teardown_instructions.iter().cloned());
            }
            txs.push(build_versioned_tx(&ixs, keypair, recent_blockhash, alt)?);
        }
    }

    // Tip transaction
    let tip_account = random_tip_account()?;
    let tip_ix = system_instruction::transfer(&payer, &tip_account, opportunity.jito_tip_lamports);
    txs.push(build_versioned_tx(&[tip_ix], keypair, recent_blockhash, alt)?);

    if txs.len() > 5 {
        anyhow::bail!("Bundle exceeds Jito's 5-transaction limit ({} txs)", txs.len());
    }

    Ok(Self { transactions: txs })
}
```

- [ ] **Step 6.5: Update `encode` and `first_tx`**

`encode` body is unchanged — `bincode::serialize` works on `VersionedTransaction`. Just update the type annotation comment. Update `first_tx`:

```rust
pub fn first_tx(&self) -> Option<&VersionedTransaction> {
    self.transactions.first()
}
```

- [ ] **Step 6.6: Verify it compiles**

```bash
cargo build --bin solana-mev 2>&1 | grep "error\[" | head -20
```

Expected errors only in `evaluator.rs`, `simulator.rs`, and `main.rs` (callers not yet updated).

- [ ] **Step 6.7: Commit**

```bash
git add src/jito/bundle.rs
git commit -m "feat(bundle): migrate JitoBundle to VersionedTransaction via v0::Message + ALT"
```

---

## Task 7: `src/arbitrage/evaluator.rs` — `estimate_v0_wire_size` + thread `alt`

**Files:**
- Modify: `src/arbitrage/evaluator.rs`

- [ ] **Step 7.1: Update imports**

Add to the existing `use solana_sdk` block:

```rust
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    // ... existing imports ...
    message::{v0, VersionedMessage},
    transaction::VersionedTransaction,
};
```

- [ ] **Step 7.2: Replace `estimate_tx_wire_size` with `estimate_v0_wire_size`**

Delete `estimate_tx_wire_size` and add:

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

- [ ] **Step 7.3: Update `build_opportunity` — add `alt` parameter and update size probe**

Change the signature:

```rust
fn build_opportunity(
    cycle: &ArbCycle,
    pools: &[Arc<Pool>],
    user: Pubkey,
    amount_in: u64,
    quote: QuoteResult,
    config: &Config,
    alt: &AddressLookupTableAccount,
) -> Option<ArbOpportunity>
```

Inside, replace the size probe call:
```rust
// was: let wire_size = estimate_tx_wire_size(&probe, &user);
let wire_size = estimate_v0_wire_size(&probe, &user, alt);
```

Also pass `alt` to the wallet-funded fallback call at the bottom of `build_opportunity`:
```rust
return build_opportunity(cycle, &pools, user, wallet_amount, wallet_quote, &wallet_config, alt);
```

- [ ] **Step 7.4: Update `optimize_input_and_tip` — add `alt` parameter and thread it**

Change signature:

```rust
pub fn optimize_input_and_tip(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    available_sol: u64,
    tip_floor: u64,
    alt: &AddressLookupTableAccount,
) -> Option<ArbOpportunity>
```

Thread `alt` to both `build_opportunity` calls inside:
```rust
let result = build_opportunity(cycle, &pools, user, best_amount_in, best_quote, config, alt);
// ...
return build_opportunity(cycle, &pools, user, wallet_amount, wallet_quote, &wallet_config, alt);
```

- [ ] **Step 7.5: Add unit test for `estimate_v0_wire_size`**

Inside the existing `#[cfg(test)]` block in `evaluator.rs`, add after the existing tests:

```rust
#[test]
fn v0_wire_size_with_alt_fits_in_1232_bytes() {
    use solana_sdk::{
        address_lookup_table::AddressLookupTableAccount,
        instruction::{AccountMeta, Instruction},
    };

    // Synthetic ALT with 200 accounts (representative of real bot ALT size)
    let alt_accounts: Vec<Pubkey> = (0..200).map(|_| Pubkey::new_unique()).collect();
    let alt = AddressLookupTableAccount {
        key: Pubkey::new_unique(),
        addresses: alt_accounts.clone(),
    };

    let payer = Pubkey::new_unique();

    // 12 instructions each touching 5 accounts from the ALT — simulates a 3-hop
    // flash loan tx (compute budget × 2 + MarginFi × 4 + swaps × 3 + teardown × 3)
    let ixs: Vec<Instruction> = (0..12)
        .map(|i| Instruction {
            program_id: alt_accounts[i],
            accounts: (0..5)
                .map(|j| AccountMeta::new(alt_accounts[i * 5 + j + 60], false))
                .collect(),
            data: vec![1u8; 16],
        })
        .collect();

    let size = estimate_v0_wire_size(&ixs, &payer, &alt);
    assert!(
        size < 1232,
        "v0 tx with ALT must be < 1232 bytes, got {size}"
    );
}
```

- [ ] **Step 7.6: Run the new test**

```bash
cargo test --bin solana-mev v0_wire_size -- --nocapture
```

Expected: `test v0_wire_size_with_alt_fits_in_1232_bytes ... ok`

- [ ] **Step 7.7: Commit**

```bash
git add src/arbitrage/evaluator.rs
git commit -m "feat(evaluator): estimate_v0_wire_size with ALT compression, thread alt through build_opportunity"
```

---

## Task 8: `src/arbitrage/simulator.rs` — `VersionedTransaction`

**Files:**
- Modify: `src/arbitrage/simulator.rs`

- [ ] **Step 8.1: Update the import and function signature**

Replace:
```rust
use solana_sdk::transaction::{Transaction, TransactionError};
```
With:
```rust
use solana_sdk::transaction::{TransactionError, VersionedTransaction};
```

Change `simulate_opportunity` signature:
```rust
pub async fn simulate_opportunity(
    opportunity: &ArbOpportunity,
    swap_txs: &[VersionedTransaction],
    rpc: &RpcClient,
) -> Result<SimOutcome>
```

The function body is **unchanged** — `rpc.simulate_transaction_with_config(tx, cfg)` accepts `&impl SerializableTransaction`, which `VersionedTransaction` implements.

- [ ] **Step 8.2: Verify it compiles**

```bash
cargo build --bin solana-mev 2>&1 | grep "error\[" | head -20
```

Expected errors only in `main.rs`.

- [ ] **Step 8.3: Commit**

```bash
git add src/arbitrage/simulator.rs
git commit -m "feat(simulator): accept &[VersionedTransaction] for simulation"
```

---

## Task 9: `src/main.rs` — `--init-alt` / `--inspect-alt` flags + thread ALT to BF tasks

**Files:**
- Modify: `src/main.rs`

- [ ] **Step 9.1: Parse both flags at the very top of `main()`**

Add immediately after `dotenvy::dotenv().ok();` (the first line of `main`):

```rust
let args: Vec<String> = std::env::args().collect();
let init_alt_flag    = args.iter().any(|a| a == "--init-alt");
let inspect_alt_flag = args.iter().any(|a| a == "--inspect-alt");
```

- [ ] **Step 9.2: Add `--inspect-alt` early-exit block and `--init-alt` / normal load**

After the `let registry = ...` and `let rpc = ...` lines (after both are initialised, around line 77), add:

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

// Load or initialise the ALT
let alt = Arc::new(if init_alt_flag {
    info!("--init-alt: creating / extending ALT...");
    alt::init_alt(&rpc, &keypair, &config, &registry, user).await?
} else {
    let addr = config.alt_address
        .context("ALT_ADDRESS required — run with --init-alt to create")?;
    info!("Loading ALT {addr}...");
    let table = alt::load_alt(&rpc, addr).await?;
    info!("ALT loaded: {} accounts", table.addresses.len());
    table
});
```

- [ ] **Step 9.3: Clone `alt` for the BF task**

In the block where `_bf` Arc clones are made (around line 566–579), add:

```rust
let alt_bf = Arc::clone(&alt);
```

- [ ] **Step 9.4: Update `optimize_input_and_tip` call**

Find (around line 736):
```rust
let result = arbitrage::evaluator::optimize_input_and_tip(
    c, &registry_bf, &config_bf, user, available_sol, tip_floor_snapshot,
);
```

Change to:
```rust
let result = arbitrage::evaluator::optimize_input_and_tip(
    c, &registry_bf, &config_bf, user, available_sol, tip_floor_snapshot, &alt_bf,
);
```

- [ ] **Step 9.5: Clone `alt_bf` into the spawned submission task**

In the block around line 802 where per-task Arcs are cloned, add:

```rust
let alt_t = Arc::clone(&alt_bf);
```

- [ ] **Step 9.6: Update `JitoBundle::build` call**

```rust
let bundle = match JitoBundle::build(&opportunity, &keypair, blockhash, &config_t, &alt_t) {
```

- [ ] **Step 9.7: Update `swap_txs` extraction**

```rust
let swap_txs: Vec<solana_sdk::transaction::VersionedTransaction> = if sim_run {
    bundle.transactions[..bundle.transactions.len().saturating_sub(1)].to_vec()
} else {
    vec![]
};
```

- [ ] **Step 9.8: Full build must pass**

```bash
cargo build --bin solana-mev 2>&1
```

Expected: zero errors.

- [ ] **Step 9.9: Run all existing tests**

```bash
cargo test --bin solana-mev 2>&1
```

Expected: all pass.

- [ ] **Step 9.10: Commit**

```bash
git add src/main.rs
git commit -m "feat(main): --init-alt and --inspect-alt flags, thread Arc<AddressLookupTableAccount> to BF tasks"
```

---

## Task 10: `.env.example`

**Files:**
- Modify: `.env.example`

- [ ] **Step 10.1: Add `ALT_ADDRESS` entry**

Add after the flash loan section:

```bash
# Address Lookup Table — created once with: cargo run --bin alt-manager -- create
# Required: versioned transactions need the ALT for account compression (both flash-loan and wallet paths).
ALT_ADDRESS=
```

- [ ] **Step 10.2: Commit**

```bash
git add .env.example
git commit -m "docs(.env): add ALT_ADDRESS variable"
```

---

## Task 11: `Cargo.toml` — no new binary needed

`Cargo.toml` requires no changes — `--init-alt` and `--inspect-alt` are flags on the existing `solana-mev` binary.

- [ ] **Step 11.1: Confirm `Cargo.toml` unchanged**

```bash
grep "\[\[bin\]\]" Cargo.toml
```

Expected: only the existing `solana-mev`, `portfolio-cli`, `portfolio-watcher` entries. No `alt-manager` needed.

- [ ] **Step 11.2: Full release build**

```bash
cargo build --release 2>&1
```

Expected: zero errors.


## Task 12: Lint, clippy, final verification

- [ ] **Step 12.1: Run clippy**

```bash
cargo clippy --all-targets 2>&1 | grep "^error" | head -20
```

Expected: no errors (warnings OK).

- [ ] **Step 12.2: Run all tests**

```bash
cargo test --bin solana-mev -- --nocapture 2>&1 | tail -30
```

Expected: all existing tests pass including the new `v0_wire_size_with_alt_fits_in_1232_bytes`.

- [ ] **Step 12.3: Verify release build**

```bash
cargo build --release 2>&1 | tail -5
```

Expected: `Compiling solana-mev ... Finished release`.

- [ ] **Step 12.4: Commit**

```bash
git add -A
git commit -m "chore: fix clippy warnings from ALT migration"
```

---

## Task 13: Update spec `.md`

**Files:**
- Modify: `docs/superpowers/specs/2026-05-16-versioned-transactions-alt-design.md`

- [ ] **Step 13.1: Update status and add CLI argument documentation**

Change `**Status:** Approved` to `**Status:** Implemented`.

Add a `## CLI Arguments` section before `## Rollout`:

```markdown
## CLI Arguments

Both flags are parsed from `std::env::args()` in `main()` before any bot logic runs.

| Flag | Behaviour |
|---|---|
| *(none)* | Load ALT from `ALT_ADDRESS` env var and start bot. Hard-errors if `ALT_ADDRESS` is unset. |
| `--init-alt` | Create ALT (if `ALT_ADDRESS` unset, saves address to `alt.json`) or extend existing ALT with any missing accounts, then start bot normally. |
| `--inspect-alt` | Load ALT from `ALT_ADDRESS`, print index → pubkey table, then **exit** (bot does not start). Requires `ALT_ADDRESS` to be set. |

Both flags can coexist with all other env-var configuration (`DRY_RUN`, `ENABLE_FLASH_LOAN`, etc.).
```

Add an `## Implementation Notes` section at the bottom:

```markdown
## Implementation Notes

- No separate `alt-manager` binary — `--init-alt` and `--inspect-alt` are flags on `solana-mev`.
- `collect_alt_accounts` lives in `src/alt/mod.rs` and uses `PoolRegistry` directly (same crate).
- `count_unique_non_wsol_mints` in `flash_loan/mod.rs` was removed after intermediate ATA loop removal.
- `estimate_tx_wire_size` renamed to `estimate_v0_wire_size`; `>1232` guard retained as safety net.
- Jito bundle API accepts `VersionedTransaction` serialized identically to `Transaction` (bincode → base58).
- New ALT address is saved to `alt.json` on first `--init-alt` run; copy to `.env` for future runs.
```

- [ ] **Step 13.2: Commit**

```bash
git add docs/superpowers/specs/2026-05-16-versioned-transactions-alt-design.md
git commit -m "docs: mark ALT spec implemented; document --init-alt and --inspect-alt arguments"
```

---

## Rollout (after implementation)

```bash
# 1. Fetch pools + create any missing user ATAs
node scripts/fetch_all.js

# 2. First run: create ALT and start bot (ALT_ADDRESS not yet in .env)
cargo build --release
cargo run --release -- --init-alt
# INFO: ALT created: <PUBKEY> — saved to alt.json
# INFO: ALT loaded: 187 accounts
# bot starts normally

# 3. Persist the address for future runs (no --init-alt needed after this)
echo "ALT_ADDRESS=$(jq -r .alt_address alt.json)" >> .env

# Subsequent runs
cargo run --release
# INFO: ALT loaded: 187 accounts
# flash loan txs now ~550 bytes — 196 bps and 158 bps cycles execute

# Inspect ALT at any time
cargo run --release -- --inspect-alt

# When pools.json changes
node scripts/fetch_all.js          # creates new ATAs for new mints
cargo run --release -- --init-alt  # extends ALT with new accounts, starts bot
```
