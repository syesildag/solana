use anyhow::Result;
use solana_mev::portfolio::feed_setup::spawn_grpc_feed;
use solana_mev::portfolio::{self, scanner, PortfolioConfig};
use solana_mev::portfolio::momentum_universe::{self, WatchedToken};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{error, info, warn};

const SOL_MINT: &str = "So11111111111111111111111111111111111111112";
// Raydium AMM v4 SOL/USDC pool — used by the gRPC smoke test (GRPC_PRICE_SMOKE=1).
const SMOKE_SOL_USDC_POOL: &str = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";

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
    let Some((feed, _task)) = spawn_grpc_feed(&cfg, &watched, &[]).await? else {
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
    let grpc_feed = match spawn_grpc_feed(&cfg, &watched, &[]).await {
        Ok(feed) => feed, // Option<(GrpcFeed, JoinHandle<()>)> — run() takes it whole.
        Err(e) => { warn!("gRPC feed setup failed ({e}) — REST only"); None }
    };
    portfolio::watcher::run(cfg, http, grpc_feed).await;

    Ok(())
}
