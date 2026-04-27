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
use dex::types::{Pool, WSOL_MINT};
use graph::{bellman_ford, exchange_graph::ExchangeGraph};
use jito::{bundle::JitoBundle, client::JitoClient};
use streamer::{client::GrpcStreamer, subscription::build_account_subscription};

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
    info!("Config loaded. dry_run={} debounce_ms={}", config.dry_run, config.bellman_ford_debounce_ms);

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
        // Non-DAMM pools: fetch vault accounts directly (SPL token accounts)
        let non_damm: Vec<Arc<Pool>> = all_pools.iter()
            .filter(|p| !matches!(p.dex, dex::types::DexKind::MeteoraDamm))
            .cloned()
            .collect();
        let vault_pubkeys: Vec<Pubkey> = non_damm.iter()
            .flat_map(|p| [p.vault_a, p.vault_b])
            .collect();

        info!("Fetching initial reserves for {} vaults...", vault_pubkeys.len());
        match rpc.get_multiple_accounts(&vault_pubkeys).await {
            Ok(accounts) => {
                let mut loaded = 0usize;
                for (pool, chunk) in non_damm.iter().zip(accounts.chunks(2)) {
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
                info!("Initialized graph with {}/{} non-DAMM pools from RPC", loaded, non_damm.len());
            }
            Err(e) => {
                warn!("Failed to pre-fetch reserves (will rely on stream updates): {e}");
                for pool in &non_damm {
                    graph.update_pool(pool);
                }
            }
        }

        // ── Compute per-pool reserves for Meteora DAMM (LP fraction method) ──
        // DAMM pools share underlying vaults; pool_reserve = vault.totalAmount * (pool_lp / vault_lp_supply)
        let damm_pools: Vec<Arc<Pool>> = all_pools.iter()
            .filter(|p| matches!(p.dex, dex::types::DexKind::MeteoraDamm))
            .filter(|p| p.extra.a_vault_lp.is_some() && p.extra.b_vault_lp.is_some())
            .cloned()
            .collect();

        if !damm_pools.is_empty() {
            // Collect unique vault pubkeys and LP token account pubkeys to fetch
            let vault_keys: Vec<Pubkey> = damm_pools.iter()
                .flat_map(|p| [p.vault_a, p.vault_b])
                .collect();
            let lp_keys: Vec<Pubkey> = damm_pools.iter()
                .flat_map(|p| [p.extra.a_vault_lp.unwrap(), p.extra.b_vault_lp.unwrap()])
                .collect();

            info!("Fetching DAMM vault+LP accounts for {} pools...", damm_pools.len());
            match tokio::try_join!(
                rpc.get_multiple_accounts(&vault_keys),
                rpc.get_multiple_accounts(&lp_keys),
            ) {
                Ok((vault_accs, lp_accs)) => {
                    // First pass: collect vault lpMint pubkeys (to fetch supplies)
                    let mut lp_mint_keys: Vec<Pubkey> = Vec::new();
                    for chunk in vault_accs.chunks(2) {
                        for opt in chunk.iter() {
                            let key = opt.as_ref()
                                .and_then(|a| dex::parse_meteora_vault_lp_mint(&a.data))
                                .unwrap_or_default();
                            lp_mint_keys.push(key);
                        }
                    }

                    // Fetch vault LP mint supplies
                    if let Ok(mint_accs) = rpc.get_multiple_accounts(&lp_mint_keys).await {
                        let mut damm_loaded = 0usize;
                        for (i, pool) in damm_pools.iter().enumerate() {
                            let va  = vault_accs.get(i*2)  .and_then(|o| o.as_ref());
                            let vb  = vault_accs.get(i*2+1).and_then(|o| o.as_ref());
                            let lpa = lp_accs.get(i*2)     .and_then(|o| o.as_ref());
                            let lpb = lp_accs.get(i*2+1)   .and_then(|o| o.as_ref());
                            let ma  = mint_accs.get(i*2)   .and_then(|o| o.as_ref());
                            let mb  = mint_accs.get(i*2+1) .and_then(|o| o.as_ref());

                            if let (Some(va), Some(vb), Some(lpa), Some(lpb), Some(ma), Some(mb)) =
                                (va, vb, lpa, lpb, ma, mb)
                            {
                                let total_a    = dex::parse_meteora_vault_amount(&va.data);
                                let total_b    = dex::parse_meteora_vault_amount(&vb.data);
                                let lp_bal_a   = dex::parse_spl_token_amount(&lpa.data);
                                let lp_bal_b   = dex::parse_spl_token_amount(&lpb.data);
                                let lp_supply_a = dex::parse_spl_mint_supply(&ma.data);
                                let lp_supply_b = dex::parse_spl_mint_supply(&mb.data);

                                if let (Some(ta), Some(tb), Some(la), Some(lb), Some(sa), Some(sb)) =
                                    (total_a, total_b, lp_bal_a, lp_bal_b, lp_supply_a, lp_supply_b)
                                {
                                    if sa > 0 && sb > 0 {
                                        let ra = ((ta as f64) * (la as f64) / (sa as f64)) as u64;
                                        let rb = ((tb as f64) * (lb as f64) / (sb as f64)) as u64;
                                        pool.reserve_a.store(ra, Ordering::Relaxed);
                                        pool.reserve_b.store(rb, Ordering::Relaxed);
                                        pool.a_lp_balance.store(la, Ordering::Relaxed);
                                        pool.b_lp_balance.store(lb, Ordering::Relaxed);
                                        graph.update_pool(pool);
                                        damm_loaded += 1;
                                        debug!("DAMM pool {}: reserve_a={} reserve_b={} (lp_frac_a={:.4}% lp_frac_b={:.4}%)",
                                            pool.id, ra, rb,
                                            la as f64/sa as f64*100.0,
                                            lb as f64/sb as f64*100.0);
                                    }
                                }
                            }
                        }
                        info!("Initialized DAMM reserves for {}/{} pools via LP fraction", damm_loaded, damm_pools.len());
                    }
                }
                Err(e) => warn!("Failed to pre-fetch DAMM vault/LP accounts: {e}"),
            }
        }

        // ── Also prefetch sqrt_price for CL pool state accounts ───────────────
        // CL pool state accounts (which carry sqrt_price) are a separate set from
        // the vault accounts above. Prefetching them avoids a startup window where
        // sqrt_price = 0 could generate phantom arbitrage signals before the first
        // gRPC state-account update arrives.
        let cl_pools: Vec<_> = all_pools.iter()
            .filter(|p| matches!(p.dex, dex::types::DexKind::OrcaWhirlpool | dex::types::DexKind::RaydiumClmm))
            .filter_map(|p| p.state_account.map(|s| (Arc::clone(p), s)))
            .collect();

        if !cl_pools.is_empty() {
            let state_pubkeys: Vec<Pubkey> = cl_pools.iter().map(|(_, s)| *s).collect();
            info!("Fetching sqrt_price for {} CL pool state accounts...", state_pubkeys.len());
            match rpc.get_multiple_accounts(&state_pubkeys).await {
                Ok(accounts) => {
                    let mut cl_loaded = 0usize;
                    for ((pool, _), acc_opt) in cl_pools.iter().zip(accounts.iter()) {
                        if let Some(acc) = acc_opt {
                            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(&acc.data, pool.dex) {
                                pool.sqrt_price_x64.store(price.to_bits(), Ordering::Relaxed);
                                pool.fee_bps.store(fee_bps, Ordering::Relaxed);
                                graph.update_pool(pool);
                                cl_loaded += 1;
                            }
                        }
                    }
                    info!("Initialized sqrt_price for {}/{} CL pools from RPC", cl_loaded, cl_pools.len());
                }
                Err(e) => warn!("Failed to pre-fetch CL state accounts: {e}"),
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

    // Per-cycle simulation failure cooldown.
    // After simulation rejects a cycle we suppress it for CYCLE_FAIL_COOLDOWN_SECS
    // to stop hammering the RPC with the same broken instruction set.
    // Key = cycle path encoded as joined pubkey strings.
    const CYCLE_FAIL_COOLDOWN_SECS: u64 = 30;
    let failed_cycles: Arc<dashmap::DashMap<String, std::time::Instant>> =
        Arc::new(dashmap::DashMap::new());

    // ── Clone Arcs for the callback closure ───────────────────────────────────
    let graph_cb            = Arc::clone(&graph);
    let registry_cb         = Arc::clone(&registry);
    let config_cb           = Arc::clone(&config);
    let rpc_cb              = Arc::clone(&rpc);
    let jito_cb             = Arc::clone(&jito);
    let keypair_cb          = Arc::clone(&keypair);
    let last_bf_ms_cb       = Arc::clone(&last_bf_ms);
    let bundle_in_flight_cb = Arc::clone(&bundle_in_flight);
    let submit_sem_cb       = Arc::clone(&submit_sem);
    let failed_cycles_cb    = Arc::clone(&failed_cycles);

    let callback = Arc::new(move |pubkey_bytes: [u8; 32], data: Vec<u8>, _slot: u64| {
        let pubkey = Pubkey::from(pubkey_bytes);

        // ── Step 1: update pool state ─────────────────────────────────────────
        if let Some((pool, is_a)) = registry_cb.get_by_lp_account(&pubkey) {
            // Meteora DAMM LP token account update: scale pool reserve proportionally.
            // pool_reserve = initial_reserve * (new_lp_balance / initial_lp_balance)
            if let Some(new_bal) = dex::parse_spl_token_amount(&data) {
                use std::sync::atomic::Ordering;
                let (old_bal, old_reserve) = if is_a {
                    (pool.a_lp_balance.load(Ordering::Relaxed), pool.reserve_a.load(Ordering::Relaxed))
                } else {
                    (pool.b_lp_balance.load(Ordering::Relaxed), pool.reserve_b.load(Ordering::Relaxed))
                };
                if old_bal > 0 && old_reserve > 0 {
                    let new_reserve = ((old_reserve as f64) * (new_bal as f64 / old_bal as f64)) as u64;
                    if is_a {
                        pool.reserve_a.store(new_reserve, Ordering::Relaxed);
                        pool.a_lp_balance.store(new_bal, Ordering::Relaxed);
                    } else {
                        pool.reserve_b.store(new_reserve, Ordering::Relaxed);
                        pool.b_lp_balance.store(new_bal, Ordering::Relaxed);
                    }
                    graph_cb.update_pool(&pool);
                }
            }
        } else if let Some(pools) = registry_cb.get_by_vault(&pubkey) {
            for pool in &pools {
                // DAMM pools: reserves are tracked via LP accounts above; skip vault updates.
                if matches!(pool.dex, dex::types::DexKind::MeteoraDamm) {
                    continue;
                }
                if let Some(amount) = dex::parse_spl_token_amount(&data) {
                    use std::sync::atomic::Ordering;
                    if pubkey == pool.vault_a {
                        pool.reserve_a.store(amount, Ordering::Relaxed);
                    } else {
                        pool.reserve_b.store(amount, Ordering::Relaxed);
                    }
                    graph_cb.update_pool(pool);
                }
            }
        } else if let Some(pool) = registry_cb.get_by_state_account(&pubkey) {
            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(&data, pool.dex) {
                use std::sync::atomic::Ordering;
                // Store price as f64 bits. Using f64 avoids the u64 overflow that occurs
                // when sqrt_price_x64 > u64::MAX (e.g. BTC/USDC where sqrt(price) ≈ 29).
                pool.sqrt_price_x64.store(price.to_bits(), Ordering::Relaxed);
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
        let debounce = config_cb.bellman_ford_debounce_ms;
        if now_ms.saturating_sub(last) < debounce {
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
            info!("Cycles detected but none profitable after evaluation (input={} lamports)", config_cb.input_sol_lamports);
            bundle_in_flight_cb.store(false, Ordering::Release);
            return;
        };

        // ── Step 4b: per-cycle simulation failure cooldown ────────────────────
        // Build a stable string key from the cycle path so we can suppress
        // repeated simulation attempts for the same broken cycle.
        let cycle_key: String = opportunity.cycle.path
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("-");

        if let Some(last_fail) = failed_cycles_cb.get(&cycle_key) {
            if last_fail.elapsed().as_secs() < CYCLE_FAIL_COOLDOWN_SECS {
                debug!(
                    "Cycle on cooldown ({:.0}s remaining) — skipping simulation",
                    CYCLE_FAIL_COOLDOWN_SECS as f64 - last_fail.elapsed().as_secs_f64()
                );
                bundle_in_flight_cb.store(false, Ordering::Release);
                return;
            }
            // Cooldown expired — remove stale entry and try again
            drop(last_fail);
            failed_cycles_cb.remove(&cycle_key);
        }

        info!("{}", opportunity.summary());

        // ── Step 5: spawn simulation + submission (rate-limited) ──────────────
        let rpc             = Arc::clone(&rpc_cb);
        let jito            = Arc::clone(&jito_cb);
        let keypair         = Arc::clone(&keypair_cb);
        let in_flight       = Arc::clone(&bundle_in_flight_cb);
        let sem             = Arc::clone(&submit_sem_cb);
        let failed_cycles_t = Arc::clone(&failed_cycles_cb);
        let cycle_key_t     = cycle_key.clone();

        tokio::spawn(async move {
            let _permit = sem.acquire().await.expect("Semaphore closed");
            let _guard  = InFlightGuard(&in_flight);

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
            use arbitrage::simulator::SimOutcome;
            match arbitrage::simulator::simulate_opportunity(&opportunity, swap_txs, &rpc).await {
                Ok(SimOutcome::Passed) => {}
                Ok(SimOutcome::MarketRejected { hop, err }) => {
                    // Real market rejection (slippage, price, DEX error).
                    // Suppress this cycle for CYCLE_FAIL_COOLDOWN_SECS so we don't
                    // spam the RPC with a quote that's unlikely to improve until prices move.
                    failed_cycles_t.insert(cycle_key_t.clone(), std::time::Instant::now());
                    info!(
                        hop,
                        ?err,
                        "Simulation market-rejected — suppressing cycle for {CYCLE_FAIL_COOLDOWN_SECS}s"
                    );
                    return;
                }
                Ok(SimOutcome::InfraError { hop, err }) => {
                    // Broken instruction or missing account — NOT a market condition.
                    // Do not cooldown; fix the underlying code or pool config instead.
                    error!(
                        hop,
                        ?err,
                        "Simulation infra error — check pool config / ATA setup (no cooldown applied)"
                    );
                    return;
                }
                Err(e) => { error!("Simulation RPC error: {e}"); return; }
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
