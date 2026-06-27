# Birdeye-Trending Discovery Source Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add `MOMENTUM_SCAN_SOURCE=trending|volume` (default `trending`) to `scripts/scan_tokens.js`, sourcing discovery candidates from Birdeye's `/defi/token_trending` feed in one call, while preserving the existing volume-pagination path behind the flag.

**Architecture:** A new pure mapper turns a trending API token into the existing candidate-row shape (carrying 24h change inline). `main()` branches on `OPTS.source` to pick the fetcher; the trending rows then flow through the **unchanged** `filterCandidates` → `verifyAll` → `rankSurvivors` pipeline. The per-mint `annotateChange24h` step becomes conditional so the trending path (which already has change) skips it. The `--json` output contract and the Rust watcher are untouched.

**Tech Stack:** Node.js (CommonJS), `node:test` + `node:assert` for unit tests, global `fetch`.

## Global Constraints

- Do NOT change the `--json` output shape: `[{symbol, mint, name, vol24, liq, change24h}]`.
- Do NOT change `filterCandidates`, `rankSurvivors`, `fetchBirdeyeTopVolume`, `fetchChange24h`, the floors (`SCAN_MIN_VOLUME=250000`, `SCAN_MIN_LIQUIDITY=200000`, `SCAN_MAX_RATIO=30`), the `0 < chg ≤ MOMENTUM_SCAN_MAX_CHANGE_PCT` band, the Jupiter verify step, the `--apply` path, or any Rust file.
- Default `MOMENTUM_SCAN_SOURCE` is `trending`; setting it to `volume` restores today's behavior exactly.
- Match the existing test style in `scripts/scan_tokens.test.js` (`node:test`, `node:assert`, require the module's exports). Network functions stay untested (live-only).
- Keep diffs minimal — do not reformat `scripts/scan_tokens.js` wholesale.
- Test command (repo root): `node --test scripts/scan_tokens.test.js`

---

## File Structure

- Modify: `scripts/scan_tokens.js` — add `mapTrendingToken`, `needsChange`, `fetchBirdeyeTrending`; extend `OPTS`; branch `main()`; carry `change24h` through the survivor map; export the two new pure functions.
- Modify: `scripts/scan_tokens.test.js` — add unit tests for `mapTrendingToken`, `needsChange`, and change24h preservation through `filterCandidates`.

---

### Task 1: Pure trending→candidate mapper (`mapTrendingToken`)

**Files:**
- Modify: `scripts/scan_tokens.js` (add function near the other fetch helpers, ~after `fetchBirdeyeTopVolume`; extend `module.exports` at line ~246)
- Test: `scripts/scan_tokens.test.js`

**Interfaces:**
- Consumes: nothing (pure).
- Produces: `mapTrendingToken(t) -> { address: string, symbol: string, name: string, v24hUSD: number, liquidity: number, change24h: number|null }`. Maps Birdeye trending fields (`address`, `symbol`, `name`, `volume24hUSD`, `liquidity`, `price24hChangePercent`) to the candidate-row shape consumed by `filterCandidates`. Non-finite `price24hChangePercent` → `null`; missing numerics → `0`.

- [ ] **Step 1: Write the failing tests**

Append to `scripts/scan_tokens.test.js`:

```javascript
// ── mapTrendingToken (trending API → candidate row) ───────────────────────────────
const { mapTrendingToken } = require("./scan_tokens");

test("mapTrendingToken maps Birdeye trending fields to the candidate row shape", () => {
  const t = {
    address: RAY, symbol: "RAY", name: "Raydium",
    volume24hUSD: 1_700_000, liquidity: 427_000, price24hChangePercent: 33.5,
  };
  assert.deepEqual(mapTrendingToken(t), {
    address: RAY, symbol: "RAY", name: "Raydium",
    v24hUSD: 1_700_000, liquidity: 427_000, change24h: 33.5,
  });
});

test("mapTrendingToken coerces missing numerics to 0 and non-finite change to null", () => {
  const out = mapTrendingToken({ address: BONK });
  assert.equal(out.symbol, "");
  assert.equal(out.name, "");
  assert.equal(out.v24hUSD, 0);
  assert.equal(out.liquidity, 0);
  assert.equal(out.change24h, null);
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/scan_tokens.test.js`
Expected: FAIL — `mapTrendingToken is not a function` (not yet exported).

- [ ] **Step 3: Implement `mapTrendingToken` and export it**

In `scripts/scan_tokens.js`, add this function immediately after `fetchBirdeyeTopVolume` (after line 139):

```javascript
// Map one Birdeye `/defi/token_trending` token to the candidate-row shape used by
// filterCandidates. Trending carries 24h change inline (price24hChangePercent), so the
// change-rank path needs no extra per-mint fetch. Non-finite change → null (dropped by band).
function mapTrendingToken(t) {
  const c = +t.price24hChangePercent;
  return {
    address: t.address,
    symbol: t.symbol || "",
    name: t.name || "",
    v24hUSD: +t.volume24hUSD || 0,
    liquidity: +t.liquidity || 0,
    change24h: Number.isFinite(c) ? c : null,
  };
}
```

Update the exports line (currently `module.exports = { filterCandidates, rankSurvivors };`) to:

```javascript
module.exports = { filterCandidates, rankSurvivors, mapTrendingToken };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/scan_tokens.test.js`
Expected: PASS (all existing + 2 new tests).

- [ ] **Step 5: Commit**

```bash
git add scripts/scan_tokens.js scripts/scan_tokens.test.js
git commit -m "feat(scan): mapTrendingToken — Birdeye trending token → candidate row"
```

---

### Task 2: Carry `change24h` through survivors + conditional annotation (`needsChange`)

**Files:**
- Modify: `scripts/scan_tokens.js` (survivor map ~line 201; annotate block ~line 206; add `needsChange`; extend exports)
- Test: `scripts/scan_tokens.test.js`

**Interfaces:**
- Consumes: `mapTrendingToken` (Task 1), `filterCandidates` (existing).
- Produces: `needsChange(s) -> boolean` — true when a survivor lacks a finite `change24h` (i.e. the volume path, which must still fetch it). Also: surviving rows now preserve `change24h` from the source row through the survivor map, so `rankSurvivors` can band/sort trending rows with no extra fetch.

- [ ] **Step 1: Write the failing tests**

Append to `scripts/scan_tokens.test.js`:

```javascript
// ── needsChange (annotate-skip predicate) + change24h flow-through ─────────────────
const { needsChange } = require("./scan_tokens");

test("needsChange is true only when change24h is non-finite", () => {
  assert.equal(needsChange({ change24h: 12.3 }), false);
  assert.equal(needsChange({ change24h: 0 }), false);
  assert.equal(needsChange({ change24h: null }), true);
  assert.equal(needsChange({ change24h: NaN }), true);
  assert.equal(needsChange({}), true);
});

test("filterCandidates preserves change24h on a surviving trending row", () => {
  const mapped = mapTrendingToken({
    address: BONK, symbol: "BONK", name: "Bonk",
    volume24hUSD: 2_000_000, liquidity: 800_000, price24hChangePercent: 22.5,
  });
  const out = filterCandidates([mapped], [], opts);
  assert.equal(out.length, 1);
  assert.equal(out[0].change24h, 22.5);
});
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `node --test scripts/scan_tokens.test.js`
Expected: FAIL — `needsChange is not a function` (not yet exported).

- [ ] **Step 3: Add `needsChange`, carry `change24h` through, make annotation conditional**

In `scripts/scan_tokens.js`:

(a) Add the predicate immediately above `annotateChange24h` (before line 181):

```javascript
// A survivor still needs a per-mint change fetch only if it has no finite change24h yet
// (the volume path). Trending rows arrive with change24h inline, so they skip the fetch.
const needsChange = (s) => !Number.isFinite(s.change24h);
```

(b) In `main()`, change the survivor map (currently lines 201–203) to carry `change24h`:

```javascript
  let survivors = verified.map((r) => ({
    symbol: r.symbol, mint: r.address, name: r.name,
    vol24: r.v24hUSD, liq: r.liquidity, change24h: r.change24h,
  }));
```

(c) In `main()`, change the annotation block (currently lines 206–208) to only fetch the rows that need it:

```javascript
  if (OPTS.rank === "change") {
    await annotateChange24h(survivors.filter(needsChange));
  }
```

> `annotateChange24h` mutates each row object in place; `survivors.filter(needsChange)` returns references to the same objects, so the originals are updated. Trending rows (finite `change24h`) are excluded → zero extra calls. Volume rows (`change24h === undefined`) are all included → identical to today.

(d) Extend the exports line to add `needsChange`:

```javascript
module.exports = { filterCandidates, rankSurvivors, mapTrendingToken, needsChange };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `node --test scripts/scan_tokens.test.js`
Expected: PASS (all existing + new tests).

- [ ] **Step 5: Commit**

```bash
git add scripts/scan_tokens.js scripts/scan_tokens.test.js
git commit -m "feat(scan): carry change24h through survivors; annotate only rows missing it"
```

---

### Task 3: Trending fetcher + source toggle wired into `main()`

**Files:**
- Modify: `scripts/scan_tokens.js` (`OPTS` ~lines 35–50; new `fetchBirdeyeTrending` after `mapTrendingToken`; `main()` row-fetch branch ~line 197; summary log ~line 230)

**Interfaces:**
- Consumes: `mapTrendingToken` (Task 1); `OPTS.source`, `OPTS.trendingLimit`, `OPTS.minVolume`, `OPTS.maxPages` (existing/new).
- Produces: `fetchBirdeyeTrending(limit) -> Promise<Array<candidateRow>>` — one GET to `/defi/token_trending`, mapped via `mapTrendingToken`. Network function (untested, live-only). No new public exports.

- [ ] **Step 1: Add the `source` and `trendingLimit` options to `OPTS`**

In `scripts/scan_tokens.js`, inside the `OPTS` object (after the `maxChangePct` entry, ~line 50), add:

```javascript
  // Discovery source: "trending" (default — /defi/token_trending, one call, carries 24h
  // change inline) or "volume" (the legacy paginated /defi/tokenlist path).
  source: (process.env.MOMENTUM_SCAN_SOURCE || "trending").trim().toLowerCase(),
  // How many trending tokens to request when source="trending".
  trendingLimit: numEnv("MOMENTUM_SCAN_TRENDING_LIMIT", 20),
```

- [ ] **Step 2: Implement `fetchBirdeyeTrending`**

Add immediately after `mapTrendingToken` (from Task 1):

```javascript
// Birdeye trending feed — a single call that returns hot movers with volume, liquidity, and
// 24h price-change inline (no pagination, no per-mint change fetch). Free-tier accessible.
async function fetchBirdeyeTrending(limit) {
  const key = process.env.BIRDEYE_API_KEY || "";
  if (!key) throw new Error("BIRDEYE_API_KEY is not set");
  const url =
    `https://public-api.birdeye.so/defi/token_trending` +
    `?sort_by=rank&sort_type=asc&offset=0&limit=${limit}`;
  const res = await fetch(url, {
    headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" },
  });
  if (!res.ok) throw new Error(`Birdeye token_trending -> HTTP ${res.status}`);
  const body = await res.json();
  const tokens = (body && body.data && body.data.tokens) || [];
  return tokens.map(mapTrendingToken);
}
```

- [ ] **Step 3: Branch the row fetch in `main()` on the source**

Replace the current row fetch (line 197):

```javascript
  const rows = await fetchBirdeyeTopVolume(OPTS.minVolume, OPTS.maxPages);
```

with:

```javascript
  const rows = OPTS.source === "volume"
    ? await fetchBirdeyeTopVolume(OPTS.minVolume, OPTS.maxPages)
    : await fetchBirdeyeTrending(OPTS.trendingLimit);
```

- [ ] **Step 4: Reflect the source in the summary log**

Replace the summary `console.log` (currently lines 233–236) so it names the source:

```javascript
  console.log(
    `Scanned ${rows.length} via ${OPTS.source} → ${filtered.length} passed filters → ` +
      `${survivors.length} kept (rank=${OPTS.rank}, ${order})`
  );
```

- [ ] **Step 5: Run the unit suite (regression — nothing pure changed)**

Run: `node --test scripts/scan_tokens.test.js`
Expected: PASS (unchanged set — Task 3 touches only network/integration code).

- [ ] **Step 6: Manual verification of both sources**

Run (trending, default):
```bash
node scripts/scan_tokens.js
```
Expected: summary line reads `Scanned N via trending → ... → ... kept (rank=change, change desc, band (0, 85]%)`, followed by 0–3 survivor rows each printing `chg=+NN.N% vol=$… liq=$…`.

Run (legacy volume path still works):
```bash
MOMENTUM_SCAN_SOURCE=volume node scripts/scan_tokens.js
```
Expected: summary line reads `Scanned N via volume → …`; behaves as before (paginated, per-mint change fetch).

Run (JSON contract intact):
```bash
node scripts/scan_tokens.js --json | node -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>{const a=JSON.parse(s);console.log('rows',a.length, a[0]?Object.keys(a[0]):'(none)')})"
```
Expected: prints `rows N [ 'symbol', 'mint', 'name', 'vol24', 'liq', 'change24h' ]` (or `(none)` if the band filtered everything) — the unchanged shape the Rust watcher parses.

- [ ] **Step 7: Commit**

```bash
git add scripts/scan_tokens.js
git commit -m "feat(scan): MOMENTUM_SCAN_SOURCE toggle — Birdeye trending discovery (default)"
```

---

## Self-Review

**Spec coverage:**
- Env toggle `MOMENTUM_SCAN_SOURCE=trending|volume` (default trending) → Task 3 ✓
- New `fetchBirdeyeTrending` single-call fetch → Task 3 ✓
- Floors/wash/dedup unchanged (`filterCandidates`) → untouched; preservation tested in Task 2 ✓
- Verify unchanged → untouched ✓
- Conditional `annotateChange24h` (skip for trending) → Task 2 ✓
- Change band + ranking unchanged (`rankSurvivors`) → untouched; existing tests cover it ✓
- `--json` contract unchanged → Task 3 Step 6 verifies the key set ✓
- `MOMENTUM_SCAN_TRENDING_LIMIT` (default 20) → Task 3 Step 1 ✓
- Rust watcher untouched → no Rust files in any task ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code; every test step shows assertions; commands have expected output. ✓

**Type consistency:** `mapTrendingToken` returns `{address,symbol,name,v24hUSD,liquidity,change24h}` (Task 1) — exactly the shape `filterCandidates` reads (`r.v24hUSD`, `r.liquidity`, `r.address`) and the survivor map carries `change24h` from `r.change24h` (Task 2). `needsChange` defined Task 2, used Task 2. `fetchBirdeyeTrending` maps via `mapTrendingToken` (Task 1). Consistent. ✓
