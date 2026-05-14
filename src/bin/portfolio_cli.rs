use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use solana_client::rpc_client::RpcClient;
use solana_mev::portfolio::{self, scanner, PortfolioConfig};
use std::collections::HashMap;

#[derive(Parser)]
#[command(name = "portfolio-cli", about = "Manage your Solana portfolio")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan wallet and create a fresh portfolio.json
    Init,
    /// Re-scan wallet and merge updates into existing portfolio.json
    Update,
    /// Print current holdings with live prices
    Show,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let cfg = PortfolioConfig::from_env()?;

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;

    match cli.command {
        Command::Init => {
            // Full scan, overwrite any existing file
            let pubkey = scanner::load_pubkey(&cfg.wallet_keypair_path)?;
            let rpc = RpcClient::new(cfg.rpc_url.clone());
            let scanned = scanner::scan_wallet(&rpc, &pubkey, &http).await?;
            portfolio::save_portfolio(&cfg.portfolio_path, &scanned)?;
            println!("Created {}", cfg.portfolio_path);
            print_portfolio(&scanned, &HashMap::new());
        }
        Command::Update => {
            // Scan and merge into existing file (same as watcher startup)
            let p = scanner::scan_and_save(&cfg, &http).await?;
            println!("Updated {}", cfg.portfolio_path);
            print_portfolio(&p, &HashMap::new());
        }
        Command::Show => {
            let p = portfolio::load_portfolio(&cfg.portfolio_path)
                .context("portfolio.json not found — run `portfolio-cli init` first")?;
            let mints: Vec<String> = p.tokens.iter().map(|t| t.mint.clone()).collect();
            let prices = portfolio::pricer::fetch_prices(&http, &mints, cfg.birdeye_api_key.as_deref())
                .await
                .unwrap_or_default();
            print_portfolio(&p, &prices);
        }
    }

    Ok(())
}

fn print_portfolio(p: &portfolio::Portfolio, prices: &HashMap<String, f64>) {
    let sol_price = prices.get("SOL").copied().unwrap_or(0.0);
    println!(
        "  SOL   {:.4} × ${:.2} = ${:.2}",
        p.sol_amount,
        sol_price,
        sol_price * p.sol_amount
    );
    let mut total = sol_price * p.sol_amount;
    for t in &p.tokens {
        let price = prices
            .get(&t.mint)
            .or_else(|| prices.get(&t.symbol))
            .copied()
            .unwrap_or(0.0);
        let value = price * t.amount;
        total += value;
        println!(
            "  {:<8} {:.4} × ${:.4} = ${:.2}",
            t.symbol, t.amount, price, value
        );
    }
    if !prices.is_empty() {
        println!("  ──────────────────────────────────");
        println!("  Total: ${:.2}", total);
    }
}
