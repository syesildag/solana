use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use solana_client::rpc_client::RpcClient;
use solana_mev::portfolio::{self, analyzer, history, scanner, PortfolioConfig};
use solana_mev::portfolio::analyzer::{AnalysisConfig, RiskReport};
use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

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

            // Load price history so drawdown and EWMA have data to work with
            let mut hist = history::load_history(Path::new(&cfg.history_path))
                .unwrap_or_default();

            let mints: Vec<String> = p.tokens.iter().map(|t| t.mint.clone()).collect();
            let prices = portfolio::pricer::fetch_prices(
                &http, &mints, cfg.birdeye_api_key.as_deref(),
            )
            .await
            .unwrap_or_default();

            // Append live snapshot so risk metrics reflect current prices
            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            hist.push_back(portfolio::history::PriceSnapshot { ts, prices: prices.clone() });

            let eur_rate = portfolio::pricer::fetch_eur_rate(&http).await.unwrap_or(0.92);

            let analysis_cfg = AnalysisConfig {
                alert_pct_5m: cfg.alert_pct_5m,
                alert_pct_1h: cfg.alert_pct_1h,
                zscore_lambda: cfg.zscore_lambda,
                zscore_threshold: cfg.zscore_threshold,
                zscore_min_obs: cfg.zscore_min_obs,
            };
            let risk = analyzer::compute_risk(&hist, &p, eur_rate, &analysis_cfg);

            print_portfolio(&p, &prices);
            print_risk_table(&risk, cfg.zscore_lambda, cfg.zscore_min_obs);
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

fn print_risk_table(report: &RiskReport, lambda: f64, min_obs: usize) {
    println!();
    println!("  Risk Metrics (EWMA lambda={lambda:.2})");
    println!("  {}", "─".repeat(60));
    println!("  {:<8}  {:<8}  {:<9}  {:<10}  {}", "Symbol", "Z-score", "sigma_ann", "DrawDown", "DD (EUR)");
    println!("  {}", "─".repeat(60));
    for a in &report.assets {
        if a.is_warm {
            let z_str = a.z_score.map_or("--".to_string(), |z| format!("{:+.2}", z));
            let vol_str = a.sigma_ann.map_or("--".to_string(), |v| format!("{:.1}%", v));
            println!(
                "  {:<8}  {:<8}  {:<9}  {:<10}  -{:.2}",
                a.symbol,
                z_str,
                vol_str,
                format!("{:.1}%", a.current_drawdown_pct),
                a.drawdown_eur,
            );
        } else {
            println!(
                "  {:<8}  (warming {}/{})",
                a.symbol, a.n_obs, min_obs
            );
        }
    }
    println!("  {}", "─".repeat(60));
    println!("  Portfolio drawdown from peak: EUR -{:.2}", report.total_drawdown_eur);
}
