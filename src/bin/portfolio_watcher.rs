use anyhow::Result;
use solana_mev::portfolio::{self, scanner, PortfolioConfig};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cfg = PortfolioConfig::from_env()?;

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

    portfolio::watcher::run(cfg, http).await;

    Ok(())
}
