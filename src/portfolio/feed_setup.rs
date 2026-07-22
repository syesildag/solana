//! gRPC price-feed bootstrap: resolve (watched token × pool) pairs into vault/state
//! subscriptions and spawn the Yellowstone stream task. Lives in the lib (not the
//! watcher bin) so the runtime scan handler can RE-spawn the feed when dynamically
//! discovered pools change (spec 2026-07-22-grpc-priced-scan-discoveries).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use futures::StreamExt;
use tracing::{error, info, warn};
use yellowstone_grpc_proto::geyser::{
    geyser_client::GeyserClient, subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts,
};

use crate::dex;
use crate::portfolio::grpc_pricer::{self, GrpcFeed};
use crate::portfolio::momentum_universe::WatchedToken;
use crate::portfolio::{scanner, PortfolioConfig};

/// Account role within a pool subscription — determines which atomic field to update.
#[derive(Clone, Copy)]
enum Role {
    VaultA,
    VaultB,
    State,
}

/// One momentum pool tracked live from gRPC account updates, backed by a real `dex::Pool`.
/// Supports CP pools (RaydiumAmmV4/Saber/PumpSwap) via vault reserves and CL pools
/// (OrcaWhirlpool/RaydiumClmm/MeteoraDlmm/Invariant) via state account sqrt_price_x64.
struct WiredPool {
    pool: std::sync::Arc<dex::types::Pool>,
    token_mint: String,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    /// `MOMENTUM_TRADE_USDC` (global config value, not per-token) — sizes the local
    /// price-impact estimate published after each update (Task 5, `MOMENTUM_LOCAL_IMPACT`).
    trade_usdc: f64,
}

impl WiredPool {
    /// Current USD price of the momentum token, or None if the pool state isn't ready.
    fn price_usd(&self, sol_usd: f64) -> Option<f64> {
        use dex::types::DexKind::*;
        let raw = match self.pool.dex {
            // CL: sqrt_price_x64 holds parse_cl_pool_state's `price` (token_b per token_a, raw units).
            OrcaWhirlpool | RaydiumClmm | MeteoraDlmm | Invariant => {
                let price = f64::from_bits(self.pool.sqrt_price_x64.load(std::sync::atomic::Ordering::Relaxed));
                if !(price > 0.0) { return None; }        // not initialised yet
                if self.momentum_is_token_a { price } else { 1.0 / price }
            }
            // CP: reserve-based rate from snapshot_state.
            _ => {
                let st = self.pool.snapshot_state();
                if self.momentum_is_token_a { st.rate_a_to_b() } else { st.rate_b_to_a() }
            }
        };
        grpc_pricer::rate_to_usd(raw, self.dec_momentum, self.dec_quote, self.quote_is_usdc, sol_usd)
    }
}

/// Merge ad-hoc decoded pool configs UNDER the pools.json set: on id collision the
/// pools.json entry wins — curated wiring is authoritative, a scan must never
/// re-route a curated token's pricing.
pub fn merge_pool_configs(
    from_file: Vec<dex::types::PoolConfig>,
    extra: Vec<dex::types::PoolConfig>,
) -> HashMap<String, dex::types::PoolConfig> {
    let mut map: HashMap<String, dex::types::PoolConfig> =
        extra.into_iter().map(|c| (c.id.clone(), c)).collect();
    for c in from_file {
        map.insert(c.id.clone(), c);
    }
    map
}

/// Build the gRPC price feed for momentum tokens configured with `pool`+`quote` in
/// momentum_tokens.json. CP (raydium_amm_v4/saber/pump_swap) pools are priced from vault
/// reserves; CL pools (Orca Whirlpool, Raydium CLMM, Meteora DLMM, Invariant) from their
/// state account. Other DEX kinds fall back to REST. Returns None when the feature is off
/// or no eligible pool is configured. The pool's structure (vaults, fee) is resolved from
/// pools.json (pump_swap entries live there for the watcher only — the arb bot's registry
/// skips them).
pub async fn spawn_grpc_feed(
    cfg: &PortfolioConfig,
    watched: &[WatchedToken],
    extra_pools: &[dex::types::PoolConfig],
) -> Result<Option<(GrpcFeed, tokio::task::JoinHandle<()>)>> {
    if !cfg.momentum_grpc_pricing {
        return Ok(None);
    }
    let Some(endpoint) = cfg.grpc_endpoint.clone() else {
        warn!("MOMENTUM_GRPC_PRICING=true but GRPC_ENDPOINT unset — REST only");
        return Ok(None);
    };
    let token = cfg.grpc_token.clone();

    let pools_raw = std::fs::read_to_string(&cfg.pools_path)
        .with_context(|| format!("reading {}", cfg.pools_path))?;
    let configs: Vec<dex::types::PoolConfig> =
        serde_json::from_str(&pools_raw).context("parsing pools.json")?;
    let merged = merge_pool_configs(configs, extra_pools.to_vec());
    let by_id: HashMap<&str, &dex::types::PoolConfig> =
        merged.iter().map(|(k, v)| (k.as_str(), v)).collect();

    // Eligible (watched token, pool) pairs + the mints we need decimals for.
    struct Pending<'a> {
        tok: &'a WatchedToken,
        pc: &'a dex::types::PoolConfig,
        quote_is_usdc: bool,
    }
    let mut pending: Vec<Pending> = Vec::new();
    let mut decimal_mints: Vec<String> = Vec::new();
    for w in watched {
        // Task 3: a token may carry several (pool, quote) venues — `pools` when present,
        // else the single `pool`+`quote` shorthand as a one-element vec, else empty (REST
        // only). Each ref becomes its own Pending/WiredPool below, same mint throughout.
        for pool_ref in w.pool_refs() {
            let pool_id = pool_ref.pool.as_str();
            let quote = pool_ref.quote.as_str();
            let Some(pc) = by_id.get(pool_id).copied() else {
                warn!("gRPC: pool {pool_id} for {} not in pools.json — REST", w.symbol);
                continue;
            };
            if !matches!(
                pc.dex,
                dex::types::DexKind::RaydiumAmmV4
                    | dex::types::DexKind::Saber
                    | dex::types::DexKind::PumpSwap
                    | dex::types::DexKind::OrcaWhirlpool
                    | dex::types::DexKind::RaydiumClmm
                    | dex::types::DexKind::MeteoraDlmm
                    | dex::types::DexKind::Invariant
            ) {
                warn!(
                    "gRPC: pool {pool_id} for {} is {:?} (unsupported DEX kind) — REST",
                    w.symbol, pc.dex
                );
                continue;
            }
            decimal_mints.push(pc.token_a.clone());
            decimal_mints.push(pc.token_b.clone());
            pending.push(Pending { tok: w, pc, quote_is_usdc: quote.eq_ignore_ascii_case("USDC") });
        }
    }
    if pending.is_empty() {
        warn!("gRPC: no eligible pools configured (raydium_amm_v4/saber/pump_swap/Orca/CLMM/DLMM/Invariant) — REST only");
        return Ok(None);
    }

    let decimals = scanner::fetch_decimals_for_mints(&cfg.rpc_url, decimal_mints)
        .await
        .unwrap_or_default();

    // Resolve each wired token's PoolConfig from pools.json → Arc<Pool>; index accounts by role.
    let mut wired: Vec<WiredPool> = Vec::new();
    let mut acct_index: HashMap<String, (usize, Role)> = HashMap::new();
    for p in &pending {
        let momentum_is_token_a = p.tok.mint == p.pc.token_a;
        let (dm, dq) = if momentum_is_token_a {
            (decimals.get(&p.pc.token_a).copied(), decimals.get(&p.pc.token_b).copied())
        } else {
            (decimals.get(&p.pc.token_b).copied(), decimals.get(&p.pc.token_a).copied())
        };
        let (Some(dec_momentum), Some(dec_quote)) = (dm, dq) else {
            warn!("gRPC: decimals missing for pool {} — REST", p.pc.id);
            continue;
        };
        let pool: std::sync::Arc<dex::types::Pool> = match std::sync::Arc::try_from(p.pc.clone()) {
            Ok(pool) => pool,
            Err(e) => { warn!("gRPC: Pool::try_from failed for {} ({e}) — REST", p.pc.id); continue; }
        };
        // Accounts this pool would subscribe, by role — resolved before touching
        // acct_index so a pool that is a total duplicate of one already wired (e.g. the
        // same pool_id listed twice, or two watched tokens pointing at the same pool)
        // never gets an empty/dead WiredPool pushed just to be ignored forever.
        let candidate_accounts: Vec<(String, Role)> = match pool.dex {
            dex::types::DexKind::RaydiumAmmV4
            | dex::types::DexKind::Saber
            | dex::types::DexKind::PumpSwap => {
                vec![(pool.vault_a.to_string(), Role::VaultA), (pool.vault_b.to_string(), Role::VaultB)]
            }
            // Whirlpool's get_quote depth factor (used by the Task 5 local-impact
            // pre-gate) reads pool.reserve_a/reserve_b — vault balances — so subscribe
            // its vaults too, alongside the state account. A vault write also
            // re-publishes the (unchanged) price with a fresh timestamp, which is
            // semantically correct: a vault write is trading activity.
            dex::types::DexKind::OrcaWhirlpool => {
                let Some(state) = pool.state_account else {
                    warn!("gRPC: {:?} pool {} has no state_account — REST", pool.dex, p.pc.id);
                    continue;
                };
                vec![
                    (state.to_string(), Role::State),
                    (pool.vault_a.to_string(), Role::VaultA),
                    (pool.vault_b.to_string(), Role::VaultB),
                ]
            }
            dex::types::DexKind::RaydiumClmm
            | dex::types::DexKind::MeteoraDlmm
            | dex::types::DexKind::Invariant => {
                let Some(state) = pool.state_account else {
                    warn!("gRPC: {:?} pool {} has no state_account — REST", pool.dex, p.pc.id);
                    continue;
                };
                vec![(state.to_string(), Role::State)]
            }
            other => {
                warn!("gRPC: pool {} is {:?} (not yet supported in this build) — REST", p.pc.id, other);
                continue;
            }
        };
        // First pool to claim an account wins; a later pool that collides on it is
        // skipped (with a warning) rather than silently stealing the subscription.
        let fresh: Vec<&(String, Role)> = candidate_accounts
            .iter()
            .filter(|(key, _)| {
                let is_new = !acct_index.contains_key(key);
                if !is_new {
                    warn!(
                        "gRPC: account {key} for pool {} ({}) already wired to another pool — first wins, skipping duplicate",
                        p.pc.id, p.tok.symbol
                    );
                }
                is_new
            })
            .collect();
        if fresh.is_empty() {
            warn!(
                "gRPC: pool {} for {} duplicates an already-wired pool (all accounts already indexed) — skipping",
                p.pc.id, p.tok.symbol
            );
            continue;
        }
        let idx = wired.len();
        for (key, role) in fresh {
            acct_index.insert(key.clone(), (idx, *role));
        }
        wired.push(WiredPool { pool, token_mint: p.tok.mint.clone(), momentum_is_token_a, dec_momentum, dec_quote, quote_is_usdc: p.quote_is_usdc, trade_usdc: cfg.momentum_trade_usdc });
    }
    if wired.is_empty() { warn!("gRPC: no eligible pools — REST only"); return Ok(None); }
    let accounts: Vec<String> = acct_index.keys().cloned().collect();
    info!("gRPC price feed: subscribing {} accounts for {} pool(s)", accounts.len(), wired.len());

    let mut feed = GrpcFeed::new();
    if cfg.momentum_spike_entry {
        feed.enable_spike(cfg.momentum_spike_bps, Duration::from_secs(cfg.momentum_spike_window_secs));
        info!(
            "gRPC spike→fast-entry ARMED: >{:.0}bps/{}s (shadow={})",
            cfg.momentum_spike_bps, cfg.momentum_spike_window_secs, cfg.momentum_spike_shadow
        );
    }
    let feed_task = feed.clone();
    let rpc_url = cfg.rpc_url.clone();
    let handle = tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match run_grpc_stream(&endpoint, token.as_deref(), &accounts, &acct_index, &wired, &feed_task, &rpc_url).await {
                Ok(()) => warn!("gRPC price stream closed — reconnecting in {}s", backoff.as_secs()),
                Err(e) => error!("gRPC price stream error: {e} — reconnecting in {}s", backoff.as_secs()),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
    Ok(Some((feed, handle)))
}

/// Apply one account write (from the stream or from seeding) to its wired pool:
/// parse by role, store the pool atomics, recompute the USD price, publish it.
/// `from_stream` is true only for live stream writes — the spike detector fires solely
/// on those, never on boot seeding (a first observation isn't a move) or the SOL-quote
/// recompute retry.
fn apply_update(w: &WiredPool, role: Role, data: &[u8], feed: &GrpcFeed, from_stream: bool) {
    match role {
        Role::VaultA | Role::VaultB => {
            let Some(amt) = dex::parse_spl_token_amount(data) else { return };
            if matches!(role, Role::VaultA) {
                w.pool.reserve_a.store(amt, std::sync::atomic::Ordering::Relaxed);
            } else {
                w.pool.reserve_b.store(amt, std::sync::atomic::Ordering::Relaxed);
            }
        }
        Role::State => {
            if let Some((price, fee_bps)) = dex::parse_cl_pool_state(data, &w.pool) {
                w.pool.sqrt_price_x64.store(price.to_bits(), std::sync::atomic::Ordering::Relaxed);
                if fee_bps > 0 {
                    w.pool.fee_bps.store(fee_bps, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }
    if let Some(usd) = w.price_usd(feed.sol_usd()) {
        feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));
        feed.note_update(&w.token_mint);

        // Spike → fast-entry detector (MOMENTUM_SPIKE_ENTRY). Fires only on live stream
        // writes: boot seeding and the SOL-quote recompute pass `from_stream=false` so a
        // first observation or a sol_usd refresh never looks like a move. A no-op unless
        // spike detection is armed (see GrpcFeed::note_spike). This supersedes the old
        // hardcoded 100bps/5s "price moved fast" log with the configurable, actionable
        // per-mint upward detector.
        if from_stream {
            feed.note_spike(&w.token_mint, usd);
        }

        // Task 5 (MOMENTUM_LOCAL_IMPACT pre-gate): keep the impact estimate fresh
        // whenever the price itself refreshes — cheap relative to the account write
        // that triggered this call, and it's what lets a quiet pool's estimate age
        // out (est_impact_bps) rather than linger from a stale trade.
        publish_impact(w, feed);
    }
}

/// Periodic reprice pass (driven by the `reprice_tick` interval in `run_grpc_stream`),
/// covering two moves the live account stream alone cannot make:
///
///   1. **First-time pricing** of a not-yet-mapped pool. A SOL-quoted pool has valid pool
///      state at boot but no USD price until the watcher publishes its first `sol_usd`
///      (`rate_to_usd` yields `None` while sol_usd is 0), so its first price is minted
///      here rather than waiting for the next trade to touch the account. A first price
///      earns trust / wakes a held-position exit (`note_update`).
///
///   2. **SOL-leg refresh** of an already-mapped SOL-quoted pool. Its USD price is
///      `price_in_sol × sol_usd`, but the pool account only writes on a trade — between
///      trades `sol_usd` still moves every tick, so without this the USD is frozen at the
///      last trade's SOL price. This is the JitoSOL "metric always the same" freeze: an
///      illiquid LST/SOL pool that rarely trades served a stale USD (99.44…) for many
///      ticks while SOL moved, flat-lining every momentum metric derived from it.
///      Recompute from current pool state × the latest `sol_usd`. A SOL-leg move is NOT a
///      new on-chain write, so this deliberately skips `note_update` (distrust must stay
///      intact — a moved SOL leg is no evidence the pool's own price re-agrees with REST)
///      and `note_spike` (a sol_usd refresh must never read as a token spike).
///
/// USDC-quoted pools have no external leg: once mapped, only a real account update (handled
/// on the stream by `apply_update`) can move their USD, so this skips them to avoid churn.
fn reprice_from_sol_leg(wired: &[WiredPool], feed: &GrpcFeed) {
    let sol_usd = feed.sol_usd();
    for w in wired {
        let mapped = feed.map.contains_key(&w.token_mint);
        if mapped && w.quote_is_usdc {
            continue;
        }
        // `price_usd` returns None while sol_usd is 0 or state is degenerate, so a reprice
        // never overwrites a good USD with a zero.
        if let Some(usd) = w.price_usd(sol_usd) {
            feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));
            if !mapped {
                feed.note_update(&w.token_mint);
            }
        }
    }
}

/// Estimate + publish the price impact (bps) of a `MOMENTUM_TRADE_USDC`-sized buy
/// (quote→momentum) from `w`'s live pool state, for the entry path's local pre-gate
/// (`MOMENTUM_LOCAL_IMPACT`). CP (raydium_amm_v4/saber/pump_swap) and Whirlpool only —
/// DLMM's `get_quote` is pure-linear (`price_impact` hardcoded 0.0 — no signal) and other
/// CL kinds aren't wired for reserves here. Any degenerate path (missing SOL/USD price,
/// zero output) publishes nothing, leaving the pre-gate with no fresh estimate to act
/// on (fails open, same as a missing price).
fn publish_impact(w: &WiredPool, feed: &GrpcFeed) {
    use dex::types::DexKind::*;
    let a_to_b = !w.momentum_is_token_a; // buy = quote -> momentum
    let amount_in: u64 = if w.quote_is_usdc {
        (w.trade_usdc * 1e6) as u64
    } else {
        let sol_usd = feed.sol_usd();
        if !(sol_usd.is_finite() && sol_usd > 0.01) {
            return;
        }
        ((w.trade_usdc / sol_usd) * 1e9) as u64
    };
    let q = match w.pool.dex {
        RaydiumAmmV4 => dex::raydium_amm::get_quote(&w.pool, amount_in, a_to_b),
        // same CP reserve math as raydium_amm_v4
        PumpSwap => dex::raydium_amm::get_quote(&w.pool, amount_in, a_to_b),
        Saber => dex::saber::get_quote(&w.pool, amount_in, a_to_b),
        OrcaWhirlpool => dex::orca::get_quote(&w.pool, amount_in, a_to_b),
        _ => return,
    };
    if q.amount_out == 0 {
        return;
    }
    let bps = (q.price_impact * 10_000.0) as u32;
    feed.publish_impact(&w.token_mint, bps);
}

/// Seed every subscribed account's current state via RPC so wired pools have a price
/// from t=0 (the gRPC stream only delivers *changes*; a quiet pool would otherwise
/// have no price until its first post-boot trade). Called at the top of every
/// `run_grpc_stream` cycle, so reconnect gaps are also re-seeded.
async fn seed_pool_state(
    rpc_url: &str,
    acct_index: &HashMap<String, (usize, Role)>,
    wired: &[WiredPool],
    feed: &GrpcFeed,
) {
    // SOL-quoted pools (e.g. JitoSOL/SOL) convert to USD via feed.sol_usd(), which is 0
    // until the watcher loop publishes its first SOL price — and that only happens AFTER
    // this seed runs. Without a SOL/USD here, every SOL-quoted pool yields None at seed and
    // is left to the 10s live-stream retry, so a rarely-traded staking pool can sit
    // REST(wired) indefinitely. Fetch it once when still unset so the seed prices SOL-quoted
    // pools symmetrically with USDC-quoted ones. On reconnects sol_usd is already warm
    // (watcher keeps publishing), so this is a one-time cost. Non-fatal on failure — the
    // retry path still covers it.
    if !(feed.sol_usd() > 0.0) {
        let http = reqwest::Client::new();
        // fetch_prices with no token mints returns just SOL/USD (Kraken) — the same source
        // and key ("SOL") the watcher loop publishes every tick.
        match crate::portfolio::pricer::fetch_prices(&http, &[], None).await {
            Ok(p) => match p.get("SOL").copied() {
                Some(px) if px > 0.0 => {
                    feed.publish_sol_usd(px);
                    info!("gRPC seed: pre-fetched SOL/USD ${px:.2} for SOL-quoted pools");
                }
                _ => warn!("gRPC seed: SOL/USD absent/non-positive — SOL-quoted pools wait for retry"),
            },
            Err(e) => warn!("gRPC seed: SOL/USD pre-fetch failed ({e}) — SOL-quoted pools wait for retry"),
        }
    }

    let keys: Vec<String> = acct_index.keys().cloned().collect();
    let rpc_url = rpc_url.to_string();
    let fetched = tokio::task::spawn_blocking(move || {
        let rpc = solana_client::rpc_client::RpcClient::new(rpc_url);
        let mut out: Vec<(String, Vec<u8>)> = Vec::new();
        for chunk in keys.chunks(100) {
            let pks: Vec<solana_sdk::pubkey::Pubkey> =
                chunk.iter().filter_map(|k| k.parse().ok()).collect();
            match rpc.get_multiple_accounts(&pks) {
                Ok(accts) => {
                    for (pk, acct) in pks.iter().zip(accts) {
                        if let Some(a) = acct {
                            out.push((pk.to_string(), a.data));
                        }
                    }
                }
                Err(e) => tracing::warn!("gRPC seed: getMultipleAccounts failed: {e}"),
            }
        }
        out
    })
    .await
    .unwrap_or_default();

    let mut seeded = 0usize;
    for (key, data) in &fetched {
        if let Some(&(idx, role)) = acct_index.get(key) {
            apply_update(&wired[idx], role, data, feed, false);
            seeded += 1;
        }
    }
    info!("gRPC seed: applied {seeded}/{} accounts, {} price(s) live", acct_index.len(), feed.map.len());
    // Name any wired pool that produced no seed price so the cause is visible instead of
    // silently manifesting as REST(wired) later. Usual reason: a CL state that didn't parse
    // to sqrt_price>0, or (before the pre-fetch above) an unset SOL/USD for a SOL-quoted pool.
    let unpriced: Vec<&str> = wired
        .iter()
        .filter(|w| !feed.map.contains_key(&w.token_mint))
        .map(|w| w.token_mint.as_str())
        .collect();
    if !unpriced.is_empty() {
        warn!(
            "gRPC seed: {} wired pool(s) unpriced after seed (will retry on live writes): {}",
            unpriced.len(),
            unpriced.join(",")
        );
    }
}

/// One connect+subscribe+receive cycle; returns on stream end/error (caller reconnects).
/// Mirrors the connection pattern in `src/streamer/client.rs` but is self-contained (no arb
/// Config / PoolRegistry). Retains pool atomic state across reconnects via `Arc<Pool>` fields.
async fn run_grpc_stream(
    endpoint: &str,
    token: Option<&str>,
    accounts: &[String],
    acct_index: &HashMap<String, (usize, Role)>,
    wired: &[WiredPool],
    feed: &GrpcFeed,
    rpc_url: &str,
) -> Result<()> {
    seed_pool_state(rpc_url, acct_index, wired, feed).await;

    use tonic::transport::{Channel, ClientTlsConfig};
    let channel = Channel::from_shared(endpoint.to_string())
        .context("invalid GRPC_ENDPOINT")?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .context("TLS config")?
        .connect()
        .await
        .context("gRPC connect")?;
    let mut client = GeyserClient::new(channel).max_decoding_message_size(64 * 1024 * 1024);

    let filter = SubscribeRequestFilterAccounts {
        account: accounts.to_vec(),
        owner: vec![],
        filters: vec![],
        ..Default::default()
    };
    let mut accounts_map = HashMap::new();
    accounts_map.insert("momentum_pools".to_string(), filter);
    let sub = SubscribeRequest {
        accounts: accounts_map,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(sub).await.ok();
    let mut request = tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    if let Some(t) = token {
        let val = t
            .parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>()
            .context("invalid GRPC_TOKEN")?;
        request.metadata_mut().insert("x-token", val);
    }
    let mut inbound = client.subscribe(request).await.context("gRPC subscribe")?.into_inner();
    let _keep_tx_alive = tx;
    info!("gRPC price stream connected");

    // Periodic reprice (see `reprice_from_sol_leg`): first-time pricing of pools that had
    // no USD at boot, PLUS the SOL-leg refresh that keeps SOL-quoted pools tracking SOL
    // between trades. 10s keeps the USD current well inside the watcher's ~60s snapshot
    // cadence without busy-repricing on every message.
    let mut reprice_tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            msg = inbound.next() => {
                let Some(msg) = msg else { break };
                let update = msg.context("stream item")?;
                let Some(UpdateOneof::Account(acc)) = update.update_oneof else { continue };
                let Some(info) = acc.account else { continue };
                let Ok(pk) = solana_sdk::pubkey::Pubkey::try_from(info.pubkey.as_slice()) else { continue };
                let Some(&(idx, role)) = acct_index.get(&pk.to_string()) else { continue };
                apply_update(&wired[idx], role, &info.data, feed, true);
            }
            _ = reprice_tick.tick() => reprice_from_sol_leg(wired, feed),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::pubkey::Pubkey;
    use std::sync::atomic::{AtomicI32, AtomicU64};

    /// Minimal constant-product (Raydium AMM v4) pool with the given reserves; everything
    /// else is a zero/default. `snapshot_state` reads `reserve_a`/`reserve_b`, which is all
    /// `WiredPool::price_usd` needs on the CP path.
    fn cp_pool(reserve_a: u64, reserve_b: u64) -> std::sync::Arc<dex::types::Pool> {
        std::sync::Arc::new(dex::types::Pool {
            id: Pubkey::default(),
            dex: dex::types::DexKind::RaydiumAmmV4,
            token_a: Pubkey::default(),
            token_b: Pubkey::default(),
            vault_a: Pubkey::default(),
            vault_b: Pubkey::default(),
            reserve_a: AtomicU64::new(reserve_a),
            reserve_b: AtomicU64::new(reserve_b),
            fee_bps: AtomicU64::new(0),
            sqrt_price_x64: AtomicU64::new(0),
            active_bin_id: AtomicI32::new(0),
            tick_current_index: AtomicI32::new(0),
            state_account: None,
            stable: false,
            damm_virtual_price: AtomicU64::new(0),
            a_lp_balance: AtomicU64::new(0),
            b_lp_balance: AtomicU64::new(0),
            last_update_ns: AtomicU64::new(0),
            extra: dex::types::PoolExtra::default(),
            clmm_tick_array_bitmap: std::array::from_fn(|_| AtomicU64::new(0)),
            clmm_observation_key: std::array::from_fn(|_| AtomicU64::new(0)),
            dlmm_token_a_is_x: AtomicU64::new(0),
        })
    }

    fn wired(mint: &str, quote_is_usdc: bool) -> Vec<WiredPool> {
        vec![WiredPool {
            pool: cp_pool(100, 200),
            token_mint: mint.to_string(),
            momentum_is_token_a: true,
            dec_momentum: 9,
            dec_quote: 9,
            quote_is_usdc,
            trade_usdc: 0.0,
        }]
    }

    fn priced(feed: &GrpcFeed, mint: &str) -> f64 {
        feed.map.get(mint).map(|e| e.value().0).expect("mint priced")
    }

    // The regression this fix targets: a SOL-quoted pool that is ALREADY mapped must refresh
    // its USD when sol_usd moves, even though its pool account has not updated. The old loop
    // did `if map.contains_key { continue }`, freezing the USD at the last trade's SOL price
    // (the JitoSOL freeze). USD = price_in_sol × sol_usd, so doubling SOL must double USD.
    #[test]
    fn sol_quoted_refreshes_when_sol_usd_moves_without_pool_update() {
        let feed = GrpcFeed::new();
        let wired = wired("JITO", false);

        feed.publish_sol_usd(100.0);
        reprice_from_sol_leg(&wired, &feed);
        let p1 = priced(&feed, "JITO");

        // SOL doubles; the pool account is untouched (no apply_update).
        feed.publish_sol_usd(200.0);
        reprice_from_sol_leg(&wired, &feed);
        let p2 = priced(&feed, "JITO");

        assert!(
            (p2 / p1 - 2.0).abs() < 1e-6,
            "SOL-quoted USD must double when SOL doubles: {p1} -> {p2}"
        );
    }

    // A USDC-quoted pool has no SOL leg: once mapped, a sol_usd change must not touch its USD.
    #[test]
    fn usdc_quoted_untouched_by_sol_leg_reprice() {
        let feed = GrpcFeed::new();
        let wired = wired("USDCQ", true);

        feed.publish_sol_usd(100.0);
        reprice_from_sol_leg(&wired, &feed);
        let p1 = priced(&feed, "USDCQ");

        feed.publish_sol_usd(500.0);
        reprice_from_sol_leg(&wired, &feed);
        let p2 = priced(&feed, "USDCQ");

        assert_eq!(p1, p2, "USDC-quoted USD must be independent of SOL");
    }

    // A SOL-leg refresh is not a fresh on-chain write, so it must NOT clear a standing
    // distrust set by the REST cross-check — only a real account update earns trust back.
    #[test]
    fn sol_leg_reprice_preserves_distrust() {
        let feed = GrpcFeed::new();
        let wired = wired("JITO", false);

        feed.publish_sol_usd(100.0);
        reprice_from_sol_leg(&wired, &feed); // first pricing → mapped
        feed.record_xcheck("JITO", false, Instant::now()); // REST diverged → distrust

        feed.publish_sol_usd(200.0);
        reprice_from_sol_leg(&wired, &feed); // SOL-leg refresh

        assert!(
            feed.distrusted_snapshot().contains("JITO"),
            "SOL-leg reprice must not clear distrust"
        );
    }

    fn pc(id: &str, token_a: &str) -> crate::dex::types::PoolConfig {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "dex": "pump_swap",
            "token_a": token_a,
            "token_b": "So11111111111111111111111111111111111111112",
            "vault_a": "va", "vault_b": "vb",
            "fee_bps": 25
        }))
        .expect("minimal PoolConfig")
    }

    #[test]
    fn merge_pool_configs_curated_wins_on_collision() {
        let curated = vec![pc("P1", "curatedMint")];
        let extra = vec![pc("P1", "scanMint"), pc("P2", "scanOnly")];
        let merged = merge_pool_configs(curated, extra);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged["P1"].token_a, "curatedMint", "pools.json entry must win");
        assert_eq!(merged["P2"].token_a, "scanOnly", "extra-only pool survives");
    }
}
