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
    let blockhash = rpc.get_latest_blockhash().await?;
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

async fn extend_with_accounts(
    rpc: &RpcClient,
    keypair: &Keypair,
    alt_address: Pubkey,
    accounts: Vec<Pubkey>,
) -> Result<()> {
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
        info!("Extending existing ALT {addr}...");
        extend_with_accounts(rpc, keypair, addr, accounts).await?;
        addr
    } else {
        // ALT creation requires a slot in the SlotHashes sysvar (last 512 confirmed slots).
        // Processed commitment is ahead of confirmed and not yet in SlotHashes — use Confirmed.
        let recent_slot = rpc
            .get_slot_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("Failed to get confirmed slot")?;
        // create_lookup_table returns (Instruction, Pubkey) — instruction first, address second
        let (create_ix, addr) = create_lookup_table(keypair.pubkey(), keypair.pubkey(), recent_slot);
        info!("Creating new ALT {addr}...");
        send_tx(rpc, keypair, create_ix).await.context("Failed to create ALT")?;

        let unique: Vec<Pubkey> = {
            let mut seen = HashSet::new();
            accounts.into_iter().filter(|p| seen.insert(*p)).collect()
        };
        info!("Extending with {} accounts ({} batches)...", unique.len(), (unique.len() + 29) / 30);
        for (i, chunk) in unique.chunks(30).enumerate() {
            let ix = extend_lookup_table(
                addr, keypair.pubkey(), Some(keypair.pubkey()), chunk.to_vec(),
            );
            send_tx(rpc, keypair, ix).await
                .with_context(|| format!("Failed to extend ALT (batch {i})"))?;
            info!("  Batch {}/{} confirmed", i + 1, (unique.len() + 29) / 30);
        }

        std::fs::write("alt.json", format!("{{\"alt_address\":\"{addr}\"}}"))
            .context("Failed to write alt.json")?;
        info!("ALT address saved to alt.json — add to .env: ALT_ADDRESS={addr}");

        // Wait ~2 slots for ALT to activate
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        addr
    };

    load_alt(rpc, alt_address).await
}
