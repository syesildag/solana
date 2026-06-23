# On-Chain Market-Neutral Pairs Trader Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a live on-chain trader that runs the backtest-validated market-neutral pairs strategy on correlated xStocks — long the statistically-cheap leg on spot, short the rich leg by borrowing it on Kamino — trading the `ln(A/B)` spread z-score.

**Architecture:** A new `pairs_trader` subsystem living beside the existing momentum trader in `src/portfolio/`, driven by the same `watcher.rs` price loop. The trade *signal* is the exact logic already proven in `src/portfolio/sim.rs` (reused, not reinvented). Execution uses Kamino `klend` for the borrowed short leg (hand-rolled Rust Anchor instructions) and `portfolio::jupiter` for the spot DEX legs. The position is **cross-margined inside one Kamino obligation** — USDC + long-leg xStock posted as collateral, short-leg xStock borrowed against it — so Kamino's own health factor nets the hedge and a rich-leg rally can't easily liquidate it.

**Tech Stack:** Rust, Anchor (CPI/instruction building, same pattern as the existing MarginFi flash-loan integration), Solana SDK, Jupiter swap-api, Kamino `klend` program, reqwest, serde, tokio.

## Global Constraints

- Build target: `cargo build --release --bin solana-mev`; tests live in `#[cfg(test)]` blocks at the bottom of each source file, run with `cargo test --lib`.
- ~~**BUILD, not buy** the Kamino integration: hand-roll `klend` instructions in Rust (no TypeScript sidecar)~~ — **SUPERSEDED 2026-06-23 → BUY** (see the "Phase 2b — status & resume guide" section below). The hand-roll bug surface (~15–20 version-drifting accounts per ix + mandatory refresh ordering) outweighed the one-process benefit on this once-per-trade, non-latency-critical path; the `klend-builder` sidecar uses the maintained `@kamino-finance/klend-sdk`. The bot still signs + submits.
- Every on-chain action MUST be gated behind a dedicated paper-mode flag `DRY_RUN_PAIRS_TRADER` (default `true`), exactly like `DRY_RUN_MOMENTUM_TRADER`. No real borrow/swap fires while it is true.
- Master switch `ENABLE_PAIRS_TRADER` (default `false`) — when off, the subsystem is inert.
- Reuse, do not duplicate: the z-spread math (`sim::zscore_last`, `sim::relval_series`), `portfolio::jupiter` swaps, `portfolio::pricer` prices, and the persistence/audit/halt patterns from `momentum_state.rs` / `momentum_actions.rs`.
- Single open position at a time to start (one `PairPosition` or none — the type enforces it).
- Lamport/USDC math: reuse `momentum::est_gas_usdc`, `jupiter::to_raw_amount` / `from_raw_amount`.
- Commit after every green test.

---

## File Structure

- **Create `src/portfolio/pairs_config.rs`** — `PairsConfig` + `PairSpec`, loaded from env and `pairs.json`. Self-contained so it doesn't bloat `PortfolioConfig`.
- **Create `src/portfolio/pairs_state.rs`** — `PairPosition`, `PairTradeRecord`, `PairsTraderState`, atomic save/load + halt file. Mirrors `momentum_state.rs`.
- **Create `src/portfolio/pairs_signal.rs`** — pure decision + risk functions: `pair_decision`, `estimate_health_factor`, the borrow-APY / health / cap gates, leg sizing. 100% unit-testable, no I/O.
- **Create `src/portfolio/kamino.rs`** — `klend` client: `build_deposit_ix`, `build_borrow_ix`, `build_repay_ix`, `build_withdraw_ix`, and `read_obligation_health`. The only module touching the Kamino program.
- **Create `src/portfolio/pairs_trader.rs`** — the live engine: ties signal + state + Kamino + Jupiter together; `tick()` entry point + open/close orchestration.
- **Modify `src/portfolio/mod.rs`** — add `pub mod pairs_config; pub mod pairs_state; pub mod pairs_signal; pub mod kamino; pub mod pairs_trader;`.
- **Modify `src/portfolio/watcher.rs`** — call `pairs_trader::tick(...)` from the existing 60s loop (next to the momentum hooks).
- **Create `assets/pairs.json`** — the pair list (start with the robust set, NVDAx-centric).
- **Modify `.env.example`** — document the new env vars.

---

# Phase 2a — Paper mode (no capital, reuses validated logic)

Goal: the live engine computes the real signal on live prices, decides opens/closes, simulates fills + a Kamino health model, and writes an audit trail — all in dry-run. Confirms live-vs-backtest parity and a sane health model before any Kamino code. Everything here is pure or paper; zero on-chain risk.

### Task 2a.1: PairSpec + PairsConfig loading

**Files:**
- Create: `src/portfolio/pairs_config.rs`
- Modify: `src/portfolio/mod.rs` (add `pub mod pairs_config;`)
- Create: `assets/pairs.json`

**Interfaces:**
- Produces: `PairSpec { symbol_a: String, mint_a: String, symbol_b: String, mint_b: String }`; `PairsConfig` with fields used throughout: `enable: bool`, `dry_run: bool`, `pairs: Vec<PairSpec>`, `lookback_obs: usize`, `z_entry: f64`, `z_exit: f64`, `z_stop: f64`, `trade_usdc: f64`, `reentry_cooldown_secs: i64`, `max_trades_per_day: u32`, `max_borrow_apy_pct: f64`, `min_health_factor: f64`, `slippage_bps: u32`, `state_path: String`, `halt_path: String`, `actions_path: String`. `PairsConfig::from_env() -> anyhow::Result<PairsConfig>`; `load_pairs(path: &Path) -> anyhow::Result<Vec<PairSpec>>`.

- [ ] **Step 1: Write the failing test** (append to `src/portfolio/pairs_config.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(json: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("pairs_{}.json", rand::random::<u32>()));
        std::fs::write(&p, json).unwrap();
        p
    }
    #[test]
    fn loads_pairs_and_skips_blanks() {
        let p = tmp(r#"[{"symbol_a":"NVDAx","mint_a":"Xsc9","symbol_b":"SPYx","mint_b":"Xso"}]"#);
        let pairs = load_pairs(&p).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].symbol_a.as_str(), pairs[0].symbol_b.as_str()), ("NVDAx", "SPYx"));
        std::fs::remove_file(&p).ok();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pairs_config 2>&1 | tail -5`
Expected: FAIL (`load_pairs` / `PairSpec` not found).

- [ ] **Step 3: Write minimal implementation** (top of `src/portfolio/pairs_config.rs`)

```rust
use std::path::Path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairSpec {
    pub symbol_a: String,
    pub mint_a: String,
    pub symbol_b: String,
    pub mint_b: String,
}

pub fn load_pairs(path: &Path) -> Result<Vec<PairSpec>> {
    let data = std::fs::read_to_string(path)
        .with_context(|| format!("could not read pairs file {}", path.display()))?;
    serde_json::from_str(&data).context("pairs file must be a JSON array of {symbol_a,mint_a,symbol_b,mint_b}")
}
```

Then add `PairsConfig` + `from_env()` following the exact `parse_env` / `parse_bool_env` / `std::env::var(...).unwrap_or_else(...)` patterns in `src/portfolio/mod.rs:139-262`. Fields and defaults:

```rust
#[derive(Debug, Clone)]
pub struct PairsConfig {
    pub enable: bool,
    pub dry_run: bool,
    pub pairs: Vec<PairSpec>,
    pub lookback_obs: usize,
    pub z_entry: f64,
    pub z_exit: f64,
    pub z_stop: f64,
    pub trade_usdc: f64,
    pub reentry_cooldown_secs: i64,
    pub max_trades_per_day: u32,
    pub max_borrow_apy_pct: f64,
    pub min_health_factor: f64,
    pub slippage_bps: u32,
    pub state_path: String,
    pub halt_path: String,
    pub actions_path: String,
}

impl PairsConfig {
    pub fn from_env() -> Result<Self> {
        let pairs_path = std::env::var("PAIRS_PATH").unwrap_or_else(|_| "assets/pairs.json".to_string());
        let pairs = load_pairs(Path::new(&pairs_path)).unwrap_or_default();
        Ok(Self {
            enable: parse_bool_env("ENABLE_PAIRS_TRADER", false),
            dry_run: parse_bool_env("DRY_RUN_PAIRS_TRADER", true),
            pairs,
            lookback_obs: parse_env("PAIRS_LOOKBACK_OBS", 240_usize)?,
            z_entry: parse_env("PAIRS_Z_ENTRY", 2.0_f64)?,
            z_exit: parse_env("PAIRS_Z_EXIT", 0.5_f64)?,
            z_stop: parse_env("PAIRS_Z_STOP", 4.5_f64)?,
            trade_usdc: parse_env("PAIRS_TRADE_USDC", 50.0_f64)?,
            reentry_cooldown_secs: parse_env("PAIRS_REENTRY_COOLDOWN_SECS", 3600_i64)?,
            max_trades_per_day: parse_env("PAIRS_MAX_TRADES_PER_DAY", 6_u32)?,
            max_borrow_apy_pct: parse_env("PAIRS_MAX_BORROW_APY_PCT", 30.0_f64)?,
            min_health_factor: parse_env("PAIRS_MIN_HEALTH_FACTOR", 1.5_f64)?,
            slippage_bps: parse_env("PAIRS_SLIPPAGE_BPS", 50_u32)?,
            state_path: std::env::var("PAIRS_STATE_PATH").unwrap_or_else(|_| "assets/pairs_state.json".to_string()),
            halt_path: std::env::var("PAIRS_HALT_PATH").unwrap_or_else(|_| "assets/pairs_halt.json".to_string()),
            actions_path: std::env::var("PAIRS_ACTIONS_PATH").unwrap_or_else(|_| "assets/pairs_actions.jsonl".to_string()),
        })
    }
}
```

Make `parse_env` / `parse_bool_env` reachable: change them from private `fn` to `pub(crate) fn` in `src/portfolio/mod.rs:240` and `:250`, and `use super::{parse_bool_env, parse_env};` in `pairs_config.rs`. Add `pub mod pairs_config;` to `mod.rs`. Create `assets/pairs.json` with the robust set:

```json
[
  {"symbol_a":"NVDAx","mint_a":"Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh","symbol_b":"SPYx","mint_b":"XsoCS1TfEyfFhfvj8EtZ528L3CaKBDBRqRapnBbDF2W"},
  {"symbol_a":"GOOGLx","mint_a":"XsCPL9dNWBMvFtTmwcCA5v3xWPSMEBCszbQdiLLq6aN","symbol_b":"NVDAx","mint_b":"Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh"},
  {"symbol_a":"QQQx","mint_a":"Xs8S1uUs1zvS2p7iwtsG3b6fkhpvmwz4GYU3gWAmWHZ","symbol_b":"NVDAx","mint_b":"Xsc9qvGR1efVDFGLrVsmkzv3qi45LTBjeUKSPmx9qEh"}
]
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib pairs_config 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/pairs_config.rs src/portfolio/mod.rs assets/pairs.json
git commit -m "feat(pairs): PairSpec + PairsConfig env loading"
```

---

### Task 2a.2: Pure decision + health + gate functions

**Files:**
- Create: `src/portfolio/pairs_signal.rs`
- Modify: `src/portfolio/mod.rs` (add `pub mod pairs_signal;`)

**Interfaces:**
- Consumes: `sim::zscore_last` (already `pub(crate)`? — if private, make it `pub` in `sim.rs`).
- Produces:
  - `enum PairDecision { Hold, Open { long_mint: String, long_sym: String, short_mint: String, short_sym: String }, Close }`
  - `fn pair_decision(z: f64, holding: bool, spec: &PairSpec, cfg: &PairsConfig) -> PairDecision`
  - `fn estimate_health_factor(collateral_usd: f64, debt_usd: f64, liq_threshold: f64) -> f64`
  - `fn borrow_apy_ok(borrow_apy_pct: f64, cfg: &PairsConfig) -> bool`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::pairs_config::{PairSpec, PairsConfig};

    fn spec() -> PairSpec { PairSpec { symbol_a:"A".into(), mint_a:"MA".into(), symbol_b:"B".into(), mint_b:"MB".into() } }
    fn cfg() -> PairsConfig {
        PairsConfig { enable:true, dry_run:true, pairs:vec![], lookback_obs:240, z_entry:2.0, z_exit:0.5,
            z_stop:4.5, trade_usdc:50.0, reentry_cooldown_secs:0, max_trades_per_day:6, max_borrow_apy_pct:30.0,
            min_health_factor:1.5, slippage_bps:50, state_path:"".into(), halt_path:"".into(), actions_path:"".into() }
    }

    #[test]
    fn opens_long_a_when_a_is_cheap() {
        // z < 0 ⇒ ln(A/B) below mean ⇒ A cheap ⇒ long A / short B.
        match pair_decision(-2.5, false, &spec(), &cfg()) {
            PairDecision::Open { long_mint, short_mint, .. } => {
                assert_eq!((long_mint.as_str(), short_mint.as_str()), ("MA", "MB"));
            }
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn opens_long_b_when_b_is_cheap() {
        match pair_decision(2.5, false, &spec(), &cfg()) {
            PairDecision::Open { long_mint, short_mint, .. } =>
                assert_eq!((long_mint.as_str(), short_mint.as_str()), ("MB", "MA")),
            other => panic!("expected Open, got {other:?}"),
        }
    }

    #[test]
    fn holds_when_spread_not_stretched_or_already_broken() {
        assert!(matches!(pair_decision(1.0, false, &spec(), &cfg()), PairDecision::Hold), "below entry");
        assert!(matches!(pair_decision(5.0, false, &spec(), &cfg()), PairDecision::Hold), "past stop, never open");
    }

    #[test]
    fn closes_on_reversion_or_stop_while_holding() {
        assert!(matches!(pair_decision(0.3, true, &spec(), &cfg()), PairDecision::Close), "reverted");
        assert!(matches!(pair_decision(4.6, true, &spec(), &cfg()), PairDecision::Close), "stopped");
        assert!(matches!(pair_decision(2.0, true, &spec(), &cfg()), PairDecision::Hold), "still on");
    }

    #[test]
    fn health_factor_and_borrow_gate() {
        // collateral 150 × 0.8 liq threshold / debt 50 = 2.4.
        assert!((estimate_health_factor(150.0, 50.0, 0.8) - 2.4).abs() < 1e-9);
        assert_eq!(estimate_health_factor(150.0, 0.0, 0.8), f64::INFINITY);
        assert!(borrow_apy_ok(25.0, &cfg()));
        assert!(!borrow_apy_ok(35.0, &cfg()));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pairs_signal 2>&1 | tail -5`
Expected: FAIL (types/functions not defined).

- [ ] **Step 3: Write minimal implementation**

```rust
use super::pairs_config::{PairSpec, PairsConfig};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairDecision {
    Hold,
    Open { long_mint: String, long_sym: String, short_mint: String, short_sym: String },
    Close,
}

/// Mirrors `sim::replay_pairs` exactly so live behavior matches the backtest:
/// open only when the spread is stretched but not broken; z<0 ⇒ A cheap ⇒ long A.
pub fn pair_decision(z: f64, holding: bool, spec: &PairSpec, cfg: &PairsConfig) -> PairDecision {
    if holding {
        if z.abs() <= cfg.z_exit || z.abs() >= cfg.z_stop { PairDecision::Close } else { PairDecision::Hold }
    } else if z.abs() >= cfg.z_entry && z.abs() < cfg.z_stop {
        if z < 0.0 {
            PairDecision::Open { long_mint: spec.mint_a.clone(), long_sym: spec.symbol_a.clone(),
                                 short_mint: spec.mint_b.clone(), short_sym: spec.symbol_b.clone() }
        } else {
            PairDecision::Open { long_mint: spec.mint_b.clone(), long_sym: spec.symbol_b.clone(),
                                 short_mint: spec.mint_a.clone(), short_sym: spec.symbol_a.clone() }
        }
    } else {
        PairDecision::Hold
    }
}

/// Kamino-style health: collateral×liq_threshold ÷ debt. ≥1 is solvent; below the
/// liquidation line the obligation can be liquidated. ∞ when there is no debt.
pub fn estimate_health_factor(collateral_usd: f64, debt_usd: f64, liq_threshold: f64) -> f64 {
    if debt_usd <= 0.0 { return f64::INFINITY; }
    collateral_usd * liq_threshold / debt_usd
}

pub fn borrow_apy_ok(borrow_apy_pct: f64, cfg: &PairsConfig) -> bool {
    borrow_apy_pct <= cfg.max_borrow_apy_pct
}
```

Add `pub mod pairs_signal;` to `mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib pairs_signal 2>&1 | tail -6`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/pairs_signal.rs src/portfolio/mod.rs
git commit -m "feat(pairs): pure decision, health-factor, and borrow gate"
```

---

### Task 2a.3: Pair position state + persistence

**Files:**
- Create: `src/portfolio/pairs_state.rs`
- Modify: `src/portfolio/mod.rs` (add `pub mod pairs_state;`)

**Interfaces:**
- Produces:
  - `PairPosition { pair_key: String, long_mint, long_sym, long_amount: f64, short_mint, short_sym, short_amount: f64, usdc_collateral: f64, entry_ts: i64, entry_z: f64, entry_long_px: f64, entry_short_px: f64, dry_run: bool }` (the two `entry_*_px` fields store the leg marks at open, so `simulate_pair_pnl` / realized P&L are pure functions of stored-entry vs current prices)
  - `PairTradeRecord { pair_key: String, entry_ts: i64, exit_ts: i64, entry_z: f64, exit_z: f64, pnl_usdc: f64, dry_run: bool }`
  - `PairsTraderState { position: Option<PairPosition>, last_close_ts_per_pair: HashMap<String,i64>, trades: Vec<PairTradeRecord> }`
  - `fn load(path: &Path) -> Result<PairsTraderState>`; `fn save(path: &Path, s: &PairsTraderState) -> Result<()>`; `fn trades_last_24h(s, now) -> usize` (counts closed `entry_ts` ≥ now−86400 plus the open position if recent)

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp() -> std::path::PathBuf { std::env::temp_dir().join(format!("ps_{}.json", rand::random::<u32>())) }
    fn pos() -> PairPosition {
        PairPosition { pair_key:"NVDAx/SPYx".into(), long_mint:"MA".into(), long_sym:"NVDAx".into(),
            long_amount:1.0, short_mint:"MB".into(), short_sym:"SPYx".into(), short_amount:0.2,
            usdc_collateral:50.0, entry_ts:1_700_000_000, entry_z:-2.4,
            entry_long_px:50.0, entry_short_px:250.0, dry_run:true }
    }
    #[test]
    fn save_load_round_trip() {
        let p = tmp();
        let mut s = PairsTraderState::default();
        s.position = Some(pos());
        s.last_close_ts_per_pair.insert("X/Y".into(), 42);
        save(&p, &s).unwrap();
        let got = load(&p).unwrap();
        assert_eq!(got.position.as_ref().unwrap().pair_key, "NVDAx/SPYx");
        assert_eq!(got.last_close_ts_per_pair.get("X/Y"), Some(&42));
        std::fs::remove_file(&p).ok();
    }
    #[test]
    fn missing_file_is_flat() {
        assert!(load(&tmp()).unwrap().position.is_none());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pairs_state 2>&1 | tail -5`
Expected: FAIL (not defined).

- [ ] **Step 3: Write minimal implementation**

Mirror `src/portfolio/momentum_state.rs:167-187` (atomic temp+rename save, lenient load). Define the three structs above with `#[derive(Debug,Clone,Serialize,Deserialize)]` (and `Default` on `PairsTraderState`), then:

```rust
use std::collections::HashMap;
use std::path::Path;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ... structs here (fields per Interfaces) ...

pub fn load(path: &Path) -> Result<PairsTraderState> {
    if !path.exists() { return Ok(PairsTraderState::default()); }
    let data = std::fs::read_to_string(path).context("read pairs state")?;
    if data.trim().is_empty() { return Ok(PairsTraderState::default()); }
    serde_json::from_str(&data).context("parse pairs state")
}

pub fn save(path: &Path, s: &PairsTraderState) -> Result<()> {
    if let Some(p) = path.parent() { std::fs::create_dir_all(p).ok(); }
    let json = serde_json::to_string_pretty(s)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn trades_last_24h(s: &PairsTraderState, now: i64) -> usize {
    let cutoff = now - 86_400;
    let closed = s.trades.iter().filter(|t| t.entry_ts >= cutoff).count();
    let open = matches!(&s.position, Some(p) if p.entry_ts >= cutoff) as usize;
    closed + open
}
```

Add `pub mod pairs_state;` to `mod.rs`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib pairs_state 2>&1 | tail -5`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/pairs_state.rs src/portfolio/mod.rs
git commit -m "feat(pairs): position state + atomic persistence"
```

---

### Task 2a.4: Paper engine — live spread, decision, simulated fills, audit

**Files:**
- Create: `src/portfolio/pairs_trader.rs`
- Modify: `src/portfolio/mod.rs` (add `pub mod pairs_trader;`)

**Interfaces:**
- Consumes: `PairsConfig`, `PairSpec`, `pairs_signal::{pair_decision, estimate_health_factor, borrow_apy_ok, PairDecision}`, `pairs_state::*`, `sim::zscore_last`, `history::PriceSnapshot`, `momentum::est_gas_usdc`.
- Produces:
  - `fn live_spread_z(history: &VecDeque<PriceSnapshot>, spec: &PairSpec, lookback: usize) -> Option<f64>` (pure)
  - `fn simulate_pair_pnl(pos: &PairPosition, long_px: f64, short_px: f64, slippage_bps: u32, sol_px: f64) -> f64` (pure — paper fill)
  - `async fn tick(cfg: &PairsConfig, history: &VecDeque<PriceSnapshot>, prices: &HashMap<String,f64>) -> Result<()>` (the engine; in 2a only the dry-run branch is implemented)

- [ ] **Step 1: Write the failing tests** (pure functions only — `tick` is integration-tested manually in Step 6)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::portfolio::history::PriceSnapshot;
    use std::collections::{HashMap, VecDeque};
    use crate::portfolio::pairs_config::PairSpec;

    fn snap(ts: u64, a: f64, b: f64) -> PriceSnapshot {
        let mut p = HashMap::new();
        p.insert("MA".to_string(), a);
        p.insert("MB".to_string(), b);
        PriceSnapshot { ts, prices: p }
    }
    fn spec() -> PairSpec { PairSpec{symbol_a:"A".into(),mint_a:"MA".into(),symbol_b:"B".into(),mint_b:"MB".into()} }

    #[test]
    fn live_spread_z_matches_window() {
        // 40 noisy points then a dislocation: z should be strongly signed.
        let mut h: VecDeque<PriceSnapshot> = VecDeque::new();
        for i in 0..40u64 { h.push_back(snap(i, if i%2==0 {99.0} else {101.0}, 100.0)); }
        h.push_back(snap(40, 110.0, 100.0)); // A spikes up vs B → ln(A/B) high → z >> 0
        let z = live_spread_z(&h, &spec(), 45).expect("z computable");
        assert!(z > 2.0, "stretched spread → high z, got {z}");
    }

    #[test]
    fn simulate_pnl_profits_on_convergence() {
        // Long A short B; A rose, B flat → net positive before/after small costs.
        // Opened with both legs at 100; long A then rises to 110, short B flat at 100.
        let pos = PairPosition { pair_key:"A/B".into(), long_mint:"MA".into(), long_sym:"A".into(),
            long_amount: 1.0, short_mint:"MB".into(), short_sym:"B".into(), short_amount: 1.0,
            usdc_collateral: 50.0, entry_ts: 0, entry_z: -2.5,
            entry_long_px: 100.0, entry_short_px: 100.0, dry_run: true };
        // long leg +~9.45 (110×0.995−100), short leg −0.5 (100−100×1.005) → net positive.
        let pnl = simulate_pair_pnl(&pos, 110.0, 100.0, 50, 150.0);
        assert!(pnl > 0.0, "convergence in our favor → profit, got {pnl}");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib pairs_trader 2>&1 | tail -5`
Expected: FAIL (not defined).

- [ ] **Step 3: Write minimal implementation**

`live_spread_z` reuses the same windowing as `sim::ranked`/`relval_series`: build the aligned `ln(a/b)` series from `history` for this pair, take the last `lookback`, call `sim::zscore_last`.

```rust
use std::collections::{HashMap, VecDeque};
use anyhow::Result;
use tracing::info;
use super::history::PriceSnapshot;
use super::pairs_config::{PairSpec, PairsConfig};
use super::pairs_signal::{pair_decision, PairDecision};
use super::{pairs_state, sim};
use super::momentum::est_gas_usdc;

pub fn live_spread_z(history: &VecDeque<PriceSnapshot>, spec: &PairSpec, lookback: usize) -> Option<f64> {
    let spreads: Vec<f64> = history.iter().filter_map(|s| {
        let a = s.prices.get(&spec.mint_a).copied().filter(|p| *p > 0.0)?;
        let b = s.prices.get(&spec.mint_b).copied().filter(|p| *p > 0.0)?;
        let z = (a / b).ln();
        z.is_finite().then_some(z)
    }).collect();
    if spreads.is_empty() { return None; }
    let lo = spreads.len().saturating_sub(lookback);
    sim::zscore_last(&spreads[lo..])
}

/// Paper P&L for a dollar-neutral pair: sell the long leg, buy back the short leg,
/// both at current prices net of slippage, minus two gas legs. Pure function of the
/// stored entry marks (`entry_long_px`/`entry_short_px`) and current prices.
pub fn simulate_pair_pnl(pos: &PairPosition, long_px: f64, short_px: f64, slippage_bps: u32, sol_px: f64) -> f64 {
    let slip = slippage_bps as f64 / 10_000.0;
    let long_pl = pos.long_amount * (long_px * (1.0 - slip) - pos.entry_long_px);          // long leg
    let short_pl = pos.short_amount * (pos.entry_short_px - short_px * (1.0 + slip));      // short leg
    long_pl + short_pl - 2.0 * est_gas_usdc(sol_px)
}
```

`use super::pairs_state::PairPosition;`. Then `tick`:

```rust
pub async fn tick(cfg: &PairsConfig, history: &VecDeque<PriceSnapshot>, prices: &HashMap<String, f64>) -> Result<()> {
    if !cfg.enable { return Ok(()); }
    let state_path = std::path::Path::new(&cfg.state_path);
    let mut state = pairs_state::load(state_path)?;
    let now = chrono::Utc::now().timestamp();

    // HOLDING: evaluate close.
    if let Some(pos) = state.position.clone() {
        if let Some(z) = live_spread_z(history, &spec_for(cfg, &pos.pair_key), cfg.lookback_obs) {
            if matches!(pair_decision(z, true, &spec_for(cfg, &pos.pair_key), cfg), PairDecision::Close) {
                let lpx = prices.get(&pos.long_mint).copied().unwrap_or(0.0);
                let spx = prices.get(&pos.short_mint).copied().unwrap_or(0.0);
                let sol = prices.get("SOL").copied().unwrap_or(0.0);
                let pnl = simulate_pair_pnl(&pos, lpx, spx, cfg.slippage_bps, sol);
                info!("pairs(paper): CLOSE {} z={z:.2} simulated pnl={pnl:+.4} USDC", pos.pair_key);
                state.trades.push(pairs_state::PairTradeRecord { pair_key: pos.pair_key.clone(),
                    entry_ts: pos.entry_ts, exit_ts: now, entry_z: pos.entry_z, exit_z: z, pnl_usdc: pnl, dry_run: true });
                state.last_close_ts_per_pair.insert(pos.pair_key.clone(), now);
                state.position = None;
                pairs_state::save(state_path, &state)?;
            }
        }
        return Ok(());
    }

    // FLAT: scan pairs, open the first whose signal fires + gates pass (paper).
    if pairs_state::trades_last_24h(&state, now) >= cfg.max_trades_per_day as usize { return Ok(()); }
    for spec in &cfg.pairs {
        let Some(z) = live_spread_z(history, spec, cfg.lookback_obs) else { continue };
        if let PairDecision::Open { long_mint, long_sym, short_mint, short_sym } = pair_decision(z, false, spec, cfg) {
            let key = format!("{}/{}", spec.symbol_a, spec.symbol_b);
            if state.last_close_ts_per_pair.get(&key).is_some_and(|&t| now - t < cfg.reentry_cooldown_secs) { continue; }
            let lpx = prices.get(&long_mint).copied().unwrap_or(0.0);
            let spx = prices.get(&short_mint).copied().unwrap_or(0.0);
            if lpx <= 0.0 || spx <= 0.0 { continue; }
            // Dollar-neutral: equal USDC per leg. (Real borrow/swaps land in Phase 2c/2d.)
            let pos = PairPosition { pair_key: key.clone(), long_mint, long_sym, long_amount: cfg.trade_usdc / lpx,
                short_mint, short_sym, short_amount: cfg.trade_usdc / spx, usdc_collateral: cfg.trade_usdc,
                entry_ts: now, entry_z: z, entry_long_px: lpx, entry_short_px: spx, dry_run: true };
            info!("pairs(paper): OPEN {key} z={z:.2} long {} short {}", pos.long_sym, pos.short_sym);
            state.position = Some(pos);
            pairs_state::save(state_path, &state)?;
            break;
        }
    }
    Ok(())
}

fn spec_for(cfg: &PairsConfig, key: &str) -> PairSpec {
    cfg.pairs.iter().find(|s| format!("{}/{}", s.symbol_a, s.symbol_b) == key).cloned()
        .unwrap_or_else(|| PairSpec { symbol_a: "?".into(), mint_a: "?".into(), symbol_b: "?".into(), mint_b: "?".into() })
}
```

Add `pub mod pairs_trader;` to `mod.rs`. Add `chrono` timestamp use (already a dependency).

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib pairs_trader 2>&1 | tail -6`
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/pairs_trader.rs src/portfolio/pairs_state.rs src/portfolio/mod.rs
git commit -m "feat(pairs): paper engine — live z, decision, simulated fills"
```

---

### Task 2a.5: Wire paper engine into the watcher loop + audit trail

**Files:**
- Modify: `src/portfolio/watcher.rs` (call `pairs_trader::tick` from the 60s loop, beside the momentum hooks)
- Modify: `.env.example` (document `ENABLE_PAIRS_TRADER`, `DRY_RUN_PAIRS_TRADER`, `PAIRS_*`)
- Create: audit via `momentum_actions`-style JSONL — reuse `momentum_actions::append` with a generic record, or add a tiny `pairs_actions` writer.

**Interfaces:**
- Consumes: `pairs_trader::tick`, the watcher's existing `history: VecDeque<PriceSnapshot>` and `prices: HashMap<String,f64>`.

- [ ] **Step 1: Add the call** in `watcher.rs` where momentum is ticked (search for `momentum::maybe_enter` / the 60s branch). After the price snapshot is appended to `history`:

```rust
if let Ok(pcfg) = crate::portfolio::pairs_config::PairsConfig::from_env() {
    if let Err(e) = crate::portfolio::pairs_trader::tick(&pcfg, &history, &prices).await {
        tracing::warn!("pairs tick failed: {e}");
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo build --release --bin solana-mev 2>&1 | grep -E "error|Finished"`
Expected: `Finished`.

- [ ] **Step 3: Document env vars** — append to `.env.example`:

```
# ── Pairs trader (market-neutral xStocks; paper by default) ──
ENABLE_PAIRS_TRADER=false
DRY_RUN_PAIRS_TRADER=true
PAIRS_PATH=assets/pairs.json
PAIRS_LOOKBACK_OBS=240
PAIRS_Z_ENTRY=2.0
PAIRS_Z_EXIT=0.5
PAIRS_Z_STOP=4.5
PAIRS_TRADE_USDC=50
PAIRS_MAX_BORROW_APY_PCT=30
PAIRS_MIN_HEALTH_FACTOR=1.5
```

- [ ] **Step 4: Manual paper run** (verification, not a unit test)

Run: `ENABLE_PAIRS_TRADER=true DRY_RUN_PAIRS_TRADER=true cargo run --release --bin solana-mev` (or the portfolio-watcher binary if that's where the watcher runs).
Expected: log lines `pairs(paper): OPEN ...` / `CLOSE ... simulated pnl=...` appear once enough history accumulates; `assets/pairs_state.json` is written. Cross-check the simulated opens/closes against `momentum-sim run --strategy pairs` over the same window — they should agree directionally.

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/watcher.rs .env.example
git commit -m "feat(pairs): wire paper engine into watcher loop + env docs"
```

**Phase 2a gate:** paper engine runs live, decisions match the backtest, no on-chain calls. STOP and review before Kamino code.

---

# Phase 2b — Kamino `klend` plumbing (~~BUILD: hand-rolled Rust instructions~~ → BUY: sidecar)

> **SUPERSEDED 2026-06-23 → BUY.** The implementation took the sidecar route — see the
> "Phase 2b — status & resume guide" section at the end of this doc for the current,
> authoritative state (what's built in `klend-builder/` + `src/portfolio/kamino.rs`, what's
> verified offline, and what still needs the live wallet). The task bodies below are the
> original BUILD breakdown; their *goals* (deposit/borrow/repay/withdraw, health read,
> cross-margin proof) still hold, but the account-wiring tasks are now the SDK's job. The
> 2b.3 cross-margin proof (Task 2b.3) is unchanged and still the gate.

Goal: borrow/repay/deposit/withdraw against a Kamino obligation, and read its health, from Rust. Proven on devnet / tiny mainnet before any strategy uses it.

> **Implementer prerequisite (research task, not a placeholder for logic):** the exact `klend` account orderings, PDAs, and instruction discriminators MUST be derived from the live program IDL and the Kamino docs at implementation time — they are versioned and cannot be safely hardcoded from this plan. Fetch the IDL (`anchor idl fetch <klend program id>` or Kamino's published IDL), and mirror the account layout the existing MarginFi integration in `src/main.rs` uses for its flash-loan CPI as the structural template. Each task below states the deliverable and its test; the account wiring is filled from the IDL.

### Task 2b.1: Kamino market/reserve discovery + obligation address

**Files:** Create `src/portfolio/kamino.rs`; modify `mod.rs` (`pub mod kamino;`).

**Interfaces:**
- Produces: `struct KaminoCtx { program_id: Pubkey, market: Pubkey, reserves: HashMap<String, ReserveInfo> }`; `ReserveInfo { reserve: Pubkey, liquidity_mint: Pubkey, liq_threshold: f64, borrow_apy_pct: f64, available_liquidity: f64 }`; `fn obligation_pda(owner: &Pubkey, market: &Pubkey, program_id: &Pubkey) -> Pubkey`; `async fn load_market(rpc: &RpcClient, ctx_cfg) -> Result<KaminoCtx>`.

- [ ] **Step 1:** Write a unit test for `obligation_pda` determinism (same inputs → same address; PDA derivation is pure once the seeds are known from the IDL).
- [ ] **Step 2:** Run it; verify FAIL.
- [ ] **Step 3:** Implement PDA derivation (seeds per IDL) and `load_market` (RPC reads of the xStocks market + reserve accounts; parse `liq_threshold`, `borrow_apy`, `available_liquidity`). Borrow APY/threshold parsing mirrors how the bot already decodes on-chain account state in `src/dex/`.
- [ ] **Step 4:** Run test; verify PASS. Add an `--inspect-kamino` style manual check that prints discovered reserves + live borrow APY (this is the number the go/no-go needs — wire it so `borrow_apy_pct` flows into `pairs_signal::borrow_apy_ok`).
- [ ] **Step 5:** Commit `feat(pairs/kamino): market + reserve discovery, obligation PDA`.

### Task 2b.2: Instruction builders — deposit / withdraw / borrow / repay

**Files:** `src/portfolio/kamino.rs`.

**Interfaces:**
- Produces: `fn build_deposit_ix(ctx, owner, reserve, amount) -> Instruction`; `fn build_borrow_ix(...) -> Instruction`; `fn build_repay_ix(...) -> Instruction`; `fn build_withdraw_ix(...) -> Instruction`. Each returns a `solana_sdk::instruction::Instruction` with accounts ordered per IDL.

- [ ] **Step 1:** Write tests asserting each builder produces an `Instruction` with the correct `program_id` and the expected number/ordering of accounts (derive expected ordering from the IDL; assert against it).
- [ ] **Step 2:** Run; verify FAIL.
- [ ] **Step 3:** Implement the four builders (discriminators + account metas from IDL; use `anchor_lang`/borsh arg encoding consistent with the MarginFi path in `main.rs`).
- [ ] **Step 4:** Run; verify PASS.
- [ ] **Step 5:** Commit `feat(pairs/kamino): deposit/withdraw/borrow/repay instruction builders`.

### Task 2b.3: Health read + cross-margin behavior proof (devnet/tiny mainnet)

**Files:** `src/portfolio/kamino.rs`.

**Interfaces:**
- Produces: `async fn read_obligation_health(rpc, obligation) -> Result<f64>` (returns the live health factor parsed from the obligation account).

- [ ] **Step 1:** Write a test for the health *parsing* given a captured obligation account fixture (collateral/debt → expected health, cross-checked with `pairs_signal::estimate_health_factor`).
- [ ] **Step 2:** Run; verify FAIL.
- [ ] **Step 3:** Implement parsing.
- [ ] **Step 4:** Run; verify PASS. **Then the cross-margin proof (manual, tiny real funds):** deposit USDC + a long-leg xStock as collateral, borrow the short-leg xStock, and confirm via `read_obligation_health` that a simulated rich-leg price rise (use a small real position + observe, or RPC `simulateTransaction`) leaves health well above the liquidation line because the deposited long leg appreciates too. Document the observed health behavior.
- [ ] **Step 5:** Commit `feat(pairs/kamino): obligation health read + cross-margin proof`.

**Phase 2b gate:** can borrow/repay an xStock and read health from Rust; cross-margin hedge behaves as designed. STOP and review.

---

# Phase 2c — DEX legs + open/close orchestration

Goal: compose the multi-step open and close as robust sequences with slippage caps and rollback. Still dry-run-gated; real submission only flips on in 2d.

### Task 2c.1: Leg sizing + slippage-capped swap wrappers

**Files:** `src/portfolio/pairs_trader.rs`.

**Interfaces:**
- Consumes: `portfolio::jupiter::{quote, to_raw_amount, from_raw_amount, price_impact_bps}` (already used by momentum).
- Produces: `async fn swap_leg(http, cfg, from_mint, to_mint, amount_raw, max_slippage_bps) -> Result<SwapResult>` where `SwapResult { out_amount: f64, impact_bps: u32 }`; aborts (Err) if `impact_bps > max_slippage_bps`.

- [ ] **Step 1:** Unit-test sizing math (`trade_usdc` → leg token amounts via `to_raw_amount`/decimals) as a pure function `fn leg_size(trade_usdc, px, decimals) -> u64`.
- [ ] **Step 2:** Run; FAIL. **Step 3:** Implement `leg_size` + `swap_leg` (wraps `jupiter::quote`, enforces the slippage cap). **Step 4:** Run; PASS. **Step 5:** Commit `feat(pairs): leg sizing + slippage-capped swap wrapper`.

### Task 2c.2: Open sequence (long first, then borrow+short) with rollback

**Files:** `src/portfolio/pairs_trader.rs`.

**Interfaces:**
- Produces: `async fn open_pair(ctx, http, rpc, signer, cfg, decision, prices) -> Result<PairPosition>`. Order: (1) buy long leg on DEX, (2) deposit USDC + long leg to Kamino, (3) borrow short leg, (4) sell short leg → USDC. On failure after step 1, **roll back** by selling the long leg back to USDC (and repaying/withdrawing if steps 2–3 partially executed). Pre-check `estimate_health_factor` ≥ `min_health_factor` and `borrow_apy_ok` BEFORE step 3.

- [ ] **Step 1:** Unit-test the *rollback decision* as a pure function `fn rollback_plan(progress: OpenProgress) -> Vec<RollbackAction>` (given how far the sequence got, what must be undone). **Step 2:** FAIL. **Step 3:** Implement the pure planner + the async `open_pair` that executes it (dry-run logs each leg; real submit gated by `cfg.dry_run`). **Step 4:** PASS. **Step 5:** Commit `feat(pairs): open sequence with rollback planner`.

### Task 2c.3: Close sequence (sell long, buy+repay short, withdraw)

**Files:** `src/portfolio/pairs_trader.rs`.

**Interfaces:**
- Produces: `async fn close_pair(ctx, http, rpc, signer, cfg, pos, prices) -> Result<f64>` (returns realized USDC P&L). Order: (1) buy back short leg → repay Kamino borrow, (2) withdraw collateral (USDC + long leg), (3) sell long leg → USDC. Unconditional (no cost gate on exit — must always be able to close), but slippage self-escalates like `momentum::escalated_slippage_bps`.

- [ ] **Step 1:** Unit-test realized-P&L computation as a pure function over fill amounts (mirror `simulate_pair_pnl`, now fed real fills). **Step 2:** FAIL. **Step 3:** Implement pure P&L + async `close_pair`. **Step 4:** PASS. **Step 5:** Commit `feat(pairs): close sequence + realized P&L`.

**Phase 2c gate:** open/close orchestrate end-to-end in dry-run with correct rollback/P&L logic. STOP and review.

---

# Phase 2d — Live, minimal size

Goal: flip real execution on for ONE pair at tiny notional, full risk layer armed.

### Task 2d.1: Risk layer + circuit breaker

**Files:** `src/portfolio/pairs_trader.rs`, `src/portfolio/pairs_state.rs`.

**Interfaces:**
- Produces: `fn risk_ok(state, cfg, ctx_health, borrow_apy) -> RiskVerdict` (pure): blocks new opens when daily cap hit, health below floor, borrow APY above cap, or halt file present; `fn maybe_halt_on_loss(state, cfg)` writing the halt file (reuse `momentum_state::write_halt` pattern) when cumulative realized P&L breaches a configured floor.

- [ ] **Step 1:** Unit-test every `risk_ok` rejection branch + the loss-halt trigger (pure). **Step 2:** FAIL. **Step 3:** Implement. **Step 4:** PASS. **Step 5:** Commit `feat(pairs): risk gates + loss circuit breaker`.

### Task 2d.2: Live wiring + health monitor

**Files:** `src/portfolio/pairs_trader.rs`.

- [ ] **Step 1:** In `tick`, replace the paper open/close branches with `open_pair`/`close_pair` when `!cfg.dry_run`, guarded by `risk_ok`. Add a HOLDING-side health check each tick: if `read_obligation_health` < `min_health_factor`, force `close_pair` (de-risk) regardless of z. **Step 2:** `cargo build --release` → `Finished`. **Step 3:** Manual canary: `ENABLE_PAIRS_TRADER=true DRY_RUN_PAIRS_TRADER=false PAIRS_TRADE_USDC=5` on a single-pair `pairs.json`, watch one full open→close round-trip on-chain; verify state, audit, and actual wallet/obligation match the logs. **Step 4:** Commit `feat(pairs): live execution + health-driven de-risk`.

### Task 2d.3: Operations runbook

**Files:** Create `docs/pairs-trader-runbook.md`.

- [ ] **Step 1:** Document: how to read live Kamino borrow APY into `PAIRS_MAX_BORROW_APY_PCT`, how to halt (touch `assets/pairs_halt.json`), how to re-validate the edge as history grows (`momentum-sim run --strategy pairs --pair-funding-bps-day <live>`), and the scale-up checklist (raise `PAIRS_TRADE_USDC` only after N clean round-trips, watch xStock pool slippage). **Step 2:** Commit `docs(pairs): operations runbook`.

**Phase 2d gate:** one pair trades live at tiny size with risk layer + breaker; behavior matches paper. Scale only after sustained clean operation.

---

## Verification (end-to-end)

1. **Unit tests:** `cargo test --lib` — all pairs modules green (`pairs_config`, `pairs_signal`, `pairs_state`, `pairs_trader`, `kamino`).
2. **Lint:** `cargo clippy` — no new warnings in the pairs modules.
3. **Paper parity (2a):** live `pairs(paper)` opens/closes agree directionally with `momentum-sim run --strategy pairs` over the same window.
4. **Kamino proof (2b):** borrow + repay + health read succeed on tiny real funds; cross-margin keeps health above the liquidation line under a rich-leg rally.
5. **Orchestration (2c):** dry-run open/close logs show correct leg order, rollback on induced failure, and P&L matching the paper model.
6. **Live canary (2d):** one $5-notional pair completes an on-chain open→close; wallet, Kamino obligation, state file, and audit trail all reconcile.

## Known risks (carry into execution)

- **NVDAx-regime concentration** — the edge is partly NVDA dispersion; re-validate as data grows (Task 2d.3 covers the re-check command).
- **Inter-leg exposure** — between open legs the position is briefly unhedged; the slippage caps + long-first ordering bound it, but it's real.
- **Borrow-rate spikes / liquidity** — `borrow_apy_ok` + `available_liquidity` checks gate this; start tiny.
- **Compliance** — confirm on-chain xStock access from the operator's jurisdiction before 2d.

---

## Phase 2b — status & resume guide (updated 2026-06-23)

**Build-vs-buy — RESOLVED: BUY (reverses the original "BUILD, not buy" constraint on
line 14).** klend's deposit/borrow/repay/withdraw carry ~15–20 accounts each (reserves,
vaults, oracles, two token programs, referrer) plus mandatory `refresh_reserve`/
`refresh_obligation` ordering, and the layouts drift by program version. The borrow path
is **once-per-trade, not latency-critical**, so the maintained `@kamino-finance/klend-sdk`
(which derives all accounts/PDAs/refresh ordering) is far safer than hand-transcribing the
IDL. A thin TS sidecar (`klend-builder/`) builds the instructions; the bot signs + submits.

**Compliance (checklist item 1) — RESOLVED: GO.** xStocks are available to France/EU
holders, and on-chain secondary trading (DEX + Kamino borrow) is permissionless — KYC
gates only the *primary* mint/redeem at the issuer, not on-chain lending. Not a blocker.

**Done — 2b.2** (branch `pairs-phase2b`):
- `klend-builder/` sidecar — `package.json` (klend-sdk 7.3.22, `@solana/kit` v2),
  `tsconfig.json`, `src/index.ts` with `/health`, `/market`, `/obligation`, and
  `/build/{deposit｜borrow｜repay｜withdraw}` (returns grouped instruction JSON via
  `createNoopSigner` so the bot signs), `README.md` with the verify loop.
- `src/portfolio/kamino.rs` — rewritten from hand-rolled stubs to a thin HTTP client:
  `KlendClient` (`market`/`obligation_health`/`build`), `KlendAction`, `ObligationHealth`
  (+ `health_factor()`), and `load_market`/`read_obligation_health` now implemented over
  the sidecar. The old hand-rolled `anchor_discriminator`/`obligation_pda`/`ix_name`
  were **removed** (SDK is now the single source of truth for derivation).
- **Verified offline:** `cargo test --lib kamino::` (6 tests — JSON→Instruction contract,
  base64 decode, account-flag preservation, flatten order, HF math, unit conversions).

**Still UNVERIFIED (needs the operator's wallet + live RPC — this is 2b.3):**
- The sidecar has never been executed. `VERIFY:` markers in `index.ts` (exact
  `KaminoAction.build*Txns` arg order, `reserve.address`/`reserve.stats`/
  `obligation.refreshedStats` accessors, APY units) must be confirmed via
  `npm run typecheck` + live `/market` once installed.
- `KLEND_MARKET` (the xStocks lending-market pubkey) must be found on app.kamino.finance.

**Resume checklist (2b.3 → 2b.4):**
1. `cd klend-builder && npm install && npm run typecheck` — fix any SDK signature drift.
2. Set `RPC_URL` + `KLEND_MARKET`, `npm start`, and walk the README verify loop
   (`/health` → `/market` → `/obligation` → `/build/deposit` tiny amount). Confirm the
   `VERIFY:` items; adjust `index.ts` + the `borrow_apy_pct` unit assumption in
   `kamino.rs::market()` to match reality.
3. Wire `KlendClient` into the pairs trader's risk layer: `borrow_apy_pct` →
   `sim::borrow_apy_ok`, `health_factor()` → `estimate_health_factor` gate.
4. **Cross-margin proof (2b.3)** on tiny mainnet funds: deposit USDC + long-leg xStock,
   borrow short-leg xStock, confirm health stays above liquidation under a rich-leg rise.
5. Then 2c (orchestration) + 2d (live $5 canary) per the tasks above.
