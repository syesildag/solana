# Momentum max-N Concurrent Positions (Simulation) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a multi-position backtest engine (`replay_multi`) and a `maxn-compare` subcommand to `momentum-sim`, to measure whether holding up to N concurrent positions beats the single-slot model on real history.

**Architecture:** Purely additive. A new `replay_multi(snapshots, watched, stream, params, regime, max_positions)` generalizes the existing single-position `replay_with_regime` to hold a `Vec<Position>` capped at N (fixed `trade_usdc` notional per slot, deduped by mint). When full and `rotate_margin > 0`, the weakest *green* held position is evicted for a stronger candidate (portfolio-level generalization of `try_rotate`). A new `maxn-compare` CLI subcommand (mirroring `regime-compare`) builds the ranked stream once and replays it at N=1..K, printing a per-N table. The production `run` grid, `replay`, and `replay_with_regime` are never modified.

**Tech Stack:** Rust, clap (CLI), rayon (already used by the grid — not touched here). Tests are `#[cfg(test)]` blocks at the bottom of `src/portfolio/sim.rs`, run with `cargo test --bin momentum-sim`.

## Global Constraints

- **No changes to the live trader** (`src/portfolio/momentum.rs`, `momentum_state.rs`) — sim-only.
- **No new `.env` variable** in this plan — `max_n` is a CLI flag only.
- **Production grid untouched:** do not modify `replay`, `replay_with_stream`, `replay_with_regime`, or `run_grid`. `replay_multi` is a new function.
- **Correctness anchor:** `replay_multi(..., max_positions = 1)` MUST produce a `SimRun` with identical `trades` (mint, entry_ts, exit_ts, usdc_in, usdc_out) and identical `equity_curve` to `replay_with_regime` on the same stream + params + regime mask. This holds for both `rotate_margin == 0` (Task 1) and `rotate_margin > 0` (Task 2).
- **Drawdown stays realized-equity based:** reuse the existing `SimRun.equity_curve` (cumulative realized P&L) and `max_drawdown_pct`. Do NOT add unrealized-equity tracking — it would break the N=1 anchor. With N positions the realized curve naturally aggregates all positions' closed P&L, which is the portfolio drawdown we report.
- **Fixed notional per slot:** each entry sizes via the existing `dynamic_trade_usdc(...)`, exactly as the single-slot path. Total deployed therefore scales with N; the compare table reports a per-$1k-deployed column so N>1 must win per-dollar, not just by deploying more.
- All existing per-tick gate ordering is preserved: **stop-family exit → eviction (rotate) → fade exit → entries**.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/portfolio/sim.rs` | Backtest engine. Add `replay_multi`, the `MaxnRow` struct, and `maxn_rows` driver. Add tests. | Modify (additive) |
| `src/bin/momentum_sim.rs` | CLI. Add `MaxnCompare` command variant, its match arm, `MaxnCompareArgs`, and `maxn_compare` printer. | Modify (additive) |

---

## Task 1: `replay_multi` engine — fill-and-hold (no eviction)

**Files:**
- Modify: `src/portfolio/sim.rs` (add `replay_multi` after `replay_with_regime`, ~line 642)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: `ranked_stream`, `replay_with_regime`, `Position` (from `momentum_state`), `Candidate`, `vol_stop_triggered`, `profit_protected_stop_triggered`, `token_atr`, `token_return_sigma`, `token_dip_z`, `is_stale_ts`, `recent_series`, `fade_take_profit`, `dynamic_trade_usdc`, `est_gas_bps`, `est_gas_usdc`, `entry_fill_price`, `exit_fill_price`, `build_trade_record`, `SOL_KEY` — all already in scope in `sim.rs`.
- Produces: `pub fn replay_multi(snapshots: &[PriceSnapshot], watched: &[WatchedToken], stream: &[Vec<Candidate>], params: &ParamSet, regime: &[bool], max_positions: usize) -> SimRun`

- [ ] **Step 1: Write the failing anchor + multi-fill + dedup tests**

Add to the `#[cfg(test)] mod tests` block at the bottom of `src/portfolio/sim.rs`:

```rust
    // ── helper: an up-then-down path that guarantees an entry then a trailing-stop exit ──
    fn rise_then_fall(token: &str, n_up: u64, n_down: u64) -> Vec<PriceSnapshot> {
        let sol = 150.0;
        let mk = |ts: u64, p: f64| {
            let mut m = HashMap::new();
            m.insert(token.to_string(), p);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let mut snaps = Vec::new();
        let mut p = 1.0_f64;
        for i in 0..n_up {
            snaps.push(mk(1000 + i * 180, p));
            p *= 1.005;
        }
        for i in n_up..(n_up + n_down) {
            snaps.push(mk(1000 + i * 180, p));
            p *= 0.95; // sharp drop → trips the 8% trail
        }
        snaps
    }

    #[test]
    fn replay_multi_n1_matches_single_slot_no_rotation() {
        // Anchor: at N=1 with rotation off, replay_multi is identical to replay_with_regime.
        let snaps = rise_then_fall("AAA", 130, 6);
        let watched = aaa();
        let params = bare_params(); // rotate_margin = 0
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];

        let single = replay_with_regime(&snaps, &watched, &stream, &params, &mask);
        let multi = replay_multi(&snaps, &watched, &stream, &params, &mask, 1);

        assert_eq!(multi.trades.len(), single.trades.len(), "same trade count");
        assert!(single.trades.len() >= 1, "fixture must produce ≥1 trade");
        for (m, s) in multi.trades.iter().zip(single.trades.iter()) {
            assert_eq!(m.mint, s.mint);
            assert_eq!(m.entry_ts, s.entry_ts);
            assert_eq!(m.exit_ts, s.exit_ts);
            assert!((m.usdc_in - s.usdc_in).abs() < 1e-9);
            assert!((m.usdc_out - s.usdc_out).abs() < 1e-9);
        }
        assert_eq!(multi.equity_curve, single.equity_curve, "equity curves identical");
    }

    #[test]
    fn replay_multi_n2_holds_two_distinct_tokens_at_once() {
        // Two tokens both rising → with N=2 both get held; with N=1 only one slot.
        let sol = 150.0;
        let mk = |ts: u64, a: f64, b: f64| {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), a);
            m.insert("BBB".to_string(), b);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None },
            WatchedToken { symbol: "BBB".into(), mint: "BBB".into(), name: None, equity: None },
        ];
        let mut snaps = Vec::new();
        let (mut a, mut b) = (1.0_f64, 1.0_f64);
        for i in 0..200u64 {
            snaps.push(mk(1000 + i * 180, a, b));
            a *= 1.004;
            b *= 1.003; // both rise the whole time → never trailing-stop
        }
        let params = bare_params(); // rotate off; both stay held to the end
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];

        // Count distinct mints entered by reading entries via a probe: re-run and inspect
        // open positions indirectly — here we assert N=2 enters BOTH names over the run.
        let n1 = replay_multi(&snaps, &watched, &stream, &params, &mask, 1);
        let n2 = replay_multi(&snaps, &watched, &stream, &params, &mask, 2);
        // Neither token ever stops (pure rise) → no closed trades in either run.
        // The observable difference is capital deployed, surfaced once positions close.
        // Force closure by appending a crash so both held positions exit.
        let mut crashed = snaps.clone();
        let (la, lb) = (a, b);
        for i in 200..210u64 {
            crashed.push(mk(1000 + i * 180, la * 0.5, lb * 0.5));
        }
        let stream_c = ranked_stream(&crashed, &watched, &params);
        let mask_c = vec![true; crashed.len()];
        let c1 = replay_multi(&crashed, &watched, &stream_c, &params, &mask_c, 1);
        let c2 = replay_multi(&crashed, &watched, &stream_c, &params, &mask_c, 2);
        let mints2: std::collections::HashSet<_> = c2.trades.iter().map(|t| t.mint.clone()).collect();
        assert_eq!(mints2.len(), 2, "N=2 holds and then closes BOTH AAA and BBB");
        assert_eq!(c1.trades.iter().map(|t| t.mint.clone()).collect::<std::collections::HashSet<_>>().len(), 1,
            "N=1 only ever holds one of them");
        let _ = (n1, n2);
    }

    #[test]
    fn replay_multi_never_holds_same_mint_twice() {
        // One token, N=3. It must occupy at most ONE slot — never duplicated.
        let snaps = rise_then_fall("AAA", 200, 0); // pure rise, stays held
        let watched = aaa();
        let params = bare_params();
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];
        // Append a crash to close whatever is held.
        let mut crashed = snaps.clone();
        let last = snaps.last().unwrap().prices["AAA"];
        for i in 0..6u64 {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), last * 0.9f64.powi(i as i32 + 1));
            m.insert(SOL_KEY.to_string(), 150.0);
            crashed.push(PriceSnapshot { ts: 1000 + (200 + i) * 180, prices: m });
        }
        let stream_c = ranked_stream(&crashed, &watched, &params);
        let mask_c = vec![true; crashed.len()];
        let run = replay_multi(&crashed, &watched, &stream_c, &params, &mask_c, 3);
        // Only ever one AAA position open at a time ⇒ at most one open→close per cycle.
        // With a single monotonic rise then one crash, exactly one trade closes.
        assert_eq!(run.trades.len(), 1, "AAA fills one slot, not three");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin momentum-sim replay_multi 2>&1 | tail -20`
Expected: FAIL — `cannot find function 'replay_multi' in this scope`.

- [ ] **Step 3: Implement `replay_multi` (fill-and-hold)**

Insert immediately after `replay_with_regime` (after its closing brace near line 642) in `src/portfolio/sim.rs`:

```rust
/// Multi-position generalization of [`replay_with_regime`]: hold up to `max_positions`
/// concurrent positions (fixed `trade_usdc` notional each, deduped by mint). At
/// `max_positions == 1` this is byte-identical to `replay_with_regime` (anchor test).
///
/// Per-tick order matches the single-slot path: stop-family exits → (eviction, added in a
/// later task) → fade exits → entries. A slot vacated by a conservative exit cannot be
/// refilled until *after* the bar the exit fills into (mirrors the single-slot
/// `i = fill_idx + 1`), enforced via `pending_free`.
pub fn replay_multi(
    snapshots: &[PriceSnapshot],
    watched: &[WatchedToken],
    stream: &[Vec<Candidate>],
    params: &ParamSet,
    regime: &[bool],
    max_positions: usize,
) -> SimRun {
    let n = snapshots.len();
    let mut trades: Vec<TradeRecord> = Vec::new();
    let mut equity_curve: Vec<(u64, f64)> = Vec::new();
    if let Some(first) = snapshots.first() {
        equity_curve.push((first.ts, 0.0));
    }
    let mut realized = 0.0_f64;
    let mut held: Vec<Position> = Vec::new();
    let mut last_exit_ts: HashMap<String, i64> = HashMap::new();
    let mut entry_tss: Vec<i64> = Vec::new();
    // Tick indices at which a vacated slot's capacity returns. Capacity is withheld while
    // `free_at > i`, so we never re-enter on the bar a conservative exit sold into.
    let mut pending_free: Vec<usize> = Vec::new();

    for i in 0..n {
        let snap = &snapshots[i];
        let ts = snap.ts as i64;
        let sol_price = snap.prices.get(SOL_KEY).copied().unwrap_or(0.0);

        // ── HOLDING: evaluate every open position for a stop-family exit ──
        let mut survivors: Vec<Position> = Vec::with_capacity(held.len());
        for mut pos in held.drain(..) {
            let Some(px) = snap.prices.get(&pos.mint).copied().filter(|p| *p > 0.0) else {
                survivors.push(pos); // no fresh price — never trip a stop on a gap
                continue;
            };
            if px > pos.peak_price_usd {
                pos.peak_price_usd = px;
            }
            let fallback_stop = vol_stop_triggered(
                px,
                pos.peak_price_usd,
                params.trail_pct,
                params.vol_stop_mode,
                params.chandelier_k,
                token_atr(snapshots, i, &pos.mint, params.vol_obs),
                token_return_sigma(snapshots, i, &pos.mint, params.vol_obs),
            );
            let gas_bps = est_gas_bps(params.trade_usdc, sol_price);
            let round_trip_cost_frac = (2 * params.slippage_bps + 2 * gas_bps) as f64 / 10_000.0;
            let stop = profit_protected_stop_triggered(
                px,
                pos.peak_price_usd,
                pos.entry_price_usd,
                round_trip_cost_frac,
                params.max_trail_pct,
                fallback_stop,
            );
            let overbought = params.overbought_z > 0.0
                && px > pos.entry_price_usd
                && token_dip_z(snapshots, i, &pos.mint, params.vol_obs)
                    .is_some_and(|z| z >= params.overbought_z);
            let is_equity = watched.iter().any(|w| w.mint == pos.mint && w.is_equity());
            let market_closed = is_equity
                && params.stale_minutes > 0
                && is_stale_ts(&recent_series(snapshots, i, &pos.mint), params.stale_minutes);
            let max_hold_hit = params.max_hold_min > 0
                && (ts - pos.entry_ts) >= params.max_hold_min as i64 * 60;
            let breakeven_hit = params.breakeven_exit
                && pos.peak_price_usd > pos.entry_price_usd
                && px <= pos.entry_price_usd;

            if stop || market_closed || overbought || max_hold_hit || breakeven_hit {
                let (fill_idx, exit_mark, exit_ts, exit_sol) = if params.optimistic_fill {
                    (i, px, snap.ts, sol_price)
                } else {
                    let fi = (i + 1).min(n - 1);
                    let fs = &snapshots[fi];
                    let mark = fs.prices.get(&pos.mint).copied().filter(|p| *p > 0.0).unwrap_or(px);
                    (fi, mark, fs.ts, fs.prices.get(SOL_KEY).copied().unwrap_or(sol_price))
                };
                let proceeds = pos.token_amount * exit_fill_price(exit_mark, params.slippage_bps);
                let usdc_out = (proceeds - est_gas_usdc(exit_sol)).max(0.0);
                let rec = build_trade_record(&pos, exit_ts as i64, exit_mark, usdc_out, "sim".into());
                realized += rec.usdc_out - rec.usdc_in;
                last_exit_ts.insert(pos.mint.clone(), exit_ts as i64);
                equity_curve.push((exit_ts, realized));
                trades.push(rec);
                pending_free.push(fill_idx + 1); // capacity returns AFTER the fill bar
                continue;
            }
            survivors.push(pos);
        }
        held = survivors;

        // ── Eviction hook (rotation) — added in Task 2; nothing here yet ──

        // ── Fade exit: independent per remaining position (slow-tick, fills at mark) ──
        if params.exit_on_fade {
            let mut after_fade: Vec<Position> = Vec::with_capacity(held.len());
            for pos in held.drain(..) {
                let px = snap.prices.get(&pos.mint).copied().filter(|p| *p > 0.0);
                let faded = match (px, stream[i].iter().find(|c| c.mint == pos.mint)) {
                    (Some(px), Some(c)) => {
                        !c.stale
                            && fade_take_profit(c.score, params.min_metric, px, pos.entry_price_usd)
                    }
                    _ => false,
                };
                if let (true, Some(px)) = (faded, px) {
                    let proceeds = pos.token_amount * exit_fill_price(px, params.slippage_bps);
                    let usdc_out = (proceeds - est_gas_usdc(sol_price)).max(0.0);
                    let rec = build_trade_record(&pos, ts, px, usdc_out, "sim".into());
                    realized += rec.usdc_out - rec.usdc_in;
                    last_exit_ts.insert(pos.mint.clone(), ts);
                    equity_curve.push((snap.ts, realized));
                    trades.push(rec);
                    pending_free.push(i + 1); // fade fills same-bar → free next bar
                    continue;
                }
                after_fade.push(pos);
            }
            held = after_fade;
        }

        // ── Entries: greedily fill free capacity, best-ranked first ──
        if !regime[i] {
            continue; // risk-off → no entries this bar
        }
        pending_free.retain(|&f| f > i); // expire returned capacity
        let withheld = pending_free.len();
        let mut capacity = max_positions.saturating_sub(held.len() + withheld);
        while capacity > 0 {
            let cutoff = ts - 86_400;
            let used = entry_tss.iter().filter(|&&e| e >= cutoff).count();
            if used >= params.max_trades_per_day as usize {
                break;
            }
            let best = stream[i].iter().find(|c| {
                !c.stale
                    && !c.overextended
                    && !c.falling
                    && !c.metric_fading
                    && !held.iter().any(|p| p.mint == c.mint)
                    && last_exit_ts
                        .get(&c.mint)
                        .is_none_or(|&last| ts - last >= params.reentry_cooldown_secs)
            });
            let Some(best) = best else { break };
            if best.score <= params.min_metric {
                break;
            }
            if params.entry_dip_obs > 0 {
                let oversold = token_dip_z(snapshots, i, &best.mint, params.entry_dip_obs)
                    .is_some_and(|z| z <= -params.entry_dip_z);
                let bouncing = token_rising(snapshots, i, &best.mint, params.dip_confirm_obs);
                if !oversold || !bouncing {
                    break;
                }
            }
            let size = dynamic_trade_usdc(
                params.trade_usdc,
                params.reinvest_frac,
                params.size_ceiling_usdc,
                realized,
            );
            let gas_bps = est_gas_bps(size, sol_price);
            if params.slippage_bps + gas_bps > params.max_cost_bps {
                break;
            }
            let entry_mark = best.price_usd;
            let token_amount = size / entry_fill_price(entry_mark, params.slippage_bps);
            held.push(Position {
                mint: best.mint.clone(),
                symbol: best.symbol.clone(),
                entry_ts: ts,
                entry_price_usd: entry_mark,
                token_amount,
                usdc_spent: size + est_gas_usdc(sol_price),
                peak_price_usd: entry_mark,
                entry_sig: "sim".into(),
                dry_run: true,
            });
            entry_tss.push(ts);
            capacity -= 1;
        }
    }

    SimRun { trades, equity_curve }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --bin momentum-sim replay_multi 2>&1 | tail -20`
Expected: PASS — `replay_multi_n1_matches_single_slot_no_rotation`, `replay_multi_n2_holds_two_distinct_tokens_at_once`, `replay_multi_never_holds_same_mint_twice` all green.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): replay_multi engine (fill-and-hold) for max-N positions

Multi-position generalization of replay_with_regime holding Vec<Position>
capped at N, fixed notional per slot, deduped by mint. Byte-identical to
the single-slot replay at N=1 (anchor test). Eviction/rotation added next.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 2: Weakest-green eviction (portfolio rotation)

**Files:**
- Modify: `src/portfolio/sim.rs` (fill in the "Eviction hook" placed in Task 1)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: `rotation_target`, `rotation_net_green`, `est_gas_bps`, `est_gas_usdc`, `exit_fill_price`, `build_trade_record` (all in scope).
- Produces: no new public symbol — extends `replay_multi`'s behavior when `rotate_margin > 0`.

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn replay_multi_n1_matches_single_slot_with_rotation() {
        // Anchor #2: at N=1 with rotation ON, eviction reduces to single-slot try_rotate.
        // Reuse the exact fixture from replay_rotates_into_a_stronger_token.
        let sol = 150.0;
        let mk = |ts: u64, a: f64, b: f64| {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), a);
            m.insert("BBB".to_string(), b);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None },
            WatchedToken { symbol: "BBB".into(), mint: "BBB".into(), name: None, equity: None },
        ];
        let mut snaps = Vec::new();
        let (mut a, mut b) = (1.0_f64, 1.0_f64);
        for i in 0..128u64 {
            snaps.push(mk(1000 + i * 180, a, b));
            a *= 1.004;
            b *= 1.001;
        }
        for i in 128..220u64 {
            snaps.push(mk(1000 + i * 180, a, b));
            a *= 1.004;
            b *= 1.03;
        }
        let mut params = bare_params();
        params.metric = RankMetric::Return;
        params.trail_pct = 8.0;
        params.rotate_margin = 0.10;
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];

        let single = replay_with_regime(&snaps, &watched, &stream, &params, &mask);
        let multi = replay_multi(&snaps, &watched, &stream, &params, &mask, 1);

        assert!(single.trades.len() >= 1, "fixture must rotate at least once");
        assert_eq!(multi.trades.len(), single.trades.len());
        for (m, s) in multi.trades.iter().zip(single.trades.iter()) {
            assert_eq!(m.mint, s.mint);
            assert_eq!(m.entry_ts, s.entry_ts);
            assert_eq!(m.exit_ts, s.exit_ts);
            assert!((m.usdc_out - s.usdc_out).abs() < 1e-9);
        }
        assert_eq!(multi.equity_curve, single.equity_curve);
    }

    #[test]
    fn replay_multi_evicts_weakest_green_when_full() {
        // N=1, both slots conceptually full with AAA; BBB rockets past the margin →
        // the held AAA (weakest-and-only green) is evicted. First closed trade is AAA.
        let sol = 150.0;
        let mk = |ts: u64, a: f64, b: f64| {
            let mut m = HashMap::new();
            m.insert("AAA".to_string(), a);
            m.insert("BBB".to_string(), b);
            m.insert(SOL_KEY.to_string(), sol);
            PriceSnapshot { ts, prices: m }
        };
        let watched = vec![
            WatchedToken { symbol: "AAA".into(), mint: "AAA".into(), name: None, equity: None },
            WatchedToken { symbol: "BBB".into(), mint: "BBB".into(), name: None, equity: None },
        ];
        let mut snaps = Vec::new();
        let (mut a, mut b) = (1.0_f64, 1.0_f64);
        for i in 0..128u64 {
            snaps.push(mk(1000 + i * 180, a, b));
            a *= 1.004;
            b *= 1.001;
        }
        for i in 128..220u64 {
            snaps.push(mk(1000 + i * 180, a, b));
            a *= 1.004;
            b *= 1.03;
        }
        let mut params = bare_params();
        params.metric = RankMetric::Return;
        params.trail_pct = 8.0;
        params.rotate_margin = 0.10;
        let stream = ranked_stream(&snaps, &watched, &params);
        let mask = vec![true; snaps.len()];
        let run = replay_multi(&snaps, &watched, &stream, &params, &mask, 1);
        assert!(run.trades.len() >= 1, "eviction should close the weakest leg");
        assert_eq!(run.trades[0].mint, "AAA", "evicted weakest-green is AAA");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --bin momentum-sim replay_multi_n1_matches_single_slot_with_rotation replay_multi_evicts 2>&1 | tail -20`
Expected: FAIL — `replay_multi_evicts_weakest_green_when_full` finds 0 trades (no eviction yet) and the rotation anchor mismatches trade counts.

- [ ] **Step 3: Implement eviction in `replay_multi`**

Replace the placeholder line in `replay_multi`:

```rust
        // ── Eviction hook (rotation) — added in Task 2; nothing here yet ──
```

with:

```rust
        // ── Eviction: when full and rotation on, swap the weakest GREEN held for a
        // stronger candidate (portfolio generalization of single-slot try_rotate). ──
        if params.rotate_margin > 0.0 && max_positions > 0 && held.len() == max_positions {
            let used = entry_tss.iter().filter(|&&e| e >= ts - 86_400).count();
            if used < params.max_trades_per_day as usize {
                // Weakest green held: lowest current score among net-green, priced, non-stale.
                let mut weakest: Option<(usize, f64)> = None;
                for (idx, pos) in held.iter().enumerate() {
                    let Some(px) = snap.prices.get(&pos.mint).copied().filter(|p| *p > 0.0) else {
                        continue;
                    };
                    if px <= pos.entry_price_usd {
                        continue; // gross-green pre-filter (mirror try_rotate)
                    }
                    let Some(c) = stream[i].iter().find(|c| c.mint == pos.mint) else { continue };
                    if c.stale {
                        continue;
                    }
                    if weakest.map_or(true, |(_, s)| c.score < s) {
                        weakest = Some((idx, c.score));
                    }
                }
                if let Some((idx, held_score)) = weakest {
                    let px = snapshots[i].prices[&held[idx].mint]; // present per filter above
                    let target = rotation_target(
                        &stream[i],
                        &held[idx].mint,
                        held_score,
                        params.min_metric,
                        params.rotate_margin,
                        params.reentry_cooldown_secs,
                        ts,
                        &last_exit_ts,
                    );
                    if let Some(target) = target {
                        let already_held = held.iter().any(|p| p.mint == target.mint);
                        let notional = held[idx].token_amount * px;
                        let gas_bps = est_gas_bps(notional, sol_price);
                        let cost_bps = params.slippage_bps + gas_bps;
                        if !already_held
                            && cost_bps <= params.max_cost_bps
                            && rotation_net_green(px, held[idx].entry_price_usd, cost_bps)
                        {
                            let pos = held.remove(idx);
                            let b_value = pos.token_amount * exit_fill_price(px, params.slippage_bps);
                            let realized_a = (b_value - est_gas_usdc(sol_price)).max(0.0);
                            let rec = build_trade_record(&pos, ts, px, realized_a, "sim-rotate".into());
                            realized += rec.usdc_out - rec.usdc_in;
                            last_exit_ts.insert(pos.mint.clone(), ts);
                            equity_curve.push((snap.ts, realized));
                            trades.push(rec);
                            held.push(Position {
                                mint: target.mint.clone(),
                                symbol: target.symbol.clone(),
                                entry_ts: ts,
                                entry_price_usd: target.price_usd,
                                token_amount: b_value / target.price_usd,
                                usdc_spent: b_value,
                                peak_price_usd: target.price_usd,
                                entry_sig: "sim-rotate".into(),
                                dry_run: true,
                            });
                            entry_tss.push(ts); // rotation counts against the daily cap
                        }
                    }
                }
            }
        }
```

- [ ] **Step 4: Run the full sim test suite to verify everything passes**

Run: `cargo test --bin momentum-sim 2>&1 | tail -25`
Expected: PASS — all tests, including both anchors (`replay_multi_n1_matches_single_slot_no_rotation`, `replay_multi_n1_matches_single_slot_with_rotation`), `replay_multi_evicts_weakest_green_when_full`, and the pre-existing suite (no regressions).

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): weakest-green eviction in replay_multi (portfolio rotation)

When N slots are full and rotate_margin>0, evict the weakest net-green
held position for a candidate beating its score by the margin. Reduces
to single-slot try_rotate at N=1 (second anchor test).

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: `maxn_rows` driver in sim.rs

**Files:**
- Modify: `src/portfolio/sim.rs` (add `MaxnRow` struct + `maxn_rows` fn after `replay_multi`)
- Test: `src/portfolio/sim.rs` `#[cfg(test)]` block

**Interfaces:**
- Consumes: `ranked_stream`, `replay_multi`, `regime_mask`, `replay_with_regime`.
- Produces:
  - `pub struct MaxnRow { pub n: usize, pub pnl_train: f64, pub pnl_test: f64, pub trades_test: usize, pub win_test: f64, pub dd_test: f64 }`
  - `pub fn maxn_rows(train: &[PriceSnapshot], test: &[PriceSnapshot], watched: &[WatchedToken], params: &ParamSet, regime_obs: usize, max_n: usize) -> Vec<MaxnRow>`

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn maxn_rows_n1_row_matches_single_slot_and_len_is_max_n() {
        let snaps = rise_then_fall("AAA", 130, 6);
        let watched = aaa();
        let params = bare_params();
        let split = (snaps.len() as f64 * 0.7) as usize;
        let (train, test) = snaps.split_at(split);

        let rows = maxn_rows(train, test, &watched, &params, 0, 3);
        assert_eq!(rows.len(), 3, "one row per N in 1..=3");
        assert_eq!(rows[0].n, 1);
        assert_eq!(rows[2].n, 3);

        // The N=1 row must equal a direct single-slot replay on the test slice.
        let stream_te = ranked_stream(test, &watched, &params);
        let mask_te = vec![true; test.len()];
        let single_te = replay_with_regime(test, &watched, &stream_te, &params, &mask_te);
        assert!((rows[0].pnl_test - single_te.net_pnl()).abs() < 1e-9, "N=1 pnl_test == single-slot");
        assert_eq!(rows[0].trades_test, single_te.n_trades());
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --bin momentum-sim maxn_rows 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'maxn_rows'` / `cannot find type 'MaxnRow'`.

- [ ] **Step 3: Implement `MaxnRow` + `maxn_rows`**

Insert after `replay_multi` in `src/portfolio/sim.rs`:

```rust
/// One row of a max-N comparison: a single config replayed at a fixed `n`.
#[derive(Debug, Clone)]
pub struct MaxnRow {
    pub n: usize,
    pub pnl_train: f64,
    pub pnl_test: f64,
    pub trades_test: usize,
    pub win_test: f64,
    pub dd_test: f64,
}

/// Replay ONE fixed config at `n = 1..=max_n` over train and test slices, returning a row
/// per N. The ranked stream is built once per slice and shared across all N (only the slot
/// cap changes). `regime_obs == 0` disables the level regime gate; otherwise SOL>MA over
/// `regime_obs` obs gates entries (trend regime is out of scope for this comparison).
pub fn maxn_rows(
    train: &[PriceSnapshot],
    test: &[PriceSnapshot],
    watched: &[WatchedToken],
    params: &ParamSet,
    regime_obs: usize,
    max_n: usize,
) -> Vec<MaxnRow> {
    let s_tr = ranked_stream(train, watched, params);
    let s_te = ranked_stream(test, watched, params);
    let mask = |s: &[PriceSnapshot]| -> Vec<bool> {
        if regime_obs == 0 {
            vec![true; s.len()]
        } else {
            regime_mask(s, regime_obs)
        }
    };
    let m_tr = mask(train);
    let m_te = mask(test);
    (1..=max_n.max(1))
        .map(|nn| {
            let r_tr = replay_multi(train, watched, &s_tr, params, &m_tr, nn);
            let r_te = replay_multi(test, watched, &s_te, params, &m_te, nn);
            MaxnRow {
                n: nn,
                pnl_train: r_tr.net_pnl(),
                pnl_test: r_te.net_pnl(),
                trades_test: r_te.n_trades(),
                win_test: r_te.win_rate(),
                dd_test: r_te.max_drawdown_pct(),
            }
        })
        .collect()
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --bin momentum-sim maxn_rows 2>&1 | tail -15`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): maxn_rows driver — replay one config at N=1..K per slice

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: `maxn-compare` CLI subcommand

**Files:**
- Modify: `src/bin/momentum_sim.rs` (add `MaxnCompare` command variant ~line 271, its match arm ~line 388, `MaxnCompareArgs` struct + `maxn_compare` fn after `regime_compare` ~line 724)

**Interfaces:**
- Consumes: `sim::maxn_rows`, `sim::MaxnRow`, `base_params`, `sim::sanitize_history`, `history::load_history`, `momentum_universe::load`, `PortfolioConfig`, `RankMetric`.
- Produces: a runnable `momentum-sim maxn-compare ...` subcommand printing a per-N table.

- [ ] **Step 1: Add the `MaxnCompare` command variant**

In `src/bin/momentum_sim.rs`, inside the `enum Command { ... }` (after the `RegimeCompare { ... }` variant, before the closing `}` near line 272), add:

```rust
    /// Compare holding up to N concurrent positions (N=1..max_n) under ONE fixed config,
    /// isolating the max-positions effect on held-out P&L. Fixed notional per slot, so the
    /// table also reports P&L per $1k deployed (higher N deploys more capital).
    MaxnCompare {
        #[arg(long, default_value_t = 0.70)]
        train_frac: f64,
        #[arg(long)]
        tokens: Option<String>,
        #[arg(long)]
        history: Option<String>,
        #[arg(long, default_value_t = 8.0)]
        max_step: f64,
        #[arg(long, default_value = "slope_r2")]
        metric: String,
        #[arg(long, default_value_t = 240)]
        lookback: usize,
        #[arg(long, default_value_t = 12.0)]
        trail: f64,
        #[arg(long, default_value_t = 0.0)]
        max_run: f64,
        #[arg(long, default_value_t = 0.0)]
        min_metric: f64,
        /// Rotation margin in the metric's units (>0 enables weakest-green eviction). 0 = off.
        #[arg(long, default_value_t = 0.0)]
        rotate_margin: f64,
        #[arg(long, default_value_t = 1000.0)]
        trade_usdc: f64,
        /// Level regime gate: only enter when SOL is above its N-obs MA. 0 = off.
        #[arg(long, default_value_t = 0)]
        regime_obs: usize,
        /// Maximum number of concurrent positions to sweep up to (rows N=1..max_n).
        #[arg(long, default_value_t = 5)]
        max_n: usize,
    },
```

- [ ] **Step 2: Add the match arm**

In `fn main()`, in the `match cli.command { ... }`, after the `Command::RegimeCompare { ... } => { ... }` arm (before the closing `}` of the match near line 388), add:

```rust
        Command::MaxnCompare {
            train_frac, tokens, history, max_step, metric, lookback, trail, max_run,
            min_metric, rotate_margin, trade_usdc, regime_obs, max_n,
        } => {
            let m = metric.parse::<RankMetric>().map_err(|e| anyhow::anyhow!("bad --metric: {e}"))?;
            maxn_compare(MaxnCompareArgs {
                cfg: &cfg, train_frac, tokens, history_override: history, max_step, metric: m,
                lookback, trail, max_run, min_metric, rotate_margin, trade_usdc, regime_obs, max_n,
            })
        }
```

- [ ] **Step 3: Add `MaxnCompareArgs` + `maxn_compare`**

After the `regime_compare` function (after its closing brace, before `fn base_params`, near line 724) in `src/bin/momentum_sim.rs`, add:

```rust
struct MaxnCompareArgs<'a> {
    cfg: &'a PortfolioConfig,
    train_frac: f64,
    tokens: Option<String>,
    history_override: Option<String>,
    max_step: f64,
    metric: RankMetric,
    lookback: usize,
    trail: f64,
    max_run: f64,
    min_metric: f64,
    rotate_margin: f64,
    trade_usdc: f64,
    regime_obs: usize,
    max_n: usize,
}

/// Replay one fixed config at N=1..max_n and print a per-N table. Capital is fixed per
/// slot, so the table reports both absolute test P&L and P&L per $1k deployed (= pnl_test
/// / (N × trade_usdc / 1000)) — N>1 must win per-dollar, not merely by deploying more.
fn maxn_compare(a: MaxnCompareArgs) -> Result<()> {
    let MaxnCompareArgs {
        cfg, train_frac, tokens, history_override, max_step, metric, lookback, trail, max_run,
        min_metric, rotate_margin, trade_usdc, regime_obs, max_n,
    } = a;
    anyhow::ensure!(train_frac > 0.0 && train_frac < 1.0, "--train-frac must be in (0,1)");
    anyhow::ensure!(max_n >= 1, "--max-n must be ≥ 1");

    let history_path = history_override.unwrap_or_else(|| cfg.history_path.clone());
    let tokens_path = tokens.unwrap_or_else(|| cfg.momentum_tokens_path.clone());
    let raw: Vec<_> = history::load_history(Path::new(&history_path))
        .with_context(|| format!("loading {history_path}"))?
        .into_iter()
        .collect();
    let snapshots = sim::sanitize_history(&raw, max_step);
    anyhow::ensure!(snapshots.len() >= 200, "only {} snapshots — need more history", snapshots.len());
    let watched = momentum_universe::load(Path::new(&tokens_path))
        .with_context(|| format!("loading {tokens_path}"))?;
    let split = (snapshots.len() as f64 * train_frac) as usize;
    let (train, test) = snapshots.split_at(split);

    let mut base = base_params(cfg);
    base.metric = metric;
    base.lookback_obs = lookback;
    base.trail_pct = trail;
    base.max_run_pct = max_run;
    base.min_metric = min_metric;
    base.trade_usdc = trade_usdc;
    base.size_ceiling_usdc = trade_usdc; // fixed notional per slot (no compounding here)
    base.reinvest_frac = 0.0;
    base.rotate_margin = rotate_margin;
    base.regime_filter_obs = 0; // mask is supplied via regime_obs in maxn_rows; don't double-gate

    let span_days = |s: &[_]| s.len() as f64 * 184.0 / 86_400.0;
    println!(
        "Max-N comparison — metric={metric} lookback={lookback} trail={trail}% max_run={max_run}% \
         min_metric={min_metric} rotate_margin={rotate_margin} trade_usdc={trade_usdc} regime_obs={regime_obs}"
    );
    println!(
        "Loaded {} snapshots (max_step={max_step}×). Train {} (~{:.1}d) / Test {} (~{:.1}d). {} tokens.\n",
        snapshots.len(), train.len(), span_days(train), test.len(), span_days(test), watched.len()
    );

    let rows = sim::maxn_rows(train, test, &watched, &base, regime_obs, max_n);

    println!(
        "{:>3} {:>10} {:>10} {:>7} {:>6} {:>8} {:>16}",
        "N", "pnl_train", "pnl_test", "trd_te", "win%", "maxDD%", "pnl_test/$1k"
    );
    println!("{}", "─".repeat(66));
    for r in &rows {
        let deployed_k = (r.n as f64) * trade_usdc / 1000.0;
        let per_k = if deployed_k > 0.0 { r.pnl_test / deployed_k } else { 0.0 };
        println!(
            "{:>3} {:>+10.2} {:>+10.2} {:>7} {:>5.0}% {:>7.1}% {:>+16.2}",
            r.n, r.pnl_train, r.pnl_test, r.trades_test, r.win_test, r.dd_test.abs(), per_k
        );
    }
    println!(
        "\nRead: N>1 earns its place only if pnl_test/$1k rises with N (not just absolute pnl_test, \
         which grows because higher N deploys more capital). Treat a short sample as suggestive, not proven."
    );
    Ok(())
}
```

- [ ] **Step 4: Build and smoke-test the subcommand**

Run:
```bash
cargo build --release --bin momentum-sim 2>&1 | tail -5
target/release/momentum-sim maxn-compare \
  --metric sharpe --min-metric 0.0377 --trail 20 --lookback 480 --max-run 6 \
  --trade-usdc 1000 --max-n 5
```
Expected: builds clean; prints the header, the Train/Test split line, and a 5-row table (N=1..5) with a `pnl_test/$1k` column. The N=1 row's `pnl_test` matches the single-slot number from `per-token`/`regime-compare` for the same config.

- [ ] **Step 5: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(sim): maxn-compare subcommand — measure max-N vs single-slot

Replays one fixed config at N=1..max_n over the curated universe and prints
per-N P&L / trades / win% / maxDD plus P&L-per-\$1k-deployed (so N>1 must win
per dollar, not by deploying more). Sim-only; no live or .env change.

Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>"
```

---

## Self-Review

**1. Spec coverage:**
- Sim-only, additive, approach A → Tasks 1–4 only touch `sim.rs` + the bin; production grid untouched. ✓
- Fixed notional per slot → `dynamic_trade_usdc` with `reinvest_frac=0`, `size_ceiling=trade_usdc` in `maxn_compare`; entry path unchanged. ✓
- Weakest-green eviction gated on `rotate_margin>0` → Task 2. ✓
- `maxn-compare` subcommand mirroring `regime-compare` → Task 4. ✓
- N=1 ≡ single-slot anchor (rotate=0 and rotate>0) → Tasks 1 & 2. ✓
- Per-$1k normalization column → Task 4. ✓
- Dedup, daily cap across slots → Task 1 (`!held.iter().any(...)`, `entry_tss` shared). ✓
- Drawdown realized-equity based (spec wording corrected from "summed unrealized" to preserve the anchor) → Global Constraints + reuse of `SimRun.max_drawdown_pct`. ✓ **(Action: update the spec's "Accounting" bullet to match — see note below.)**

**2. Placeholder scan:** No TBD/TODO; every code step contains complete code; every test step contains real assertions. ✓

**3. Type consistency:** `replay_multi` signature identical across Tasks 1–3 and the `maxn_rows` call. `MaxnRow` fields (`n, pnl_train, pnl_test, trades_test, win_test, dd_test`) used identically in Task 3 and Task 4's printer. `MaxnCompareArgs` fields match the command variant and match arm. ✓

**Spec correction needed:** the design doc's `replay_multi` "Accounting" bullet says drawdown is "summed unrealized across concurrently-held positions." That conflicts with the N=1 byte-identical anchor (the existing metric is realized-equity only). The implementation keeps the realized-equity curve. Update that bullet in `docs/superpowers/specs/2026-06-28-momentum-maxn-sim-design.md` to: *"`max_drawdown_pct` is the existing realized-equity-curve drawdown, now fed by all N positions' realized P&L — a portfolio drawdown of realized equity."*
