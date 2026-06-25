# Live Token Scanner (momentum discovery overlay) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every ~hour, the `portfolio-watcher` runs a Node scanner that finds liquid, Jupiter-verified, non-wash Solana tokens, and feeds the top-3-by-volume into the momentum trader **in memory** (the curated `momentum_tokens.json` is never written), so the trader ranks `curated ∪ discovered ∪ {held}`.

**Architecture:** Two components. (A) `scripts/scan_tokens.js` — Birdeye top-volume → denylist/dedup/floors/anti-wash-ratio → Jupiter-verify → emits JSON (`--json`) or appends to the file (`--apply`, manual only). (B) Watcher integration — a periodic `tokio::process` one-shot spawn parses the JSON into a rolling in-memory `discovered: Vec<WatchedToken>`, and a pure `effective_universe()` merges it (deduped, curated-first) with the held position, replacing `&watched` in both the entry and exit `MomentumContext`.

**Tech Stack:** Rust (tokio, serde, anyhow), Node ≥18 (`fetch`, `node:test`), Birdeye REST, Jupiter token-search REST.

## Global Constraints

- **Node ≥ 18** — the scanner uses global `fetch` and the test uses `node:test`. Verify with `node --version` before Task 2.
- **The live path NEVER writes `assets/momentum_tokens.json`.** Only the manual `--apply` flag writes it; the watcher only ever calls `--json`.
- **New config defaults OFF** (`MOMENTUM_SCAN_ENABLE=false`). When unset, `effective == watched` and behavior is byte-for-byte unchanged.
- **Do not edit `.env`** (only `.env.example`). **Never echo `BIRDEYE_API_KEY` or `RPC_URL`** in logs, commits, or commands.
- **Single source for Jupiter search:** `scripts/lib/jup.js`. Do not duplicate the search/verify logic.
- **Birdeye:** host `https://public-api.birdeye.so`, headers `X-API-KEY: $BIRDEYE_API_KEY` + `x-chain: solana`. Tokenlist path `/defi/tokenlist?sort_by=v24hUSD&sort_type=desc`.
- **Reuse production fns** (`backfill_watched_cold`, `build_known_price_keys`, `momentum_state::load`); do not reimplement warming or pricing.

---

### Task 1: Extract shared Jupiter helpers into `scripts/lib/jup.js`

Factor the Jupiter search/verify/constants out of `add_momentum_token.js` so the scanner reuses one tested implementation (DRY). Refactor the existing consumer and prove it still works.

**Files:**
- Create: `scripts/lib/jup.js`
- Modify: `scripts/add_momentum_token.js` (lines 34–52 — replace local constants + `search` with a require)

**Interfaces:**
- Produces: `module.exports = { USDC_MINT, JUP_HOST, MINT_RE, search, isVerifiedMint }`
  - `USDC_MINT: string`, `JUP_HOST: string`, `MINT_RE: RegExp`
  - `async search(query: string) -> Array<{id, symbol, name, isVerified, ...}>`
  - `async isVerifiedMint(mint: string) -> boolean` (fail-closed: network error ⇒ false)

- [ ] **Step 1: Create `scripts/lib/jup.js`**

```js
"use strict";
// Shared Jupiter token-search helpers. Single source of truth for the verified-token
// gate used by add_momentum_token.js (manual) and scan_tokens.js (live discovery).

const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// The token search lives on the API host root, not under /swap/v1.
const JUP_HOST = (process.env.MOMENTUM_JUPITER_API_URL || "https://lite-api.jup.ag")
  .replace(/\/swap\/v1\/?$/, "")
  .replace(/\/$/, "");

// Solana addresses are 32–44 base58 chars (no 0, O, I, l).
const MINT_RE = /^[1-9A-HJ-NP-Za-km-z]{32,44}$/;

async function search(query) {
  const url = `${JUP_HOST}/tokens/v2/search?query=${encodeURIComponent(query)}`;
  const res = await fetch(url, { headers: { accept: "application/json" } });
  if (!res.ok) throw new Error(`Jupiter token search -> HTTP ${res.status} (${url})`);
  const body = await res.json();
  return Array.isArray(body) ? body : body.tokens || [];
}

// Is `mint` a Jupiter-verified token? Searches by mint and matches the exact id.
// Fail-closed: any network/parse error returns false (an unverified token is skipped).
async function isVerifiedMint(mint) {
  try {
    const hit = (await search(mint)).find((t) => t.id === mint);
    return !!(hit && hit.isVerified);
  } catch (_) {
    return false;
  }
}

module.exports = { USDC_MINT, JUP_HOST, MINT_RE, search, isVerifiedMint };
```

- [ ] **Step 2: Refactor `add_momentum_token.js` to require the lib**

Replace lines 34–52 (the `USDC_MINT`/`TOKENS_PATH`/`JUP_HOST`/`MINT_RE` consts and the `search` function) with — keeping `TOKENS_PATH` local since it is specific to this script:

```js
const { USDC_MINT, MINT_RE, search } = require("./lib/jup");

const TOKENS_PATH =
  process.env.MOMENTUM_TOKENS_PATH ||
  path.join(__dirname, "..", "assets", "momentum_tokens.json");
```

Leave the rest of the file (`fmt`, `resolveQuery`, `resolveMint`, `loadList`, `main`) unchanged — they already call `search(...)` and use `MINT_RE`/`USDC_MINT`, now sourced from the lib.

- [ ] **Step 3: Verify the refactored tool still loads and dedups**

Run: `node scripts/add_momentum_token.js --help`
Expected: prints the usage line, exits 0 (no `require` error).

Run (dedup path, no network needed for an already-listed mint — pick any mint already in `assets/momentum_tokens.json`):
`node scripts/add_momentum_token.js <a-mint-already-in-the-file>`
Expected: `• Already in the watch list: …  — nothing to do.` and the file is unchanged (`git diff --stat assets/momentum_tokens.json` shows nothing).

- [ ] **Step 4: Commit**

```bash
git add scripts/lib/jup.js scripts/add_momentum_token.js
git commit -m "refactor(scripts): extract shared Jupiter search into scripts/lib/jup.js"
```

---

### Task 2: `scripts/scan_tokens.js` — the discovery scanner

Birdeye top-volume → pure filter (denylist/dedup/floors/anti-wash) → Jupiter-verify → emit. The pure filter is exported and unit-tested network-free; the network glue is exercised by a live run.

**Files:**
- Create: `scripts/scan_tokens.js`
- Create: `scripts/scan_tokens.test.js`

**Interfaces:**
- Consumes: `scripts/lib/jup.js` (`USDC_MINT`, `MINT_RE`, `isVerifiedMint`)
- Produces:
  - `module.exports = { filterCandidates }`
  - `filterCandidates(rows, curatedMints, opts) -> Array<row>` — PURE, network-free, deterministic. `rows: [{address, symbol, name, v24hUSD, liquidity}]`; `curatedMints: string[]`; `opts: {minVolume, minLiquidity, maxRatio}`. Returns survivors **sorted by `v24hUSD` desc**.
  - CLI: `node scripts/scan_tokens.js [--json | --apply]` — `--json` prints `[{symbol, mint, name, vol24, liq}]` to stdout (the live path); `--apply` appends new survivors to `MOMENTUM_TOKENS_PATH`; no flag prints a human table.

- [ ] **Step 1: Verify Node version**

Run: `node --version`
Expected: `v18.x` or higher (needed for `node:test` and global `fetch`). If lower, stop and flag — the rest of the task assumes ≥18.

- [ ] **Step 2: Write the failing filter test**

Create `scripts/scan_tokens.test.js`:

```js
"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { filterCandidates } = require("./scan_tokens");

// Valid base58 mints (so they pass MINT_RE).
const RAY  = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";
const BONK = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
const WIF  = "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const opts = { minVolume: 250_000, minLiquidity: 200_000, maxRatio: 30 };

const row = (address, symbol, v24hUSD, liquidity, name = "") =>
  ({ address, symbol, name, v24hUSD, liquidity });

test("rejects wash-trade tokens by the vol/liq ratio cap", () => {
  const rows = [row(BONK, "SV151", 50_000_000, 164)]; // ratio ≈ 305,000×
  assert.equal(filterCandidates(rows, [], opts).length, 0);
});

test("denylists stablecoins by mint and by symbol", () => {
  const rows = [
    row(USDC, "USDC", 9_000_000, 5_000_000),
    row(BONK, "wUSDT", 9_000_000, 5_000_000),
  ];
  assert.equal(filterCandidates(rows, [], opts).length, 0);
});

test("dedups mints already curated", () => {
  const rows = [row(RAY, "RAY", 2_000_000, 800_000)];
  assert.equal(filterCandidates(rows, [RAY], opts).length, 0);
});

test("rejects below the volume or liquidity floors", () => {
  const rows = [
    row(BONK, "LOWVOL", 100_000, 800_000),
    row(WIF, "LOWLIQ", 2_000_000, 50_000),
  ];
  assert.equal(filterCandidates(rows, [], opts).length, 0);
});

test("passes a clean liquid token and sorts survivors by volume desc", () => {
  const rows = [
    row(BONK, "BONK", 2_000_000, 800_000), // ratio 2.5
    row(WIF, "WIF", 5_000_000, 1_000_000), // ratio 5.0, higher volume
  ];
  const out = filterCandidates(rows, [], opts);
  assert.deepEqual(out.map((r) => r.symbol), ["WIF", "BONK"]);
});
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `node --test scripts/scan_tokens.test.js`
Expected: FAIL — `Cannot find module './scan_tokens'` (the module doesn't exist yet).

- [ ] **Step 4: Implement `scripts/scan_tokens.js`**

```js
#!/usr/bin/env node
"use strict";
/**
 * Generic liquid-token scanner for the momentum trader's live discovery overlay.
 *
 * Birdeye top-by-volume → drop stables/wrapped → drop already-curated → volume &
 * liquidity floors + anti-wash vol/liq ratio cap → Jupiter-verified only → emit.
 *
 * Output modes:
 *   --json    print [{symbol, mint, name, vol24, liq}] (volume-sorted) to stdout.
 *             THE LIVE PATH — the portfolio-watcher spawns `node scan_tokens.js --json`.
 *             Never writes any file.
 *   --apply   append new survivors to MOMENTUM_TOKENS_PATH (manual one-off only).
 *   (none)    human-readable table.
 *
 * Env: BIRDEYE_API_KEY (required), SCAN_MIN_VOLUME (250000), SCAN_MIN_LIQUIDITY
 * (200000), SCAN_MAX_RATIO (30), SCAN_LIMIT (100), MOMENTUM_TOKENS_PATH,
 * MOMENTUM_JUPITER_API_URL.
 */
const fs = require("fs");
const path = require("path");
const { USDC_MINT, MINT_RE, isVerifiedMint } = require("./lib/jup");

const TOKENS_PATH =
  process.env.MOMENTUM_TOKENS_PATH ||
  path.join(__dirname, "..", "assets", "momentum_tokens.json");

function numEnv(key, dflt) {
  const v = parseFloat(process.env[key]);
  return Number.isFinite(v) ? v : dflt;
}
const OPTS = {
  minVolume: numEnv("SCAN_MIN_VOLUME", 250_000),
  minLiquidity: numEnv("SCAN_MIN_LIQUIDITY", 200_000),
  maxRatio: numEnv("SCAN_MAX_RATIO", 30),
  limit: numEnv("SCAN_LIMIT", 100),
};

// Stablecoins + wrapped SOL: never momentum candidates.
const DENY_MINTS = new Set([
  USDC_MINT,
  "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
  "So11111111111111111111111111111111111111112",  // wSOL
]);
const DENY_SYM_RE = /^(w?usd|usd[ct]|usde|dai|fdusd|pyusd|eur[a-z]*|gyen|busd)/i;

/**
 * PURE filter (network-free, deterministic) — denylist + dedup-vs-curated +
 * floors + anti-wash ratio, sorted by 24h volume desc. Unit-tested.
 */
function filterCandidates(rows, curatedMints, opts) {
  const curated = new Set(curatedMints);
  return rows
    .filter((r) => r && MINT_RE.test(r.address || ""))
    .filter((r) => !DENY_MINTS.has(r.address) && !DENY_SYM_RE.test(r.symbol || ""))
    .filter((r) => !curated.has(r.address))
    .filter((r) => {
      const vol = +r.v24hUSD || 0;
      const liq = +r.liquidity || 0;
      if (vol < opts.minVolume || liq < opts.minLiquidity) return false;
      return vol / liq <= opts.maxRatio;
    })
    .sort((a, b) => (+b.v24hUSD || 0) - (+a.v24hUSD || 0));
}

async function fetchBirdeyeTopVolume(limit) {
  const key = process.env.BIRDEYE_API_KEY || "";
  if (!key) throw new Error("BIRDEYE_API_KEY is not set");
  const url =
    `https://public-api.birdeye.so/defi/tokenlist` +
    `?sort_by=v24hUSD&sort_type=desc&offset=0&limit=${limit}`;
  const res = await fetch(url, {
    headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" },
  });
  if (!res.ok) throw new Error(`Birdeye tokenlist -> HTTP ${res.status}`);
  const body = await res.json();
  const tokens = (body && body.data && body.data.tokens) || [];
  return tokens.map((t) => ({
    address: t.address,
    symbol: t.symbol || "",
    name: t.name || "",
    v24hUSD: +t.v24hUSD || 0,
    liquidity: +t.liquidity || 0,
  }));
}

function loadList() {
  if (!fs.existsSync(TOKENS_PATH)) return [];
  const raw = fs.readFileSync(TOKENS_PATH, "utf8").trim();
  if (!raw) return [];
  const parsed = JSON.parse(raw);
  if (!Array.isArray(parsed)) throw new Error(`${TOKENS_PATH} is not a JSON array`);
  return parsed;
}
const curatedMintsFromFile = () => loadList().map((e) => e.mint).filter(Boolean);

async function verifyAll(cands) {
  // Sequential to respect the public Jupiter tier's rate limit (small post-filter set).
  const out = [];
  for (const c of cands) {
    if (await isVerifiedMint(c.address)) out.push(c);
  }
  return out;
}

const fmtNum = (n) => Math.round(n).toLocaleString("en-US");

async function main() {
  const args = process.argv.slice(2);
  const asJson = args.includes("--json");
  const apply = args.includes("--apply");

  const rows = await fetchBirdeyeTopVolume(OPTS.limit);
  const filtered = filterCandidates(rows, curatedMintsFromFile(), OPTS);
  const verified = await verifyAll(filtered);
  const survivors = verified.map((r) => ({
    symbol: r.symbol, mint: r.address, name: r.name, vol24: r.v24hUSD, liq: r.liquidity,
  }));

  if (asJson) {
    process.stdout.write(JSON.stringify(survivors) + "\n");
    return;
  }
  if (apply) {
    const list = loadList();
    let added = 0;
    for (const s of survivors) {
      if (!list.some((e) => e.mint === s.mint)) {
        const e = { symbol: s.symbol, mint: s.mint };
        if (s.name) e.name = s.name;
        list.push(e);
        added++;
      }
    }
    fs.writeFileSync(TOKENS_PATH, JSON.stringify(list, null, 2) + "\n");
    console.log(`✓ appended ${added} new token(s) to ${TOKENS_PATH} (${list.length} total)`);
    return;
  }
  console.log(
    `Scanned ${rows.length} by volume → ${filtered.length} passed filters → ${survivors.length} verified`
  );
  for (const s of survivors) {
    console.log(
      `  ${s.symbol.padEnd(10)} vol=$${fmtNum(s.vol24)} liq=$${fmtNum(s.liq)} ` +
        `ratio=${(s.vol24 / Math.max(s.liq, 1)).toFixed(1)}  ${s.mint}`
    );
  }
}

module.exports = { filterCandidates };

if (require.main === module) {
  main().catch((e) => {
    console.error(`✗ ${e.message}`);
    process.exit(1);
  });
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `node --test scripts/scan_tokens.test.js`
Expected: PASS — `# pass 5  # fail 0`.

- [ ] **Step 6: Live smoke test (`--json`)**

Run (uses the real `BIRDEYE_API_KEY` from the shell env; do NOT print the key):
`node scripts/scan_tokens.js --json | head -c 2000`
Expected: a JSON array (possibly `[]` if nothing clears the filters right now) of `{symbol, mint, name, vol24, liq}` objects, volume-sorted. Also run without `--json` to eyeball the human table and confirm the filter funnel counts look sane (hundreds scanned → handful verified).

- [ ] **Step 7: Commit**

```bash
git add scripts/scan_tokens.js scripts/scan_tokens.test.js
git commit -m "feat(scripts): scan_tokens.js — Birdeye+Jupiter liquid-token scanner (--json/--apply)"
```

---

### Task 3: PortfolioConfig scan knobs + `.env.example`

Add the four `MOMENTUM_SCAN_*` config fields (default OFF) and document them plus the scanner thresholds.

**Files:**
- Modify: `src/portfolio/mod.rs:153-159` (struct fields) and `:258-260` (from_env)
- Modify: `.env.example`

**Interfaces:**
- Produces (on `PortfolioConfig`): `momentum_scan_enable: bool`, `momentum_scan_interval_secs: u64`, `momentum_scan_top_n: usize`, `momentum_scan_script: String`.

- [ ] **Step 1: Add the struct fields**

In `src/portfolio/mod.rs`, after the `momentum_pnl_path: String,` field (line 158) and before the closing `}` of the struct (line 159), add:

```rust

    // ----- Live token discovery (momentum overlay; opt-in) -----
    /// Master switch. When false, no scanning happens and the momentum universe
    /// is exactly the curated file (zero behavior change). Env `MOMENTUM_SCAN_ENABLE`.
    pub momentum_scan_enable: bool,
    /// Seconds between scans. The watcher runs `scan_tokens.js --json` this often
    /// (floored to one 60s monitor tick). Env `MOMENTUM_SCAN_INTERVAL_SECS` (3600).
    pub momentum_scan_interval_secs: u64,
    /// How many top-by-volume discoveries to keep in the rolling in-memory overlay.
    /// Env `MOMENTUM_SCAN_TOP_N` (3).
    pub momentum_scan_top_n: usize,
    /// Path to the Node scanner spawned each interval. Env `MOMENTUM_SCAN_SCRIPT`.
    pub momentum_scan_script: String,
```

- [ ] **Step 2: Add the from_env reads**

In `src/portfolio/mod.rs`, after the `momentum_pnl_path: …,` initializer (line 258-259) and before the closing `})` (line 260), add:

```rust
            momentum_scan_enable: parse_bool_env("MOMENTUM_SCAN_ENABLE", false),
            momentum_scan_interval_secs: parse_env("MOMENTUM_SCAN_INTERVAL_SECS", 3600_u64)?,
            momentum_scan_top_n: parse_env("MOMENTUM_SCAN_TOP_N", 3_usize)?,
            momentum_scan_script: std::env::var("MOMENTUM_SCAN_SCRIPT")
                .unwrap_or_else(|_| "scripts/scan_tokens.js".to_string()),
```

- [ ] **Step 3: Build to confirm the config compiles**

Run: `cargo build --bin solana-mev 2>&1 | tail -5`
Expected: builds (warnings OK; the new fields are not yet read elsewhere — that lands in Task 5). If a `dead_code` warning appears for the new fields, it is expected until Task 5 wires them.

- [ ] **Step 4: Document in `.env.example`**

Read `.env.example`, find the last `MOMENTUM_*` line (e.g. `MOMENTUM_PNL_PATH=...`), and insert this block after it:

```bash

# ----- Live token discovery (momentum overlay; opt-in, default off) -----
# Periodically scan Birdeye top-volume ∩ Jupiter-verified for liquid tokens and feed
# the top-N into the momentum universe IN MEMORY (assets/momentum_tokens.json untouched).
# Transient: replaced each scan, reset on restart. Honest caveat: this is a curation
# heuristic (finds tradeable names), not a momentum edge.
MOMENTUM_SCAN_ENABLE=false
MOMENTUM_SCAN_INTERVAL_SECS=3600
MOMENTUM_SCAN_TOP_N=3
MOMENTUM_SCAN_SCRIPT=scripts/scan_tokens.js
# Scanner filter thresholds (read by scan_tokens.js):
SCAN_MIN_VOLUME=250000
SCAN_MIN_LIQUIDITY=200000
SCAN_MAX_RATIO=30
SCAN_LIMIT=100
```

- [ ] **Step 5: Commit**

```bash
git add src/portfolio/mod.rs .env.example
git commit -m "feat(momentum): MOMENTUM_SCAN_* config (off by default) + .env.example"
```

---

### Task 4: Watcher pure helpers + unit tests

Add the four watcher-private helpers and their tests. They compile and are tested here but are not yet wired into `run()` (that is Task 5), so this task is independently reviewable.

**Files:**
- Modify: `src/portfolio/watcher.rs:1` (imports — add `HashSet`, `anyhow::Context`)
- Modify: `src/portfolio/watcher.rs` (add helpers near the other free fns, e.g. after `build_known_price_keys`, ~line 656)
- Modify: `src/portfolio/watcher.rs` (tests in the existing `#[cfg(test)] mod tests`, ~line 962)

**Interfaces:**
- Produces:
  - `struct ScanCandidate { symbol: String, mint: String, name: Option<String> }` (serde Deserialize; ignores extra JSON fields)
  - `async fn run_token_scan(script: &str, top_n: usize) -> anyhow::Result<Vec<WatchedToken>>`
  - `fn effective_universe(curated: &[WatchedToken], discovered: &[WatchedToken], held: Option<&WatchedToken>) -> Vec<WatchedToken>`
  - `fn discovered_changed(old: &[WatchedToken], new: &[WatchedToken]) -> bool`
  - `fn held_token(cfg: &PortfolioConfig) -> Option<WatchedToken>`

- [ ] **Step 1: Update imports**

In `src/portfolio/watcher.rs`, change line 1:

```rust
use std::collections::{HashMap, HashSet, VecDeque};
```

and add, after the `use std::time::...;` line (line 3):

```rust
use anyhow::Context;
```

- [ ] **Step 2: Write the failing tests**

In the `#[cfg(test)] mod tests` block in `src/portfolio/watcher.rs` (after the existing `holdings_changed_detects_relevant_moves` test, before the closing `}`), add:

```rust
    fn wt(sym: &str, mint: &str) -> WatchedToken {
        WatchedToken { symbol: sym.into(), mint: mint.into(), name: None, equity: None }
    }

    #[test]
    fn effective_universe_dedups_curated_first() {
        let curated = vec![wt("RAY", "mRAY"), wt("JUP", "mJUP")];
        let discovered = vec![wt("RAY2", "mRAY"), wt("BONK", "mBONK")]; // mRAY is a dup
        let eff = effective_universe(&curated, &discovered, None);
        let mints: Vec<&str> = eff.iter().map(|w| w.mint.as_str()).collect();
        assert_eq!(mints, vec!["mRAY", "mJUP", "mBONK"]);
        assert_eq!(eff[0].symbol, "RAY", "curated entry wins the dup");
    }

    #[test]
    fn effective_universe_retains_and_dedups_held() {
        let curated = vec![wt("RAY", "mRAY")];
        let discovered = vec![wt("BONK", "mBONK")];
        // Held token absent from both → retained.
        let held = wt("WIF", "mWIF");
        let eff = effective_universe(&curated, &discovered, Some(&held));
        assert_eq!(eff.len(), 3);
        assert!(eff.iter().any(|w| w.mint == "mWIF"));
        // Held token already present → not duplicated.
        let held2 = wt("RAY", "mRAY");
        let eff2 = effective_universe(&curated, &discovered, Some(&held2));
        assert_eq!(eff2.len(), 2);
    }

    #[test]
    fn effective_universe_empty_discovered_equals_curated() {
        let curated = vec![wt("RAY", "mRAY"), wt("JUP", "mJUP")];
        let eff = effective_universe(&curated, &[], None);
        assert_eq!(eff.len(), 2);
    }

    #[test]
    fn discovered_changed_is_mint_set_aware() {
        let a = vec![wt("RAY", "mRAY"), wt("BONK", "mBONK")];
        let b = vec![wt("BONK", "mBONK"), wt("RAY", "mRAY")]; // same set, reordered
        assert!(!discovered_changed(&a, &b));
        let c = vec![wt("RAY", "mRAY"), wt("WIF", "mWIF")];
        assert!(discovered_changed(&a, &c));
        assert!(discovered_changed(&a, &a[..1]), "different length");
    }

    #[test]
    fn scan_candidate_parses_and_take_n_maps_to_watched() {
        let json = r#"[
            {"symbol":"AAA","mint":"mAAA","name":"Alpha","vol24":9.0,"liq":1.0},
            {"symbol":"BBB","mint":"mBBB","vol24":8.0,"liq":1.0},
            {"symbol":"CCC","mint":"mCCC","vol24":7.0,"liq":1.0}
        ]"#;
        let cands: Vec<ScanCandidate> = serde_json::from_str(json).unwrap();
        let top: Vec<WatchedToken> = cands.into_iter().take(2)
            .map(|c| WatchedToken { symbol: c.symbol, mint: c.mint, name: c.name, equity: None })
            .collect();
        assert_eq!(top.len(), 2);
        assert_eq!((top[0].symbol.as_str(), top[0].name.as_deref()), ("AAA", Some("Alpha")));
        assert_eq!(top[1].mint, "mBBB");
        assert!(top[1].name.is_none(), "missing name → None, extra fields ignored");
    }
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --bin solana-mev watcher 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function effective_universe` / `cannot find type ScanCandidate` (helpers don't exist yet).

- [ ] **Step 4: Implement the helpers**

In `src/portfolio/watcher.rs`, after `build_known_price_keys` (ends ~line 656), add:

```rust
/// One row of `scan_tokens.js --json`. Extra fields (vol24, liq) are ignored —
/// the script already volume-sorted, so the watcher only needs identity.
#[derive(Debug, serde::Deserialize)]
struct ScanCandidate {
    symbol: String,
    mint: String,
    #[serde(default)]
    name: Option<String>,
}

/// Spawn `node <script> --json`, parse stdout, and return the top-`top_n` rows as
/// watch entries. Best-effort: the caller logs any Err and keeps the prior set.
async fn run_token_scan(script: &str, top_n: usize) -> anyhow::Result<Vec<WatchedToken>> {
    let out = tokio::process::Command::new("node")
        .arg(script)
        .arg("--json")
        .output()
        .await
        .with_context(|| format!("failed to spawn `node {script} --json`"))?;
    if !out.status.success() {
        anyhow::bail!(
            "scan exited {}: {}",
            out.status.code().map_or_else(|| "signal".to_string(), |c| c.to_string()),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let cands: Vec<ScanCandidate> = serde_json::from_slice(&out.stdout)
        .context("scan stdout was not a JSON array of {symbol,mint,name,...}")?;
    Ok(cands
        .into_iter()
        .take(top_n)
        .map(|c| WatchedToken { symbol: c.symbol, mint: c.mint, name: c.name, equity: None })
        .collect())
}

/// Effective momentum universe = curated ∪ discovered ∪ {held}, deduped by mint
/// (curated wins, then discovered, then the held token). The held clause keeps a
/// position in a discovered name rankable after it rolls off the top-N.
fn effective_universe(
    curated: &[WatchedToken],
    discovered: &[WatchedToken],
    held: Option<&WatchedToken>,
) -> Vec<WatchedToken> {
    let mut out: Vec<WatchedToken> = Vec::with_capacity(curated.len() + discovered.len() + 1);
    let mut seen: HashSet<&str> = HashSet::new();
    for w in curated.iter().chain(discovered.iter()).chain(held) {
        if seen.insert(w.mint.as_str()) {
            out.push(w.clone());
        }
    }
    out
}

/// True if two discovered sets differ as mint sets (order-independent) — gates the
/// warm/log work so an unchanged hourly scan is a no-op.
fn discovered_changed(old: &[WatchedToken], new: &[WatchedToken]) -> bool {
    if old.len() != new.len() {
        return true;
    }
    let olds: HashSet<&str> = old.iter().map(|w| w.mint.as_str()).collect();
    new.iter().any(|w| !olds.contains(w.mint.as_str()))
}

/// The momentum trader's currently-held token (if any), read from its state file,
/// as a watch entry — so the rolling overlay never orphans an open position.
/// `name`/`equity` are unknown here (`None`); the exit path doesn't need them.
fn held_token(cfg: &PortfolioConfig) -> Option<WatchedToken> {
    super::momentum_state::load(Path::new(&cfg.momentum_state_path))
        .ok()
        .and_then(|s| s.position)
        .map(|p| WatchedToken { symbol: p.symbol, mint: p.mint, name: None, equity: None })
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --bin solana-mev watcher 2>&1 | tail -20`
Expected: PASS — the 5 new tests plus the existing `holdings_changed` test. `run_token_scan`/`held_token` compile but are unused → expect `dead_code` warnings; they are consumed in Task 5.

- [ ] **Step 6: Commit**

```bash
git add src/portfolio/watcher.rs
git commit -m "feat(momentum): watcher discovery helpers (effective_universe, run_token_scan) + tests"
```

---

### Task 5: Wire the discovery overlay into the watcher loop

Maintain `discovered`/`effective`, run the scan on a tick counter, warm new entrants, and route both `MomentumContext`s through `effective`.

**Files:**
- Modify: `src/portfolio/watcher.rs` — state vars (~after line 239), fast-exit arm (~line 316), rescan union (~line 365), scan block (~after line 384), effective recompute + entry ctx (~line 465-471).

**Interfaces:**
- Consumes: `effective_universe`, `discovered_changed`, `held_token`, `run_token_scan` (Task 4); `backfill_watched_cold`, `build_known_price_keys` (existing).

- [ ] **Step 1: Add the overlay state vars**

In `src/portfolio/watcher.rs`, after `let mut ticks_since_rescan = 0u32;` (line 239), add:

```rust

    // Live token discovery overlay (momentum only; opt-in). `discovered` is the
    // rolling top-N from scan_tokens.js; `effective` = curated ∪ discovered ∪ held,
    // recomputed each monitor tick and shared by the entry + fast-exit paths. When
    // scanning is off, `effective` stays equal to `watched` (zero behavior change).
    let mut discovered: Vec<WatchedToken> = Vec::new();
    let mut effective: Vec<WatchedToken> = watched.clone();
    // Scan cadence in 60s monitor ticks (floored to 1 so a tiny interval can't div to 0).
    let scan_every_ticks = (cfg.momentum_scan_interval_secs / 60).max(1);
    // Pre-armed so the first eligible monitor tick scans (warm start), then hourly.
    let mut ticks_since_scan = scan_every_ticks;
```

- [ ] **Step 2: Route the fast EXIT path through `effective`**

In the `fast_ticker.tick()` arm, change the `MomentumContext` construction (line ~316):

```rust
                        let mctx = MomentumContext {
                            cfg: &cfg, watched: &effective, prices_usd: &last_prices,
                            history: &history, decimals: &decimals, http: &http,
                            usdc_balance: usdc_balance(&portfolio),
                        };
```

(only `watched: &watched` → `watched: &effective`).

- [ ] **Step 3: Fold `discovered` into the rescan pricing union**

In the periodic wallet re-scan block, change the union loop (line 365):

```rust
                    // Re-union watched + pairs + discovered mints so a re-scan doesn't drop them.
                    for w in watched.iter().chain(pairs_mints.iter()).chain(discovered.iter()) {
                        if !token_mints.contains(&w.mint) {
                            token_mints.push(w.mint.clone());
                        }
                    }
```

(only the iterator gains `.chain(discovered.iter())` and the comment updates).

- [ ] **Step 4: Add the periodic scan block**

In `src/portfolio/watcher.rs`, immediately after the wallet re-scan block closes (after line 384, the `}` ending `if ticks_since_rescan >= 5 { … }`) and before the price-fetch comment (line 386), insert:

```rust

        // Periodic generic token scan → rolling in-memory top-N discovery overlay
        // (momentum only; opt-in). One-shot `node scan_tokens.js --json`; best-effort —
        // a failed/slow scan logs and keeps the prior `discovered`. Curated file untouched.
        if cfg.enable_momentum_trader && cfg.momentum_scan_enable {
            ticks_since_scan += 1;
            if ticks_since_scan >= scan_every_ticks {
                ticks_since_scan = 0;
                match run_token_scan(&cfg.momentum_scan_script, cfg.momentum_scan_top_n).await {
                    Ok(found) => {
                        if discovered_changed(&discovered, &found) {
                            discovered = found;
                            let syms: Vec<&str> = discovered.iter().map(|w| w.symbol.as_str()).collect();
                            info!("momentum: scan → discovered {:?}", syms);
                            // Warm cold new entrants so they are rankable immediately
                            // (no-op for mints already warm/held).
                            if let Some(api_key) = &cfg.birdeye_api_key {
                                backfill_watched_cold(
                                    &http, api_key, &discovered,
                                    cfg.momentum_lookback_obs, &mut history, &history_path,
                                ).await;
                            }
                            // Fold discovered mints into the priced set for this tick onward.
                            for w in &discovered {
                                if !token_mints.contains(&w.mint) {
                                    token_mints.push(w.mint.clone());
                                }
                            }
                            known_price_keys = build_known_price_keys(&token_mints);
                        } else {
                            info!("momentum: scan → no change ({} discovered)", discovered.len());
                        }
                    }
                    Err(e) => warn!("momentum: token scan failed ({e}); keeping {} discovered", discovered.len()),
                }
            }
        }
```

- [ ] **Step 5: Recompute `effective` and route the ENTRY path through it**

In `src/portfolio/watcher.rs`, in the momentum ENTRY block (line 465-479), insert the recompute just inside the `if cfg.enable_momentum_trader {` (before the `let outcome = {`), and change the context's `watched`:

```rust
        if cfg.enable_momentum_trader {
            // Refresh the effective universe (curated ∪ discovered ∪ held) so this
            // tick's ranking — and the fast exit arm until the next tick — see the
            // current overlay. Skipped when scanning is off (effective == watched).
            if cfg.momentum_scan_enable {
                effective = effective_universe(&watched, &discovered, held_token(&cfg).as_ref());
            }
            let outcome = {
                let mctx = MomentumContext {
                    cfg: &cfg, watched: &effective, prices_usd: &prices,
                    history: &history, decimals: &decimals, http: &http,
                    usdc_balance: usdc_balance(&portfolio),
                };
                momentum::maybe_enter(&mctx).await
            };
```

(the new `if cfg.momentum_scan_enable { … }` block, and `watched: &watched` → `watched: &effective`).

- [ ] **Step 6: Build and run the full watcher + momentum test suite**

Run: `cargo build --bin solana-mev 2>&1 | tail -5`
Expected: builds clean (the Task 4 `dead_code` warnings are gone now that the helpers are used).

Run: `cargo test --bin solana-mev watcher 2>&1 | tail -20` and `cargo test --bin solana-mev momentum 2>&1 | tail -20`
Expected: all PASS (no regression in momentum behavior — `effective == watched` when scan is disabled, which all existing tests exercise).

- [ ] **Step 7: Commit**

```bash
git add src/portfolio/watcher.rs
git commit -m "feat(momentum): wire live discovery overlay into the watcher loop"
```

---

### Task 6: End-to-end verification + docs

Prove the integrated path works against a live (paper) run and document the feature.

**Files:**
- Modify: `CLAUDE.md` (Strategy research section — one note on live discovery)

- [ ] **Step 1: Full clippy + test sweep**

Run: `cargo clippy --bin solana-mev 2>&1 | tail -15` and `cargo test --bin solana-mev 2>&1 | tail -15`
Expected: no new clippy warnings in `watcher.rs`/`mod.rs`; all tests pass.

- [ ] **Step 2: Live integration smoke (paper, short)**

Run a brief paper session with the scanner on and a fast interval so a scan fires quickly (the env is set inline for this one run; do NOT write it to `.env`):

```bash
MOMENTUM_SCAN_ENABLE=true MOMENTUM_SCAN_INTERVAL_SECS=60 ENABLE_MOMENTUM_TRADER=true \
DRY_RUN_MOMENTUM_TRADER=true \
timeout 150 cargo run --release --bin solana-mev 2>&1 | grep -E "momentum: scan|discovered|token scan failed" | head
```

Expected: within ~2 min, a `momentum: scan → discovered [...]` (or `→ no change`) line. If `token scan failed`, read the error — most likely `node` not on PATH or `BIRDEYE_API_KEY` unset; both are environment issues, not code. Confirm the curated file is untouched: `git diff --stat assets/momentum_tokens.json` shows nothing.

- [ ] **Step 3: Document in `CLAUDE.md`**

In `CLAUDE.md`, in the "Strategy research & the pairs trader" section (or the momentum trader area), add a short paragraph after the momentum-sim bullet:

```markdown
- **Live token discovery** (opt-in, `MOMENTUM_SCAN_ENABLE`) — when the momentum
  trader is live, the watcher runs `scripts/scan_tokens.js --json` every
  `MOMENTUM_SCAN_INTERVAL_SECS` (~hourly) to find liquid, Jupiter-verified,
  non-wash tokens (Birdeye top-volume ∩ verified, minus stables/wrapped, with
  volume/liquidity floors and a vol/liq ratio cap). The **top-3 by 24h volume**
  are held **in memory** and ranked alongside the curated list (`curated ∪
  discovered ∪ held`); `assets/momentum_tokens.json` is never written by this path.
  It is a curation heuristic (broadens *what's watched*), not a momentum edge.
  Manual one-off: `node scripts/scan_tokens.js --apply` appends survivors to the file.
```

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: live token discovery overlay for the momentum trader"
```

---

## Self-Review

**Spec coverage** (against `docs/superpowers/specs/2026-06-26-token-scan-design.md`):
- Component A (scan_tokens.js): source/denylist/dedup/floors+ratio/verify/`--json`/`--apply`/thresholds → Task 2. ✓
- `scripts/lib/jup.js` shared helper → Task 1. ✓
- Component B config (`momentum_scan_{enable,interval_secs,top_n,script}`) → Task 3. ✓
- Periodic run (tick counter, one-shot `node … --json`, best-effort) → Task 5 Step 4. ✓
- In-memory `discovered` top-N, replace each scan, never persisted → Task 5 Step 4. ✓
- Effective universe (curated ∪ discovered ∪ held, deduped) → Task 4 `effective_universe` + Task 5 Step 5. ✓
- Warming new entrants via `backfill_watched_cold` + pricing union → Task 5 Step 4. ✓
- Logging per scan → Task 5 Step 4. ✓
- Testing (pure filter; effective_universe dedup/retain/cap) → Task 2 Step 2, Task 4 Step 2. ✓
- `.env.example` knobs (default off) → Task 3 Step 4. ✓

**Placeholder scan:** No TBD/TODO; every code step shows full code. ✓

**Type consistency:** `WatchedToken { symbol, mint, name, equity }` used identically in mod/watcher/tests; `ScanCandidate` fields (symbol/mint/name) match the JS `--json` output keys (the JS also emits vol24/liq, which serde ignores). `effective_universe`/`discovered_changed`/`held_token`/`run_token_scan` signatures match between Task 4 (definition) and Task 5 (call sites). `momentum_scan_*` field names identical across mod.rs, watcher.rs, and `.env.example`. ✓

**Risk note:** Task 5 changes the live `run()` loop. The safety rests on `effective == watched` whenever `MOMENTUM_SCAN_ENABLE=false` (default), so existing momentum tests are an unchanged-behavior guard. The one new always-on cost is `watched.clone()` once at startup — negligible.
