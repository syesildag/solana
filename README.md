# solana-mev

A real-time MEV arbitrage bot for Solana. It streams live DEX pool updates via Yellowstone gRPC, detects profitable cyclic trades using the Bellman-Ford algorithm on a log-weight token graph, and submits winning bundles atomically through the Jito Block Engine.

---

## How it works

```
Yellowstone gRPC stream
       │
       ▼  (pool vault / state account update)
  Pool Registry ──► Exchange Graph update
                         │
                         ▼  (on every tick)
                   Bellman-Ford
                   negative-cycle detection
                         │
                    cycle found?
                         │ yes
                         ▼
                   Opportunity Evaluator
                   (quote chain + fees + slippage + Jito tip)
                         │
                    profitable?
                         │ yes
                         ▼
                   RPC simulateTransaction
                         │
                    simulation pass?
                         │ yes
                         ▼
                   Jito Bundle (sign + encode)
                         │
                         ▼
                   Block Engine POST
```

### Arbitrage detection: negative-weight cycles

Every DEX pool is modelled as two directed edges in a weighted graph:

```
edge A→B  weight = −ln(rate_A_to_B)
edge B→A  weight = −ln(rate_B_to_A)
```

Because `ln` turns products into sums, a cycle `SOL → X → Y → SOL` is profitable when:

```
weight(SOL→X) + weight(X→Y) + weight(Y→SOL) < 0
⟺  −ln(r₁) − ln(r₂) − ln(r₃) < 0
⟺  ln(r₁ · r₂ · r₃) > 0
⟺  r₁ · r₂ · r₃ > 1   (gross profit)
```

Bellman-Ford detects exactly these negative-weight cycles in O(V·E) time, where V and E are bounded by the number of active tokens and pools being watched.

---

## Architecture

```
solana-mev/
├── Cargo.toml
├── .env.example
├── pools.json              ← you supply this (see Pool Config section)
└── src/
    ├── main.rs             ← entry point: wires stream → graph → arb → bundle
    ├── config.rs           ← reads all settings from .env
    │
    ├── streamer/
    │   ├── client.rs       ← GrpcStreamer: Tonic bidirectional gRPC stream
    │   └── subscription.rs ← builds SubscribeRequest filter for pool accounts
    │
    ├── graph/
    │   ├── exchange_graph.rs  ← live DashMap<(Mint,Mint), Edge> of log-rates
    │   └── bellman_ford.rs    ← negative-cycle detection → Vec<ArbCycle>
    │
    ├── dex/
    │   ├── types.rs        ← Pool, PoolState, SwapQuote, DexKind, PoolRegistry
    │   ├── raydium_amm.rs  ← AMM V4 quote (constant-product) + swap instruction
    │   ├── raydium_clmm.rs ← CLMM quote (sqrt-price) + swap_v2 instruction
    │   ├── orca.rs         ← Whirlpool state parser + swap instruction
    │   └── meteora.rs      ← DAMM quote + swap instruction
    │
    ├── arbitrage/
    │   ├── opportunity.rs  ← ArbOpportunity: amounts, fees, profit, instructions
    │   ├── evaluator.rs    ← chains quotes → computes net profit → filters
    │   └── simulator.rs    ← simulateTransaction check before committing
    │
    └── jito/
        ├── bundle.rs       ← builds + signs swap txs + tip tx into JitoBundle
        └── client.rs       ← POSTs bundle to Jito Block Engine REST API
```

---

## Supported DEX protocols

| Protocol | Kind | Fee | Program ID |
|---|---|---|---|
| Raydium AMM V4 | Constant-product | 25 bps | `675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp4` |
| Raydium CLMM | Concentrated liquidity | Per-pool (1–100 bps) | `CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK` |
| Orca Whirlpool | Concentrated liquidity | Per-pool (1–300 bps) | `whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc` |
| Meteora DAMM | Dynamic AMM | Dynamic | `Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EkAW7cP` |

**Raydium AMM V4** uses exact constant-product math:
```
amount_out = floor(reserve_out · amount_in · 9975 / (reserve_in · 10000 + amount_in · 9975))
```

**Raydium CLMM / Orca** use a single-tick approximation derived from `sqrt_price_x64`:
```
price      = (sqrt_price_x64 / 2⁶⁴)²
vr_a       = liquidity / sqrt_price
vr_b       = liquidity × sqrt_price
amount_out = vr_b · amount_in · (10000 − fee) / (vr_a · 10000 + amount_in · (10000 − fee))
```

---

## Pool reserves: how they're tracked

For **constant-product pools** (Raydium AMM V4, Meteora), reserves live in two SPL token vault accounts. The bot subscribes to those accounts via gRPC and reads:

```
reserve = u64 at byte offset 64 of the SPL token account data
```

For **concentrated liquidity pools** (Raydium CLMM, Orca), the bot subscribes to the pool state account and reads `sqrt_price_x64` and `fee_rate` from their documented byte offsets.

This means the pool config JSON must supply `vault_a`, `vault_b` for constant-product pools and `state_account` for CL pools.

---

## Profit calculation

Given a detected cycle `[SOL, X, Y, SOL]` and an input of `amount_in` lamports:

```
quote_1  = swap SOL → X  (amount_in)
quote_2  = swap X   → Y  (quote_1.amount_out)
quote_3  = swap Y   → SOL (quote_2.amount_out)

gross_out = quote_3.amount_out

costs:
  swap_fees  = sum of fee_amount across all 3 quotes
  tx_fee     = 4 × 5000 lamports  (3 swap txs + 1 tip tx)
  jito_tip   = clamp(gross_profit × TIP_RATIO, 1000, MAX_TIP_LAMPORTS)

net_profit = gross_out − amount_in − swap_fees − tx_fee − jito_tip
```

The opportunity is only pursued if `net_profit ≥ MIN_PROFIT_LAMPORTS`.

**Slippage guard**: each swap instruction encodes `minimum_amount_out = expected × (1 − SLIPPAGE_BPS / 10000)`. If the on-chain price moves beyond this, the transaction reverts rather than losing money.

---

## Jito tip

The tip scales with profit to remain competitive:

```
tip = gross_profit × TIP_RATIO      (default 50%)
tip = clamp(tip, 1_000, MAX_TIP_LAMPORTS)
```

The tip is paid as a SOL transfer to one of eight Jito-controlled tip accounts (chosen randomly per bundle):

```
96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5
HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe
Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY
ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49
DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh
ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt
DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL
3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT
```

---

## Bundle execution flow

```
1. Bellman-Ford detects negative cycle
2. Evaluator computes net_profit → creates ArbOpportunity with signed swap instructions
3. Fetch latest blockhash via RPC
4. Build JitoBundle (swap txs + tip tx, ≤ 5 txs total)
5. Call simulateTransaction on first swap tx
   └─ If simulation fails: discard, no SOL spent
6. POST bundle to https://mainnet.block-engine.jito.wtf/api/v1/bundles
   Method: sendBundle (JSON-RPC 2.0)
   Params: [["<base58_tx0>", "<base58_tx1>", "<base58_tx2>", "<base58_tx3>"]]
7. Log bundle UUID and net profit
```

Steps 3–7 run in a spawned Tokio task so the gRPC stream is never blocked.

---

## Setup

### 1. Prerequisites

- Rust 1.75+
- A Yellowstone gRPC endpoint (e.g. from [Helius](https://helius.dev), [Triton](https://triton.one), or self-hosted)
- A funded Solana wallet keypair
- A Solana RPC endpoint

### 2. Configuration

Copy `.env.example` to `.env` and fill in your values:

```env
GRPC_ENDPOINT=https://your-yellowstone-endpoint:443
GRPC_TOKEN=your-x-token-here
WALLET_KEYPAIR_PATH=/Users/you/.config/solana/id.json
RPC_URL=https://api.mainnet-beta.solana.com

POOLS_CONFIG_PATH=pools.json

MIN_PROFIT_LAMPORTS=10000       # minimum net profit to pursue (0.00001 SOL)
INPUT_SOL_LAMPORTS=100000000    # input per arb attempt (0.1 SOL)
SLIPPAGE_BPS=50                 # 0.5% slippage tolerance

TIP_RATIO=0.5                   # 50% of gross profit goes to Jito tip
MAX_TIP_LAMPORTS=1000000        # cap tip at 0.001 SOL

DRY_RUN=false                   # true = log bundles without submitting
RUST_LOG=solana_mev=info
```

### 3. Pool config (`pools.json`)

Create a JSON array of pools to watch. Each pool needs vault pubkeys (for AMM) or a state account (for CL pools):

```json
[
  {
    "id": "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2",
    "dex": "raydium_amm_v4",
    "token_a": "So11111111111111111111111111111111111111112",
    "token_b": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    "vault_a": "DQyrAcCrDXQ7NeoqGgDCZwBvWDcYmFCjSb9JtteuvPpz",
    "vault_b": "HLmqeL62xR1QoZ1HKKbXRrdN1p3phKpxRMb2VVopvBBz",
    "fee_bps": 25,
    "extra": {
      "amm_authority": "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1",
      "open_orders": "...",
      "market": "...",
      "..."
    }
  },
  {
    "id": "...",
    "dex": "orca_whirlpool",
    "token_a": "So11111111111111111111111111111111111111112",
    "token_b": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    "vault_a": "...",
    "vault_b": "...",
    "state_account": "...",
    "fee_bps": 30,
    "extra": {
      "tick_array_0": "...",
      "tick_array_1": "...",
      "tick_array_2": "...",
      "oracle": "..."
    }
  }
]
```

Pool accounts (vaults, market, tick arrays) can be fetched from:
- **Raydium AMM V4**: [Raydium pool API](https://api.raydium.io/v2/sdk/liquidity/mainnet.json)
- **Orca Whirlpools**: [Orca API](https://api.mainnet.orca.so/v1/whirlpool/list)
- **Raydium CLMM**: [Raydium CLMM API](https://api.raydium.io/v2/ammV3/ammPools)
- **Meteora**: [Meteora API](https://dlmm-api.meteora.ag/pair/all)

### 4. Build and run

```bash
cargo build --release
./target/release/solana-mev
```

Dry-run mode (no bundles submitted, safe for testing):
```bash
DRY_RUN=true cargo run
```

---

## User contribution: input amount optimization

The file [src/arbitrage/evaluator.rs](src/arbitrage/evaluator.rs) contains a `TODO` for the `optimize_input_and_tip()` function. The current implementation uses a fixed `INPUT_SOL_LAMPORTS` for every cycle. A better approach is to search for the amount that maximizes net profit:

```rust
pub fn optimize_input_and_tip(
    cycle: &ArbCycle,
    registry: &PoolRegistry,
    config: &Config,
    user: Pubkey,
    available_sol: u64,
) -> Option<ArbOpportunity> {
    // Example: try 25%, 50%, 75%, 100% of available capital
    // and pick the amount with the highest net_profit_lamports
    let candidates = [0.25, 0.5, 0.75, 1.0];
    candidates
        .iter()
        .filter_map(|&frac| {
            let amount = (available_sol as f64 * frac) as u64;
            optimize_and_evaluate(cycle, registry, config, user, amount)
        })
        .max_by_key(|opp| opp.net_profit_lamports)
}
```

A more precise approach is binary search over `amount_in` — increasing input increases gross output, but also increases price impact and slippage cost. The optimal point is where the marginal gain equals the marginal cost.

---

## Dependencies

| Crate | Version | Purpose |
|---|---|---|
| `tokio` | 1 | Async runtime |
| `tonic` | 0.12 | gRPC client (Yellowstone stream) |
| `yellowstone-grpc-proto` | 5 | Geyser protobuf types |
| `solana-sdk` | 2 | Transaction building, keypair, pubkey |
| `solana-client` | 2 | RPC client (simulate, blockhash) |
| `dashmap` | 6 | Lock-free concurrent HashMap for graph |
| `reqwest` | 0.12 | HTTP client for Jito REST API (native-tls) |
| `serde_json` | 1 | Pool config + Jito JSON-RPC |
| `bincode` | 1 | Transaction serialization for bundle encoding |
| `spl-token` | 6 | SPL token program ID |
| `spl-token-2022` | 4 | Token-2022 program ID (CLMM) |
| `rand` | 0.8 | Random tip account selection |

> **Note on TLS**: `reqwest` uses `native-tls` (not `rustls-tls`). Using `rustls-tls` here causes a `zeroize` version conflict with the `ed25519-dalek` version pulled in by `yellowstone-grpc-proto v5` via `solana-sdk`. Do not switch this without resolving that conflict first.

---

## Limitations and next steps

- **Tick array accounts for CLMM/Orca** must be pre-computed and stored in `pools.json`. They depend on the current tick index and change as price moves. Production bots derive them dynamically from the pool state at execution time.
- **User ATA resolution** in [jito/bundle.rs](src/jito/bundle.rs) uses `Pubkey::default()` as a placeholder. Replace with `spl_associated_token_account::get_associated_token_address(&user, &mint)` for real execution.
- **`optimize_input_and_tip()`** in [arbitrage/evaluator.rs](src/arbitrage/evaluator.rs) uses a fixed input amount. Implement binary search for optimal sizing.
- **Reconnection logic**: the gRPC stream does not auto-reconnect on disconnect. A production setup should wrap `GrpcStreamer::start()` in a retry loop.
- **Multi-pool per pair**: the `find_pool()` method returns the first matching pool. A production bot should evaluate all pools for a token pair and pick the best quote.
