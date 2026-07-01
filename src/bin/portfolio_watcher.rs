use anyhow::Result;
use solana_mev::portfolio::{self, scanner, PortfolioConfig};
use tracing::{error, info, warn};

// gRPC price feed (Option B): compile the shared arb `dex`/`graph` source modules into this
// binary so the momentum pricer can reuse the real pool parsers + PoolState. They are a closed
// pair (reference only each other + external crates), so this pulls in nothing else.
#[path = "../dex/mod.rs"]
mod dex;
#[path = "../graph/mod.rs"]
mod graph;

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

    portfolio::watcher::run(cfg, http, None).await;

    Ok(())
}
