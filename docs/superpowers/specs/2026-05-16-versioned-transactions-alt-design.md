# Versioned Transactions + Address Lookup Table (ALT)

**Date:** 2026-05-16  
**Status:** Implemented  
**Goal:** Fix the recurring `Flash loan tx too large` failures on the best cycles (196 bps, 158 bps) by migrating all Jito bundle transactions from legacy `Transaction` to versioned `VersionedTransaction` backed by an on-chain Address Lookup Table, and pre-create user token accounts so flash loan setup instructions stay minimal.

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
`scripts/fetch_all.js` gains a final step (`scripts/create_atas.js`) that reads `pools.json`, derives every user ATA for every unique mint, and creates any that are missing. This guarantees ATAs exist before the bot starts, allowing the flash loan setup to drop all intermediate `create_associated_token_account_idempotent` instructions from the hot path. WSOL ATA creation stays — it is closed at the end of each bundle and must be re-created each time.

**2. Versioned transactions + Address Lookup Table**  
Solana versioned transactions (`v0` message format) compress account references from 32 bytes to 1 byte each by pointing into an on-chain ALT. A pre-populated ALT containing all pool/vault/program/MarginFi accounts reduces a 3-hop flash loan tx from ~1945 bytes to ~550 bytes.

All bundle transactions — flash loan and wallet-funded — are unified to `VersionedTransaction`. Legacy `Transaction` is removed from the hot path. No separate binary is needed: two CLI flags on the main binary handle ALT lifecycle.

---

## CLI Arguments

Parsed from `std::env::args()` at the very start of `main()`, before any bot logic:

| Flag | Behaviour |
|---|---|
| *(none)* | Load ALT from `ALT_ADDRESS` env var and start bot normally. Hard-errors if `ALT_ADDRESS` is unset. |
| `--init-alt` | Create ALT (if `ALT_ADDRESS` unset, saves address to `alt.json`) or extend existing ALT with missing accounts, then start bot normally. |
| `--inspect-alt` | Load ALT from `ALT_ADDRESS`, print full index → pubkey table, then **exit**. Bot does not start. Requires `ALT_ADDRESS` to be set. |

Both flags are compatible with all env-var configuration (`DRY_RUN`, `ENABLE_FLASH_LOAN`, etc.).

---

## Architecture

```
startup
  main.rs
    ├── parse --init-alt / --inspect-alt from argv
    ├── Config::from_env()   // reads ALT_ADDRESS → Option<Pubkey>
    ├── registry = PoolRegistry::load(pools.json)
    │
    ├── --inspect-alt branch:
    │     alt::load_alt(rpc, ALT_ADDRESS) → print accounts → return Ok(())
    │
    ├── --init-alt branch:
    │     alt::init_alt(rpc, keypair, config, registry, user)
    │       ├── collect_alt_accounts(registry, flash, user)   // ~187 accounts
    │       ├── ALT_ADDRESS set? → extend existing ALT
    │       └── ALT_ADDRESS unset? → create ALT, write address to alt.json
    │
    └── normal branch:
          alt::load_alt(rpc, ALT_ADDRESS)   // hard error if unset
                └── Arc::new(alt) cloned into every spawned task

hot path (every BF cycle)
  optimize_input_and_tip(&alt)
    └── build_opportunity(&alt)
          └── estimate_v0_wire_size(ixs, payer, &alt)   // ~550 bytes now
                └── v0::Message::try_compile(...)

  JitoBundle::build(..., &alt)
    └── build_versioned_tx(ixs, keypair, blockhash, &alt)
          └── v0::Message::try_compile + VersionedTransaction::try_new
    └── JitoBundle { transactions: Vec<VersionedTransaction> }

  simulate_opportunity(..., swap_vtxs: &[VersionedTransaction])
    └── rpc.simulate_transaction_with_config(&vtx, cfg)
```

---

## Files Changed

| File | Action | Change |
|---|---|---|
| `scripts/package.json` | modify | add `@solana/spl-token` dependency |
| `scripts/create_atas.js` | **new** | check and create user ATAs for all mints in pools.json |
| `scripts/fetch_all.js` | modify | add `create_atas.js` as final step |
| `src/alt/mod.rs` | **new** | `load_alt()`, `collect_alt_accounts()`, `init_alt()` |
| `src/config.rs` | modify | add `alt_address: Option<Pubkey>` parsed from `ALT_ADDRESS` |
| `src/flash_loan/mod.rs` | modify | remove intermediate ATA creates; update `end_index`; remove `count_unique_non_wsol_mints` |
| `src/jito/bundle.rs` | modify | `Vec<Transaction>` → `Vec<VersionedTransaction>`; `build()` takes `&AddressLookupTableAccount` |
| `src/arbitrage/evaluator.rs` | modify | `estimate_tx_wire_size` → `estimate_v0_wire_size`; thread `alt` through `build_opportunity` / `optimize_input_and_tip` |
| `src/arbitrage/simulator.rs` | modify | `&[Transaction]` → `&[VersionedTransaction]` |
| `src/main.rs` | modify | `mod alt;`; parse `--init-alt`/`--inspect-alt`; load/init ALT; thread `Arc<AddressLookupTableAccount>` to BF tasks |
| `.env.example` | modify | add `ALT_ADDRESS=` with comment |
| `CLAUDE.md` | modify | document `--init-alt`, `--inspect-alt`, and first-time setup flow |

No new Rust dependencies — `solana-sdk 2.x` already ships `AddressLookupTableAccount`, `v0::Message`, and `VersionedTransaction`.  
`@solana/spl-token` added to `scripts/package.json` for ATA creation in Node.js.

---

## ALT Contents (~187 accounts, limit 256)

**Program IDs (~13)**
- DEX programs: Raydium AMM V4, Raydium CLMM, Orca Whirlpool, Meteora DAMM, DLMM, Phoenix, Lifinity, Invariant, Saber
- Token programs: `spl_token`, `token_2022`, `memo_program`, `associated_token_program`
- Infrastructure: `system_program`, `sysvar::instructions`, MarginFi (`MFv2hWf31Z9kbCa1snEPdcgp8b3wL2KLJ95EAn3r4mJ`)

**Per-pool accounts (~160, from pools.json via `PoolRegistry`)**
- `pool.state_account` (CL/CLOB pools)
- `pool.vault_a`, `pool.vault_b`
- All `pool.extra.*` pubkeys: `amm_authority`, `open_orders`, `market`, `tick_array_0/1/2`, `oracle`, `clmm_amm_config`, `clmm_observation`, `a_vault_lp`, `b_vault_lp`, `a_token_vault`, `b_token_vault`, `a_vault_lp_mint`, `b_vault_lp_mint`, `admin_token_fee_a/b`, `token_program_a/b`

**MarginFi accounts (~6, from .env)**
- `marginfi_group`, `marginfi_account`
- `marginfi_sol_bank`, `marginfi_sol_bank_oracle`
- `bank_liquidity_vault` PDA, `bank_liquidity_vault_authority` PDA

**User ATAs (~15, derived from wallet + each unique mint)**
- WSOL ATA + one per intermediate token (RAY, USDC, USDT, BTC, ETH, mSOL, jitoSOL, EURC, POPCAT, …)

The fee payer / signer cannot go into an ALT. Everything else compresses from 32 bytes to 1 byte.

---

## Component Details

### `scripts/create_atas.js`

Final step of `fetch_all.js`. Reads `WALLET_KEYPAIR_PATH`, `RPC_URL`, `POOLS_CONFIG_PATH` from env. Collects unique non-WSOL mints, derives ATAs, batch-checks existence, creates missing ones in batches of 10. Prints `Created N ATAs, M already existed`.

### `src/alt/mod.rs`

```rust
/// Fetch and deserialize an ALT from the chain.
pub async fn load_alt(rpc: &RpcClient, address: Pubkey) -> Result<AddressLookupTableAccount>

/// Collect all accounts that should appear in flash-loan transactions.
/// Uses PoolRegistry directly — pool/vault/extra pubkeys, program IDs,
/// MarginFi PDAs, and user ATAs. Deduplicates and excludes the signer.
pub fn collect_alt_accounts(
    registry: &PoolRegistry,
    flash: Option<&FlashLoanConfig>,
    user: Pubkey,
) -> Vec<Pubkey>

/// Create-or-extend the ALT based on current config and pool registry.
///   ALT_ADDRESS set   → extend with any missing accounts, return loaded ALT
///   ALT_ADDRESS unset → create new ALT, save address to alt.json, return loaded ALT
pub async fn init_alt(
    rpc: &RpcClient,
    keypair: &Keypair,
    config: &Config,
    registry: &PoolRegistry,
    user: Pubkey,
) -> Result<AddressLookupTableAccount>
```

Extend batches 30 accounts per transaction. After creating a new ALT, waits 1 second (~2 slots) for activation before returning.

### `src/flash_loan/mod.rs`

`build_setup_instructions` drops the intermediate ATA loop — ATAs are guaranteed by `create_atas.js`. Updated transaction layout:

```
[0]      SetComputeUnitLimit
[1]      SetComputeUnitPrice
[2]      CreateATA(WSOL)           ← WSOL closed each bundle, must re-create
[3]      StartFlashloan
[4]      Borrow
[5..4+H] Swap × H
[5+H]    Repay
[6+H]    EndFlashloan  ← end_index = 6 + H
[7+H]    CloseAccount
```

`count_unique_non_wsol_mints` is removed (no longer needed). `end_index` simplifies to `(6 + hops) as u64`.

### `src/jito/bundle.rs`

```rust
pub struct JitoBundle {
    pub transactions: Vec<VersionedTransaction>,   // was Vec<Transaction>
}

pub fn build(
    opportunity: &ArbOpportunity,
    keypair: &Keypair,
    recent_blockhash: Hash,
    config: &Config,
    alt: &AddressLookupTableAccount,               // new
) -> Result<Self>

fn build_versioned_tx(
    ixs: &[Instruction],
    keypair: &Keypair,
    blockhash: Hash,
    alt: &AddressLookupTableAccount,
) -> Result<VersionedTransaction>
```

Both flash-loan and wallet-funded hops use `build_versioned_tx`. `encode()` unchanged — `bincode::serialize` + `bs58::encode` works identically for `VersionedTransaction`. Jito block engine accepts versioned transactions.

### `src/arbitrage/evaluator.rs`

```rust
fn estimate_v0_wire_size(
    ixs: &[Instruction],
    payer: &Pubkey,
    alt: &AddressLookupTableAccount,
) -> usize
```

Uses `v0::Message::try_compile` + a zeroed-signature `VersionedTransaction` for accurate wire-size estimation. The `>1232` guard is retained as a safety net. `optimize_input_and_tip` gains `alt: &AddressLookupTableAccount` parameter, threaded through to `build_opportunity`.

### `src/arbitrage/simulator.rs`

```rust
pub async fn simulate_opportunity(
    opportunity: &ArbOpportunity,
    swap_txs: &[VersionedTransaction],   // was &[Transaction]
    rpc: &RpcClient,
) -> Result<SimOutcome>
```

Body unchanged — `rpc.simulate_transaction_with_config` accepts `&impl SerializableTransaction`, which `VersionedTransaction` satisfies.

### `src/main.rs`

```rust
// At the very top of main(), before any other logic:
let args: Vec<String> = std::env::args().collect();
let init_alt_flag    = args.iter().any(|a| a == "--init-alt");
let inspect_alt_flag = args.iter().any(|a| a == "--inspect-alt");

// After registry + rpc are initialized:
if inspect_alt_flag {
    let addr = config.alt_address.context("ALT_ADDRESS required for --inspect-alt")?;
    let alt = alt::load_alt(&rpc, addr).await?;
    println!("ALT: {addr}  ({} accounts)", alt.addresses.len());
    for (i, pk) in alt.addresses.iter().enumerate() { println!("  [{i:3}] {pk}"); }
    return Ok(());
}
let alt = Arc::new(if init_alt_flag {
    alt::init_alt(&rpc, &keypair, &config, &registry, user).await?
} else {
    let addr = config.alt_address.context("ALT_ADDRESS required — run with --init-alt to create")?;
    alt::load_alt(&rpc, addr).await?
});
```

`alt_bf = Arc::clone(&alt)` added to the BF task clone block. `alt_t = Arc::clone(&alt_bf)` added to the spawn block. Both `optimize_input_and_tip` and `JitoBundle::build` receive `&alt_bf` / `&alt_t`.

---

## Error Handling

| Scenario | Behaviour |
|---|---|
| `ALT_ADDRESS` not set (normal startup) | Hard error: "ALT_ADDRESS required — run with --init-alt to create" |
| `ALT_ADDRESS` not set (`--inspect-alt`) | Hard error: "ALT_ADDRESS required for --inspect-alt" |
| ALT account not found on-chain | Hard error at startup — wrong address or ALT deleted; re-run `--init-alt` |
| ALT missing an account for a flash loan tx | `v0::Message::try_compile` returns error → `estimate_v0_wire_size` returns `usize::MAX` → opportunity skipped with `warn!`; fix by re-running `--init-alt` |
| Extra stale accounts in ALT | Silent — unused entries are harmless |
| New pools added without extending ALT | Same as missing account — caught at size estimation; fix by running `--init-alt` again |

---

## Testing

**Existing tests** — all unit tests in `evaluator.rs` (slippage, tip math, profit identity) are unaffected.

**New unit tests**
- `estimate_v0_wire_size` with a synthetic 200-account ALT returns < 1232 for a 12-instruction probe
- `JitoBundle::encode` round-trips correctly for a `VersionedTransaction`

---

## Rollout

```bash
# 1. Fetch pools + create any missing user ATAs
node scripts/fetch_all.js

# 2. First run — create ALT and start bot (ALT_ADDRESS not yet in .env)
cargo build --release
cargo run --release --bin solana-mev -- --init-alt
# INFO: ALT created: <PUBKEY> — saved to alt.json
# INFO: ALT loaded: 187 accounts
# bot starts normally

# 3. Persist ALT address for future runs
echo "ALT_ADDRESS=$(jq -r .alt_address alt.json)" >> .env

# Subsequent normal runs
cargo run --release --bin solana-mev
# INFO: ALT loaded: 187 accounts
# flash loan txs now ~550 bytes — 196 bps and 158 bps cycles execute

# Inspect ALT at any time
cargo run --release --bin solana-mev -- --inspect-alt

# When pools.json changes (new pools added)
node scripts/fetch_all.js                                    # creates new ATAs
cargo run --release --bin solana-mev -- --init-alt           # extends ALT, starts bot
```

---

## Jito Bypass for Thin Flash Loan Cycles

**Date added:** 2026-05-17  
**Status:** Implemented

### Problem

With `ENABLE_FLASH_LOAN=true`, the Jito tip consumed ~99% of profit on thin cycles (≤ 20 bps gross). After the 9 bps MarginFi flash fee only 7 bps remained, and the Jito tip took the rest → `profitable=0` on all thin cycles despite real AMM dislocations.

### Solution

Two new env vars gate a direct-RPC submission path for thin flash loan cycles:

| Var | Default | Meaning |
|---|---|---|
| `BYPASS_JITO_BUNDLE` | `false` | Enable the feature |
| `JITO_BUNDLE_THRESHOLD` | `20` (bps) | Cycles at or below use direct RPC; above use Jito |

**Routing logic** (`src/arbitrage/evaluator.rs` — `optimize_input_and_tip`):
```rust
let gross_bps = (cycle.gross_ratio() - 1.0) * 10_000.0;
let use_direct = config.enable_flash_loan
    && config.bypass_jito_bundle
    && gross_bps <= config.jito_bundle_threshold_bps;
```

**Fee model** (`evaluate_quotes`):
- Direct path: `tx_fee = BASE_FEE_PER_TX + cu_fee` (1 tx, no tip tx), `jito_tip = 0`
- Jito path: unchanged (`tx_fee = 2*BASE_FEE + cu_fee`, tip from `compute_jito_tip`)

**Bundle** (`src/jito/bundle.rs`): tip tx skipped when `opportunity.use_direct_rpc = true`.

**Submission** (`src/main.rs`): `rpc.send_transaction_with_config()` with `skip_preflight=false`; confirmation polled every 400 ms; 30 s timeout treated as Dropped with exponential backoff.

### Files Changed

| File | Change |
|---|---|
| `src/config.rs` | add `bypass_jito_bundle: bool`, `jito_bundle_threshold_bps: f64` |
| `src/arbitrage/opportunity.rs` | add `use_direct_rpc: bool` |
| `src/arbitrage/evaluator.rs` | thread `use_direct` through `evaluate_quotes`, `ternary_search_net_profit`, `build_opportunity`; conditional tx_fee and jito_tip |
| `src/jito/bundle.rs` | gate tip tx on `!opportunity.use_direct_rpc` |
| `src/main.rs` | add `RpcSendTransactionConfig`, `CommitmentConfig` imports; conditional routing with 30s confirmation poll |
| `.env.example` | document `BYPASS_JITO_BUNDLE` and `JITO_BUNDLE_THRESHOLD` |

### Profit comparison on 16 bps cycle at 50 SOL

| Path | Gross (after flash fee) | Cost | Net kept |
|---|---|---|---|
| Jito (before) | 35M lamports | ~34.65M tip | ~350K |
| Direct RPC (after) | 35M lamports | ~1.2M CU fee | **~33.8M** |

### Usage

```bash
# Enable in .env:
BYPASS_JITO_BUNDLE=true
JITO_BUNDLE_THRESHOLD=20
COMPUTE_UNIT_PRICE_MICRO_LAMPORTS=1000000   # 1 lamport/CU priority fee
```
