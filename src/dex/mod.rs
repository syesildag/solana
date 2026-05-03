pub mod dlmm;
pub mod meteora;
pub mod orca;
pub mod phoenix;
pub mod raydium_amm;
pub mod raydium_clmm;
pub mod types;

use anyhow::{Context, Result};
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::sync::Arc;
use types::{DexKind, Pool, PoolConfig, PoolExtra};

/// Central registry mapping pool ID → Pool and vault → Pool (for fast gRPC updates).
pub struct PoolRegistry {
    /// pool_id → pool
    pools: DashMap<Pubkey, Arc<Pool>>,
    /// vault_pubkey → pools (Meteora DAMM shares vaults across multiple pools)
    vault_index: DashMap<Pubkey, Vec<Arc<Pool>>>,
    /// state_account → pool (for CL pools that expose sqrt_price in their state)
    state_index: DashMap<Pubkey, Arc<Pool>>,
    /// Meteora DAMM a_vault_lp / b_vault_lp → (pool, is_vault_a)
    lp_index: DashMap<Pubkey, (Arc<Pool>, bool)>,
}

impl PoolRegistry {
    pub fn load(path: &str) -> Result<Self> {
        let data = std::fs::read_to_string(path)
            .with_context(|| format!("Cannot read pool config: {path}"))?;
        let configs: Vec<PoolConfig> =
            serde_json::from_str(&data).context("Invalid pool config JSON")?;

        let registry = Self {
            pools: DashMap::new(),
            vault_index: DashMap::new(),
            state_index: DashMap::new(),
            lp_index: DashMap::new(),
        };

        for cfg in configs {
            let pool: Arc<Pool> = Arc::try_from(cfg)?;
            registry.vault_index.entry(pool.vault_a).or_default().push(Arc::clone(&pool));
            registry.vault_index.entry(pool.vault_b).or_default().push(Arc::clone(&pool));
            if let Some(state_acc) = pool.state_account {
                registry.state_index.insert(state_acc, Arc::clone(&pool));
            }
            if let Some(lp_a) = pool.extra.a_vault_lp {
                registry.lp_index.insert(lp_a, (Arc::clone(&pool), true));
            }
            if let Some(lp_b) = pool.extra.b_vault_lp {
                registry.lp_index.insert(lp_b, (Arc::clone(&pool), false));
            }
            registry.pools.insert(pool.id, pool);
        }

        Ok(registry)
    }

    #[allow(dead_code)]
    pub fn get_by_pool_id(&self, id: &Pubkey) -> Option<Arc<Pool>> {
        self.pools.get(id).map(|r| Arc::clone(r.value()))
    }

    pub fn get_by_vault(&self, vault: &Pubkey) -> Option<Vec<Arc<Pool>>> {
        self.vault_index.get(vault).map(|r| r.value().clone())
    }

    pub fn get_by_state_account(&self, acc: &Pubkey) -> Option<Arc<Pool>> {
        self.state_index.get(acc).map(|r| Arc::clone(r.value()))
    }

    /// Returns the Meteora DAMM pool whose vault LP token account matches,
    /// and whether it is vault_a (true) or vault_b (false).
    pub fn get_by_lp_account(&self, acc: &Pubkey) -> Option<(Arc<Pool>, bool)> {
        self.lp_index.get(acc).map(|r| (Arc::clone(&r.value().0), r.value().1))
    }

    /// All account pubkeys to subscribe to in gRPC (vaults + CL state accounts + DAMM LP accounts).
    pub fn subscribe_accounts(&self) -> Vec<Pubkey> {
        let mut accounts: HashSet<Pubkey> = self.vault_index.iter()
            .map(|r| *r.key())
            .collect();
        for r in self.state_index.iter() {
            accounts.insert(*r.key());
        }
        for r in self.lp_index.iter() {
            accounts.insert(*r.key());
        }
        accounts.into_iter().collect()
    }

    /// Find the best pool connecting token_a → token_b (in either direction).
    #[allow(dead_code)]
    pub fn find_pool(&self, token_a: &Pubkey, token_b: &Pubkey) -> Option<Arc<Pool>> {
        for r in self.pools.iter() {
            let p = r.value();
            if (&p.token_a == token_a && &p.token_b == token_b)
                || (&p.token_a == token_b && &p.token_b == token_a)
            {
                return Some(Arc::clone(p));
            }
        }
        None
    }

    pub fn all_pools(&self) -> Vec<Arc<Pool>> {
        self.pools.iter().map(|r| Arc::clone(r.value())).collect()
    }

    /// Validate that every pool's `extra` accounts required by its swap instruction
    /// builder are present. Returns an error listing all missing fields so the user
    /// can fix pools.json before the bot wastes RPC budget on doomed simulations.
    pub fn validate(&self) -> Result<()> {
        let mut errors: Vec<String> = Vec::new();
        for r in self.pools.iter() {
            let pool = r.value();
            let id = &pool.id.to_string()[..8];
            check_extra(id, pool.dex, &pool.extra, &mut errors);
            // CL pools without state_account can never receive sqrt_price updates —
            // their edges stay at weight=0 (price_bits==0 guard in update_pool) until
            // a vault trade happens to push a non-zero price. Warn so the user knows
            // to add `state_account` to pools.json.
            if matches!(pool.dex, DexKind::RaydiumClmm | DexKind::OrcaWhirlpool | DexKind::MeteoraDlmm | DexKind::Phoenix)
                && pool.state_account.is_none()
            {
                tracing::warn!(
                    "Pool {}... ({:?}): no state_account in config — \
                     sqrt_price won't be pre-fetched and edges may stay at zero until \
                     a vault trade triggers a gRPC update",
                    id,
                    pool.dex,
                );
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("Pool config validation failed:\n{}", errors.join("\n"))
        }
    }
}

fn check_extra(id: &str, dex: DexKind, ex: &PoolExtra, errors: &mut Vec<String>) {
    let mut missing: Vec<&str> = Vec::new();
    match dex {
        DexKind::OrcaWhirlpool => {
            if ex.tick_array_0.is_none() { missing.push("tick_array_0"); }
            if ex.tick_array_1.is_none() { missing.push("tick_array_1"); }
            if ex.tick_array_2.is_none() { missing.push("tick_array_2"); }
            if ex.oracle.is_none()       { missing.push("oracle"); }
        }
        DexKind::RaydiumAmmV4 => {
            if ex.amm_authority.is_none()     { missing.push("amm_authority"); }
            if ex.open_orders.is_none()        { missing.push("open_orders"); }
            if ex.target_orders.is_none()      { missing.push("target_orders"); }
            if ex.market_program.is_none()     { missing.push("market_program"); }
            if ex.market.is_none()             { missing.push("market"); }
            if ex.market_bids.is_none()        { missing.push("market_bids"); }
            if ex.market_asks.is_none()        { missing.push("market_asks"); }
            if ex.market_event_queue.is_none() { missing.push("market_event_queue"); }
            if ex.market_coin_vault.is_none()  { missing.push("market_coin_vault"); }
            if ex.market_pc_vault.is_none()    { missing.push("market_pc_vault"); }
            if ex.market_vault_signer.is_none(){ missing.push("market_vault_signer"); }
        }
        DexKind::MeteoraDamm => {
            if ex.a_vault_lp.is_none()      { missing.push("a_vault_lp"); }
            if ex.b_vault_lp.is_none()      { missing.push("b_vault_lp"); }
            if ex.a_token_vault.is_none()   { missing.push("a_token_vault"); }
            if ex.b_token_vault.is_none()   { missing.push("b_token_vault"); }
            if ex.a_vault_lp_mint.is_none() { missing.push("a_vault_lp_mint"); }
            if ex.b_vault_lp_mint.is_none() { missing.push("b_vault_lp_mint"); }
            if ex.admin_token_fee_a.is_none(){ missing.push("admin_token_fee_a"); }
            if ex.admin_token_fee_b.is_none(){ missing.push("admin_token_fee_b"); }
        }
        DexKind::RaydiumClmm => {
            if ex.clmm_amm_config.is_none()  { missing.push("clmm_amm_config"); }
            if ex.clmm_tick_spacing.is_none(){ missing.push("clmm_tick_spacing"); }
        }
        DexKind::MeteoraDlmm => {
            if ex.dlmm_bin_step.is_none() { missing.push("dlmm_bin_step"); }
        }
        DexKind::Phoenix => {
            if ex.phoenix_base_lot_size.is_none()  { missing.push("phoenix_base_lot_size"); }
            if ex.phoenix_quote_lot_size.is_none() { missing.push("phoenix_quote_lot_size"); }
        }
    }
    if !missing.is_empty() {
        errors.push(format!("  {}... ({:?}): missing {}", id, dex, missing.join(", ")));
    }
}

/// Parse a raw SPL token account's `amount` field (offset 64, 8 bytes LE).
/// Used for vault account updates to get the current reserve.
pub fn parse_spl_token_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 72 {
        return None;
    }
    Some(u64::from_le_bytes(data[64..72].try_into().ok()?))
}

/// Parse a Meteora vault account's `totalAmount` field (Borsh offset 11, 8 bytes LE).
/// Layout after 8-byte Anchor discriminator:
///   enabled (u8) + bumps.vaultBump (u8) + bumps.tokenVaultBump (u8) = 3 bytes → offset 8-10
///   totalAmount (u64) → offset 11-18
pub fn parse_meteora_vault_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 19 {
        return None;
    }
    Some(u64::from_le_bytes(data[11..19].try_into().ok()?))
}

/// Parse a Meteora vault account's `lpMint` pubkey (Borsh offset 115, 32 bytes).
/// Used at startup to find the vault LP mint so we can read its total supply.
pub fn parse_meteora_vault_lp_mint(data: &[u8]) -> Option<Pubkey> {
    if data.len() < 147 {
        return None;
    }
    Pubkey::try_from(&data[115..147]).ok()
}

/// Parse an SPL mint account's `supply` field (offset 36, 8 bytes LE).
pub fn parse_spl_mint_supply(data: &[u8]) -> Option<u64> {
    if data.len() < 44 {
        return None;
    }
    Some(u64::from_le_bytes(data[36..44].try_into().ok()?))
}

/// Parse CL pool state to extract (price_a_to_b as f64, fee_bps).
/// The price is in raw token units: token_b per token_a (no decimal adjustment).
/// For Raydium CLMM, validates the amm_config pubkey from pool state against
/// pool.extra.clmm_amm_config to reject updates from wrong/mismatched accounts.
pub fn parse_cl_pool_state(data: &[u8], pool: &types::Pool) -> Option<(f64, u64)> {
    let result = match pool.dex {
        DexKind::RaydiumClmm   => raydium_clmm::parse_state(data, pool.extra.clmm_amm_config),
        DexKind::OrcaWhirlpool => orca::parse_state(data),
        DexKind::MeteoraDlmm   => dlmm::parse_state(data, pool),
        DexKind::Phoenix       => phoenix::parse_state(data, pool),
        _ => None,
    };
    // Cache tick_current_index to avoid re-deriving it via float arithmetic in the swap builder.
    // Orca: offset 81 (i32); Raydium CLMM: offset 269 (i32).  Valid whenever result is Some.
    if result.is_some() {
        let tick_offset: Option<usize> = match pool.dex {
            DexKind::OrcaWhirlpool => Some(81),
            DexKind::RaydiumClmm   => Some(269),
            _ => None,
        };
        if let Some(off) = tick_offset {
            if let Ok(bytes) = data[off..off + 4].try_into() {
                use std::sync::atomic::Ordering;
                pool.tick_current_index.store(i32::from_le_bytes(bytes), Ordering::Relaxed);
            }
        }
        // For Raydium CLMM, also update the tick array bitmap and observation key.
        if pool.dex == DexKind::RaydiumClmm {
            if let Some(bm) = raydium_clmm::parse_tick_array_bitmap(data) {
                use std::sync::atomic::Ordering;
                for (i, &word) in bm.iter().enumerate() {
                    pool.clmm_tick_array_bitmap[i].store(word, Ordering::Relaxed);
                }
            }
            // observation_key is at offset 201–232 of raw pool state data.
            // Cache it so build_swap_instruction uses the ground-truth address
            // instead of a PDA derivation that may not match older pools.
            if data.len() >= 233 {
                use std::sync::atomic::Ordering;
                let obs_bytes: &[u8; 32] = data[201..233].try_into().unwrap();
                for (i, chunk) in obs_bytes.chunks_exact(8).enumerate() {
                    let word = u64::from_le_bytes(chunk.try_into().unwrap());
                    pool.clmm_observation_key[i].store(word, Ordering::Relaxed);
                }
            }
        }
    }
    result
}

#[cfg(test)]
impl PoolRegistry {
    /// Build a registry directly from a list of pools (test only).
    pub fn from_pools(pools: Vec<Arc<Pool>>) -> Self {
        let registry = Self {
            pools: DashMap::new(),
            vault_index: DashMap::new(),
            state_index: DashMap::new(),
            lp_index: DashMap::new(),
        };
        for pool in pools {
            registry.vault_index.entry(pool.vault_a).or_default().push(Arc::clone(&pool));
            registry.vault_index.entry(pool.vault_b).or_default().push(Arc::clone(&pool));
            registry.pools.insert(pool.id, pool);
        }
        registry
    }
}
