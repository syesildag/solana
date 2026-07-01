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
#[path = "../dex/mod.rs"]
mod dex;
#[path = "../graph/mod.rs"]
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

/// One momentum pool tracked live from gRPC account updates (constant-product only for now:
/// price = reserve ratio × fee, decimal- and USD-adjusted via `grpc_pricer::price_usd`).
struct TrackedCp {
    token_mint: String,
    fee_bps: u64,
    momentum_is_token_a: bool,
    dec_momentum: u8,
    dec_quote: u8,
    quote_is_usdc: bool,
    reserve_a: Option<u64>,
    reserve_b: Option<u64>,
}

impl TrackedCp {
    fn price(&self, sol_usd: f64) -> Option<f64> {
        let state = dex::types::PoolState::ConstantProduct {
            reserve_a: self.reserve_a?,
            reserve_b: self.reserve_b?,
            fee_bps: self.fee_bps,
        };
        grpc_pricer::price_usd(
            &state,
            self.momentum_is_token_a,
            self.dec_momentum,
            self.dec_quote,
            self.quote_is_usdc,
            sol_usd,
        )
    }
}

/// Build the gRPC price feed for momentum tokens configured with `pool`+`quote` in
/// momentum_tokens.json (constant-product / raydium_amm_v4 only for now; every other DEX
/// kind logs and falls back to REST). Returns None when the feature is off or no eligible
/// pool is configured. The pool's structure (vaults, fee) is resolved from pools.json.
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
        let (Some(pool_id), Some(quote)) = (w.pool.as_deref(), w.quote.as_deref()) else {
            continue;
        };
        let Some(pc) = by_id.get(pool_id).copied() else {
            warn!("gRPC: pool {pool_id} for {} not in pools.json — REST", w.symbol);
            continue;
        };
        if !matches!(pc.dex, dex::types::DexKind::RaydiumAmmV4) {
            warn!(
                "gRPC: pool {pool_id} for {} is {:?} (only raydium_amm_v4 supported so far) — REST",
                w.symbol, pc.dex
            );
            continue;
        }
        decimal_mints.push(pc.token_a.clone());
        decimal_mints.push(pc.token_b.clone());
        pending.push(Pending { tok: w, pc, quote_is_usdc: quote.eq_ignore_ascii_case("USDC") });
    }
    if pending.is_empty() {
        warn!("gRPC: no eligible constant-product pools configured — REST only");
        return Ok(None);
    }

    let decimals = scanner::fetch_decimals_for_mints(&cfg.rpc_url, decimal_mints)
        .await
        .unwrap_or_default();

    let mut tracked: Vec<TrackedCp> = Vec::new();
    let mut acct_index: HashMap<String, (usize, bool)> = HashMap::new();
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
        let idx = tracked.len();
        acct_index.insert(p.pc.vault_a.clone(), (idx, true));
        acct_index.insert(p.pc.vault_b.clone(), (idx, false));
        tracked.push(TrackedCp {
            token_mint: p.tok.mint.clone(),
            fee_bps: p.pc.fee_bps,
            momentum_is_token_a,
            dec_momentum,
            dec_quote,
            quote_is_usdc: p.quote_is_usdc,
            reserve_a: None,
            reserve_b: None,
        });
    }
    if tracked.is_empty() {
        return Ok(None);
    }

    let accounts: Vec<String> = acct_index.keys().cloned().collect();
    info!("gRPC price feed: subscribing {} vault accounts for {} pool(s)", accounts.len(), tracked.len());

    let feed = GrpcFeed::new();
    let feed_task = feed.clone();
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match run_grpc_stream(&endpoint, token.as_deref(), &accounts, &acct_index, &mut tracked, &feed_task).await {
                Ok(()) => warn!("gRPC price stream closed — reconnecting in {}s", backoff.as_secs()),
                Err(e) => error!("gRPC price stream error: {e} — reconnecting in {}s", backoff.as_secs()),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
    Ok(Some(feed))
}

/// One connect+subscribe+receive cycle; returns on stream end/error (caller reconnects).
/// Mirrors the connection pattern in `src/streamer/client.rs` but is self-contained (no arb
/// Config / PoolRegistry). Retains `tracked` reserves across reconnects.
async fn run_grpc_stream(
    endpoint: &str,
    token: Option<&str>,
    accounts: &[String],
    acct_index: &HashMap<String, (usize, bool)>,
    tracked: &mut [TrackedCp],
    feed: &GrpcFeed,
) -> Result<()> {
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

    while let Some(msg) = inbound.next().await {
        let update = msg.context("stream item")?;
        let Some(UpdateOneof::Account(acc)) = update.update_oneof else { continue };
        let Some(info) = acc.account else { continue };
        let Ok(pk) = solana_sdk::pubkey::Pubkey::try_from(info.pubkey.as_slice()) else { continue };
        let Some(&(idx, is_a)) = acct_index.get(&pk.to_string()) else { continue };
        let Some(amt) = dex::parse_spl_token_amount(&info.data) else { continue };
        let t = &mut tracked[idx];
        if is_a {
            t.reserve_a = Some(amt);
        } else {
            t.reserve_b = Some(amt);
        }
        if let Some(usd) = t.price(feed.sol_usd()) {
            feed.map.insert(t.token_mint.clone(), (usd, Instant::now()));
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
    if !watched.iter().any(|w| w.pool.is_some() && w.quote.is_some()) {
        info!("gRPC smoke: no pool+quote wired in {} — falling back to the SOL/USDC test pool", cfg.momentum_tokens_path);
        watched = vec![WatchedToken {
            symbol: "SOL".into(),
            mint: SOL_MINT.into(),
            name: None,
            equity: Some(false),
            params: None,
            pool: Some(SMOKE_SOL_USDC_POOL.into()),
            quote: Some("USDC".into()),
        }];
    }
    let sym: HashMap<String, String> =
        watched.iter().map(|w| (w.mint.clone(), w.symbol.clone())).collect();
    let Some(feed) = spawn_grpc_feed(&cfg, &watched).await? else {
        warn!("gRPC smoke: feed not started (no eligible raydium_amm_v4 pool / check GRPC_ENDPOINT)");
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
