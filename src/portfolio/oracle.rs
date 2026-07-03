//! Perfect-foresight ("oracle") analysis for the momentum trader.
//!
//! Computes the maximum profit extractable from the recorded price history under
//! the replay's exact cost model (slippage per side + flat gas per swap, fixed
//! non-compounding notional), as a *diagnostic ceiling* for the strategy:
//!
//! - per-token: exact DP over one price series → optimal disjoint round-trips;
//! - across tokens: a single-slot "achievable oracle" schedule (weighted interval
//!   scheduling over the per-token trades — feasible, so a valid benchmark; the
//!   true multi-token optimum may be slightly higher).
//!
//! IMPORTANT: the oracle is non-causal (it knows the future). Its trades are a
//! measuring stick (ceiling, capture ratio, feature diagnosis) — never a target
//! to fit parameters against; any hypothesis it suggests still has to survive the
//! walk-forward robustness gate in `sim::run_grid`.

/// Fixed-notional cost model for oracle round-trips — mirrors the sim replay's
/// fill model exactly: buy fills at px×(1+s), sell at px×(1−s), flat gas per swap.
#[derive(Debug, Clone, Copy)]
pub struct OracleCosts {
    pub trade_usdc: f64,
    pub slippage_bps: u32,
    pub gas_usdc: f64,
}

impl OracleCosts {
    /// Net USDC P&L of one round-trip at the given snapshot marks.
    pub fn round_trip_pnl(&self, entry_px: f64, exit_px: f64) -> f64 {
        let s = self.slippage_bps as f64 / 10_000.0;
        self.trade_usdc * ((exit_px * (1.0 - s)) / (entry_px * (1.0 + s)) - 1.0)
            - 2.0 * self.gas_usdc
    }
}

/// One perfect-foresight round-trip, decorated for cross-token scheduling.
#[derive(Debug, Clone)]
pub struct OracleTrade {
    pub symbol: String,
    pub mint: String,
    pub entry_i: usize,
    pub exit_i: usize,
    pub entry_ts: i64,
    pub exit_ts: i64,
    pub entry_px: f64,
    pub exit_px: f64,
    pub pnl_usdc: f64,
}

/// Exact DP over one token's price series: the set of disjoint fixed-notional
/// round-trips maximizing total net P&L under `costs`. Returns `(entry_i, exit_i,
/// pnl_usdc)` oldest-first. O(N²) — fine for an offline diagnostic (~26k snapshots
/// ≈ a fraction of a second per token in release; callers parallelize per token).
///
/// `min_hold_secs` floors each trade's hold time. At 0 the ceiling is dominated by
/// 1–2-snapshot print flickers (quote noise no causal strategy can trade); a floor
/// near the strategy's own timescale gives the meaningful "strategy-shaped" ceiling.
pub fn oracle_trades(
    series: &[(i64, f64)],
    costs: &OracleCosts,
    min_hold_secs: i64,
) -> Vec<(usize, usize, f64)> {
    let n = series.len();
    if n < 2 {
        return Vec::new();
    }
    // best[i] = max P&L using snapshots 0..=i and ending flat at i.
    // par[i]  = None → carried flat from i-1; Some(j) → a round-trip j→i closed here.
    let mut best = vec![0.0_f64; n];
    let mut par: Vec<Option<usize>> = vec![None; n];
    for i in 1..n {
        best[i] = best[i - 1];
        let (exit_ts, exit_px) = series[i];
        for j in 0..i {
            if exit_ts - series[j].0 < min_hold_secs {
                break; // series is time-ordered: every later j is held even shorter
            }
            let pnl = costs.round_trip_pnl(series[j].1, exit_px);
            if pnl > 0.0 && best[j] + pnl > best[i] {
                best[i] = best[j] + pnl;
                par[i] = Some(j);
            }
        }
    }
    let mut out = Vec::new();
    let mut i = n - 1;
    loop {
        match par[i] {
            Some(j) => {
                out.push((j, i, costs.round_trip_pnl(series[j].1, series[i].1)));
                if j == 0 {
                    break;
                }
                i = j;
            }
            None => {
                if i == 0 {
                    break;
                }
                i -= 1;
            }
        }
    }
    out.reverse();
    out
}

/// Achievable single-slot schedule: the max-total-P&L subset of non-overlapping
/// trades across all tokens (weighted interval scheduling). Back-to-back is
/// allowed — a new entry may share the previous exit's timestamp (rotation-style).
/// Feasible by construction, so a valid single-slot benchmark; the true multi-token
/// optimum could be slightly higher (per-token trade sets are fixed upstream).
pub fn single_slot_schedule(trades: &[OracleTrade]) -> Vec<OracleTrade> {
    let mut ts: Vec<&OracleTrade> = trades.iter().collect();
    ts.sort_by_key(|t| (t.exit_ts, t.entry_ts));
    let n = ts.len();
    if n == 0 {
        return Vec::new();
    }
    let exits: Vec<i64> = ts.iter().map(|t| t.exit_ts).collect();
    // best[k] = max P&L over the first k trades (sorted by exit); take[k-1] marks
    // whether trade k-1 is in that optimum.
    let mut best = vec![0.0_f64; n + 1];
    let mut take = vec![false; n];
    for k in 0..n {
        // Last trade (by exit order) compatible with ts[k]: exit ≤ ts[k]'s entry.
        let p = exits[..k].partition_point(|&e| e <= ts[k].entry_ts);
        let with = best[p] + ts[k].pnl_usdc;
        if with > best[k] {
            best[k + 1] = with;
            take[k] = true;
        } else {
            best[k + 1] = best[k];
        }
    }
    let mut out = Vec::new();
    let mut k = n;
    while k > 0 {
        if take[k - 1] {
            out.push(ts[k - 1].clone());
            k = exits[..k - 1].partition_point(|&e| e <= ts[k - 1].entry_ts);
        } else {
            k -= 1;
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const COSTS: OracleCosts = OracleCosts { trade_usdc: 100.0, slippage_bps: 50, gas_usdc: 0.05 };

    /// Net P&L of one round-trip entry_px→exit_px under COSTS (mirrors the sim's
    /// fill model: buy at px×(1+s), sell at px×(1−s), gas per side).
    fn rt(entry_px: f64, exit_px: f64) -> f64 {
        let s = 50.0 / 10_000.0;
        100.0 * ((exit_px * (1.0 - s)) / (entry_px * (1.0 + s)) - 1.0) - 2.0 * 0.05
    }

    fn series(prices: &[f64]) -> Vec<(i64, f64)> {
        prices.iter().enumerate().map(|(i, &p)| (i as i64 * 180, p)).collect()
    }

    #[test]
    fn oracle_takes_single_upswing_net_of_costs() {
        let s = series(&[1.0, 1.1, 1.25, 1.4, 1.5, 1.5]);
        let trades = oracle_trades(&s, &COSTS, 0);
        assert_eq!(trades.len(), 1);
        let (e, x, pnl) = trades[0];
        assert_eq!((e, x), (0, 4), "enter the valley, exit the peak");
        assert!((pnl - rt(1.0, 1.5)).abs() < 1e-9);
    }

    #[test]
    fn oracle_min_hold_excludes_print_flickers() {
        // A 1-bar spike (untradeable print noise) followed by a slow climb. With no
        // hold floor the oracle harvests the spike; with a 3-bar floor (540 s) it
        // must sit through real moves only.
        let s = series(&[1.0, 1.3, 1.0, 1.02, 1.06, 1.10, 1.15]);
        let raw = oracle_trades(&s, &COSTS, 0);
        assert!(raw.iter().any(|&(e, x, _)| (e, x) == (0, 1)), "no floor → takes the spike");
        let held = oracle_trades(&s, &COSTS, 540);
        assert!(!held.is_empty(), "the slow climb is still worth trading");
        for &(e, x, _) in &held {
            assert!(s[x].0 - s[e].0 >= 540, "every trade honors the hold floor");
        }
        // Spike-only series → nothing tradeable at the floor.
        let spike = series(&[1.0, 1.3, 1.0, 1.0, 1.0]);
        assert!(oracle_trades(&spike, &COSTS, 540).is_empty());
    }

    #[test]
    fn oracle_skips_moves_below_cost_hurdle() {
        // +0.5% move < ~1.1% round-trip cost → trading would lose money.
        let s = series(&[1.0, 1.002, 1.005]);
        assert!(oracle_trades(&s, &COSTS, 0).is_empty());
        // Monotonic downtrend → nothing to do either.
        let d = series(&[1.5, 1.3, 1.1, 1.0]);
        assert!(oracle_trades(&d, &COSTS, 0).is_empty());
    }

    #[test]
    fn oracle_merges_dip_smaller_than_roundtrip_cost() {
        // Dip 1.30→1.29 (−0.8%) is cheaper to sit through than to exit+re-enter
        // (~1.1% round-trip) → one merged trade 1.0→1.6.
        let s = series(&[1.0, 1.3, 1.29, 1.6]);
        let trades = oracle_trades(&s, &COSTS, 0);
        assert_eq!(trades.len(), 1);
        assert_eq!((trades[0].0, trades[0].1), (0, 3));
        // A deep dip (1.30→1.05) is worth exiting for → two trades.
        let s2 = series(&[1.0, 1.3, 1.05, 1.6]);
        let trades2 = oracle_trades(&s2, &COSTS, 0);
        assert_eq!(trades2.len(), 2);
        assert_eq!((trades2[0].0, trades2[0].1), (0, 1));
        assert_eq!((trades2[1].0, trades2[1].1), (2, 3));
        // Sanity: the DP's total really beats the merged alternative.
        let split: f64 = trades2.iter().map(|t| t.2).sum();
        assert!(split > rt(1.0, 1.6));
    }

    fn ot(symbol: &str, entry_ts: i64, exit_ts: i64, pnl: f64) -> OracleTrade {
        OracleTrade {
            symbol: symbol.into(),
            mint: symbol.into(),
            entry_i: 0,
            exit_i: 0,
            entry_ts,
            exit_ts,
            entry_px: 1.0,
            exit_px: 1.0,
            pnl_usdc: pnl,
        }
    }

    #[test]
    fn single_slot_schedule_picks_max_weight_non_overlapping() {
        // A and B overlap → keep the heavier B; C is disjoint → kept.
        let trades = vec![
            ot("A", 0, 100, 10.0),
            ot("B", 50, 150, 15.0),
            ot("C", 200, 300, 5.0),
        ];
        let picked = single_slot_schedule(&trades);
        let syms: Vec<&str> = picked.iter().map(|t| t.symbol.as_str()).collect();
        assert_eq!(syms, ["B", "C"]);
        assert!((picked.iter().map(|t| t.pnl_usdc).sum::<f64>() - 20.0).abs() < 1e-9);
        // Back-to-back (exit ts == next entry ts) is allowed — rotation-style.
        let touching = vec![ot("A", 0, 100, 10.0), ot("B", 100, 200, 15.0)];
        assert_eq!(single_slot_schedule(&touching).len(), 2);
    }
}
