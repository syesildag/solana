---
name: add-momentum-token
description: >
  Quick-add a token to the momentum trader's curated watch list by mint address —
  WITHOUT vetting — and fully wire it: resolve its best pool, add it to
  assets/momentum_tokens.json, pin the pool in the DEX fetcher, and regenerate
  pools.json so the gRPC pricer can stream it. Use this whenever the user gives a
  mint (or ticker) and says "add it to the curated list", "add this token", "watch
  this token", "add X with its pool" — especially with "don't vet" / "no
  validation". For a full statistical vet + conditional add, use vet-momentum-token
  instead; this skill is the fast unconditional path.
---

# Add Momentum Token (quick add, no vetting)

Add a token to the curated momentum watch list AND wire its pool end-to-end. A
list-only add leaves the token REST-priced — slower ranking, and a trailing stop
exposed to REST rate limits if the token is ever held. Adding the token and wiring
its pool are ONE operation, never two (user rule, 2026-07-21).

## Inputs

- **mint** (required): base-58 mint address. A ticker/name is acceptable —
  `add_momentum_token.js` resolves it via Jupiter verified search — but prefer the
  mint when given.
- The user may name a specific pool; otherwise discover it (step 1).

## Steps

### 1. Identify the token and its main pool

```bash
curl -s "https://api.dexscreener.com/latest/dex/tokens/<MINT>"
```

From `pairs`, pick the pool with the **highest `volume.h24`** — NOT the highest
liquidity (fake-TVL pools exist; volume is the honest signal). Record: `dexId`,
`pairAddress`, symbol, name, quote token, liquidity, 24h volume, `pairCreatedAt`.

Report these to the user before writing anything — the identity check ("that mint
is CUBEMAN, PumpSwap pool 4z3ZkJik…, $276k liq") catches wrong-mint mistakes early.

### 2. Add to the curated list

Use the existing helper (dedups by mint, updates pool in place if re-adding):

```bash
node scripts/add_momentum_token.js <MINT> [SYMBOL] --pool <POOL> --quote <SOL|USDC>
```

`--quote` is the pool's quote side from step 1 (usually SOL for pump.fun tokens).
If the helper refuses (unverified token) pass the raw mint — mint input skips the
verified-only gate. Verify the entry landed:

```bash
jq -r '.[] | select(.mint=="<MINT>")' assets/momentum_tokens.json
```

### 3. Pin the pool in the matching fetcher

The gRPC pricer only subscribes pools present in pools.json, and pools.json is
GENERATED — never edit it directly; pin in the fetcher and regenerate.

- `dexId == "pumpswap"` (the usual case): add the pool address to `TARGET_POOLS`
  in `scripts/fetch_pumpswap_pools.js`, with a dated comment noting symbol,
  launch date, and that it was added unvetted.
- `dexId == "meteora"` (DLMM): add to `DLMM_PINNED` in
  `scripts/fetch_meteora_dlmm.js` (auto-discovery misses liquid pools).
- Raydium / Orca: the respective fetchers discover by liquidity; check whether the
  pool already appears after step 4 and pin only if missing.

### 4. Regenerate pools.json and verify

```bash
node scripts/fetch_pumpswap_pools.js   # or the fetcher edited in step 3
node scripts/merge_pools.js
grep -c "<POOL>" pools.json            # must be ≥ 1
```

The pumpswap fetcher decodes the on-chain layout with a vault↔mint cross-check —
if it prints an error for the pool, stop and investigate; do not force-merge.

### 5. Report and remind

Tell the user, concretely:

- Token identity, pool, liquidity, 24h volume, age (from step 1).
- The watcher subscribes vaults **at startup** — a restart is needed before the
  new pool actually streams over gRPC; until then the token is REST-priced.
- The token has **no backtest history** until `scripts/build_pump_history.js
  --days 14` is re-run (only do this if the user wants a simulation — it takes
  ~10 min of GeckoTerminal paging for the whole list).
- This path adds **unvetted** — no liquidity floor, no wash-trading screen, no
  backtest gate. If liquidity is thin (< ~$200k) or the token is mature (> ~1
  month old, rarely moves ±25%/4h), say so: thin pools gap through trailing
  stops, and mature tokens may never clear a launch-phase entry bar.

## Failure modes to watch

- **No DexScreener pairs**: token too new or wrong mint — confirm with the user.
- **Main pool not on pumpswap/known DEX**: wire the best *supported* venue's pool
  and note the discrepancy (pricing follows the wired pool, not the main venue).
- **Duplicate add**: the helper dedups by mint; a re-add with `--pool` updates the
  pool in place — that's fine and useful when a token's liquidity migrates.
