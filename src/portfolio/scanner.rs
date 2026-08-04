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
    let scanned = scan_wallet(&cfg.rpc_url, &pubkey, http).await?;

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
pub async fn scan_wallet(rpc_url: &str, pubkey: &Pubkey, http: &Client) -> Result<Portfolio> {
    // SOL balance + raw token amounts come from blocking RPC, run on a dedicated
    // blocking thread so the scan never stalls the async runtime (same pattern as
    // fetch_token_balance / fetch_decimals_for_mints). Symbol resolution below is async.
    let (sol_amount, raw_tokens) = fetch_wallet_balances(rpc_url, *pubkey).await?;

    let jupiter_symbols = fetch_symbol_map(http).await.unwrap_or_default();

    let mut tokens: Vec<TokenEntry> = raw_tokens
        .into_iter()
        .map(|(mint, amount)| {
            let symbol = jupiter_symbols
                .get(&mint)
                .cloned()
                .unwrap_or_else(|| format!("UNK_{}", &mint[..6]));
            TokenEntry { mint, symbol, amount }
        })
        .collect();

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

/// Blocking RPC, offloaded to a dedicated thread: SOL balance + every non-zero
/// `(mint, ui_amount)` across both token programs. Kept off the async runtime so a
/// periodic re-scan never stalls the watcher's `select!` loop. Returns raw amounts;
/// symbol resolution (async HTTP) happens in `scan_wallet`.
async fn fetch_wallet_balances(rpc_url: &str, pubkey: Pubkey) -> Result<(f64, Vec<(String, f64)>)> {
    let rpc_url = rpc_url.to_string();
    tokio::task::spawn_blocking(move || -> Result<(f64, Vec<(String, f64)>)> {
        let rpc = RpcClient::new(rpc_url);
        let lamports = rpc.get_balance(&pubkey).context("failed to fetch SOL balance")?;
        let sol_amount = lamports as f64 / 1_000_000_000.0;

        // Fetch from both token programs and combine.
        let mut all_accounts = rpc
            .get_token_accounts_by_owner(&pubkey, TokenAccountsFilter::ProgramId(spl_token::id()))
            .context("failed to fetch SPL Token accounts")?;
        match rpc.get_token_accounts_by_owner(
            &pubkey,
            TokenAccountsFilter::ProgramId(spl_token_2022::id()),
        ) {
            Ok(t22) => all_accounts.extend(t22),
            Err(e) => tracing::warn!("wallet scan: Token 2022 query failed: {e}"),
        }

        let mut out: Vec<(String, f64)> = Vec::new();
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
                    let decimals = fetch_mint_decimals(&rpc, &acct.mint).unwrap_or(0);
                    let amount = acct.amount as f64 / 10f64.powi(decimals as i32);
                    (acct.mint.to_string(), amount)
                }
                _ => continue,
            };
            if ui_amount == 0.0 {
                continue;
            }
            out.push((mint, ui_amount));
        }
        Ok((sol_amount, out))
    })
    .await
    .context("wallet scan join failed")?
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
    Ok(load_keypair(keypair_path)?.pubkey())
}

/// Load a Solana keypair from a JSON byte-array file. Used via `load_pubkey`
/// to derive the wallet address for portfolio scanning.
pub fn load_keypair(keypair_path: &str) -> Result<solana_sdk::signature::Keypair> {
    let expanded = if keypair_path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        format!("{}{}", home, &keypair_path[1..])
    } else {
        keypair_path.to_string()
    };
    let data = std::fs::read_to_string(&expanded).context("failed to read keypair file")?;
    let bytes: Vec<u8> = serde_json::from_str(&data).context("invalid keypair JSON")?;
    solana_sdk::signature::Keypair::from_bytes(&bytes).context("invalid keypair bytes")
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

/// Read decimals for an arbitrary list of mints via Solana RPC. Used by the
/// momentum trader at startup so it can convert human ↔ raw amounts for the
/// watched tokens (which we may not yet hold). SOL is added automatically with
/// the canonical 9 decimals.
///
/// Returns a map keyed by mint address (string). Mints that fail to fetch are
/// omitted rather than failing the whole call — the trader simply skips any
/// candidate whose decimals it cannot resolve, which is the safe behavior.
pub async fn fetch_decimals_for_mints(
    rpc_url: &str,
    mints: Vec<String>,
) -> Result<std::collections::HashMap<String, u8>> {
    let rpc_url = rpc_url.to_string();
    tokio::task::spawn_blocking(move || -> Result<std::collections::HashMap<String, u8>> {
        let rpc = RpcClient::new(rpc_url);
        let mut out = std::collections::HashMap::new();
        out.insert(
            "So11111111111111111111111111111111111111112".to_string(),
            9,
        );
        for mint_str in mints {
            let Ok(mint_pk) = mint_str.parse::<Pubkey>() else {
                tracing::warn!("decimals fetch: skipping invalid mint {mint_str}");
                continue;
            };
            match fetch_mint_decimals(&rpc, &mint_pk) {
                Ok(d) => { out.insert(mint_str, d); }
                Err(e) => tracing::warn!("decimals fetch: skipping {mint_str}: {e}"),
            }
        }
        Ok(out)
    })
    .await
    .context("decimals join failed")?
}

/// Sum the wallet's on-chain balance in **RAW base units** for a single mint across both
/// token programs — the amount a swap actually takes.
///
/// Prefer this over `fetch_token_balance` for anything that sizes a transaction. Converting
/// a UI amount back to raw (`ui × 10^decimals`) is WRONG for a Token-2022 mint carrying the
/// `scaledUiAmount` extension, because there `uiAmount = raw × multiplier / 10^decimals`.
/// Measured 2026-08-04 on AAPLx (multiplier 1.002018559, and a larger one already queued):
/// the account held 429,004,520 raw / uiAmount 4.30147477, so the naive round-trip asked to
/// sell 430,147,477 — 1,142,957 raw more than existed. Jupiter rejected every attempt at
/// preflight (custom error 6024) on every route and venue, escalating slippage could never
/// fix an over-balance amount, and the exit wedged for 395 consecutive attempts. Simulating
/// the true raw amount on the same route succeeded immediately. Backed/Backpack xStocks all
/// carry this extension and its multiplier accrues, so the overshoot grows over time.
pub async fn fetch_token_balance_raw(rpc_url: &str, owner: &str, mint: &str) -> Result<u64> {
    let rpc_url = rpc_url.to_string();
    let owner = owner.to_string();
    let mint = mint.to_string();
    tokio::task::spawn_blocking(move || -> Result<u64> {
        let owner_pk: Pubkey = owner.parse().context("invalid owner pubkey")?;
        let mint_pk: Pubkey = mint.parse().context("invalid mint pubkey")?;
        let rpc = RpcClient::new(rpc_url);
        let accounts = rpc
            .get_token_accounts_by_owner(&owner_pk, TokenAccountsFilter::Mint(mint_pk))
            .context("get_token_accounts_by_owner(mint) failed")?;
        let mut total: u64 = 0;
        for keyed in &accounts {
            match &keyed.account.data {
                // `tokenAmount.amount` is the raw base-unit string — unscaled by design.
                UiAccountData::Json(parsed) => {
                    if let Some(a) = parsed
                        .parsed
                        .get("info")
                        .and_then(|i| i.get("tokenAmount"))
                        .and_then(|ta| ta.get("amount"))
                        .and_then(|a| a.as_str())
                        .and_then(|s| s.parse::<u64>().ok())
                    {
                        total = total.saturating_add(a);
                    }
                }
                UiAccountData::Binary(b64, _) => {
                    let data = STANDARD.decode(b64).unwrap_or_default();
                    if data.len() >= TokenAccount::LEN {
                        if let Ok(acct) = TokenAccount::unpack(&data[..TokenAccount::LEN]) {
                            total = total.saturating_add(acct.amount);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(total)
    })
    .await
    .context("raw token balance join failed")?
}

/// Sum the wallet's on-chain balance (human units) for a single mint across both
/// token programs. Reporting/display only — for sizing a swap use
/// `fetch_token_balance_raw`, which is correct for `scaledUiAmount` mints.
pub async fn fetch_token_balance(rpc_url: &str, owner: &str, mint: &str) -> Result<f64> {
    let rpc_url = rpc_url.to_string();
    let owner = owner.to_string();
    let mint = mint.to_string();
    tokio::task::spawn_blocking(move || -> Result<f64> {
        let owner_pk: Pubkey = owner.parse().context("invalid owner pubkey")?;
        let mint_pk: Pubkey = mint.parse().context("invalid mint pubkey")?;
        let rpc = RpcClient::new(rpc_url);
        let accounts = rpc
            .get_token_accounts_by_owner(&owner_pk, TokenAccountsFilter::Mint(mint_pk))
            .context("get_token_accounts_by_owner(mint) failed")?;
        let mut total = 0.0;
        for keyed in &accounts {
            match &keyed.account.data {
                UiAccountData::Json(parsed) => {
                    if let Some(a) = parsed
                        .parsed
                        .get("info")
                        .and_then(|i| i.get("tokenAmount"))
                        .and_then(|ta| ta.get("uiAmount"))
                        .and_then(|a| a.as_f64())
                    {
                        total += a;
                    }
                }
                UiAccountData::Binary(b64, _) => {
                    let data = STANDARD.decode(b64).unwrap_or_default();
                    if data.len() >= TokenAccount::LEN {
                        if let Ok(acct) = TokenAccount::unpack(&data[..TokenAccount::LEN]) {
                            let decimals = fetch_mint_decimals(&rpc, &acct.mint).unwrap_or(0);
                            total += acct.amount as f64 / 10f64.powi(decimals as i32);
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(total)
    })
    .await
    .context("token balance join failed")?
}
