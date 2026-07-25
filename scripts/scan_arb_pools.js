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
  // "meteora_dlmm" (not "dlmm") — matches fetch_meteora_dlmm.js's output and every DLMM
  // entry in pools.json; the wrong string here meant this check never fired for DLMM.
  clPools: new Set(["raydium_clmm", "orca_whirlpool", "meteora_dlmm"]),
};

function validateBook(pools, opts = {}) {
  const pumpTradeable = opts.pumpTradeable === true;
  const errors = [];
  if (!Array.isArray(pools) || pools.length === 0) {
    return { ok: false, errors: ["book is empty — refusing to write"] };
  }
  const seen = new Set();
  for (const p of pools) {
    const tag = (p && p.id) || "<no id>";
    for (const f of REQUIRED.base) if (!p[f]) errors.push(`${tag}: missing ${f}`);
    if (REQUIRED.clPools.has(p.dex) && !p.state_account) errors.push(`${tag}: missing state_account (CL pool)`);
    // Only a TRADEABLE pump pool needs coin_creator (required by the swap builder); a
    // pricing-only pump pool (ENABLE_PUMPSWAP_TRADING=false, e.g. the momentum watcher's
    // pinned feed) legitimately ships with extra:{} — mirrors check_extra in
    // src/dex/mod.rs, which only enforces this when the pool is actually loaded to trade.
    if (p.dex === "pump_swap" && pumpTradeable && !(p.extra && p.extra.pumpswap_coin_creator)) {
      errors.push(`${tag}: missing extra.pumpswap_coin_creator (tradeable pump_swap)`);
    }
    if (seen.has(p.id)) errors.push(`${tag}: duplicate pool id`);
    seen.add(p.id);
  }
  return { ok: errors.length === 0, errors };
}

// Recursively rebuild an object with its keys sorted at EVERY nesting depth — unlike
// JSON.stringify(v, Object.keys(v).sort()), which applies that top-level key whitelist
// recursively and so silently collapses nested objects with different key sets (e.g. a
// pool's `extra`) down to `{}`, dropping their content from the comparison entirely.
function sortKeysDeep(v) {
  if (Array.isArray(v)) return v.map(sortKeysDeep);
  if (v && typeof v === "object") {
    const out = {};
    for (const k of Object.keys(v).sort()) out[k] = sortKeysDeep(v[k]);
    return out;
  }
  return v;
}

function bookChanged(oldPools, newPools) {
  const canon = (pools) =>
    JSON.stringify([...pools].map(sortKeysDeep).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)));
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
  const v = validateBook(next, { pumpTradeable: CFG.pumpTradeable });
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
