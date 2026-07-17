#!/usr/bin/env node
/**
 * merge_febu.js — one-off: inject febu's 1-minute USD closes into the EXISTING
 * extended history. Unlike backfill_history.js (which only splices candles OLDER
 * than the live file's first snapshot), this attaches febu prices onto snapshots
 * that already exist in the file — needed because febu launched (2026-07-08) AFTER
 * the live recorder started, so its early data falls inside the live window.
 *
 * Only ADDS prices[FEBU_MINT] where absent; never overwrites live febu prices and
 * never touches any other token. Same fetch as backfill_history.js: currency=usd,
 * pinned pool, isolated-glitch filter.
 */
"use strict";
const fs = require("fs");
const path = require("path");

const GECKO = "https://api.geckoterminal.com/api/v2";
const FEBU_MINT = "4ko5tSr5o3H4v1sFtjTSd9MPUW7yx5AFCpkNPoL6pump";
const FEBU_POOL = "68nVMrVPyxGJGbGH2P92E93SYhJcbe6QociZrqoqdjcB";
const FILE = path.join(__dirname, "..", "assets", "price_history.extended.jsonl");
const DAYS = 14; // febu is ~10d old; 14 covers it with margin
const PAGE_PAUSE_MS = 2_100;
const GLITCH_PCT = 0.20;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function gecko(url, tries = 6) {
  for (let i = 0; ; i++) {
    const res = await fetch(url, { headers: { accept: "application/json" } });
    if (res.ok) return res.json();
    if ((res.status === 429 || res.status >= 500) && i < tries - 1) {
      const ra = parseInt(res.headers.get("retry-after") || "", 10);
      const wait = Math.max(isFinite(ra) ? ra : 0, Math.min(2 ** i * 5, 60)) * 1000;
      console.warn(`  GT ${res.status} — backing off ${wait / 1000}s`);
      await sleep(wait);
      continue;
    }
    throw new Error(`GeckoTerminal ${res.status}`);
  }
}

async function fetchFebu(fromTs, toTs) {
  const byTs = new Map();
  let before = toTs;
  const maxPages = Math.ceil(((toTs - fromTs) / 60_000 / 1000) * 1.5) + 5;
  for (let page = 0; page < maxPages; page++) {
    if (page) await sleep(PAGE_PAUSE_MS);
    const url = `${GECKO}/networks/solana/pools/${FEBU_POOL}/ohlcv/minute` +
      `?aggregate=1&limit=1000&currency=usd&before_timestamp=${Math.floor(before / 1000)}`;
    const list = (await gecko(url))?.data?.attributes?.ohlcv_list || [];
    if (!list.length) break;
    let oldest = Infinity;
    for (const row of list) {
      const ts = row[0] * 1000, close = row[4];
      if (typeof row[0] === "number" && typeof close === "number" && close > 0) byTs.set(ts, close);
      if (row[0] * 1000 < oldest) oldest = row[0] * 1000;
    }
    process.stdout.write(`\r  febu ${byTs.size} candles, back to ${new Date(oldest).toISOString().slice(0, 16)}   `);
    if (!isFinite(oldest) || oldest <= fromTs || oldest >= before) break;
    before = oldest;
  }
  console.log("");
  return byTs;
}

function dropGlitches(byTs) {
  const e = [...byTs.entries()].sort((a, b) => a[0] - b[0]);
  let dropped = 0;
  for (let i = 1; i + 1 < e.length; i++) {
    const p0 = e[i - 1][1], [ts, p1] = e[i], p2 = e[i + 1][1];
    const jump = p1 / p0 - 1, revert = p2 / p1 - 1;
    if (Math.abs(jump) > GLITCH_PCT && jump * revert < 0 && Math.abs(revert) > Math.abs(jump) / 2) {
      byTs.delete(ts); dropped++;
    }
  }
  if (dropped) console.log(`  dropped ${dropped} isolated glitch print(s)`);
}

(async () => {
  const now = Date.now();
  const series = await fetchFebu(now - DAYS * 86_400_000, now);
  dropGlitches(series);
  if (!series.size) throw new Error("no febu candles fetched");
  // minute-floor (seconds) -> close
  const byMin = new Map();
  for (const [ms, px] of series) byMin.set(Math.floor(ms / 1000 / 60) * 60, px);
  const febuTs = [...byMin.keys()];
  const lo = Math.min(...febuTs), hi = Math.max(...febuTs);
  console.log(`febu candles: ${byMin.size}, span ${new Date(lo*1000).toISOString().slice(0,16)} -> ${new Date(hi*1000).toISOString().slice(0,16)}`);

  const lines = fs.readFileSync(FILE, "utf8").split("\n").filter((l) => l.trim());
  let added = 0, already = 0, snapsInRange = 0;
  const out = lines.map((l) => {
    let s; try { s = JSON.parse(l); } catch { return l; }
    if (s.ts < lo - 60 || s.ts > hi + 60) return l;
    snapsInRange++;
    if (FEBU_MINT in s.prices) { already++; return l; }
    const min = Math.floor(s.ts / 60) * 60;
    const px = byMin.get(min) ?? byMin.get(min - 60) ?? byMin.get(min + 60);
    if (px == null) return l;
    s.prices[FEBU_MINT] = px;
    added++;
    return JSON.stringify(s);
  });
  fs.writeFileSync(FILE, out.join("\n") + "\n");
  console.log(`snapshots in febu range: ${snapsInRange} | febu added: ${added} | already had febu: ${already}`);
})().catch((e) => { console.error(`\nmerge failed: ${e.message}`); process.exit(1); });
