#!/usr/bin/env node
/**
 * One-off: build a self-contained 1-minute history file for Agamemnon (a day-0
 * pump.fun launch) so momentum-sim can walk-forward over its own ~9h of life.
 * Fetches Agamemnon + SOL 1m USD closes from GeckoTerminal for the overlapping
 * window and writes {ts, prices:{<mint>, "SOL", <wsol>}} snapshots. Regime gate
 * reads prices["SOL"], so SOL is aligned into the same minute grid.
 */
"use strict";
const fs = require("fs");
const path = require("path");

const GECKO = "https://api.geckoterminal.com/api/v2";
const AGA_MINT = "2cAtqsRafKS7baN3mvJARhyZiMRdW4fZYNUUWUrCpump";
const AGA_POOL = "636bkx7Ugs6Vdb9FhAJdwFdi4afupHDarrTW2nTVuEag";
const SOL_MINT = "So11111111111111111111111111111111111111112";
const OUT = path.join(__dirname, "..", "assets", "price_history.agamemnon.jsonl");
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

async function busiestPool(mint) {
  const pools = (await gecko(`${GECKO}/networks/solana/tokens/${mint}/pools?page=1`))?.data || [];
  pools.sort((a, b) =>
    parseFloat(b.attributes?.volume_usd?.h24 || 0) - parseFloat(a.attributes?.volume_usd?.h24 || 0));
  return pools[0].attributes.address;
}

async function candles(pool, before) {
  const url = `${GECKO}/networks/solana/pools/${pool}/ohlcv/minute?aggregate=1&limit=1000&currency=usd` +
    (before ? `&before_timestamp=${before}` : "");
  const list = (await gecko(url))?.data?.attributes?.ohlcv_list || [];
  const m = new Map();
  for (const r of list) if (typeof r[0] === "number" && typeof r[4] === "number" && r[4] > 0) m.set(r[0], r[4]);
  return m;
}

(async () => {
  const aga = await candles(AGA_POOL);
  const ts = [...aga.keys()].sort((a, b) => a - b);
  const lo = ts[0], hi = ts[ts.length - 1];
  console.log(`Agamemnon: ${aga.size} candles, ${new Date(lo * 1000).toISOString()} → ${new Date(hi * 1000).toISOString()}`);

  const solPool = await busiestPool(SOL_MINT);
  await sleep(2100);
  const sol = await candles(solPool, hi + 60); // one page (1000 min) covers the 9h window
  console.log(`SOL: ${sol.size} candles via pool ${solPool.slice(0, 8)}…`);

  const rows = [];
  for (const t of ts) {
    const prices = { [AGA_MINT]: aga.get(t) };
    // nearest SOL minute (fill ±1) so the regime gate has a value
    const s = sol.get(t) ?? sol.get(t - 60) ?? sol.get(t + 60);
    if (s != null) { prices["SOL"] = s; prices[SOL_MINT] = s; }
    rows.push({ ts: t, prices });
  }
  fs.writeFileSync(OUT, rows.map((r) => JSON.stringify(r)).join("\n") + "\n");
  const solCov = rows.filter((r) => "SOL" in r.prices).length;
  console.log(`Wrote ${rows.length} snapshots (SOL present in ${solCov}) → ${OUT}`);
})().catch((e) => { console.error("failed:", e.message); process.exit(1); });
