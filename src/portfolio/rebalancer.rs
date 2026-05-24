//! Auto-rebalancer — executes one mean-reversion swap per tick when all
//! gates are satisfied. Called from the watcher's 60-second loop.
//!
//! Decision pipeline (short-circuit on first gate that fails):
//!   1. Master switch (`ENABLE_AUTO_REBALANCE`)
//!   2. Recovery gate (live total ≥ last snapshot total)
//!   3. Daily cap (`REBALANCE_MAX_SWAPS_PER_DAY` in 24h)
//!   4. Signal generation (analyzer::generate_rebalance_signals)
//!   5. Per-signal: hold-cooldown, Jupiter quote, cost gate
//!   6. Snapshot the portfolio (append-only, restart-safe)
//!   7. Sign + submit + confirm via Solana RPC
//!   8. Persist execution + send email
//!
//! See docs/portfolio/auto-rebalance.md for the architecture overview.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use reqwest::Client;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::VersionedTransaction;
use tracing::{error, info, warn};

use super::analyzer::{
    self, RebalanceSignal, RebalanceSignalConfig, RiskReport,
};
use super::emailer;
use super::history::PriceSnapshot;
use super::jupiter;
use super::rebalancer_actions::{self, Action, ActionKind};
use super::rebalancer_snapshots::{self, PlannedAction, PortfolioSnapshot};
use super::rebalancer_state::{self, ExecutionRecord, HaltRecord};
use super::scanner;
use super::{Portfolio, PortfolioConfig};

/// Native SOL fee multiplier — base fee (~5_000 lamports) per signature.
const BASE_FEE_LAMPORTS: u64 = 5_000;

/// Maximum wall-clock to wait for a tx to confirm before giving up.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(45);

/// Decision input gathered by the watcher each tick. Borrowed references keep
/// allocations down; the rebalancer never mutates them.
pub struct RebalanceContext<'a> {
    pub cfg: &'a PortfolioConfig,
    pub portfolio: &'a Portfolio,
    pub prices_usd: &'a HashMap<String, f64>,
    pub history: &'a VecDeque<PriceSnapshot>,
    pub risk: &'a RiskReport,
    pub http: &'a Client,
    pub eur_rate: f64,
    pub decimals: &'a HashMap<String, u8>,
}

#[derive(Debug)]
pub struct ExecutedSwap {
    pub record: ExecutionRecord,
    pub snapshot: PortfolioSnapshot,
    pub dry_run: bool,
}

pub async fn maybe_rebalance(ctx: &RebalanceContext<'_>) -> Result<Option<ExecutedSwap>> {
    // 1. Master switch.
    if !ctx.cfg.enable_auto_rebalance {
        return Ok(None);
    }

    let state_path = Path::new(&ctx.cfg.rebalancer_state_path);
    let snapshots_path = Path::new(&ctx.cfg.rebalancer_snapshots_path);
    let halt_path = Path::new(&ctx.cfg.rebalancer_halt_path);

    // 1b. Loss-halt circuit breaker — once tripped, every tick exits silently
    // until the user manually deletes the halt file.
    if let Some(halt) = rebalancer_state::read_halt(halt_path)
        .unwrap_or_else(|e| { warn!("rebalancer: halt-file unreadable ({e})"); None })
    {
        info!(
            "rebalancer: HALTED (since unix {}, deficit €{:.2}); delete {} to re-arm",
            halt.ts, halt.deficit_eur, halt_path.display(),
        );
        return Ok(None);
    }

    let exec_log = rebalancer_state::load(state_path)
        .unwrap_or_else(|e| {
            warn!("rebalancer: state file unreadable, treating as empty: {e}");
            Default::default()
        });

    // 2. Recovery gate — must clear before anything else (per design spec).
    if ctx.cfg.rebalance_require_recovery {
        if let Some(baseline) = rebalancer_snapshots::latest(snapshots_path)
            .unwrap_or_else(|e| { warn!("rebalancer: cannot read baseline ({e})"); None })
        {
            let live_eur = current_portfolio_value_eur(ctx);
            if live_eur < baseline.total_eur {
                let deficit = baseline.total_eur - live_eur;
                let now_ts = unix_now();
                let age_secs = (now_ts - baseline.ts).max(0);
                let age_days = age_secs as f64 / 86_400.0;
                let halt_secs = (ctx.cfg.rebalance_loss_halt_days as i64) * 86_400;

                if age_secs >= halt_secs {
                    // Persistent loss past the wait period → trip the circuit breaker.
                    // Write the marker first (idempotent — the early gate above will
                    // block every future tick), then fire the one-time email.
                    let halt = HaltRecord {
                        ts: now_ts,
                        reason: format!(
                            "portfolio still €{:.2} below snapshot after {:.1}d (limit {}d)",
                            deficit, age_days, ctx.cfg.rebalance_loss_halt_days,
                        ),
                        snapshot_ts: baseline.ts,
                        snapshot_total_eur: baseline.total_eur,
                        current_total_eur: live_eur,
                        deficit_eur: deficit,
                        age_days,
                    };
                    if let Err(e) = rebalancer_state::write_halt(halt_path, &halt) {
                        error!("rebalancer: could not write halt marker: {e:#}");
                    } else {
                        error!(
                            "rebalancer: AUTO-HALT — portfolio €{:.2} < snapshot €{:.2} (deficit €{:.2}) for {:.1}d",
                            live_eur, baseline.total_eur, deficit, age_days,
                        );
                        log_action(ctx, now_ts, ActionKind::HaltTriggered {
                            deficit_eur: deficit,
                            snapshot_total_eur: baseline.total_eur,
                            current_total_eur: live_eur,
                            snapshot_age_days: age_days,
                        });
                        send_halt_email(ctx, &halt).await;
                    }
                    return Ok(None);
                }

                let days_until_halt = (halt_secs - age_secs) as f64 / 86_400.0;
                info!(
                    "rebalancer: waiting for recovery (€{:.2} < snapshot €{:.2}, deficit €{:.2}, age {:.1}d / halt at {}d)",
                    live_eur, baseline.total_eur, deficit,
                    age_days, ctx.cfg.rebalance_loss_halt_days,
                );
                log_action(ctx, now_ts, ActionKind::RecoveryWait {
                    deficit_eur: deficit,
                    snapshot_total_eur: baseline.total_eur,
                    current_total_eur: live_eur,
                    snapshot_age_days: age_days,
                    days_until_halt,
                });
                return Ok(None);
            }
        }
    }

    // 3. Daily cap.
    let now_ts = unix_now();
    let used = rebalancer_state::count_last_24h(&exec_log, now_ts);
    if used >= ctx.cfg.rebalance_max_swaps_per_day as usize {
        info!(
            "rebalancer: daily cap reached ({}/{})",
            used, ctx.cfg.rebalance_max_swaps_per_day
        );
        return Ok(None);
    }

    // 4. Signals.
    let signal_cfg = RebalanceSignalConfig {
        lookback_days: ctx.cfg.rebalance_lookback_days,
        extreme_window_hours: ctx.cfg.rebalance_extreme_window_hours,
        reversal_window_min: ctx.cfg.rebalance_reversal_window_min,
        reversal_pct: ctx.cfg.rebalance_reversal_pct,
    };
    let signals = analyzer::generate_rebalance_signals(
        ctx.history,
        ctx.portfolio,
        ctx.risk,
        &signal_cfg,
    );
    if signals.is_empty() {
        return Ok(None);
    }
    info!("rebalancer: {} candidate signal(s)", signals.len());

    // 5. Try each signal in priority order until one passes every gate.
    for signal in signals {
        match evaluate_and_execute(ctx, &exec_log, &signal, now_ts).await {
            Ok(Some(swap)) => return Ok(Some(swap)),
            Ok(None) => continue,
            Err(e) => {
                error!("rebalancer: signal {}→{} failed: {e:#}", signal.sell_symbol, signal.buy_symbol);
                continue;
            }
        }
    }
    Ok(None)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Best-effort write to the action log. Failures are logged but never bubble
/// up — losing a single audit line must not abort a swap.
fn log_action(ctx: &RebalanceContext<'_>, ts: i64, kind: ActionKind) {
    let action = Action { ts, kind };
    let path = Path::new(&ctx.cfg.rebalancer_actions_path);
    if let Err(e) = rebalancer_actions::append(path, &action) {
        warn!("rebalancer: action log write failed: {e:#}");
    }
}

async fn send_halt_email(ctx: &RebalanceContext<'_>, halt: &HaltRecord) {
    let subject = format!(
        "[portfolio-watcher] AUTO-REBALANCE HALTED — deficit €{:.2}",
        halt.deficit_eur,
    );
    let mut body = String::new();
    body.push_str("AUTO-REBALANCE HALTED\n");
    body.push_str(&"=".repeat(40));
    body.push('\n');
    body.push_str(&format!("Triggered at unix {} ({})\n", halt.ts, iso_ts(halt.ts)));
    body.push_str(&format!("Reason: {}\n\n", halt.reason));
    body.push_str("Snapshot baseline\n-----------------\n");
    body.push_str(&format!("Taken at unix {} ({})\n", halt.snapshot_ts, iso_ts(halt.snapshot_ts)));
    body.push_str(&format!("Snapshot total: €{:.2}\n", halt.snapshot_total_eur));
    body.push_str(&format!("Current total:  €{:.2}\n", halt.current_total_eur));
    body.push_str(&format!("Deficit:        €{:.2}\n", halt.deficit_eur));
    body.push_str(&format!("Age:            {:.1} days\n\n", halt.age_days));
    body.push_str("What this means\n---------------\n");
    body.push_str("The strategy expected mean-reversion within ");
    body.push_str(&format!("{} days but the portfolio is still underwater. ", ctx.cfg.rebalance_loss_halt_days));
    body.push_str("Auto-rebalance has been disabled to prevent further losses. ");
    body.push_str("Price tracking, risk reporting, and email alerts continue as normal.\n\n");
    body.push_str("Re-arm\n------\n");
    body.push_str(&format!("Delete the halt file to resume trading: {}\n", ctx.cfg.rebalancer_halt_path));
    body.push_str("Consider widening the universe, tightening signal filters, or pausing\n");
    body.push_str("the strategy entirely (ENABLE_AUTO_REBALANCE=false) before re-arming.\n");

    match emailer::send_alert(ctx.cfg, &subject, &body).await {
        Ok(true)  => info!("rebalancer: halt email sent"),
        Ok(false) => warn!("rebalancer: halt email skipped (SMTP creds missing)"),
        Err(e)    => error!("rebalancer: halt email failed: {e:#}"),
    }
}

fn current_portfolio_value_eur(ctx: &RebalanceContext<'_>) -> f64 {
    let mut total = 0.0;
    if let Some(&sol) = ctx.prices_usd.get("SOL") {
        total += sol * ctx.eur_rate * ctx.portfolio.sol_amount;
    }
    for t in &ctx.portfolio.tokens {
        let key = if ctx.prices_usd.contains_key(&t.mint) { &t.mint } else { &t.symbol };
        let px = ctx.prices_usd.get(key).copied().unwrap_or(0.0) * ctx.eur_rate;
        total += px * t.amount;
    }
    total
}

async fn evaluate_and_execute(
    ctx: &RebalanceContext<'_>,
    exec_log: &rebalancer_state::ExecutionLog,
    signal: &RebalanceSignal,
    now_ts: i64,
) -> Result<Option<ExecutedSwap>> {
    // Audit: the signal made it past every top-level gate.
    log_action(ctx, now_ts, ActionKind::ConsideredSignal {
        sell: signal.sell_symbol.clone(),
        buy: signal.buy_symbol.clone(),
        sell_value_eur: signal.sell_value_eur,
        buy_value_eur: signal.buy_value_eur,
        sell_decline_pct: signal.sell_decline_pct,
        buy_rise_pct: signal.buy_rise_pct,
    });

    // 5a. Minimum-position gate: both legs must hold at least the configured
    // EUR amount. Filters dust and prevents the rebalancer from churning trivial
    // positions where cost-as-fraction-of-value blows past the budget.
    let min_eur = ctx.cfg.rebalance_min_position_eur;
    if signal.sell_value_eur < min_eur || signal.buy_value_eur < min_eur {
        info!(
            "rebalancer: {}→{} skipped — position floor (sell €{:.2}, buy €{:.2}, min €{:.2})",
            signal.sell_symbol, signal.buy_symbol,
            signal.sell_value_eur, signal.buy_value_eur, min_eur,
        );
        log_action(ctx, now_ts, ActionKind::SkipMinPosition {
            sell: signal.sell_symbol.clone(),
            buy: signal.buy_symbol.clone(),
            sell_value_eur: signal.sell_value_eur,
            buy_value_eur: signal.buy_value_eur,
            min_eur,
        });
        return Ok(None);
    }

    // 5b. Hold-cooldown for same-direction reverse swap.
    if let Some(last) = rebalancer_state::last_execution_of(exec_log, &signal.sell_mint, &signal.buy_mint) {
        let age_secs = now_ts - last.ts;
        let hold_secs = (ctx.cfg.rebalance_hold_days as i64) * 86_400;
        if age_secs < hold_secs {
            let buy_eur_now = current_eur_price(ctx, &signal.buy_symbol, &signal.buy_mint);
            let pnl = rebalancer_state::pnl_pct_since(&last, buy_eur_now);
            if pnl < ctx.cfg.rebalance_take_profit_pct {
                let age_days = age_secs as f64 / 86_400.0;
                info!(
                    "rebalancer: {}→{} on hold (age={:.1}d / {}d, pnl={:+.2}% / {:.1}% required)",
                    signal.sell_symbol, signal.buy_symbol,
                    age_days, ctx.cfg.rebalance_hold_days,
                    pnl, ctx.cfg.rebalance_take_profit_pct,
                );
                log_action(ctx, now_ts, ActionKind::SkipHoldCooldown {
                    sell: signal.sell_symbol.clone(),
                    buy: signal.buy_symbol.clone(),
                    age_days,
                    pnl_pct: pnl,
                    take_profit_pct: ctx.cfg.rebalance_take_profit_pct,
                });
                return Ok(None);
            }
        }
    }

    // 5c. Compute sell amount in raw lamports.
    let sell_dec = lookup_decimals(ctx, &signal.sell_mint, &signal.sell_symbol)?;
    let buy_dec  = lookup_decimals(ctx, &signal.buy_mint, &signal.buy_symbol)?;
    let sell_human = holdings_of(ctx.portfolio, &signal.sell_mint, &signal.sell_symbol)
        * ctx.cfg.rebalance_size_fraction;
    if sell_human <= 0.0 {
        return Ok(None);
    }
    let sell_raw = jupiter::to_raw_amount(sell_human, sell_dec);

    // 5d. Jupiter quote.
    let quote = jupiter::quote(
        ctx.http,
        &ctx.cfg.jupiter_api_url,
        &signal.sell_mint,
        &signal.buy_mint,
        sell_raw,
        ctx.cfg.rebalance_max_slippage_bps,
    )
    .await
    .context("jupiter quote failed")?;
    let expected_buy_amount = quote.out_amount.parse::<u64>()
        .map(|raw| jupiter::from_raw_amount(raw, buy_dec))
        .unwrap_or(0.0);

    // 5e. Cost gate: gas (lamports → EUR) + slippage (price impact bps).
    let sol_price_usd = ctx.prices_usd.get("SOL").copied().unwrap_or(0.0);
    let sol_price_eur = sol_price_usd * ctx.eur_rate;
    let gas_lamports = BASE_FEE_LAMPORTS * 2 + 5_000;      // arb tx + ATA-creation rent buffer
    let jito_tip_lamports = 0;                              // Jupiter handles tip when needed
    let gas_eur = lamports_to_eur(gas_lamports + jito_tip_lamports, sol_price_eur);
    let trade_eur = signal.sell_value_eur * ctx.cfg.rebalance_size_fraction;
    let gas_bps = if trade_eur > 0.0 { (gas_eur / trade_eur * 10_000.0) as u32 } else { 0 };
    let slip_bps = jupiter::price_impact_bps(&quote);
    let total_cost_bps = gas_bps + slip_bps;
    if total_cost_bps > ctx.cfg.rebalance_max_cost_bps {
        info!(
            "rebalancer: {}→{} rejected — total cost {} bps > budget {} bps (gas {} bps, slip {} bps)",
            signal.sell_symbol, signal.buy_symbol,
            total_cost_bps, ctx.cfg.rebalance_max_cost_bps, gas_bps, slip_bps,
        );
        log_action(ctx, now_ts, ActionKind::SkipCostGate {
            sell: signal.sell_symbol.clone(),
            buy: signal.buy_symbol.clone(),
            total_cost_bps,
            gas_bps,
            slip_bps,
            budget_bps: ctx.cfg.rebalance_max_cost_bps,
        });
        return Ok(None);
    }

    // 6. Snapshot BEFORE submission (skip in dry-run so baseline isn't poisoned).
    let planned = PlannedAction {
        sell_symbol: signal.sell_symbol.clone(),
        sell_mint: signal.sell_mint.clone(),
        sell_amount: sell_human,
        buy_symbol: signal.buy_symbol.clone(),
        buy_mint: signal.buy_mint.clone(),
        expected_buy_amount,
    };
    let snapshot = rebalancer_snapshots::build(
        now_ts,
        "pre-swap",
        ctx.portfolio,
        ctx.prices_usd,
        ctx.eur_rate,
        planned.clone(),
    );
    if !ctx.cfg.rebalance_dry_run {
        rebalancer_snapshots::append(
            Path::new(&ctx.cfg.rebalancer_snapshots_path),
            &snapshot,
        )
        .context("snapshot append failed")?;
    }

    // 7. BEFORE banner.
    log_before_banner(signal, sell_human, expected_buy_amount, gas_bps, slip_bps, total_cost_bps, ctx);

    if ctx.cfg.rebalance_dry_run {
        info!("rebalancer: dry-run — skipping submission");
        log_action(ctx, now_ts, ActionKind::DryRun {
            sell: signal.sell_symbol.clone(),
            buy: signal.buy_symbol.clone(),
            sell_amount: sell_human,
            expected_buy_amount,
            total_cost_bps,
        });
        return Ok(Some(ExecutedSwap {
            record: build_record(
                signal, now_ts, sell_human, expected_buy_amount,
                expected_buy_amount, gas_lamports, jito_tip_lamports,
                total_cost_bps, slip_bps, "dry-run".to_string(), ctx,
            ),
            snapshot,
            dry_run: true,
        }));
    }

    // 8. Sign + submit + confirm — with retry on submit/confirm failure.
    // Slippage rejections are the common case: a stale quote's slippage
    // tolerance is exceeded between sign and execute. Re-quoting refreshes
    // both the price ceiling and the routing.
    let max_attempts = ctx.cfg.rebalance_retry_attempts.max(1);
    let backoff = Duration::from_millis(ctx.cfg.rebalance_retry_backoff_ms);
    let mut current_quote = quote.clone();
    let mut current_expected_out = expected_buy_amount;
    let mut current_total_cost_bps = total_cost_bps;
    let mut current_slip_bps = slip_bps;
    let mut attempts_used: u32 = 0;
    let (sig, confirmed) = loop {
        attempts_used += 1;
        match submit_and_confirm(ctx, &current_quote).await {
            Ok((sig, confirmed)) => break (sig, confirmed),
            Err(e) => {
                let reason = format!("{e:#}");
                warn!(
                    "rebalancer: {}→{} attempt {}/{} failed: {reason}",
                    signal.sell_symbol, signal.buy_symbol, attempts_used, max_attempts,
                );
                log_action(ctx, unix_now(), ActionKind::RetryAttempt {
                    sell: signal.sell_symbol.clone(),
                    buy: signal.buy_symbol.clone(),
                    attempt: attempts_used,
                    max_attempts,
                    reason: reason.clone(),
                });
                if attempts_used >= max_attempts {
                    log_action(ctx, unix_now(), ActionKind::AllRetriesFailed {
                        sell: signal.sell_symbol.clone(),
                        buy: signal.buy_symbol.clone(),
                        attempts: attempts_used,
                        reason: reason.clone(),
                    });
                    error!(
                        "rebalancer: {}→{} abandoned after {} attempts: {reason}",
                        signal.sell_symbol, signal.buy_symbol, attempts_used,
                    );
                    return Err(anyhow::anyhow!("all {max_attempts} attempts failed: {reason}"));
                }
                tokio::time::sleep(backoff).await;
                // Re-quote so slippage and route reflect the new market state.
                // Keep the old quote on re-quote failure (transient HTTP issue).
                match jupiter::quote(
                    ctx.http,
                    &ctx.cfg.jupiter_api_url,
                    &signal.sell_mint,
                    &signal.buy_mint,
                    sell_raw,
                    ctx.cfg.rebalance_max_slippage_bps,
                ).await {
                    Ok(q) => {
                        let new_slip_bps = jupiter::price_impact_bps(&q);
                        let new_total_cost = gas_bps + new_slip_bps;
                        // Re-check cost budget — if the market moved so far that
                        // the new quote is over budget, abandoning is safer than
                        // burning more fees.
                        if new_total_cost > ctx.cfg.rebalance_max_cost_bps {
                            warn!(
                                "rebalancer: re-quote cost {} bps > budget {} bps; abandoning retries",
                                new_total_cost, ctx.cfg.rebalance_max_cost_bps,
                            );
                            log_action(ctx, unix_now(), ActionKind::AllRetriesFailed {
                                sell: signal.sell_symbol.clone(),
                                buy: signal.buy_symbol.clone(),
                                attempts: attempts_used,
                                reason: format!(
                                    "re-quote cost {new_total_cost} bps > budget {} bps",
                                    ctx.cfg.rebalance_max_cost_bps,
                                ),
                            });
                            return Ok(None);
                        }
                        current_expected_out = q.out_amount.parse::<u64>()
                            .map(|raw| jupiter::from_raw_amount(raw, buy_dec))
                            .unwrap_or(0.0);
                        current_total_cost_bps = new_total_cost;
                        current_slip_bps = new_slip_bps;
                        current_quote = q;
                    }
                    Err(qe) => {
                        warn!("rebalancer: re-quote failed: {qe:#} — reusing previous quote");
                    }
                }
            }
        }
    };

    // 9. Record + email.
    let mut record = build_record(
        signal, now_ts, sell_human, current_expected_out, current_expected_out,
        gas_lamports, jito_tip_lamports, current_total_cost_bps, current_slip_bps,
        sig.to_string(), ctx,
    );
    record.status = if confirmed { "confirmed" } else { "unconfirmed" }.to_string();
    {
        let mut log = exec_log.clone();
        log.executions.push(record.clone());
        rebalancer_state::save(Path::new(&ctx.cfg.rebalancer_state_path), &log)
            .context("execution-state save failed")?;
    }
    log_action(ctx, now_ts, ActionKind::Executed {
        sell: record.sell_symbol.clone(),
        buy: record.buy_symbol.clone(),
        sell_amount: record.sell_amount,
        buy_amount: record.buy_amount,
        total_cost_bps: record.total_cost_bps,
        tx_sig: record.tx_sig.clone(),
        status: record.status.clone(),
        attempts_used,
    });
    log_after_banner(&record, confirmed);

    // 10. Email (bypasses ALERT_COOLDOWN_MIN — execution receipts must arrive).
    let (subject, body) = build_execution_email(&record, &snapshot, ctx, confirmed);
    match emailer::send_alert(ctx.cfg, &subject, &body).await {
        Ok(true)  => info!("rebalancer: execution email sent ({sig})"),
        Ok(false) => warn!("rebalancer: execution email skipped (SMTP creds missing)"),
        Err(e)    => error!("rebalancer: execution email failed: {e:#}"),
    }

    Ok(Some(ExecutedSwap { record, snapshot, dry_run: false }))
}

/// One full submit attempt: fetch swap tx from Jupiter, sign, submit, confirm.
/// Returns `Ok((sig, true))` when the tx confirmed inside the timeout,
/// `Ok((sig, false))` when it submitted but didn't confirm in time (no retry —
/// we can't undo a submitted tx without double-spending), and `Err` for every
/// other failure (build error, on-chain reversion, RPC issue).
async fn submit_and_confirm(
    ctx: &RebalanceContext<'_>,
    quote: &super::jupiter::QuoteResponse,
) -> Result<(Signature, bool)> {
    let keypair = scanner::load_keypair(&ctx.cfg.wallet_keypair_path)
        .context("could not load wallet keypair")?;
    let user_pubkey = keypair.pubkey().to_string();
    let swap_resp = jupiter::swap(ctx.http, &ctx.cfg.jupiter_api_url, quote, &user_pubkey)
        .await
        .context("jupiter /swap failed")?;

    let tx_b64 = swap_resp.swap_transaction.clone();
    let rpc_url_submit = ctx.cfg.rpc_url.clone();
    let sig: Signature = tokio::task::spawn_blocking(move || -> Result<Signature> {
        let raw = STANDARD.decode(tx_b64).context("base64 decode of swap tx failed")?;
        let mut tx: VersionedTransaction = bincode::deserialize(&raw)
            .context("bincode decode of swap tx failed")?;
        tx = sign_versioned(tx, &keypair)?;
        let rpc = RpcClient::new_with_commitment(rpc_url_submit, CommitmentConfig::confirmed());
        rpc.send_transaction(&tx).context("send_transaction failed")
    })
    .await
    .context("swap submit join failed")??;

    let rpc_url_confirm = ctx.cfg.rpc_url.clone();
    let confirmed: bool = tokio::task::spawn_blocking(move || -> Result<bool> {
        let rpc = RpcClient::new_with_commitment(rpc_url_confirm, CommitmentConfig::confirmed());
        let started = Instant::now();
        while started.elapsed() < CONFIRM_TIMEOUT {
            let statuses = rpc.get_signature_statuses(&[sig]).ok();
            if let Some(st) = statuses.and_then(|r| r.value.into_iter().next()).flatten() {
                if st.err.is_some() {
                    anyhow::bail!("transaction reverted on chain: {:?}", st.err);
                }
                if st.confirmation_status.is_some() {
                    return Ok(true);
                }
            }
            std::thread::sleep(Duration::from_millis(800));
        }
        Ok(false)
    })
    .await
    .context("confirm join failed")??;

    Ok((sig, confirmed))
}

fn sign_versioned(mut tx: VersionedTransaction, keypair: &Keypair) -> Result<VersionedTransaction> {
    // Jupiter returns the transaction with an empty signature slot in position 0 for
    // the fee-payer. Sign the message and overwrite that slot.
    let msg = tx.message.serialize();
    let sig = keypair.sign_message(&msg);
    if tx.signatures.is_empty() {
        tx.signatures.push(sig);
    } else {
        tx.signatures[0] = sig;
    }
    Ok(tx)
}

fn lookup_decimals(ctx: &RebalanceContext<'_>, mint: &str, symbol: &str) -> Result<u8> {
    if symbol == "SOL" || mint == analyzer::SOL_MINT {
        return Ok(9);
    }
    ctx.decimals
        .get(mint)
        .copied()
        .with_context(|| format!("decimals missing for mint {mint} ({symbol})"))
}

fn holdings_of(portfolio: &Portfolio, mint: &str, symbol: &str) -> f64 {
    if symbol == "SOL" || mint == analyzer::SOL_MINT {
        return portfolio.sol_amount;
    }
    portfolio
        .tokens
        .iter()
        .find(|t| t.mint == mint || t.symbol == symbol)
        .map(|t| t.amount)
        .unwrap_or(0.0)
}

fn current_eur_price(ctx: &RebalanceContext<'_>, symbol: &str, mint: &str) -> f64 {
    let key = if ctx.prices_usd.contains_key(mint) { mint } else { symbol };
    ctx.prices_usd.get(key).copied().unwrap_or(0.0) * ctx.eur_rate
}

fn lamports_to_eur(lamports: u64, sol_eur: f64) -> f64 {
    (lamports as f64 / 1_000_000_000.0) * sol_eur
}

#[allow(clippy::too_many_arguments)]
fn build_record(
    signal: &RebalanceSignal,
    ts: i64,
    sell_amount: f64,
    expected_buy_amount: f64,
    buy_amount: f64,
    gas_lamports: u64,
    jito_tip_lamports: u64,
    total_cost_bps: u32,
    slip_bps: u32,
    tx_sig: String,
    ctx: &RebalanceContext<'_>,
) -> ExecutionRecord {
    ExecutionRecord {
        ts,
        sell_symbol: signal.sell_symbol.clone(),
        sell_mint: signal.sell_mint.clone(),
        sell_amount,
        sell_price_eur: signal.sell_price_usd * ctx.eur_rate,
        buy_symbol: signal.buy_symbol.clone(),
        buy_mint: signal.buy_mint.clone(),
        buy_amount,
        buy_price_eur: signal.buy_price_usd * ctx.eur_rate,
        expected_buy_amount,
        slippage_bps_realized: slip_bps,
        gas_lamports,
        jito_tip_lamports,
        total_cost_bps,
        tx_sig,
        status: "submitted".to_string(),
    }
}

fn log_before_banner(
    s: &RebalanceSignal, sell_amount: f64, expected_out: f64,
    gas_bps: u32, slip_bps: u32, total_bps: u32, ctx: &RebalanceContext<'_>,
) {
    let lines = [
        "\x1b[33m╔═ REBALANCE START ═════════════════════════════════".to_string(),
        format!("║ Sell:  {} {:.4} @ €{:.4}", s.sell_symbol, sell_amount, s.sell_price_usd * ctx.eur_rate),
        format!("║        30d_high=€{:.4} (touched {:.1}h ago)  decline_60m={:+.2}%",
            s.sell_30d_high * ctx.eur_rate, s.sell_hours_since_high, s.sell_decline_pct),
        format!("║ Buy:   {} expected≈{:.4} @ €{:.4}", s.buy_symbol, expected_out, s.buy_price_usd * ctx.eur_rate),
        format!("║        30d_low=€{:.4} (touched {:.1}h ago)   rise_60m={:+.2}%",
            s.buy_30d_low * ctx.eur_rate, s.buy_hours_since_low, s.buy_rise_pct),
        format!("║ Cost:  gas={} bps  slip={} bps  total={} bps  budget={} bps",
            gas_bps, slip_bps, total_bps, ctx.cfg.rebalance_max_cost_bps),
        "╚════════════════════════════════════════════════════\x1b[0m".to_string(),
    ];
    for l in &lines { eprintln!("{l}"); }
}

fn log_after_banner(r: &ExecutionRecord, confirmed: bool) {
    let status = if confirmed { "confirmed" } else { "\x1b[31munconfirmed\x1b[0m" };
    let lines = [
        "\x1b[32m╔═ REBALANCE DONE ══════════════════════════════════".to_string(),
        format!("║ Tx:    {}", r.tx_sig),
        format!("║ Explorer: https://solscan.io/tx/{}", r.tx_sig),
        format!("║ Sold:  {:.4} {}  →  Got: {:.4} {} (expected {:.4})",
            r.sell_amount, r.sell_symbol, r.buy_amount, r.buy_symbol, r.expected_buy_amount),
        format!("║ Realized cost: {} bps   status={}", r.total_cost_bps, status),
        "╚════════════════════════════════════════════════════\x1b[0m".to_string(),
    ];
    for l in &lines { eprintln!("{l}"); }
}

fn build_execution_email(
    r: &ExecutionRecord,
    snap: &PortfolioSnapshot,
    ctx: &RebalanceContext<'_>,
    confirmed: bool,
) -> (String, String) {
    let subject = if confirmed {
        format!("[portfolio-watcher] Swap executed: {} → {} (€{:.2})",
            r.sell_symbol, r.buy_symbol, snap.total_eur * ctx.cfg.rebalance_size_fraction)
    } else {
        format!("[portfolio-watcher] Swap UNCONFIRMED: {}", r.tx_sig)
    };

    let live_eur = current_portfolio_value_eur(ctx);
    let next_unlock_ts = r.ts + (ctx.cfg.rebalance_hold_days as i64) * 86_400;

    let mut body = String::new();
    body.push_str(&format!("EXECUTED at unix {}  ({})\n", r.ts, iso_ts(r.ts)));
    body.push_str(&"=".repeat(40));
    body.push('\n');

    body.push_str("\nBEFORE\n------\n");
    body.push_str(&format!("Sell  {}  {:.4}  @ €{:.4}\n", r.sell_symbol, r.sell_amount, r.sell_price_eur));
    body.push_str(&format!("Buy   {}   expected ~{:.4}  @ €{:.4}\n", r.buy_symbol, r.expected_buy_amount, r.buy_price_eur));
    body.push_str(&format!("Portfolio value before: €{:.2}\n", snap.total_eur));

    body.push_str("\nCOST BUDGET\n-----------\n");
    body.push_str(&format!("Gas:        {} lamports\n", r.gas_lamports));
    body.push_str(&format!("Jito tip:   {} lamports\n", r.jito_tip_lamports));
    body.push_str(&format!("Slippage:   {} bps\n", r.slippage_bps_realized));
    body.push_str(&format!("Total:      {} bps  / budget {} bps\n",
        r.total_cost_bps, ctx.cfg.rebalance_max_cost_bps));

    body.push_str("\nAFTER\n-----\n");
    body.push_str(&format!("Tx sig: {}\n", r.tx_sig));
    body.push_str(&format!("Explorer: https://solscan.io/tx/{}\n", r.tx_sig));
    body.push_str(&format!("Status: {}\n", if confirmed { "confirmed" } else { "UNCONFIRMED" }));
    body.push_str(&format!("Portfolio value after: €{:.2}\n", live_eur));

    body.push_str("\nNEXT\n----\n");
    body.push_str(&format!("Hold cooldown until: unix {} ({})\n", next_unlock_ts, iso_ts(next_unlock_ts)));
    body.push_str(&format!("Reverse swap blocked unless {} gains ≥ {:.1}%\n",
        r.buy_symbol, ctx.cfg.rebalance_take_profit_pct));

    (subject, body)
}

/// Cheap UTC-ish ISO-8601 formatter: avoids pulling in chrono just for one line.
/// Produces "1970-01-01T00:00:00Z"-style strings using only standard lib math.
fn iso_ts(ts: i64) -> String {
    if ts <= 0 { return "epoch".to_string(); }
    let secs_per_day: i64 = 86_400;
    let days = ts / secs_per_day;
    let secs_of_day = ts % secs_per_day;
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    // Days since 1970-01-01 → calendar date (proleptic Gregorian).
    let (y, mo, d) = days_to_ymd(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

fn days_to_ymd(mut days: i64) -> (i32, u32, u32) {
    let mut y: i32 = 1970;
    loop {
        let leap = is_leap(y);
        let dy = if leap { 366 } else { 365 };
        if days < dy { break; }
        days -= dy;
        y += 1;
    }
    let months = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m: u32 = 1;
    for len in months {
        if days < len { break; }
        days -= len;
        m += 1;
    }
    (y, m, days as u32 + 1)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_ts_known_dates() {
        assert_eq!(iso_ts(0), "epoch");
        assert_eq!(iso_ts(1), "1970-01-01T00:00:01Z");
        assert_eq!(iso_ts(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(iso_ts(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn lamports_to_eur_converts_correctly() {
        assert!((lamports_to_eur(1_000_000_000, 150.0) - 150.0).abs() < 1e-9);
        assert!((lamports_to_eur(500_000_000, 100.0) - 50.0).abs() < 1e-9);
    }
}
