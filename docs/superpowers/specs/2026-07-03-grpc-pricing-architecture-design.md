# gRPC Pricing Architecture Upgrade — Design

**Date:** 2026-07-03
**Status:** approved in conversation (pending spec review)
**Owner:** momentum trader (`portfolio-watcher` binary)

## Goal

Make Yellowstone gRPC the primary, near-permanent price source for all watched momentum
tokens — eliminating the REST fallback for wired pools — and use live pool state to guard
entry execution. All changes are flag-gated or behavior-preserving by default.

## Findings that shaped this design (2026-07-03 exploration)

1. **No decoder gap exists.** All 8 watched tokens' pools are Meteora DLMM
   (`MET, BP, ARX, ORE`) or Orca Whirlpool (`JTO, SLX, PUMP, JUP`) — both supported by
   `spawn_grpc_feed` in `src/bin/portfolio_watcher.rs`. The `REST(wired)=[BP,SLX,ARX]`
   log line means those pools ARE wired but their price is older than
   `MOMENTUM_GRPC_STALE_SECS=90`.
2. **The stream never seeds.** Yellowstone delivers account *changes* only. A pool that
   hasn't traded since watcher boot has no gRPC price at all; a pool trading less often
   than the stale window flaps between gRPC and REST.
3. **An AMM price cannot move without an account write.** The last decoded state IS the
   pool's current price, indefinitely. Staleness is not data age — it is the risk that
   price discovery happens on venues we don't watch (or that our stream died).
4. `grpc_pricer.rs`'s module doc still claims "only raydium_amm_v4 supported" — stale.
5. ORE's pool `C7hF6MvQwErhsf1KrFvnKzdArb9PsofFiwZdipo9c7cz` was absent from
   `pools.json` (fixed operationally: added to `DLMM_PINNED`, `fetch_all.js`,
   `--init-alt`).

## Work items

### A. Seed pool state at startup and reconnect

- Factor the per-account update logic in `run_grpc_stream` (vault amount parse / CL state
  parse → pool atomics → `price_usd` → `feed.map` insert + `note_update`) into a helper
  used by both the stream loop and a new seeding step.
- In `spawn_grpc_feed` (and at the top of each `run_grpc_stream` cycle, i.e. on every
  reconnect), call `getMultipleAccounts` (chunked ≤100) for every key in `acct_index`,
  and run each result through that helper. Every wired pool then has a price from t=0,
  and reconnect gaps cannot leave silent holes.
- **SOL-quote bootstrap:** `feed.sol_usd()` may still be `0.0` when the seed runs
  (the watcher publishes it on its first poll). Seeded pool *state* is stored either way;
  a light retry in the ingestion task (every ~10 s, for wired pools with state but no
  `feed.map` entry) re-attempts `price_usd` until it succeeds. Self-heals without
  ordering constraints.

### B. Staleness → divergence semantics

- `MOMENTUM_GRPC_STALE_SECS=0` becomes a sentinel: **trust-until-changed** (no TTL).
  Any positive value keeps today's TTL behavior. Default stays `90` → unchanged unless
  opted in.
- New cross-check (active only in trust-until-changed mode):
  - `MOMENTUM_GRPC_XCHECK_SECS` (default `300`, `0` = off): per mint, at most every N
    seconds, fetch the REST price anyway and compare against the gRPC price.
  - `MOMENTUM_GRPC_XCHECK_BPS` (default `100`): if they diverge by more than this, mark
    the mint **distrusted** — it falls back to REST pricing until a fresh account write
    arrives or a later cross-check re-agrees. Log at `warn`.
- `select_prices` consults the distrust set; distrusted mints go to `to_rest` regardless
  of freshness. Covers the dead-stream case: a frozen gRPC price diverges from REST and
  auto-falls back.
- State lives on `GrpcFeed` (mint → last-xcheck `Instant`, distrust flag).

### C. Wire ORE (operational — done alongside this spec)

`DLMM_PINNED` += `C7hF6MvQwErhsf1KrFvnKzdArb9PsofFiwZdipo9c7cz` (ORE/USDC, binStep 50),
re-run `node scripts/fetch_all.js`, then `--init-alt`, then restart the watcher.
Side effect (accepted, precedented by MET/BP/ARX): the arb bot's graph gains ORE edges.

### D. Multi-pool per token

- `momentum_tokens.json` schema: optional `pools: [{ "pool": "...", "quote": "USDC|SOL" }]`
  array per token. Existing single `pool`+`quote` fields remain valid shorthand
  (parsed into a one-element list). No migration needed.
- `spawn_grpc_feed` wires one `WiredPool` per (token, pool); `acct_index` already
  supports it. Both pools write the same `feed.map` mint key — **last write wins**
  (freshest venue is by definition the latest price discovery).
- Seeding (A) covers all pools of a token.
- Cross-venue observability: at write time, if the new price differs from the previous
  map entry by more than `MOMENTUM_GRPC_XCHECK_BPS` and the previous entry is younger
  than 5 s, log at `info` (venues disagree). No behavior change in v1.

### E. Local impact pre-gate (scope-limited)

- Flag: `MOMENTUM_LOCAL_IMPACT` (default `false`).
- At entry, before the Jupiter `/quote` round-trip, if the candidate's wired pool state
  is available and the pool is **CP or Whirlpool** (kinds whose `dex::get_quote` works
  from subscribed state alone — DLMM needs bin arrays we don't subscribe, so it falls
  through), estimate the fill for `MOMENTUM_TRADE_USDC` and derive impact bps vs mid.
- **Pre-skip only when estimate > 2× `MOMENTUM_MAX_COST_BPS`** (conservative — the local
  model under-represents routing, so only obviously-doomed entries are skipped without a
  REST call). Everything else proceeds to Jupiter, which remains the execution source of
  truth.

### F. Entry price-freshness guard

- Flag: `MOMENTUM_ENTRY_DIVERGENCE_BPS` (default `0` = off).
- At entry (and rotation buy), after Jupiter `/quote` returns: implied price =
  `in_usdc / out_tokens`. If a trusted gRPC price exists for the mint and
  `|implied − grpc| / grpc` exceeds the threshold, skip this entry attempt and log.
  Protects against executing on a signal computed from a price that has since moved.
- Explicitly **not** event-driven entries: the ranking signal changes only when a 60 s
  snapshot lands, and the backtest is calibrated on that cadence. Entry evaluation
  cadence is unchanged.

### G. Housekeeping

- Fix `grpc_pricer.rs` module doc (claims raydium_amm_v4-only) and the
  `spawn_grpc_feed` docstring (same claim).
- Document the new env vars in the momentum trader doc and `.env.example`.

## Config summary

| Var | Default | Meaning |
|---|---|---|
| `MOMENTUM_GRPC_STALE_SECS` | `90` | TTL; **`0` = trust-until-changed** (new) |
| `MOMENTUM_GRPC_XCHECK_SECS` | `300` | REST cross-check cadence per mint (trust mode only) |
| `MOMENTUM_GRPC_XCHECK_BPS` | `100` | Divergence that distrusts a mint back to REST |
| `MOMENTUM_LOCAL_IMPACT` | `false` | Local impact pre-gate before Jupiter quote |
| `MOMENTUM_ENTRY_DIVERGENCE_BPS` | `0` | Skip entry when Jupiter fill diverges from gRPC price |

## Testing

- Unit: `select_prices` with TTL=0 + distrust set; seed helper parse-path reuse; universe
  parsing of `pools` array + single-pool shorthand; impact pre-gate and divergence guard
  as pure functions.
- Integration: `GRPC_PRICE_SMOKE=1` run must show **all 8** tokens priced within seconds
  of boot (seeding), not only recently-traded ones.
- Paper validation: run with `MOMENTUM_GRPC_STALE_SECS=0` + defaults for a few days;
  the `pricing gRPC=[…] REST(wired)=[…]` log line should show all wired tokens on gRPC
  every tick, with occasional `xcheck` lines and zero sustained distrusts.

## Rollout order

1. **Phase 1:** A + B + G (one coherent change — kills the REST fallback), C operational.
2. **Phase 2:** D (multi-pool schema + wiring).
3. **Phase 3:** F, then E.

## Out of scope

- Event-driven entry/signal cadence changes (backtest-calibrated at 60 s).
- New DEX-kind decoders (DAMM v2, pumpswap) — no watched pool needs them.
- Any arb-bot behavior change beyond pools.json gaining ORE entries.
