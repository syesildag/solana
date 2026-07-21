#!/usr/bin/env node
/**
 * One-off: append the missing JitoSOL+SOL tail (GT 1m closes, pinned deep pools)
 * onto assets/price_history.extended.jsonl, writing a new file
 * assets/price_history.jitosol150.jsonl — the extended file itself is not touched.
 * Needed because JitoSOL left the live watch list on 2026-07-19, so the live
 * recorder no longer carries it; GT fills the gap from the extended file's last
 * row to now. Same glitch filter as backfill_history.js.
 */
"use strict";
const fs = require("fs");
const path = require("path");

const GECKO = "https://api.geckoterminal.com/api/v2";
const JITO_MINT = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn";
const JITO_POOL = "2uoKbPEidR7KAMYtY4x7xdkHXWqYib5k4CutJauSL3Mc"; // deep pool — ALWAYS pin for JitoSOL
const SOL_MINT = "So11111111111111111111111111111111111111112";
const SOL_POOL = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2";
const SRC = path.join(__dirname, "..", "assets", "price_history.extended.jsonl");
const OUT = path.join(__dirname, "..", "assets", "price_history.jitosol150.jsonl");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function gecko(url, tries = 6) {
  for (let i = 0; ; i++) {
    const res = await fetch(url, { headers: { accept: "application/json" } });
    if (res.ok) return res.json();
    if ((res.status === 429 || res.status >= 500) && i < tries - 1) {
      await sleep(Math.min(2 ** i * 5, 60) * 1000);
      continue;
    }
    throw new Error(`GT ${res.status}`);
  }
}

async function candles(pool, label, fromTs) {
  const byTs = new Map();
  let before = Math.floor(Date.now() / 1000) + 60;
  for (let page = 0; page < 12; page++) {
    if (page) await sleep(2100);
    const list = (await gecko(`${GECKO}/networks/solana/pools/${pool}/ohlcv/minute?aggregate=1&limit=1000&currency=usd&before_timestamp=${before}`))?.data?.attributes?.ohlcv_list || [];
    if (!list.length) break;
    let oldest = Infinity;
    for (const r of list) {
      if (typeof r[0] === "number" && typeof r[4] === "number" && r[4] > 0) byTs.set(r[0], r[4]);
      if (r[0] < oldest) oldest = r[0];
    }
    if (oldest <= fromTs || oldest >= before) break;
    before = oldest;
  }
  console.log(`  ${label}: ${byTs.size} tail candles`);
  return byTs;
}

function dropGlitches(byTs) {
  const e = [...byTs.entries()].sort((a, b) => a[0] - b[0]);
  let n = 0;
  for (let i = 1; i + 1 < e.length; i++) {
    const p0 = e[i - 1][1], [ts, p1] = e[i], p2 = e[i + 1][1];
    const jump = p1 / p0 - 1, revert = p2 / p1 - 1;
    if (Math.abs(jump) > 0.20 && jump * revert < 0 && Math.abs(revert) > Math.abs(jump) / 2) { byTs.delete(ts); n++; }
  }
  if (n) console.log(`  dropped ${n} glitch print(s)`);
}

(async () => {
  const lines = fs.readFileSync(SRC, "utf8").split("\n").filter((l) => l.trim());
  const lastTs = JSON.parse(lines[lines.length - 1]).ts;
  console.log(`extended ends ${new Date(lastTs * 1000).toISOString()} — fetching tail…`);

  const jito = await candles(JITO_POOL, "JitoSOL", lastTs);
  const sol = await candles(SOL_POOL, "SOL", lastTs);
  dropGlitches(jito); dropGlitches(sol);

  const grid = new Map();
  for (const [ts, px] of jito) { if (ts > lastTs) { let r = grid.get(ts) ?? {}; r[JITO_MINT] = px; grid.set(ts, r); } }
  for (const [ts, px] of sol) { if (ts > lastTs) { let r = grid.get(ts) ?? {}; r["SOL"] = px; r[SOL_MINT] = px; grid.set(ts, r); } }
  const tail = [...grid.entries()].sort((a, b) => a[0] - b[0]).map(([ts, prices]) => JSON.stringify({ ts, prices }));

  fs.writeFileSync(OUT, lines.concat(tail).join("\n") + "\n");
  console.log(`Wrote ${lines.length}+${tail.length} rows → ${OUT}`);
  console.log(`span: ${new Date(JSON.parse(lines[0]).ts * 1000).toISOString()} → ${new Date(JSON.parse(tail[tail.length - 1] ?? lines[lines.length - 1]).ts * 1000).toISOString()}`);
})().catch((e) => { console.error("failed:", e.message); process.exit(1); });
