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
                    (explicit path enumeration, 2- and 3-hop)
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

**Key env vars for submission routing:**

| Var | Default | Purpose |
|---|---|---|
| `BYPASS_JITO_BUNDLE` | `false` | Enable floor-tip path for thin cycles |
| `JITO_BUNDLE_THRESHOLD` | `20` bps | Cycles at or below use floor tip; above use ratio tip |
| `COMPUTE_UNIT_PRICE_MICRO_LAMPORTS` | `1000` | CU priority fee; raise to `200_000`–`500_000` for better landing |

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
  `cargo run --release --bin momentum-sim -- run [--strategy ...]`. **Verdict (updated
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
  when full if `MOMENTUM_ROTATE_MARGIN>0`). Per-token `min_metric`/`trail_pct`/`max_run_pct`
  overrides in `momentum_tokens.json` apply per-slot. Startup adoption
  (`MOMENTUM_ADOPT_WALLET_POSITION`) generalizes to multi-slot at N>1 (adopts up to free
  capacity sorted by USD value desc; single-slot still warns on ambiguity). **Paper-test
  first** (`DRY_RUN_MOMENTUM_TRADER=true`, `MOMENTUM_MAX_POSITIONS>1`) before any live
  multi-slot run — single-slot is the validated edge.
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
  "dex": "raydium_amm_v4" | "raydium_clmm" | "orca_whirlpool" | "meteora_damm" | "dlmm" | "phoenix",
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

## DEX-specific notes

**Raydium AMM V4** — constant-product; reserves read from vault SPL token accounts (byte offset 64).

**Raydium CLMM** — `sqrt_price_x64` at offset 253, `tick_current` at offset 269, `tick_array_bitmap [u64; 16]` at offset 910 of the pool state account. `observation_key` at offset 201 (32 bytes). Tick array PDAs use big-endian `start_index.to_be_bytes()` as seed. `TICK_ARRAY_SIZE = 60`. The bitmap can lag on-chain state, so `swap_tick_arrays` falls back to repeating `start0` for all 3 slots when the bitmap is absent or stale — MEV swaps never cross tick array boundaries.

`swap_v2` account order: `[0]payer [1]amm_config [2]pool_state [3]input_acct [4]output_acct [5]input_vault [6]output_vault **[7]observation_state** [8]token_program [9]token_program_2022 [10]memo_program [11]input_mint [12]output_mint [13–15]tick_arrays`. Observation_state is at index 7 (before programs/mints), tick arrays are remaining_accounts.

**Orca Whirlpool** — `sqrt_price_x64` at offset 65, `tick_current_index` at offset 81. `TICK_ARRAY_SIZE = 88`. `tick_array_0/1/2` and `oracle` are required `extra` fields.

**Meteora DLMM** — does **not** enforce any token_x/token_y ordering when creating lb_pairs. `token_x_mint` is at lb_pair offset 88 and must be read at startup to determine orientation. Cached in `pool.dlmm_token_a_is_x` (1=token_a is X, 2=token_b is X) by `parse_state`. Do NOT use `pool.token_a < pool.token_b` to determine orientation — it is unreliable across pools.

**Meteora DAMM** — uses vault LP token balances and LP mint supply to compute virtual reserves. Subscribes to `a_vault_lp` / `b_vault_lp` accounts (via `lp_index`) in addition to vaults.

**Phoenix** — CLOB; price parsed from FIFOMarket account. `phoenix_base_lot_size` and `phoenix_quote_lot_size` required in `extra`. Real liquidity is typically thin — treat Phoenix cycles with caution.

**PumpSwap** — *pricing-only* venue (pump.fun AMM, CP with two SPL vaults). Exists so the portfolio-watcher's gRPC feed can price momentum tokens whose liquidity lives on pumpswap: `PoolRegistry::load` **skips** `dex:"pump_swap"` entries (the arb bot never builds edges through them; `build_swap_ix` bails as backstop), while the watcher subscribes their vaults and prices via the CP path. Pools come from `scripts/fetch_pumpswap_pools.js` (pinned `TARGET_POOLS`, on-chain layout decode with a mandatory vault↔mint cross-check).

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
