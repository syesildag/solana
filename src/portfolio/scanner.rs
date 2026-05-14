use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use solana_account_decoder::UiAccountData;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_request::TokenAccountsFilter;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use spl_token::state::Account as TokenAccount;
use std::collections::HashMap;

use super::{load_portfolio, save_portfolio, Portfolio, PortfolioConfig, TokenEntry};

/// Scan the wallet and write (or merge into) portfolio.json.
/// Called automatically at watcher startup and by `portfolio-cli init/update`.
pub async fn scan_and_save(cfg: &PortfolioConfig, http: &Client) -> Result<Portfolio> {
    let pubkey = load_pubkey(&cfg.wallet_keypair_path)?;
    let rpc = RpcClient::new(cfg.rpc_url.clone());

    let scanned = scan_wallet(&rpc, &pubkey, http).await?;

    let portfolio = match load_portfolio(&cfg.portfolio_path) {
        Ok(existing) => merge(existing, scanned),
        Err(_) => scanned, // no existing file → use scan result directly
    };

    save_portfolio(&cfg.portfolio_path, &portfolio)?;
    Ok(portfolio)
}

/// Scan wallet for SOL balance and all non-zero token accounts.
/// Queries both the original SPL Token program and Token 2022 (used by
/// tokenised stocks like GOOGLEX, NVDAX, and other newer assets).
pub async fn scan_wallet(rpc: &RpcClient, pubkey: &Pubkey, http: &Client) -> Result<Portfolio> {
    let lamports = rpc
        .get_balance(pubkey)
        .context("failed to fetch SOL balance")?;
    let sol_amount = lamports as f64 / 1_000_000_000.0;

    // Fetch from both token programs and combine
    let mut all_accounts = rpc
        .get_token_accounts_by_owner(pubkey, TokenAccountsFilter::ProgramId(spl_token::id()))
        .context("failed to fetch SPL Token accounts")?;

    match rpc.get_token_accounts_by_owner(
        pubkey,
        TokenAccountsFilter::ProgramId(spl_token_2022::id()),
    ) {
        Ok(t22) => all_accounts.extend(t22),
        Err(e) => tracing::warn!("wallet scan: Token 2022 query failed: {e}"),
    }

    let jupiter_symbols = fetch_symbol_map(http).await.unwrap_or_default();

    let mut tokens: Vec<TokenEntry> = Vec::new();
    for keyed in &all_accounts {
        // get_token_accounts_by_owner returns JsonParsed encoding; the Binary
        // arm handles any legacy fallback path.
        let (mint, ui_amount) = match &keyed.account.data {
            UiAccountData::Json(parsed) => {
                let info = parsed.parsed.get("info");
                let mint = info
                    .and_then(|i| i.get("mint"))
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                let amount = info
                    .and_then(|i| i.get("tokenAmount"))
                    .and_then(|ta| ta.get("uiAmount"))
                    .and_then(|a| a.as_f64());
                match (mint, amount) {
                    (Some(m), Some(a)) => (m, a),
                    _ => continue,
                }
            }
            UiAccountData::Binary(b64, _) => {
                let data = STANDARD.decode(b64).unwrap_or_default();
                if data.len() < TokenAccount::LEN {
                    continue;
                }
                let acct = match TokenAccount::unpack(&data[..TokenAccount::LEN]) {
                    Ok(a) => a,
                    Err(_) => continue,
                };
                let decimals = fetch_mint_decimals(rpc, &acct.mint).unwrap_or(0);
                let amount = acct.amount as f64 / 10f64.powi(decimals as i32);
                (acct.mint.to_string(), amount)
            }
            _ => continue,
        };

        if ui_amount == 0.0 {
            continue;
        }

        let symbol = jupiter_symbols
            .get(&mint)
            .cloned()
            .unwrap_or_else(|| format!("UNK_{}", &mint[..6]));

        tokens.push(TokenEntry { mint, symbol, amount: ui_amount });
    }

    // For any mints Jupiter didn't recognise, try DexScreener.
    let unknown_mints: Vec<String> = tokens
        .iter()
        .filter(|t| t.symbol.starts_with("UNK_"))
        .map(|t| t.mint.clone())
        .collect();

    if !unknown_mints.is_empty() {
        let dex_symbols = crate::portfolio::pricer::resolve_symbols_dexscreener(http, &unknown_mints).await;
        for token in &mut tokens {
            if token.symbol.starts_with("UNK_") {
                if let Some(sym) = dex_symbols.get(&token.mint) {
                    token.symbol = sym.clone();
                }
            }
        }
    }

    tokens.sort_by(|a, b| a.mint.cmp(&b.mint));

    tracing::info!(
        "wallet scan: {pubkey} — {:.4} SOL, {} token(s)",
        sol_amount,
        tokens.len()
    );

    Ok(Portfolio { sol_amount, tokens })
}

/// Merge on-chain scan into existing portfolio: update amounts, drop zeroed
/// tokens, append new ones. Preserves the existing token ordering.
pub fn merge(mut existing: Portfolio, scanned: Portfolio) -> Portfolio {
    existing.sol_amount = scanned.sol_amount;

    let scanned_map: HashMap<String, &TokenEntry> =
        scanned.tokens.iter().map(|t| (t.mint.clone(), t)).collect();

    existing.tokens.retain_mut(|t| {
        if let Some(s) = scanned_map.get(&t.mint) {
            t.amount = s.amount;
            // Upgrade symbol if previously unresolved and we now have a real name
            if t.symbol.starts_with("UNK_") && !s.symbol.starts_with("UNK_") {
                t.symbol = s.symbol.clone();
            }
            true
        } else {
            false
        }
    });

    let existing_mints: std::collections::HashSet<String> =
        existing.tokens.iter().map(|t| t.mint.clone()).collect();
    for t in scanned.tokens {
        if !existing_mints.contains(&t.mint) {
            existing.tokens.push(t);
        }
    }

    existing
}

/// Fetch the Jupiter all-tokens list and return mint → symbol map.
pub async fn fetch_symbol_map(http: &Client) -> Result<HashMap<String, String>> {
    let tokens: Vec<serde_json::Value> = http
        .get("https://token.jup.ag/all")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    Ok(tokens
        .into_iter()
        .filter_map(|v| {
            let mint = v.get("address")?.as_str()?.to_string();
            let symbol = v.get("symbol")?.as_str()?.to_string();
            Some((mint, symbol))
        })
        .collect())
}

pub fn load_pubkey(keypair_path: &str) -> Result<Pubkey> {
    let expanded = if keypair_path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}{}", home, &keypair_path[1..])
    } else {
        keypair_path.to_string()
    };
    let data = std::fs::read_to_string(&expanded).context("failed to read keypair file")?;
    let bytes: Vec<u8> = serde_json::from_str(&data).context("invalid keypair JSON")?;
    let keypair = solana_sdk::signature::Keypair::from_bytes(&bytes)
        .context("invalid keypair bytes")?;
    Ok(keypair.pubkey())
}

fn fetch_mint_decimals(rpc: &RpcClient, mint: &Pubkey) -> Result<u8> {
    use spl_token::state::Mint;
    let data = rpc.get_account_data(mint)?;
    // Token 2022 mints have extensions after the base Mint layout; slice to
    // exactly Mint::LEN so unpack doesn't reject the extra bytes.
    let slice = if data.len() >= Mint::LEN { &data[..Mint::LEN] } else { &data };
    let mint_state = Mint::unpack(slice)?;
    Ok(mint_state.decimals)
}
