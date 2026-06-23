//! Kamino `klend` integration for the on-chain pairs trader (Phase 2b).
//!
//! **Approach = BUY (sidecar).** Rather than hand-roll klend's long, version-specific
//! Anchor account lists in Rust (error-prone and unverifiable without the live IDL),
//! this module is a thin HTTP client to the `klend-builder` Node sidecar, which uses
//! the official `@kamino-finance/klend-sdk` to derive every account, PDA and refresh
//! ordering. The sidecar returns instructions as JSON; the bot assembles the tx,
//! signs and submits — exactly how `dex::jupiter` consumes `/swap-instructions`.
//!
//! Sidecar endpoints (see `klend-builder/src/index.ts`):
//! | method | path | returns |
//! |---|---|---|
//! | GET  | `/market` | per-reserve borrow APY / liq threshold / available liquidity |
//! | GET  | `/obligation?owner=<pubkey>` | the user's vanilla-obligation health |
//! | POST | `/build/{deposit\|borrow\|repay\|withdraw}` `{owner,symbol,amount}` | grouped ix JSON |
//!
//! **Status (Phase 2b.2):** the client + JSON→`Instruction` parsing are implemented
//! and unit-tested here (the wire contract with the sidecar is verified offline).
//! End-to-end correctness — that the SDK builds *working* klend txns — requires
//! running the sidecar against a live RPC + market with a funded wallet. That is
//! Phase 2b.3 (devnet / tiny real funds) and cannot be verified offline.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Deserialize;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

/// Kamino Lending (`klend`) program id — mainnet.
pub const KLEND_PROGRAM_ID: &str = "KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD";
/// Staging deployment on mainnet (for dry runs against the staging market).
pub const KLEND_STAGING_PROGRAM_ID: &str = "SLendK7ySfcEzyaFqy93gDnD3RtrpXJcnRwb6zFHJSh";
/// The resolved Kamino "xStocks Market" lending market (mainnet) — all four xStocks +
/// USDC are reserves here. Default for the auto-launched sidecar; see
/// `docs/pairs-trader-runbook.md`.
pub const XSTOCKS_MARKET: &str = "5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua";

pub fn program_id() -> Pubkey {
    KLEND_PROGRAM_ID.parse().expect("valid klend program id")
}

/// The lending actions the sidecar can build, mapped to its `/build/{action}` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KlendAction {
    Deposit,
    Borrow,
    Repay,
    Withdraw,
}

impl KlendAction {
    pub fn as_str(self) -> &'static str {
        match self {
            KlendAction::Deposit => "deposit",
            KlendAction::Borrow => "borrow",
            KlendAction::Repay => "repay",
            KlendAction::Withdraw => "withdraw",
        }
    }
}

/// One borrowable/collateral reserve in a klend market, with the fields the pairs
/// trader's risk layer needs. Populated from the sidecar `/market` read.
#[derive(Debug, Clone)]
pub struct ReserveInfo {
    /// The reserve account pubkey.
    pub reserve: Pubkey,
    pub liquidity_mint: Pubkey,
    /// Liquidation threshold (0–1) — used by `sim::estimate_health_factor`.
    pub liq_threshold: f64,
    /// Current borrow APY in percent — gated against `PAIRS_MAX_BORROW_APY_PCT`.
    pub borrow_apy_pct: f64,
    /// Available liquidity to borrow, in whole token units (raw ÷ 10^decimals).
    pub available_liquidity: f64,
    /// Borrow cap in whole token units (raw ÷ 10^decimals). 0 ⇒ borrowing disabled.
    pub borrow_cap: f64,
    /// Whether this reserve can be borrowed at all (cap > 0). GOOGLx in the xStocks
    /// market is `false` (collateral-only) — the pairs trader must not short it.
    pub borrowable: bool,
}

/// A loaded klend market: the program, the market address, and its reserves keyed by
/// token symbol (e.g. "NVDAx" → its reserve).
#[derive(Debug, Clone)]
pub struct KaminoCtx {
    pub program_id: Pubkey,
    pub market: Pubkey,
    pub reserves: HashMap<String, ReserveInfo>,
}

/// Health snapshot of a user's obligation, from the sidecar `/obligation` read.
#[derive(Debug, Clone)]
pub struct ObligationHealth {
    pub address: Option<String>,
    /// Total deposited value (market units, e.g. USD).
    pub user_total_deposit: f64,
    /// Total borrowed value (market units).
    pub user_total_borrow: f64,
    /// Max value the obligation may borrow.
    pub borrow_limit: f64,
    /// Current loan-to-value = borrowed ÷ deposited.
    pub loan_to_value: f64,
    /// LTV at which the obligation is liquidatable (collateral-weighted threshold).
    pub liquidation_ltv: f64,
    pub net_account_value: f64,
}

impl ObligationHealth {
    /// Health factor ≈ `liquidation_ltv / loan_to_value` (> 1 is safe; ∞ when there is
    /// no debt). Cross-check against `sim::estimate_health_factor`. VERIFY the
    /// `loanToValue`/`liquidationLtv` semantics against the SDK on first live run.
    pub fn health_factor(&self) -> f64 {
        if self.loan_to_value <= 0.0 {
            f64::INFINITY
        } else {
            self.liquidation_ltv / self.loan_to_value
        }
    }
}

// ─── Sidecar wire types (mirror klend-builder/src/index.ts JSON) ────────────────────

#[derive(Deserialize)]
struct RawAccount {
    pubkey: String,
    #[serde(rename = "isSigner")]
    is_signer: bool,
    #[serde(rename = "isWritable")]
    is_writable: bool,
}

#[derive(Deserialize)]
struct RawInstruction {
    #[serde(rename = "programId")]
    program_id: String,
    accounts: Vec<RawAccount>,
    data: String,
}

impl RawInstruction {
    fn into_instruction(self) -> Result<Instruction> {
        use std::str::FromStr;
        let program_id = Pubkey::from_str(&self.program_id)
            .with_context(|| format!("bad klend programId: {}", self.program_id))?;
        let accounts = self
            .accounts
            .into_iter()
            .map(|a| {
                let pk = Pubkey::from_str(&a.pubkey)
                    .with_context(|| format!("bad klend account pubkey: {}", a.pubkey))?;
                Ok(AccountMeta { pubkey: pk, is_signer: a.is_signer, is_writable: a.is_writable })
            })
            .collect::<Result<Vec<_>>>()?;
        let data = B64.decode(&self.data).context("bad klend instruction data (base64)")?;
        Ok(Instruction { program_id, accounts, data })
    }
}

#[derive(Deserialize)]
struct BuildResponse {
    #[serde(rename = "computeBudgetIxs", default)]
    _compute_budget_ixs: Vec<RawInstruction>,
    #[serde(rename = "setupIxs", default)]
    setup_ixs: Vec<RawInstruction>,
    #[serde(rename = "inBetweenIxs", default)]
    in_between_ixs: Vec<RawInstruction>,
    #[serde(rename = "lendingIxs", default)]
    lending_ixs: Vec<RawInstruction>,
    #[serde(rename = "cleanupIxs", default)]
    cleanup_ixs: Vec<RawInstruction>,
    #[serde(default)]
    error: Option<String>,
}

impl BuildResponse {
    /// Flatten in execution order. `computeBudgetIxs` is dropped — the bot sets its
    /// own compute budget when it assembles the transaction (same as the Jupiter path).
    fn into_instructions(self) -> Result<Vec<Instruction>> {
        if let Some(e) = self.error {
            anyhow::bail!("klend /build error: {e}");
        }
        let mut out = Vec::new();
        for group in [self.setup_ixs, self.in_between_ixs, self.lending_ixs, self.cleanup_ixs] {
            for ix in group {
                out.push(ix.into_instruction()?);
            }
        }
        Ok(out)
    }
}

#[derive(Deserialize)]
struct RawReserve {
    address: String,
    mint: String,
    #[serde(rename = "borrowApy")]
    borrow_apy: Option<f64>,
    #[serde(rename = "liqThreshold")]
    liq_threshold: Option<f64>,
    #[serde(rename = "availableLiquidityRaw")]
    available_liquidity_raw: Option<f64>,
    decimals: Option<f64>,
    #[serde(rename = "borrowCap")]
    borrow_cap: Option<f64>,
    borrowable: Option<bool>,
}

#[derive(Deserialize)]
struct MarketResponse {
    #[serde(default)]
    reserves: HashMap<String, RawReserve>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ObligationResponse {
    exists: bool,
    address: Option<String>,
    #[serde(rename = "userTotalDeposit")]
    user_total_deposit: Option<f64>,
    #[serde(rename = "userTotalBorrow")]
    user_total_borrow: Option<f64>,
    #[serde(rename = "borrowLimit")]
    borrow_limit: Option<f64>,
    #[serde(rename = "loanToValue")]
    loan_to_value: Option<f64>,
    #[serde(rename = "liquidationLtv")]
    liquidation_ltv: Option<f64>,
    #[serde(rename = "netAccountValue")]
    net_account_value: Option<f64>,
    #[serde(default)]
    error: Option<String>,
}

// ─── Thin HTTP client to the klend-builder sidecar ──────────────────────────────────

/// Client for the local `klend-builder` sidecar. Hand-rolled on `reqwest` + serde,
/// mirroring `dex::jupiter::JupiterClient`.
#[derive(Clone)]
pub struct KlendClient {
    http: reqwest::Client,
    base_url: String,
}

impl KlendClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("reqwest client builds");
        Self { http, base_url: base_url.into() }
    }

    /// GET /market → reserves keyed by token symbol.
    pub async fn market(&self) -> Result<HashMap<String, ReserveInfo>> {
        use std::str::FromStr;
        let url = format!("{}/market", self.base_url);
        let resp = self.http.get(&url).send().await.context("klend /market request failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("klend /market returned HTTP {}", resp.status());
        }
        let parsed: MarketResponse = resp.json().await.context("klend /market bad JSON")?;
        if let Some(e) = parsed.error {
            anyhow::bail!("klend /market error: {e}");
        }
        let mut out = HashMap::new();
        for (symbol, r) in parsed.reserves {
            let reserve = Pubkey::from_str(&r.address)
                .with_context(|| format!("bad reserve address: {}", r.address))?;
            let liquidity_mint = Pubkey::from_str(&r.mint)
                .with_context(|| format!("bad reserve mint: {}", r.mint))?;
            let scale = 10f64.powf(r.decimals.unwrap_or(0.0));
            let raw = r.available_liquidity_raw.unwrap_or(0.0);
            let cap_raw = r.borrow_cap.unwrap_or(0.0);
            let scaled = |x: f64| if scale > 0.0 { x / scale } else { x };
            out.insert(
                symbol,
                ReserveInfo {
                    reserve,
                    liquidity_mint,
                    liq_threshold: r.liq_threshold.unwrap_or(0.0),
                    // sidecar returns APY as a fraction; ×100 → percent (verified live: fraction).
                    borrow_apy_pct: r.borrow_apy.unwrap_or(0.0) * 100.0,
                    available_liquidity: scaled(raw),
                    borrow_cap: scaled(cap_raw),
                    // trust the sidecar's flag; fall back to cap>0 if absent.
                    borrowable: r.borrowable.unwrap_or(cap_raw > 0.0),
                },
            );
        }
        Ok(out)
    }

    /// GET /obligation?owner=… → health snapshot, or `None` if the user has no obligation yet.
    pub async fn obligation_health(&self, owner: &Pubkey) -> Result<Option<ObligationHealth>> {
        let url = format!("{}/obligation", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[("owner", owner.to_string())])
            .send()
            .await
            .context("klend /obligation request failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("klend /obligation returned HTTP {}", resp.status());
        }
        let parsed: ObligationResponse =
            resp.json().await.context("klend /obligation bad JSON")?;
        if let Some(e) = parsed.error {
            anyhow::bail!("klend /obligation error: {e}");
        }
        if !parsed.exists {
            return Ok(None);
        }
        Ok(Some(ObligationHealth {
            address: parsed.address,
            user_total_deposit: parsed.user_total_deposit.unwrap_or(0.0),
            user_total_borrow: parsed.user_total_borrow.unwrap_or(0.0),
            borrow_limit: parsed.borrow_limit.unwrap_or(0.0),
            loan_to_value: parsed.loan_to_value.unwrap_or(0.0),
            liquidation_ltv: parsed.liquidation_ltv.unwrap_or(0.0),
            net_account_value: parsed.net_account_value.unwrap_or(0.0),
        }))
    }

    /// POST /build/{action} → the klend instructions, flattened, ready for the bot to
    /// assemble + sign + submit. `amount_base_units` is raw token base units (lamports
    /// of the token), serialized as a string the way the sidecar expects.
    pub async fn build(
        &self,
        action: KlendAction,
        owner: &Pubkey,
        symbol: &str,
        amount_base_units: u64,
    ) -> Result<Vec<Instruction>> {
        let url = format!("{}/build/{}", self.base_url, action.as_str());
        let body = serde_json::json!({
            "owner": owner.to_string(),
            "symbol": symbol,
            "amount": amount_base_units.to_string(),
        });
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("klend /build/{} request failed", action.as_str()))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            anyhow::bail!("klend /build/{} returned HTTP {status}: {txt}", action.as_str());
        }
        let parsed: BuildResponse = resp.json().await.context("klend /build bad JSON")?;
        parsed.into_instructions()
    }
}

/// Load the xStocks market + its reserves via the sidecar (borrow APY, liq threshold,
/// available liquidity per reserve).
pub async fn load_market(sidecar_url: &str, market: &Pubkey) -> Result<KaminoCtx> {
    let reserves = KlendClient::new(sidecar_url).market().await?;
    Ok(KaminoCtx { program_id: program_id(), market: *market, reserves })
}

/// Read the live health factor of an owner's obligation via the sidecar. Errors if the
/// owner has no obligation yet (deposit first).
pub async fn read_obligation_health(sidecar_url: &str, owner: &Pubkey) -> Result<f64> {
    match KlendClient::new(sidecar_url).obligation_health(owner).await? {
        Some(h) => Ok(h.health_factor()),
        None => anyhow::bail!("no klend obligation for owner {owner}"),
    }
}

// ─── Sidecar process management (mirrors dex::jupiter::spawn_metis) ──────────────────

/// Parse the port out of a sidecar base URL (`"http://127.0.0.1:8181"` → `8181`).
pub fn sidecar_port(base_url: &str) -> Option<u16> {
    base_url.rsplit(':').next()?.split('/').next()?.trim().parse().ok()
}

/// Auto-launch the `klend-builder` Node sidecar as a child process — the klend analogue of
/// `spawn_metis`. Spawns the `tsx` server binary **directly** (not `npm start`, which forks
/// a child that can orphan and hold the port), so `kill_on_drop` / `kill()` reliably stop
/// the real server. Requires `npm install` to have populated `node_modules`. stdout/stderr
/// are inherited so the sidecar's logs appear inline.
pub fn spawn_klend_sidecar(
    builder_dir: &str,
    rpc_url: &str,
    market: &str,
    port: u16,
) -> Result<tokio::process::Child> {
    use std::process::Stdio;
    let tsx = std::path::Path::new(builder_dir).join("node_modules/.bin/tsx");
    if !tsx.exists() {
        anyhow::bail!(
            "klend-builder not installed ({} missing) — run `npm install` in {builder_dir}",
            tsx.display()
        );
    }
    let mut cmd = tokio::process::Command::new(&tsx);
    cmd.arg("src/index.ts")
        .current_dir(builder_dir)
        .env("RPC_URL", rpc_url)
        .env("KLEND_MARKET", market)
        .env("KLEND_BUILDER_PORT", port.to_string())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    cmd.spawn()
        .with_context(|| format!("failed to launch klend-builder via {}", tsx.display()))
}

/// Poll the sidecar `/health` until it responds 200 or `timeout_secs` elapses.
pub async fn wait_until_ready(base_url: &str, timeout_secs: u64) -> bool {
    let http = reqwest::Client::new();
    let url = format!("{base_url}/health");
    for _ in 0..(timeout_secs * 2).max(1) {
        if let Ok(resp) = http.get(&url).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn program_id_parses() {
        assert_eq!(program_id(), Pubkey::from_str(KLEND_PROGRAM_ID).unwrap());
    }

    #[test]
    fn sidecar_port_parses_url() {
        assert_eq!(sidecar_port("http://127.0.0.1:8181"), Some(8181));
        assert_eq!(sidecar_port("http://localhost:9000/"), Some(9000));
        assert_eq!(sidecar_port("http://127.0.0.1"), None); // no port → None
    }

    #[test]
    fn action_paths_are_stable() {
        assert_eq!(KlendAction::Deposit.as_str(), "deposit");
        assert_eq!(KlendAction::Borrow.as_str(), "borrow");
        assert_eq!(KlendAction::Repay.as_str(), "repay");
        assert_eq!(KlendAction::Withdraw.as_str(), "withdraw");
    }

    /// The sidecar wire contract: grouped instruction JSON deserializes + flattens into
    /// `solana_sdk::Instruction`s in setup→inBetween→lending→cleanup order, with
    /// base64 data decoded and account flags preserved. `computeBudgetIxs` is dropped.
    #[test]
    fn build_response_parses_and_flattens() {
        // "AQID" = base64([1,2,3]); "BAUG" = base64([4,5,6]); "Bw==" = base64([7]).
        let json = r#"{
            "action": "borrow",
            "computeBudgetIxs": [
                {"programId":"ComputeBudget111111111111111111111111111111","accounts":[],"data":"Bw=="}
            ],
            "setupIxs": [
                {"programId":"KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD",
                 "accounts":[{"pubkey":"So11111111111111111111111111111111111111112","isSigner":false,"isWritable":true}],
                 "data":"AQID"}
            ],
            "lendingIxs": [
                {"programId":"KLend2g3cP87fffoy8q1mQqGKjrxjC8boSyAYavgmjD",
                 "accounts":[{"pubkey":"So11111111111111111111111111111111111111112","isSigner":true,"isWritable":false}],
                 "data":"BAUG"}
            ]
        }"#;
        let parsed: BuildResponse = serde_json::from_str(json).unwrap();
        let ixs = parsed.into_instructions().unwrap();
        assert_eq!(ixs.len(), 2, "compute-budget ix dropped; setup + lending kept");
        // setup ix first
        assert_eq!(ixs[0].program_id, program_id());
        assert_eq!(ixs[0].data, vec![1, 2, 3]);
        assert!(ixs[0].accounts[0].is_writable && !ixs[0].accounts[0].is_signer);
        // lending ix second, flags preserved
        assert_eq!(ixs[1].data, vec![4, 5, 6]);
        assert!(ixs[1].accounts[0].is_signer && !ixs[1].accounts[0].is_writable);
    }

    #[test]
    fn build_response_surfaces_sidecar_error() {
        let json = r#"{"error":"no reserve for symbol 'XYZ'"}"#;
        let parsed: BuildResponse = serde_json::from_str(json).unwrap();
        assert!(parsed.into_instructions().is_err());
    }

    #[test]
    fn obligation_health_factor_math() {
        let h = ObligationHealth {
            address: None,
            user_total_deposit: 1000.0,
            user_total_borrow: 300.0,
            borrow_limit: 700.0,
            loan_to_value: 0.30,
            liquidation_ltv: 0.60,
            net_account_value: 700.0,
        };
        assert!((h.health_factor() - 2.0).abs() < 1e-9, "0.60/0.30 = 2.0");
        let no_debt = ObligationHealth { loan_to_value: 0.0, ..h.clone() };
        assert!(no_debt.health_factor().is_infinite(), "no debt → infinite HF");
    }

    #[test]
    fn market_response_parses() {
        let json = r#"{
            "market": "7u3HeHxYDLhnCoErrtycNokbQYbWGzLs6Js6CCnGgPx7",
            "reserves": {
                "USDC": {
                    "address":"D6q6wuQSrifJKZYpR1M8R4YawnLDtDsMmWM1NbBmgJ59",
                    "mint":"EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
                    "borrowApy":0.12,"liqThreshold":0.85,
                    "availableLiquidityRaw":5000000000.0,"decimals":6.0,
                    "borrowCap":16000000000000.0,"borrowable":true
                },
                "GOOGLx": {
                    "address":"4wg6rEkGgHaEuxMduP46C1xFZ24Lnp5YgdNkZAHxFzsN",
                    "mint":"XsCPL9dNWBMvFtTmwcCA5v3xWPSMEBCszbQdiLLq6aN",
                    "borrowApy":0.034,"liqThreshold":0.70,
                    "availableLiquidityRaw":612010066128.0,"decimals":8.0,
                    "borrowCap":0.0,"borrowable":false
                }
            }
        }"#;
        let parsed: MarketResponse = serde_json::from_str(json).unwrap();
        // mirror KlendClient::market()'s unit conversions
        let r = &parsed.reserves["USDC"];
        assert!((r.borrow_apy.unwrap() * 100.0 - 12.0).abs() < 1e-9, "fraction→percent");
        let scale = 10f64.powf(r.decimals.unwrap());
        assert!((r.available_liquidity_raw.unwrap() / scale - 5000.0).abs() < 1e-6, "raw→units");
        assert_eq!(parsed.reserves["USDC"].borrowable, Some(true));
        assert_eq!(parsed.reserves["GOOGLx"].borrowable, Some(false), "GOOGLx collateral-only");
        assert_eq!(parsed.reserves["GOOGLx"].borrow_cap, Some(0.0));
    }
}
