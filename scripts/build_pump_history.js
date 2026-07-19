#!/usr/bin/env node
/**
 * build_pump_history.js — build a self-contained 1-minute history file for the
 * CURATED momentum tokens (assets/momentum_tokens.json), for micro-backtesting
 * young pump.fun launches whose data falls inside the live window (where
 * backfill_history.js can't splice them — see the mid-window limitation).
 *
 * For each curated entry with a `pool` field it pages GT 1m USD closes back
 * --days (default 7), plus SOL (for the regime gate), onto one minute grid.
 * Output: assets/price_history.pump.jsonl (own file; live files never touched).
 *
 * Usage: node scripts/build_pump_history.js [--days N]
 * Generalizes the earlier one-off build_agamemnon_history.js.
 */
"use strict";
const fs = require("fs");
const path = require("path");

const GECKO = "https://api.geckoterminal.com/api/v2";
const SOL_MINT = "So11111111111111111111111111111111111111112";
const SOL_POOL = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"; // deep SOL/USDC (pinned; see memory)
const TOKENS_PATH = path.join(__dirname, "..", "assets", "momentum_tokens.json");
const OUT = path.join(__dirname, "..", "assets", "price_history.pump.jsonl");
const PAGE_PAUSE_MS = 2_100;

const argVal = (f, d) => { const i = process.argv.indexOf(f); return i >= 0 ? process.argv[i + 1] : d; };
const DAYS = parseInt(argVal("--days", "7"), 10);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function gecko(url, tries = 6) {
  for (let i = 0; ; i++) {
    const res = await fetch(url, { headers: { accept: "application/json" } });
    if (res.ok) return res.json();
    if ((res.status === 429 || res.status >= 500) && i < tries - 1) {
      await sleep(Math.min(2 ** i * 5, 60) * 1000);
      continue;
    }
    throw new Error(`GeckoTerminal ${res.status}`);
  }
}

/** Page 1m closes for a pool back to fromTs. Returns Map<minuteSecTs, close>. */
async function candles(pool, label, fromTs) {
  const byTs = new Map();
  let before = Math.floor(Date.now() / 1000) + 60;
  const maxPages = Math.ceil((DAYS * 1440) / 1000) + 3;
  for (let page = 0; page < maxPages; page++) {
    if (page) await sleep(PAGE_PAUSE_MS);
    const url = `${GECKO}/networks/solana/pools/${pool}/ohlcv/minute?aggregate=1&limit=1000&currency=usd&before_timestamp=${before}`;
    let list;
    try { list = (await gecko(url))?.data?.attributes?.ohlcv_list || []; }
    catch (e) { console.warn(`  ${label}: page ${page} failed (${e.message}) — keeping ${byTs.size}`); break; }
    if (!list.length) break;
    let oldest = Infinity;
    for (const r of list) {
      if (typeof r[0] === "number" && typeof r[4] === "number" && r[4] > 0) byTs.set(r[0], r[4]);
      if (r[0] < oldest) oldest = r[0];
    }
    process.stdout.write(`\r  ${label}: ${byTs.size} candles, back to ${new Date(oldest * 1000).toISOString().slice(0, 16)}   `);
    if (!isFinite(oldest) || oldest <= fromTs || oldest >= before) break;
    before = oldest;
  }
  console.log("");
  return byTs;
}

// Same isolated-glitch filter as backfill_history.js (>20% jump-and-revert).
function dropGlitches(byTs, label) {
  const e = [...byTs.entries()].sort((a, b) => a[0] - b[0]);
  let n = 0;
  for (let i = 1; i + 1 < e.length; i++) {
    const p0 = e[i - 1][1], [ts, p1] = e[i], p2 = e[i + 1][1];
    const jump = p1 / p0 - 1, revert = p2 / p1 - 1;
    if (Math.abs(jump) > 0.20 && jump * revert < 0 && Math.abs(revert) > Math.abs(jump) / 2) { byTs.delete(ts); n++; }
  }
  if (n) console.log(`  ${label}: dropped ${n} glitch print(s)`);
}

(async () => {
  const watched = JSON.parse(fs.readFileSync(TOKENS_PATH, "utf8")).filter((t) => t.pool);
  if (!watched.length) throw new Error("no curated tokens with a pool field");
  const fromTs = Math.floor(Date.now() / 1000) - DAYS * 86_400;
  console.log(`Building ${DAYS}d pump micro-history for ${watched.length} token(s) + SOL…`);

  const grid = new Map(); // minuteSecTs -> {mint: px}
  const put = (ts, key, px) => { let r = grid.get(ts); if (!r) { r = {}; grid.set(ts, r); } r[key] = px; };

  for (const t of watched) {
    const s = await candles(t.pool, t.symbol, fromTs);
    dropGlitches(s, t.symbol);
    for (const [ts, px] of s) put(ts, t.mint, px);
    await sleep(PAGE_PAUSE_MS);
  }
  const sol = await candles(SOL_POOL, "SOL", fromTs);
  dropGlitches(sol, "SOL");
  for (const [ts, px] of sol) { put(ts, "SOL", px); put(ts, SOL_MINT, px); }

  const rows = [...grid.entries()].sort((a, b) => a[0] - b[0]).map(([ts, prices]) => ({ ts, prices }));
  fs.writeFileSync(OUT, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
  for (const t of watched) {
    const n = rows.filter((r) => t.mint in r.prices).length;
    console.log(`  ${t.symbol}: ${n} snapshots`);
  }
  console.log(`Wrote ${rows.length} snapshots → ${OUT}`);
})().catch((e) => { console.error("failed:", e.message); process.exit(1); });
