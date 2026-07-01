# Momentum gRPC pricing — Phase 1: CL-state pools (Orca + Raydium CLMM + Meteora DLMM)

**Date:** 2026-07-01
**Status:** Approved (design).
**Scope:** Extend the opt-in gRPC price feed (shipped, constant-product/raydium_amm_v4 only) to
concentrated-liquidity pools priced from their **state account's sqrt-price**: Orca Whirlpool,
Raydium CLMM, Meteora DLMM, Invariant. Reuses the arb's real `dex::Pool` + parsers (Approach A;
no arb-path changes). **Meteora DAMM (virtual reserves) is Phase 2**, out of scope here. PUMP
(pumpswap) has no parser and is unsupported. Additive, default-off.

## Problem

The gRPC price feed (`MOMENTUM_GRPC_PRICING`, default off) currently prices only `raydium_amm_v4`
(constant-product, two-vault) pools. The momentum watch-list tokens don't have active raydium_amm_v4
pools — JTO/SLX trade on **Orca** (concentrated liquidity), MET/BP/JUP/ARX on **Meteora**. So no
watch-list token can be gRPC-priced today; they all fall back to REST (rate-limited/quota-capped).

Concentrated-liquidity pools must NOT be priced from raw vault balances: near/outside the active
range one vault can hold almost everything, so a constant-product formula on vault reserves yields
wildly wrong "phantom" prices (this is exactly why the arb's `exchange_graph` uses the state-account
`sqrt_price` for these pools, not vault reserves). Correct CL pricing reads the pool **state
account**.

## Goal

Price Orca / Raydium CLMM / Meteora DLMM / Invariant momentum pools from their on-chain state
account over gRPC, converted to USD, feeding the existing `GrpcFeed` map (watcher prefers fresh
gRPC, REST-fills the rest — unchanged). Then wire JTO + SLX (Orca) as the first real watch-list
tokens on gRPC.

Non-goals: Meteora DAMM (Phase 2 — virtual LP reserves); Phoenix/Lifinity; pumpswap; any change to
momentum decision logic, the arb path, or the default-off safety.

## Approach (A — reuse `dex::Pool` + the arb's parsers, mirrored in the watcher)

Build a real `dex::Pool` per wired momentum pool and drive its atomics from streamed account
updates using the **same public `dex` parse functions the arb uses**, then read the price. No changes
to `src/arbitrage/`, `src/graph/`, `src/streamer/`, or the arb binary — the watcher binary already
`#[path]`-includes `dex`/`graph`.

### Pricing mechanisms, routed by `DexKind`

| DexKind | Accounts subscribed | Parse → store | Price source |
|---|---|---|---|
| RaydiumAmmV4, Saber | `vault_a`, `vault_b` | `parse_spl_token_amount` → `reserve_a/reserve_b` | `snapshot_state()` (ConstantProduct) |
| OrcaWhirlpool, RaydiumClmm, MeteoraDlmm, Invariant | `state_account` | `parse_cl_pool_state(data, &pool)` → `sqrt_price_x64` (f64 bits) + `fee_bps` | that `price` (token_b per token_a, **raw units**) |
| MeteoraDamm, Phoenix, Lifinity, unknown | — | — | **skip + log → REST fallback** (DAMM = Phase 2) |

`parse_cl_pool_state` requires `&Pool` (it reads `pool.extra`, caches tick data into pool atomics),
which is why we build a real `dex::Pool` rather than the ad-hoc `TrackedCp` struct — the CP path is
migrated onto `dex::Pool` too, unifying both paths and deleting `TrackedCp`.

### Shared rate→USD core

`parse_cl_pool_state`'s `price` is token_b per token_a in **raw (atomic) units** — the same basis as
`PoolState::rate_a_to_b()`. So both paths share one conversion (factored out of the current
`price_usd`):

```
rate_to_usd(raw_rate_quote_per_momentum, dec_momentum, dec_quote, quote_is_usdc, sol_usd):
    human = raw_rate * 10^(dec_momentum - dec_quote)
    usd   = if quote_is_usdc { human } else { human * sol_usd }
    (None on non-finite / <= 0)
```

- **CP path** feeds it `snapshot_state().rate_a_to_b()` (momentum=token_a) or `rate_b_to_a()` (token_b).
- **CL path** feeds it `price` (momentum=token_a) or `1.0 / price` (momentum=token_b).

`price_usd` (existing, unit-tested) is refactored to delegate to `rate_to_usd`, so its tests keep
passing unchanged.

## Components / files

- **Modify** `src/portfolio/grpc_pricer.rs` (lib): factor `rate_to_usd` out of `price_usd` (pure,
  add CL-orientation tests). No dependency on `dex` (stays lib-clean).
- **Modify** `src/bin/portfolio_watcher.rs`: replace `TrackedCp` with `Arc<dex::types::Pool>` built
  via `Pool::try_from`; route subscription accounts + parsing by `DexKind` (CP-vaults vs CL-state);
  compute price via `snapshot_state`/`rate_to_usd` (CP) or `parse_cl_pool_state` + `rate_to_usd` (CL);
  extend the account→pool index to key on vault **and** state pubkeys. Keep the default-off gate,
  reconnect/backoff, decimals fetch, and REST-fallback-on-skip exactly as they are.
- **Modify** `.env.example`: note the newly supported DEX kinds (Orca/CLMM/DLMM) for
  `MOMENTUM_GRPC_PRICING`.
- **Operational (own plan task)**: add JTO + SLX active Orca pools to `pools.json` via
  `scripts/fetch_orca_pools.js` + `scripts/merge_pools.js` (never hand-edit pools.json), then wire
  with `add_momentum_token.js <mint> --pool <id> --quote <USDC|SOL>`.

## Error handling / degradation (unchanged safety model)

- Default-off: `MOMENTUM_GRPC_PRICING=false` → `spawn_grpc_feed` returns `None` → REST-only, byte-identical.
- Unsupported DEX kind, missing pool in pools.json, `Pool::try_from` failure, or `parse_*` returning
  `None` → that token is skipped (logged) → REST prices it. No crash.
- CL state not yet seen (sqrt_price_x64 == 0) → no price emitted → REST-filled until the first update.
- Stream disconnect → reconnect w/ capped backoff; map ages out → REST fallback via `select_prices`.

## Testing

- **`rate_to_usd` (pure):** CP + CL rates, momentum=token_a and token_b, USDC and SOL quote, decimal
  scaling, degenerate → None. The existing `price_usd` tests continue to pass through the refactor.
- **CL orientation:** given a `price` (raw b/a), assert USD for momentum=token_a (`price`) and
  momentum=token_b (`1/price`) with correct decimals.
- **Live smoke (`GRPC_PRICE_SMOKE=1`):** verified against an **existing** Orca or Raydium-CLMM pool
  already in `pools.json` (e.g. a SOL/USDC whirlpool) — the derived USD price must match DexScreener
  within a few percent. This is the unit/orientation safety net; it cannot run in CI (needs the live
  endpoint), so it is an operator/'`GRPC_PRICE_SMOKE`' step, documented.

## Phase 2 (documented, not built here)

Meteora DAMM: subscribe `vault_a`/`vault_b` (as `parse_meteora_vault_amount`), `a_vault_lp`/`b_vault_lp`
(`parse_spl_token_amount`), and the vault LP mints (`parse_spl_mint_supply`); compute virtual reserves
`reserve = total × lp_bal / lp_supply` (as `main.rs` does at startup), plus the stable-pool
`virtual_price` for stable DAMM. Then `snapshot_state()` (ConstantProduct) → `rate_to_usd`. Covers
MET/BP/JUP/ARX if their active pools are DAMM.
