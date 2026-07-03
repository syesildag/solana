use anyhow::{Context, Result};
use solana_mev::portfolio::{self, scanner, PortfolioConfig};
use solana_mev::portfolio::grpc_pricer::{self, GrpcFeed};
use solana_mev::portfolio::momentum_universe::{self, WatchedToken};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use futures::StreamExt;
use tracing::{error, info, warn};
use yellowstone_grpc_proto::geyser::{
    geyser_client::GeyserClient, subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest,
    SubscribeRequestFilterAccounts,
};

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
// Raydium AMM v4 SOL/USDC pool — used by the gRPC smoke test (GRPC_PRICE_SMOKE=1).
const SMOKE_SOL_USDC_POOL: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";

// gRPC price feed (Option B): compile the shared arb `dex`/`graph` source modules into this
// binary so the momentum pricer can reuse the real pool parsers + PoolState. They are a closed
// pair (reference only each other + external crates), so this pulls in nothing else.
// `allow(dead_code)`: this binary uses only a slice of dex/graph (PoolState, parsers, Pool);
// the rest (swap builders, get_quote, PDAs, …) is used by the arb binary, so it's not dead in
// the project — only unused in this binary's view. Suppress the noise without touching dex source.
#[path = "../dex/mod.rs"]
#[allow(dead_code)]
mod dex;
#[path = "../graph/mod.rs"]
#[allow(dead_code)]
mod graph;

// Bridge the arb `dex::PoolState` (binary-local) to the lib's `PoolRates` trait, so the
// gRPC ingestion task (to be built) can feed real pool state into `grpc_pricer::price_usd`.
// Orphan rule is satisfied: `dex::types::PoolState` is local to this binary crate.
// `self.rate_*` resolve to PoolState's inherent methods (inherent wins over trait), so
// these delegate rather than recurse.
impl solana_mev::portfolio::grpc_pricer::PoolRates for dex::types::PoolState {
    fn rate_a_to_b(&self) -> f64 {
        self.rate_a_to_b()
    }
    fn rate_b_to_a(&self) -> f64 {
        self.rate_b_to_a()
    }
}

/// Account role within a pool subscription — determines which atomic field to update.
#[derive(Clone, Copy)]
enum Role {
    VaultA,
    VaultB,
    State,
}

/// One momentum pool tracked live from gRPC account updates, backed by a real `dex::Pool`.
/// Supports CP pools (RaydiumAmmV4/Saber) via vault reserves and CL pools
/// (OrcaWhirlpool/RaydiumClmm/MeteoraDlmm/Invariant) via state account sqrt_price_x64.
struct WiredPool {
    pool: std::sync::Arc<dex::types::Pool>,
    token_mint: String,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
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

/// Build the gRPC price feed for momentum tokens configured with `pool`+`quote` in
/// momentum_tokens.json. CP (raydium_amm_v4/saber) pools are priced from vault reserves;
/// CL pools (Orca Whirlpool, Raydium CLMM, Meteora DLMM, Invariant) from their state
/// account. Other DEX kinds fall back to REST. Returns None when the feature is off or
/// no eligible pool is configured. The pool's structure (vaults, fee) is resolved from
/// pools.json.
async fn spawn_grpc_feed(cfg: &PortfolioConfig, watched: &[WatchedToken]) -> Result<Option<GrpcFeed>> {
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
    let by_id: HashMap<&str, &dex::types::PoolConfig> =
        configs.iter().map(|c| (c.id.as_str(), c)).collect();

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
        warn!("gRPC: no eligible pools configured (raydium_amm_v4/saber/Orca/CLMM/DLMM/Invariant) — REST only");
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
            dex::types::DexKind::RaydiumAmmV4 | dex::types::DexKind::Saber => {
                vec![(pool.vault_a.to_string(), Role::VaultA), (pool.vault_b.to_string(), Role::VaultB)]
            }
            dex::types::DexKind::OrcaWhirlpool
            | dex::types::DexKind::RaydiumClmm
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
        wired.push(WiredPool { pool, token_mint: p.tok.mint.clone(), momentum_is_token_a, dec_momentum, dec_quote, quote_is_usdc: p.quote_is_usdc });
    }
    if wired.is_empty() { warn!("gRPC: no eligible pools — REST only"); return Ok(None); }
    let accounts: Vec<String> = acct_index.keys().cloned().collect();
    info!("gRPC price feed: subscribing {} accounts for {} pool(s)", accounts.len(), wired.len());

    let feed = GrpcFeed::new();
    let feed_task = feed.clone();
    let rpc_url = cfg.rpc_url.clone();
    tokio::spawn(async move {
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
    Ok(Some(feed))
}

/// Apply one account write (from the stream or from seeding) to its wired pool:
/// parse by role, store the pool atomics, recompute the USD price, publish it.
fn apply_update(w: &WiredPool, role: Role, data: &[u8], feed: &GrpcFeed) {
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
        // Cross-venue observability (Task 3): this mint may now be priced from more than
        // one pool (multiple PoolRefs). If another venue wrote a price for the same mint
        // very recently and it disagrees materially, log it — purely informational, the
        // shared map is still last-write-wins below. Hardcoded 5s/100bps: an operator
        // signal, not the REST divergence cross-check's configurable budget.
        let disagreement = feed.map.get(&w.token_mint).and_then(|entry| {
            let (prev, ts) = *entry.value();
            (ts.elapsed() < Duration::from_secs(5) && prev > 0.0).then_some(prev)
        });
        if let Some(prev) = disagreement {
            let diff_bps = ((usd - prev).abs() / prev) * 10_000.0;
            if diff_bps > 100.0 {
                info!(
                    "gRPC: venues disagree for {}: new=${usd:.6} prev=${prev:.6}",
                    w.token_mint
                );
            }
        }
        feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));
        feed.note_update(&w.token_mint);
    }
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
            apply_update(&wired[idx], role, data, feed);
            seeded += 1;
        }
    }
    info!("gRPC seed: applied {seeded}/{} accounts, {} price(s) live", acct_index.len(), feed.map.len());
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

    // SOL-quoted pools have state but no price until the watcher publishes sol_usd (see
    // `GrpcFeed::sol_usd`); re-attempt those on a slow tick rather than waiting for the
    // next trade to touch the account.
    let mut unpriced_retry = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            msg = inbound.next() => {
                let Some(msg) = msg else { break };
                let update = msg.context("stream item")?;
                let Some(UpdateOneof::Account(acc)) = update.update_oneof else { continue };
                let Some(info) = acc.account else { continue };
                let Ok(pk) = solana_sdk::pubkey::Pubkey::try_from(info.pubkey.as_slice()) else { continue };
                let Some(&(idx, role)) = acct_index.get(&pk.to_string()) else { continue };
                apply_update(&wired[idx], role, &info.data, feed);
            }
            _ = unpriced_retry.tick() => {
                for w in wired {
                    if feed.map.contains_key(&w.token_mint) { continue }
                    if let Some(usd) = w.price_usd(feed.sol_usd()) {
                        feed.map.insert(w.token_mint.clone(), (usd, Instant::now()));
                        feed.note_update(&w.token_mint);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Standalone smoke test (set GRPC_PRICE_SMOKE=1): stream whatever momentum tokens are wired
/// with a `pool`+`quote` in momentum_tokens.json and print their on-chain prices for ~25s,
/// then exit. If nothing is wired, falls back to the Raydium SOL/USDC pool so the pipeline is
/// always exercised. Verifies subscription + parsing + price math against the live endpoint
/// WITHOUT running the trader.
async fn run_grpc_smoke(cfg: &PortfolioConfig) -> Result<()> {
    let mut cfg = cfg.clone();
    cfg.momentum_grpc_pricing = true;
    let mut watched = momentum_universe::load(std::path::Path::new(&cfg.momentum_tokens_path))
        .unwrap_or_default();
    if !watched.iter().any(|w| !w.pool_refs().is_empty()) {
        let smoke_pool = std::env::var("GRPC_SMOKE_POOL").unwrap_or_else(|_| SMOKE_SOL_USDC_POOL.to_string());
        let smoke_quote = std::env::var("GRPC_SMOKE_QUOTE").unwrap_or_else(|_| "USDC".to_string());
        info!("gRPC smoke: no pool+quote wired in {} — falling back to pool {smoke_pool}", cfg.momentum_tokens_path);
        watched = vec![WatchedToken {
            symbol: "SOL".into(),
            mint: SOL_MINT.into(),
            name: None,
            equity: Some(false),
            params: None,
            pool: Some(smoke_pool),
            quote: Some(smoke_quote),
            pools: None,
        }];
    }
    let sym: HashMap<String, String> =
        watched.iter().map(|w| (w.mint.clone(), w.symbol.clone())).collect();
    let Some(feed) = spawn_grpc_feed(&cfg, &watched).await? else {
        warn!("gRPC smoke: feed not started (no eligible raydium_amm_v4/saber/Orca/CLMM/DLMM/Invariant pool / check GRPC_ENDPOINT)");
        return Ok(());
    };
    info!("gRPC smoke: waiting ~25s for on-chain prices…");
    for _ in 0..25 {
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    let mut any = false;
    for e in feed.map.iter() {
        any = true;
        let s = sym.get(e.key()).cloned().unwrap_or_else(|| e.key().clone());
        info!("gRPC smoke: {s} = ${:.4}  (updated {:.1}s ago)", e.value().0, e.value().1.elapsed().as_secs_f64());
    }
    if any {
        info!("gRPC smoke: PASS");
    } else {
        warn!("gRPC smoke: FAIL — no on-chain prices received");
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Try .env next to the binary first, then fall back to cwd.
    // This makes the binary work regardless of the working directory it is launched from.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            dotenvy::from_path(dir.join(".env")).ok();
        }
    }
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = PortfolioConfig::from_env()?;

    // Standalone gRPC price-feed smoke test (GRPC_PRICE_SMOKE=1): verify the subscription +
    // parsing against the live endpoint without starting the trader, then exit.
    if std::env::var("GRPC_PRICE_SMOKE").is_ok() {
        return run_grpc_smoke(&cfg).await;
    }

    // Validate SMTP addresses early so misconfiguration surfaces at startup,
    // not silently when the first alert fires.
    if cfg.smtp_from.parse::<lettre::message::Mailbox>().is_err() {
        warn!("SMTP_FROM {:?} is not a valid email address — alert emails will fail", cfg.smtp_from);
    }
    if cfg.alert_email.parse::<lettre::message::Mailbox>().is_err() {
        warn!("ALERT_EMAIL {:?} is not a valid email address — alert emails will fail", cfg.alert_email);
    }

    info!(
        "Portfolio watcher starting — scanning wallet to refresh {}",
        cfg.portfolio_path
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    // Scan wallet on every startup: creates portfolio.json if absent, merges if present.
    match scanner::scan_and_save(&cfg, &http).await {
        Ok(p) => info!(
            "Wallet scan complete — {:.4} SOL, {} token(s)",
            p.sol_amount,
            p.tokens.len()
        ),
        Err(e) => error!("Wallet scan failed, proceeding with existing portfolio.json: {e}"),
    }

    // Spawn the gRPC price feed (opt-in; None when off or no eligible pool) and hand it to
    // the watcher, which prefers fresh on-chain prices and REST-fills the rest.
    let watched =
        momentum_universe::load(std::path::Path::new(&cfg.momentum_tokens_path)).unwrap_or_default();
    let grpc_feed = match spawn_grpc_feed(&cfg, &watched).await {
        Ok(f) => f,
        Err(e) => {
            warn!("gRPC price feed setup failed: {e} — REST only");
            None
        }
    };
    portfolio::watcher::run(cfg, http, grpc_feed).await;

    Ok(())
}
