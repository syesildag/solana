use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use plotters::prelude::*;
use solana_client::rpc_client::RpcClient;
use solana_mev::portfolio::{self, analyzer, history, scanner, PortfolioConfig};
use solana_mev::portfolio::analyzer::{AnalysisConfig, RiskReport};
use solana_mev::portfolio::history::PriceSnapshot;
use solana_mev::portfolio::Portfolio;
use std::collections::{HashMap, VecDeque};
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
    /// Print current holdings with live prices and risk metrics
    Show,
    /// Generate SVG price charts for every asset and portfolio total
    Plot,
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
            let pubkey = scanner::load_pubkey(&cfg.wallet_keypair_path)?;
            let rpc = RpcClient::new(cfg.rpc_url.clone());
            let scanned = scanner::scan_wallet(&rpc, &pubkey, &http).await?;
            portfolio::save_portfolio(&cfg.portfolio_path, &scanned)?;
            println!("Created {}", cfg.portfolio_path);
            print_portfolio(&scanned, &HashMap::new(), 1.0);
        }
        Command::Update => {
            let p = scanner::scan_and_save(&cfg, &http).await?;
            println!("Updated {}", cfg.portfolio_path);
            print_portfolio(&p, &HashMap::new(), 1.0);
        }
        Command::Show => {
            let p = portfolio::load_portfolio(&cfg.portfolio_path)
                .context("portfolio.json not found — run `portfolio-cli init` first")?;

            let mut hist = history::load_history(Path::new(&cfg.history_path))
                .unwrap_or_default();

            let mints: Vec<String> = p.tokens.iter().map(|t| t.mint.clone()).collect();
            let prices = portfolio::pricer::fetch_prices(
                &http, &mints, cfg.birdeye_api_key.as_deref(),
            )
            .await
            .unwrap_or_default();

            let ts = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            hist.push_back(PriceSnapshot { ts, prices: prices.clone() });

            let eur_rate = portfolio::pricer::fetch_eur_rate(&http).await.unwrap_or(0.92);

            let analysis_cfg = AnalysisConfig {
                alert_pct_5m: cfg.alert_pct_5m,
                alert_pct_1h: cfg.alert_pct_1h,
                zscore_lambda: cfg.zscore_lambda,
                zscore_threshold: cfg.zscore_threshold,
                zscore_min_obs: cfg.zscore_min_obs,
            };
            let risk = analyzer::compute_risk(&hist, &p, eur_rate, &analysis_cfg);

            print_portfolio(&p, &prices, eur_rate);
            print_risk_table(&risk, cfg.zscore_lambda, cfg.zscore_min_obs);
        }
        Command::Plot => {
            let p = portfolio::load_portfolio(&cfg.portfolio_path)
                .context("portfolio.json not found — run `portfolio-cli init` first")?;

            // Prefer 30-day hourly Birdeye data for a meaningful chart span.
            // Fall back to the local 7-day 1-minute history when no API key is set.
            let hist = if let Some(api_key) = &cfg.birdeye_api_key {
                println!("Fetching 30-day hourly history from Birdeye…");
                build_monthly_history(&http, api_key, &p).await
            } else {
                println!("No BIRDEYE_API_KEY — plotting from local history (set key for 30-day charts).");
                history::load_history(Path::new(&cfg.history_path)).unwrap_or_default()
            };

            if hist.len() < 2 {
                println!("Not enough history to plot (need at least 2 snapshots).");
                return Ok(());
            }

            let eur_rate = portfolio::pricer::fetch_eur_rate(&http).await.unwrap_or(0.92);

            let out_dir = Path::new("assets/charts");
            std::fs::create_dir_all(out_dir)?;

            // Per-asset charts
            let sol_series = price_series_eur("SOL", "SOL", &hist, eur_rate);
            if sol_series.len() >= 2 {
                let path = out_dir.join("SOL.svg");
                render_chart("SOL", "€", &sol_series, &path)?;
                println!("  {}", path.display());
            }

            for token in &p.tokens {
                let series = price_series_eur(&token.mint, &token.symbol, &hist, eur_rate);
                if series.len() < 2 { continue; }
                let path = out_dir.join(format!("{}.svg", token.symbol));
                render_chart(&token.symbol, "€", &series, &path)?;
                println!("  {}", path.display());
            }

            // Portfolio total value chart
            let total_series = portfolio_total_series(&hist, &p, eur_rate);
            if total_series.len() >= 2 {
                let path = out_dir.join("portfolio_total.svg");
                render_chart("Portfolio Total", "€", &total_series, &path)?;
                println!("  {}", path.display());
            }

            println!("\n{} charts written to {}/", p.tokens.len() + 2, out_dir.display());
        }
    }

    Ok(())
}

// ── 30-day Birdeye history assembly ──────────────────────────────────────────

/// Fetch 30 days of hourly candles from Birdeye for every portfolio asset and
/// merge them into a single time-ordered VecDeque<PriceSnapshot> suitable for
/// plotting. Each snapshot contains all assets whose price is known at that hour.
async fn build_monthly_history(
    http: &reqwest::Client,
    api_key: &str,
    portfolio: &Portfolio,
) -> VecDeque<PriceSnapshot> {
    use std::collections::BTreeMap;

    const SOL_MINT: &str = "So11111111111111111111111111111111111111112";

    // ts → { key → price }  (BTreeMap keeps timestamps sorted)
    let mut combined: BTreeMap<u64, HashMap<String, f64>> = BTreeMap::new();

    let mut assets: Vec<(String, String)> = vec![(SOL_MINT.to_string(), "SOL".to_string())];
    for token in &portfolio.tokens {
        assets.push((token.mint.clone(), token.symbol.clone()));
    }

    for (i, (mint, symbol)) in assets.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        }
        match portfolio::pricer::fetch_monthly_history(http, api_key, mint).await {
            Ok(snaps) => {
                println!("  {symbol}: {} hourly candles", snaps.len());
                for snap in snaps {
                    let price = snap.prices.get(mint).copied().unwrap_or(0.0);
                    let entry = combined.entry(snap.ts).or_default();
                    entry.insert(mint.clone(), price);
                    // SOL is additionally keyed by "SOL" for portfolio total computation.
                    if symbol == "SOL" {
                        entry.insert("SOL".to_string(), price);
                    }
                }
            }
            Err(e) => eprintln!("  warning: could not fetch history for {symbol}: {e}"),
        }
    }

    combined
        .into_iter()
        .map(|(ts, prices)| PriceSnapshot { ts, prices })
        .collect()
}

// ── Chart data helpers ────────────────────────────────────────────────────────

/// Extract (timestamp_secs, price_eur) pairs for one asset from history.
fn price_series_eur(
    mint_key: &str,
    symbol: &str,
    history: &VecDeque<PriceSnapshot>,
    eur_rate: f64,
) -> Vec<(u64, f64)> {
    history
        .iter()
        .filter_map(|snap| {
            let price_usd = snap.prices.get(mint_key)
                .or_else(|| snap.prices.get(symbol))
                .copied()
                .filter(|&p| p > 0.0)?;
            Some((snap.ts, price_usd * eur_rate))
        })
        .collect()
}

/// Compute total portfolio EUR value at each snapshot tick.
fn portfolio_total_series(
    history: &VecDeque<PriceSnapshot>,
    portfolio: &Portfolio,
    eur_rate: f64,
) -> Vec<(u64, f64)> {
    history
        .iter()
        .filter_map(|snap| {
            let sol_usd = snap.prices.get("SOL").copied().unwrap_or(0.0);
            let mut total = portfolio.sol_amount * sol_usd * eur_rate;
            for token in &portfolio.tokens {
                let p = snap.prices.get(&token.mint)
                    .or_else(|| snap.prices.get(&token.symbol))
                    .copied()
                    .unwrap_or(0.0);
                total += token.amount * p * eur_rate;
            }
            if total > 0.0 { Some((snap.ts, total)) } else { None }
        })
        .collect()
}

// ── SVG rendering ─────────────────────────────────────────────────────────────

fn asset_color(symbol: &str) -> RGBColor {
    match symbol {
        "SOL"            => RGBColor(153,  85, 255),
        "JitoSOL"        => RGBColor(130,  60, 220),
        "NVDAx"          => RGBColor( 84, 186,  72),
        "AAPLx"          => RGBColor( 80,  80,  80),
        "GOOGLx"         => RGBColor( 66, 133, 244),
        "TSLAx"          => RGBColor(204,  51,  51),
        "QQQx"           => RGBColor(  0, 150, 136),
        "SPYx"           => RGBColor(255, 152,   0),
        "USDY"           => RGBColor( 46, 160,  67),
        "Portfolio Total"=> RGBColor( 30, 120, 200),
        _                => RGBColor(100, 149, 237),
    }
}

/// Downsample to at most `max` points while always keeping first and last.
fn downsample(data: &[(u64, f64)], max: usize) -> Vec<(u64, f64)> {
    if data.len() <= max { return data.to_vec(); }
    let step = (data.len() as f64 / max as f64).ceil() as usize;
    let mut out: Vec<(u64, f64)> = data.iter().step_by(step).copied().collect();
    if out.last() != data.last() {
        out.push(*data.last().unwrap());
    }
    out
}

/// Format a Unix timestamp offset (in hours from chart start) as a readable label.
fn fmt_hours(h: f32) -> String {
    if h < 24.0 {
        format!("{:.0}h", h)
    } else {
        format!("{:.0}d", h / 24.0)
    }
}

/// Rolling SMA computed in x-axis hours rather than array indices, so the window
/// is correct regardless of whether the underlying data is hourly or minute-level.
/// Uses an expanding window for the first `window_hours` of data (standard
/// "partial SMA" behaviour — same as TradingView when history is shorter than period).
fn sma_by_hours(xy: &[(f32, f32)], window_hours: f32) -> Vec<(f32, f32)> {
    xy.iter()
        .enumerate()
        .map(|(i, &(x, _))| {
            let cutoff = x - window_hours;
            let window: Vec<f32> = xy[..=i]
                .iter()
                .filter(|(px, _)| *px >= cutoff)
                .map(|(_, py)| *py)
                .collect();
            let avg = window.iter().sum::<f32>() / window.len() as f32;
            (x, avg)
        })
        .collect()
}

const MA_7D_COLOR:  RGBColor = RGBColor(255, 160,   0); // amber
const MA_30D_COLOR: RGBColor = RGBColor(200,  50,  50); // muted red

fn render_chart(title: &str, unit: &str, data: &[(u64, f64)], path: &Path) -> Result<()> {
    let points = downsample(data, 500);

    let first_ts = points[0].0;
    let xy: Vec<(f32, f32)> = points
        .iter()
        .map(|(ts, p)| ((*ts - first_ts) as f32 / 3600.0, *p as f32))
        .collect();

    let x_max = xy.last().map(|p| p.0).unwrap_or(1.0);
    let y_vals: Vec<f32> = xy.iter().map(|p| p.1).collect();
    let y_min = y_vals.iter().cloned().fold(f32::INFINITY, f32::min);
    let y_max = y_vals.iter().cloned().fold(f32::NEG_INFINITY, f32::max);
    let y_pad = ((y_max - y_min) * 0.08).max(y_max * 0.01);

    let color = asset_color(title);

    // Compute MAs before drawing (7d = 168h, 30d = 720h)
    let ma_7d  = sma_by_hours(&xy, 7.0  * 24.0);
    let ma_30d = sma_by_hours(&xy, 30.0 * 24.0);

    let root = SVGBackend::new(path, (900, 420)).into_drawing_area();
    root.fill(&RGBColor(248, 249, 250))?;

    let mut chart = ChartBuilder::on(&root)
        .caption(
            format!("{} — Price History", title),
            ("sans-serif", 22).into_font().color(&RGBColor(40, 40, 40)),
        )
        .margin(24)
        .x_label_area_size(44)
        .y_label_area_size(72)
        .build_cartesian_2d(0f32..x_max, (y_min - y_pad)..(y_max + y_pad))?;

    chart
        .configure_mesh()
        .light_line_style(RGBColor(225, 225, 225))
        .bold_line_style(RGBColor(210, 210, 210))
        .x_desc("Time")
        .y_desc(format!("Price ({})", unit))
        .x_label_formatter(&|x| fmt_hours(*x))
        .y_label_formatter(&|y| format!("{}{:.2}", unit, y))
        .x_labels(8)
        .y_labels(6)
        .draw()?;

    // Draw MAs first so they appear behind the price line
    chart
        .draw_series(LineSeries::new(ma_30d, MA_30D_COLOR.stroke_width(1)))?
        .label("30d MA")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], MA_30D_COLOR));

    chart
        .draw_series(LineSeries::new(ma_7d, MA_7D_COLOR.stroke_width(1)))?
        .label("7d MA")
        .legend(|(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], MA_7D_COLOR));

    // Price line drawn last — on top
    chart
        .draw_series(LineSeries::new(xy.iter().copied(), color.stroke_width(2)))?
        .label("Price")
        .legend(move |(x, y)| PathElement::new(vec![(x, y), (x + 20, y)], color));

    // Min / max dots
    if let (Some(&(_, low)), Some(&(_, high))) = (
        points.iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap()),
        points.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()),
    ) {
        let low_x  = points.iter().find(|(_, p)| *p == low ).map(|(ts, _)| (*ts - first_ts) as f32 / 3600.0).unwrap_or(0.0);
        let high_x = points.iter().find(|(_, p)| *p == high).map(|(ts, _)| (*ts - first_ts) as f32 / 3600.0).unwrap_or(0.0);
        chart.draw_series(std::iter::once(Circle::new((low_x,  low  as f32), 4, RGBColor(200, 60, 60).filled())))?;
        chart.draw_series(std::iter::once(Circle::new((high_x, high as f32), 4, RGBColor(60, 160, 60).filled())))?;
    }

    chart
        .configure_series_labels()
        .background_style(WHITE.mix(0.85))
        .border_style(RGBColor(200, 200, 200))
        .label_font(("sans-serif", 13))
        .position(SeriesLabelPosition::UpperLeft)
        .draw()?;

    root.present()?;
    Ok(())
}

// ── Display helpers ───────────────────────────────────────────────────────────

fn print_portfolio(p: &Portfolio, prices: &HashMap<String, f64>, eur_rate: f64) {
    let sol_usd = prices.get("SOL").copied().unwrap_or(0.0);
    let sol_eur = sol_usd * eur_rate;
    println!("  SOL      {:.4} × €{:.2} = €{:.2}", p.sol_amount, sol_eur, sol_eur * p.sol_amount);
    let mut total = sol_eur * p.sol_amount;
    for t in &p.tokens {
        let price_usd = prices.get(&t.mint).or_else(|| prices.get(&t.symbol)).copied().unwrap_or(0.0);
        let price_eur = price_usd * eur_rate;
        let value = price_eur * t.amount;
        if value < 0.01 && !prices.is_empty() { continue; }
        total += value;
        println!("  {:<8} {:.4} × €{:.4} = €{:.2}", t.symbol, t.amount, price_eur, value);
    }
    if !prices.is_empty() {
        println!("  ──────────────────────────────────");
        println!("  Total: €{:.2}", total);
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
                a.symbol, z_str, vol_str,
                format!("{:.1}%", a.current_drawdown_pct),
                a.drawdown_eur,
            );
        } else {
            println!("  {:<8}  (warming {}/{})", a.symbol, a.n_obs, min_obs);
        }
    }
    println!("  {}", "─".repeat(60));
    println!(
        "  Portfolio drawdown from combined peak: EUR -{:.2} ({:.1}%)",
        report.portfolio_drawdown_eur, report.portfolio_drawdown_pct.abs()
    );
}
