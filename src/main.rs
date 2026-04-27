mod arbitrage;
mod config;
mod dex;
mod graph;
mod jito;
mod streamer;

use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::read_keypair_file,
    signer::Signer,
};
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use config::Config;
use dex::PoolRegistry;
use dex::types::WSOL_MINT;
use graph::{bellman_ford, exchange_graph::ExchangeGraph};
use jito::{bundle::JitoBundle, client::JitoClient};
use streamer::{client::GrpcStreamer, subscription::build_account_subscription};

/// Minimum milliseconds between Bellman-Ford runs.
/// Prevents CPU saturation on bursty gRPC update storms.
const BELLMAN_FORD_DEBOUNCE_MS: u64 = 50;

/// Maximum concurrent RPC simulation + Jito bundle submission tasks.
/// Public RPCs typically allow 100 req/s; private ones 200–1000 req/s.
/// Keep this low to avoid triggering rate limits.
const MAX_CONCURRENT_SUBMISSIONS: usize = 2;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            // RUST_LOG takes full precedence; fall back to info only if unset.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("solana_mev=info"))
        )
        .init();

    let config = Arc::new(Config::from_env()?);
    info!("Config loaded. dry_run={}", config.dry_run);

    let keypair = Arc::new(
        read_keypair_file(&config.wallet_keypair_path)
            .map_err(|e| anyhow::anyhow!("Failed to read keypair: {e}"))?,
    );
    let user = keypair.pubkey();
    info!("Wallet: {user}");

    let registry = Arc::new(PoolRegistry::load(&config.pools_config_path)?);
    let account_keys = registry.subscribe_accounts();
    info!(
        "Loaded {} pools, monitoring {} accounts",
        registry.all_pools().len(),
        account_keys.len()
    );

    let rpc = Arc::new(RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::processed(),
    ));

    // ── Pre-fetch initial reserves for all pool vaults via RPC ───────────────
    // The gRPC stream only delivers updates when accounts *change*. Pools with
    // low volume may not update for minutes, leaving their graph edges at NaN
    // weights. Fetching initial balances ensures all edges are valid from the
    // first Bellman-Ford run.
    let graph = Arc::new(ExchangeGraph::new());
    {
        let all_pools = registry.all_pools();
        let vault_pubkeys: Vec<Pubkey> = all_pools.iter()
            .flat_map(|p| [p.vault_a, p.vault_b])
            .collect();

        info!("Fetching initial reserves for {} vaults...", vault_pubkeys.len());
        match rpc.get_multiple_accounts(&vault_pubkeys).await {
            Ok(accounts) => {
                let mut loaded = 0usize;
                for (pool, chunk) in all_pools.iter().zip(accounts.chunks(2)) {
                    if let (Some(Some(acc_a)), Some(Some(acc_b))) = (chunk.get(0), chunk.get(1)) {
                        if let (Some(ra), Some(rb)) = (
                            dex::parse_spl_token_amount(&acc_a.data),
                            dex::parse_spl_token_amount(&acc_b.data),
                        ) {
                            pool.reserve_a.store(ra, Ordering::Relaxed);
                            pool.reserve_b.store(rb, Ordering::Relaxed);
                            graph.update_pool(pool);
                            loaded += 1;
                            debug!("Pool {}: reserve_a={} reserve_b={}", pool.id, ra, rb);
                        }
                    }
                }
                info!("Initialized graph with {}/{} pools from RPC", loaded, all_pools.len());
            }
            Err(e) => {
                warn!("Failed to pre-fetch reserves (will rely on stream updates): {e}");
                for pool in &all_pools {
                    graph.update_pool(pool);
                }
            }
        }
    }

    let jito = Arc::new(JitoClient::new(config.dry_run));
    let sol_mint = Pubkey::from_str(WSOL_MINT)?;

    // ── Rate-limiting primitives ──────────────────────────────────────────────

    // Timestamp (unix ms) of the last Bellman-Ford run. Shared across callbacks.
    let last_bf_ms: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));

    // True while a bundle is in-flight (simulating or submitting).
    // Prevents spawning duplicate submissions for the same detected opportunity.
    let bundle_in_flight: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Caps concurrent simulation + Jito HTTP calls.
    let submit_sem: Arc<Semaphore> = Arc::new(Semaphore::new(MAX_CONCURRENT_SUBMISSIONS));

    // ── Clone Arcs for the callback closure ───────────────────────────────────
    let graph_cb          = Arc::clone(&graph);
    let registry_cb       = Arc::clone(&registry);
    let config_cb         = Arc::clone(&config);
    let rpc_cb            = Arc::clone(&rpc);
    let jito_cb           = Arc::clone(&jito);
    let keypair_cb        = Arc::clone(&keypair);
    let last_bf_ms_cb     = Arc::clone(&last_bf_ms);
    let bundle_in_flight_cb = Arc::clone(&bundle_in_flight);
    let submit_sem_cb     = Arc::clone(&submit_sem);

    let callback = Arc::new(move |pubkey_bytes: [u8; 32], data: Vec<u8>, _slot: u64| {
        let pubkey = Pubkey::from(pubkey_bytes);

        // ── Step 1: update pool state ─────────────────────────────────────────
        if let Some(pool) = registry_cb.get_by_vault(&pubkey) {
            if let Some(amount) = dex::parse_spl_token_amount(&data) {
                use std::sync::atomic::Ordering;
                if pubkey == pool.vault_a {
                    pool.reserve_a.store(amount, Ordering::Relaxed);
                } else {
                    pool.reserve_b.store(amount, Ordering::Relaxed);
                }
                graph_cb.update_pool(&pool);
            }
        } else if let Some(pool) = registry_cb.get_by_state_account(&pubkey) {
            if let Some((sqrt_price, fee_bps)) = dex::parse_cl_pool_state(&data, pool.dex) {
                use std::sync::atomic::Ordering;
                pool.sqrt_price_x64.store(sqrt_price as u64, Ordering::Relaxed);
                pool.fee_bps.store(fee_bps, Ordering::Relaxed);
                graph_cb.update_pool(&pool);
            }
        } else {
            debug!("Received update for untracked account: {pubkey}");
            return;
        }

        // ── Step 2: debounce Bellman-Ford ─────────────────────────────────────
        // Pool updates can arrive thousands of times per second. Running the
        // full graph search on each one would saturate CPU and block the stream.
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let last = last_bf_ms_cb.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < BELLMAN_FORD_DEBOUNCE_MS {
            debug!("Bellman-Ford debounced ({} ms since last run)", now_ms.saturating_sub(last));
            return;
        }
        last_bf_ms_cb.store(now_ms, Ordering::Relaxed);

        // ── Step 3: detect negative cycles ───────────────────────────────────
        let cycles = bellman_ford::find_negative_cycles(&graph_cb, sol_mint);
        if cycles.is_empty() {
            debug!("Bellman-Ford: no negative cycles found");
            return;
        }
        debug!("Bellman-Ford: {} negative cycle(s) detected", cycles.len());
        for (i, c) in cycles.iter().enumerate() {
            debug!(
                "  cycle[{i}] hops={} gross_ratio={:.6} total_weight={:.6}",
                c.edges.len(), c.gross_ratio(), c.total_weight
            );
        }

        // ── Step 4: in-flight guard ───────────────────────────────────────────
        // If a bundle is already being simulated or submitted, skip. Submitting
        // two bundles for the same opportunity wastes RPC calls and may cause
        // both to fail (first one changes prices, invalidating the second).
        if bundle_in_flight_cb.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            debug!("Bundle already in-flight, skipping {} new cycle(s)", cycles.len());
            return;
        }

        // Evaluate the most profitable cycle only (highest gross_ratio)
        let best = cycles
            .iter()
            .filter_map(|c| {
                arbitrage::evaluator::optimize_input_and_tip(
                    c, &registry_cb, &config_cb, user, config_cb.input_sol_lamports,
                )
            })
            .max_by_key(|o| o.net_profit_lamports);

        let Some(opportunity) = best else {
            // No profitable cycle — release the in-flight flag immediately
            warn!("Cycles detected but none profitable after evaluation (input={} lamports)", config_cb.input_sol_lamports);
            bundle_in_flight_cb.store(false, Ordering::Release);
            return;
        };

        info!("{}", opportunity.summary());

        // ── Step 5: spawn simulation + submission (rate-limited) ──────────────
        let rpc             = Arc::clone(&rpc_cb);
        let jito            = Arc::clone(&jito_cb);
        let keypair         = Arc::clone(&keypair_cb);
        let in_flight       = Arc::clone(&bundle_in_flight_cb);
        let sem             = Arc::clone(&submit_sem_cb);

        tokio::spawn(async move {
            // Acquire semaphore slot — blocks if MAX_CONCURRENT_SUBMISSIONS are busy
            let _permit = sem.acquire().await.expect("Semaphore closed");

            // Always release in-flight flag when this task exits, regardless of outcome
            let _guard = InFlightGuard(&in_flight);

            let blockhash = match rpc.get_latest_blockhash().await {
                Ok(h) => h,
                Err(e) => { error!("Blockhash fetch failed: {e}"); return; }
            };

            let bundle = match JitoBundle::build(&opportunity, &keypair, blockhash) {
                Ok(b) => b,
                Err(e) => { error!("Bundle build failed: {e}"); return; }
            };

            // Simulate every swap tx (not the tip tx, which is the last one)
            let swap_txs = &bundle.transactions[..bundle.transactions.len().saturating_sub(1)];
            match arbitrage::simulator::simulate_opportunity(&opportunity, swap_txs, &rpc).await {
                Ok(true) => {}
                Ok(false) => { warn!("Simulation rejected — skipping bundle"); return; }
                Err(e) => { error!("Simulation error: {e}"); return; }
            }

            match jito.submit_bundle(&bundle).await {
                Ok(id) => info!(
                    bundle_id = %id,
                    net_profit = opportunity.net_profit_lamports,
                    "Bundle submitted"
                ),
                Err(e) => error!("Bundle submission failed: {e}"),
            }
        });
    });

    let mut streamer = GrpcStreamer::new(Arc::clone(&config));
    let initial_subscription = build_account_subscription(&account_keys);
    streamer.start(initial_subscription, callback).await?;
    info!("Streaming started. Press Ctrl+C to stop.");

    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");
    streamer.stop();

    Ok(())
}

/// RAII guard: resets the in-flight flag when dropped, even on early return or panic.
struct InFlightGuard<'a>(&'a AtomicBool);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
