use anyhow::{Context, Result};
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    address_lookup_table::{
        instruction::{create_lookup_table, extend_lookup_table},
        state::AddressLookupTable,
        AddressLookupTableAccount,
    },
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    sysvar,
    transaction::Transaction,
};
use spl_associated_token_account::get_associated_token_address;
use std::collections::HashSet;
use tracing::{info, warn};

use crate::config::{Config, FlashLoanConfig};
use crate::dex::PoolRegistry;
use crate::flash_loan::MARGINFI_PROGRAM_ID;

// ─── Load ─────────────────────────────────────────────────────────────────────

/// Fetch and deserialize a single ALT from the chain.
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

/// Fetch and deserialize all ALTs from a list of addresses.
pub async fn load_alts(rpc: &RpcClient, addresses: &[Pubkey]) -> Result<Vec<AddressLookupTableAccount>> {
    let mut alts = Vec::with_capacity(addresses.len());
    for &addr in addresses {
        alts.push(load_alt(rpc, addr).await?);
    }
    Ok(alts)
}

// ─── Collect ──────────────────────────────────────────────────────────────────

fn bank_liquidity_vault(bank: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"liquidity_vault", bank.as_ref()], program_id).0
}

fn bank_liquidity_vault_authority(bank: &Pubkey, program_id: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"liquidity_vault_authority", bank.as_ref()], program_id).0
}

/// Collect all accounts that should be in the ALT(s).
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
        accounts.insert(pool.dex.program_id());
        accounts.insert(pool.vault_a);
        accounts.insert(pool.vault_b);
        if let Some(state) = pool.state_account {
            accounts.insert(state);
        }
        let e = &pool.extra;
        for opt in [
            e.amm_authority,     e.open_orders,       e.target_orders,      e.market_program,
            e.market,            e.market_bids,        e.market_asks,        e.market_event_queue,
            e.market_coin_vault, e.market_pc_vault,    e.market_vault_signer,
            e.tick_array_0,      e.tick_array_1,       e.tick_array_2,       e.oracle,
            e.clmm_amm_config,   e.clmm_observation,
            e.a_vault_lp,        e.b_vault_lp,         e.a_token_vault,      e.b_token_vault,
            e.a_vault_lp_mint,   e.b_vault_lp_mint,
            e.admin_token_fee_a, e.admin_token_fee_b,
            e.token_program_a,   e.token_program_b,
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

/// Send and confirm a single-instruction transaction, skipping preflight simulation.
/// Preflight must be skipped for create_lookup_table: the simulation bank's SlotHashes
/// may not contain the recent_slot we embedded in the instruction, causing a false
/// "not a recent slot" failure that never occurs when the transaction actually lands.
async fn send_tx(
    rpc: &RpcClient,
    keypair: &Keypair,
    ix: solana_sdk::instruction::Instruction,
) -> Result<()> {
    // Fetch at confirmed so is_blockhash_valid (also at confirmed) sees the same view.
    // A processed blockhash is invisible to confirmed queries and appears immediately expired.
    let (blockhash, _) = rpc
        .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
        .await?;
    let tx = Transaction::new_signed_with_payer(
        &[ix], Some(&keypair.pubkey()), &[keypair], blockhash,
    );
    let sig = rpc
        .send_transaction_with_config(&tx, RpcSendTransactionConfig {
            skip_preflight: true,
            ..Default::default()
        })
        .await
        .context("Failed to send transaction")?;

    // Poll at confirmed commitment — extend_lookup_table fails with Custom(1) if it
    // runs before the create tx has been committed (not just processed).
    loop {
        match rpc.get_signature_status_with_commitment(&sig, CommitmentConfig::confirmed()).await? {
            Some(Ok(())) => return Ok(()),
            Some(Err(e)) => anyhow::bail!("Transaction failed on-chain: {e:?}"),
            None => {}
        }
        if !rpc.is_blockhash_valid(&blockhash, CommitmentConfig::confirmed()).await? {
            anyhow::bail!("Transaction expired — blockhash no longer valid (sig: {sig})");
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// Create a new on-chain ALT and populate it with `accounts` (max 256 per ALT).
/// Returns the new ALT's address.
async fn create_alt_with_accounts(
    rpc: &RpcClient,
    keypair: &Keypair,
    accounts: &[Pubkey],
) -> Result<Pubkey> {
    assert!(accounts.len() <= 256, "ALT capacity is 256 accounts");

    let recent_slot = rpc
        .get_slot_with_commitment(CommitmentConfig::confirmed())
        .await
        .context("Failed to get confirmed slot")?;
    // create_lookup_table returns (Instruction, Pubkey) — instruction first, address second
    let (create_ix, addr) = create_lookup_table(keypair.pubkey(), keypair.pubkey(), recent_slot);
    info!("Creating new ALT {addr} ({} accounts)...", accounts.len());
    send_tx(rpc, keypair, create_ix).await.context("Failed to create ALT")?;

    let n_batches = (accounts.len() + 29) / 30;
    info!("Extending with {} accounts ({n_batches} batches)...", accounts.len());
    for (i, chunk) in accounts.chunks(30).enumerate() {
        let ix = extend_lookup_table(
            addr, keypair.pubkey(), Some(keypair.pubkey()), chunk.to_vec(),
        );
        send_tx(rpc, keypair, ix).await
            .with_context(|| format!("Failed to extend ALT (batch {i})"))?;
        info!("  Batch {}/{n_batches} confirmed", i + 1);
    }

    Ok(addr)
}

/// Extend existing ALTs with any accounts not already covered.
/// Fills ALTs with remaining capacity first; creates new ones for overflow.
/// Returns any newly created ALT addresses (caller should persist these).
async fn extend_existing_alts(
    rpc: &RpcClient,
    keypair: &Keypair,
    existing_addresses: &[Pubkey],
    new_accounts: Vec<Pubkey>,
) -> Result<Vec<Pubkey>> {
    // Collect what's already covered and how much space each ALT has left
    let mut covered: HashSet<Pubkey> = HashSet::new();
    let mut capacities: Vec<(Pubkey, usize)> = Vec::new();
    for &addr in existing_addresses {
        let account = rpc.get_account(&addr).await
            .with_context(|| format!("Failed to load ALT {addr}"))?;
        let table = AddressLookupTable::deserialize(&account.data)
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        covered.extend(table.addresses.iter().copied());
        capacities.push((addr, 256usize.saturating_sub(table.addresses.len())));
    }

    let mut to_add: Vec<Pubkey> = new_accounts.into_iter()
        .filter(|pk| !covered.contains(pk))
        .collect();

    if to_add.is_empty() {
        info!("All ALTs already up to date — no new accounts to add");
        return Ok(vec![]);
    }
    info!("Adding {} new accounts across existing ALTs...", to_add.len());

    // Fill existing ALTs that still have room
    for (addr, cap) in &capacities {
        if to_add.is_empty() { break; }
        if *cap == 0 { continue; }
        let chunk: Vec<Pubkey> = to_add.drain(..to_add.len().min(*cap)).collect();
        info!("Extending ALT {addr} with {} accounts...", chunk.len());
        let n_batches = (chunk.len() + 29) / 30;
        for (i, batch) in chunk.chunks(30).enumerate() {
            let ix = extend_lookup_table(*addr, keypair.pubkey(), Some(keypair.pubkey()), batch.to_vec());
            send_tx(rpc, keypair, ix).await
                .with_context(|| format!("Failed to extend ALT {addr} (batch {i})"))?;
            info!("  Batch {}/{n_batches} confirmed", i + 1);
        }
    }

    // Create new ALTs for any remaining overflow
    let mut new_addrs = Vec::new();
    for chunk in to_add.chunks(256) {
        let addr = create_alt_with_accounts(rpc, keypair, chunk).await?;
        warn!("New ALT {addr} created — add to ALT_ADDRESSES in .env");
        new_addrs.push(addr);
    }
    Ok(new_addrs)
}

/// Create-or-extend ALTs, then return all loaded accounts.
///
///   --init-alt, no ALT_ADDRESSES   → create ALT(s) (256 accounts max each), save to alt.json
///   --init-alt, ALT_ADDRESSES set  → extend existing ALTs with any missing accounts
pub async fn init_alt(
    rpc: &RpcClient,
    keypair: &Keypair,
    config: &Config,
    registry: &PoolRegistry,
    user: Pubkey,
) -> Result<Vec<AddressLookupTableAccount>> {
    let all_accounts = collect_alt_accounts(registry, config.flash_loan.as_ref(), user);
    let unique: Vec<Pubkey> = {
        let mut seen = HashSet::new();
        all_accounts.into_iter().filter(|p| seen.insert(*p)).collect()
    };
    info!("Collected {} unique accounts for ALT(s)", unique.len());

    let final_addresses: Vec<Pubkey> = if config.alt_addresses.is_empty() {
        // Create mode: split into 256-account chunks, one ALT per chunk
        let mut addresses = Vec::new();
        for chunk in unique.chunks(256) {
            let addr = create_alt_with_accounts(rpc, keypair, chunk).await?;
            addresses.push(addr);
        }

        // Persist all addresses
        let json = serde_json::json!({
            "alt_addresses": addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>()
        });
        std::fs::write("alt.json", serde_json::to_string_pretty(&json)?)
            .context("Failed to write alt.json")?;
        let env_val = addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",");
        info!("ALT addresses saved to alt.json");
        info!("Add to .env:  ALT_ADDRESSES={env_val}");

        // Wait ~2 slots for ALTs to activate
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        addresses
    } else {
        // Extend mode: top up existing ALTs, create new ones for overflow
        let new_addrs = extend_existing_alts(rpc, keypair, &config.alt_addresses, unique).await?;
        let mut all = config.alt_addresses.clone();
        all.extend(new_addrs);
        all
    };

    load_alts(rpc, &final_addresses).await
}
