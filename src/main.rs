#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod arbitrage;
mod config;
mod dex;
mod graph;
mod jito;
mod streamer;

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::read_keypair_file,
    signer::Signer,
};
use std::{
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use solana_sdk::hash::Hash;
use tokio::sync::{Semaphore, RwLock, watch};
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
    registry.validate()?;
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

    // ── Wallet balance check ──────────────────────────────────────────────────
    // Each arb bundle now creates ATAs and wraps SOL inline (idempotent), so no
    // pre-flight ATA setup is required. However the wallet must hold enough SOL
    // to cover: ATA rent (~0.002 SOL each × N mints), the arb input amount, and
    // transaction fees. Warn early so the user knows before the first cycle runs.
    match rpc.get_balance(&user).await {
        Ok(lamports) => {
            const MIN_LAMPORTS: u64 = 200_000_000; // 0.2 SOL soft minimum
            if lamports < MIN_LAMPORTS {
                warn!(
                    "Wallet balance is {} lamports ({:.4} SOL) — below 0.2 SOL. \
                     Fund the wallet before bundles can succeed.",
                    lamports,
                    lamports as f64 / 1e9
                );
            } else {
                info!("Wallet balance: {} lamports ({:.4} SOL)", lamports, lamports as f64 / 1e9);
            }
        }
        Err(e) => warn!("Could not fetch wallet balance: {e}"),
    }

    // Print all edge rates so stale/wrong pool data is visible before the bot starts
    graph.log_rates(&Pubkey::from_str(WSOL_MINT)?);

    let jito = Arc::new(JitoClient::new(config.dry_run));
    let sol_mint = Pubkey::from_str(WSOL_MINT)?;

    // ── Blockhash cache ───────────────────────────────────────────────────────
    // Fetched synchronously at startup so the cache is never Hash::default()
    // (all-zeros) when the first bundle is submitted. The background task then
    // refreshes every 2 s; blockhashes are valid for ~150 slots (~60 s).
    let initial_blockhash = rpc.get_latest_blockhash().await
        .context("Failed to fetch initial blockhash")?;
    let cached_blockhash: Arc<RwLock<Hash>> = Arc::new(RwLock::new(initial_blockhash));
    {
        let rpc  = Arc::clone(&rpc);
        let cache = Arc::clone(&cached_blockhash);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                match rpc.get_latest_blockhash().await {
                    Ok(h) => { *cache.write().await = h; }
                    Err(e) => warn!("Blockhash cache refresh failed: {e}"),
                }
            }
        });
    }

    // ── Wallet balance cache ──────────────────────────────────────────────────
    // Refreshed every 5 s. Used to cap `amount_in` to what the wallet can
    // actually afford, accounting for ATA rent + tx fees overhead.
    //
    // Overhead reservation:
    //   ATA rent:  2_039_280 lamports × 3 accounts (WSOL + 2 intermediates)
    //   Tx fees:   5_000 × 4 txs
    //   Buffer:    ~1 M lamports
    //   Total:     ~8 M lamports  (0.008 SOL)
    const BALANCE_OVERHEAD_LAMPORTS: u64 = 8_000_000;
    let cached_balance: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let rpc     = Arc::clone(&rpc);
        let cache   = Arc::clone(&cached_balance);
        let wallet  = user;
        tokio::spawn(async move {
            loop {
                match rpc.get_balance(&wallet).await {
                    Ok(b) => { cache.store(b, Ordering::Relaxed); }
                    Err(e) => warn!("Balance cache refresh failed: {e}"),
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    // ── Graph-update signal (watch channel) ───────────────────────────────────
    // The callback only updates pool state then sends a signal.
    // A dedicated task does the Bellman-Ford search, so the gRPC receive loop
    // is never blocked by graph computation.
    let (update_tx, update_rx) = watch::channel(0u64); // counter: incremented on every pool change

    // ── Rate-limiting primitives ──────────────────────────────────────────────
    let bundle_in_flight: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let submit_sem: Arc<Semaphore>         = Arc::new(Semaphore::new(MAX_CONCURRENT_SUBMISSIONS));
    const CYCLE_FAIL_COOLDOWN_SECS: u64 = 30;
    /// After a successful submission, suppress the same cycle for this long.
    /// Gives the bundle time to land (or be dropped) before re-evaluating.
    const CYCLE_SUBMIT_COOLDOWN_SECS: u64 = 5;
    let failed_cycles: Arc<dashmap::DashMap<u64, std::time::Instant>> =
        Arc::new(dashmap::DashMap::new());

    // ── Callback: pool state update + signal (no BF) ─────────────────────────
    let graph_cb    = Arc::clone(&graph);
    let registry_cb = Arc::clone(&registry);
    let update_tx_cb = update_tx.clone();

    let callback = Arc::new(move |pubkey_bytes: [u8; 32], data: Vec<u8>, _slot: u64| {
        let pubkey = Pubkey::from(pubkey_bytes);

        let updated = if let Some((pool, is_a)) = registry_cb.get_by_lp_account(&pubkey) {
            if let Some(new_bal) = dex::parse_spl_token_amount(&data) {
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
                    true
                } else { false }
            } else { false }
        } else if let Some(pools) = registry_cb.get_by_vault(&pubkey) {
            let mut any = false;
            for pool in &pools {
                if matches!(pool.dex, dex::types::DexKind::MeteoraDamm) { continue; }
                if let Some(amount) = dex::parse_spl_token_amount(&data) {
                    if pubkey == pool.vault_a { pool.reserve_a.store(amount, Ordering::Relaxed); }
                    else                      { pool.reserve_b.store(amount, Ordering::Relaxed); }
                    graph_cb.update_pool(pool);
                    any = true;
                }
            }
            any
        } else if let Some(pool) = registry_cb.get_by_state_account(&pubkey) {
            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(&data, pool.dex) {
                pool.sqrt_price_x64.store(price.to_bits(), Ordering::Relaxed);
                pool.fee_bps.store(fee_bps, Ordering::Relaxed);
                graph_cb.update_pool(&pool);
                true
            } else { false }
        } else {
            debug!("Received update for untracked account: {pubkey}");
            false
        };

        // Signal the BF task only when a pool edge actually changed
        if updated {
            update_tx_cb.send_modify(|v| *v = v.wrapping_add(1));
        }
    });

    // ── Bellman-Ford + evaluation task ────────────────────────────────────────
    // Runs in its own async task so the gRPC stream is never stalled.
    // Debounce: after a signal we sleep `debounce_ms` to coalesce rapid bursts,
    // then call borrow_and_update() to mark the version as "seen" before running BF.
    {
        let graph_bf        = Arc::clone(&graph);
        let registry_bf     = Arc::clone(&registry);
        let config_bf       = Arc::clone(&config);
        let rpc_bf          = Arc::clone(&rpc);
        let jito_bf         = Arc::clone(&jito);
        let keypair_bf      = Arc::clone(&keypair);
        let in_flight_bf    = Arc::clone(&bundle_in_flight);
        let sem_bf          = Arc::clone(&submit_sem);
        let failed_bf       = Arc::clone(&failed_cycles);
        let blockhash_bf    = Arc::clone(&cached_blockhash);
        let balance_bf      = Arc::clone(&cached_balance);
        let mut update_rx   = update_rx;
        let debounce_ms     = config.bellman_ford_debounce_ms;

        tokio::spawn(async move {
            // ── Per-window stats (reset every 10 s, same cadence as "Stream alive") ──
            let mut stat_bf_runs:        u64   = 0;
            let mut stat_cycles:         u64   = 0; // negative cycles BF found
            let mut stat_profitable:     u64   = 0; // passed full evaluation
            let mut stat_eval_rejected:  u64   = 0; // cycles evaluated but unprofitable
            let mut stat_best_gross_bps: f64   = 0.0; // best margin among NEGATIVE cycles (bps)
            // Best ratio across ALL examined paths (negative + positive weight). When
            // stat_cycles is 0, this reveals whether the market is just below break-even
            // (e.g. -3.5 bps, no real arb available) vs. broken pricing (e.g. -500 bps).
            let mut stat_best_overall_bps: f64 = f64::NEG_INFINITY;
            let mut stat_paths_examined: u64   = 0;
            let mut stat_last = std::time::Instant::now();
            const STAT_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

            loop {
                // Wait until any pool changed
                if update_rx.changed().await.is_err() { break; }
                // Mark current version as seen before running BF so any update
                // that arrives *during* the BF run triggers the next iteration.
                let _version = *update_rx.borrow_and_update();

                // ── Bellman-Ford ──────────────────────────────────────────────
                stat_bf_runs += 1;
                let search = bellman_ford::find_negative_cycles_with_diag(&graph_bf, sol_mint);
                let cycles = search.cycles;
                stat_paths_examined += search.n_paths_examined as u64;
                if search.best_weight.is_finite() {
                    let overall_bps = ((-search.best_weight).exp() - 1.0) * 10_000.0;
                    if overall_bps > stat_best_overall_bps { stat_best_overall_bps = overall_bps; }
                }

                // Coalesce rapid-fire pool updates that arrived while BF was running:
                // sleep the debounce window then discard accumulated signals, so we
                // don't immediately re-trigger on stale updates from the same burst.
                if debounce_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
                    let _ = update_rx.borrow_and_update();
                }

                if cycles.is_empty() {
                    debug!("Bellman-Ford: no negative cycles found");
                } else {
                    stat_cycles += cycles.len() as u64;
                    for (i, c) in cycles.iter().enumerate() {
                        let gross_bps = (c.gross_ratio() - 1.0) * 10_000.0;
                        stat_best_gross_bps = stat_best_gross_bps.max(gross_bps);
                        debug!("  cycle[{i}] hops={} gross_ratio={:.6} total_weight={:.6}",
                            c.edges.len(), c.gross_ratio(), c.total_weight);
                    }
                    debug!("Bellman-Ford: {} negative cycle(s) detected", cycles.len());
                }

                // ── Periodic stats log (every 10 s) ──────────────────────────
                if stat_last.elapsed() >= STAT_WINDOW {
                    let secs = stat_last.elapsed().as_secs_f64();
                    let edges = graph_bf.edge_count();
                    let by_dex = graph_bf.edge_count_by_dex();
                    let avg_paths = stat_paths_examined as f64 / stat_bf_runs.max(1) as f64;
                    let best_overall_str = if stat_best_overall_bps.is_finite() {
                        format!("{:+.2}bps", stat_best_overall_bps)
                    } else {
                        "n/a".to_string()
                    };
                    info!(
                        "BF window — runs={} neg_cycles={} evaluated={} profitable={} ({:.1} runs/s) \
                         best_margin={:+.2}bps best_overall={} | edges={} (raydium={} clmm={} orca={} damm={}) avg_paths/run={:.0}",
                        stat_bf_runs, stat_cycles, stat_eval_rejected + stat_profitable,
                        stat_profitable, stat_bf_runs as f64 / secs, stat_best_gross_bps,
                        best_overall_str, edges, by_dex[0], by_dex[1], by_dex[2], by_dex[3], avg_paths,
                    );
                    stat_bf_runs           = 0;
                    stat_cycles            = 0;
                    stat_profitable        = 0;
                    stat_eval_rejected     = 0;
                    stat_best_gross_bps    = 0.0;
                    stat_best_overall_bps  = f64::NEG_INFINITY;
                    stat_paths_examined    = 0;
                    stat_last              = std::time::Instant::now();
                }

                if cycles.is_empty() { continue; }

                // ── In-flight guard ───────────────────────────────────────────
                if in_flight_bf.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
                    debug!("Bundle already in-flight, skipping {} cycle(s)", cycles.len());
                    continue;
                }

                // ── Evaluate best cycle ───────────────────────────────────────
                // In dry_run the wallet is unfunded on-chain; use the configured
                // input amount directly so evaluation still runs and logs outcomes.
                let available_sol = if config_bf.dry_run {
                    config_bf.input_sol_lamports
                } else {
                    let wallet_balance = balance_bf.load(Ordering::Relaxed);
                    let spendable = wallet_balance
                        .saturating_sub(BALANCE_OVERHEAD_LAMPORTS)
                        .min(config_bf.input_sol_lamports);
                    if spendable == 0 {
                        debug!("Wallet balance ({wallet_balance} lamports) too low for overhead reserve — skipping");
                        in_flight_bf.store(false, Ordering::Release);
                        continue;
                    }
                    spendable
                };

                let mut rejected_this_run = 0u64;
                let best = cycles.iter().filter_map(|c| {
                    let result = arbitrage::evaluator::optimize_input_and_tip(
                        c, &registry_bf, &config_bf, user, available_sol,
                    );
                    if result.is_none() { rejected_this_run += 1; }
                    result
                }).max_by_key(|o| o.net_profit_lamports);
                stat_eval_rejected += rejected_this_run;

                let Some(opportunity) = best else {
                    debug!("Cycles detected but none profitable (input={available_sol} lamports, {rejected_this_run} rejected)");
                    in_flight_bf.store(false, Ordering::Release);
                    continue;
                };

                stat_profitable += 1;

                // ── Cooldown check ────────────────────────────────────────────
                // 64-bit hash of the cycle path — avoids heap-allocating a
                // (n_pubkeys × 32)-byte Vec per opportunity, and DashMap key
                // hashing is now O(1) instead of O(96–128).
                let cycle_key: u64 = {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    opportunity.cycle.path.hash(&mut h);
                    h.finish()
                };

                if let Some(last_fail) = failed_bf.get(&cycle_key) {
                    if last_fail.elapsed().as_secs() < CYCLE_FAIL_COOLDOWN_SECS {
                        debug!("Cycle on cooldown ({:.0}s remaining)",
                            CYCLE_FAIL_COOLDOWN_SECS as f64 - last_fail.elapsed().as_secs_f64());
                        in_flight_bf.store(false, Ordering::Release);
                        continue;
                    }
                    drop(last_fail);
                    failed_bf.remove(&cycle_key);
                }

                info!("{}", opportunity.summary());

                // ── Spawn submission task ─────────────────────────────────────
                let rpc          = Arc::clone(&rpc_bf);
                let jito         = Arc::clone(&jito_bf);
                let keypair      = Arc::clone(&keypair_bf);
                let in_flight    = Arc::clone(&in_flight_bf);
                let sem          = Arc::clone(&sem_bf);
                let failed_t     = Arc::clone(&failed_bf);
                let cycle_key_t  = cycle_key.clone();
                let bh_cache     = Arc::clone(&blockhash_bf);
                let config_t     = Arc::clone(&config_bf);
                let dry_run      = config_bf.dry_run;

                tokio::spawn(async move {
                    let _permit = sem.acquire().await.expect("Semaphore closed");
                    let _guard  = InFlightGuard(&in_flight);

                    // Use pre-cached blockhash — saves ~100 ms vs get_latest_blockhash()
                    let blockhash = *bh_cache.read().await;

                    let bundle = match JitoBundle::build(&opportunity, &keypair, blockhash, &config_t) {
                        Ok(b) => b,
                        Err(e) => { error!("Bundle build failed: {e}"); return; }
                    };

                    // In dry_run mode the wallet may not exist on-chain yet (0 lamports),
                    // which would cause every simulation to fail with AccountNotFound.
                    // Skip simulation — the Jito client won't submit in dry_run anyway.
                    if !dry_run {
                        let swap_txs = &bundle.transactions[..bundle.transactions.len().saturating_sub(1)];
                        use arbitrage::simulator::SimOutcome;
                        match arbitrage::simulator::simulate_opportunity(&opportunity, swap_txs, &rpc).await {
                            Ok(SimOutcome::Passed) => {}
                            Ok(SimOutcome::MarketRejected { hop, err }) => {
                                failed_t.insert(cycle_key_t.clone(), std::time::Instant::now());
                                info!(hop, ?err, "Simulation market-rejected — suppressing for {CYCLE_FAIL_COOLDOWN_SECS}s");
                                return;
                            }
                            Ok(SimOutcome::InfraError { hop, err }) => {
                                // Cooldown on infra errors too — the bundle is broken (wrong accounts,
                                // missing ATAs, bad instruction layout). Retrying every BF cycle just
                                // wastes RPC budget until the underlying config issue is fixed.
                                failed_t.insert(cycle_key_t.clone(), std::time::Instant::now());
                                error!(hop, ?err, "Simulation infra error — suppressing for {CYCLE_FAIL_COOLDOWN_SECS}s (check pool config / ATA setup)");
                                return;
                            }
                            Err(e) => { error!("Simulation RPC error: {e}"); return; }
                        }
                    }

                    match jito.submit_bundle(&bundle).await {
                        Ok(id) => {
                            info!(bundle_id = %id, net_profit = opportunity.net_profit_lamports, "Bundle submitted");
                            // Suppress this cycle for a few seconds — the bundle is now in-flight.
                            // Without this, every BF tick re-submits the same opportunity until
                            // the on-chain state actually changes.
                            failed_t.insert(cycle_key_t.clone(), std::time::Instant::now()
                                .checked_sub(std::time::Duration::from_secs(
                                    CYCLE_FAIL_COOLDOWN_SECS.saturating_sub(CYCLE_SUBMIT_COOLDOWN_SECS)
                                ))
                                .unwrap_or(std::time::Instant::now()));
                        }
                        Err(e) => {
                            error!("Bundle submission failed: {e}");
                            failed_t.insert(cycle_key_t.clone(), std::time::Instant::now());
                        }
                    }
                });
            }
        });
    }


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
