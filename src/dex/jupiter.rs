//! Jupiter v6 aggregator integration (self-hosted swap-api).
//!
//! Jupiter is modelled as a synthetic, vault-less edge in the exchange graph. Unlike every
//! other DEX — whose state arrives via gRPC — a Jupiter edge's rate is maintained by a
//! background REST poller (`spawn_poller`) that periodically hits `/quote` and stores an
//! implied marginal rate + price impact on the pool's atomics. The hot path (`get_quote`)
//! only ever reads those atomics, so it stays synchronous and pure like every other DEX.
//!
//! The authoritative route + instructions are fetched once, at submit time, via
//! `/swap-instructions` (`JupiterClient::swap_instructions`) — that's the only place a
//! Jupiter network round-trip enters the submission path, and it lives in the already-async
//! resolver in `main.rs`.
//!
//! ## Atomic field mapping for Jupiter pools
//! | field                | meaning (f64 bits unless noted)            |
//! |----------------------|--------------------------------------------|
//! | `sqrt_price_x64`     | implied marginal rate a→b (out/in)         |
//! | `damm_virtual_price` | implied marginal rate b→a (out/in)         |
//! | `reserve_a`          | price impact fraction at probe, a→b        |
//! | `reserve_b`          | price impact fraction at probe, b→a        |
//! | `a_lp_balance`       | probe amount used by the poller (u64 raw)  |

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Deserialize;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;

use crate::dex::types::{Pool, SwapQuote};
use crate::graph::exchange_graph::ExchangeGraph;

/// Floor on the probe price impact so a deep route reported as ~0 impact doesn't let the
/// ternary search oversize without bound. 1e-6 ⇒ implied reserve ≈ probe × 1e6.
const MIN_PROBE_IMPACT: f64 = 1e-6;

// ─── Sync hot-path quote ────────────────────────────────────────────────────────

/// Synchronous quote read from cached poller state. Approximate by design — the real route
/// is fetched at submit time. Returns `amount_out = 0` until the poller has populated a rate.
pub fn get_quote(pool: &Pool, amount_in: u64, a_to_b: bool) -> SwapQuote {
    if amount_in == 0 {
        return SwapQuote { amount_in, amount_out: 0, fee_amount: 0, price_impact: 0.0, a_to_b };
    }

    let rate_bits = if a_to_b {
        pool.sqrt_price_x64.load(Ordering::Relaxed)
    } else {
        pool.damm_virtual_price.load(Ordering::Relaxed)
    };
    let impact_bits = if a_to_b {
        pool.reserve_a.load(Ordering::Relaxed)
    } else {
        pool.reserve_b.load(Ordering::Relaxed)
    };
    let probe = pool.a_lp_balance.load(Ordering::Relaxed);

    let rate = f64::from_bits(rate_bits);
    if !(rate > 0.0) || !rate.is_finite() || rate_bits == 0 {
        return SwapQuote { amount_in, amount_out: 0, fee_amount: 0, price_impact: 0.0, a_to_b };
    }

    // Derive an implied constant-product reserve from the probe impact, then apply the same
    // size-based impact formula used elsewhere in the bot. This is monotonic and saturating,
    // so the ternary search can't be fooled into oversizing past where the route can fill.
    let base_impact = f64::from_bits(impact_bits).clamp(MIN_PROBE_IMPACT, 0.99);
    let probe = if probe == 0 { amount_in } else { probe };
    let implied_reserve = probe as f64 * (1.0 - base_impact) / base_impact;
    let impact = amount_in as f64 / (implied_reserve + amount_in as f64);

    let amount_out = (amount_in as f64 * rate * (1.0 - impact)) as u64;

    SwapQuote {
        amount_in,
        amount_out,
        fee_amount: 0, // Jupiter fee is embedded in the quoted out-amount
        price_impact: impact,
        a_to_b,
    }
}

// ─── REST client ─────────────────────────────────────────────────────────────────

/// Minimal client for the self-hosted Jupiter swap-api. Hand-rolled on `reqwest` + serde to
/// avoid pulling in `jupiter-swap-api-client` (which inherits its own `solana-sdk` version and
/// would conflict with this crate's pin).
#[derive(Clone)]
pub struct JupiterClient {
    http: reqwest::Client,
    base_url: String,
}

/// Bundle of instructions + ALT addresses returned for one Jupiter hop. One logical hop turns
/// into several instructions (setup + swap + cleanup).
pub struct SwapIxBundle {
    pub instructions: Vec<Instruction>,
    pub alt_addresses: Vec<Pubkey>,
}

#[derive(Debug, Clone)]
pub struct JupiterQuote {
    /// Raw JSON echoed back to /swap-instructions verbatim.
    raw: serde_json::Value,
    pub out_amount: u64,
}

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

#[derive(Deserialize)]
struct SwapInstructionsResponse {
    #[serde(rename = "setupInstructions", default)]
    setup_instructions: Vec<RawInstruction>,
    #[serde(rename = "swapInstruction")]
    swap_instruction: RawInstruction,
    #[serde(rename = "cleanupInstruction", default)]
    cleanup_instruction: Option<RawInstruction>,
    #[serde(rename = "addressLookupTableAddresses", default)]
    address_lookup_table_addresses: Vec<String>,
}

impl RawInstruction {
    fn into_instruction(self) -> Result<Instruction> {
        use std::str::FromStr;
        let program_id = Pubkey::from_str(&self.program_id)
            .with_context(|| format!("bad Jupiter programId: {}", self.program_id))?;
        let accounts = self
            .accounts
            .into_iter()
            .map(|a| {
                let pk = Pubkey::from_str(&a.pubkey)
                    .with_context(|| format!("bad Jupiter account pubkey: {}", a.pubkey))?;
                Ok(AccountMeta {
                    pubkey: pk,
                    is_signer: a.is_signer,
                    is_writable: a.is_writable,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let data = B64.decode(&self.data).context("bad Jupiter instruction data (base64)")?;
        Ok(Instruction { program_id, accounts, data })
    }
}

impl JupiterClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client builds");
        Self { http, base_url: base_url.into() }
    }

    /// GET /quote. `restrict_intermediate_tokens` keeps routes to liquid intermediates, which
    /// also keeps the resulting transaction smaller (fewer accounts) — important for fitting
    /// the flash-loan single-tx under 1232 bytes.
    pub async fn quote(
        &self,
        input_mint: &Pubkey,
        output_mint: &Pubkey,
        amount: u64,
        slippage_bps: u64,
    ) -> Result<JupiterQuote> {
        let url = format!("{}/quote", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&[
                ("inputMint", input_mint.to_string()),
                ("outputMint", output_mint.to_string()),
                ("amount", amount.to_string()),
                ("slippageBps", slippage_bps.to_string()),
                ("restrictIntermediateTokens", "true".to_string()),
            ])
            .send()
            .await
            .context("Jupiter /quote request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!("Jupiter /quote returned HTTP {}", resp.status());
        }
        let raw: serde_json::Value = resp.json().await.context("Jupiter /quote bad JSON")?;
        let out_amount = raw
            .get("outAmount")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .context("Jupiter /quote missing outAmount")?;
        Ok(JupiterQuote { raw, out_amount })
    }

    /// POST /swap-instructions. Echoes the quote back, overriding `otherAmountThreshold` so the
    /// on-chain swap reverts unless it delivers at least the cycle's required `min_out`
    /// (defense in depth on top of the fixed flash-loan repay).
    pub async fn swap_instructions(
        &self,
        mut quote: JupiterQuote,
        user: &Pubkey,
        min_out: u64,
    ) -> Result<SwapIxBundle> {
        // Tighten the threshold: max(Jupiter's own, our cycle requirement).
        let jup_threshold = quote
            .raw
            .get("otherAmountThreshold")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);
        if let Some(obj) = quote.raw.as_object_mut() {
            obj.insert(
                "otherAmountThreshold".to_string(),
                serde_json::Value::String(jup_threshold.max(min_out).to_string()),
            );
        }

        let body = serde_json::json!({
            "quoteResponse": quote.raw,
            "userPublicKey": user.to_string(),
            // We manage SOL wrap/unwrap + compute budget + tip in our own bundle.
            "wrapAndUnwrapSol": false,
            "useSharedAccounts": true,
        });

        let url = format!("{}/swap-instructions", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("Jupiter /swap-instructions request failed")?;
        if !resp.status().is_success() {
            anyhow::bail!("Jupiter /swap-instructions returned HTTP {}", resp.status());
        }
        let parsed: SwapInstructionsResponse =
            resp.json().await.context("Jupiter /swap-instructions bad JSON")?;

        // Order matters: setup (ATA creation etc.) → swap → cleanup. We intentionally drop
        // computeBudgetInstructions and tokenLedgerInstruction — the bundle sets its own.
        let mut instructions = Vec::new();
        for ix in parsed.setup_instructions {
            instructions.push(ix.into_instruction()?);
        }
        instructions.push(parsed.swap_instruction.into_instruction()?);
        if let Some(cleanup) = parsed.cleanup_instruction {
            instructions.push(cleanup.into_instruction()?);
        }

        use std::str::FromStr;
        let alt_addresses = parsed
            .address_lookup_table_addresses
            .iter()
            .filter_map(|s| Pubkey::from_str(s).ok())
            .collect();

        Ok(SwapIxBundle { instructions, alt_addresses })
    }
}

// ─── Self-hosted Metis binary launcher ─────────────────────────────────────────────

/// Launch the self-hosted Metis swap-api binary (jup-ag/metis-binary) as a child process,
/// pointed at the same RPC + Yellowstone gRPC the bot uses. Returns the child handle, which
/// the caller must keep alive for the bot's lifetime — `kill_on_drop` terminates Metis when
/// the handle drops (note: a `panic = "abort"` build skips this, so a hard abort can orphan it).
///
/// Metis serves on its default port 8080; ensure `JUPITER_API_URL` matches. stdout/stderr are
/// inherited so its indexing progress is visible during the ~1-2 min warm-up.
pub fn spawn_metis(
    binary_path: &str,
    binary_key: &str,
    rpc_url: &str,
    grpc_endpoint: &str,
    grpc_token: Option<&str>,
) -> anyhow::Result<tokio::process::Child> {
    use std::process::Stdio;
    let mut cmd = tokio::process::Command::new(binary_path);
    cmd.arg("--binary-key").arg(binary_key)
        .arg("--rpc-url").arg(rpc_url)
        .arg("--yellowstone-grpc-endpoint").arg(grpc_endpoint);
    if let Some(token) = grpc_token.filter(|t| !t.is_empty()) {
        cmd.arg("--yellowstone-grpc-x-token").arg(token);
    }
    cmd.env("RUST_LOG", "info")
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    cmd.spawn()
        .with_context(|| format!("failed to launch Metis binary at {binary_path}"))
}

// ─── Background rate poller ────────────────────────────────────────────────────────

/// Spawn the Jupiter rate poller. Every `interval_ms`, for each Jupiter pool, probe both swap
/// directions at `probe_lamports` (raw base units of the input mint), store the implied rate +
/// impact on the pool's atomics, and refresh the graph edge. A direction that has no route is
/// stored as rate 0 → `update_pool` removes that edge.
pub fn spawn_poller(
    client: JupiterClient,
    pools: Vec<Arc<Pool>>,
    graph: Arc<ExchangeGraph>,
    probe_lamports: u64,
    interval_ms: u64,
    slippage_bps: u64,
) {
    if pools.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let interval = Duration::from_millis(interval_ms.max(50));
        loop {
            for pool in &pools {
                let (ab_rate, ab_impact) =
                    probe_direction(&client, &pool.token_a, &pool.token_b, probe_lamports, slippage_bps).await;
                let (ba_rate, ba_impact) =
                    probe_direction(&client, &pool.token_b, &pool.token_a, probe_lamports, slippage_bps).await;

                pool.sqrt_price_x64.store(ab_rate.to_bits(), Ordering::Relaxed);
                pool.damm_virtual_price.store(ba_rate.to_bits(), Ordering::Relaxed);
                pool.reserve_a.store(ab_impact.to_bits(), Ordering::Relaxed);
                pool.reserve_b.store(ba_impact.to_bits(), Ordering::Relaxed);
                pool.a_lp_balance.store(probe_lamports, Ordering::Relaxed);
                pool.stamp_update();
                graph.update_pool(pool);
            }
            tokio::time::sleep(interval).await;
        }
    });
}

/// Probe one direction. Returns (rate = out/in, price_impact_fraction). On any failure returns
/// (0.0, 0.0) so the edge is removed.
async fn probe_direction(
    client: &JupiterClient,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
    probe: u64,
    slippage_bps: u64,
) -> (f64, f64) {
    match client.quote(input_mint, output_mint, probe, slippage_bps).await {
        Ok(q) if probe > 0 && q.out_amount > 0 => {
            let rate = q.out_amount as f64 / probe as f64;
            let impact = q
                .raw
                .get("priceImpactPct")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.0)
                .abs();
            (rate, impact)
        }
        _ => (0.0, 0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dex::types::Pool;

    fn jup_pool() -> Arc<Pool> {
        Pool::new_jupiter(
            "So11111111111111111111111111111111111111112".parse().unwrap(),
            "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v".parse().unwrap(),
        )
    }

    /// Store rate + impact the way the poller does (f64 bits), at a given probe size.
    fn set_state(pool: &Pool, rate_ab: f64, impact_ab: f64, probe: u64) {
        pool.sqrt_price_x64.store(rate_ab.to_bits(), Ordering::Relaxed);
        pool.reserve_a.store(impact_ab.to_bits(), Ordering::Relaxed);
        pool.a_lp_balance.store(probe, Ordering::Relaxed);
    }

    #[test]
    fn unpolled_pool_quotes_zero() {
        let pool = jup_pool();
        let q = get_quote(&pool, 1_000_000_000, true);
        assert_eq!(q.amount_out, 0);
    }

    #[test]
    fn impact_is_monotonic_in_size() {
        let pool = jup_pool();
        set_state(&pool, 150.0, 0.001, 1_000_000_000); // rate 150, 0.1% impact @ 1 SOL probe
        let small = get_quote(&pool, 100_000_000, true).price_impact;
        let large = get_quote(&pool, 10_000_000_000, true).price_impact;
        assert!(large > small, "impact must rise with size: {small} vs {large}");
        assert!(large < 1.0, "impact saturates below 1.0");
    }

    #[test]
    fn larger_size_yields_worse_effective_rate() {
        let pool = jup_pool();
        set_state(&pool, 150.0, 0.005, 1_000_000_000);
        let eff = |amt: u64| get_quote(&pool, amt, true).amount_out as f64 / amt as f64;
        assert!(eff(10_000_000_000) < eff(100_000_000), "effective rate degrades with size");
    }

    #[test]
    fn near_zero_impact_is_floored_not_unbounded() {
        let pool = jup_pool();
        set_state(&pool, 1.0, 0.0, 1_000_000_000); // reported 0 impact
        // With the MIN_PROBE_IMPACT floor, a huge trade still shows meaningful impact.
        let impact = get_quote(&pool, 1_000_000_000_000, true).price_impact;
        assert!(impact > 0.0, "floored impact must be positive");
    }
}
