# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
# Build
cargo build --release

# Run (requires .env populated from .env.example)
cargo run --release --bin solana-mev
DRY_RUN=true cargo run --release --bin solana-mev   # no bundle submission

# ALT management (Address Lookup Table — required for versioned transactions)
cargo run --release --bin solana-mev -- --init-alt      # create/extend ALT then start bot
cargo run --release --bin solana-mev -- --inspect-alt   # print ALT contents and exit

# Pool + ATA refresh (run before --init-alt when pools.json changes)
node scripts/fetch_all.js   # fetches all DEX pools, merges pools.json, creates missing user ATAs

# Test — all tests live in #[cfg(test)] blocks at the bottom of each source file
cargo test --bin solana-mev
cargo test --bin solana-mev raydium_clmm   # filter by module/test name
cargo test --bin solana-mev evaluator -- --nocapture

# Lint / fmt
cargo clippy
cargo fmt
```

## First-time setup

```bash
# 1. Copy and fill in .env
cp .env.example .env
# edit .env: GRPC_ENDPOINT, WALLET_KEYPAIR_PATH, RPC_URL, ENABLE_FLASH_LOAN, MARGINFI_*, etc.

# 2. Fetch pool data and create user ATAs
node scripts/fetch_all.js

# 3. Create ALT (writes address to alt.json) and start bot
cargo build --release
cargo run --release --bin solana-mev -- --init-alt

# 4. Persist ALT address for future runs (so --init-alt is not needed every time)
echo "ALT_ADDRESS=$(jq -r .alt_address alt.json)" >> .env

# Subsequent runs
cargo run --release --bin solana-mev
```

**When pools.json changes** (new pools added via `fetch_all.js`):
```bash
node scripts/fetch_all.js                                   # refresh pools + create new ATAs
cargo run --release --bin solana-mev -- --init-alt          # extend ALT with new accounts
```

## Running the Jupiter swap-api

Only needed when `ENABLE_JUPITER=true`. The self-hosted Jupiter Swap API (Jupiter's "Metis"
routing engine, downloaded from the `jup-ag/metis-binary` releases page) must serve `/quote` +
`/swap-instructions` on `JUPITER_API_URL` (default `http://127.0.0.1:8080`). Run it locally so
`/quote` answers in single-digit ms; the public `quote-api.jup.ag` is too slow/rate-limited for
the poller's hot loop.

The Metis binary is **gated**: it requires a `--binary-key` license key from your provider
(Triton/QuickNode/Jupiter). Without it Metis prints usage and exits.

**Auto-launch (recommended):** set `JUPITER_BINARY_PATH` (e.g. `./metis-binary`) **and**
`JUPITER_BINARY_KEY` and the bot spawns Metis itself as a child process — pointed at the same
RPC + gRPC, `kill_on_drop` on exit, stdout/stderr inherited. (Path set but key missing → the bot
warns and skips auto-launch.) Just run the bot:

```bash
DRY_RUN=true cargo run --release --bin solana-mev
# logs: "Launched Metis swap-api ... indexing pools (~1-2 min)" then "Jupiter swap-api ready after Ns"
```

**Manual:** leave `JUPITER_BINARY_PATH` unset and run it yourself, pointed at the same RPC + gRPC:

```bash
RUST_LOG=info ./metis-binary \
  --binary-key "$JUPITER_BINARY_KEY" \
  --rpc-url "$RPC_URL" \
  --yellowstone-grpc-endpoint "$GRPC_ENDPOINT" \
  --yellowstone-grpc-x-token  "$GRPC_TOKEN"
# → serves HTTP on 0.0.0.0:8080 (matches JUPITER_API_URL default)
```

> macOS: the downloaded binary is Gatekeeper-quarantined — clear it once with
> `xattr -d com.apple.quarantine ./metis-binary` (re-run after each re-download).

- **First boot is slow** — the binary indexes the full pool set before `:8080` comes up (1–2 min).
  Until then the poller gets zero rates and Jupiter edges simply don't appear.
- Co-locate it with the bot + RPC for lowest latency. RPC-only (no gRPC) works but updates far
  less often — gRPC is strongly recommended for arbitrage.
- Verify it's live: `jupiter=N` appears in the `BF window` log line once edges populate.
- Jupiter pairs are configured in `jupiter_pairs.json` (separate from `pools.json`): a flat list of
  `{ "token_a", "token_b" }`. See the **Jupiter** entry under DEX-specific notes.

## Architecture overview

The bot has a tight event loop: gRPC account update → graph edge recompute → Bellman-Ford → quote chain → simulate → submit.

```
Yellowstone gRPC ──► Pool reserves/sqrt_price (atomic stores)
                              │
                    ExchangeGraph::update_pool()
                    (edge weight = −ln(rate), DashMap)
                              │
                    find_negative_cycles_with_diag()
                    (explicit path enumeration, 2- and 3-hop;
                     MAX_ARB_HOPS=2 skips the 3-hop scan)
                              │  cycle.total_weight < 0
                    optimize_input_and_tip()
                    (chain AMM/CLMM quotes, subtract fees + tip)
                              │  net_profit ≥ MIN_PROFIT_LAMPORTS
                    simulateTransaction  (RPC, semaphore-limited)
                              │  passes
                    JitoBundle::build()
                              │
                    ┌─────────┴──────────────────────────┐
                    │ use_direct_rpc=true                 │ use_direct_rpc=false
                    │ (thin cycle, BYPASS_JITO_BUNDLE)    │ (fat cycle or bypass disabled)
                    │ floor-anchored tip (~6_000L)        │ ratio-based tip
                    ▼                                     ▼
           POST /api/v1/bundles (Jito)        POST /api/v1/bundles (Jito)
           [tip ≈ floor × multiplier]         [tip = gross × tip_ratio]
           route: floor-tip in log            normal Bundle submitted log
```

**Concurrency model:** A single Tokio task runs Bellman-Ford and evaluation on every update signal. Simulation and submission use a `Semaphore(2)` so at most 2 in-flight RPC calls exist at once. Pool state is updated lock-free via `AtomicU64` / `AtomicI32` fields on `Pool`.

**Submission routing** (flash loan mode only):

```
optimize_input_and_tip()
  ① Run ternary search with candidate_direct model to find slippage-optimal amount_in
  ② actual_gross_bps = (gross_out / amount_in - 1) × 10_000   (real AMM margin, not graph rate)
  ③ use_direct = enable_flash_loan && bypass_jito_bundle && actual_gross_bps ≤ threshold
  ④ If routing flipped from ①, re-evaluate quote with correct fee model
    │
    ├── use_direct=true  (thin cycle ≤ jito_bundle_threshold_bps)
    │     tx_fee   = 2 base fees + CU fee   (arb tx + tip tx, same structure)
    │     jito_tip = floor_tip only          (floor × multiplier ≈ 6_000L)
    │     → jito.submit_bundle() — floor-tip competes in Jito auction
    │       Summary shows:  route: floor-tip
    │
    └── use_direct=false (fat cycle > threshold, or bypass_jito_bundle=false)
          tx_fee   = 2 base fees + CU fee
          jito_tip = ratio_tip OR floor_tip (whichever is larger)
          → jito.submit_bundle() — normal Jito bidding
```

**Why all cycles go via Jito:** Raw RPC with v0+ALT transactions fails with
`ProgramAccountNotFound` on non-Jito validators (~10% of stake) that don't correctly
resolve ALT-derived program accounts during block production. Jito validators handle
this correctly. The floor-anchored tip for thin cycles keeps 99.5% of profit.

**Raw-RPC carve-out (`ENABLE_RAW_RPC`, wallet-funded cycles only — any base):** a thin
wallet-funded 2-hop local-DEX cycle small enough to fit in ONE ≤1232-byte transaction
with **zero** address lookup tables is immune to that failure by construction (a v0
message with no `address_table_lookups` needs no ALT resolution), so it is sent via
plain `sendTransaction` instead of a bundle — no tip, no auction, valid
on every leader slot. Preflight is **ON** for raw sends (with
`preflight_commitment: Processed` to match the blockhash source — the node's default
`finalized` bank hasn't seen a `processed` blockhash yet and rejects every send): the
raw path skips the internal `simulate_opportunity` gate, so preflight IS its simulation,
and it rejects quote-divergent fills (e.g. DLMM `ExceededAmountSlippageTolerance` from
the bin-depth-blind quote model) for free instead of landing-and-reverting for fees. Works for both bases: a native (SOL) base's wrap
(`transfer`+`sync_native`) and WSOL-close instructions ride the same per-cycle size
probe. Flash-loan mode force-disables it (the borrow/repay mega-tx needs ALTs → Jito).
Everything else (fat, 3-hop, Jupiter, oversized, flash-mode) keeps the Jito path
unchanged. Eligibility is decided in `build_opportunity`
(`ArbOpportunity.raw_rpc`, size-probed with empty ALTs); the single tx is built by
`build_raw_wallet_tx` in `src/jito/bundle.rs`; outcomes are polled by
`monitor_raw_outcome` in `main.rs` and reported in the 10-min `RAW summary` line
(`src/arbitrage/raw_stats.rs` — separate from the LATENCY ring on purpose). Cost
asymmetry to remember: a failed bundle is free, but an included-and-reverted raw tx
pays base + priority fees — the summary's `est_fee_burn` tracks it; preflight (above)
exists to catch those before they land, so a `RAW send rejected (preflight/transport)`
log line is the free-fail path working, not an incident. A raw SEND error
never falls back to Jito (the tx may have propagated; double-execution risk) — only a
raw BUILD error does.

**Key env vars for submission routing:**

| Var | Default | Purpose |
|---|---|---|
| `BYPASS_JITO_BUNDLE` | `false` | Enable floor-tip path for thin cycles |
| `JITO_BUNDLE_THRESHOLD` | `20` bps | Cycles at or below use floor tip; above use ratio tip |
| `COMPUTE_UNIT_PRICE_MICRO_LAMPORTS` | `1000` | CU priority fee; raise to `200_000`–`500_000` for better landing |
| `ENABLE_RAW_RPC` | `false` | Wallet-funded only (flash off; any base): thin 2-hop cycles that fit one no-ALT ≤1232B tx go via raw `sendTransaction` (no tip/auction; see carve-out above) |
| `BASE_BALANCE_RESERVE_UNITS` | `0` | Base units held back from arb sizing AND the P&L-halt threshold — reserve for the momentum trader sharing the wallet |

## Base token (SOL default, USDC opt-in)

Every arbitrage cycle starts and ends at one **base token**, configured via `BASE_MINT`
(default = wrapped SOL, so an unchanged `.env` behaves exactly as before). Resolved at
startup by `resolve_base_token` in `src/dex/types.rs` into a `BaseToken { mint, decimals,
symbol, is_native }`; supported bases are SOL and USDC. The single real branch point is
**`is_native`** (WSOL vs a plain SPL token), not SOL-vs-USDC:

- **Funding/settlement** (`build_setup_instructions`/`build_teardown_instructions` in
  `src/arbitrage/evaluator.rs`): a native base wraps SOL into a WSOL ATA (`transfer` +
  `sync_native`) and closes it at teardown; a non-native base (USDC) funds directly from
  its wallet ATA — **no wrap, no close**. Flash loan is **force-disabled** for a
  non-native base (wallet-funded only — see `resolve_flash_loan_enabled` in `config.rs`).
- **Thresholds are in the base token's smallest unit** (SOL = 9 dp, USDC = 6 dp). The
  fields/env vars are base-neutral: `MIN_PROFIT_BASE_UNITS` (alias of legacy
  `MIN_PROFIT_LAMPORTS`) and `INPUT_BASE_UNITS` (alias of `INPUT_SOL_LAMPORTS`) — primary
  name wins, then alias, then default. The `Config`/`ArbOpportunity` fields are
  `min_profit_base_units` / `input_base_units` / `net_profit_base_units`. The profit gate
  is computed entirely in base units: SOL-denominated costs (tx fee, Jito tip) are
  converted to base units via `sol_cost_in_base_units` before subtraction (identity for a
  native base, so the SOL path is byte-identical).
- **Jito tips are always paid in SOL.** For a non-native base the SOL tip is sized by
  converting base-unit profit → lamports via a cached SOL/USD price
  (`src/arbitrage/sol_price.rs`). The price is polled **in-process** by a poller in
  `main.rs` (Kraken, ~45 s) into a process-wide static the evaluator reads — it must run
  in the bot's own process; the separate `portfolio-watcher` binary cannot reach this
  static. A stale/missing price → floor tip (and non-native cycles are skipped rather than
  mis-priced).
- **Dual-guard halt** (`src/arbitrage/capital.rs` + `main.rs`): base-unit P&L drawdown
  (2-strike debounce) **and** an independent immediate SOL gas-floor guard
  (`MIN_SOL_GAS_LAMPORTS`, default 0.1 SOL) that only fires for a non-native base.

| Var | Default | Purpose |
|---|---|---|
| `BASE_MINT` | WSOL mint | Base/starting token of every cycle. SOL or USDC. |
| `MIN_PROFIT_BASE_UNITS` | `10000` | Min net profit in base-token units (alias: `MIN_PROFIT_LAMPORTS`). |
| `INPUT_BASE_UNITS` | `1_000_000_000` | Max swap input in base-token units (alias: `INPUT_SOL_LAMPORTS`). |
| `MIN_SOL_GAS_LAMPORTS` | `100_000_000` | Halt if native SOL gas falls below this; enforced only for a non-native base. |

Running `base=USDC` also needs (operational, not code): USDC-quoted pools in `pools.json`
(+ `--init-alt`), the wallet funded with **both** USDC capital and SOL for gas, and the
in-process price poller running (it always is, in the main bot).

## Strategy research & the pairs trader

Two subsystems live under `src/portfolio/` alongside the momentum trader, both
documented in `docs/`:

- **`momentum-sim`** (binary `src/bin/momentum_sim.rs`, engine `src/portfolio/sim.rs`) —
  a walk-forward backtest harness that replays `assets/price_history.jsonl` through
  the production decision code to grid-search strategy parameters with an honest
  robustness verdict. Strategies: `momentum｜meanrev｜pairs｜relval｜relstrength`
  (plus a `per-token` subcommand for single-token breakdowns). Run:
  `cargo run --release --bin momentum-sim -- run [--strategy ...]`. **Per-token tuning
  (2026-09-06): `momentum-sim per-token-sweep --token X --history <validated file>`** replays the
  whole book with one token's `params` swept over the full factorial (min × trail × lookback ×
  z-gate × regime_filter), collapses identical outcomes into families that name the inert knobs,
  and lists the incumbent, the top rows per objective (test P&L, worst-slice, $/hour, least
  drawdown, SQN), the P&L-vs-σ Pareto frontier, a consensus list and paste-ready `params` JSON —
  the `optimize-momentum-config` skill documents the reading rules. **Verdict (updated
  2026-06-27): single-name momentum IS robust on the sample once trailing stops are wide
  (20–30%) — the old "0 robust" verdict was an artifact of the ≤12% default trail grid.
  159/4480 robust in a focused grid; trend-regime gating dominates (105/159).** Caveat:
  one favorable 70/30 test slice — promising, not proven. Market-neutral pairs remains the
  most regime-independent edge. The live momentum trader has a `MOMENTUM_REGIME_MODE`
  (`off|level|trend`) entry gate — `trend` = SOL slope_r2 clean-uptrend (regime momentum,
  backtest-preferred); compare modes with `momentum-sim regime-compare`. The grid is
  rayon-parallelized. Full reference + findings: **[docs/momentum-sim.md](docs/momentum-sim.md)**.
  Multi-slot live trading: `MOMENTUM_MAX_POSITIONS` (default `1` = single-slot, identical
  to the original trader; >1 fills free slots each tick, evicts the weakest-green held
  when full if `MOMENTUM_ROTATE_MARGIN>0`). Per-token `min_metric`/`trail_pct`/`max_run_pct`/
  `entry_max_z_obs`+`entry_max_z` (overbought gate; `entry_max_z_obs: 0` = exempt)/
  `lookback_obs` (per-token ranking window, must exceed 120 or the token never ranks;
  the sim sizes its trailing deque off the MAX override so a long-lookback token isn't
  starved) overrides in `momentum_tokens.json` apply per-slot. Metric stays global. Startup adoption
  (`MOMENTUM_ADOPT_WALLET_POSITION`) generalizes to multi-slot at N>1 (adopts up to free
  capacity sorted by USD value desc; single-slot still warns on ambiguity). Adopted
  positions never fade-exit, even when the token is curated (`Position::is_adopted`,
  derived from `entry_sig` so pre-existing state needs no migration): the adoption-time
  entry is not a cost basis, so both fade arms would compare against a meaningless price —
  trail/rotation/stagnation own those exits. **Paper-test
  first** (`DRY_RUN_MOMENTUM_TRADER=true`, `MOMENTUM_MAX_POSITIONS>1`) before any live
  multi-slot run — single-slot is the validated edge.
- **Exit-side mechanisms tested and REJECTED** (all default-off, all kept for the record):
  `MOMENTUM_INITIAL_STOP_PCT` (price stop below entry),
  `MOMENTUM_INITIAL_STOP_RELEASE_PCT` (require a real gain before the price stop is released
  — a +0.03% tick used to exempt a position permanently), `fade_stop` (drop the fade exit's
  green requirement unconditionally; sim-only, no env var), and
  `MOMENTUM_FADE_UNDERWATER_MAX_GAIN_PCT` (extend the fade exit to an underwater position
  whose peak never exceeded N% — a momentum-signal trigger, not a price level), and
  `MOMENTUM_FADE_UNDERWATER_SCORE` (give that arm its own bar *below* the entry bar so it
  fires later and stops pre-empting). **Five mechanisms, five losses out-of-sample.** Two failure modes recur and generalize:
  (1) any rule that fires on a *price level* cuts the positions that recover; (2) any rule
  that fires *earlier* than stagnation eviction **pre-empts** it — the underwater fade drove
  stagnation evictions from 4 to **0**, substituting a worse exit for a better one. Exit
  mechanisms **compete** for the same positions and are not additive, so a new one must be
  measured with the rest of the stack live (that is what the sweep's `evict` column is for).
  The lower-bar variant makes (2) precise: driving the bar down (0 → −10 → −30) walks held-out
  P&L back toward the off-baseline **from below** and never crosses it (+291 → +461 → +534.29
  vs off +534.62), because the only position it can reach is the one stagnation eviction
  already closes at a better price (worst −22.06 vs −14.01). A rule rare enough not to
  cannibalize is a rule too rare to contribute.
  Conclusion for the −$100.66 never-green class of trade: it is the premium paid for not
  clipping recoveries. That loss is prevented **upstream** — regime gate, per-token
  `min_metric`, z-gate — not on the exit side.
  **Momentum-DECLINE exit (2026-09-06, sim-only, REJECTED — six of seven).** "Exit when the
  momentum decreases, not only when it falls back to the entry bar": two green-only variants
  behind `maxn-compare --fade-decline-obs N` (score below its value N obs ago — the exit-side
  mirror of the `confirm_lag_obs` entry gate) and `--fade-decline-frac f` (score has given back
  fraction f of its peak since entry — a trailing stop on the metric); `ParamSet::fade_decline_*`,
  predicates `momentum::fade_on_decline` / `fade_on_score_drawdown`, exits tagged `sim-decline`.
  Measured on the live per-token configs, $1000, HYPE+ZEC 183 d (N=1 base +617/+766, N=2
  +1608/+960) and JitoSOL 80 d (+250/+191), 24 cells each
  (`assets/exit_decline_sweep_2026-09-06.txt`): the lagged variant multiplies trade count 2–3×
  and loses held-out at every lag ≤ 120 (−72 → −355), while lag 240 flips sign by slice (N=2
  test +171, train −464); the drawdown variant's only both-slices-positive cell (f=0.25, HYPE+ZEC
  N=1: +301/+49) fails at N=2 (train −332) and on JitoSOL (train −138, and it INTRODUCES a
  −$100 worst trade into a slice whose baseline worst was −$0.86). f=0.75 is test-positive and
  train-negative on all three runs — slice-dependent, not a mechanism. Two failure modes, both
  already on record: an earlier trigger pre-empts the fade/trail exits that were doing better,
  and every early exit changes the trade SEQUENCE — the freed slot re-enters later and trails out,
  which is where the new −$100…−$203 worst trades come from. Knobs kept default-off for the record.
  **Head-to-head of every other sim-only mechanism (2026-09-06, same harness, same baselines
  HZ +617/+766 · JITO +250/+191; `assets/exit_h2h_2026-09-06.txt`; single-value passthrough
  flags added to `maxn-compare`: `--confirm-k --no-fade --vol-stop-mode --chandelier-k --vol-obs
  --overbought-z --entry-dip-obs/-z --low-gate-obs/-pct --max-trail-pct --reinvest-frac
  --size-ceiling`).** Nothing on the signal or exit side beats the deployed per-token configs on
  both slices: `confirm_k=4` never binds (test identical); dip-timed entries collapse test to
  +26/+165 (HZ) and +18/+35 (JITO); the low-anchored gate is mixed (20%@10080: HZ test +61 but
  train −136 and JITO −32 — the +2→+11 in its code comment was an older HYPE config); overbought
  take-profit z2/z3: −346/−160 on HZ with 120% DD at z2; σ-scaled trails k60–200 all −120…−160 on
  HZ and never better on JITO (k≥90 reproduces the fixed-trail trades exactly); ATR trails put
  the HZ train slice at −132…+327 with ~100 trades; profit-protected `max_trail_pct` 15≡25
  (breakeven floor dominates, as before): HZ +466 (185 trades, 32% win), JITO +120 with a
  4768% DD. The ONLY cell positive in both slices on both files is equity compounding
  (`reinvest_frac` 0.5/ceiling 3000: HZ +671/+880, JITO +264/+195; 1.0/5000: +706/+1012,
  +278/+200) — identical trade lists, larger size after banked profit, i.e. a multiplier on the
  existing edge, not a new one, and it MUST wait for the honest-accounting fix (compounding off
  today's phantom +$11.5k realized would size to the ceiling immediately). Read: the curated
  stack (per-token bar + trail + fade + stagnation + LST regime-death) is a local optimum on this
  data; further signal work needs NEW data (price history for discovered/adopted mints, a
  volume axis), not new knobs on the same series.
  **Swing overlay + velocity crash exit (2026-09-06 PM, `assets/exit_swing_crash_2026-09-06.txt`).**
  Operator idea: sell into the spike, rebuy the bottom; use a sudden 1-min flush with heavy sell
  volume (STONK 15:35→15:45, −15% in 8 min) as the sell signal. (a) z-version = `--overbought-z`
  + `--entry-dip-obs/-z` + short cooldown: HZ 42→155 trades, held-out +799→+579 (cd300) / +420
  with 120% DD (cd3600); requiring a real dip for the rebuy → +32/+77; JITO +191→+17/+19. Dead.
  (b) velocity version = new sim-only `crash_exit_pct/obs` (green-only: price ≤ recent-N-obs high
  × (1−X%)): at the STONK-shaped widths (8–20% in 5–30 obs) it NEVER fires on HYPE/ZEC/JitoSOL —
  the curated book does not flush like a day-6 meme; at 2–4% it fires and is a wash (JITO
  2%@10 test +44 with train flat, HZ −145; 4%@30 HZ +8/−7). Side finding: a 300 s re-entry
  cooldown alone lifts the HZ baseline +617/+766 → +701/+799 (cd is `.env`-frozen; worth a
  proper sweep). The volume half of the signal is un-backtestable (recorder stores closes only)
  and STONK itself has no history (discovered/adopted mints are not recorded) — both point at
  the Tier-2 recorder change; live, a DOWN-spike twin of `grpc_pricer::detect_spike_bps` with
  the sell-side vault delta as volume confirmation is the implementable form (shadow → paper).
- **PROBE sizing** (`MOMENTUM_PROBE_USDC`/`_WINDOW_SECS`/`_MARGIN_PCT`, **SIM-ONLY, default
  off**) — enter with a small first tranche and commit the remainder only once the position
  proves itself inside the window: price above entry (margin 0) **and** the entry thesis
  re-validated (score ≥ the token's own `min_metric`, non-stale, regime ON). The overbought
  z-gate is deliberately NOT re-applied — a position that just went green IS extended, so
  that gate is anti-correlated with the trigger and vetoed the best top-ups (−$352). Basis
  blends on add (`momentum::blend_entry`) so fade/rotation/regime-death/trail all compare
  against true average cost. Shared-slot replay, 4 tokens, $1000 trade, 1 h window: probe
  $100 → full-period **+1287 vs +1731 (−444)**, held-out +1262 vs +1253 (+9), win 76%;
  probe $250 → −239 / +11 / win 81%. Held-out is noise, the full-period cost is not — win
  rate rises 70% → 81–89% (real smoothness) but costs 14–26% of P&L, a worse rate than
  stagnation eviction or regime-death, which cost nothing. **Not wired live for that reason**
  (`sim::base_params` is the only consumer; there is no probe-sized entry or top-up tick).
  Margin > 0 is strictly worse (0.25% costs $156 and RAISES drawdown 31→34) because this
  book's winners are slow starters — 40–83 h holds that had not gained 1% in their first
  hour. **Methodological note:** a first-order re-accounting of the fixed trade list
  predicted only −2%; the real replay is far worse because probe sizing changes the trade
  SEQUENCE (exits move, so the slot frees at different times: 53 → 45 trades, in-market
  1811 h → 2127 h). Never estimate a sizing change off a fixed trade list — replay it.
- **Regime-death exit** (per-token `regime_exit_obs` in `momentum_tokens.json`; sim-validated
  AND live-wired: `regime_off_run_obs` recomputes the off-run from the history deque each
  tick — restart-safe, no persisted state, zero cost when no held token opts in; exit reason
  `"regime death"`, shares the dwell-confirm with the other stop legs; global fallback
  `MOMENTUM_REGIME_EXIT_OBS`, default 0) — the ONE exit-side mechanism that survived after the five rejections
  above, because it uses a genuinely new trigger: the **entry premise**, not the position.
  For a token that IS the regime asset (an LST — JitoSOL ≡ SOL), exit an UNDERWATER position
  once the SOL trend regime has been continuously OFF for N obs (`sim-regime` tag in dumps).
  Three structural properties the failed five lacked: (1) the signal is the position thesis
  itself, not a price level — a still-trending position is never touched; (2) while the
  regime is off, entries are blocked anyway, so the freed slot cannot be misused — zero
  opportunity cost by construction; (3) JitoSOL cannot fall 10% without flipping the SOL
  regime, so the −10% never-green class is intercepted within the debounce BY CONSTRUCTION.
  Measured (156d, on top of stagnation 96h/2%): +77 net, worst JitoSOL trade −100.66 → −31.83
  (cut at −0.8% after 4.5h), in-market −106h, and it takes the JitoSOL squatter off
  stagnation eviction's hands 103h earlier at a better price. Debounce plateau is flat
  (D=60..720 all ≈ +59..+77); deploy D=480 = the regime's own window (principled, not
  fitted). The tax: one dip-recovery per 156d cut at −32 instead of −1 (that event is the
  whole −31 OOS delta on the 40/60 split — both benefit events fall in-sample; n=3 events
  total, same epistemic class as stagnation eviction). Applied to an idiosyncratic token the
  same rule measured **−$946** — the SOL regime is a foreign signal for ZEC/HYPE/cbBTC/WETH;
  the per-token scoping IS the mechanism. Do not add to any grid objective.
- **Stagnation eviction** (opt-in, `MOMENTUM_STAGNATION_HOURS`) — frees a slot from a
  position that stopped working. With M watched tokens and N<M slots, a **flat underwater**
  position is closed by *nothing*: the trail needs a giveback from a peak that never rose,
  fade-exit needs green, and `MOMENTUM_ROTATE_MARGIN` skips anything at or below entry
  (`rotation_net_green`), so it is unevictable at every margin. Measured on a 156-day
  single-slot replay of the deployed config, two such positions held the slot for 449 h and
  535 h — 26% of the whole window — and blocked more upside than the replay realized.
  A position is **stalled** when it has made no new high for `MOMENTUM_STAGNATION_HOURS`
  **and** still sits within `MOMENTUM_STAGNATION_BAND_PCT` of entry (default 2). Both
  conditions matter: the band is what separates *flat* from *falling*, and without it the
  rule is a stop-loss in disguise — a time-only version evicted a position at −16.9% that
  finished +18.5%. Below the band the trailing stop owns the exit. Eviction also requires a
  challenger clearing `MOMENTUM_STAGNATION_MARGIN` **and its own per-token entry bar** (not
  the global `MOMENTUM_MIN_METRIC`, which is ~10× the per-token overrides in practice);
  releasing a slot into cash pays two-way costs to hold nothing. Shared code: predicate
  `momentum::is_stalled`, live selection `momentum::weakest_stalled` (the no-green-gate
  sibling of `weakest_green`), execution via `try_rotate(..., EvictKind::Stagnation)` which
  skips both green gates but keeps the cost gate, divergence guard and daily cap. Live logs
  tag it `EVICT-STALLED`; the sim tags the trade `sim-stagnant`. Clock lives in
  `Position.peak_ts` (RFC3339, `serde(default)`; a pre-upgrade state file reads 0 = *not*
  stalled, so a restart never evicts on the first tick). **It is a rare-trigger tail guard,
  not a P&L engine** — at 96 h/2% it fires ~5× per 109 days; the pathology occurred twice in
  156 days and was validated out-of-sample once (+535 held-out, worst trade −14.01 vs
  −150.42). Aggressive settings (24–48 h, band 5–8%) fire often and **lost** money
  out-of-sample (−42 to −143). Deliberately NOT wired into any grid-search objective — with
  n=2 events, optimizing against it fits noise. Sweep it with
  `momentum-sim maxn-compare --stagnation-hours … --stagnation-band-pct …` (comma lists;
  reports train and held-out side by side with the loss tail). Paper-test before live.
- **Unwatched-holdings adoption** (opt-in, `MOMENTUM_ADOPT_ALL_TOKENS`, default off;
  spec: `docs/superpowers/specs/2026-08-09-adopt-all-tokens-design.md`) — a second
  adoption pass adopts NON-curated wallet tokens (minus WSOL/USDC/USDT + configured
  excludes, Jupiter-sellability-gated) into free slots, trail-only at `MOMENTUM_ADOPT_TRAIL_PCT`
  (no fade exit, rotation-exempt; stagnation eviction applies). Emails fire only on REAL
  adoptions (both passes, same trade-email path as ENTER/EXIT/ROTATE); paper mode sends
  nothing — the unwatched pass logs 'would adopt' lines only. Adopted unwatched tokens get
  dynamic gRPC pool wiring — the watcher resolves each one's best venue via DexScreener
  (volume-ranked, SOL/USDC-quoted, pumpswap/raydium/orca/meteora) and re-spawns the feed
  with the decoded pool, so the 1-s gRPC fast exit arm covers them; unresolvable/undecodable
  venues fail open to REST with bounded retry/cool-down. pools.json is never written;
  restart re-resolves. Adoption latency: with the flag on, the wallet re-scan runs every
  tick (~60 s, vs every 5), and re-adoption after the trader's own exit — or after an
  invalidation (a position written off without a sell once its balance is CONFIRMED
  zero on-chain) — waits `MOMENTUM_ADOPT_COOLDOWN_SECS` (default =
  `MOMENTUM_REENTRY_COOLDOWN_SECS`; set 60 for ~1-min adoption of manual re-buys); the
  watched pass honors the same bench. The decimals cache self-heals per tick, so a
  mid-run mint is sizeable (exit-able) the same tick it is adopted.
- **gRPC spike → fast entry** (opt-in, `MOMENTUM_SPIKE_ENTRY`; "latency accelerant") —
  when a watched token's gRPC price jumps up past `MOMENTUM_SPIKE_BPS` within
  `MOMENTUM_SPIKE_WINDOW_SECS`, the ingestion task signals the watcher (an `mpsc<mint>` on
  `GrpcFeed`, detector `grpc_pricer::detect_spike_bps`/`note_spike`) and a dedicated
  `select!` arm re-runs the **normal validated entry decision** for that one mint
  immediately (`momentum::maybe_enter_spike` → the same
  `rank_candidates`/`select_entries`/`try_open_position` as the 60s tick) instead of
  waiting up to 60s. The spike wins **latency, not the decision**:
  MIN_METRIC/regime/over-extension/cost/divergence/capacity/cooldown/daily-cap all still
  apply, and because the rank window is the 1-min `history` (the sub-second spike isn't in
  it), a spike **cannot manufacture** a passing metric — only accelerate a token that
  already qualifies. Requires `MOMENTUM_GRPC_PRICING`. **Un-backtestable** (sub-second
  events don't exist in the 1-min history) — default off; `MOMENTUM_SPIKE_SHADOW=true`
  (log-only) is the safe first stage; roll out **shadow → paper
  (`DRY_RUN_MOMENTUM_TRADER=true`) → live**. Do **not** wire the spike knobs into
  `optimize-momentum-config` (no 1-min signal to optimize); the detector math is
  unit-tested, but the edge is earned live, not in backtest.
- **Liquidity drain guard** (opt-in, `MOMENTUM_MAX_EXIT_IMPACT_BPS` / `_ENTRY_IMPACT_BPS`,
  default off; spec: `docs/superpowers/specs/2026-08-01-liquidity-drain-guard-design.md`) —
  the trader was price-only, so a pool draining *while a position is open* was invisible and
  the trailing stop would "fire" at a price nobody fills. The gRPC ingestion task now
  publishes each pool's **quote-side depth in USD** (`feed_setup::publish_depth` →
  `GrpcFeed.depth`), and the trader converts it to impact for the position it *actually*
  holds: for constant product this is **exact**, `impact = V/(D+V)` where **D is the QUOTE-SIDE
  reserve**, not the both-sides headline liquidity (`momentum::sell_impact_bps`;
  inverse `max_position_for_impact` sizes the entry cap). That position-sizing is the
  difference from the older `MOMENTUM_LOCAL_IMPACT` pre-gate, which quotes a fixed
  `MOMENTUM_TRADE_USDC` buy and so *understates* risk exactly on winners. The exit leg shares
  the dwell-confirm with the other stop legs (a momentary depth dip must not liquidate) and
  tags the trade `liquidity drain`; the entry cap trims the notional and **skips** the entry
  outright if the cap falls below half the configured size (a dust position pays two-way costs
  for nothing). **CP pools only** (`raydium_amm_v4`/`pump_swap`/`saber`): Whirlpool and CLMM
  publish nothing because a concentrated-liquidity vault total overstates depth usable near
  the tick — a confidently-wrong number is worse than none — and DLMM has no reserve signal.
  Every uncovered or stale case **fails open** (no exit, no cap), so the guard only ever acts
  on fresh data. Value is concentrated in the unvetted-add path (`/add-momentum-token`);
  it will never fire on JitoSOL/WETH/HYPE/ZEC. **Un-backtestable** — no liquidity history
  exists, so `momentum-sim` can never score it; stage `MOMENTUM_EXIT_IMPACT_SHADOW=true`
  (log-only) → paper → live, and remember it is an exit rule competing with the trail/fade/
  stagnation legs, where five of six tested mechanisms lost money out-of-sample.
- **Spike-crash exit gate** (opt-in, `MOMENTUM_SPIKE_EXIT`, shadow-first via
  `MOMENTUM_SPIKE_EXIT_SHADOW=true`; 2026-09-06) — the downward twin of the spike-entry detector.
  The gRPC ingestion task keeps one rolling price window per mint (`GrpcFeed::note_print`, shared
  with the up-detector; held mints are now INCLUDED — they used to be skipped) and, for HELD
  mints, runs `detect_drop_bps`: price ≤ `confirmed_high × (1 − MOMENTUM_SPIKE_EXIT_BPS)` where the
  high must be reached by two prints ≥ `MOMENTUM_SPIKE_EXIT_CONFIRM_GAP_MS` apart (a single swap's
  2–3 prints are one burst and can never be the baseline). A breach must repeat on
  `MOMENTUM_SPIKE_EXIT_CONFIRM_PRINTS` spaced prints (`advance_streak`); then a `CrashSignal` is
  published (`at` = first confirmation, `last` = latest breach) and cleared by the first recovering
  print. `maybe_exit` reads it only for positions priced from gRPC THIS tick (REST/distrusted/stale
  fail OPEN), ignores it past `MOMENTUM_SPIKE_EXIT_MAX_AGE_SECS`, re-checks the line against the
  tick's own price, and — live — sells with reason `"spike crash"`, BYPASSING the 3 s dwell (the
  spaced prints are the wick filter; a confirmed trailing stop still owns the attribution). Shadow
  writes one `SpikeExitShadow` audit per confirmed signal carrying entry/peak prices so each
  would-exit can be scored against the position's eventual `Exited.pnl_pct` — the ONLY dataset this
  gate will ever have: it is un-backtestable at sub-minute resolution (the sim cousin
  `crash_exit_pct/obs` sees 1-min closes and never fired at 8–20% on the curated book; on that book the
  original 5% gate was 2–6× tighter than every trail, so when it fires it REPLACES the trail's decision —
  the default is now **10% (1000 bps)**, operator decision 2026-09-06). **Per-token overrides** live in
  `momentum_tokens.json` `params`: `spike_exit: false` exempts a token (an LST whose 10 %/60 s flush is a
  SOL crash, not a token event), `spike_exit_bps` / `spike_exit_window_secs` replace the two globals
  (0 = off for that token; prints/gap stay global). `momentum::spike_exit_cfg_for` resolves them for BOTH
  the feed's detector (`GrpcFeed::set_crash_overrides`, installed at feed boot; the shared window is
  sized to the longest window in use) and the exit leg, so a token is detected and judged at one bar.
  Tokens without the fields — discovered/adopted included — inherit the `.env` globals, and the `.env`
  master `MOMENTUM_SPIKE_EXIT` (default off) is the only on-switch: no params block can enable the gate.
  **Dynamic bar** (opt-in, `MOMENTUM_SPIKE_EXIT_DYN_K`, default 0 = fixed bar): every slow tick the
  watcher sizes each HELD mint's bar from its own history — `k × σ(1-obs log returns over
  MOMENTUM_SPIKE_EXIT_DYN_OBS)` in bps, floored at `MOMENTUM_SPIKE_EXIT_MIN_BPS` (300) and capped at the
  position's trail (`momentum::dynamic_crash_bps` / `dynamic_crash_bars`; adopted-unwatched positions cap
  at the adopt trail) — and pushes the set to the feed (`GrpcFeed::set_crash_bars`, replace semantics).
  The feed layers it over the params base (`crash_resolution` → `CrashBarSource` global/static/dynamic;
  an exempt mint is never revived, an explicit per-token `spike_exit_bps` PINS the token so dynamic skips
  it, a mint with fewer than `MOMENTUM_SPIKE_EXIT_DYN_MIN_OBS` priced observations keeps the fixed bar),
  and the exit leg reads the bar FROM the feed so detector and verdict agree; `SpikeExitShadow` carries
  `threshold_bps` + `bar_source` so shadow events can be scored per bar. Sim cousin: `maxn-compare
  --crash-exit-k K` (with `--crash-exit-obs` and `--vol-obs`; `ParamSet::crash_exit_k`), 1-min closes,
  directional only — use it to pick k, not by feel. **Preview 2026-09-06**
  (`assets/exit_dyncrash_2026-09-06.txt`, $1000, live params, cd 3600): at k ≤ 5 the 300 bps floor binds
  on the WHOLE curated book (k3 ≡ k5 cell for cell — HYPE/ZEC/JitoSOL 1-min σ × 5 < 3%), so what the sim
  measured is a fixed 3% bar: HYPE+ZEC 2-obs window +17/−18 (N=1) and +14/−13 (N=2) = noise, 10-obs
  window −73/−83 … −13/−15 (both slices, all k); JitoSOL +16 (2-obs) / +38 (10-obs) test-only with the
  train slice untouched (no events). Verdict: no reason to enable it on the curated book — the dynamic
  part only ever exceeds the floor on meme-class σ (1–3%/min ⇒ 5–15% bars), which is where the gate was
  built for; scope it by PINNING curated tokens (`spike_exit_bps` in their params) so only adopted/
  discovered positions get a σ bar, and stage it through shadow (`bar_source: dynamic` in the audit).
  **Spike-TOP take-profit (2026-09-07, SIM-ONLY so far):** the upward mirror — sell a GREEN position once
  the rise from the window LOW is ≥ k × σ, sharing every crash knob (`maxn-compare --spike-tp-k K` with
  `--crash-exit-obs` as the low window, `--vol-obs`, the `MOMENTUM_SPIKE_EXIT_MIN_BPS` floor; uncapped;
  `ParamSet::spike_tp_k`, `sim::spike_tp_exit`, exits tagged `sim-spiketop`). Operator idea "exit when
  the price rises more than 5σ in one minute"; the live form, if the preview earns it, would be one master
  `MOMENTUM_SPIKE_TP` + `MOMENTUM_SPIKE_TP_K` reusing the crash gate's window/prints/gap/age/σ/floor/
  shadow/per-token exemption. Prior on record is negative (overbought-z take-profit −346/−160 on HZ,
  swing overlay dead); decision rule agreed BEFORE the numbers: build live only if some cell is
  non-negative in both slices on both files without a swing-like trade-count explosion. **Result
  (`assets/exit_spiketop_2026-09-07.txt`, k ∈ {3,5,8} × low window {2,10}, $1000, cd 3600):** the 3%
  floor binds again (k3 ≡ k5 ≡ k8 at @2), so it is a fixed "+3% above the 2-/10-min low" take-profit.
  @2: HYPE+ZEC N=1 +43/+10 (both slices, IDENTICAL at the 0.8 cut — the only exit mechanism today
  positive in both), N=2 +140/−2, JitoSOL −0/−12; @10: HZ train +166…+197 but test −72/−99, JitoSOL
  test-only +119. Every spiketop exit in the dumps is a winner (HZ 0.8 test: 4 ZEC exits +391, all on
  the Aug-21 run, re-entered at the 1-h cooldown), yet the book barely moves because the trail would have
  carried most of them further. FAILS the rule (JitoSOL test negative, HZ N=2 test flat) ⇒ NOT built
  live; knob kept default-off for the record. If revisited: exempt the LST (`spike_exit: false`) and
  measure HYPE/ZEC alone at the 0.8 cut before any live shadow.
  Two latent defects fixed alongside: `set_held` is refreshed right after entries/spike entries
  (a new position was invisible to the feed for up to 60 s) and resets a newly-held mint's window;
  the dwell arm is removed after any successful flatten (bypass sells used to leave a stale arm).
  Roll out shadow (≥ a week or several events) → paper → live; keep the knobs OUT of the optimizer
  grid. A dump followed by silence never produces a second print — the dwell legs remain the backstop.
- **Order-flow entry gate** (opt-in, `MOMENTUM_MIN_VOL_DECAY` / `MOMENTUM_MAX_SELL_BUY_RATIO`,
  default off; `src/portfolio/flow.rs`) — price alone cannot tell "rising on real demand"
  from "rising while every holder distributes into it". A 60 s background poller
  (`flow::spawn_poller`) pulls each watched pool's 1 h trade counts + volume from
  DexScreener into `FlowCache`, and `flow::flow_gate` vetoes an entry on either a **volume
  collapse** or **distribution into strength**. Two design facts matter. (1) DexScreener
  publishes `txns.{buys,sells}` and one `volume` total — there is **no buy/sell volume
  split** — so a bare ratio can't separate dust sells from real distribution; the
  `min_txns_h1` **guard** (default 200, hence non-zero) exists because JitoSOL logged
  **67 sells against ONE buy** while rising, the most extreme ratio in the book on its
  healthiest token. (2) The ratio only fires when the price is **rising** — sells dominating
  a falling price is ordinary and the metric's slope already handles it. Volume is expressed
  as **decay vs the token's own 24 h hourly average** rather than an absolute floor, so a
  natively quiet deep pool isn't punished (book measured 0.55–1.32× on 2026-08-01; 0.3 has
  headroom, ratio baseline 1.3–3.2 so ~5.0 is the outlier line). All four knobs are
  **per-token overridable** in `momentum_tokens.json` — the point of the feature, since a
  deep LST and a day-6 pump.fun token need opposite answers. Runs before the Jupiter quote
  (a rejection costs no REST call), audited as `SkipFlowGate`. Stale/missing/poller-off ⇒
  **fails open**. **Un-backtestable** (`price_history.jsonl` holds prices only), which is
  why the reading is logged every tick — console `momentum flow:` plus
  `ActionKind::FlowSnapshot` in `momentum_actions.jsonl` — **even with every gate off**:
  that record is the only dataset that will ever exist for judging the thresholds.
  Entry-side, so a false positive costs an opportunity, not a position.
- **Monitor-tick health & background fetchers** (2026-09-05; `src/portfolio/tick_timing.rs`,
  `src/portfolio/rest_prices.rs`, watcher.rs) — the trailing stop is evaluated by the SAME
  single `select!` loop that runs every network-bound slow-tick step, so a stalled step is a
  blind stop: from 2026-08-30 the loop was dark 9–20 h/day (16-min blocks every 17 min on
  09-04) and two late stop fills cost ~$67 of the week's ~$111 real loss. Three layers, all
  in `.env.example`: (1) **measure** — every slow tick writes `ActionKind::TickTiming
  { gap_secs, total_ms, steps }` (per-phase ms: `wallet_scan`, `scan`, `venues`, `wiring`,
  `prices`, `history`, `risk`, `decimals`, `reconcile_adopt`, `evict`, `enter`,
  `pairs_liq`, `alerts`), warns past `MOMENTUM_TICK_WARN_MS` naming the slowest phases, and
  emails when the start-to-start gap exceeds `MOMENTUM_MAX_TICK_GAP_SECS` (0 = off); (2)
  **bound** — every await on the loop is capped (`MOMENTUM_SCAN_TIMEOUT_SECS` with
  `kill_on_drop` on the node children, `MOMENTUM_WALLET_SCAN_TIMEOUT_SECS`,
  `MOMENTUM_PRICES_TIMEOUT_SECS` via `pricer::fetch_prices_until` which KEEPS partial
  results, `MOMENTUM_ADOPT_TIMEOUT_SECS` per await inside adoption — never around a
  load-modify-save function, which audits/emails before it saves — and
  `ALERT_EMAIL_TIMEOUT_SECS`); the 1 s exit tick's own REST fetch is capped at 5 s;
  `fetch_symbol_map` now uses the lite-api verified list (the old `token.jup.ag/all` host
  stopped resolving), memoised hourly, with a negative cache for DexScreener symbol misses;
  `maybe_retry_entry` clears `entry_attempt` when the retry fails an ordinary gate
  (`retry_verdict`, audited `EntryRetryCleared`) instead of re-running the full entry path
  at 1 Hz; (3) **move off the loop** (opt-in, default off, paper-first): `MOMENTUM_REST_BG`
  (background Kraken+DexScreener price cache read by the slow tick AND `maybe_exit`; gRPC
  cross-check mints still read live, bounded; a price older than
  `MOMENTUM_REST_MAX_AGE_SECS` is ABSENT, never a fake move), `MOMENTUM_SCAN_BG` (scan child
  + cold Birdeye warm-up on a task, results over a channel; `history` is only ever merged on
  the monitor task), `MOMENTUM_WALLET_BG` (wallet re-scan task publishing snapshots; a
  snapshot predating the last live fill is skipped, one older than
  `MOMENTUM_WALLET_MAX_AGE_SECS` makes reconcile/adoption skip the tick). No background task
  writes `momentum_state.json` — every `maybe_*` is load-modify-save and safe only because it
  runs serially. Still inline by design: swap confirmation (`CONFIRM_TIMEOUT` 90 s, rare;
  moving it needs a single-writer redesign of the state file) and pool decode/feed re-spawn
  (bounded 30 s). Verify a deployment with the `TickTiming` records: p99 `total_ms` should
  sit in the low seconds and recorder gaps in `price_history.jsonl` should vanish.
- **Honest accounting & custody re-basing** (2026-09-06; `momentum::balance_adjust` /
  `reconcile_position_sizes`, `momentum_state::{CloseKind, BasisKind, TradeRecord::pnl,
  realized_since}`) — the exit sells the WHOLE on-chain balance of a held mint (operator
  request 2026-08-23: a manual Solflare top-up rides with the position), but until 2026-09-06
  the close record divided those proceeds by the bot's own basis: a $10 ZEC entry on a 1.7-ZEC
  manual bag booked +14,524%, 20 of 98 records were inflated ×1.1–×143, `momentum_pnl.json`
  read +$11,503 on a ~$1,300 wallet (honest: −$111 on real sells), and the loss breaker —
  which compares that lifetime figure with `-MOMENTUM_MAX_LOSS_USDC` — was ~$11,650 out of
  reach. Now: (1) every slow tick, `reconcile_position_sizes` compares each live position's
  tracked amount with the wallet snapshot (after the 180 s entry-age guard, mints absent from
  the snapshot left to invalidation); a surplus beyond `MOMENTUM_REBASE_TOLERANCE_BPS` (50) is
  folded in at the current mark — `usdc_spent += surplus × mark`, entry = cost-basis average,
  trail peak untouched (a price level) — audited `Rebased`; a shortfall (manual partial sell)
  trims tokens and basis proportionally, audited `Trimmed`; `TraderState.rebased_mints` marks
  the position so its close record is `BasisKind::Rebased`. `flatten_position` repeats the
  check against the raw balance it is about to sell (exit-mark fold ⇒ zero P&L on a surplus the
  tick missed). Detection-time marking is the adoption rule generalised: the bot's P&L on the
  operator's leg starts when it took custody. (2) `TradeRecord` gains `token_amount`,
  `gas_usdc`, `close_kind` (Sold | Rotated | Invalidated), `basis_kind` (Entered | Adopted |
  Rebased), all `serde(default)`; `TradeRecord::pnl()` trusts proceeds when the quantity is
  known and values a LEGACY record (`token_amount == 0`) at `usdc_in × (exit/entry − 1)`.
  `summarize` realizes real sells only — `Invalidated` legs (mark-to-market, nothing sold) go to
  `written_off_usdc/_trades`, never into the breaker — so the sidecar self-heals on the next
  close without editing history. (3) `finalize_pnl_and_halt` reads that honest figure, optionally
  over a rolling `MOMENTUM_MAX_LOSS_WINDOW_HOURS` (0 = lifetime). Also: `load` heals a 0/NaN/
  below-entry peak (`repair_peak`, previously dead code) and prunes exit-escalation counters of
  unheld mints (AAPLx sat at 400 ⇒ its next exit would have quoted at the slippage cap);
  `SkipInsufficientUsdc` carries the candidate `symbol` and runs after the capacity check. The
  simulator's records are unchanged in value (`build_trade_record` fills quantity = tracked).
- **Dropped-submission handling** (always on; `SubmitOutcome` / `classify_confirm` in
  `src/portfolio/momentum.rs`) — a submitted swap has THREE outcomes, not two. Until
  2026-08-29 the confirm loop only knew "confirmed" and "gave up at 45s", and every
  caller booked the second as a fill: two ZEC entries that never landed became open
  positions, wrote fake `TradeRecord`s, burned daily-cap slots and benched the mint for
  the full `MOMENTUM_REENTRY_COOLDOWN_SECS` it never earned. The fix leans on a hard
  Solana guarantee — **once a tx's blockhash expires it can never land** — so an expired
  hash with no signature status is *proof* of non-inclusion, not a guess. `Dropped` ⇒ the
  caller undoes nothing because nothing happened: entry stays FLAT (no position, no
  cooldown, no slippage escalation — a drop is a propagation failure, not a slippage
  one), a tranche stops the ladder without counting as spent, **rotate keeps A and never
  opens B**, and **exit keeps the position with its stop still armed** — and, like entry,
  without escalating slippage: tolerance lives inside the swap instruction and is only
  evaluated if the tx executes, so it cannot influence inclusion. Escalating on a drop
  buys nothing and costs real money (at a 5 bps base, seven consecutive dropped exits
  would re-quote at 640 bps). The lever that DOES affect inclusion is
  `MOMENTUM_PRIORITY_FEE_LAMPORTS`; repeated `SubmitDropped` lines are the signal to
  raise it. The rotate/exit legs mattered most: booking them optimistically drops a REAL
  holding out of tracking, which no reconciliation pass restores (invalidation only ever
  removes positions). `Unknown`
  (deadline hit, blockhash still live) keeps the old optimistic behaviour with the 180s
  invalidation guard as backstop — the change only ever acts on a certainty. Evidence
  ordering is the safety property: a status ALWAYS outranks an expired hash, because a tx
  can land in its last valid slot and a false `Dropped` would let the trader buy the same
  token twice. Sibling of the arb side's `classify_raw_status`, deliberately not shared
  (that one collapses "unresolvable" into `Expired`, which is free for arb and a double-buy
  here). The loop also **re-broadcasts every 2s** until the hash dies — one
  `sendTransaction` is a single shot at a single leader and a non-staked shared RPC drops
  them under load, which is what actually ate the ZEC entries. `MOMENTUM_PRIORITY_FEE_LAMPORTS`
  (default `0` = omit the field, keeping Jupiter's auto fee — byte-identical to the old
  request) attaches an explicit fee if drops persist. Audited as `SubmitDropped { leg }`.
- **Staged (TWAP) entry** (opt-in, `MOMENTUM_ENTRY_STEPS`) — split the entry notional into
  N≥2 sequential swaps (`MOMENTUM_ENTRY_STEP_SLEEP_SECS` apart, default 1 s; steps clamped
  to 10), trading price impact for gas. Lives entirely in `try_open_position` so it applies
  uniformly to slow-tick and spike entries and always records **one** `Position` (one slot,
  one daily-cap count). Gates run once on tranche 1 (gas charged per tranche); a mid-ladder
  tranche failure **keeps the partial fill** and stops buying (audited as `EntryStepFailed`),
  never touching the entry-escalation record. Unset = single-swap, byte-identical to before.
  Execution-level knob: invisible to momentum-sim, do **not** grid it. Caveat: tranches run
  inline on the watcher task — each live tranche can stall exits up to ~45 s confirm + sleep.
- **Live token discovery** (opt-in, `MOMENTUM_SCAN_ENABLE`) — when the momentum trader
  is live, the watcher runs `scripts/scan_tokens.js --json` every
  `MOMENTUM_SCAN_INTERVAL_SECS` (~hourly) to find liquid, Jupiter-verified, non-wash
  tokens (Birdeye top-volume ∩ verified, minus stables/wrapped, with volume/liquidity
  floors and a vol/liq ratio cap; Jupiter helpers shared via `scripts/lib/jup.js`). The
  **top-3 by 24h volume** are held **in memory** and ranked alongside the curated list
  (`curated ∪ discovered ∪ held`); `assets/momentum_tokens.json` is never written by this
  path, and a restart resets to the curated list. It is a curation heuristic (broadens
  *what's watched*), not a momentum edge. Manual one-off:
  `node scripts/scan_tokens.js --apply` appends survivors to the curated file.
  Since 2026-08-29 (FONE incident: 88.72% bundler-held supply live-entered — top-10
  10.6%, largest wallet 1.44%, launch-block bundle 1.01%, so every balance cap passed)
  the momentum scan also rejects **day-scale launch bundles** (`SCAN_MAX_DAY1_PCT`,
  default 8: non-pool top-20 supply born within `SCAN_DAY1_WINDOW_SECS` (86400) of
  launch — same sampled data as the 300s bundle screen, zero extra RPC; balance
  uniformity was calibrated and REJECTED as a discriminator: JitoSOL/ZEC/WIF top-20s
  are *flatter* than FONE's, wallet AGE is what separates them) and **too-young
  tokens** (`SCAN_MIN_TOKEN_AGE_DAYS`, default 5, oldest DexScreener `pairCreatedAt`).
  Both are position-risk gates: the arb child zeroes them like the other carve-outs.
  Since 2026-07-22 the scanner also emits each survivor's best **PumpSwap** pool
  (`SCAN_POOL_ENRICH_MAX`, default 5; DexScreener highest-24h-volume rule) and the
  watcher **gRPC-wires discoveries dynamically**: on a changed discovered-pool set it
  decodes the pools ad-hoc (`fetch_pumpswap_pools.js --pools …`, vault↔mint
  cross-checked) and re-spawns the price feed with them merged in (curated pools.json
  entries win on collision; pools.json is never written; non-PumpSwap-venue
  discoveries stay REST-priced). Also since 2026-07-22 the scanner rejects
  concentrated supply (`SCAN_MAX_TOP_HOLDERS_PCT`, default 30) and ranks by
  `MOMENTUM_SCAN_CHANGE_WINDOW` (set `4h` to match the trader's return metric). Since
  2026-07-28 `MOMENTUM_SCAN_RANK=slope` (live setting) ranks finalists by a GT-OHLCV
  ln-slope×R² over that window — the trader's own trend semantics — keeping only
  positive-slope tokens, so a pumped-but-rolling-over mover (price up on the day, slope
  negative) never takes a discovery watch slot.
- **Pairs trader** (`src/portfolio/pairs_{config,signal,state,trader}.rs`) — a
  market-neutral xStocks pairs strategy (the only strategy the backtests found a
  robust edge for). **Phase 2a = paper mode only** (no on-chain calls), gated by
  `ENABLE_PAIRS_TRADER` / `DRY_RUN_PAIRS_TRADER`. On-chain Kamino-borrow execution
  is Phase 2b–2d, planned not built. Reference: **[docs/pairs-trader.md](docs/pairs-trader.md)**;
  plan: `docs/superpowers/plans/2026-06-21-onchain-pairs-trader.md`.

Tests for both: `cargo test --lib sim::` and `cargo test --lib pairs`.

## Key types and their locations

| Type | File | Purpose |
|------|------|---------|
| `Pool` | `src/dex/types.rs` | Central state for one pool: atomic reserves, sqrt_price, fee_bps, tick_current_index, `clmm_tick_array_bitmap [AtomicU64; 16]`, `extra` accounts |
| `PoolRegistry` | `src/dex/mod.rs` | Maps vault/state/lp accounts → `Arc<Pool>` for O(1) gRPC dispatch; also `vault_index`, `state_index`, `lp_index` |
| `ExchangeGraph` | `src/graph/exchange_graph.rs` | `DashMap<(Pubkey,Pubkey), Edge>` — one edge per ordered token pair, weight = `−ln(rate)` |
| `ArbCycle` | `src/graph/bellman_ford.rs` | Path + edge list + `total_weight`; sorted most-negative first |
| `ArbOpportunity` | `src/arbitrage/opportunity.rs` | Amounts, swap instructions, slippage-guarded thresholds, net profit; `use_direct_rpc: bool` = thin cycle (floor-tip) flag |
| `SimOutcome` | `src/arbitrage/simulator.rs` | `Passed` / `MarketRejected` (cooldown) / `InfraError` (suppress 30 s) |

## Pool config (pools.json)

Each entry is a flat JSON object. Fields consumed by `PoolConfig` → `Pool::try_from`:

```json
{
  "id": "<pool pubkey>",
  "dex": "raydium_amm_v4" | "raydium_clmm" | "orca_whirlpool" | "meteora_damm" | "meteora_dlmm" | "phoenix",
  "token_a": "<mint>",
  "token_b": "<mint>",
  "vault_a": "<SPL token account>",   // subscribed for reserve updates
  "vault_b": "<SPL token account>",
  "fee_bps": 25,
  "state_account": "<pubkey>",        // CL pools only — carries sqrt_price
  "stable": false,
  "extra": { ... }                    // DEX-specific accounts (see check_extra in dex/mod.rs)
}
```

`PoolRegistry::validate()` is called at startup and hard-errors on any missing `extra` fields. The `check_extra` function in `src/dex/mod.rs` lists every required field per DEX kind.

## Dynamic arb pool discovery (offline periodic re-scan)

`pools.json` is normally generated by `fetch_all.js` from top-N-by-liquidity queries plus
hand-pinned addresses. `scripts/scan_arb_pools.js` adds **automatic discovery**: it takes
trending, security-screened tokens and admits only those that form executable cycles,
inside a fixed subscribed-account budget.

Pipeline: `scan_tokens.js` filters (verified, volume/liquidity floors, anti-wash vol/liq
cap, top-holder cap) → **arb safety gate** (`lib/token_safety.js`: freeze authority off,
no Token-2022 transfer hook — both trap capital *between legs*, a risk pricing-only
consumers never face) → **venue resolution** (`lib/venues.js`, best pool per venue by 24h
volume) → **cycle-closure** (`pruneToCycles`: a non-hub token needs ≥2 *tradeable* venues
and must reach SOL or USDC — so a PumpSwap-only token is dropped by construction while a
graduated one qualifies) → **budget prune** (`lib/book_budget.js`: protected core always
kept, activity-ranked fill, eviction hysteresis, hard `ARB_ACCOUNT_BUDGET` cap) → decode
via the per-DEX fetchers' `--pools` flag → atomic validated write.

`scripts/arb_refresh_loop.sh` runs it periodically: scan `--apply` → `--init-alt` →
`kill -HUP` the bot, which **re-execs itself** (same PID, same terminal — no supervisor).
Exit codes: `0` changed, `10` unchanged, other = failure (book untouched).

**Floor-only mode:** `ARB_DISCOVERY_ENABLE=false` skips the trending scan entirely — the
book becomes deterministically core + floor tokens (`assets/arb_raw_floor.json`). This is
the only reliable way to hold the book to an exact target set: discovery re-admits any
mover that trends during an apply, and suppressing it via `SCAN_MIN_VOLUME` silently fails
(`arbScanChildEnv` spreads the arb defaults OVER `process.env`; `ARB_SCAN_MIN_VOLUME` is
the real override name).

**Run `node scripts/scan_arb_pools.js` (report mode, writes nothing) and inspect the diff
for a few cycles before enabling the loop.** Caveat: PumpSwap only counts as a tradeable
venue when `ENABLE_PUMPSWAP_TRADING=true`; otherwise pump pools stay pricing-only and
cannot close a cycle.

**Raw-RPC FOCUS mode (`ARB_RAW_RPC_FOCUS`, default on since 2026-07-26)** rebuilds the book
around the no-tip raw-RPC 2-hop edge *only* (see the raw-RPC carve-out above). The 2-hop
**quote follows the bot's `BASE_MINT`** (`resolveRawQuote`: unset → SOL, else SOL or USDC),
so the scanner always builds the book for the base the bot actually trades. The protected
core shrinks to **momentum-watcher pools + SOL/USDC/USDT hub↔hub pricing pools** — majors,
general memecoins, and the broad fetcher pins are all dropped; general incumbents are NOT
carried forward. The only tokens admitted are freshly-discovered movers with **≥2 quote
venues each ≥ `ARB_RAW_MIN_USDC_LIQ`** (default `50000`; env name historical — it applies
to whichever quote `BASE_MINT` selects) — the `QUOTE→X→QUOTE` shape the raw path lands —
with each quote leg's activity boosted ×`ARB_RAW_RPC_BOOST` (default `3.0`). A token with
<2 liquid quote venues is skipped even if it forms a 3-hop cycle; a mover with several
(like PUMP, 4 USDC venues) is admitted. Floor tokens (`assets/arb_raw_floor.json`) keep
their quote-side legs through every scan. **Safety
guard:** the scanner **refuses to write a core-only book** — if a scan surfaces no
raw-eligible targets (Birdeye discovery is rate-limited and flaky per-scan, so `0 discovered`
happens) it exits `10` (unchanged) rather than stripping every arb target and idling the bot,
so the working book persists until discovery recovers. Because admission is discovery-driven,
a proven target that isn't a mover this scan (e.g. ANSEM) is dropped until it trends again —
pin it in a fetcher's `TARGET_POOLS` if you want a permanent floor. Set `ARB_RAW_RPC_FOCUS=false`
for the legacy general-arb book (majors + any cycle-closing discovery).

## DEX-specific notes

**Raydium AMM V4** — constant-product; reserves read from vault SPL token accounts (byte offset 64).

**Raydium CLMM** — `sqrt_price_x64` at offset 253, `tick_current` at offset 269, `tick_array_bitmap [u64; 16]` at offset 910 of the pool state account. `observation_key` at offset 201 (32 bytes). Tick array PDAs use big-endian `start_index.to_be_bytes()` as seed. `TICK_ARRAY_SIZE = 60`. The bitmap can lag on-chain state, so `swap_tick_arrays` falls back to repeating `start0` for all 3 slots when the bitmap is absent or stale — MEV swaps never cross tick array boundaries.

`swap_v2` account order: `[0]payer [1]amm_config [2]pool_state [3]input_acct [4]output_acct [5]input_vault [6]output_vault **[7]observation_state** [8]token_program [9]token_program_2022 [10]memo_program [11]input_mint [12]output_mint [13–15]tick_arrays`. Observation_state is at index 7 (before programs/mints), tick arrays are remaining_accounts.

**Orca Whirlpool** — `sqrt_price_x64` at offset 65, `tick_current_index` at offset 81. `TICK_ARRAY_SIZE = 88`. `tick_array_0/1/2` and `oracle` are required `extra` fields.

**Meteora DLMM** — does **not** enforce any token_x/token_y ordering when creating lb_pairs. `token_x_mint` is at lb_pair offset 88 and must be read at startup to determine orientation. Cached in `pool.dlmm_token_a_is_x` (1=token_a is X, 2=token_b is X) by `parse_state`. Do NOT use `pool.token_a < pool.token_b` to determine orientation — it is unreliable across pools.

Since 2026-07-27 DLMM quotes can use a **real bin fill walk** (`DLMM_BIN_QUOTE=off|shadow|live`,
default `shadow`): per-pool gRPC owner+memcmp filters (offset 24 = lb_pair) stream every
BinArray (10,136 B; amounts @+0/+8 of each 144-B bin) into `Pool.dlmm_bins`
(`RwLock<DlmmBinCache>`, active-array ±2 window, `try_read` + haircut fallback — never
blocks); `parse_state` also decodes the dynamic-fee params (StaticParameters @8..40,
VariableParameters @40..72) so the walk charges the real base+variable fee. `shadow`
logs `dlmm-shadow` walk-vs-haircut divergence lines from the evaluator (near-miss +
final-size call sites); flip to `live` after a clean session. Pools with a transfer-fee
(Token-2022) mint are pinned to the haircut quote. The swap builder derives bin-array
coverage from the walk (up to 3 arrays) — and note the neighbour direction: `swap_for_y`
walks bin ids DOWN (array −1), per Meteora's reference (the pre-2026-07-27 builder had
this inverted). Startup seeds active±1 arrays per pool via RPC (`Seeded N DLMM bin
arrays` log); the backfill poller re-fetches them alongside lb_pair state.

In `live` mode the **graph edges are walk-derived too** (`edge_rate_via_walk`, probe = 1%
of the input-side reserve): the marker rate on a coarse-bin pool whose active bin holds
dust manufactures permanent mirage cycles (graph +bps, quote negative → `quote_failed`
forever — the EtPcWELe/DXfnX2oC class), so `update_pool` prices DLMM edges from the same
bin walk the quote layer uses, marker as fallback. BF then only surfaces fillable edges.

**Meteora DAMM** — uses vault LP token balances and LP mint supply to compute virtual reserves. Subscribes to `a_vault_lp` / `b_vault_lp` accounts (via `lp_index`) in addition to vaults.

**Phoenix** — CLOB; price parsed from FIFOMarket account. `phoenix_base_lot_size` and `phoenix_quote_lot_size` required in `extra`. Real liquidity is typically thin — treat Phoenix cycles with caution.

**PumpSwap** — pump.fun AMM, CP with two SPL vaults. **Pricing-only by default**
(`PoolRegistry::load` skips `dex:"pump_swap"`), so the portfolio-watcher's gRPC feed can
price momentum tokens whose liquidity lives there. Pools come from
`scripts/fetch_pumpswap_pools.js` (pinned `TARGET_POOLS`, on-chain layout decode with a
mandatory vault↔mint cross-check; also emits `token_program_a/b` + `pumpswap_coin_creator`).

*Phase 2 behind `ENABLE_PUMPSWAP_TRADING`* (default false — **builder VALIDATED on-chain
2026-07-25; see [docs/pumpswap-trading.md](docs/pumpswap-trading.md)**):
`pumpswap::build_swap_instruction` emits the AMM's FULL declared interface — **buy=23 /
sell=21 accounts, read from the program's own on-chain Anchor IDL** (buy exact-out, sell
exact-in; discriminators Anchor `sha256("global:…")`; all PDAs **asserted equal to
live-mainnet constants**; token-2022 threaded). `FEE_PROGRAM` / `PROTOCOL_FEE_RECIPIENT`
are sourced on-chain and banked as consts; `check_extra` needs only the per-pool
`pumpswap_coin_creator`. The 2–4 trailing accounts seen on organic swaps are OPTIONAL
buyback `remaining_accounts` (a rotating fee-program `BuybackVault` with no PDA seeds) —
proven unnecessary by `simulateTransaction` of the tail-stripped 23-account buy, which
resolved every account and entered `Buy` (failing only on an uninitialized user ATA that
the evaluator's setup instructions create in real cycles). Pump cycles route via
flash+Jito+ALT, not the raw no-ALT path (23 accounts don't fit). Run the in-context
`simulateTransaction` gate in the doc before enabling on real funds.

**Jupiter** — *synthetic, vault-less* aggregator edge. Fundamentally different from every other DEX: it has no on-chain account to subscribe to via gRPC. Instead a background REST poller (`dex::jupiter::spawn_poller`) hits the **self-hosted swap-api** `/quote` periodically and stores an implied marginal rate per direction on the pool's atomics; the hot-path `get_quote` reads that cache and applies a conservative implied-CP-reserve impact model (so it stays synchronous like every other DEX). The real route + instructions are fetched once, at submit time, from `/swap-instructions` by `resolve_jupiter_hops` in `main.rs` (the only Jupiter network round-trip in the submission path), which splices the returned instructions into the opportunity, merges Jupiter's own ALTs with the bot's, and re-runs the wire-size guard.

- **Config is separate from `pools.json`**: Jupiter pairs live in `jupiter_pairs.json` (a flat list of `{ "token_a", "token_b" }`), loaded by `PoolRegistry::load_jupiter_pairs` into the **id-keyed map only** — never `vault_index`/`state_index`/subscription. `Pool::new_jupiter` builds them with a deterministic id (hash of sorted mints) and sentinel `Pubkey::default()` vaults.
- **Atomic field reuse** (Jupiter pools only): `sqrt_price_x64` = a→b implied rate (f64 bits), `damm_virtual_price` = b→a rate, `reserve_a`/`reserve_b` = per-direction probe impact, `a_lp_balance` = probe size. Edge generation lives in a dedicated `update_pool` branch mirroring the Phoenix two-atomic pattern (the two directions are independently polled and **not** reciprocal).
- **REST client is hand-rolled** on `reqwest` + serde (not the `jupiter-swap-api-client` crate) to avoid a conflicting `solana-sdk` transitive pin.
- **Accepted limitation**: in flash-loan single-tx mode a Jupiter route (itself multi-DEX) often exceeds 1232 bytes alongside borrow/repay → the resolver returns an error and the cycle is gracefully skipped. The wallet-funded fallback in `build_opportunity` does **not** fire for these (size check happens later, in the resolver).

**Jupiter env vars:** `ENABLE_JUPITER` (default `false`), `JUPITER_API_URL` (default `http://127.0.0.1:8080`), `JUPITER_BINARY_PATH` (unset = run Metis externally; set e.g. `./metis-binary` = bot auto-launches it), `JUPITER_BINARY_KEY` (required for auto-launch — Metis `--binary-key` license, secret), `JUPITER_PAIRS_PATH` (default `jupiter_pairs.json`), `JUPITER_POLL_INTERVAL_MS` (default `500`), `JUPITER_PROBE_LAMPORTS` (default `1_000_000_000`; reference size for marginal-rate polling — note non-SOL inputs are probed in raw base units, so the impact estimate is crude for pairs far from SOL value).

## Simulation error handling

`SimOutcome` in `src/arbitrage/simulator.rs`:
- **`MarketRejected`** — the opportunity has disappeared (price moved); suppress with cooldown (≈30 s). Anchor constraint errors in range 2000–2999.
- **`InfraError`** — transient RPC or account state issue; suppress the pool without penalising the cycle. Anchor errors 3000–3099 (e.g. `AccountNotInitialized=3012`, `AccountOwnedByWrongProgram=3007`).

## Adding a new DEX

1. Add a variant to `DexKind` in `src/dex/types.rs` with its `program_id()`.
2. Add required `extra` fields to `PoolExtra` (also in `types.rs`).
3. Implement `get_quote(pool, amount_in, a_to_b) -> SwapQuote` and `build_swap_instruction(...)` in a new `src/dex/<name>.rs`.
4. Wire `parse_cl_pool_state` or vault parsing in `src/dex/mod.rs`.
5. Add the `extra` validation arm to `check_extra` in `src/dex/mod.rs`.
6. Register subscriptions in `src/streamer/subscription.rs`.
