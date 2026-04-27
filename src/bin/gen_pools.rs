/// gen_pools — fetch top Raydium AMM V4 SOL pools and write pools.json
///
/// Usage:
///   cargo run --bin gen_pools -- --output pools.json --min-tvl 500000
///
/// What it does:
///   1. Downloads the Raydium AMM V4 pool list (v3 API, sorted by TVL)
///   2. Keeps only pools where token A or B is WSOL
///   3. Writes a pools.json in the format expected by solana-mev
///
/// All vault/market accounts come directly from Raydium's API — no guessing.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

const WSOL: &str = "So11111111111111111111111111111111111111112";

// ── Raydium v3 API response types ────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct ApiResponse {
    data: ApiData,
}

#[derive(Deserialize, Debug)]
struct ApiData {
    data: Vec<PoolInfo>,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PoolInfo {
    id: String,
    #[serde(rename = "type")]
    pool_type: String,
    mint_a: Mint,
    mint_b: Mint,
    tvl: f64,
    config: PoolConfig,
    keys: PoolKeys,
}

#[derive(Deserialize, Debug)]
struct Mint {
    address: String,
    symbol: String,
    decimals: u8,
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PoolConfig {
    trade_fee_rate: f64, // in bps * 100, e.g. 2500 = 25 bps
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct PoolKeys {
    vault_a: KeyAddr,
    vault_b: KeyAddr,
    authority: KeyAddr,
    open_orders: KeyAddr,
    target_orders: KeyAddr,
    market_program_id: KeyAddr,
    market_id: KeyAddr,
    market_bids: KeyAddr,
    market_asks: KeyAddr,
    market_event_queue: KeyAddr,
    market_base_vault: KeyAddr,
    market_quote_vault: KeyAddr,
    market_authority: KeyAddr,
}

#[derive(Deserialize, Debug)]
struct KeyAddr {
    address: String,
}

// ── Output format (matches PoolConfig in dex/types.rs) ───────────────────────

#[derive(Serialize)]
struct OutPool {
    id: String,
    dex: &'static str,
    token_a: String,
    token_b: String,
    vault_a: String,
    vault_b: String,
    fee_bps: u64,
    extra: OutExtra,
}

#[derive(Serialize)]
struct OutExtra {
    amm_authority: String,
    open_orders: String,
    target_orders: String,
    market_program: String,
    market: String,
    market_bids: String,
    market_asks: String,
    market_event_queue: String,
    market_coin_vault: String,
    market_pc_vault: String,
    market_vault_signer: String,
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let output = arg(&args, "--output").unwrap_or("pools.json".to_string());
    let min_tvl: f64 = arg(&args, "--min-tvl")
        .and_then(|s| s.parse().ok())
        .unwrap_or(500_000.0); // $500k minimum TVL
    let max_pools: usize = arg(&args, "--max-pools")
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);

    eprintln!("Fetching Raydium AMM V4 pools (min TVL ${min_tvl}, max {max_pools} pools) …");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("solana-mev/gen_pools")
        .build()?;

    // Fetch top pools by TVL, filtering for Standard (AMM V4) type
    let url = format!(
        "https://api-v3.raydium.io/pools/info/list\
         ?poolType=Standard&sortField=tvl&sortType=desc&pageSize={max_pools}&page=1"
    );

    let resp: ApiResponse = client
        .get(&url)
        .send()
        .await
        .context("Failed to reach Raydium API")?
        .json()
        .await
        .context("Failed to parse Raydium API response")?;

    let sol_pools: Vec<&PoolInfo> = resp.data.data
        .iter()
        .filter(|p| p.pool_type == "Standard")
        .filter(|p| p.tvl >= min_tvl)
        .filter(|p| p.mint_a.address == WSOL || p.mint_b.address == WSOL)
        .collect();

    eprintln!("Found {} SOL pools above ${} TVL", sol_pools.len(), min_tvl);

    // Deduplicate by pool ID (API can return dupes across pages)
    let mut seen = HashSet::new();
    let mut out: Vec<OutPool> = Vec::new();

    for pool in sol_pools {
        if !seen.insert(pool.id.clone()) {
            continue;
        }

        // Raydium API stores trade_fee_rate as micro-bps (e.g. 2500 = 0.25%)
        // Divide by 100 to get bps
        let fee_bps = (pool.config.trade_fee_rate / 100.0).round() as u64;

        // Ensure token_a is always WSOL for consistent direction
        let (token_a, token_b, vault_a, vault_b, coin_vault, pc_vault) =
            if pool.mint_a.address == WSOL {
                (
                    pool.mint_a.address.clone(),
                    pool.mint_b.address.clone(),
                    pool.keys.vault_a.address.clone(),
                    pool.keys.vault_b.address.clone(),
                    pool.keys.market_base_vault.address.clone(),
                    pool.keys.market_quote_vault.address.clone(),
                )
            } else {
                // Swap A↔B so SOL is always token_a
                (
                    pool.mint_b.address.clone(),
                    pool.mint_a.address.clone(),
                    pool.keys.vault_b.address.clone(),
                    pool.keys.vault_a.address.clone(),
                    pool.keys.market_quote_vault.address.clone(),
                    pool.keys.market_base_vault.address.clone(),
                )
            };

        eprintln!(
            "  {:>12} / {:<12}  TVL=${:.0}  fee={}bps  id={}",
            if token_a == WSOL { "SOL" } else { &token_a[..6] },
            pool.mint_b.symbol,
            pool.tvl,
            fee_bps,
            &pool.id[..8]
        );

        out.push(OutPool {
            id: pool.id.clone(),
            dex: "raydium_amm_v4",
            token_a,
            token_b,
            vault_a,
            vault_b,
            fee_bps,
            extra: OutExtra {
                amm_authority:       pool.keys.authority.address.clone(),
                open_orders:         pool.keys.open_orders.address.clone(),
                target_orders:       pool.keys.target_orders.address.clone(),
                market_program:      pool.keys.market_program_id.address.clone(),
                market:              pool.keys.market_id.address.clone(),
                market_bids:         pool.keys.market_bids.address.clone(),
                market_asks:         pool.keys.market_asks.address.clone(),
                market_event_queue:  pool.keys.market_event_queue.address.clone(),
                market_coin_vault:   coin_vault,
                market_pc_vault:     pc_vault,
                market_vault_signer: pool.keys.market_authority.address.clone(),
            },
        });
    }

    let json = serde_json::to_string_pretty(&out)?;
    std::fs::write(&output, &json)?;
    eprintln!("\nWrote {} pools to {output}", out.len());
    eprintln!("\nNext steps:");
    eprintln!("  1. Verify a few vault addresses on Solscan to confirm they hold the expected tokens");
    eprintln!("  2. Set POOLS_CONFIG_PATH={output} in your .env");
    eprintln!("  3. Restart the bot — only pools above MIN_RESERVE (1 SOL per side) will enter the graph");

    Ok(())
}

fn arg(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}
