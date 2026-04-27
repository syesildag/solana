pub mod meteora;
pub mod orca;
pub mod raydium_amm;
pub mod raydium_clmm;
pub mod types;

use anyhow::{Context, Result};
use dashmap::DashMap;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::sync::Arc;
use types::{DexKind, Pool, PoolConfig};

/// Central registry mapping pool ID → Pool and vault → Pool (for fast gRPC updates).
pub struct PoolRegistry {
    /// pool_id → pool
    pools: DashMap<Pubkey, Arc<Pool>>,
    /// vault_pubkey → pool (a vault update tells us which pool changed)
    vault_index: DashMap<Pubkey, Arc<Pool>>,
    /// state_account → pool (for CL pools that expose sqrt_price in their state)
    state_index: DashMap<Pubkey, Arc<Pool>>,
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
        };

        for cfg in configs {
            let pool: Arc<Pool> = Arc::try_from(cfg)?;
            registry.vault_index.insert(pool.vault_a, Arc::clone(&pool));
            registry.vault_index.insert(pool.vault_b, Arc::clone(&pool));
            if let Some(state_acc) = pool.state_account {
                registry.state_index.insert(state_acc, Arc::clone(&pool));
            }
            registry.pools.insert(pool.id, pool);
        }

        Ok(registry)
    }

    #[allow(dead_code)]
    pub fn get_by_pool_id(&self, id: &Pubkey) -> Option<Arc<Pool>> {
        self.pools.get(id).map(|r| Arc::clone(r.value()))
    }

    pub fn get_by_vault(&self, vault: &Pubkey) -> Option<Arc<Pool>> {
        self.vault_index.get(vault).map(|r| Arc::clone(r.value()))
    }

    pub fn get_by_state_account(&self, acc: &Pubkey) -> Option<Arc<Pool>> {
        self.state_index.get(acc).map(|r| Arc::clone(r.value()))
    }

    /// All account pubkeys to subscribe to in gRPC (vaults + CL state accounts).
    pub fn subscribe_accounts(&self) -> Vec<Pubkey> {
        let mut accounts: HashSet<Pubkey> = self.vault_index.iter()
            .map(|r| *r.key())
            .collect();
        for r in self.state_index.iter() {
            accounts.insert(*r.key());
        }
        accounts.into_iter().collect()
    }

    /// Find the best pool connecting token_a → token_b (in either direction).
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
}

/// Parse a raw SPL token account's `amount` field (offset 64, 8 bytes LE).
/// Used for vault account updates to get the current reserve.
pub fn parse_spl_token_amount(data: &[u8]) -> Option<u64> {
    if data.len() < 72 {
        return None;
    }
    Some(u64::from_le_bytes(data[64..72].try_into().ok()?))
}

/// Parse CL pool state to extract (price_a_to_b as f64, fee_bps).
/// The price is in raw token units: token_b per token_a (no decimal adjustment).
pub fn parse_cl_pool_state(data: &[u8], dex: DexKind) -> Option<(f64, u64)> {
    match dex {
        DexKind::RaydiumClmm => raydium_clmm::parse_state(data),
        DexKind::OrcaWhirlpool => orca::parse_state(data),
        _ => None,
    }
}

#[cfg(test)]
impl PoolRegistry {
    /// Build a registry directly from a list of pools (test only).
    pub fn from_pools(pools: Vec<Arc<Pool>>) -> Self {
        let registry = Self {
            pools: DashMap::new(),
            vault_index: DashMap::new(),
            state_index: DashMap::new(),
        };
        for pool in pools {
            registry.vault_index.insert(pool.vault_a, Arc::clone(&pool));
            registry.vault_index.insert(pool.vault_b, Arc::clone(&pool));
            registry.pools.insert(pool.id, pool);
        }
        registry
    }
}
