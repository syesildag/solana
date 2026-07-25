# Dynamic Arb Pool Discovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Periodically re-scan for trending, security-screened tokens that form executable arb cycles, fit them into the gRPC feed's account budget, rewrite `pools.json`, and have the running bot reload itself via SIGHUP.

**Architecture:** Four pure JS library modules (safety, venues, budget, plus exported reuse from `reduce_pools.js`) composed by one orchestrator script `scripts/scan_arb_pools.js`; a thin shell trigger loop; and one self-contained Rust addition — a SIGHUP handler that drains in-flight submissions then `exec()`s the same binary (same PID, same terminal). No changes to the arb hot loop.

**Tech Stack:** Node 25 (`node:test` + `node:assert`, CommonJS, no external deps), Rust (tokio `signal::unix`, `std::os::unix::process::CommandExt`), Solana JSON-RPC, Birdeye + DexScreener REST.

**Spec:** `docs/superpowers/specs/2026-07-25-dynamic-arb-pool-discovery-design.md`

## Global Constraints

- **Node scripts:** CommonJS (`require`), `"use strict"`, **no new npm dependencies** (repo has no `package.json` deps; use `node:https`/`node:http`/`node:fs`).
- **Tests:** `node:test` + `node:assert`, colocated as `scripts/<name>.test.js`, testing **exported pure functions only** (no network in tests). Run with `node --test scripts/`.
- **Rust tests:** `#[cfg(test)] mod tests` at the **bottom of the same file**. Run `cargo test --bin solana-mev`.
- **NEVER run `cargo fmt` or `rustfmt`** on whole files — the repo is not rustfmt-clean and it causes massive diff churn.
- **`pools.json` is auto-generated** — only ever written via the atomic validated path in Task 6; never hand-edited.
- **Hubs:** `SOL = So11111111111111111111111111111111111111112`, `USDC = EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v`.
- **Account budget default:** `ARB_ACCOUNT_BUDGET=200`; **drain default:** `ARB_REEXEC_DRAIN_SECS=30`; **evict margin:** `ARB_SCAN_EVICT_MARGIN=1.25`; **activity window:** `ARB_ACTIVITY_WINDOW_SECS=300`.
- **Exit-status contract** for `scan_arb_pools.js`: `0` = book changed and written, `10` = no change, any other non-zero = failure.
- Commit after every task.

## File Structure

| File | Responsibility |
|---|---|
| `scripts/reduce_pools.js` (modify) | Export + correct the two reused primitives (`pruneToCycles`, `countAccounts`) |
| `scripts/lib/token_safety.js` (create) | Arb-specific mint screening: freeze authority, Token-2022 transfer hook |
| `scripts/lib/venues.js` (create) | DexScreener pairs → per-venue candidate pools; tradeable-venue counting |
| `scripts/lib/book_budget.js` (create) | Protected-core classification, activity-ranked selection, hysteresis, account cap |
| `scripts/fetch_*.js` (modify) | `--pools <addr,…>` override so a specific address can be decoded on demand |
| `scripts/scan_arb_pools.js` (create) | Orchestrator: discover → screen → resolve → close → budget → decode → atomic write |
| `scripts/arb_refresh_loop.sh` (create) | Timer: scan `--apply` → `--init-alt` → `kill -HUP` |
| `src/main.rs` (modify) | SIGHUP flag + `should_reexec` predicate + `exec()` self-restart |
| `CLAUDE.md`, `.env.example` (modify) | Document the subsystem and its env knobs |

---

### Task 1: Export and correct the reused primitives in `reduce_pools.js`

Two real bugs for our use case: `countAccounts` skips `pump_swap` (correct when PumpSwap was pricing-only, **wrong now that it is tradeable** — those vaults are subscribed and must count against the budget), and `pruneToCycles` seeds hub-connectivity from **USDC only** (the bot's base is SOL, so a SOL-only component would be wrongly discarded).

**Files:**
- Modify: `scripts/reduce_pools.js` (add `module.exports`; fix `countAccounts`; fix `pruneToCycles` seeding)
- Test: `scripts/reduce_pools.test.js` (create)

**Interfaces:**
- Consumes: nothing (first task).
- Produces:
  - `pruneToCycles(pools: PoolCfg[]) -> PoolCfg[]` — drops non-hub tokens with <2 venues to fixpoint, then keeps only the component reachable from **either** hub. Each input pool must carry a numeric `_act` (activity score) used to choose which offender to drop.
  - `countAccounts(pools: PoolCfg[], opts?: {countPumpSwap?: boolean}) -> number` — distinct subscribed accounts. `countPumpSwap: true` includes `pump_swap` vaults.
  - `HUBS: Set<string>`, `USDC: string`, `SOL: string`.

- [ ] **Step 1: Write the failing tests**

Create `scripts/reduce_pools.test.js`:

```js
"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { pruneToCycles, countAccounts, HUBS } = require("./reduce_pools");

const SOL  = "So11111111111111111111111111111111111111112";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const AAA  = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const BBB  = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

// Minimal pool config: only the fields the two functions read.
const pool = (id, token_a, token_b, act, extra = {}) => ({
  id, token_a, token_b, _act: act, dex: extra.dex || "raydium_amm_v4",
  vault_a: id + "-va", vault_b: id + "-vb", ...extra,
});

test("pruneToCycles drops a non-hub token with only one venue", () => {
  const pools = [pool("p1", SOL, AAA, 10)];            // AAA degree 1 → dead end
  assert.deepEqual(pruneToCycles(pools), []);
});

test("pruneToCycles keeps a non-hub token with two venues", () => {
  const pools = [pool("p1", SOL, AAA, 10), pool("p2", SOL, AAA, 9)];
  assert.equal(pruneToCycles(pools).length, 2);
});

test("pruneToCycles cascades: removing one pool can orphan another", () => {
  // AAA has 2 venues, but BBB has 1. Dropping BBB's pool leaves AAA with 1 → both go.
  const pools = [pool("p1", SOL, AAA, 10), pool("p2", AAA, BBB, 9)];
  assert.deepEqual(pruneToCycles(pools), []);
});

test("pruneToCycles keeps a SOL-only component (base is SOL, not USDC)", () => {
  // No USDC anywhere: seeding connectivity from USDC alone would wrongly drop everything.
  const pools = [pool("p1", SOL, AAA, 10), pool("p2", SOL, AAA, 9)];
  const kept = pruneToCycles(pools);
  assert.equal(kept.length, 2, "SOL-connected component must survive");
});

test("countAccounts counts pump_swap vaults when asked (tradeable venue)", () => {
  const pools = [pool("pump1", SOL, AAA, 5, { dex: "pump_swap" })];
  assert.equal(countAccounts(pools), 0, "default: pricing-only, not counted");
  assert.equal(countAccounts(pools, { countPumpSwap: true }), 2, "tradeable: both vaults count");
});

test("countAccounts dedups shared accounts and includes CL state + DAMM lp", () => {
  const pools = [
    pool("p1", SOL, AAA, 5, { state_account: "st1" }),
    pool("p2", SOL, BBB, 5, { extra: { a_vault_lp: "lp1", b_vault_lp: "lp2" } }),
    pool("p3", SOL, AAA, 5, { vault_a: "p1-va", vault_b: "p1-vb" }), // duplicate vaults
  ];
  // p1: va,vb,st1 = 3 | p2: va,vb,lp1,lp2 = 4 | p3: dupes = 0  → 7
  assert.equal(countAccounts(pools), 7);
});

test("HUBS contains both SOL and USDC", () => {
  assert.ok(HUBS.has(SOL) && HUBS.has(USDC));
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/reduce_pools.test.js`
Expected: FAIL — `pruneToCycles is not a function` (no `module.exports` yet).

- [ ] **Step 3: Add exports and apply both corrections**

At the **end** of `scripts/reduce_pools.js`, add:

```js
module.exports = { pruneToCycles, countAccounts, selectDiverse, HUBS, USDC, SOL };
```

If `SOL` is not already a top-level const in the file, add it next to `USDC`:

```js
const SOL = 'So11111111111111111111111111111111111111112';
```

Replace the connectivity-seeding lines at the end of `pruneToCycles` (currently seeded from `USDC` only) with a **both-hubs** seed:

```js
  // Keep only the component reachable from a hub. Seed from EVERY hub present: the arb
  // base is SOL, so a SOL-only component is legitimate and must not be discarded.
  const seen = new Set();
  const stack = [];
  for (const h of HUBS) if (adj.has(h)) { seen.add(h); stack.push(h); }
  while (stack.length) {
    for (const n of adj.get(stack.pop()) || []) if (!seen.has(n)) { seen.add(n); stack.push(n); }
  }
  return kept.filter((p) => seen.has(p.token_a) && seen.has(p.token_b));
```

Change `countAccounts` to take the opt-in flag:

```js
function countAccounts(pools, opts = {}) {
  const countPumpSwap = opts.countPumpSwap === true;
  const s = new Set();
  for (const p of pools) {
    // PumpSwap vaults are only subscribed when the venue is tradeable
    // (ENABLE_PUMPSWAP_TRADING); otherwise they are pricing-only for the watcher.
    if (p.dex === 'pump_swap' && !countPumpSwap) continue;
    for (const k of ['vault_a', 'vault_b', 'state_account']) if (p[k]) s.add(p[k]);
    const ex = p.extra || {};
    for (const k of ['a_vault_lp', 'b_vault_lp']) if (ex[k]) s.add(ex[k]);
  }
  return s.size;
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/reduce_pools.test.js`
Expected: PASS (7 tests). Also confirm the script still runs standalone:
`node scripts/reduce_pools.js` → prints its report, exits 0.

- [ ] **Step 5: Commit**

```bash
git add scripts/reduce_pools.js scripts/reduce_pools.test.js
git commit -m "refactor(scan): export reduce_pools primitives; count pump vaults, seed hubs from SOL+USDC"
```

---

### Task 2: Arb-specific token safety gate

Momentum's screening never faces this: inside an arb *cycle*, a token whose freeze authority is live can have a leg frozen, and a Token-2022 transfer hook can block the second leg — both trap capital mid-cycle (honeypot).

**Files:**
- Create: `scripts/lib/token_safety.js`
- Test: `scripts/lib/token_safety.test.js`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `classifyMintSafety(info: object|null) -> { safe: boolean, reasons: string[] }` — pure; `info` is the `data.parsed.info` of a `getAccountInfo(mint, {encoding:"jsonParsed"})` response.
  - `async fetchMintSafety(rpcUrl: string, mints: string[]) -> Map<string, {safe, reasons}>` — batched `getMultipleAccounts`, applies `classifyMintSafety`.

- [ ] **Step 1: Write the failing tests**

Create `scripts/lib/token_safety.test.js`:

```js
"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { classifyMintSafety } = require("./token_safety");

const clean = { decimals: 6, mintAuthority: null, freezeAuthority: null };

test("accepts a clean mint", () => {
  const r = classifyMintSafety(clean);
  assert.equal(r.safe, true);
  assert.deepEqual(r.reasons, []);
});

test("rejects a mint with freeze authority enabled", () => {
  const r = classifyMintSafety({ ...clean, freezeAuthority: "Fr33zeAuth11111111111111111111111111111111" });
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /freeze authority/i);
});

test("rejects a Token-2022 mint with a transfer hook", () => {
  const info = {
    ...clean,
    extensions: [{ extension: "transferHook", state: { programId: "Hook111111111111111111111111111111111111111" } }],
  };
  const r = classifyMintSafety(info);
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /transfer hook/i);
});

test("allows benign Token-2022 extensions (e.g. metadata pointer)", () => {
  const info = { ...clean, extensions: [{ extension: "metadataPointer", state: {} }] };
  assert.equal(classifyMintSafety(info).safe, true);
});

test("treats a missing mint account as unsafe", () => {
  const r = classifyMintSafety(null);
  assert.equal(r.safe, false);
  assert.match(r.reasons.join(" "), /not found/i);
});

test("mint authority alone does not reject (recorded only)", () => {
  const r = classifyMintSafety({ ...clean, mintAuthority: "Mint111111111111111111111111111111111111111" });
  assert.equal(r.safe, true, "inflatable supply is a momentum concern, not a trapped-capital one");
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/lib/token_safety.test.js`
Expected: FAIL — cannot find module `./token_safety`.

- [ ] **Step 3: Implement the module**

Create `scripts/lib/token_safety.js`:

```js
#!/usr/bin/env node
/*
 * token_safety.js — arb-specific mint screening.
 *
 * WHY (different from the momentum scanner's checks): inside an arb CYCLE, capital sits
 * in the intermediate token between legs. A live freeze authority can freeze that leg,
 * and a Token-2022 transfer hook can make the second leg fail — both strand funds. These
 * risks do not exist for a pricing-only watcher, so they are screened here, not in
 * scan_tokens.js.
 */
"use strict";
const https = require("https");
const http = require("http");

/** Pure: classify a parsed mint account. `info` = data.parsed.info, or null if absent. */
function classifyMintSafety(info) {
  const reasons = [];
  if (!info) return { safe: false, reasons: ["mint account not found or unparseable"] };
  if (info.freezeAuthority) {
    reasons.push(`freeze authority enabled (${info.freezeAuthority}) — a leg can be frozen mid-cycle`);
  }
  const hook = (info.extensions || []).find(
    (e) => e.extension === "transferHook" && e.state && e.state.programId,
  );
  if (hook) {
    reasons.push(`token-2022 transfer hook ${hook.state.programId} — can block the second leg`);
  }
  return { safe: reasons.length === 0, reasons };
}

function rpc(rpcUrl, method, params) {
  return new Promise((resolve, reject) => {
    const mod = rpcUrl.startsWith("https") ? https : http;
    const req = mod.request(rpcUrl, { method: "POST", headers: { "content-type": "application/json" } }, (res) => {
      let buf = "";
      res.on("data", (c) => (buf += c));
      res.on("end", () => {
        try { resolve(JSON.parse(buf)); } catch (e) { reject(new Error(`bad RPC response: ${buf.slice(0, 80)}`)); }
      });
    });
    req.on("error", reject);
    req.end(JSON.stringify({ jsonrpc: "2.0", id: 1, method, params }));
  });
}

/** Fetch + classify many mints. Batches of 100 (getMultipleAccounts limit). */
async function fetchMintSafety(rpcUrl, mints) {
  const out = new Map();
  for (let i = 0; i < mints.length; i += 100) {
    const batch = mints.slice(i, i + 100);
    const res = await rpc(rpcUrl, "getMultipleAccounts", [batch, { encoding: "jsonParsed" }]);
    const values = (res.result && res.result.value) || [];
    batch.forEach((mint, j) => {
      const v = values[j];
      const info = v && v.data && v.data.parsed ? v.data.parsed.info : null;
      out.set(mint, classifyMintSafety(info));
    });
  }
  return out;
}

module.exports = { classifyMintSafety, fetchMintSafety };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/lib/token_safety.test.js`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add scripts/lib/token_safety.js scripts/lib/token_safety.test.js
git commit -m "feat(scan): arb token safety gate (freeze authority, token-2022 transfer hook)"
```

---

### Task 3: Multi-venue resolver

A trending token is only useful to the arb bot if it trades on **≥2 venues the bot can execute**. DexScreener tells us where a token has pools; the exact `dex` kind is settled later by the decoder (DexScreener cannot distinguish Raydium AMM v4 from CLMM reliably), so this module returns *candidates* plus a tradeable-venue count.

**Files:**
- Create: `scripts/lib/venues.js`
- Test: `scripts/lib/venues.test.js`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `SUPPORTED_DEX_IDS: Set<string>` — DexScreener `dexId`s the bot can decode: `raydium`, `orca`, `meteora`, `pumpswap`.
  - `bestPoolPerVenue(pairs: object[], opts: {quoteAllowlist: Set<string>}) -> {dexId, pairAddress, quoteMint, liquidityUsd, volume24h}[]` — pure; per `dexId` keeps the pool with the highest 24 h volume (repo convention: volume, not liquidity — fake-TVL pools exist), and only pairs whose quote side is an allowed hub.
  - `tradeableVenueCount(venues: {dexId}[], opts: {pumpTradeable: boolean}) -> number` — pure; counts distinct venues, excluding `pumpswap` when it is not tradeable.

- [ ] **Step 1: Write the failing tests**

Create `scripts/lib/venues.test.js`:

```js
"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { bestPoolPerVenue, tradeableVenueCount, SUPPORTED_DEX_IDS } = require("./venues");

const SOL  = "So11111111111111111111111111111111111111112";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const OTHER = "9999999999999999999999999999999999999999999";
const hubs = new Set([SOL, USDC]);

const pair = (dexId, pairAddress, quote, vol, liq = 500_000) => ({
  dexId, pairAddress,
  quoteToken: { address: quote },
  volume: { h24: vol },
  liquidity: { usd: liq },
});

test("keeps the highest-24h-volume pool per venue", () => {
  const pairs = [
    pair("raydium", "ray-low", SOL, 1_000),
    pair("raydium", "ray-high", SOL, 900_000),
    pair("orca", "orca-1", SOL, 50_000),
  ];
  const out = bestPoolPerVenue(pairs, { quoteAllowlist: hubs });
  assert.equal(out.length, 2);
  assert.equal(out.find((v) => v.dexId === "raydium").pairAddress, "ray-high");
});

test("drops unsupported venues", () => {
  const pairs = [pair("someotherdex", "x-1", SOL, 999_999)];
  assert.deepEqual(bestPoolPerVenue(pairs, { quoteAllowlist: hubs }), []);
});

test("drops pairs whose quote side is not a hub", () => {
  const pairs = [pair("raydium", "ray-1", OTHER, 999_999)];
  assert.deepEqual(bestPoolPerVenue(pairs, { quoteAllowlist: hubs }), []);
});

test("tradeableVenueCount excludes pumpswap when the venue is not tradeable", () => {
  const venues = [{ dexId: "pumpswap" }, { dexId: "raydium" }];
  assert.equal(tradeableVenueCount(venues, { pumpTradeable: false }), 1);
  assert.equal(tradeableVenueCount(venues, { pumpTradeable: true }), 2);
});

test("a pumpswap-only token has <2 tradeable venues either way", () => {
  const venues = [{ dexId: "pumpswap" }];
  assert.ok(tradeableVenueCount(venues, { pumpTradeable: true }) < 2);
});

test("SUPPORTED_DEX_IDS covers exactly the decodable venues", () => {
  assert.deepEqual([...SUPPORTED_DEX_IDS].sort(), ["meteora", "orca", "pumpswap", "raydium"]);
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/lib/venues.test.js`
Expected: FAIL — cannot find module `./venues`.

- [ ] **Step 3: Implement the module**

Create `scripts/lib/venues.js`:

```js
#!/usr/bin/env node
/*
 * venues.js — map a token's DexScreener pairs to candidate pools per venue.
 *
 * Ranking is by 24h VOLUME, not liquidity: fake-TVL pools exist (see the DexScreener
 * pricing notes), and volume is the honest signal for "this pool actually trades".
 *
 * The precise `dex` kind (raydium_amm_v4 vs raydium_clmm, meteora_damm vs dlmm) is NOT
 * decided here — DexScreener's dexId is too coarse. The per-DEX decoder settles it.
 */
"use strict";

/** DexScreener dexIds the bot has a decoder + swap builder for. */
const SUPPORTED_DEX_IDS = new Set(["raydium", "orca", "meteora", "pumpswap"]);

function bestPoolPerVenue(pairs, opts) {
  const allow = opts.quoteAllowlist;
  const best = new Map();
  for (const p of pairs || []) {
    if (!SUPPORTED_DEX_IDS.has(p.dexId)) continue;
    const quoteMint = p.quoteToken && p.quoteToken.address;
    if (!quoteMint || !allow.has(quoteMint)) continue; // must close back to a hub
    const volume24h = (p.volume && p.volume.h24) || 0;
    const prev = best.get(p.dexId);
    if (!prev || volume24h > prev.volume24h) {
      best.set(p.dexId, {
        dexId: p.dexId,
        pairAddress: p.pairAddress,
        quoteMint,
        liquidityUsd: (p.liquidity && p.liquidity.usd) || 0,
        volume24h,
      });
    }
  }
  return [...best.values()];
}

function tradeableVenueCount(venues, opts) {
  const pumpTradeable = opts.pumpTradeable === true;
  const ids = new Set();
  for (const v of venues) {
    if (v.dexId === "pumpswap" && !pumpTradeable) continue;
    ids.add(v.dexId);
  }
  return ids.size;
}

module.exports = { SUPPORTED_DEX_IDS, bestPoolPerVenue, tradeableVenueCount };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/lib/venues.test.js`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add scripts/lib/venues.js scripts/lib/venues.test.js
git commit -m "feat(scan): multi-venue resolver (best pool per venue, tradeable-venue count)"
```

---

### Task 4: Budgeted book selection with protected core and hysteresis

The feed starves past ~200 subscribed accounts (the July-5 root cause), so the book must be **replaced within a budget**, never grown. Churn costs an ALT extension + a restart, so a challenger must clearly beat an incumbent before evicting it.

**Files:**
- Create: `scripts/lib/book_budget.js`
- Test: `scripts/lib/book_budget.test.js`

**Interfaces:**
- Consumes: `countAccounts` from Task 1 (`require("../reduce_pools")`).
- Produces:
  - `isProtected(pool, ctx: {pinnedIds: Set<string>, momentumPoolIds: Set<string>, hubs: Set<string>}) -> boolean` — pure; true for hub-major pools (both sides hubs), fetcher-pinned addresses, and pools referenced by `momentum_tokens.json`.
  - `selectBook(args) -> {kept: PoolCfg[], evicted: PoolCfg[], skipped: {pool, reason}[], accounts: number}` where `args = {core, candidates, incumbentIds: Set<string>, budget: number, evictMargin: number, countPumpSwap: boolean}`; throws `Error` when the core alone exceeds the budget.

- [ ] **Step 1: Write the failing tests**

Create `scripts/lib/book_budget.test.js`:

```js
"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { isProtected, selectBook } = require("./book_budget");

const SOL  = "So11111111111111111111111111111111111111112";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const AAA  = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const hubs = new Set([SOL, USDC]);

// Each pool contributes exactly 2 accounts (vault_a + vault_b).
const pool = (id, token_a, token_b, act) => ({
  id, token_a, token_b, _act: act, dex: "raydium_amm_v4",
  vault_a: id + "-va", vault_b: id + "-vb",
});
const ctx = { pinnedIds: new Set(["pinned1"]), momentumPoolIds: new Set(["mom1"]), hubs };

test("isProtected: hub-major pool, pinned address, momentum pool", () => {
  assert.equal(isProtected(pool("x", SOL, USDC, 1), ctx), true, "SOL/USDC major");
  assert.equal(isProtected(pool("pinned1", SOL, AAA, 1), ctx), true, "fetcher-pinned");
  assert.equal(isProtected(pool("mom1", SOL, AAA, 1), ctx), true, "momentum watcher pool");
  assert.equal(isProtected(pool("other", SOL, AAA, 1), ctx), false);
});

test("selectBook keeps all core and fills remaining budget by activity", () => {
  const core = [pool("core1", SOL, USDC, 1)];                 // 2 accounts
  const candidates = [pool("c-hi", SOL, AAA, 100), pool("c-lo", SOL, AAA, 1)];
  const r = selectBook({ core, candidates, incumbentIds: new Set(), budget: 4, evictMargin: 1.25, countPumpSwap: false });
  assert.deepEqual(r.kept.map((p) => p.id), ["core1", "c-hi"], "highest activity wins the last slot");
  assert.equal(r.accounts, 4);
});

test("selectBook never exceeds the account budget", () => {
  const core = [pool("core1", SOL, USDC, 1)];
  const candidates = [pool("c1", SOL, AAA, 9), pool("c2", SOL, AAA, 8)];
  const r = selectBook({ core, candidates, incumbentIds: new Set(), budget: 3, evictMargin: 1.25, countPumpSwap: false });
  assert.equal(r.kept.length, 1, "core only — no room for a 2-account candidate");
  assert.ok(r.accounts <= 3);
});

test("selectBook throws when the core alone exceeds the budget", () => {
  const core = [pool("core1", SOL, USDC, 1), pool("core2", SOL, USDC, 1)];
  assert.throws(
    () => selectBook({ core, candidates: [], incumbentIds: new Set(), budget: 2, evictMargin: 1.25, countPumpSwap: false }),
    /core .*exceeds/i,
  );
});

test("hysteresis: a marginally-better challenger cannot evict an incumbent", () => {
  const core = [];
  const incumbent = pool("inc", SOL, AAA, 100);
  const challenger = pool("new", SOL, AAA, 110);      // only 1.1x — below the 1.25 margin
  const r = selectBook({
    core, candidates: [challenger, incumbent], incumbentIds: new Set(["inc"]),
    budget: 2, evictMargin: 1.25, countPumpSwap: false,
  });
  assert.deepEqual(r.kept.map((p) => p.id), ["inc"], "incumbent holds the slot");
});

test("hysteresis: a decisively-better challenger does evict", () => {
  const core = [];
  const incumbent = pool("inc", SOL, AAA, 100);
  const challenger = pool("new", SOL, AAA, 500);      // 5x — clears the margin
  const r = selectBook({
    core, candidates: [challenger, incumbent], incumbentIds: new Set(["inc"]),
    budget: 2, evictMargin: 1.25, countPumpSwap: false,
  });
  assert.deepEqual(r.kept.map((p) => p.id), ["new"]);
  assert.deepEqual(r.evicted.map((p) => p.id), ["inc"]);
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/lib/book_budget.test.js`
Expected: FAIL — cannot find module `./book_budget`.

- [ ] **Step 3: Implement the module**

Create `scripts/lib/book_budget.js`:

```js
#!/usr/bin/env node
/*
 * book_budget.js — choose the final pool book under a SUBSCRIBED-ACCOUNT budget.
 *
 * WHY a budget: free/shared Yellowstone tiers throttle large subscriptions and starve the
 * graph (the documented cause of phantom stale-edge cycles). Adding pools without a cap
 * makes freshness worse, so discovery REPLACES within a fixed budget.
 *
 * WHY hysteresis: every book change costs an ALT extension + a bot restart (which also
 * resets the LATENCY ring). A challenger must beat an incumbent by `evictMargin` to take
 * its slot, so the book does not thrash on noise.
 */
"use strict";
const { countAccounts } = require("../reduce_pools");

function isProtected(pool, ctx) {
  if (ctx.pinnedIds.has(pool.id)) return true;
  if (ctx.momentumPoolIds.has(pool.id)) return true;
  return ctx.hubs.has(pool.token_a) && ctx.hubs.has(pool.token_b); // hub-major
}

function selectBook(args) {
  const { core, candidates, incumbentIds, budget, evictMargin, countPumpSwap } = args;
  const opts = { countPumpSwap };

  const coreAccounts = countAccounts(core, opts);
  if (coreAccounts > budget) {
    throw new Error(
      `protected core needs ${coreAccounts} accounts which exceeds ARB_ACCOUNT_BUDGET=${budget} — ` +
      `raise the budget or trim pinned pools`,
    );
  }

  // Incumbents first at equal strength (hysteresis), then by activity desc.
  const ranked = candidates
    .filter((p) => !core.some((c) => c.id === p.id))
    .sort((a, b) => (b._act - a._act) || (incumbentIds.has(b.id) ? 1 : 0) - (incumbentIds.has(a.id) ? 1 : 0));

  const kept = core.slice();
  const skipped = [];
  for (const cand of ranked) {
    const trial = kept.concat([cand]);
    if (countAccounts(trial, opts) <= budget) { kept.push(cand); continue; }

    // Budget full. A NON-incumbent may displace the weakest kept non-core incumbent only
    // if it beats it by the eviction margin.
    const weakest = kept
      .filter((p) => !core.some((c) => c.id === p.id))
      .sort((a, b) => a._act - b._act)[0];
    if (!incumbentIds.has(cand.id) && weakest && cand._act >= weakest._act * evictMargin) {
      const idx = kept.findIndex((p) => p.id === weakest.id);
      kept.splice(idx, 1);
      if (countAccounts(kept.concat([cand]), opts) <= budget) {
        kept.push(cand);
        skipped.push({ pool: weakest, reason: `evicted by ${cand.id} (activity ${cand._act} ≥ ${weakest._act}×${evictMargin})` });
        continue;
      }
      kept.splice(idx, 0, weakest); // restore — did not actually fit
    }
    skipped.push({ pool: cand, reason: "account budget full" });
  }

  const keptIds = new Set(kept.map((p) => p.id));
  const evicted = [...incumbentIds].filter((id) => !keptIds.has(id)).map((id) => ({ id }));
  return { kept, evicted, skipped, accounts: countAccounts(kept, opts) };
}

module.exports = { isProtected, selectBook };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/lib/book_budget.test.js`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
git add scripts/lib/book_budget.js scripts/lib/book_budget.test.js
git commit -m "feat(scan): budgeted book selection with protected core and eviction hysteresis"
```

---

### Task 5: `--pools` decode override on the non-pump fetchers

`fetch_pumpswap_pools.js` already accepts `--pools <addr,…>`. The scanner needs the same for the other venues so a discovered address can be decoded on demand into a full `PoolConfig`.

**Files:**
- Modify: `scripts/fetch_orca_pools.js`, `scripts/fetch_meteora_dlmm.js`, `scripts/fetch_raydium_pools.js`
- Test: manual CLI verification (these are network-bound discovery scripts; the repo has no fixtures for them)

**Interfaces:**
- Consumes: nothing.
- Produces: each fetcher supports `--pools <comma-separated addresses>` → decode exactly those addresses (skipping its own discovery) and `--output <path>` → write there instead of the default file. Unknown/undecodable addresses are reported and skipped, exit code stays `0` if at least one decoded.

- [ ] **Step 1: Read each fetcher's existing CLI + discovery boundary**

Run:
```bash
grep -n "process.argv\|--output\|async function main\|DISCOVER\|TARGET" scripts/fetch_orca_pools.js scripts/fetch_meteora_dlmm.js scripts/fetch_raydium_pools.js
grep -n "cliPools\|--pools" scripts/fetch_pumpswap_pools.js
```
Note the pattern used by the pump fetcher (`const cliPools = process.argv.includes("--pools") ? … : null;` then `const targets = cliPools ?? TARGET_POOLS;`) — mirror it exactly.

- [ ] **Step 2: Add the flag to each fetcher**

In each of the three files, immediately after the existing arg parsing, add (adjusting the variable name that holds the discovered address list):

```js
// --pools <addr,…>: decode exactly these addresses instead of running discovery.
// Used by scan_arb_pools.js to decode a newly-discovered pool on demand.
const cliPools = process.argv.includes("--pools")
  ? process.argv[process.argv.indexOf("--pools") + 1].split(",").map((s) => s.trim()).filter(Boolean)
  : null;
```

Then, at the point where the script has its list of addresses to decode, short-circuit discovery:

```js
const targets = cliPools ?? (await discoverAddresses());  // existing discovery call
```

For `fetch_raydium_pools.js`, whose discovery returns full API objects rather than addresses, fetch the pinned ids through the same Raydium `pools/info/ids` endpoint the file already uses; if that endpoint is not already present, decode from chain like the Orca path. Keep the existing behavior byte-identical when `--pools` is absent.

- [ ] **Step 3: Verify each fetcher decodes a known address**

Run (using addresses already present in `pools.json`):
```bash
node scripts/fetch_orca_pools.js --pools $(python3 -c "import json;print([p['id'] for p in json.load(open('pools.json')) if p['dex']=='orca_whirlpool'][0])") --output /tmp/orca_one.json
python3 -c "import json;d=json.load(open('/tmp/orca_one.json'));print('orca entries:',len(d));print(d[0]['dex'], d[0]['id'][:8])"
```
Expected: exactly 1 entry, `dex == "orca_whirlpool"`, with `vault_a`/`vault_b`/`state_account` populated. Repeat for `fetch_meteora_dlmm.js` (a `dlmm` pool) and `fetch_raydium_pools.js` (a `raydium_amm_v4` pool).

- [ ] **Step 4: Verify default behavior is unchanged**

Run: `node scripts/fetch_orca_pools.js --output /tmp/orca_all.json` and confirm the entry count matches the current `pools.json` Orca count:
```bash
python3 -c "import json;a=len(json.load(open('/tmp/orca_all.json')));b=len([p for p in json.load(open('pools.json')) if p['dex']=='orca_whirlpool']);print('fetched',a,'vs in book',b)"
```
Expected: same order of magnitude (discovery is live, so exact equality is not required — a large drop means the flag broke discovery).

- [ ] **Step 5: Commit**

```bash
git add scripts/fetch_orca_pools.js scripts/fetch_meteora_dlmm.js scripts/fetch_raydium_pools.js
git commit -m "feat(scan): --pools decode override on orca/dlmm/raydium fetchers"
```

---

### Task 6: The scanner orchestrator `scan_arb_pools.js`

Composes Tasks 1–5 into the pipeline and owns the atomic validated write plus the exit-status contract.

**Files:**
- Create: `scripts/scan_arb_pools.js`
- Test: `scripts/scan_arb_pools.test.js` (pure helpers only: validation + change detection)

**Interfaces:**
- Consumes: `pruneToCycles`, `countAccounts`, `HUBS` (Task 1); `fetchMintSafety` (Task 2); `bestPoolPerVenue`, `tradeableVenueCount` (Task 3); `isProtected`, `selectBook` (Task 4); the fetchers' `--pools` flag (Task 5); `filterCandidates` + `rankSurvivors` from `scripts/scan_tokens.js`.
- Produces:
  - `validateBook(pools) -> {ok: boolean, errors: string[]}` — pure; schema + required-field completeness per dex + non-empty.
  - `bookChanged(oldPools, newPools) -> boolean` — pure; order-insensitive comparison by canonical JSON.
  - CLI: `--report` (default; writes nothing) / `--apply`; exit `0` changed, `10` unchanged, other non-zero on failure.

- [ ] **Step 1: Write the failing tests**

Create `scripts/scan_arb_pools.test.js`:

```js
"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { validateBook, bookChanged } = require("./scan_arb_pools");

const SOL = "So11111111111111111111111111111111111111112";
const AAA = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ok = (id, dex = "raydium_amm_v4", extra = {}) => ({
  id, dex, token_a: SOL, token_b: AAA, vault_a: id + "-va", vault_b: id + "-vb",
  fee_bps: 25, ...extra,
});

test("validateBook accepts a well-formed book", () => {
  assert.deepEqual(validateBook([ok("p1")]), { ok: true, errors: [] });
});

test("validateBook rejects an empty book", () => {
  const r = validateBook([]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /empty/i);
});

test("validateBook rejects a pool missing a vault", () => {
  const bad = ok("p1");
  delete bad.vault_b;
  const r = validateBook([bad]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /vault_b/);
});

test("validateBook requires state_account for concentrated-liquidity pools", () => {
  const r = validateBook([ok("clmm1", "raydium_clmm")]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /state_account/);
});

test("validateBook requires pumpswap_coin_creator for a tradeable pump pool", () => {
  const r = validateBook([ok("pump1", "pump_swap")]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /coin_creator/);
});

test("bookChanged ignores ordering", () => {
  const a = [ok("p1"), ok("p2")];
  const b = [ok("p2"), ok("p1")];
  assert.equal(bookChanged(a, b), false);
});

test("bookChanged detects an added or removed pool", () => {
  assert.equal(bookChanged([ok("p1")], [ok("p1"), ok("p2")]), true);
  assert.equal(bookChanged([ok("p1"), ok("p2")], [ok("p1")]), true);
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/scan_arb_pools.test.js`
Expected: FAIL — cannot find module `./scan_arb_pools`.

- [ ] **Step 3: Implement the orchestrator**

Create `scripts/scan_arb_pools.js`. Start with the pure helpers the tests need, then the pipeline:

```js
#!/usr/bin/env node
/*
 * scan_arb_pools.js — discover trending, security-screened tokens that form EXECUTABLE
 * arb cycles, fit them into the gRPC account budget, and rewrite pools.json.
 *
 * Pipeline: discover (Birdeye + scan_tokens filters) → arb safety gate (freeze authority,
 * transfer hook) → resolve venues (DexScreener) → cycle-closure prune (non-hub token needs
 * ≥2 TRADEABLE venues, hub-connected) → budget prune (activity-ranked, protected core,
 * hysteresis) → decode via the per-DEX fetchers → atomic validated write.
 *
 * Exit: 0 = book changed & written | 10 = no change | other = failure (book untouched).
 *
 * Usage:
 *   node scripts/scan_arb_pools.js            # report only, writes nothing
 *   node scripts/scan_arb_pools.js --apply    # write pools.json (backup first)
 *
 * Design: docs/superpowers/specs/2026-07-25-dynamic-arb-pool-discovery-design.md
 */
"use strict";
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const { pruneToCycles, countAccounts, HUBS } = require("./reduce_pools");
const { fetchMintSafety } = require("./lib/token_safety");
const { bestPoolPerVenue, tradeableVenueCount } = require("./lib/venues");
const { isProtected, selectBook } = require("./lib/book_budget");

const POOLS_PATH = path.join(__dirname, "..", "pools.json");
const TOKENS_PATH = process.env.MOMENTUM_TOKENS_PATH ||
  path.join(__dirname, "..", "assets", "momentum_tokens.json");

// LSTs: SOL↔LST rate is the staking rate — no arb edge across venues (mirrors the
// DENY_MINTS set in reduce_pools.js). Kept local so the scanner is self-contained.
const LST_DENY = new Set([
  "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", // jitoSOL
  "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",  // mSOL
  "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1",  // bSOL
  "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v",  // JupSOL
  "BonK1YhkXEGLZzwtcvRTip3gAL9nCeQD7ppZBLXhtTs",  // bonkSOL
]);
const APPLY = process.argv.includes("--apply");
const num = (k, d) => Number(process.env[k] || d);
const CFG = {
  budget: num("ARB_ACCOUNT_BUDGET", 200),
  evictMargin: num("ARB_SCAN_EVICT_MARGIN", 1.25),
  activityWindow: num("ARB_ACTIVITY_WINDOW_SECS", 300),
  pumpTradeable: String(process.env.ENABLE_PUMPSWAP_TRADING || "false") === "true",
};

/** Required fields per dex kind — mirrors check_extra in src/dex/mod.rs. */
const REQUIRED = {
  base: ["id", "dex", "token_a", "token_b", "vault_a", "vault_b"],
  clPools: new Set(["raydium_clmm", "orca_whirlpool", "dlmm"]),
};

function validateBook(pools) {
  const errors = [];
  if (!Array.isArray(pools) || pools.length === 0) {
    return { ok: false, errors: ["book is empty — refusing to write"] };
  }
  const seen = new Set();
  for (const p of pools) {
    const tag = (p && p.id) || "<no id>";
    for (const f of REQUIRED.base) if (!p[f]) errors.push(`${tag}: missing ${f}`);
    if (REQUIRED.clPools.has(p.dex) && !p.state_account) errors.push(`${tag}: missing state_account (CL pool)`);
    if (p.dex === "pump_swap" && !(p.extra && p.extra.pumpswap_coin_creator)) {
      errors.push(`${tag}: missing extra.pumpswap_coin_creator (pump_swap)`);
    }
    if (seen.has(p.id)) errors.push(`${tag}: duplicate pool id`);
    seen.add(p.id);
  }
  return { ok: errors.length === 0, errors };
}

const canon = (pools) =>
  JSON.stringify([...pools].map((p) => JSON.stringify(p, Object.keys(p).sort())).sort());

function bookChanged(oldPools, newPools) {
  return canon(oldPools) !== canon(newPools);
}

module.exports = { validateBook, bookChanged };

// ─── Pipeline (only when run directly) ───────────────────────────────────────
if (require.main === module) {
  main().catch((e) => { console.error("scan failed:", e.message); process.exit(1); });
}

async function main() {
  const rpcUrl = process.env.RPC_URL;
  if (!rpcUrl) throw new Error("RPC_URL is required");
  const current = JSON.parse(fs.readFileSync(POOLS_PATH, "utf8"));

  // 1. Discover candidates via the existing momentum scanner (--json prints survivors).
  const discovered = JSON.parse(
    execFileSync(process.execPath, [path.join(__dirname, "scan_tokens.js"), "--json"], {
      encoding: "utf8", env: process.env,
    }) || "[]",
  );
  console.log(`discovered ${discovered.length} candidate token(s) from scan_tokens`);

  // 2. Arb safety gate.
  const safety = await fetchMintSafety(rpcUrl, discovered.map((t) => t.mint));
  const safe = discovered.filter((t) => {
    const s = safety.get(t.mint);
    if (!s || !s.safe) console.log(`  reject ${t.symbol}: ${(s && s.reasons.join("; ")) || "unknown"}`);
    return s && s.safe;
  });

  // 3. Resolve venues per survivor (DexScreener), keep tokens with ≥2 TRADEABLE venues.
  //    LSTs are excluded: their SOL↔LST rate IS the staking exchange rate, so a
  //    cross-venue cycle through them cannot clear multi-hop fees (dead weight).
  const candidatePools = [];
  for (const t of safe) {
    if (LST_DENY.has(t.mint)) { console.log(`  skip ${t.symbol}: LST (SOL-pegged, no arb edge)`); continue; }
    const pairs = await dexscreenerPairs(t.mint);
    const venues = bestPoolPerVenue(pairs, { quoteAllowlist: HUBS });
    if (tradeableVenueCount(venues, { pumpTradeable: CFG.pumpTradeable }) < 2) {
      console.log(`  skip ${t.symbol}: <2 tradeable venues (no cycle)`);
      continue;
    }
    for (const v of venues) candidatePools.push({ token: t, venue: v });
  }

  // 4. Decode each candidate address into a PoolConfig via its fetcher (Task 5).
  const decoded = [];
  for (const c of candidatePools) {
    const cfg = decodeViaFetcher(c.venue);
    if (!cfg) { console.log(`  skip ${c.venue.pairAddress.slice(0, 8)}: decode failed`); continue; }
    decoded.push({ ...cfg, _act: c.venue.volume24h });
  }

  // 5. Cycle-closure + budget.
  const pinnedIds = collectPinnedIds();
  const momentumPoolIds = collectMomentumPoolIds();
  const ctx = { pinnedIds, momentumPoolIds, hubs: HUBS };
  const withAct = current.map((p) => ({ ...p, _act: p._act || 0 }));
  const core = withAct.filter((p) => isProtected(p, ctx));
  const closed = pruneToCycles(core.concat(decoded));
  const sel = selectBook({
    core,
    candidates: closed.filter((p) => !core.some((c) => c.id === p.id)),
    incumbentIds: new Set(current.map((p) => p.id)),
    budget: CFG.budget,
    evictMargin: CFG.evictMargin,
    countPumpSwap: CFG.pumpTradeable,
  });

  const next = sel.kept.map(({ _act, ...rest }) => rest);
  const v = validateBook(next);
  if (!v.ok) throw new Error(`validation failed:\n  ${v.errors.join("\n  ")}`);

  console.log(`\nbook: ${next.length} pools / ${sel.accounts} accounts (budget ${CFG.budget})`);
  for (const s of sel.skipped) console.log(`  skipped ${s.pool.id.slice(0, 8)}: ${s.reason}`);

  if (!bookChanged(current, next)) { console.log("no change"); process.exit(10); }
  if (!APPLY) { console.log("report only — re-run with --apply to write"); process.exit(0); }

  fs.copyFileSync(POOLS_PATH, POOLS_PATH + ".bak");
  const tmp = POOLS_PATH + ".tmp";
  fs.writeFileSync(tmp, JSON.stringify(next, null, 2) + "\n");
  fs.renameSync(tmp, POOLS_PATH);      // atomic
  console.log(`wrote pools.json (backup at pools.json.bak)`);
  process.exit(0);
}
```

Implement the four remaining helpers in the same file, each small and single-purpose:

```js
/** DexScreener pairs for one mint. */
function dexscreenerPairs(mint) {
  return httpJson(`https://api.dexscreener.com/latest/dex/tokens/${mint}`).then((d) => (d && d.pairs) || []);
}

/** Run the venue's fetcher with --pools <addr> and return the single decoded PoolConfig. */
function decodeViaFetcher(venue) {
  const script = {
    raydium: "fetch_raydium_pools.js", orca: "fetch_orca_pools.js",
    meteora: "fetch_meteora_dlmm.js", pumpswap: "fetch_pumpswap_pools.js",
  }[venue.dexId];
  if (!script) return null;
  const out = path.join(require("os").tmpdir(), `arbscan_${venue.pairAddress}.json`);
  try {
    execFileSync(process.execPath, [path.join(__dirname, script), "--pools", venue.pairAddress, "--output", out],
      { encoding: "utf8", env: process.env, stdio: "pipe" });
    const arr = JSON.parse(fs.readFileSync(out, "utf8"));
    return Array.isArray(arr) && arr.length ? arr[0] : null;
  } catch { return null; }
}

/** Pool addresses hard-pinned inside the fetchers (never evict these). */
function collectPinnedIds() {
  const ids = new Set();
  for (const f of ["fetch_pumpswap_pools.js", "fetch_meteora_dlmm.js"]) {
    const src = fs.readFileSync(path.join(__dirname, f), "utf8");
    for (const m of src.matchAll(/"([1-9A-HJ-NP-Za-km-z]{32,44})"/g)) ids.add(m[1]);
  }
  return ids;
}

/** Pools the momentum watcher prices from — must survive every scan. */
function collectMomentumPoolIds() {
  try {
    return new Set(JSON.parse(fs.readFileSync(TOKENS_PATH, "utf8")).map((t) => t.pool).filter(Boolean));
  } catch { return new Set(); }
}

function httpJson(url) {
  const https = require("https");
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      let b = ""; res.on("data", (c) => (b += c));
      res.on("end", () => { try { resolve(JSON.parse(b)); } catch (e) { reject(e); } });
    }).on("error", reject);
  });
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/scan_arb_pools.test.js`
Expected: PASS (7 tests).

Then a live report run (writes nothing):
```bash
export $(grep -E "^(RPC_URL|BIRDEYE_API_KEY|ENABLE_PUMPSWAP_TRADING)=" .env | xargs)
node scripts/scan_arb_pools.js; echo "exit=$?"
```
Expected: prints discovered/rejected/skipped lines, a final `book: N pools / M accounts` with `M <= 200`, and exits `0` (changed) or `10` (no change). `git status` must show **no modification to `pools.json`**.

- [ ] **Step 5: Commit**

```bash
git add scripts/scan_arb_pools.js scripts/scan_arb_pools.test.js
git commit -m "feat(scan): scan_arb_pools orchestrator with atomic validated book write"
```

---

### Task 7: SIGHUP self re-exec in the bot

Lets the refresh loop reload the book with `kill -HUP` — same PID, same terminal, same stdout, no process supervisor. The decision is a pure predicate so it is unit-testable; the `exec` itself is verified manually once.

**Files:**
- Modify: `src/main.rs` (add `should_reexec` + a SIGHUP watcher task near the other `tokio::spawn` setup, before the BF loop)
- Test: `src/main.rs` `#[cfg(test)] mod tests` at the file bottom

**Interfaces:**
- Consumes: the existing `bundle_in_flight: Arc<AtomicBool>` (declared around `src/main.rs:862`).
- Produces: `fn should_reexec(requested: bool, in_flight: bool, waited_secs: u64, drain_secs: u64) -> bool`.

- [ ] **Step 1: Write the failing test**

Add at the **bottom** of `src/main.rs` (create the `mod tests` block if absent):

```rust
#[cfg(test)]
mod tests {
    use super::should_reexec;

    #[test]
    fn no_reexec_until_requested() {
        assert!(!should_reexec(false, false, 0, 30));
        assert!(!should_reexec(false, true, 999, 30));
    }

    #[test]
    fn reexecs_immediately_when_idle() {
        assert!(should_reexec(true, false, 0, 30));
    }

    #[test]
    fn waits_for_in_flight_submission_to_drain() {
        assert!(!should_reexec(true, true, 5, 30), "must not interrupt a submission");
    }

    #[test]
    fn reexecs_anyway_once_the_drain_window_elapses() {
        assert!(should_reexec(true, true, 30, 30), "bounded wait — never hang forever");
        assert!(should_reexec(true, true, 31, 30));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --bin solana-mev should_reexec`
Expected: FAIL — `cannot find function should_reexec in this scope`.

- [ ] **Step 3: Implement the predicate, the signal watcher, and the re-exec**

Add near the other free functions in `src/main.rs` (e.g. above `async fn main`):

```rust
/// True when a SIGHUP-requested restart may proceed: nothing is in flight, or the bounded
/// drain window has elapsed (so a stuck in-flight flag can never block a reload forever).
fn should_reexec(requested: bool, in_flight: bool, waited_secs: u64, drain_secs: u64) -> bool {
    requested && (!in_flight || waited_secs >= drain_secs)
}

/// Replace this process image with a fresh copy of the same binary + args. Same PID, same
/// terminal, same stdout — so `kill -HUP` reloads pools.json without a supervisor and
/// without detaching the operator's live log view. Returns only on failure.
fn reexec_self() -> anyhow::Error {
    use std::os::unix::process::CommandExt;
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => return anyhow::anyhow!("current_exe failed: {e}"),
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let err = std::process::Command::new(exe).args(args).exec(); // never returns on success
    anyhow::anyhow!("exec failed: {err}")
}
```

Then, after `bundle_in_flight` is declared (around `src/main.rs:862`) and before the BF loop, add the watcher:

```rust
    // ── SIGHUP → reload pools.json by re-exec'ing ourselves ───────────────────
    // `scripts/arb_refresh_loop.sh` rewrites the book + extends the ALT, then HUPs us.
    // We drain any in-flight submission first so a refresh never interrupts one.
    {
        let restart_requested = Arc::new(AtomicBool::new(false));
        let flag_sig = Arc::clone(&restart_requested);
        tokio::spawn(async move {
            let mut hup = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => { warn!("could not install SIGHUP handler: {e}"); return; }
            };
            while hup.recv().await.is_some() {
                warn!("SIGHUP received — reloading pools.json after in-flight submissions drain");
                flag_sig.store(true, Ordering::Release);
            }
        });
        let in_flight_re = Arc::clone(&bundle_in_flight);
        let drain_secs: u64 = std::env::var("ARB_REEXEC_DRAIN_SECS")
            .ok().and_then(|v| v.parse().ok()).unwrap_or(30);
        tokio::spawn(async move {
            let mut waited = 0u64;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                let requested = restart_requested.load(Ordering::Acquire);
                if !requested { waited = 0; continue; }
                let in_flight = in_flight_re.load(Ordering::Acquire);
                if should_reexec(requested, in_flight, waited, drain_secs) {
                    warn!("re-exec now (waited {waited}s, in_flight={in_flight})");
                    let e = reexec_self();
                    error!("re-exec failed, continuing with the OLD book: {e}");
                    restart_requested.store(false, Ordering::Release);
                    waited = 0;
                } else {
                    waited += 1;
                }
            }
        });
    }
```

- [ ] **Step 4: Run tests and build**

Run:
```bash
cargo test --bin solana-mev should_reexec
cargo test --bin solana-mev 2>&1 | tail -2
cargo clippy --bin solana-mev 2>&1 | grep -cE "^error"
cargo build --release --bin solana-mev 2>&1 | tail -1
```
Expected: 4 new tests PASS; full bin suite PASS; clippy errors `0`; release build finishes.

- [ ] **Step 5: Manually verify the reload (one-time)**

Start the bot (`DRY_RUN=true cargo run --release --bin solana-mev`), note its PID and the `Loaded N pools` line, then in another shell:
```bash
kill -HUP $(pgrep -f "target/release/solana-mev" | head -1)
```
Expected: the **same terminal** prints the SIGHUP warn, then a fresh startup banner with `Loaded N pools`; `pgrep` shows the **same PID**. Stop the bot.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat(arb): SIGHUP self re-exec to reload pools.json without a supervisor"
```

---

### Task 8: Trigger loop + documentation

**Files:**
- Create: `scripts/arb_refresh_loop.sh`
- Modify: `CLAUDE.md`, `.env.example`

**Interfaces:**
- Consumes: `scan_arb_pools.js` exit codes (Task 6); the bot's SIGHUP handler (Task 7).
- Produces: `scripts/arb_refresh_loop.sh` — one cycle per `ARB_SCAN_INTERVAL_SECS`.

- [ ] **Step 1: Write the loop script**

Create `scripts/arb_refresh_loop.sh`:

```bash
#!/usr/bin/env bash
# arb_refresh_loop.sh — periodic arb book refresh.
#
# One cycle: scan --apply → (on change) extend the ALT → SIGHUP the bot so it reloads.
# Deliberately dumb: no pool logic here, and no process management — the bot re-execs
# itself on SIGHUP (same PID, same terminal).
#
# Usage: ./scripts/arb_refresh_loop.sh            # loop forever
#        ONESHOT=1 ./scripts/arb_refresh_loop.sh  # single cycle (cron / manual)
set -uo pipefail
cd "$(dirname "$0")/.."
set -a; [ -f .env ] && . ./.env; set +a

INTERVAL="${ARB_SCAN_INTERVAL_SECS:-21600}"   # ~6h

one_cycle() {
  echo "[$(date -u +%FT%TZ)] arb refresh: scanning"
  node scripts/scan_arb_pools.js --apply
  local rc=$?
  case "$rc" in
    0)  echo "  book changed — extending ALT"
        if ! cargo run --release --bin solana-mev -- --init-alt; then
          echo "  !! --init-alt FAILED — not sending SIGHUP (book+ALT stay consistent)" >&2
          return 1
        fi
        local pid; pid="$(pgrep -f 'target/release/solana-mev' | head -1)"
        if [ -n "$pid" ]; then echo "  HUP -> $pid"; kill -HUP "$pid";
        else echo "  no bot running — book+ALT ready for next start"; fi ;;
    10) echo "  no change" ;;
    *)  echo "  !! scan FAILED (rc=$rc) — book untouched" >&2; return 1 ;;
  esac
}

if [ -n "${ONESHOT:-}" ]; then one_cycle; exit $?; fi
while true; do one_cycle || true; sleep "$INTERVAL"; done
```

- [ ] **Step 2: Make it executable and dry-run one cycle**

Run:
```bash
chmod +x scripts/arb_refresh_loop.sh
ONESHOT=1 ARB_SCAN_INTERVAL_SECS=1 ./scripts/arb_refresh_loop.sh
```
Expected: it scans and reports; with no bot running it prints "no bot running". If it reports "book changed", confirm `git diff --stat pools.json` is a sane diff (not a wipe) before trusting it.

- [ ] **Step 3: Document in `.env.example`**

Append:

```bash
# ── Dynamic arb pool discovery (scripts/arb_refresh_loop.sh; opt-in, run manually) ──
# Periodically re-scans for trending, security-screened tokens that form executable arb
# cycles, rewrites pools.json within a subscribed-account budget, extends the ALT, and
# SIGHUPs the bot to reload. See docs/superpowers/specs/2026-07-25-dynamic-arb-pool-discovery-design.md
ARB_ACCOUNT_BUDGET=200          # max subscribed accounts in the generated book (feed starves past ~200)
ARB_SCAN_INTERVAL_SECS=21600    # refresh cadence (~6h); restarts reset the LATENCY ring, so keep it coarse
ARB_SCAN_EVICT_MARGIN=1.25      # a challenger must beat an incumbent's activity by this factor to evict it
ARB_ACTIVITY_WINDOW_SECS=300    # on-chain activity-ranking window
ARB_REEXEC_DRAIN_SECS=30        # bounded wait for in-flight submissions before re-exec on SIGHUP
```

- [ ] **Step 4: Document in `CLAUDE.md`**

Add a subsection after the "Pool config (pools.json)" section:

```markdown
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

**Run `node scripts/scan_arb_pools.js` (report mode, writes nothing) and inspect the diff
for a few cycles before enabling the loop.** Caveat: PumpSwap only counts as a tradeable
venue when `ENABLE_PUMPSWAP_TRADING=true`; otherwise pump pools stay pricing-only and
cannot close a cycle.
```

- [ ] **Step 5: Commit**

```bash
git add scripts/arb_refresh_loop.sh .env.example CLAUDE.md
git commit -m "feat(scan): arb refresh loop + docs for dynamic pool discovery"
```

---

## Deferred (spec items intentionally not implemented here)

- **Venue-per-pair cap.** The spec lists it, but the protected-core design largely subsumes
  its concern: SOL/USDC majors live in the always-kept core (counted separately), so they
  cannot crowd out discovery slots — the candidates competing for the budget are already
  cross-venue tokens. A hard per-pair cap on *discovered* pools is a small follow-on if a
  single 4-venue token ever monopolizes slots; not needed for correctness now.

## Verification (whole feature)

```bash
node --test scripts/                      # all JS unit tests
cargo test --bin solana-mev               # incl. should_reexec
cargo clippy --lib --bin solana-mev       # expect 0 errors
node scripts/scan_arb_pools.js            # report mode: no pools.json change
git status --short                        # pools.json must be untouched by a report run
```

Acceptance before trusting automation: run report mode across a few hours and confirm
(a) majors/pinned/momentum pools are always kept, (b) account count ≤ budget, (c) every
rejection has a stated reason, (d) discovered tokens genuinely have ≥2 tradeable venues.
Then one `--apply` + `kill -HUP` and confirm the bot reloads with the new pool count.
