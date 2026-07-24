---
name: add-momentum-token
description: >
  Quick-add a token to the momentum trader's curated watch list by mint address —
  WITHOUT vetting — and fully wire it: resolve its best pool, add it to
  assets/momentum_tokens.json, pin the pool in the DEX fetcher, and regenerate
  pools.json so the gRPC pricer can stream it. Use this whenever the user gives a
  mint (or ticker) and says "add it to the curated list", "add this token", "watch
  this token", "add X with its pool", "add it and tune it" — especially with
  "don't vet" / "no validation". For a full statistical vet + conditional add, use
  vet-momentum-token instead; this skill is the fast unconditional path.
---

# Add Momentum Token (quick add, no vetting)

Add a token to the curated momentum watch list AND wire its pool AND backfill its
price history AND tune its per-token params — end-to-end. A list-only add leaves
the token REST-priced (slower ranking, trailing stop exposed to REST rate limits);
a wired-but-untuned add leaves it on the GLOBAL bar, which is denominated for the
whole universe, not this token. Add + wire + backfill + params are ONE operation
(user rule 2026-07-21, extended 2026-07-24 after MANIFEST/TripleT/Jimothy shipped
untuned and needed a separately-commissioned optimization pass). Only skip steps
5-7 if the user explicitly says "skip the backtest/params" — then say the token
runs on the global config until tuned.

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

### 5. Backfill the token's price history into the combined file

Report the add/wiring to the user first, then run this (background — GT throttles
to ~30 req/min; an old active pool takes 20-40 min, a day-0 token ~2 min):

```bash
node scripts/backfill_history.js --days 150 --no-splice \
  --output <scratchpad>/gt_<SYMBOL>.jsonl --tokens "<MINT>::<POOL>"
```

**Always pin the pool wired in step 3** (`MINT::POOL` — empty key slot): the
volume-ranked auto-pick can choose a young pool and silently produce a file with
no history head. Then ts-union-merge into the current combined history
(`assets/price_history.curated150.jsonl` or whatever HISTORY file the last
optimization used): rows merge by timestamp (`prices` dicts unioned), clamped to
the combined file's span. Acceptance gate before tuning: `grep -c <MINT>` on the
merged file, and note the first-row date.

### 6. Tune its per-token params (isolation sweep)

Metric/lookback are GLOBAL — read them (and the global min/z) from `.env`, then
sweep ONLY this token's overridable knobs with `momentum-sim per-token`:
`min_metric` ∈ {½×, 1×, 2× global} × trail {20, 30} × z {global, off} (~12 runs,
each prints a per-token train/test row):

```bash
HISTORY_PATH=<merged file> HISTORY_MAX_SNAPSHOTS=300000 ./target/release/momentum-sim \
  per-token --tokens <params-stripped tokens copy> --metric <global> --lookback <global> \
  --min-metric <X> --trail <T> --max-run 0 --regime-mode off --regime-obs 0 \
  --entry-max-z-obs <480|0> --entry-max-z <Z> --trade-usdc 100
```

Use a **params-stripped copy** of the tokens file (existing overrides contaminate
the sweep) and split z pairs like `"480:1.0"` with `${Z%%:*}`/`${Z##*:}` (zsh does
not word-split `$VAR`). Verdict rules (same as the optimize-momentum-config skill):

- **Token's data starts inside the test slice** (younger than ~6 weeks on a 150d
  file): **write NO params** — tuning one week of data is curve-fitting; it runs
  on the global bar until the next full re-optimization.
- **Negative in every sweep config**: write the least-bad HIGH bar and tell the
  user plainly that evidence says watch-only.
- **Robust in ≥1 config** (positive both slices): write the worst-slice-best
  `min_metric` (+ `trail_pct` only if ≠ global; + `entry_max_z_obs: 0` only if
  z-off beats the global z in BOTH slices).

### 7. Write the params into momentum_tokens.json

Update ONLY this token's entry — add/replace its `params` block, preserving
`pool`/`quote`/`name`. Verify: `jq '.[] | select(.mint=="<MINT>")' assets/momentum_tokens.json`.

### 8. Report and remind

Tell the user, concretely:

- Token identity, pool, liquidity, 24h volume, age (from step 1).
- The sweep verdict and the exact `params` written (or why none were — test-only
  data / watch-only evidence), with the winning row's train/test P&L and trades.
- The watcher subscribes vaults **at startup** — a restart is needed before the
  new pool actually streams over gRPC; until then the token is REST-priced.
- This path adds **unvetted** — no liquidity floor, no wash-trading screen, and
  the params step is tuning, not a go/no-go gate. If liquidity is thin (< ~$200k)
  or the token is mature (> ~1 month old, rarely moves ±25%/4h), say so: thin
  pools gap through trailing stops, and mature tokens may never clear a
  launch-phase entry bar.

## Failure modes to watch

- **No DexScreener pairs**: token too new or wrong mint — confirm with the user.
- **Main pool not on pumpswap/known DEX**: wire the best *supported* venue's pool
  and note the discrepancy (pricing follows the wired pool, not the main venue).
- **Duplicate add**: the helper dedups by mint; a re-add with `--pool` updates the
  pool in place — that's fine and useful when a token's liquidity migrates.
- **Global metric/lookback changed since the last full optimization**: per-token
  `min_metric` is denominated in the GLOBAL metric's units — do NOT tune this
  token against a `.env` that other tokens' params don't match; run the full
  optimize-momentum-config procedure instead (its multi-slot + per-token section).
- **Sweep totals all ~0 trades**: the merged history probably lacks the token
  (failed merge or wrong mint key) — re-check the step-5 acceptance gate before
  concluding anything about the token.
