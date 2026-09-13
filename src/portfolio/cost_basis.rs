//! Real fill basis for ADOPTED positions (2026-09-13).
//!
//! When the momentum trader adopts a wallet holding it only knows the mark at the moment it
//! noticed the balance, so `entry`, `peak` and the logged drawdown all start there — a STONK
//! bought at $0.270 and adopted at $0.253 showed "drawdown −0.9%" while the operator was −7%.
//! This module reads the owner's token-account history over JSON-RPC, derives the swaps that
//! built the CURRENT balance (newest first), and returns their volume-weighted fill price.
//!
//! Decisions fixed with the operator (2026-09-13): the fill drives the position's reference
//! price, the trail's peak is seeded at max(fill, adoption mark) — a holding already under its
//! fill is NOT sold on adoption — and the bot's realized P&L keeps counting from custody
//! (`usdc_spent` stays mark × amount; `TradeRecord` accounting is untouched).
//!
//! Fail-open everywhere: any RPC error, timeout or unparseable transaction ⇒ `None` ⇒ the
//! adoption falls back to today's mark-based seeding.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::portfolio::momentum_state::Position;

pub const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
pub const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
/// SOL spent at or below this in a transaction that ALSO has a USDC leg is account rent and
/// fees, not price. Without a USDC leg it is the buy itself only when clearly above this.
pub const RENT_SOL_MAX: f64 = 0.01;
/// A holding is "covered" when the walked fills explain at least this share of it; below,
/// the basis is unknown rather than a guess built from a fraction of the position.
pub const MIN_COVERAGE: f64 = 0.5;

/// One swap that INCREASED the owner's balance of the mint: what arrived and what left.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub ts: i64,
    pub tokens: f64,
    pub usdc_paid: f64,
    pub sol_paid: f64,
}

impl Fill {
    /// USD paid: the USDC leg if present, else the SOL leg at `sol_usd` (rent-sized SOL alone
    /// prices nothing).
    pub fn cost_usd(&self, sol_usd: f64) -> Option<f64> {
        if self.usdc_paid > 0.0 {
            Some(self.usdc_paid)
        } else if self.sol_paid > RENT_SOL_MAX && sol_usd > 0.0 {
            Some(self.sol_paid * sol_usd)
        } else {
            None
        }
    }
    pub fn price_usd(&self, sol_usd: f64) -> Option<f64> {
        if self.tokens <= 0.0 {
            return None;
        }
        self.cost_usd(sol_usd).map(|c| c / self.tokens)
    }
}

/// Volume-weighted fill basis of the fills that make up the current balance.
#[derive(Debug, Clone, PartialEq)]
pub struct FillBasis {
    pub avg_price_usd: f64,
    pub cost_usdc: f64,
    pub covered_tokens: f64,
    pub first_fill_ts: i64,
    pub last_fill_ts: i64,
    pub n_fills: usize,
}

fn owned_ui(balances: Option<&Value>, owner: &str, mint: &str) -> f64 {
    balances
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter(|b| b.get("owner").and_then(Value::as_str) == Some(owner) && b.get("mint").and_then(Value::as_str) == Some(mint))
                .map(|b| {
                    let ta = b.get("uiTokenAmount");
                    ta.and_then(|t| t.get("uiAmount")).and_then(Value::as_f64).or_else(|| {
                        ta.and_then(|t| t.get("uiAmountString")).and_then(Value::as_str).and_then(|s| s.parse().ok())
                    }).unwrap_or(0.0)
                })
                .sum()
        })
        .unwrap_or(0.0)
}

/// The fill a jsonParsed `getTransaction` result represents for `owner`/`mint`, or `None`
/// when the tx failed, moved no tokens in, or paid nothing (a transfer or airdrop — cost
/// unknown). Native SOL is read from the owner's account balances net of the fee; wrapped SOL
/// from its token balance.
pub fn fill_from_parsed_tx(tx: &Value, owner: &str, mint: &str) -> Option<Fill> {
    let meta = tx.get("meta")?;
    if !meta.get("err").is_none_or(Value::is_null) {
        return None;
    }
    let ts = tx.get("blockTime")?.as_i64()?;
    let (pre, post) = (meta.get("preTokenBalances"), meta.get("postTokenBalances"));
    let d_tok = owned_ui(post, owner, mint) - owned_ui(pre, owner, mint);
    if d_tok <= 1e-12 {
        return None;
    }
    let d_usdc = owned_ui(post, owner, USDC_MINT) - owned_ui(pre, owner, USDC_MINT);
    let d_wsol = owned_ui(post, owner, WSOL_MINT) - owned_ui(pre, owner, WSOL_MINT);
    let keys = tx.pointer("/transaction/message/accountKeys").and_then(Value::as_array);
    let owner_idx = keys.and_then(|ks| {
        ks.iter().position(|k| k.as_str() == Some(owner) || k.get("pubkey").and_then(Value::as_str) == Some(owner))
    });
    let lamports = |k: &str, i: usize| meta.get(k).and_then(Value::as_array).and_then(|a| a.get(i)).and_then(Value::as_f64);
    let fee = meta.get("fee").and_then(Value::as_f64).unwrap_or(0.0);
    let d_sol = owner_idx
        .and_then(|i| Some((lamports("postBalances", i)? - lamports("preBalances", i)? + fee) / 1e9))
        .unwrap_or(0.0);
    let usdc_paid = (-d_usdc).max(0.0);
    let sol_paid = (-(d_sol + d_wsol)).max(0.0);
    if usdc_paid <= 0.0 && sol_paid <= RENT_SOL_MAX {
        return None; // tokens arrived, nothing left: a transfer, not a purchase
    }
    Some(Fill { ts, tokens: d_tok, usdc_paid, sol_paid })
}

/// Walk fills NEWEST FIRST, taking tokens until `balance` is explained (the lots still held
/// are the most recent buys; earlier lots were sold). A partially consumed oldest lot counts
/// pro rata. Below `MIN_COVERAGE` of the balance the basis is unknown. Sells between buys are
/// not netted (a documented approximation — bot round-trips are whole lots).
pub fn basis_for_balance(fills_newest_first: &[Fill], balance: f64, sol_usd_at: impl Fn(i64) -> f64) -> Option<FillBasis> {
    if balance <= 0.0 {
        return None;
    }
    let tolerance = balance * 0.005;
    let mut remaining = balance;
    let (mut cost, mut covered, mut n) = (0.0_f64, 0.0_f64, 0usize);
    let (mut first, mut last) = (i64::MAX, i64::MIN);
    for f in fills_newest_first {
        if remaining <= tolerance {
            break;
        }
        let Some(price) = f.price_usd(sol_usd_at(f.ts)) else { continue };
        let take = f.tokens.min(remaining);
        if take <= 0.0 {
            continue;
        }
        cost += take * price;
        covered += take;
        remaining -= take;
        n += 1;
        first = first.min(f.ts);
        last = last.max(f.ts);
    }
    if n == 0 || covered < MIN_COVERAGE * balance {
        return None;
    }
    Some(FillBasis { avg_price_usd: cost / covered, cost_usdc: cost, covered_tokens: covered, first_fill_ts: first, last_fill_ts: last, n_fills: n })
}

/// Peak to seed an adopted position with: the higher of the real fill and the adoption mark
/// (operator decision 2026-09-13 — a holding already under its fill is not sold on adoption).
/// Returns `(peak, peak_is_the_fill)`.
pub fn seed_adopted_peak(mark: f64, fill: Option<f64>) -> (f64, bool) {
    match fill {
        Some(f) if f > mark => (f, true),
        _ => (mark, false),
    }
}

/// Write a found basis onto a position: fill fields always; the peak only if the fill is
/// higher than the current peak (never lower it). Returns whether the peak was raised.
/// `entry_price_usd`/`usdc_spent` are left alone — the bot's P&L keeps counting from custody.
pub fn apply_fill_to_position(pos: &mut Position, basis: &FillBasis) -> bool {
    pos.fill_price_usd = Some(basis.avg_price_usd);
    pos.fill_ts = basis.first_fill_ts;
    if basis.avg_price_usd > pos.peak_price_usd {
        pos.peak_price_usd = basis.avg_price_usd;
        pos.peak_ts = basis.first_fill_ts;
        true
    } else {
        false
    }
}

async fn rpc(http: &reqwest::Client, url: &str, method: &str, params: Value) -> Result<Value> {
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
    let v: Value = http.post(url).json(&body).send().await.with_context(|| format!("{method}: send"))?.json().await.with_context(|| format!("{method}: body"))?;
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        return Err(anyhow!("{method}: {err}"));
    }
    Ok(v.get("result").cloned().unwrap_or(Value::Null))
}

/// Fill basis of `owner`'s current `balance` of `mint` from its token-account history: the
/// most recent `max_sigs` signatures per token account, newest first, each fetched with
/// `getTransaction` (jsonParsed) until the balance is covered. Every failure is an `Err` the
/// caller treats as "unknown" — it never blocks an adoption.
pub async fn wallet_fill_basis(
    http: &reqwest::Client,
    rpc_url: &str,
    owner: &str,
    mint: &str,
    balance: f64,
    max_sigs: usize,
    sol_usd_at: impl Fn(i64) -> f64,
) -> Result<Option<FillBasis>> {
    let accounts = rpc(http, rpc_url, "getTokenAccountsByOwner", json!([owner, {"mint": mint}, {"encoding": "jsonParsed"}])).await?;
    let tas: Vec<String> = accounts
        .get("value").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.get("pubkey").and_then(Value::as_str).map(str::to_string)).collect())
        .unwrap_or_default();
    let mut sigs: Vec<(String, i64)> = Vec::new();
    for ta in &tas {
        let list = rpc(http, rpc_url, "getSignaturesForAddress", json!([ta, {"limit": max_sigs.clamp(1, 1000)}])).await?;
        for e in list.as_array().into_iter().flatten() {
            if !e.get("err").is_none_or(Value::is_null) {
                continue;
            }
            if let (Some(sig), Some(t)) = (e.get("signature").and_then(Value::as_str), e.get("blockTime").and_then(Value::as_i64)) {
                sigs.push((sig.to_string(), t));
            }
        }
    }
    sigs.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    let mut fills: Vec<Fill> = Vec::new();
    let mut covered = 0.0;
    for (sig, _) in &sigs {
        let tx = rpc(http, rpc_url, "getTransaction", json!([sig, {"encoding": "jsonParsed", "maxSupportedTransactionVersion": 0}])).await?;
        if tx.is_null() {
            continue;
        }
        if let Some(f) = fill_from_parsed_tx(&tx, owner, mint) {
            covered += f.tokens;
            fills.push(f);
            if covered >= balance * 0.995 {
                break;
            }
        }
    }
    Ok(basis_for_balance(&fills, balance, sol_usd_at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWNER: &str = "3gPdd4617xMsXjPd4w6ZaggFPdVvNPTKJTQS2JJJtgvz";
    const MINT: &str = "6GmAFSYs4gk3FDao5FzzySQpPZaWsa4rUJHacpMpUNgx";

    /// A jsonParsed `getTransaction` result: the owner's token/SOL balances before and after.
    fn tx(block_time: i64, tok_pre: f64, tok_post: f64, usdc_pre: f64, usdc_post: f64, lamports_pre: u64, lamports_post: u64, fee: u64) -> serde_json::Value {
        let tb = |mint: &str, ui: f64, idx: u64| json!({
            "accountIndex": idx, "mint": mint, "owner": OWNER,
            "uiTokenAmount": {"uiAmount": ui, "decimals": 6, "amount": format!("{}", (ui * 1e6) as u64), "uiAmountString": ui.to_string()}
        });
        let mut pre = vec![tb(USDC_MINT, usdc_pre, 2)];
        if tok_pre > 0.0 { pre.push(tb(MINT, tok_pre, 1)); } // an ATA created in this tx has no pre entry
        let post = vec![tb(MINT, tok_post, 1), tb(USDC_MINT, usdc_post, 2)];
        json!({
            "blockTime": block_time,
            "meta": {
                "err": null, "fee": fee,
                "preBalances": [lamports_pre, 2_039_280, 2_039_280], "postBalances": [lamports_post, 2_039_280, 2_039_280],
                "preTokenBalances": pre, "postTokenBalances": post
            },
            "transaction": {"message": {"accountKeys": [
                {"pubkey": OWNER, "signer": true, "writable": true},
                {"pubkey": "ATA111111111111111111111111111111111111111", "signer": false, "writable": true},
                {"pubkey": "USDCATA1111111111111111111111111111111111111", "signer": false, "writable": true}
            ]}}
        })
    }

    #[test]
    fn usdc_buy_becomes_a_fill_and_rent_is_not_a_cost() {
        // 371.6682 tokens for $100, plus 0.0018 SOL of ATA rent and a 5000-lamport fee.
        let t = tx(1_789_296_000, 0.0, 371.6682, 500.0, 400.0, 1_000_000_000, 1_000_000_000 - 1_856_000 - 5_000, 5_000);
        let f = fill_from_parsed_tx(&t, OWNER, MINT).expect("a buy");
        assert_eq!(f.ts, 1_789_296_000);
        assert!((f.tokens - 371.6682).abs() < 1e-9);
        assert!((f.usdc_paid - 100.0).abs() < 1e-9);
        assert!(f.sol_paid < 0.003, "rent-sized SOL is carried but must not price the fill");
        assert!((f.price_usd(200.0).unwrap() - 100.0 / 371.6682).abs() < 1e-12, "USDC-denominated ⇒ price = USDC / tokens");
    }

    #[test]
    fn sol_buy_prices_through_the_sol_rate_at_that_time() {
        // 1000 tokens bought for 0.5 SOL (no USDC leg), SOL at $200 then ⇒ $0.10 each.
        let t = tx(1_789_000_000, 0.0, 1000.0, 50.0, 50.0, 2_000_000_000, 2_000_000_000 - 500_000_000 - 5_000, 5_000);
        let f = fill_from_parsed_tx(&t, OWNER, MINT).expect("a buy");
        assert!((f.usdc_paid).abs() < 1e-12 && (f.sol_paid - 0.5).abs() < 1e-9);
        assert!((f.price_usd(200.0).unwrap() - 0.10).abs() < 1e-12);
    }

    #[test]
    fn sells_and_transfers_are_not_fills() {
        let sell = tx(1_789_000_000, 342.0741, 0.0, 400.0, 501.5237, 1_000_000_000, 1_000_000_000 - 5_000, 5_000);
        assert!(fill_from_parsed_tx(&sell, OWNER, MINT).is_none(), "token balance fell ⇒ a sale");
        let transfer = tx(1_789_000_000, 0.0, 5416.3547, 400.0, 400.0, 1_000_000_000, 1_000_000_000 - 5_000, 5_000);
        assert!(fill_from_parsed_tx(&transfer, OWNER, MINT).is_none(), "tokens arrived with nothing paid ⇒ a transfer, cost unknown");
        let failed = {
            let mut t = tx(1_789_000_000, 0.0, 10.0, 110.0, 100.0, 1_000_000_000, 1_000_000_000, 5_000);
            t["meta"]["err"] = json!({"InstructionError": [0, "Custom"]});
            t
        };
        assert!(fill_from_parsed_tx(&failed, OWNER, MINT).is_none(), "a failed tx moved nothing");
    }

    #[test]
    fn basis_walks_newest_fills_until_the_balance_is_covered() {
        // Newest first: the two $100 buys that make today's 740.37 balance, then an older
        // lot that was sold long ago and must NOT enter the average.
        let fills = vec![
            Fill { ts: 1_789_296_441, tokens: 368.7034, usdc_paid: 100.0, sol_paid: 0.0 },
            Fill { ts: 1_789_295_853, tokens: 371.6682, usdc_paid: 100.0, sol_paid: 0.0 },
            Fill { ts: 1_789_000_000, tokens: 5000.0, usdc_paid: 50.0, sol_paid: 0.0 },
        ];
        let b = basis_for_balance(&fills, 740.371623768, |_| 200.0).expect("covered");
        assert_eq!(b.n_fills, 2);
        assert!((b.cost_usdc - 200.0).abs() < 1e-6);
        assert!((b.avg_price_usd - 200.0 / 740.3716).abs() < 1e-6, "volume-weighted: {}", b.avg_price_usd);
        assert_eq!((b.first_fill_ts, b.last_fill_ts), (1_789_295_853, 1_789_296_441));
        assert!((b.covered_tokens - 740.3716).abs() < 1e-3);
    }

    #[test]
    fn basis_takes_a_partial_oldest_lot_and_gives_up_when_coverage_is_thin() {
        // Balance 500 = all of the newest 300 + 200 of an older 1000-lot bought at $0.05.
        let fills = vec![
            Fill { ts: 200, tokens: 300.0, usdc_paid: 30.0, sol_paid: 0.0 },   // $0.10
            Fill { ts: 100, tokens: 1000.0, usdc_paid: 50.0, sol_paid: 0.0 },  // $0.05
        ];
        let b = basis_for_balance(&fills, 500.0, |_| 0.0).unwrap();
        assert!((b.cost_usdc - (30.0 + 10.0)).abs() < 1e-9, "200 of the older lot at $0.05 = $10");
        assert!((b.avg_price_usd - 0.08).abs() < 1e-12);
        // Only 300 of a 1000 balance is explained by swaps ⇒ unknown basis, not a wild guess.
        assert!(basis_for_balance(&fills[..1], 1000.0, |_| 0.0).is_none());
        assert!(basis_for_balance(&[], 10.0, |_| 0.0).is_none());
    }

    #[test]
    fn apply_fill_sets_fill_fields_and_only_raises_the_peak() {
        let mut pos = Position {
            mint: MINT.into(), symbol: "STONK".into(), entry_ts: 1_789_302_624, entry_price_usd: 0.2527,
            token_amount: 740.37, usdc_spent: 187.09, peak_price_usd: 0.2527, peak_ts: 1_789_302_624,
            topup_usdc: 0.0, entry_sig: "adopted".into(), dry_run: false, adopted_unwatched: false,
            fill_price_usd: None, fill_ts: 0,
        };
        let b = FillBasis { avg_price_usd: 0.2701, cost_usdc: 200.0, covered_tokens: 740.37, first_fill_ts: 1_789_295_853, last_fill_ts: 1_789_296_441, n_fills: 2 };
        assert!(apply_fill_to_position(&mut pos, &b), "fill above the mark raises the peak");
        assert_eq!((pos.fill_price_usd, pos.fill_ts, pos.peak_price_usd, pos.peak_ts), (Some(0.2701), 1_789_295_853, 0.2701, 1_789_295_853));
        assert_eq!((pos.entry_price_usd, pos.usdc_spent), (0.2527, 187.09), "custody accounting untouched");
        let lower = FillBasis { avg_price_usd: 0.20, ..b.clone() };
        pos.peak_price_usd = 0.30;
        assert!(!apply_fill_to_position(&mut pos, &lower), "a fill below the peak never lowers it");
        assert_eq!((pos.fill_price_usd, pos.peak_price_usd), (Some(0.20), 0.30));
    }

    #[test]
    fn seed_peak_is_the_higher_of_fill_and_mark() {
        assert_eq!(seed_adopted_peak(0.2527, Some(0.2701)), (0.2701, true), "under water at adoption: peak = fill");
        assert_eq!(seed_adopted_peak(0.30, Some(0.27)), (0.30, false), "above the fill: peak = mark");
        assert_eq!(seed_adopted_peak(0.30, None), (0.30, false), "no fill ⇒ today's behaviour");
    }
}
