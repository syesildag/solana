# Long-Tail MEV Expansion — Design Spec

## Context

The bot currently submits 2–3 bps arb bundles against major SOL/USDC/USDT pools and gets dropped ~80% of the time because it's competing on latency against well-funded bots with co-located infrastructure. The solution is to target opportunities where being 100 ms late doesn't instantly lose: niche token pairs (LSTs, meme coins), stablecoin depegs, and DEXes where large bots have no coverage.

## Approach: Incremental (A)

Three independent phases, each shippable on its own:

1. **Phase 1 — Curve StableSwap Invariant**: Fix the broken stable-pool math so the 3 existing Meteora DAMM stable pools (SOL/mSOL, USDC/USDT, USDT/USDC) enter the Bellman-Ford graph.
2. **Phase 2 — Pool Expansion**: Add ~30 LST, meme-coin, and stablecoin pool entries to `pools.json` for the 6 already-supported DEXes (zero code changes).
3. **Phase 3 — New DEX Integrations**: Add Lifinity (oracle-lag AMM), Invariant (CLMM), and Saber (StableSwap).

---

## Phase 1: Curve StableSwap Invariant

### Problem

`exchange_graph.rs:52` hard-returns on every `pool.stable` pool:

```rust
if pool.stable {
    return;
}
```

The constant-product formula gives rates ~2× wrong on near-peg pools (USDC/USDT), making them useless for arbitrage detection. The stable pools are already subscribed and updating live; only the math is missing.

### Three existing stable pools (already in pools.json)

| Pool ID | Pair | Amp |
|---------|------|-----|
| `HcjZvfeSNJbNkfLD4eEcRBr96AD3w1GpmMppaeRZf7ur` | SOL / mSOL | 100 |
| `32D4zRxNc1EssbJieVHfPhZM3rH6CzfUPrWUuWxD9prG` | USDC / USDT | 100 |
| `EMyXvKEi9izVMMsJPaSx8SZzoW69brf9MDPMEbwKDCvF` | USDT / USDC | 100 |

### Curve invariant (n=2)

```
4A(x + y) + D = 4AD + D³ / (4xy)
```

`D` is computed via Newton's method. `A` is the amplification coefficient (100 for all three pools). The invariant produces near-1:1 rates at peg and amplifies depeg arbs.

### Files changed

| File | Change |
|------|--------|
| `src/dex/stable_math.rs` | **New** — `compute_d`, `compute_y`, `get_amount_out`, `marginal_rate` |
| `src/dex/meteora.rs` | Branch in `get_quote`: if `pool.stable` → `stable_math::get_amount_out` |
| `src/graph/exchange_graph.rs` | Remove `if pool.stable { return; }` guard; add stable rate path |
| `src/dex/types.rs` | Add `damm_amp: Option<u64>` to `PoolExtra` + `ExtraConfig` |
| `pools.json` | Add `"damm_amp": 100` to the 3 stable pool `extra` objects |

### Verification

- Unit tests: `compute_d(50_000_000, 50_000_000, 100)` should equal `~100_000_000`
- Unit test: `get_amount_out(1_000_000, 50_000_000, 50_000_000, 100, 5)` should be `≈ 999_495` (1:1 minus 0.05% fee)
- Integration: `DRY_RUN=true cargo run` → `log_rates` shows `USDC -[Meteora]→ USDT rate≈0.9995`
- `edge_count_by_dex` in BF window log: `damm` count rises from ~16 to ~22

---

## Phase 2: Pool Expansion

### Target pairs

| Category | Pairs | DEX |
|----------|-------|-----|
| LSTs | jitoSOL/SOL, bSOL/SOL, stSOL/SOL | Orca Whirlpool, Raydium CLMM |
| Meme coins | BONK/SOL, WIF/SOL, POPCAT/SOL, JUP/USDC | Raydium AMM V4, Orca |
| Stablecoins | USDY/USDC | Meteora DAMM stable |

### Code changes

- `src/dex/types.rs:mint_symbol()` — add symbol entries for jitoSOL, bSOL, stSOL, hSOL, BONK, WIF, JUP, POPCAT, USDY

### Verification

- `DRY_RUN=true cargo run` → `log_rates` shows new token pairs
- All new edges have rates within 2% of current market price

---

## Phase 3: New DEX Integrations

### Candidates

| DEX | Why long-tail | Complexity |
|-----|--------------|-----------|
| Lifinity | Oracle-anchored AMM — arb windows last seconds, not ms | Medium (new account layout) |
| Invariant | CLMM with niche pairs — same sqrt_price_x64 layout as Orca | Low (reuse CLMM path) |
| Saber | Curve stable AMM — reuses Phase 1 stable_math | Low-medium |

### Per-DEX requirements

Each requires: `src/dex/<name>.rs` + `DexKind` variant + `check_extra` arm + subscription wiring.

### Verification

Per-DEX: add one test pool entry, run `get_quote` against a known reserve snapshot, compare output to on-chain simulation.
