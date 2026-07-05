#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod alt;
mod arbitrage;
mod config;
mod dex;
mod flash_loan;
mod graph;
mod jito;
mod streamer;

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    pubkey::Pubkey,
    signature::read_keypair_file,
    signer::Signer,
};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use solana_sdk::hash::Hash;
use tokio::sync::{Semaphore, RwLock, watch};
use tracing::{debug, error, info, warn};

use config::Config;
use dex::PoolRegistry;
use dex::types::{Pool, mint_symbol};
use graph::{bellman_ford, exchange_graph::ExchangeGraph};
use jito::{bundle::JitoBundle, client::JitoClient};
use streamer::{client::GrpcStreamer, subscription::build_account_subscription};

/// Maximum concurrent RPC simulation + Jito bundle submission tasks.
/// Public RPCs typically allow 100 req/s; private ones 200–1000 req/s.
/// Keep this low to avoid triggering rate limits.
const MAX_CONCURRENT_SUBMISSIONS: usize = 2;

/// Resolve a profitable opportunity's Jupiter hops into real instructions before bundling.
///
/// For all-local cycles (`jupiter_hops` empty) this is a no-op that returns the base ALTs.
/// Otherwise, for each Jupiter hop it fetches `/quote` + `/swap-instructions`, splices the
/// returned instructions into `opportunity.swap_instructions` (one placeholder slot → N real
/// instructions), merges Jupiter's own ALTs with the bot's (caching fetched ALTs), and re-runs
/// the wire-size guard against the fully spliced flash-loan transaction. Returns the ALT set to
/// pass to `JitoBundle::build`, or an error to gracefully skip the opportunity.
async fn resolve_jupiter_hops(
    opportunity: &mut arbitrage::opportunity::ArbOpportunity,
    rpc: &RpcClient,
    jup: &dex::jupiter::JupiterClient,
    alt_cache: &dashmap::DashMap<Pubkey, AddressLookupTableAccount>,
    base_alts: &[AddressLookupTableAccount],
    user: Pubkey,
    config: &Config,
) -> Result<Vec<AddressLookupTableAccount>> {
    use std::collections::{HashMap, HashSet};
    use solana_sdk::compute_budget::ComputeBudgetInstruction;
    use solana_sdk::instruction::Instruction;

    if opportunity.jupiter_hops.is_empty() {
        return Ok(base_alts.to_vec());
    }

    // Fetch the real instructions per hop, keyed by hop_index so we can splice positionally.
    let mut resolved: HashMap<usize, Vec<Instruction>> = HashMap::new();
    let mut jup_alt_addrs: Vec<Pubkey> = Vec::new();
    for hop in &opportunity.jupiter_hops {
        let quote = jup
            .quote(&hop.input_mint, &hop.output_mint, hop.amount_in, config.slippage_bps)
            .await?;
        if quote.out_amount < hop.min_out {
            anyhow::bail!(
                "Jupiter hop {} quotes {} < required min_out {}",
                hop.hop_index, quote.out_amount, hop.min_out,
            );
        }
        let ix_bundle = jup.swap_instructions(quote, &user, hop.min_out).await?;
        jup_alt_addrs.extend(ix_bundle.alt_addresses);
        resolved.insert(hop.hop_index, ix_bundle.instructions);
    }

    // Splice: rebuild swap_instructions, replacing each placeholder with its resolved set.
    // Iterating original indices avoids index-shift when one slot expands to many.
    let mut spliced = Vec::with_capacity(opportunity.swap_instructions.len());
    for (i, ix) in opportunity.swap_instructions.iter().enumerate() {
        match resolved.remove(&i) {
            Some(real) => spliced.extend(real),
            None => spliced.push(ix.clone()),
        }
    }
    opportunity.swap_instructions = spliced;

    // Merge ALTs: base + Jupiter's (deduped). Fetched Jupiter ALTs are cached — they rotate
    // rarely and indices are append-only, so a stale cache entry still compiles correctly.
    let mut merged = base_alts.to_vec();
    let mut have: HashSet<Pubkey> = merged.iter().map(|a| a.key).collect();
    for addr in jup_alt_addrs {
        if !have.insert(addr) {
            continue;
        }
        let loaded = if let Some(cached) = alt_cache.get(&addr) {
            cached.clone()
        } else {
            let fetched = alt::load_alt(rpc, addr).await
                .with_context(|| format!("failed to load Jupiter ALT {addr}"))?;
            alt_cache.insert(addr, fetched.clone());
            fetched
        };
        merged.push(loaded);
    }

    // Re-run the wire-size guard now that real instructions + merged ALTs are known.
    // Mirrors the flash-loan probe in build_opportunity (single mega-tx).
    if config.enable_flash_loan {
        let cu_limit = config.compute_unit_limit.max(1_200_000) as u32;
        let cu_price = config.compute_unit_price_micro_lamports;
        let mut probe: Vec<Instruction> = vec![
            ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
            ComputeBudgetInstruction::set_compute_unit_price(cu_price),
        ];
        probe.extend(opportunity.setup_instructions.iter().cloned());
        probe.extend(opportunity.swap_instructions.iter().cloned());
        probe.extend(opportunity.teardown_instructions.iter().cloned());
        let wire = arbitrage::evaluator::estimate_v0_wire_size(&probe, &user, &merged);
        if wire > 1232 {
            anyhow::bail!("Jupiter cycle tx too large ({wire} bytes) after splice");
        }
    }

    Ok(merged)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let args: Vec<String> = std::env::args().collect();
    let init_alt_flag    = args.iter().any(|a| a == "--init-alt");
    let inspect_alt_flag = args.iter().any(|a| a == "--inspect-alt");

    tracing_subscriber::fmt()
        .with_ansi(true)  // force ANSI through even when cargo pipes stdout (non-TTY)
        .with_env_filter(
            // RUST_LOG takes full precedence; fall back to info only if unset.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("solana_mev=info"))
        )
        .init();

    arbitrage::latency::init();

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

    // ── Jupiter: optionally launch the Metis binary, then load pairs + client ──
    // Launch first so it can index pools (~1-2 min) while the bot does its own startup.
    // `_metis_child` must stay in scope for the bot's lifetime — kill_on_drop stops Metis
    // on normal exit. Only when enable_jupiter=true and JUPITER_BINARY_PATH is set; otherwise
    // we assume Metis is running externally at JUPITER_API_URL.
    let _metis_child = if config.enable_jupiter {
        match (config.jupiter_binary_path.as_deref(), config.jupiter_binary_key.as_deref()) {
            (Some(path), Some(key)) => {
                match dex::jupiter::spawn_metis(path, key, &config.rpc_url, &config.grpc_endpoint, config.grpc_token.as_deref()) {
                    Ok(child) => {
                        info!("Launched Metis swap-api from {path} (pid {:?}) — indexing pools (~1-2 min)", child.id());
                        Some(child)
                    }
                    Err(e) => {
                        warn!("Could not launch Metis ({e}); continuing — expecting an external instance at {}", config.jupiter_api_url);
                        None
                    }
                }
            }
            (Some(_), None) => {
                warn!("JUPITER_BINARY_PATH set but JUPITER_BINARY_KEY missing — Metis requires --binary-key. \
                       Skipping auto-launch; expecting an external instance at {}", config.jupiter_api_url);
                None
            }
            (None, _) => {
                info!("ENABLE_JUPITER=true with no JUPITER_BINARY_PATH — expecting external Metis at {}", config.jupiter_api_url);
                None
            }
        }
    } else {
        None
    };

    // Loaded AFTER subscribe_accounts so these vault-less pools never enter the gRPC
    // subscription. They live only in the registry's id-keyed map (see load_jupiter_pairs).
    let jupiter_pools: Vec<Arc<Pool>> = if config.enable_jupiter {
        let loaded = registry.load_jupiter_pairs(&config.jupiter_pairs_path)?;
        info!("Loaded {} Jupiter pair(s) from {}", loaded.len(), config.jupiter_pairs_path);
        loaded
    } else {
        Vec::new()
    };
    let jupiter_client = Arc::new(dex::jupiter::JupiterClient::new(config.jupiter_api_url.clone()));
    let jupiter_alt_cache: Arc<dashmap::DashMap<Pubkey, AddressLookupTableAccount>> =
        Arc::new(dashmap::DashMap::new());

    let rpc = Arc::new(RpcClient::new_with_commitment(
        config.rpc_url.clone(),
        solana_sdk::commitment_config::CommitmentConfig::processed(),
    ));

    // ── ALT: inspect, init, or load ───────────────────────────────────────────
    if inspect_alt_flag {
        if config.alt_addresses.is_empty() {
            anyhow::bail!("ALT_ADDRESSES (or ALT_ADDRESS) required for --inspect-alt");
        }
        for (n, &addr) in config.alt_addresses.iter().enumerate() {
            let alt = alt::load_alt(&rpc, addr).await?;
            println!("ALT {n}: {addr}  ({} accounts)", alt.addresses.len());
            for (i, pk) in alt.addresses.iter().enumerate() {
                println!("  [{i:3}] {pk}");
            }
        }
        return Ok(());
    }
    let alts = Arc::new(if init_alt_flag {
        info!("--init-alt: creating / extending ALT(s)...");
        alt::init_alt(&rpc, &keypair, &config, &registry, user).await?
    } else {
        if config.alt_addresses.is_empty() {
            anyhow::bail!("ALT_ADDRESSES is required — run with --init-alt to create");
        }
        let loaded = alt::load_alts(&rpc, &config.alt_addresses).await?;
        let total: usize = loaded.iter().map(|a| a.addresses.len()).sum();
        info!("Loaded {} ALT(s) covering {} accounts", loaded.len(), total);
        loaded
    });

    // ── Pre-fetch initial reserves for all pool vaults via RPC ───────────────
    // The gRPC stream only delivers updates when accounts *change*. Pools with
    // low volume may not update for minutes, leaving their graph edges at NaN
    // weights. Fetching initial balances ensures all edges are valid from the
    // first Bellman-Ford run.
    let graph = Arc::new(ExchangeGraph::new());
    {
        let all_pools = registry.all_pools();
        // AMM pools only: fetch vault SPL token accounts for reserve-based pricing.
        // CLMM pools (Raydium CLMM, Orca Whirlpool) use sqrt_price, not reserves —
        // they are initialized in the CL state-account prefetch below.
        // Meteora DAMM uses LP-fraction reserves fetched in its own block below.
        // Saber uses plain SPL token vault accounts (same parse path as Raydium AMM V4).
        let non_damm: Vec<Arc<Pool>> = all_pools.iter()
            .filter(|p| matches!(p.dex,
                dex::types::DexKind::RaydiumAmmV4 |
                dex::types::DexKind::Saber))
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
                info!("Initialized graph with {}/{} AMM pools from RPC", loaded, non_damm.len());
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

            // ── Prefetch virtual_price_r for stable DAMM pools ──────────────────
            // Stable DAMM pools (SOL/mSOL, USDC/USDT) store a Curve virtual price in
            // the pool state account. Without it the invariant treats reserves as equal
            // value, producing phantom 38%+ profit cycles for LST/SOL pairs. We fetch
            // once at startup; the rate changes at most daily (staking epoch cadence).
            let stable_damm: Vec<Arc<Pool>> = damm_pools.iter()
                .filter(|p| p.stable)
                .cloned()
                .collect();
            if !stable_damm.is_empty() {
                let pool_keys: Vec<Pubkey> = stable_damm.iter().map(|p| p.id).collect();
                info!("Fetching virtual_price_r for {} stable DAMM pools...", stable_damm.len());
                match rpc.get_multiple_accounts(&pool_keys).await {
                    Ok(accs) => {
                        for (pool, acc_opt) in stable_damm.iter().zip(accs.iter()) {
                            match acc_opt {
                                Some(acc) => {
                                    match dex::parse_damm_virtual_price(&acc.data, 0) {
                                        Some(vpr) => {
                                            pool.damm_virtual_price.store(vpr, Ordering::Relaxed);
                                            // Cross-check on-chain amp against pools.json to catch mismatches early.
                                            if let Some(on_chain_amp) = dex::parse_damm_amp(&acc.data) {
                                                let cfg_amp = pool.extra.damm_amp.unwrap_or(100);
                                                if on_chain_amp != cfg_amp {
                                                    warn!("DAMM stable {}: amp mismatch — on-chain={} pools.json={} \
                                                        (update pools.json to fix phantom quotes)",
                                                        &pool.id.to_string()[..8], on_chain_amp, cfg_amp);
                                                }
                                            }
                                            graph.update_pool(pool);
                                            info!("DAMM stable {}: virtual_price_r={} ({:.6}×) amp={}",
                                                &pool.id.to_string()[..8], vpr, vpr as f64 / 1e9,
                                                pool.extra.damm_amp.unwrap_or(0));
                                        }
                                        None => warn!("DAMM stable {}: could not parse baseVirtualPrice \
                                            (expected disc=1 at offset 874, amp in [1,100000], \
                                            vpr in [500000,2000000]); falling back to 1:1. \
                                            Inspect with: solana account {} --output json | \
                                            python3 -c \"import base64,json,struct,sys; \
                                            d=base64.b64decode(json.load(sys.stdin)['account']['data'][0]); \
                                            print('disc@874=',d[874],'amp@875=',struct.unpack_from('<Q',d,875)[0],\
                                            'vpr@900=',struct.unpack_from('<Q',d,900)[0])\"",
                                            &pool.id.to_string()[..8],
                                            pool.id),
                                    }
                                }
                                None => warn!("DAMM stable {}: pool state account not found",
                                    &pool.id.to_string()[..8]),
                            }
                        }
                    }
                    Err(e) => warn!("Failed to fetch stable DAMM pool states: {e}"),
                }
            }
        }

        // ── Also prefetch sqrt_price for CL pool state accounts ───────────────
        // CL pool state accounts (which carry sqrt_price) are a separate set from
        // the vault accounts above. Prefetching them avoids a startup window where
        // sqrt_price = 0 could generate phantom arbitrage signals before the first
        // gRPC state-account update arrives.
        let cl_pools: Vec<_> = all_pools.iter()
            .filter(|p| matches!(p.dex,
                dex::types::DexKind::OrcaWhirlpool |
                dex::types::DexKind::RaydiumClmm   |
                dex::types::DexKind::MeteoraDlmm   |
                dex::types::DexKind::Phoenix        |
                dex::types::DexKind::Lifinity       |
                dex::types::DexKind::Invariant))
            .filter_map(|p| p.state_account.map(|s| (Arc::clone(p), s)))
            .collect();

        if !cl_pools.is_empty() {
            let state_pubkeys: Vec<Pubkey> = cl_pools.iter().map(|(_, s)| *s).collect();
            info!("Fetching price for {} CL/DLMM pool state accounts...", state_pubkeys.len());
            match rpc.get_multiple_accounts(&state_pubkeys).await {
                Ok(accounts) => {
                    let mut cl_loaded = 0usize;
                    for ((pool, _), acc_opt) in cl_pools.iter().zip(accounts.iter()) {
                        if let Some(acc) = acc_opt {
                            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(&acc.data, pool) {
                                pool.sqrt_price_x64.store(price.to_bits(), Ordering::Relaxed);
                                if fee_bps > 0 {
                                    pool.fee_bps.store(fee_bps, Ordering::Relaxed);
                                }
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

    // ── Raydium CLMM observation key audit ───────────────────────────────────
    // Covers every CLMM pool. Observation keys are read from pool state (offset
    // 201–232) during the prefetch above; they are NOT derived via PDA because
    // the PDA derivation disagrees with the on-chain value for most pools.
    {
        use dex::types::DexKind;
        for pool in registry.all_pools() {
            if pool.dex != DexKind::RaydiumClmm { continue; }
            let short = &pool.id.to_string()[..8];
            let words: [u64; 4] = std::array::from_fn(|i| {
                pool.clmm_observation_key[i].load(Ordering::Relaxed)
            });
            let bytes: [u8; 32] = unsafe { std::mem::transmute(words) };
            let obs = Pubkey::from(bytes);
            if obs == Pubkey::default() {
                warn!(pool = %short,
                    "CLMM pool has no state_account — observation key not loaded; \
                     swap instructions will fail until first gRPC state update");
            } else {
                debug!(pool = %short, %obs, "CLMM observation key loaded from state");
            }
        }
    }

    // ── CHECK_POOLS mode: simulate one swap per pool, then exit ──────────────
    if config.check_pools {
        let ok = arbitrage::pool_check::check_pools(&registry, &rpc, user).await?;
        std::process::exit(if ok { 0 } else { 1 });
    }

    // ── Wallet balance check ──────────────────────────────────────────────────
    // Each arb bundle now creates ATAs and wraps SOL inline (idempotent), so no
    // pre-flight ATA setup is required. However the wallet must hold enough SOL
    // to cover: ATA rent (~0.002 SOL each × N mints), the arb input amount, and
    // transaction fees. Warn early so the user knows before the first cycle runs.
    let start_balance: u64 = match rpc.get_balance(&user).await {
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
            lamports
        }
        Err(e) => { warn!("Could not fetch wallet balance: {e}"); 0 }
    };

    // P&L baseline in base-token units. Native base: same as the SOL balance above.
    // SPL base: the wallet's base-token ATA balance at startup.
    let start_base_balance: u64 = if config.base_token.is_native {
        start_balance
    } else {
        let base_ata = spl_associated_token_account::get_associated_token_address(&user, &config.base_token.mint);
        match rpc.get_token_account_balance(&base_ata).await {
            Ok(ui) => ui.amount.parse::<u64>().unwrap_or(0),
            Err(e) => { warn!("Could not fetch base-token ({}) balance: {e}", config.base_token.symbol); 0 }
        }
    };

    if config.enable_flash_loan {
        let flash = config.flash_loan.as_ref().expect("flash_loan set when enable_flash_loan=true");
        info!(
            "Flash loan enabled — MarginFi account: {} | group: {} | SOL bank: {}",
            flash.marginfi_account,
            flash.marginfi_group,
            flash.marginfi_sol_bank,
        );
        if !config.disable_simulation {
            warn!(
                "Flash loan is enabled but DISABLE_SIMULATION=false. \
                 Pre-submission simulation will likely fail (MarginFi health checks require \
                 on-chain state). Consider setting DISABLE_SIMULATION=true."
            );
        }
    }

    // Print all edge rates so stale/wrong pool data is visible before the bot starts
    let base_mint = config.base_token.mint;
    info!(
        "Arbitrage base token: {} ({}, {} decimals, native={})",
        config.base_token.symbol, base_mint, config.base_token.decimals, config.base_token.is_native,
    );
    graph.log_rates(&base_mint);

    let jito = Arc::new(JitoClient::new(config.dry_run));
    jito.warmup_connections().await;
    Arc::clone(&jito).spawn_keepalive();
    let tip_floor_cache = Arc::clone(&jito).spawn_tip_floor_cache();

    // ── Jupiter rate poller ───────────────────────────────────────────────────
    // Maintains synthetic Jupiter edges in the graph via periodic /quote calls. The
    // hot path reads the cached rate; the real route is fetched at submit time.
    if config.enable_jupiter && !jupiter_pools.is_empty() {
        dex::jupiter::spawn_poller(
            (*jupiter_client).clone(),
            jupiter_pools.clone(),
            Arc::clone(&graph),
            config.jupiter_probe_lamports,
            config.jupiter_poll_interval_ms,
            config.slippage_bps,
        );
        info!(
            "Jupiter poller started ({} pair(s), {}ms interval, api={})",
            jupiter_pools.len(), config.jupiter_poll_interval_ms, config.jupiter_api_url,
        );

        // Readiness logger: poll one pair until the swap-api answers, then log once and exit.
        // Gives a clear "Metis ready" signal instead of staring at jupiter=0 during warm-up.
        let probe_client = (*jupiter_client).clone();
        let (a, b) = (jupiter_pools[0].token_a, jupiter_pools[0].token_b);
        let probe_amt = config.jupiter_probe_lamports;
        let probe_slip = config.slippage_bps;
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            loop {
                if probe_client.quote(&a, &b, probe_amt, probe_slip).await.is_ok() {
                    info!("Jupiter swap-api ready after {:.0}s — edges will populate", started.elapsed().as_secs_f64());
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }

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
                tokio::time::sleep(std::time::Duration::from_millis(400)).await;
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
    // Overhead reservation (native SOL base):
    //   ATA rent:  2_039_280 lamports × 3 accounts (WSOL + 2 intermediates)
    //   Tx fees:   5_000 × 4 txs
    //   Buffer:    ~1 M lamports
    //   Total:     ~8 M lamports  (0.008 SOL)
    //
    // For a non-native base (e.g. USDC), the cached balance stores the SPL ATA
    // balance (base-token units). Gas is tracked separately via the gas guard.
    const BALANCE_OVERHEAD_LAMPORTS: u64 = 8_000_000;
    let cached_balance: Arc<std::sync::atomic::AtomicU64> =
        Arc::new(std::sync::atomic::AtomicU64::new(0));
    {
        let rpc              = Arc::clone(&rpc);
        let cache            = Arc::clone(&cached_balance);
        let wallet           = user;
        let dry_run          = config.dry_run;
        let base_is_native   = config.base_token.is_native;
        let base_mint_for_cache = config.base_token.mint;
        let base_symbol      = config.base_token.symbol;
        let gas_floor        = config.min_sol_gas_lamports;
        let start_base_balance = start_base_balance;
        tokio::spawn(async move {
            // Counts consecutive polls where base balance < pnl threshold.
            // Two consecutive low readings (≥10 s after the first) are needed before halting,
            // to avoid false positives from the transient dip while a bundle is in-flight
            // (SOL moves to the WSOL ATA and returns within ~2 s when the bundle settles).
            let mut below_start_count = 0u32;
            // Overhead is SOL-rent for the native wrap path; an SPL base reserves nothing
            // from its trading capital (gas comes from the separate SOL balance).
            let base_overhead = if base_is_native { BALANCE_OVERHEAD_LAMPORTS } else { 0 };
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;

                // Native SOL balance — always needed (gas guard + native P&L).
                // A fetch ERROR must NOT touch the strike counter or publish a fake 0:
                // a transient RPC failure should look like "no reading this tick", not a
                // drawdown. Log and skip the guard until the next poll.
                let b_sol = match rpc.get_balance(&wallet).await {
                    Ok(b) => b,
                    Err(e) => { warn!("Balance cache refresh failed: {e}"); continue; }
                };
                // Base-token capital balance — same error treatment for the SPL ATA fetch.
                let b_base = if base_is_native {
                    b_sol
                } else {
                    let ata = spl_associated_token_account::get_associated_token_address(&wallet, &base_mint_for_cache);
                    match rpc.get_token_account_balance(&ata).await {
                        Ok(ui) => match ui.amount.parse::<u64>() {
                            Ok(v) => v,
                            Err(e) => { warn!("Could not parse base-token ({base_symbol}) balance: {e}"); continue; }
                        },
                        Err(e) => { warn!("Base-token ({base_symbol}) balance refresh failed: {e}"); continue; }
                    }
                };
                // Publish spendable capital for the hot loop's amount_in cap.
                cache.store(b_base, Ordering::Relaxed);

                if !dry_run && start_base_balance > 0 {
                    let pnl_threshold = start_base_balance.saturating_sub(base_overhead);
                    match crate::arbitrage::capital::evaluate_halt(
                        b_base, pnl_threshold, b_sol, gas_floor, base_is_native,
                    ) {
                        crate::arbitrage::capital::HaltDecision::HaltGas => {
                            error!(
                                "HALT: SOL gas balance {:.6} below floor {:.6} — cannot pay tips/fees.",
                                b_sol as f64 / 1e9, gas_floor as f64 / 1e9,
                            );
                            std::process::exit(1);
                        }
                        crate::arbitrage::capital::HaltDecision::WarnPnl => {
                            below_start_count += 1;
                            if below_start_count >= 2 {
                                error!(
                                    "HALT: base {} {} below threshold {} (start {}) — stopping to prevent further losses.",
                                    base_symbol, b_base, pnl_threshold, start_base_balance,
                                );
                                std::process::exit(1);
                            }
                            warn!(
                                "Base {} balance {} below P&L threshold {} (start {}) — will halt if still low next poll",
                                base_symbol, b_base, pnl_threshold, start_base_balance,
                            );
                        }
                        crate::arbitrage::capital::HaltDecision::Continue => {
                            below_start_count = 0;
                        }
                    }
                }
            }
        });
    }

    // ── SOL/USD price poller ───────────────────────────────────────────────────
    // Feeds the in-process price cache used to size SOL-denominated Jito tips when the
    // base token is non-native (USDC). Must run in THIS process so publish + get_fresh
    // share the binary crate's static. Harmless for a SOL base (conversion is identity).
    tokio::spawn(async move {
        let http = reqwest::Client::new();
        loop {
            match arbitrage::sol_price::fetch_sol_usd(&http).await {
                Ok(px) => arbitrage::sol_price::publish(px),
                Err(e) => warn!("SOL/USD price poll failed: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(45)).await;
        }
    });

    // ── Graph-update signal (watch channel) ───────────────────────────────────
    // The callback only updates pool state then sends a signal.
    // A dedicated task does the Bellman-Ford search, so the gRPC receive loop
    // is never blocked by graph computation.
    let (update_tx, update_rx) = watch::channel(0u64); // counter: incremented on every pool change
    let latency_stats = arbitrage::latency::LatencyStats::new();

    // ── Rate-limiting primitives ──────────────────────────────────────────────
    let bundle_in_flight: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let submit_sem: Arc<Semaphore>         = Arc::new(Semaphore::new(MAX_CONCURRENT_SUBMISSIONS));
    // Timestamp (UNIX millis) of the most-recent bundle submission attempt.
    // Enforces a minimum interval between submissions so rapid rejection bursts
    // (~250 ms Tokyo RTT × many cycles) don't exceed Jito's per-IP rate limit.
    let last_submission_ms: Arc<AtomicU64> = Arc::new(AtomicU64::new(0));
    const MIN_SUBMISSION_INTERVAL_MS: u64 = 1_500;

    // ── Whale back-run primitives ─────────────────────────────────────────────
    // Separate gate prevents tx flood from overwhelming the BF signal channel.
    let whale_in_flight: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let (whale_hint_tx, mut whale_hint_rx) =
        tokio::sync::mpsc::unbounded_channel::<(solana_sdk::pubkey::Pubkey, u64, u64)>();
    // (pool_id, estimated_sol_lamports, slot)
    /// Cooldown after a simulation failure or on-chain failure (market moved — retry soon).
    const CYCLE_FAIL_COOLDOWN_SECS: u64 = 30;
    /// Cooldown after a bundle is in-flight (from submission until first DROPPED check).
    const CYCLE_SUBMIT_COOLDOWN_SECS: u64 = 25;
    // Stale tick array cooldown.  For pools with large tick_spacing (e.g. 64) one gRPC
    // update resolves it in < 1 s, but for tick_spacing=1 pools (Orca SOL/mSOL, Raydium
    // SOL/mSOL) the tick moves continuously on every swap, making 2 s far too short —
    // the cycle re-fires every 2 s and spams simulation indefinitely.  30 s matches the
    // MarketRejected cooldown and prevents the spam while still retrying reasonably soon.
    const STALE_TICK_COOLDOWN_SECS: u64 = 30;
    // Cooldown after a DROPPED outcome (lost the Jito block auction — suppress the
    // exact cycle briefly so we don't spam identical bundles, but free the pools
    // immediately so other cycles through the same pools can fire right away).
    const CYCLE_DROPPED_COOLDOWN_SECS: u64 = 15;
    // Each entry is (stamped_at, cooldown_duration_secs).  The cycle is suppressed while
    // stamped_at.elapsed() < cooldown_duration_secs.
    let failed_cycles: Arc<dashmap::DashMap<u64, (std::time::Instant, u64)>> =
        Arc::new(dashmap::DashMap::new());
    // Counts how many times each cycle has been dropped (lost Jito auction).
    // Used to compute exponential backoff: cooldown = BASE * 2^min(drops, MAX_SHIFT).
    let drop_counts: Arc<dashmap::DashMap<u64, u32>> = Arc::new(dashmap::DashMap::new());
    // Pool-level cooldown: keyed by pool Pubkey.  When ANY cycle through a pool is
    // submitted, all other cycles sharing that pool are blocked for the same window.
    // This prevents the bot from spamming 4+ identical bundles through HcjZvfeS when
    // one is already in-flight.  Uses the same (stamped_at, cooldown_secs) convention.
    let submitted_pools: Arc<dashmap::DashMap<solana_sdk::pubkey::Pubkey, (std::time::Instant, u64)>> =
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
                    pool.stamp_update();
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
                    pool.stamp_update();
                    graph_cb.update_pool(pool);
                    any = true;
                }
            }
            any
        } else if let Some(pool) = registry_cb.get_by_state_account(&pubkey) {
            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(&data, &pool) {
                pool.sqrt_price_x64.store(price.to_bits(), Ordering::Relaxed);
                if fee_bps > 0 {
                    pool.fee_bps.store(fee_bps, Ordering::Relaxed);
                }
                pool.stamp_update();
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

    // ── Whale back-run callback (transaction subscription) ────────────────────
    // Fires on every confirmed transaction that touches a tracked vault account.
    // Filters by swap size, does a vault→pool lookup, and sends a hint to the
    // whale task which bypasses the BF debounce to evaluate immediately.
    let tx_callback: streamer::client::TransactionCallback = {
        let registry_whale   = Arc::clone(&registry);
        let whale_hint_tx_cb = whale_hint_tx.clone();
        let whale_threshold  = config.whale_min_sol_lamports;
        Arc::new(move |account_keys: Vec<[u8; 32]>, estimated: u64, slot: u64| {
            if estimated < whale_threshold { return; }
            for key_bytes in &account_keys {
                let pubkey = solana_sdk::pubkey::Pubkey::from(*key_bytes);
                if let Some(pools) = registry_whale.get_by_vault(&pubkey) {
                    if let Some(pool) = pools.first() {
                        let _ = whale_hint_tx_cb.send((pool.id, estimated, slot));
                        return; // one hint per transaction is enough
                    }
                }
            }
        })
    };

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
        let in_flight_bf          = Arc::clone(&bundle_in_flight);
        let sem_bf                = Arc::clone(&submit_sem);
        let last_submission_ms_bf = Arc::clone(&last_submission_ms);
        let failed_bf          = Arc::clone(&failed_cycles);
        let drop_counts_bf     = Arc::clone(&drop_counts);
        let submitted_pools_bf = Arc::clone(&submitted_pools);
        let blockhash_bf    = Arc::clone(&cached_blockhash);
        let balance_bf      = Arc::clone(&cached_balance);
        let tip_floor_bf    = Arc::clone(&tip_floor_cache);
        let latency_stats_bf = Arc::clone(&latency_stats);
        let alt_bf          = Arc::clone(&alts);
        let jupiter_client_bf   = Arc::clone(&jupiter_client);
        let jupiter_alt_cache_bf = Arc::clone(&jupiter_alt_cache);
        let mut update_rx   = update_rx;
        let debounce_ms     = config.bellman_ford_debounce_ms;

        tokio::spawn(async move {
            // ── Per-window stats (reset every 10 s, same cadence as "Stream alive") ──
            let mut stat_bf_runs:        u64   = 0;
            let mut stat_cycles:         u64   = 0; // negative cycles BF found
            let mut stat_profitable:     u64   = 0; // cycles (not runs) that passed full evaluation
            let mut stat_eval_rejected:  u64   = 0; // cycles evaluated but unprofitable
            let mut stat_stale_gated:    u64   = 0; // cycles skipped: stalest leg > MAX_CYCLE_STALENESS_MS
            let mut stat_best_gross_bps: f64   = 0.0; // best margin among NEGATIVE cycles (bps)
            // Best ratio across ALL examined paths (negative + positive weight). When
            // stat_cycles is 0, this reveals whether the market is just below break-even
            // (e.g. -3.5 bps, no real arb available) vs. broken pricing (e.g. -500 bps).
            let mut stat_best_overall_bps: f64 = f64::NEG_INFINITY;
            let mut stat_paths_examined: u64   = 0;
            let mut stat_last = std::time::Instant::now();
            const STAT_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

            // Suppress repeated logs of the same cycle within this window.
            let mut cycle_log_seen: std::collections::HashMap<u64, std::time::Instant> =
                std::collections::HashMap::new();
            const CYCLE_LOG_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

            loop {
                // Wait until any pool changed
                if update_rx.changed().await.is_err() { break; }
                // Mark current version as seen before running BF so any update
                // that arrives *during* the BF run triggers the next iteration.
                let _version = *update_rx.borrow_and_update();

                // ── Periodic stats log (every 10 s) ──────────────────────────
                // Checked at the top of the loop so that each run's BF cycle
                // detection and its evaluation are always in the same window.
                // Checking mid-run would split neg_cycles and evaluated across
                // two windows, making evaluated > neg_cycles possible.
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
                    let floor = tip_floor_bf.load(Ordering::Relaxed);
                    let floor_str = if floor > 0 { format!("{}L", floor) } else { "n/a".to_string() };
                    info!(
                        "BF window — runs={} neg_cycles={} evaluated={} profitable={} gated_stale={} ({:.1} runs/s) \
                         best_margin={:+.2}bps best_overall={} tip_floor_ema50={} | \
                         edges={} (raydium={} clmm={} orca={} damm={} dlmm={} phoenix={} jupiter={}) avg_paths/run={:.0}",
                        stat_bf_runs, stat_cycles, stat_eval_rejected + stat_profitable,
                        stat_profitable, stat_stale_gated, stat_bf_runs as f64 / secs, stat_best_gross_bps,
                        best_overall_str, floor_str, edges,
                        by_dex[0], by_dex[1], by_dex[2], by_dex[3], by_dex[4], by_dex[5], by_dex[9], avg_paths,
                    );
                    // Feed-health: name the pools whose last live update is oldest.
                    // Seconds-old entries on high-volume pools mean the gRPC feed
                    // is starving the graph (stale edges → phantom cycles).
                    let sr = registry_bf.staleness_report(arbitrage::latency::now_ns(), 5);
                    if sr.stamped > 0 {
                        let line: String = sr.stalest.iter()
                            .map(|(id, age_ns)| format!("{}={:.1}s", &id.to_string()[..8], *age_ns as f64 / 1e9))
                            .collect::<Vec<_>>()
                            .join(" ");
                        info!("STALEST pools: {line} | median={:.1}s  >5s: {}  >60s: {}  (of {} stamped)",
                            sr.median_ms as f64 / 1000.0, sr.over_5s, sr.over_60s, sr.stamped);
                    }
                    if let Some(report) = latency_stats_bf.maybe_report(floor) {
                        info!("\n{report}");
                    }
                    stat_bf_runs           = 0;
                    stat_cycles            = 0;
                    stat_profitable        = 0;
                    stat_eval_rejected     = 0;
                    stat_stale_gated       = 0;
                    stat_best_gross_bps    = 0.0;
                    stat_best_overall_bps  = f64::NEG_INFINITY;
                    stat_paths_examined    = 0;
                    stat_last              = std::time::Instant::now();
                    let now = std::time::Instant::now();
                    cycle_log_seen.retain(|_, t| now.duration_since(*t) < CYCLE_LOG_COOLDOWN);
                }

                // ── Bellman-Ford ──────────────────────────────────────────────
                stat_bf_runs += 1;
                let mut timeline = arbitrage::latency::LatencyTimeline {
                    bf_start: Some(arbitrage::latency::now_ns()),
                    ..Default::default()
                };
                let search = bellman_ford::find_negative_cycles_with_diag(&graph_bf, base_mint);
                timeline.bf_done = Some(arbitrage::latency::now_ns());
                let cycles = search.cycles;
                stat_paths_examined += search.n_paths_examined as u64;
                if search.best_weight.is_finite() {
                    let overall_bps = ((-search.best_weight).exp() - 1.0) * 10_000.0;
                    if overall_bps > stat_best_overall_bps { stat_best_overall_bps = overall_bps; }
                }

                if cycles.is_empty() {
                    // Coalesce rapid-fire pool updates: only debounce on idle runs so
                    // profitable cycles are evaluated and submitted without artificial delay.
                    if debounce_ms > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
                        let _ = update_rx.borrow_and_update();
                    }
                    debug!("Bellman-Ford: no negative cycles found");
                } else {
                    stat_cycles += cycles.len() as u64;
                    for (i, c) in cycles.iter().enumerate() {
                        let gross_bps = (c.gross_ratio() - 1.0) * 10_000.0;
                        stat_best_gross_bps = stat_best_gross_bps.max(gross_bps);
                        debug!("  cycle[{i}] hops={} gross_ratio={:.6} total_weight={:.6}",
                            c.edges.len(), c.gross_ratio(), c.total_weight);
                        if gross_bps >= config_bf.log_cycle_threshold_bps {
                            let fp = {
                                use std::hash::{Hash, Hasher};
                                let mut h = std::collections::hash_map::DefaultHasher::new();
                                for e in &c.edges { e.pool_id.hash(&mut h); e.a_to_b.hash(&mut h); }
                                h.finish()
                            };
                            let now = std::time::Instant::now();
                            if cycle_log_seen.get(&fp).map_or(true, |t| now.duration_since(*t) >= CYCLE_LOG_COOLDOWN) {
                                cycle_log_seen.insert(fp, now);
                                let path_str: String = {
                                    let mut s = mint_symbol(&c.path[0]).to_string();
                                    for e in &c.edges {
                                        s.push_str(&format!(" -[{}:{}]→ {}",
                                            e.dex.short_name(),
                                            &e.pool_id.to_string()[..8],
                                            mint_symbol(&e.to)));
                                    }
                                    s
                                };
                                info!("cycle gross={:+.2}bps  {}", gross_bps, path_str);
                            }
                        }
                    }
                    debug!("Bellman-Ford: {} negative cycle(s) detected", cycles.len());
                }

                if cycles.is_empty() { continue; }

                // ── In-flight guard ───────────────────────────────────────────
                if in_flight_bf.compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed).is_err() {
                    debug!("Bundle already in-flight, skipping {} cycle(s)", cycles.len());
                    continue;
                }

                // ── Evaluate best cycle ───────────────────────────────────────
                // Flash loan: arb capital is borrowed from MarginFi, not the wallet.
                // Wallet still needs BALANCE_OVERHEAD_LAMPORTS for tx fees + Jito tip,
                // but no longer constrains the swap input amount.
                // dry_run: wallet is unfunded on-chain; use configured cap directly.
                let available_sol = if config_bf.dry_run {
                    config_bf.input_base_units
                } else if config_bf.enable_flash_loan {
                    // Capital is borrowed — wallet balance is not the constraint.
                    // The ternary search finds the slippage-optimal amount within this cap.
                    config_bf.flash_loan_max_input_lamports
                } else {
                    let wallet_balance = balance_bf.load(Ordering::Relaxed);
                    let base_overhead = if config_bf.base_token.is_native { BALANCE_OVERHEAD_LAMPORTS } else { 0 };
                    let spendable = crate::arbitrage::capital::spendable_base(
                        wallet_balance, base_overhead, config_bf.input_base_units,
                    );
                    if spendable == 0 {
                        debug!("Base-token balance ({wallet_balance}) too low for overhead reserve — skipping");
                        in_flight_bf.store(false, Ordering::Release);
                        continue;
                    }
                    spendable
                };

                let tip_floor_snapshot = tip_floor_bf.load(Ordering::Relaxed);
                let now_ns_gate = arbitrage::latency::now_ns();
                let mut rejected_this_run  = 0u64;
                let mut profitable_this_run = 0u64;
                let mut stale_gated_this_run = 0u64;
                let mut evaluated: Vec<_> = cycles.iter().filter_map(|c| {
                    let cycle_key: u64 = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        c.path.hash(&mut h);
                        h.finish()
                    };
                    // Skip evaluation for cycles already on cooldown (submission failures,
                    // drops, or phantom AMM failures from a previous BF run).
                    if let Some(entry) = failed_bf.get(&cycle_key) {
                        let (stamped, cooldown) = *entry;
                        if stamped.elapsed().as_secs() < cooldown {
                            rejected_this_run += 1;
                            return None;
                        }
                        drop(entry);
                        failed_bf.remove(&cycle_key);
                    }
                    // Staleness gate: never submit a cycle whose stalest leg is older
                    // than the threshold — such edges are likely phantom (the real
                    // pool has moved) and the bundle dies in Jito Block Engine
                    // simulation regardless of tip. Skip before evaluation so stale
                    // cycles are never chosen. Disabled when threshold == 0.
                    if config_bf.max_cycle_staleness_ms > 0 {
                        let stale_ms = registry_bf.max_staleness_ms(
                            c.edges.iter().map(|e| e.pool_id), now_ns_gate);
                        if stale_ms > config_bf.max_cycle_staleness_ms {
                            stale_gated_this_run += 1;
                            return None;
                        }
                    }
                    let result = arbitrage::evaluator::optimize_input_and_tip(
                        c, &registry_bf, &config_bf, user, available_sol, tip_floor_snapshot, &alt_bf,
                    );
                    match &result {
                        Some(_) => profitable_this_run += 1,
                        None => {
                            rejected_this_run += 1;
                            // Suppress phantom/illiquid cycles (zero AMM output, price impact,
                            // or sanity-cap violations) for the same cooldown as simulation
                            // failures. Prevents repeated evaluation of unchanging dead cycles.
                            failed_bf.insert(cycle_key, (std::time::Instant::now(), CYCLE_FAIL_COOLDOWN_SECS));
                        }
                    }
                    result
                }).collect();
                stat_eval_rejected += rejected_this_run;
                stat_profitable    += profitable_this_run;
                stat_stale_gated   += stale_gated_this_run;

                if evaluated.is_empty() {
                    debug!("Cycles detected but none profitable (input={available_sol} lamports, {rejected_this_run} rejected)");
                    in_flight_bf.store(false, Ordering::Release);
                    continue;
                }

                // Sort best-profit first so the loop below tries the most valuable
                // cycle first and falls through to alternatives when blocked.
                evaluated.sort_unstable_by(|a, b| b.net_profit_base_units.cmp(&a.net_profit_base_units));

                // ── Cooldown check — iterate until a non-blocked cycle is found ─
                // 64-bit hash of the cycle path — avoids heap-allocating a
                // (n_pubkeys × 32)-byte Vec per opportunity, and DashMap key
                // hashing is now O(1) instead of O(96–128).
                let mut chosen: Option<(/* opportunity */ _, /* cycle_key */ u64)> = None;
                for opp in evaluated {
                    let cycle_key: u64 = {
                        use std::hash::{Hash, Hasher};
                        let mut h = std::collections::hash_map::DefaultHasher::new();
                        opp.cycle.path.hash(&mut h);
                        h.finish()
                    };

                    if let Some(entry) = failed_bf.get(&cycle_key) {
                        let (stamped, cooldown) = *entry;
                        if stamped.elapsed().as_secs() < cooldown {
                            debug!("Cycle on cooldown ({:.0}s remaining), trying next",
                                cooldown as f64 - stamped.elapsed().as_secs_f64());
                            continue;
                        }
                        drop(entry);
                        failed_bf.remove(&cycle_key);
                    }

                    let blocking_pool = opp.cycle.edges.iter().find(|e| {
                        submitted_pools_bf.get(&e.pool_id)
                            .map(|entry| { let (stamped, cd) = *entry; stamped.elapsed().as_secs() < cd })
                            .unwrap_or(false)
                    });
                    if let Some(e) = blocking_pool {
                        debug!(pool = &e.pool_id.to_string()[..8], "Pool in-flight — trying next cycle");
                        continue;
                    }

                    chosen = Some((opp, cycle_key));
                    break;
                }

                let Some((opportunity, cycle_key)) = chosen else {
                    debug!("All profitable cycles on cooldown or pool-blocked");
                    in_flight_bf.store(false, Ordering::Release);
                    continue;
                };

                timeline.eval_done = Some(arbitrage::latency::now_ns());
                let pool_stamps: Vec<(solana_sdk::pubkey::Pubkey, u64)> = opportunity.cycle.edges.iter()
                    .filter_map(|e| registry_bf.get_by_pool_id(&e.pool_id)
                        .map(|p| (e.pool_id, p.last_update_ns.load(Ordering::Relaxed))))
                    .collect();
                timeline.set_pool_stamps(&pool_stamps);

                // ── Global submission rate limiter ────────────────────────────
                // Prevents rapid-fire chains: after a rejection (~250 ms Tokyo RTT)
                // the gate releases and the BF immediately finds the next cycle,
                // producing 3–4 submissions/second × 5 regions ≈ 18 API calls/s
                // which triggers Jito's per-IP rate limit (-32097 on all regions).
                {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let last_ms = last_submission_ms_bf.load(Ordering::Acquire);
                    if now_ms.saturating_sub(last_ms) < MIN_SUBMISSION_INTERVAL_MS {
                        debug!(
                            elapsed_ms = now_ms.saturating_sub(last_ms),
                            "Rate-limit guard: too soon since last submission — skipping"
                        );
                        in_flight_bf.store(false, Ordering::Release);
                        continue;
                    }
                    last_submission_ms_bf.store(now_ms, Ordering::Release);
                }

                info!("{}", opportunity.summary());

                // ── Spawn submission task ─────────────────────────────────────
                let rpc_bf_t     = Arc::clone(&rpc_bf);
                let jito         = Arc::clone(&jito_bf);
                let keypair      = Arc::clone(&keypair_bf);
                let in_flight    = Arc::clone(&in_flight_bf);
                let sem          = Arc::clone(&sem_bf);
                let failed_t           = Arc::clone(&failed_bf);
                let drop_counts        = Arc::clone(&drop_counts_bf);
                let submitted_pools_t  = Arc::clone(&submitted_pools_bf);
                let pool_ids_t: Vec<solana_sdk::pubkey::Pubkey> =
                    opportunity.cycle.edges.iter().map(|e| e.pool_id).collect();
                let cycle_key_t  = cycle_key.clone();
                let bh_cache     = Arc::clone(&blockhash_bf);
                let config_t     = Arc::clone(&config_bf);
                let tip_floor_t  = Arc::clone(&tip_floor_bf);
                let alt_t        = Arc::clone(&alt_bf); // Arc<Vec<AddressLookupTableAccount>>
                let jup_client_t    = Arc::clone(&jupiter_client_bf);
                let jup_alt_cache_t = Arc::clone(&jupiter_alt_cache_bf);
                let user_t          = user;
                let latency_stats_t = Arc::clone(&latency_stats_bf);

                tokio::spawn(async move {
                    let mut opportunity = opportunity;
                    let mut timeline = timeline;
                    timeline.spawned = Some(arbitrage::latency::now_ns());
                    let _permit = sem.acquire().await.expect("Semaphore closed");
                    timeline.sem_acquired = Some(arbitrage::latency::now_ns());
                    let guard   = InFlightGuard(&in_flight);

                    // Resolve any Jupiter hops: fetch real /swap-instructions, splice into the
                    // opportunity, merge Jupiter's ALTs with ours, and re-run the wire-size guard.
                    // For all-local cycles this is a no-op returning the base ALTs unchanged.
                    let submit_alts = match resolve_jupiter_hops(
                        &mut opportunity, &rpc_bf_t, &jup_client_t, &jup_alt_cache_t,
                        &alt_t, user_t, &config_t,
                    ).await {
                        Ok(a) => a,
                        Err(e) => { warn!("Jupiter hop resolution failed — skipping: {e}"); return; }
                    };
                    timeline.jup_resolved = Some(arbitrage::latency::now_ns());

                    // Use pre-cached blockhash — saves ~100 ms vs get_latest_blockhash()
                    let blockhash = *bh_cache.read().await;

                    let bundle = match JitoBundle::build(&opportunity, &keypair, blockhash, &config_t, &submit_alts) {
                        Ok(b) => b,
                        Err(e) => { error!("Bundle build failed: {e}"); return; }
                    };
                    timeline.built = Some(arbitrage::latency::now_ns());

                    // Extract swap txs before moving bundle — simulation runs after Jito submit.
                    // Direct-RPC flash loan path: bundle has 1 tx, [..0] = empty → sim skipped
                    // (MarginFi health checks can't be reliably simulated off-chain anyway).
                    use arbitrage::simulator::SimOutcome;
                    let sim_run = !config_t.disable_simulation && !config_t.dry_run;
                    let swap_txs: Vec<solana_sdk::transaction::VersionedTransaction> = if sim_run {
                        bundle.transactions[..bundle.transactions.len().saturating_sub(1)].to_vec()
                    } else {
                        vec![]
                    };

                    // ── Jito bundle submission ─────────────────────────────────────────
                    // All flash loan cycles go via Jito — thin cycles with floor-anchored
                    // tip, fat cycles with ratio-based tip. Raw RPC fails for v0+ALT txs
                    // on non-Jito validators (~10% of stake have no ALT program resolution).
                    timeline.submit_started = Some(arbitrage::latency::now_ns());
                    match jito.submit_bundle(&bundle).await {
                        Ok(receipt) => {
                            timeline.accepted = Some(arbitrage::latency::now_ns());
                            timeline.region = Some(receipt.region);
                            let jito::client::SubmitReceipt { bundle_id: id, accept_ms, .. } = receipt;
                            let floor_now = tip_floor_t.load(Ordering::Relaxed);
                            let ratio_str = if floor_now > 0 {
                                format!("  floor={}L  ratio={}×", floor_now,
                                    opportunity.jito_tip_lamports / floor_now.max(1))
                            } else { String::new() };
                            eprintln!("\x1b[31mBundle submitted  bundle_id={}  tip={}  net_profit={}{}\x1b[0m",
                                id, opportunity.jito_tip_lamports, opportunity.net_profit_base_units, ratio_str);
                            info!("{}", timeline.summary_line(accept_ms, opportunity.jito_tip_lamports, floor_now));
                            // Mark cycle + pools in-flight before releasing the global guard.
                            failed_t.insert(cycle_key_t.clone(), (std::time::Instant::now(), CYCLE_SUBMIT_COOLDOWN_SECS));
                            for &pid in &pool_ids_t {
                                submitted_pools_t.insert(pid, (std::time::Instant::now(), CYCLE_SUBMIT_COOLDOWN_SECS));
                            }
                            // Release in_flight now — pool-level cooldowns block re-entry
                            // through the same pools, so BF can immediately chase other cycles.
                            drop(guard);

                            // Spawn monitor (independent of simulation below).
                            // Watch the outcome and apply the appropriate cooldown:
                            //   Landed        → remove pool entries (opportunity fully captured)
                            //   FailedOnChain → 30 s on cycle + pools (market moved)
                            //   Dropped       → 15 s on cycle only, pools freed (lost Jito auction)
                            let jito_poll            = Arc::clone(&jito);
                            let failed_outcome       = Arc::clone(&failed_t);
                            let sp_outcome           = Arc::clone(&submitted_pools_t);
                            let drop_counts_outcome  = Arc::clone(&drop_counts);
                            let pool_ids_outcome     = pool_ids_t.clone();
                            let cycle_key_outcome    = cycle_key_t.clone();
                            let tip_dropped          = opportunity.jito_tip_lamports;
                            let amount_in_dropped    = opportunity.amount_in;
                            let cap_dropped          = available_sol; // ternary search upper bound
                            let floor_dropped        = Arc::clone(&tip_floor_t);
                            let stats_outcome      = Arc::clone(&latency_stats_t);
                            let timeline_outcome   = timeline; // Copy
                            let accept_ms_outcome  = accept_ms;
                            let floor_at_submit    = floor_now;
                            tokio::spawn(async move {
                                use jito::client::BundleOutcome;
                                let outcome = jito_poll.log_bundle_outcome(&id).await;
                                let rec_outcome = match &outcome {
                                    BundleOutcome::Landed        => arbitrage::latency::RecordOutcome::Landed,
                                    BundleOutcome::FailedOnChain => arbitrage::latency::RecordOutcome::FailedOnChain,
                                    BundleOutcome::Dropped       => arbitrage::latency::RecordOutcome::Dropped,
                                };
                                if let Some(rec) = arbitrage::latency::LatencyRecord::from_timeline(
                                    &timeline_outcome, accept_ms_outcome, tip_dropped, floor_at_submit, rec_outcome,
                                ) {
                                    stats_outcome.record(rec);
                                }
                                match outcome {
                                    BundleOutcome::Landed => {
                                        for pid in &pool_ids_outcome { sp_outcome.remove(pid); }
                                    }
                                    BundleOutcome::FailedOnChain => {
                                        failed_outcome.insert(cycle_key_outcome, (std::time::Instant::now(), CYCLE_FAIL_COOLDOWN_SECS));
                                        for &pid in &pool_ids_outcome {
                                            sp_outcome.insert(pid, (std::time::Instant::now(), CYCLE_FAIL_COOLDOWN_SECS));
                                        }
                                    }
                                    BundleOutcome::Dropped => {
                                        // Exponential backoff: each consecutive drop doubles the
                                        // cooldown (capped at 4 doublings = 240 s). This prevents
                                        // chronically-unwinnable cycles from monopolising submission
                                        // slots while still allowing fresh cycles a short first retry.
                                        const MAX_SHIFT: u32 = 4; // cap at 15 * 2^4 = 240 s
                                        let drops = {
                                            let mut entry = drop_counts_outcome
                                                .entry(cycle_key_outcome)
                                                .or_insert(0);
                                            *entry += 1;
                                            *entry
                                        };
                                        let base_cooldown = CYCLE_DROPPED_COOLDOWN_SECS
                                            * (1u64 << (drops - 1).min(MAX_SHIFT));
                                        warn!(
                                            "Bundle DROPPED — cycle suppressed {base_cooldown}s \
                                             (drop #{drops}, backoff ×{}), pools blocked {POOL_DROP_COOLDOWN_SECS}s",
                                            1u64 << (drops - 1).min(MAX_SHIFT),
                                        );
                                        const COMPETITIVE_MULTIPLE: u64 = 5_000;
                                        const BALANCE_OVERHEAD: u64     = 8_000_000;
                                        let floor = floor_dropped.load(Ordering::Relaxed);
                                        if floor > 0 && tip_dropped > 0 {
                                            let target_tip = floor.saturating_mul(COMPETITIVE_MULTIPLE);
                                            if target_tip > tip_dropped {
                                                // Tip is below the competitive threshold.
                                                // Only suggest more capital if the ternary search
                                                // hit the cap (amount_in ≥ 95 % of available_sol).
                                                // If the optimal is well below the cap, the pool depth
                                                // limits profitability — more capital increases slippage
                                                // and would NOT raise the tip.
                                                let at_cap = cap_dropped > 0
                                                    && amount_in_dropped >= cap_dropped.saturating_mul(95) / 100;
                                                if at_cap {
                                                    let scale = target_tip as f64 / tip_dropped as f64;
                                                    let needed = ((amount_in_dropped as f64 * scale) as u64)
                                                        .saturating_add(BALANCE_OVERHEAD);
                                                    warn!(
                                                        "  → competitive tip {}L (floor {}L × {}): \
                                                         suggested capital ≥{:.1} SOL \
                                                         (current {:.2} SOL bid {}L)",
                                                        target_tip, floor, COMPETITIVE_MULTIPLE,
                                                        needed as f64 / 1e9,
                                                        amount_in_dropped as f64 / 1e9, tip_dropped,
                                                    );
                                                } else {
                                                    warn!(
                                                        "  → competitive tip {}L (floor {}L × {}): \
                                                         pool-depth limited at {:.2} SOL — \
                                                         more capital increases slippage, not tip \
                                                         (bid {}L)",
                                                        target_tip, floor, COMPETITIVE_MULTIPLE,
                                                        amount_in_dropped as f64 / 1e9, tip_dropped,
                                                    );
                                                }
                                            } else {
                                                warn!(
                                                    "  → tip {}L already exceeds floor × {} target {}L — \
                                                     drops are from arb-specific competition, not capital",
                                                    tip_dropped, COMPETITIVE_MULTIPLE, target_tip,
                                                );
                                            }
                                        }
                                        // Escalate to 1-hour suppression once the backoff is saturated
                                        // and the tip is demonstrably unwinnable (tip < floor × 5000).
                                        // Prevents an indefinite 240s retry loop on structurally lost cycles.
                                        const ABANDONED_COOLDOWN_SECS: u64 = 3_600;
                                        let cooldown = if drops > MAX_SHIFT
                                            && floor > 0
                                            && tip_dropped < floor.saturating_mul(COMPETITIVE_MULTIPLE)
                                        {
                                            warn!(
                                                "  → Cycle abandoned for {}s — tip {}L is structurally \
                                                 uncompetitive (floor {}L × {} = {}L, bid only {}×)",
                                                ABANDONED_COOLDOWN_SECS,
                                                tip_dropped, floor, COMPETITIVE_MULTIPLE,
                                                floor.saturating_mul(COMPETITIVE_MULTIPLE),
                                                tip_dropped / floor.max(1),
                                            );
                                            ABANDONED_COOLDOWN_SECS
                                        } else {
                                            base_cooldown
                                        };
                                        failed_outcome.insert(cycle_key_outcome, (std::time::Instant::now(), cooldown));
                                        // Keep pools blocked briefly after a drop so other cycle
                                        // paths through the same dislocated hub pool don't cascade
                                        // into back-to-back doomed submissions. 8 s lets the market
                                        // correct one or two blocks before the next attempt.
                                        const POOL_DROP_COOLDOWN_SECS: u64 = 8;
                                        for &pid in &pool_ids_outcome {
                                            sp_outcome.insert(pid, (std::time::Instant::now(), POOL_DROP_COOLDOWN_SECS));
                                        }
                                    }
                                }
                            });

                            // Simulate post-submission for diagnostics and cooldown refinement.
                            // Slippage guards in swap instructions are the on-chain safety net;
                            // simulation here only improves cooldown accuracy on market moves.
                            if sim_run {
                                match arbitrage::simulator::simulate_opportunity(
                                    &opportunity, &swap_txs, &rpc_bf_t
                                ).await {
                                    Ok(SimOutcome::Passed) => {}
                                    Ok(SimOutcome::MarketRejected { hop, err }) => {
                                        info!(hop, ?err, "Simulation: market moved — tightening cycle cooldown");
                                        failed_t.insert(cycle_key_t.clone(), (std::time::Instant::now(), CYCLE_FAIL_COOLDOWN_SECS));
                                    }
                                    Ok(SimOutcome::StaleTickData { hop, err }) => {
                                        info!(hop, ?err, "Simulation: stale tick — tightening cycle cooldown");
                                        failed_t.insert(cycle_key_t.clone(), (std::time::Instant::now(), STALE_TICK_COOLDOWN_SECS));
                                    }
                                    Ok(SimOutcome::InfraError { hop, err }) => {
                                        error!(hop, ?err, "Simulation infra error (check pool config / ATA setup)");
                                        failed_t.insert(cycle_key_t.clone(), (std::time::Instant::now(), CYCLE_FAIL_COOLDOWN_SECS));
                                    }
                                    Err(e) => { warn!("Simulation RPC error: {e}"); }
                                }
                            }
                        }
                        Err(e) => {
                            error!("Bundle submission failed: {e}");
                            failed_t.insert(cycle_key_t.clone(), (std::time::Instant::now(), CYCLE_FAIL_COOLDOWN_SECS));
                        }
                    }
                });
            }
        });
    }


    // ── Whale back-run task ───────────────────────────────────────────────────
    // Receives whale hints, sleeps briefly for the account update to arrive,
    // then pokes the BF watch channel to bypass the normal debounce window.
    {
        let update_tx_whale = update_tx.clone();
        let in_flight_whale = Arc::clone(&whale_in_flight);
        let delay_ms        = config.whale_back_run_delay_ms;
        tokio::spawn(async move {
            while let Some((pool_id, estimated_sol, slot)) = whale_hint_rx.recv().await {
                // Skip if a whale evaluation is already queued
                if in_flight_whale
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_err()
                {
                    continue;
                }
                let size_sol = estimated_sol as f64 / 1e9;
                tracing::debug!(
                    pool = %&pool_id.to_string()[..8],
                    slot,
                    size_sol,
                    "Whale tx detected — bypassing BF debounce"
                );
                // Let the vault account update arrive and write new reserves into atomics
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                update_tx_whale.send_modify(|v| *v = v.wrapping_add(1));
                in_flight_whale.store(false, Ordering::Release);
            }
        });
    }

    let mut streamer = GrpcStreamer::new(Arc::clone(&config));
    let initial_subscription = build_account_subscription(&account_keys);
    streamer.start(initial_subscription, callback, Some(tx_callback)).await?;
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
