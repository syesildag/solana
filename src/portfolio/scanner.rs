use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use solana_account_decoder::{UiAccount, UiAccountData, UiAccountEncoding};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::RpcAccountInfoConfig;
use solana_client::rpc_request::{RpcRequest, TokenAccountsFilter};
use solana_client::rpc_response::{Response, RpcKeyedAccount};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::program_pack::Pack;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use spl_associated_token_account::get_associated_token_address_with_program_id;
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

        // Fetch from both token programs and combine. A Token-2022 failure here
        // propagates as `Err` (via `?`) rather than warn-and-continue: silently
        // returning just the SPL Token half would present a PARTIAL scan as a
        // complete one, and `scan_and_save` would merge/overwrite portfolio.json
        // with it — dropping every real Token-2022 balance (GOOGLEX, NVDAX, etc.)
        // from the file. An `Err` here instead makes `scan_and_save` keep the
        // previous portfolio.json for this tick and retry next time.
        let mut all_accounts = rpc
            .get_token_accounts_by_owner(&pubkey, TokenAccountsFilter::ProgramId(spl_token::id()))
            .context("failed to fetch SPL Token accounts")?;
        let t22_accounts = rpc
            .get_token_accounts_by_owner(&pubkey, TokenAccountsFilter::ProgramId(spl_token_2022::id()))
            .context("failed to fetch Token-2022 accounts")?;
        all_accounts.extend(t22_accounts);

        let mut out: Vec<(String, f64)> = Vec::new();
        for keyed in &all_accounts {
            // get_token_accounts_by_owner returns JsonParsed encoding; the Binary
            // arm handles any legacy fallback path.
            let (mint, ui_amount) = match &keyed.account.data {
                UiAccountData::Json(parsed) => {
                    match parsed.parsed.get("info").and_then(parse_token_amount) {
                        Some((m, a)) => (m, a),
                        None => continue,
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

/// Parse one jsonParsed token-account `info` object (the `{"mint":..,"tokenAmount":..}`
/// value under `parsed.parsed.get("info")`) into `(mint, ui_amount)`.
///
/// Falls back to `amount / 10^decimals` when `uiAmount` is null (or absent) instead of
/// dropping the token from the scan — observed on Token-2022 mints carrying the
/// `scaledUiAmount` extension, where the RPC can report a null `uiAmount` even though
/// the raw `amount` + `decimals` are always present.
///
/// The fallback is deliberately the NAIVE `raw / 10^decimals`, NOT the true scaled UI
/// value (`raw × multiplier / 10^decimals` — see the doc on `fetch_token_balance_raw`).
/// Whatever this returns ends up as `Position.token_amount`, which sell-sizing call
/// sites (`jupiter::to_raw_amount`, used throughout `momentum.rs`) convert back to raw
/// via the same naive `human × 10^decimals`. Storing the true scaled value here would
/// silently reintroduce the AAPLx-class over-sell bug documented on
/// `fetch_token_balance_raw` the moment the multiplier is anything but 1; the naive
/// value round-trips through `to_raw_amount` back to the original raw amount exactly.
fn parse_token_amount(info: &serde_json::Value) -> Option<(String, f64)> {
    let mint = info.get("mint")?.as_str()?.to_string();
    let token_amount = info.get("tokenAmount")?;
    let ui_amount = match token_amount.get("uiAmount").and_then(|a| a.as_f64()) {
        Some(a) => a,
        None => {
            let raw: u64 = token_amount.get("amount")?.as_str()?.parse().ok()?;
            let decimals = token_amount.get("decimals")?.as_u64()?;
            raw as f64 / 10f64.powi(decimals as i32)
        }
    };
    Some((mint, ui_amount))
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

/// One ATA lookup outcome, normalized for the zero-confirmation verdict.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AtaLookup {
    /// RPC answered positively: no account at that address.
    Absent,
    /// Account exists and holds this raw amount.
    Amount(u64),
    /// Account exists but its data did not parse as a token account.
    Unparseable,
}

/// Pure verdict (unit-tested): is the balance CONFIRMED zero?
/// `owner_total` = the owner-indexed query result; `ata_spl`/`ata_2022` = direct lookups.
/// Confirmed zero requires the owner query to have found nothing AND both direct
/// lookups to answer Absent or Amount(0). Anything else keeps the position:
/// any Amount(>0) anywhere is non-zero; Unparseable is ambiguous ⇒ NOT confirmed.
pub fn zero_verdict(owner_total: u64, ata_spl: AtaLookup, ata_2022: AtaLookup) -> ZeroVerdict {
    let max_nonzero = [
        owner_total,
        match ata_spl {
            AtaLookup::Amount(n) => n,
            _ => 0,
        },
        match ata_2022 {
            AtaLookup::Amount(n) => n,
            _ => 0,
        },
    ]
    .into_iter()
    .max()
    .unwrap_or(0);

    if max_nonzero > 0 {
        return ZeroVerdict::NonZero(max_nonzero);
    }
    if matches!(ata_spl, AtaLookup::Unparseable) || matches!(ata_2022, AtaLookup::Unparseable) {
        return ZeroVerdict::Unconfirmed;
    }
    ZeroVerdict::ConfirmedZero
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ZeroVerdict {
    ConfirmedZero,
    /// Positive evidence of a balance: the **max** of three raw on-chain amounts (the
    /// strict owner-index sum and both direct ATA reads), in base units.
    ///
    /// Primarily evidence — a caller with a working `fetch_token_balance_raw` must use
    /// that, because this number is a max-of-sources and can exceed what any single token
    /// account holds when a balance is split across a non-ATA account.
    ///
    /// One sanctioned exception, and only because it is provably no worse than the
    /// alternative: the sell paths that reach `confirm_zero_balance` **only** after
    /// `fetch_token_balance_raw` already returned 0 (`flatten_position`, `try_rotate`) may
    /// size from it. There, the "normal path" has already failed to see the balance, so the
    /// choice is between this number and not selling at all; it is ≥ the owner sum that
    /// path would have produced (identical over-size risk, no regression), and in the case
    /// that actually lands there — owner index empty, ATA funded — it is the exact
    /// single-account amount the swap will spend.
    NonZero(u64),
    /// Ambiguous (unparseable account data) — treat like a failed read.
    Unconfirmed,
}

/// Owner-indexed raw sum for `confirm_zero_balance` only — same query as
/// `fetch_token_balance_raw`, but (via `sum_keyed_accounts_strict`) a parse failure on
/// any returned account is an `Err`, never a silently-skipped zero. `fetch_token_balance_raw`
/// itself is left untouched for its other (non-invalidation) callers: this stricter
/// variant exists because a silent 0 here would repeat the exact bug (commit ecf5669)
/// this primitive was built to close — a partial/empty read must never look identical
/// to a genuinely empty wallet.
fn sum_owner_accounts_strict(rpc: &RpcClient, owner: &Pubkey, mint: &Pubkey) -> Result<u64> {
    let accounts = rpc
        .get_token_accounts_by_owner(owner, TokenAccountsFilter::Mint(*mint))
        .context("get_token_accounts_by_owner(mint) failed")?;
    sum_keyed_accounts_strict(&accounts)
}

/// The response-classification half of `sum_owner_accounts_strict`, pulled out so the
/// logic the fail-closed guarantee actually rests on is unit-testable without a live
/// `RpcClient`. Every per-account parse failure — bad JSON path, bad base64, undersized
/// data, `unpack` failure, or an unexpected/legacy encoding — is an `Err`; none silently
/// contributes 0 to the sum.
fn sum_keyed_accounts_strict(accounts: &[RpcKeyedAccount]) -> Result<u64> {
    let mut total: u64 = 0;
    for keyed in accounts {
        let amount: u64 = match &keyed.account.data {
            UiAccountData::Json(parsed) => parsed
                .parsed
                .get("info")
                .and_then(|i| i.get("tokenAmount"))
                .and_then(|ta| ta.get("amount"))
                .and_then(|a| a.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .with_context(|| format!("unparseable tokenAmount for account {}", keyed.pubkey))?,
            // Only the encoding we actually expect from a legacy Binary fallback is
            // trusted; anything else (wrong declared encoding, base58 legacy blob,
            // etc.) falls through to the `other` arm below and errors instead of
            // being silently mis-parsed.
            UiAccountData::Binary(b64, UiAccountEncoding::Base64) => {
                let data = STANDARD
                    .decode(b64)
                    .with_context(|| format!("bad base64 for account {}", keyed.pubkey))?;
                if data.len() < TokenAccount::LEN {
                    anyhow::bail!("account {} data too short to be a token account", keyed.pubkey);
                }
                TokenAccount::unpack(&data[..TokenAccount::LEN])
                    .with_context(|| format!("unparseable token account {}", keyed.pubkey))?
                    .amount
            }
            other => anyhow::bail!("unexpected account data encoding for {}: {other:?}", keyed.pubkey),
        };
        total = total.saturating_add(amount);
    }
    Ok(total)
}

/// Classify one direct ATA lookup result — the response-classification half of
/// `confirm_zero_balance`'s ATA cross-check, pulled out so it is unit-testable without
/// a live `RpcClient`. `expected_owner`/`expected_mint` guard against ever trusting a
/// syntactically-valid-but-mismatched account: cheap defense in depth on top of the PDA
/// derivation itself.
///
/// `acct: None` here MUST mean a true positive absence (the raw `getMultipleAccounts`
/// response slot was JSON `null`) — this is deliberately NOT fed from the convenience
/// `RpcClient::get_multiple_accounts` helper, whose `Option<Account>` collapses two very
/// different outcomes into the same `None`: a genuinely absent account, AND an account
/// that exists but whose data failed to decode (wrong/unexpected encoding, corrupt or
/// truncated payload, an unparseable owner field). See `solana-rpc-client`'s
/// `get_multiple_accounts_with_config`: `rpc_account.and_then(|a| a.decode())` — a decode
/// failure and a `null` both end up `None`. That conflation is exactly the kind of
/// silent-zero this primitive exists to prevent, so `confirm_zero_balance` calls the raw
/// `getMultipleAccounts` RPC itself (`RpcClient::send`) and passes `Option<&UiAccount>`
/// here instead, where a decode failure is classified explicitly below.
fn classify_ata(acct: Option<&UiAccount>, expected_owner: &Pubkey, expected_mint: &Pubkey) -> AtaLookup {
    let ui = match acct {
        None => return AtaLookup::Absent,
        Some(ui) => ui,
    };
    // We explicitly requested Base64 encoding; anything else (a non-conforming
    // provider ignoring the request, a legacy/base58 payload, or a parsed-JSON
    // account) is ambiguous — never assume it means "no account".
    let data = match &ui.data {
        UiAccountData::Binary(b64, UiAccountEncoding::Base64) => match STANDARD.decode(b64) {
            Ok(d) => d,
            Err(_) => return AtaLookup::Unparseable,
        },
        _ => return AtaLookup::Unparseable,
    };
    if data.len() < TokenAccount::LEN {
        return AtaLookup::Unparseable;
    }
    match TokenAccount::unpack(&data[..TokenAccount::LEN]) {
        Ok(t) if &t.mint == expected_mint && &t.owner == expected_owner => AtaLookup::Amount(t.amount),
        // Decoded fine but doesn't match the ATA we asked for — never trust it as this
        // account's balance, and never treat it as absence either.
        Ok(_) => AtaLookup::Unparseable,
        Err(_) => AtaLookup::Unparseable,
    }
}

/// Evidence-grade zero-balance confirmation, built to be genuinely independent of
/// `fetch_token_balance_raw`'s owner-indexed query rather than repeating it: an
/// earlier fix (commit ecf5669) re-confirmed a suspected-zero balance with the SAME
/// `get_token_accounts_by_owner` method on the same endpoint, so a transient partial
/// response from that one index could still confirm zero and delete a live position.
///
/// This primitive cross-checks the owner-indexed sum against a direct lookup of both
/// possible ATAs (spl-token + token-2022) — no secondary owner index involved in the
/// ATA path at all — both at CONFIRMED commitment. The ATA lookup issues the raw
/// `getMultipleAccounts` RPC directly (`RpcClient::send` + explicit Base64 encoding)
/// rather than the `get_multiple_accounts` convenience helper — see `classify_ata`'s doc
/// for why that helper's client-side decode is unsafe here. `zero_verdict` then requires
/// ALL THREE observations to agree the balance is zero; any disagreement or ambiguity
/// (including an unparseable account) falls out as `NonZero`/`Unconfirmed`, never
/// `ConfirmedZero`. A transport error anywhere — including a malformed/short RPC
/// response — is `Err`, never a panic: the caller keeps the position on any `Err`,
/// exactly like every other fail-closed read in this module (release builds run
/// `panic = "abort"`, so a panic anywhere would take down the whole process, not just
/// this one confirmation — every fallible step here is therefore an explicit `Result`,
/// never an indexing/unwrap that could panic on a non-conforming provider response).
pub async fn confirm_zero_balance(rpc_url: &str, owner: &str, mint: &str) -> Result<ZeroVerdict> {
    let rpc_url = rpc_url.to_string();
    let owner = owner.to_string();
    let mint = mint.to_string();
    tokio::task::spawn_blocking(move || -> Result<ZeroVerdict> {
        let owner_pk: Pubkey = owner.parse().context("invalid owner pubkey")?;
        let mint_pk: Pubkey = mint.parse().context("invalid mint pubkey")?;
        let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

        let owner_total = sum_owner_accounts_strict(&rpc, &owner_pk, &mint_pk)?;

        let ata_spl = get_associated_token_address_with_program_id(&owner_pk, &mint_pk, &spl_token::id());
        let ata_2022 =
            get_associated_token_address_with_program_id(&owner_pk, &mint_pk, &spl_token_2022::id());

        let config = RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            data_slice: None,
            min_context_slot: None,
        };
        let pubkey_strs = vec![ata_spl.to_string(), ata_2022.to_string()];
        let resp: Response<Vec<Option<UiAccount>>> = rpc
            .send(RpcRequest::GetMultipleAccounts, serde_json::json!([pubkey_strs, config]))
            .context("getMultipleAccounts(ata_spl, ata_2022) failed")?;

        // A non-conforming provider/proxy returning the wrong-length array must not
        // panic on the indexing below — bail into `Err` instead, which keeps the
        // position exactly like any other transport failure.
        if resp.value.len() != 2 {
            anyhow::bail!("getMultipleAccounts returned {} entries, expected 2", resp.value.len());
        }
        let ata_spl_lookup = classify_ata(resp.value[0].as_ref(), &owner_pk, &mint_pk);
        let ata_2022_lookup = classify_ata(resp.value[1].as_ref(), &owner_pk, &mint_pk);

        Ok(zero_verdict(owner_total, ata_spl_lookup, ata_2022_lookup))
    })
    .await
    .context("confirm_zero_balance join failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::program_option::COption;
    use spl_token::state::AccountState;

    /// Build a valid (or extension-padded) SPL Token account byte layout for tests.
    fn packed_token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> Vec<u8> {
        let acct = TokenAccount {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut buf = vec![0u8; TokenAccount::LEN];
        acct.pack_into_slice(&mut buf);
        buf
    }

    /// Wrap raw bytes as the `UiAccount` shape `classify_ata` expects from a
    /// Base64-encoded `getMultipleAccounts` response.
    fn ui_account_base64(data: &[u8]) -> UiAccount {
        UiAccount {
            lamports: 2_039_280,
            data: UiAccountData::Binary(STANDARD.encode(data), UiAccountEncoding::Base64),
            owner: spl_token::id().to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(data.len() as u64),
        }
    }

    #[test]
    fn classify_ata_none_is_absent() {
        // A raw `getMultipleAccounts` `null` slot is a TRUE positive absence.
        assert_eq!(
            classify_ata(None, &Pubkey::new_unique(), &Pubkey::new_unique()),
            AtaLookup::Absent
        );
    }

    #[test]
    fn classify_ata_valid_account_is_amount() {
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let bytes = packed_token_account(mint, owner, 42);
        let ui = ui_account_base64(&bytes);
        assert_eq!(classify_ata(Some(&ui), &owner, &mint), AtaLookup::Amount(42));
    }

    #[test]
    fn classify_ata_extension_bytes_still_unpack_base_layout() {
        // Token-2022 accounts carry extension TLV bytes after the base 165-byte
        // layout; classify_ata must still unpack the base fields from the first
        // 165 bytes rather than rejecting the account as unparseable.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let mut bytes = packed_token_account(mint, owner, 777);
        bytes.extend_from_slice(&[0xAB; 40]); // simulated extension TLV tail
        let ui = ui_account_base64(&bytes);
        assert_eq!(classify_ata(Some(&ui), &owner, &mint), AtaLookup::Amount(777));
    }

    #[test]
    fn classify_ata_short_data_is_unparseable() {
        // Valid base64, but fewer bytes than a token account layout requires.
        let ui = ui_account_base64(&[1, 2, 3]);
        assert_eq!(
            classify_ata(Some(&ui), &Pubkey::new_unique(), &Pubkey::new_unique()),
            AtaLookup::Unparseable
        );
    }

    #[test]
    fn classify_ata_bad_base64_is_unparseable() {
        let ui = UiAccount {
            lamports: 1,
            data: UiAccountData::Binary("not valid base64 !!".to_string(), UiAccountEncoding::Base64),
            owner: spl_token::id().to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(0),
        };
        assert_eq!(
            classify_ata(Some(&ui), &Pubkey::new_unique(), &Pubkey::new_unique()),
            AtaLookup::Unparseable
        );
    }

    #[test]
    fn classify_ata_non_base64_encoding_is_unparseable_not_absent() {
        // A provider that ignores the requested Base64 encoding (e.g. serves
        // jsonParsed anyway) must never be treated as "no account" — that would
        // let an existing, funded ATA masquerade as Absent (the exact bug class
        // `RpcClient::get_multiple_accounts`'s lossy `.decode()` was replaced for).
        let ui = UiAccount {
            lamports: 1,
            data: serde_json::from_value(serde_json::json!({
                "program": "spl-token",
                "parsed": {},
                "space": 165u64
            }))
            .unwrap(),
            owner: spl_token::id().to_string(),
            executable: false,
            rent_epoch: 0,
            space: Some(165),
        };
        assert_eq!(
            classify_ata(Some(&ui), &Pubkey::new_unique(), &Pubkey::new_unique()),
            AtaLookup::Unparseable
        );
    }

    #[test]
    fn classify_ata_mismatched_mint_or_owner_is_unparseable() {
        // Syntactically valid, but not the account we asked for — never trusted as
        // this ATA's balance, and never treated as absence either.
        let owner = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let wrong_mint = Pubkey::new_unique();
        let bytes = packed_token_account(mint, owner, 5);
        let ui = ui_account_base64(&bytes);
        assert_eq!(classify_ata(Some(&ui), &owner, &wrong_mint), AtaLookup::Unparseable);

        let wrong_owner = Pubkey::new_unique();
        assert_eq!(classify_ata(Some(&ui), &wrong_owner, &mint), AtaLookup::Unparseable);
    }

    /// Build an `RpcKeyedAccount` carrying a jsonParsed tokenAmount payload, mirroring
    /// what `get_token_accounts_by_owner` actually returns.
    fn keyed_json_account(pubkey: &str, raw_amount: &str) -> RpcKeyedAccount {
        RpcKeyedAccount {
            pubkey: pubkey.to_string(),
            account: UiAccount {
                lamports: 2_039_280,
                data: serde_json::from_value(serde_json::json!({
                    "program": "spl-token",
                    "parsed": { "info": { "tokenAmount": {
                        "amount": raw_amount, "decimals": 6, "uiAmount": null
                    } } },
                    "space": 165u64
                }))
                .unwrap(),
                owner: spl_token::id().to_string(),
                executable: false,
                rent_epoch: 0,
                space: Some(165),
            },
        }
    }

    #[test]
    fn sum_keyed_accounts_strict_empty_is_zero() {
        assert_eq!(sum_keyed_accounts_strict(&[]).unwrap(), 0);
    }

    #[test]
    fn sum_keyed_accounts_strict_sums_valid_json_entries() {
        let a = keyed_json_account("acct-a", "1000000");
        let b = keyed_json_account("acct-b", "2500000");
        assert_eq!(sum_keyed_accounts_strict(&[a, b]).unwrap(), 3_500_000);
    }

    #[test]
    fn sum_keyed_accounts_strict_one_unparseable_entry_is_err() {
        // This is the binding fail-closed constraint: a parse failure anywhere in
        // the batch must propagate as `Err`, never silently contribute 0 (the exact
        // swallow this primitive replaces).
        let good = keyed_json_account("acct-a", "1000000");
        let bad = RpcKeyedAccount {
            pubkey: "acct-bad".to_string(),
            account: UiAccount {
                lamports: 1,
                // Missing "tokenAmount.amount" entirely — the parse-failure path.
                data: serde_json::from_value(serde_json::json!({
                    "program": "spl-token",
                    "parsed": { "info": { "tokenAmount": {} } },
                    "space": 165u64
                }))
                .unwrap(),
                owner: spl_token::id().to_string(),
                executable: false,
                rent_epoch: 0,
                space: Some(165),
            },
        };
        assert!(sum_keyed_accounts_strict(&[good, bad]).is_err());
    }

    #[test]
    fn sum_keyed_accounts_strict_legacy_binary_is_err() {
        let keyed = RpcKeyedAccount {
            pubkey: "acct-legacy".to_string(),
            account: UiAccount {
                lamports: 1,
                data: UiAccountData::LegacyBinary("11111111111111111111111111111111".to_string()),
                owner: spl_token::id().to_string(),
                executable: false,
                rent_epoch: 0,
                space: Some(0),
            },
        };
        assert!(sum_keyed_accounts_strict(&[keyed]).is_err());
    }

    #[test]
    fn zero_verdict_requires_positive_absence_everywhere() {
        use AtaLookup::*;
        // Confirmed zero: owner query empty + both ATAs positively absent/zero.
        assert_eq!(zero_verdict(0, Absent, Absent), ZeroVerdict::ConfirmedZero);
        assert_eq!(zero_verdict(0, Amount(0), Absent), ZeroVerdict::ConfirmedZero);
        // Any positive balance anywhere wins.
        assert_eq!(zero_verdict(5, Absent, Absent), ZeroVerdict::NonZero(5));
        assert_eq!(zero_verdict(0, Amount(7), Absent), ZeroVerdict::NonZero(7));
        assert_eq!(zero_verdict(0, Absent, Amount(9)), ZeroVerdict::NonZero(9));
        // Ambiguity never confirms zero.
        assert_eq!(zero_verdict(0, Unparseable, Absent), ZeroVerdict::Unconfirmed);
        assert_eq!(zero_verdict(0, Absent, Unparseable), ZeroVerdict::Unconfirmed);
    }

    #[test]
    fn wallet_scan_falls_back_to_raw_amount_when_ui_amount_is_null() {
        // Simulate the jsonParsed tokenAmount payload of a scaledUiAmount mint whose
        // uiAmount is null: the scan must derive amount/10^decimals instead of skipping.
        let info = serde_json::json!({
            "mint": "M",
            "tokenAmount": { "uiAmount": null, "amount": "2500000", "decimals": 6 }
        });
        assert_eq!(parse_token_amount(&info), Some(("M".to_string(), 2.5)));
        // And the normal path still works.
        let info2 = serde_json::json!({
            "mint": "M2",
            "tokenAmount": { "uiAmount": 1.25, "amount": "1250000", "decimals": 6 }
        });
        assert_eq!(parse_token_amount(&info2), Some(("M2".to_string(), 1.25)));
    }
}
