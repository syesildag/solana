# Design: `--objective pnl-per-hold` for `momentum-sim run`

**Date:** 2026-07-02  
**Objective:** Add a new ranking objective to the momentum backtester that prioritizes capital efficiency (PnL per hour deployed) over absolute total PnL, exposing fast, profitable turnovers while filtering out "slow money" configs.

---

## 1. Problem Statement

The current `momentum-sim run` grid search ranks configs by **best absolute net PnL** on the held-out test slice (`net_pnl_test`), then filters "robust" (profitable in both train and test, ≥min_trades). This finds the most profitable strategy overall.

However, this doesn't distinguish between:
- Config A: +$10 PnL over 60 hours of holding → $0.167/hour efficiency
- Config B: +$8 PnL over 5 hours of holding → $1.6/hour efficiency

For portfolio management, **capital efficiency** (PnL per time-in-market) matters as much as absolute return: a 10x faster turnover on 80% of the PnL is often preferable because the capital recycles.

**Goal:** Enable ranking configs by PnL-per-hour deployed (worst-slice rate, mirroring the existing robustness philosophy) while keeping absolute PnL visible for context.

---

## 2. Metric Definition

### Rate Calculation
For each `SimRun` (train or test slice):

```
hold_hours = Σ max(0, exit_ts − entry_ts) / 3600  [seconds → hours]
rate_$/h = net_pnl / hold_hours  [$/hour deployed]
```

Guard: if `hold_hours ≤ 0`, rate = 0.0 (sinks to bottom of rankings).

### Selection Key ("Dependability")

The winning config is the one with the **best worst-slice rate**:

```
dependability = min(rate_train, rate_test)
```

Rationale: This mirrors the existing selection philosophy. A config that is *efficient in both slices* is more robust than one that lucked into high turnover in just the test set. The worst-slice guard prevents configs from gaming a 30-second winning trade.

### Robustness Gate
Unchanged from today:
```
robust ⟺ net_pnl_train > 0 ∧ net_pnl_test > 0 ∧ n_trades_train ≥ min_trades ∧ n_trades_test ≥ min_trades
```

(PnL > 0 and hold_hours > 0 together guarantee rate > 0, so no new gate needed.)

---

## 3. User-Facing Interface

### New Command-Line Flag
```bash
cargo run --release --bin momentum-sim -- run \
  --objective net-pnl       # default (current behavior)
  --objective pnl-per-hold  # new: rank by $/hour
  [other flags unchanged]
```

- **Default:** `net-pnl` (backward-compatible; existing scripts and documented findings unaffected).
- **Error handling:** If user passes `--objective pnl-per-hold` with `--strategy meanrev|pairs|relval|relstrength`, reject with a clear message (those strategies aren't implemented for the new objective).

### Output Table
The existing `print_table()` output is extended with two columns:

```
metric    train_pnl  train_trd  test_pnl  test_trd  test_win%  test_$/h  test_hold_h  max_dd%
sortino     +45.20       12      +38.50       11      54.5%      7.70       5.0        8.2%
```

- `test_$/h` — held-out rate (=test_pnl / hold_hours_test).
- `test_hold_h` — total hours deployed in the test slice.
- When `--objective net-pnl`, rows are sorted by `test_pnl` (unchanged).
- When `--objective pnl-per-hold`, rows are sorted by `min(rate_train, rate_test)` (new).

Header line includes a note: `"(sorted by worst-slice $/h)"` or `"(sorted by test_pnl)"` depending on the objective.

### `.env` Block
The winning config is printed as before. A one-line comment is added to note the selection criterion:

```
# Selected via: momentum-sim run --objective pnl-per-hold
MOMENTUM_METRIC=sortino
MOMENTUM_MIN_METRIC=0.15
...
```

---

## 4. Implementation Scope

### New Methods & Fields

**`sim.rs`:**
- `SimRun::total_hold_hours() → f64` — sums `max(0, exit_ts − entry_ts)/3600` over all trades.
- `SimResult` — add three fields:
  - `hold_hours_train: f64`
  - `hold_hours_test: f64`
  - Methods: `rate_train() → f64` and `rate_test() → f64` (guard: hold_hours ≤ 0 → 0.0).

**`momentum_sim.rs`:**
- `enum Objective { NetPnl, PnlPerHold }` — clap-derived from `--objective` flag.
- Rename `worst_slice(r: &SimResult) → f64` to `dependability(r: &SimResult, obj: Objective) → f64`:
  - `NetPnl` → `r.net_pnl_train.min(r.net_pnl_test)`
  - `PnlPerHold` → `r.rate_train().min(r.rate_test())`
- Update `run_grid()` calls to populate `hold_hours_train/test` from `SimRun::total_hold_hours()`.
- Update the robust sort: `robust.sort_by(|a, b| dependability(b, obj).partial_cmp(&dependability(a, obj))...)`.
- Update `print_table()` to take the objective and add `test_$/h` and `test_hold_h` columns.

### Scope Boundaries (NOT in this task)
- **meanrev/pairs/relval/relstrength:** Keep net-PnL only. Pass objective enum down, but these branches error if pnl-per-hold is requested.
- **New grid axes:** No. We're only changing the ranking criterion, not the parameter sweep.
- **Regression tests:** The existing grid logic is untouched; only the sort key changes.

---

## 5. Edge Cases & Guards

| Case | Behavior |
|------|----------|
| `hold_hours = 0` (no trades closed in slice) | `rate = 0.0` → sinks to bottom |
| Config A: +$0.50 in 1000 hours vs B: +$5 in 0.5 hours | A: 0.0005 $/h, B: 10 $/h → B ranked higher (correct for efficiency) |
| Robust filter on worst-slice rate | Same robustness gate (pnl > 0 in both slices); if a config makes money, rate ≥ 0 is guaranteed |
| `--objective pnl-per-hold --strategy meanrev` | Error: "pnl-per-hold not supported for meanrev; use meanrev with --objective net-pnl" |

---

## 6. Testing Strategy

**Unit tests (sim.rs):**
1. `total_hold_hours()` — sum of durations, handles zero-duration trades, guard on negative durations.
2. `rate_train()` / `rate_test()` — correct division, guard on zero hold_hours.
3. **Ranking decision test:** Two configs where net-PnL picks A but pnl-per-hold picks B, confirming the objective actually swaps the winner.

**Integration test (momentum_sim.rs):**
- Run grid with `--objective pnl-per-hold` on a small dataset and verify output table columns and sort order match expectations.

---

## 7. Backward Compatibility

- Default `--objective net-pnl` makes this opt-in; no behavior change for existing workflows.
- All existing `.env` comments, hardcoded param sets, and scripts continue to work.
- The new columns in `print_table()` are additions; existing column order is preserved.

---

## 8. Open Questions (Resolved)

✅ **Metric formula:** Total PnL ÷ total time-in-market (not per-trade mean or scaled variant).  
✅ **Objective exposure:** New `--objective` flag (not replacing default or a separate subcommand).  
✅ **Guardrail against flukes:** Keep existing robustness gate + show both $/h and absolute PnL in output (visual inspection; no magic floor).  
✅ **Selection key:** Worst-slice rate `min(rate_train, rate_test)` (consistent with existing philosophy).  
✅ **Rate units:** $/hour.

---

## 9. Success Criteria

1. ✅ `momentum-sim run --objective pnl-per-hold` ranks configs by worst-slice $/h.
2. ✅ Output table shows `test_$/h` and `test_hold_h` columns.
3. ✅ `.env` block is written from `robust[0]` (the most capital-efficient config).
4. ✅ Default `--objective net-pnl` preserves existing behavior (tests pass, existing docs valid).
5. ✅ Edge cases (zero hold_hours, negative durations) are handled safely.
6. ✅ meanrev/pairs/relval/relstrength reject pnl-per-hold with clear error.
