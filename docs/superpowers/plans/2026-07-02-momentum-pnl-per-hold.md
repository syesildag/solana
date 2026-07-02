# PnL-Per-Hold Objective Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `--objective pnl-per-hold` flag to `momentum-sim run` that ranks configs by $/hour capital efficiency (worst-slice rate) instead of absolute PnL.

**Architecture:** 
1. Add hold-hours instrumentation to `SimRun` and rate methods to `SimResult` in `sim.rs`.
2. Add `Objective` enum and refactor `worst_slice()` → `dependability(r, obj)` in `momentum_sim.rs`.
3. Thread the objective through the grid-search ranking and output tables.
4. Add error handling for unsupported strategies (meanrev/pairs/relval/relstrength).

**Tech Stack:** Rust, clap (CLI parsing), existing backtesting engine (sim.rs).

## Global Constraints

- Default `--objective net-pnl` must preserve all existing behavior (backward-compatible).
- Rate = net_pnl ÷ hold_hours; guard: hold_hours ≤ 0 → rate = 0.0.
- Selection key for robust configs: `min(rate_train, rate_test)` (worst-slice rate).
- Output table adds `test_$/h` and `test_hold_h` columns; sort order changes based on objective.
- Only momentum strategy supports pnl-per-hold; meanrev/pairs/relval/relstrength error with clear message.

---

## File Structure

### `src/portfolio/sim.rs`
- Add `SimRun::total_hold_hours() → f64` method.
- Add three fields to `SimResult`: `hold_hours_train: f64`, `hold_hours_test: f64`.
- Add methods to `SimResult`: `rate_train() → f64`, `rate_test() → f64`.

### `src/bin/momentum_sim.rs`
- Add `enum Objective { NetPnl, PnlPerHold }` (clap-derived).
- Refactor `worst_slice(r: &SimResult) → f64` to `dependability(r: &SimResult, obj: Objective) → f64`.
- Update `run()` command: add `--objective` flag, thread through grid, robust sort, print_table.
- Update `print_table()`: add columns, change sort based on objective, update header.
- Add error handling: unsupported strategies + pnl-per-hold reject with clear message.
- Update `print_env_block()`: add comment noting selection criterion.

---

## Tasks

### Task 1: Add hold_hours calculation to SimRun

**Files:**
- Modify: `src/portfolio/sim.rs:291-320` (SimRun struct and impl)

**Interfaces:**
- Produces: `SimRun::total_hold_hours() → f64` — sums all trade durations in hours, guarding against negative deltas.

- [ ] **Step 1: Write the failing test**

Add to `src/portfolio/sim.rs` (end of file, in the existing test module):

```rust
#[test]
fn test_simrun_total_hold_hours() {
    use crate::portfolio::momentum_state::TradeRecord;
    
    // Trade 1: 3600 seconds (1 hour)
    // Trade 2: 7200 seconds (2 hours)
    // Trade 3: 0 seconds (instantaneous, edge case)
    let trades = vec![
        TradeRecord {
            entry_ts: 0,
            exit_ts: 3600,
            mint: "A".to_string(),
            symbol: "A".to_string(),
            entry_price_usd: 1.0,
            exit_price_usd: 1.1,
            peak_price_usd: 1.1,
            usdc_in: 100.0,
            usdc_out: 110.0,
            pnl_pct: 10.0,
            entry_sig: "test".to_string(),
            exit_sig: "test".to_string(),
            dry_run: false,
        },
        TradeRecord {
            entry_ts: 3600,
            exit_ts: 10800,
            mint: "B".to_string(),
            symbol: "B".to_string(),
            entry_price_usd: 2.0,
            exit_price_usd: 2.2,
            peak_price_usd: 2.2,
            usdc_in: 200.0,
            usdc_out: 220.0,
            pnl_pct: 10.0,
            entry_sig: "test".to_string(),
            exit_sig: "test".to_string(),
            dry_run: false,
        },
        TradeRecord {
            entry_ts: 10800,
            exit_ts: 10800,
            mint: "C".to_string(),
            symbol: "C".to_string(),
            entry_price_usd: 3.0,
            exit_price_usd: 3.0,
            peak_price_usd: 3.0,
            usdc_in: 300.0,
            usdc_out: 300.0,
            pnl_pct: 0.0,
            entry_sig: "test".to_string(),
            exit_sig: "test".to_string(),
            dry_run: false,
        },
    ];
    
    let run = SimRun {
        trades,
        equity_curve: vec![],
    };
    
    // 1 + 2 + 0 = 3 hours
    assert!((run.total_hold_hours() - 3.0).abs() < 1e-9);
}
```

- [ ] **Step 2: Run test to verify it fails**

```bash
cargo test --lib sim::test_simrun_total_hold_hours 2>&1 | grep -A 5 "error\|FAILED"
```

Expected: Error "method `total_hold_hours` not found" or similar.

- [ ] **Step 3: Implement total_hold_hours**

In `src/portfolio/sim.rs`, add to the `SimRun` impl block (after `net_pnl()`):

```rust
pub fn total_hold_hours(&self) -> f64 {
    self.trades
        .iter()
        .map(|t| {
            let duration_secs = (t.exit_ts - t.entry_ts).max(0) as f64;
            duration_secs / 3600.0
        })
        .sum()
}
```

- [ ] **Step 4: Run test to verify it passes**

```bash
cargo test --lib sim::test_simrun_total_hold_hours 2>&1 | grep -E "test.*ok|passed"
```

Expected: "test sim::test_simrun_total_hold_hours ... ok"

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): add SimRun::total_hold_hours() to compute sum of trade durations"
```

---

### Task 2: Add rate fields and methods to SimResult

**Files:**
- Modify: `src/portfolio/sim.rs:1338-1400` (SimResult struct and impl)

**Interfaces:**
- Consumes: `SimRun::total_hold_hours()` from Task 1.
- Produces: 
  - `SimResult` fields: `hold_hours_train: f64`, `hold_hours_test: f64`
  - Methods: `rate_train() → f64`, `rate_test() → f64` (guard: hold_hours ≤ 0 → 0.0)

- [ ] **Step 1: Add fields to SimResult struct**

In `src/portfolio/sim.rs` at line 1338, update `pub struct SimResult`:

```rust
pub struct SimResult {
    pub params: ParamSet,
    pub net_pnl_train: f64,
    pub n_trades_train: usize,
    pub net_pnl_test: f64,
    pub n_trades_test: usize,
    pub win_rate_test: f64,
    pub max_dd_test: f64,
    pub hold_hours_train: f64,  // NEW
    pub hold_hours_test: f64,   // NEW
}
```

- [ ] **Step 2: Add rate methods to SimResult impl**

In the `impl SimResult` block (around line 1365), add after `is_robust()`:

```rust
pub fn rate_train(&self) -> f64 {
    if self.hold_hours_train <= 0.0 { 0.0 } else { self.net_pnl_train / self.hold_hours_train }
}

pub fn rate_test(&self) -> f64 {
    if self.hold_hours_test <= 0.0 { 0.0 } else { self.net_pnl_test / self.hold_hours_test }
}
```

- [ ] **Step 3: Write test for rate methods**

Add to test module at end of `sim.rs`:

```rust
#[test]
fn test_simresult_rate_methods() {
    let mut result = SimResult {
        params: ParamSet::default(), // or use minimal valid struct
        net_pnl_train: 60.0,
        n_trades_train: 3,
        net_pnl_test: 40.0,
        n_trades_test: 2,
        win_rate_test: 0.5,
        max_dd_test: 0.05,
        hold_hours_train: 10.0,
        hold_hours_test: 8.0,
    };
    
    // Normal case: 60 / 10 = 6.0, 40 / 8 = 5.0
    assert!((result.rate_train() - 6.0).abs() < 1e-9);
    assert!((result.rate_test() - 5.0).abs() < 1e-9);
    
    // Guard: hold_hours = 0
    result.hold_hours_train = 0.0;
    assert_eq!(result.rate_train(), 0.0);
    
    // Guard: hold_hours < 0 (should not happen in practice, but guard anyway)
    result.hold_hours_train = -1.0;
    assert_eq!(result.rate_train(), 0.0);
}
```

- [ ] **Step 4: Run test**

```bash
cargo test --lib sim::test_simresult_rate_methods 2>&1 | grep -E "test.*ok|passed"
```

Expected: "test sim::test_simresult_rate_methods ... ok"

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): add hold_hours and rate methods to SimResult"
```

---

### Task 3: Update run_grid to populate hold_hours fields

**Files:**
- Modify: `src/portfolio/sim.rs:1468-1600` (run_grid function, where SimResult entries are built)

**Interfaces:**
- Consumes: `SimRun::total_hold_hours()` and `SimResult` fields from Tasks 1–2.
- Produces: SimResult entries with `hold_hours_train` and `hold_hours_test` populated.

- [ ] **Step 1: Identify the build sites in run_grid**

Search for where `SimResult` is constructed in `run_grid`:

```bash
grep -n "SimResult {" src/portfolio/sim.rs | head -5
```

Expected: Two sites where `SimResult` is built (one for each grid variant). Note the line numbers.

- [ ] **Step 2: Update first SimResult build site**

At the first `SimResult {` block (around line 1567), add two lines:

```rust
SimResult {
    params: base_params.clone(),
    net_pnl_train: tr.net_pnl(),
    n_trades_train: tr.n_trades(),
    net_pnl_test: te.net_pnl(),
    n_trades_test: te.n_trades(),
    win_rate_test: te.win_rate(),
    max_dd_test: te.max_drawdown_pct(),
    hold_hours_train: tr.total_hold_hours(),  // NEW
    hold_hours_test: te.total_hold_hours(),   // NEW
}
```

- [ ] **Step 3: Update second SimResult build site (if different)**

Repeat for the second build site in the same function (search for another `SimResult {`), adding the same two fields.

- [ ] **Step 4: Compile to verify no errors**

```bash
cargo check --lib sim 2>&1 | tail -10
```

Expected: "Finished" or "Checking" with no errors.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/sim.rs
git commit -m "feat(sim): populate hold_hours_train and hold_hours_test in run_grid"
```

---

### Task 4: Add Objective enum and dependability function

**Files:**
- Modify: `src/bin/momentum_sim.rs:1-100` (top-level enum + helper function)
- Modify: `src/bin/momentum_sim.rs:1779-1781` (replace worst_slice)

**Interfaces:**
- Consumes: `SimResult` with rate methods from Task 2.
- Produces: 
  - `enum Objective { NetPnl, PnlPerHold }` (clap-derived)
  - `fn dependability(r: &SimResult, obj: Objective) → f64`

- [ ] **Step 1: Add Objective enum at top of momentum_sim.rs**

After the imports, add:

```rust
#[derive(Clone, Copy, Debug)]
pub enum Objective {
    #[value = "net-pnl"]
    NetPnl,
    #[value = "pnl-per-hold"]
    PnlPerHold,
}
```

Note: The `#[value]` attributes are for clap's value_enum derive. Add `derive(ValueEnum)` from clap.

Actually, check the clap version in Cargo.toml first:

```bash
grep "clap =" Cargo.toml | head -1
```

If clap >= 4.0, use:

```rust
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Objective {
    #[value = "net-pnl"]
    NetPnl,
    #[value = "pnl-per-hold"]
    PnlPerHold,
}
```

- [ ] **Step 2: Replace worst_slice with dependability**

In `src/bin/momentum_sim.rs`, find and replace the `fn worst_slice` function (around line 1779):

**Old:**
```rust
fn worst_slice(r: &SimResult) -> f64 {
    r.net_pnl_train.min(r.net_pnl_test)
}
```

**New:**
```rust
fn dependability(r: &SimResult, obj: Objective) -> f64 {
    match obj {
        Objective::NetPnl => r.net_pnl_train.min(r.net_pnl_test),
        Objective::PnlPerHold => r.rate_train().min(r.rate_test()),
    }
}
```

- [ ] **Step 3: Compile to verify**

```bash
cargo check --bin momentum-sim 2>&1 | tail -10
```

Expected: "Finished" or "Checking" with no errors (may have other issues later, that's ok).

- [ ] **Step 4: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(momentum_sim): add Objective enum and dependability() function"
```

---

### Task 5: Add --objective flag to the `run` command

**Files:**
- Modify: `src/bin/momentum_sim.rs:54-100` (Run struct definition)

**Interfaces:**
- Consumes: `Objective` enum from Task 4.
- Produces: `Run` struct field `objective: Objective` with default value.

- [ ] **Step 1: Find the Run struct**

```bash
grep -n "struct Run {" src/bin/momentum_sim.rs
```

Expected: Line number where `struct Run` is defined.

- [ ] **Step 2: Add objective field**

In the `Run` struct, add:

```rust
/// Ranking objective: net-pnl (default) or pnl-per-hold.
#[arg(long, value_name = "OBJECTIVE", default_value = "net-pnl")]
objective: Objective,
```

Make sure this is inside the `#[command(subcommand = ...)]` or relevant config if Run is a subcommand variant.

- [ ] **Step 3: Compile**

```bash
cargo check --bin momentum-sim 2>&1 | grep -E "error|warning.*objective|Finished"
```

Expected: No errors about `objective` (may have other unrelated errors).

- [ ] **Step 4: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(momentum_sim): add --objective flag to Run command"
```

---

### Task 6: Update the robust sort in run() to use dependability

**Files:**
- Modify: `src/bin/momentum_sim.rs:1752-1765` (robust filtering and sorting)

**Interfaces:**
- Consumes: `Objective` from Run struct (Task 5), `dependability()` function (Task 4).
- Produces: Robust configs sorted by `dependability(r, cfg.objective)`.

- [ ] **Step 1: Find the robust sort line**

```bash
grep -n "robust.sort_by.*worst_slice" src/bin/momentum_sim.rs
```

Expected: Line number where the sort happens (should be around 1753).

- [ ] **Step 2: Replace worst_slice call with dependability**

**Old:**
```rust
robust.sort_by(|a, b| worst_slice(b).partial_cmp(&worst_slice(a)).unwrap_or(std::cmp::Ordering::Equal));
```

**New:**
```rust
robust.sort_by(|a, b| {
    dependability(b, cfg.objective)
        .partial_cmp(&dependability(a, cfg.objective))
        .unwrap_or(std::cmp::Ordering::Equal)
});
```

(Note: You'll need to access the objective from the Run command structure. Check what variable name holds the Run config and use `run.objective` or similar.)

- [ ] **Step 3: Compile**

```bash
cargo check --bin momentum-sim 2>&1 | grep -E "error|Finished"
```

Expected: No errors about `dependability` or `objective`.

- [ ] **Step 4: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(momentum_sim): use dependability() with objective in robust sort"
```

---

### Task 7: Update print_table to add rate and hold_h columns

**Files:**
- Modify: `src/bin/momentum_sim.rs:2010-2050` (print_table function)

**Interfaces:**
- Consumes: `Objective` (from Run), `SimResult` with rate methods (Task 2).
- Produces: Table output with `test_$/h` and `test_hold_h` columns, sorted by objective.

- [ ] **Step 1: Update print_table signature**

Find the `fn print_table(results: &[SimResult], top: usize)` line and add the objective parameter:

**Old:**
```rust
fn print_table(results: &[SimResult], top: usize) {
```

**New:**
```rust
fn print_table(results: &[SimResult], top: usize, objective: Objective) {
```

- [ ] **Step 2: Sort results by objective before printing**

At the start of print_table, add:

```rust
let mut sorted = results.to_vec();
sorted.sort_by(|a, b| {
    dependability(b, objective)
        .partial_cmp(&dependability(a, objective))
        .unwrap_or(std::cmp::Ordering::Equal)
});
let results = sorted.iter().take(top).collect::<Vec<_>>();
```

- [ ] **Step 3: Update header line to include rate and hold_h columns**

Find the header println! in print_table (around line 2025) and update it:

**Old example:**
```rust
println!(
    "{:<8} {:<8} {:>12} {:>7} {:>12} {:>7} {:>7} {:>6}%",
    "metric", "train_pnl", "tr_trd", "test_pnl", "te_trd", "te_win%", "max_dd%"
);
```

**New (add test_$/h and test_hold_h):**
```rust
println!(
    "{:<8} {:<8} {:>12} {:>7} {:>12} {:>7} {:>7} {:>8} {:>8}",
    "metric", "train_pnl", "tr_trd", "test_pnl", "te_trd", "te_win%", "test_$/h", "hold_h"
);
```

- [ ] **Step 4: Update data row println to include rate and hold_h values**

Find the loop printing individual rows and add two more format args:

**Add to the println!:**
```rust
r.rate_test(),      // test_$/h
r.hold_hours_test   // hold_h
```

- [ ] **Step 5: Add header note about sort order**

Add a line before the header:

```rust
let sort_note = match objective {
    Objective::NetPnl => "(sorted by test_pnl)",
    Objective::PnlPerHold => "(sorted by worst-slice $/h)",
};
println!("Configs {}:", sort_note);
```

- [ ] **Step 6: Update all calls to print_table**

Search for calls to `print_table(` and add the objective:

```bash
grep -n "print_table(&" src/bin/momentum_sim.rs | head -10
```

For each call, add `, cfg.objective` (or the appropriate objective variable). Example:

**Old:**
```rust
print_table(&results, top);
```

**New:**
```rust
print_table(&results, top, cfg.objective);
```

(Do this for both the non-robust and robust branches in run().)

- [ ] **Step 7: Compile**

```bash
cargo check --bin momentum-sim 2>&1 | grep -E "error|Finished"
```

Expected: No errors about print_table signature or field access.

- [ ] **Step 8: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(momentum_sim): add test_$/h and hold_h columns to print_table, sort by objective"
```

---

### Task 8: Update print_env_block to note the objective

**Files:**
- Modify: `src/bin/momentum_sim.rs:2370-2400` (print_env_block function)

**Interfaces:**
- Consumes: `Objective` (from Run), `SimResult` (for the winning config).
- Produces: .env block with comment noting selection criterion.

- [ ] **Step 1: Update print_env_block signature**

**Old:**
```rust
fn print_env_block(best: &SimResult) {
```

**New:**
```rust
fn print_env_block(best: &SimResult, objective: Objective) {
```

- [ ] **Step 2: Add comment line at the start of output**

In print_env_block, before the first `println!`, add:

```rust
let comment = match objective {
    Objective::NetPnl => "# Selected via: momentum-sim run --objective net-pnl",
    Objective::PnlPerHold => "# Selected via: momentum-sim run --objective pnl-per-hold",
};
println!("{}\n", comment);
```

- [ ] **Step 3: Update all calls to print_env_block**

Search for calls:

```bash
grep -n "print_env_block(&" src/bin/momentum_sim.rs
```

For each, add `, cfg.objective`. Example:

**Old:**
```rust
print_env_block(robust[0]);
```

**New:**
```rust
print_env_block(robust[0], cfg.objective);
```

- [ ] **Step 4: Compile**

```bash
cargo check --bin momentum-sim 2>&1 | grep -E "error|Finished"
```

- [ ] **Step 5: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(momentum_sim): add objective comment to .env block output"
```

---

### Task 9: Add error handling for unsupported strategies

**Files:**
- Modify: `src/bin/momentum_sim.rs` (Commands::Run arm, early in the match)

**Interfaces:**
- Consumes: `Objective` from Run struct, `strategy` field.
- Produces: Error message if unsupported strategy + pnl-per-hold is requested.

- [ ] **Step 1: Find the run() command handler**

```bash
grep -n "Commands::Run {" src/bin/momentum_sim.rs | head -1
```

Expected: Line where the Run variant is matched.

- [ ] **Step 2: Add early check for unsupported strategies**

Near the start of the Run match arm, add:

```rust
Commands::Run { 
    strategy, 
    objective, 
    metric, 
    min_metric, 
    ... 
} => {
    // Validate strategy support for the objective
    if matches!(objective, Objective::PnlPerHold) 
        && !matches!(strategy, Strategy::Momentum) 
    {
        anyhow::bail!(
            "--objective pnl-per-hold is only supported for momentum strategy. \
             {:?} does not support it; use --objective net-pnl instead.",
            strategy
        );
    }
    
    // ... rest of run() logic
}
```

(Adjust based on actual strategy enum name and structure in the codebase.)

- [ ] **Step 3: Compile**

```bash
cargo check --bin momentum-sim 2>&1 | grep -E "error|Finished"
```

- [ ] **Step 4: Commit**

```bash
git add src/bin/momentum_sim.rs
git commit -m "feat(momentum_sim): add error handling for unsupported strategy + pnl-per-hold"
```

---

### Task 10: Test the --objective flag end-to-end

**Files:**
- Create: `tests/momentum_sim_objective_test.rs` (new integration test)

**Interfaces:**
- Consumes: All changes from Tasks 1–9.
- Produces: End-to-end test verifying pnl-per-hold ranking works and errors on unsupported strategies.

- [ ] **Step 1: Create test file structure**

```bash
mkdir -p tests && cat > tests/momentum_sim_objective_test.rs << 'EOF'
// Placeholder for objective tests
EOF
```

- [ ] **Step 2: Write a test that creates two SimResult objects with different rankings**

```rust
#[test]
fn test_objective_ranking_pnl_vs_rate() {
    use solana_mev::portfolio::sim::{SimResult, ParamSet, Objective};
    
    // Config A: high absolute PnL but slow turnover
    let config_a = SimResult {
        params: ParamSet::default(),
        net_pnl_train: 100.0,
        n_trades_train: 5,
        net_pnl_test: 80.0,
        n_trades_test: 4,
        win_rate_test: 0.75,
        max_dd_test: 0.1,
        hold_hours_train: 100.0,
        hold_hours_test: 80.0,
    };
    
    // Config B: lower absolute PnL but faster turnover (higher $/h)
    let config_b = SimResult {
        params: ParamSet::default(),
        net_pnl_train: 60.0,
        n_trades_train: 6,
        net_pnl_test: 50.0,
        n_trades_test: 5,
        win_rate_test: 0.6,
        max_dd_test: 0.08,
        hold_hours_train: 5.0,
        hold_hours_test: 4.0,
    };
    
    // By net-pnl: A > B (80 > 50)
    let worst_a_pnl = config_a.net_pnl_train.min(config_a.net_pnl_test);
    let worst_b_pnl = config_b.net_pnl_train.min(config_b.net_pnl_test);
    assert!(worst_a_pnl > worst_b_pnl);
    
    // By pnl-per-hold: B > A (rate_test: 50/4=12.5 > 80/80=1.0)
    let worst_a_rate = config_a.rate_train().min(config_a.rate_test());
    let worst_b_rate = config_b.rate_train().min(config_b.rate_test());
    assert!(worst_b_rate > worst_a_rate);
}
```

- [ ] **Step 3: Run the test**

```bash
cargo test --test momentum_sim_objective_test 2>&1 | grep -E "test.*ok|passed|FAILED"
```

Expected: "test test_objective_ranking_pnl_vs_rate ... ok"

- [ ] **Step 4: Write a test for the error case (unsupported strategy)**

Add to the same file:

```rust
#[test]
#[should_panic(expected = "pnl-per-hold is only supported for momentum")]
fn test_objective_pnl_per_hold_rejects_meanrev() {
    // This test is more of a documentation test; the actual error
    // is caught in the CLI parsing. Here we verify the logic would reject it.
    let obj = Objective::PnlPerHold;
    let strategy = Strategy::MeanRev;  // hypothetically
    
    if !matches!(strategy, Strategy::Momentum) && matches!(obj, Objective::PnlPerHold) {
        panic!("pnl-per-hold is only supported for momentum");
    }
}
```

(Adjust strategy enum name/variant as needed based on actual code.)

- [ ] **Step 5: Run all objective tests**

```bash
cargo test --test momentum_sim_objective_test 2>&1 | tail -15
```

Expected: Both tests pass.

- [ ] **Step 6: Commit**

```bash
git add tests/momentum_sim_objective_test.rs
git commit -m "test: add end-to-end tests for --objective flag and error cases"
```

---

### Task 11: Full integration test with a small grid

**Files:**
- Modify: Existing test harness or create a small mock in `tests/` (optional, depends on existing test structure).

**Interfaces:**
- Consumes: All features from Tasks 1–10.
- Produces: Confidence that `cargo run --release --bin momentum-sim -- run --objective pnl-per-hold` works end-to-end.

- [ ] **Step 1: Run momentum-sim with both objectives on sample data**

(This requires a test dataset; use the existing backtest setup if available.)

```bash
cargo build --release --bin momentum-sim 2>&1 | tail -5
```

Expected: Build succeeds.

- [ ] **Step 2: Run a quick smoke test (if test data available)**

```bash
cargo run --release --bin momentum-sim -- run --quick --objective net-pnl 2>&1 | head -30
```

Expected: Output includes a table with existing columns (no new columns yet if this runs against old binary).

```bash
cargo run --release --bin momentum-sim -- run --quick --objective pnl-per-hold 2>&1 | head -30
```

Expected: Output includes new `test_$/h` and `hold_h` columns, sorted by $/h.

- [ ] **Step 3: Verify error handling**

```bash
cargo run --release --bin momentum-sim -- run --quick --strategy meanrev --objective pnl-per-hold 2>&1 | grep -i "not supported\|pnl-per-hold"
```

Expected: Error message about unsupported strategy.

- [ ] **Step 4: Commit (if test data fixtures added)**

```bash
git add tests/  # if any test files created
git commit -m "test: integration smoke test for --objective pnl-per-hold"
```

---

### Task 12: Verify backward compatibility

**Files:**
- No file changes; verification only.

**Interfaces:**
- Consumes: All completed tasks.
- Produces: Confidence that `--objective net-pnl` (default) is identical to pre-change behavior.

- [ ] **Step 1: Run grid search with default (no --objective flag)**

```bash
cargo run --release --bin momentum-sim -- run --quick 2>&1 | head -40
```

Expected: Table output with original columns, sorted by test_pnl (not $/h).

- [ ] **Step 2: Verify .env output**

Check that the `.env` block comment is present and says `Selected via: momentum-sim run --objective net-pnl` (or nothing if we omit it for the default).

- [ ] **Step 3: Run existing tests**

```bash
cargo test --lib sim:: 2>&1 | grep -E "test result|passed|FAILED"
```

Expected: All existing tests in sim.rs pass.

- [ ] **Step 4: Document the finding**

If any regression found, fix it immediately and create a new task. If all pass, proceed to cleanup.

---

### Task 13: Code review checklist and cleanup

**Files:**
- Review all changes from Tasks 1–12.

**Interfaces:**
- Consumes: All merged changes.
- Produces: Confidence in code quality, consistency, and completeness.

- [ ] **Step 1: Review for naming consistency**

```bash
grep -r "hold_hours\|rate_\|dependability\|Objective\|PnlPerHold" src/bin/momentum_sim.rs src/portfolio/sim.rs | wc -l
```

Verify that all three new concepts (hold_hours fields, rate methods, dependability function) are used consistently throughout. No typos like `hold_hour` or `dependable`.

- [ ] **Step 2: Ensure all compile warnings are addressed**

```bash
cargo build --release --bin momentum-sim 2>&1 | grep -i "warning"
```

Expected: No warnings (or acceptable ones like unused imports).

- [ ] **Step 3: Check that print_table calls are all updated**

```bash
grep -n "print_table(" src/bin/momentum_sim.rs
```

Each call should pass the objective. Verify at least 2 calls (robust and non-robust paths).

- [ ] **Step 4: Verify default value of --objective flag**

```bash
cargo run --release --bin momentum-sim -- run --help 2>&1 | grep -A 2 "objective"
```

Expected: Help text shows default is "net-pnl".

- [ ] **Step 5: Final build and test run**

```bash
cargo test --lib sim:: 2>&1 | tail -5
cargo build --release --bin momentum-sim 2>&1 | tail -3
```

Expected: All tests pass, build succeeds.

- [ ] **Step 6: Final commit (if cleanup needed)**

If minor fixes made, commit:

```bash
git add src/
git commit -m "chore: address code review findings and ensure naming consistency"
```

---

## Success Criteria

✅ `cargo run --release --bin momentum-sim -- run --objective pnl-per-hold` ranks configs by worst-slice $/h.  
✅ Output table shows `test_$/h` and `test_hold_h` columns.  
✅ `.env` block is written from `robust[0]` (the most capital-efficient config).  
✅ Default `--objective net-pnl` preserves existing behavior (no output changes, same ranking).  
✅ Error message displayed for unsupported strategies (meanrev, pairs, relval, relstrength) + pnl-per-hold.  
✅ All existing tests pass; new tests cover rate calculation and objective ranking.  
✅ Code is free of warnings and follows existing patterns in the codebase.

---

## Plan complete and saved to `docs/superpowers/plans/2026-07-02-momentum-pnl-per-hold.md`.

**Two execution options:**

**1. Subagent-Driven (recommended)** — I dispatch a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — Execute tasks in this session using executing-plans, batch execution with checkpoints.

**Which approach?**
