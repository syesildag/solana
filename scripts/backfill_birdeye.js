#!/usr/bin/env node
/*
 * backfill_birdeye.js — build 1-minute price history from Birdeye OHLCV.
 *
 * WHY this exists alongside backfill_history.js (GeckoTerminal): GT's keyless free tier
 * caps at ~30 req/min and 1000 candles/page, so 150 days of 1-minute data is ~217 pages =
 * hours per token, and a burst of parallel runs exhausts the quota for a long cooldown
 * (observed 2026-07-28: two parallel 150d backfills produced 110+ 429s, then bare probes
 * kept 429ing). Birdeye (keyed) serves the same 1000-candle pages but tolerates ~1 req/s,
 * turning the same job into ~4 minutes per token. GT stays the keyless fallback.
 *
 * Output is the standard snapshot JSONL the sim/watcher read:
 *   {"ts":<unix>,"prices":{"<mint>":<close>}}
 * One row per candle per token; `--merge <file>` ts-union-merges into an existing history
 * (prices dicts unioned, rows outside the target file's span dropped) so a per-token
 * backfill can extend the combined file the last optimization used.
 *
 * Usage:
 *   node scripts/backfill_birdeye.js --days 150 --tokens <MINT>[,<MINT>…] --output out.jsonl
 *   node scripts/backfill_birdeye.js --days 150 --tokens <MINT> --merge assets/price_history.curated150.jsonl
 */
"use strict";
require("./lib/load_env");
const fs = require("fs");

const arg = (flag, dflt) => {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : dflt;
};
const DAYS = parseInt(arg("--days", "150"), 10);
const MINTS = (arg("--tokens", "") || "").split(",").map((s) => s.trim()).filter(Boolean);
const OUTPUT = arg("--output", null);
const MERGE = arg("--merge", null);
const PAGE_PAUSE_MS = 1200; // Birdeye free tier tolerates ~1 req/s; 1.2s leaves headroom
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

if (!MINTS.length || (!OUTPUT && !MERGE)) {
  console.error("usage: --tokens <mint[,mint]> (--output <file> | --merge <file>) [--days 150]");
  process.exit(1);
}

/** One OHLCV page (≤1000 candles, newest-first window ending at `timeTo`). Retries 429/5xx. */
async function page(mint, timeFrom, timeTo) {
  const url = `https://public-api.birdeye.so/defi/ohlcv?address=${mint}&type=1m` +
    `&time_from=${timeFrom}&time_to=${timeTo}`;
  for (let attempt = 0; attempt < 6; attempt++) {
    const res = await fetch(url, {
      headers: { "X-API-KEY": process.env.BIRDEYE_API_KEY || "", "x-chain": "solana", accept: "application/json" },
    });
    if (res.ok) return ((await res.json()).data?.items) || [];
    // Birdeye signals free-tier throttling as 401 as well as 429 (see the scan_tokens notes).
    if ([401, 429].includes(res.status) || res.status >= 500) {
      const back = 2000 * (attempt + 1);
      process.stderr.write(` BE ${res.status} — backing off ${back / 1000}s`);
      await sleep(back);
      continue;
    }
    // Non-retryable (e.g. 400 from an inverted/edge window): stop this token and KEEP what
    // was already fetched. A throw here aborted the whole run and discarded every candle.
    process.stderr.write(` BE ${res.status} (non-retryable) — stopping this token`);
    return null;
  }
  return null; // exhausted retries — caller keeps what it has
}

/** Page backwards until `days` are covered. Returns Map<ts, close>. */
async function fetchToken(mint, days) {
  const now = Math.floor(Date.now() / 1000);
  const floor = now - days * 86400;
  const out = new Map();
  let timeTo = now;
  // 1000 candles is the hard page cap, so each request must span ~1000 MINUTES. Asking for a
  // wide window does NOT page — Birdeye silently downsamples to fit 1000 items (a 150d
  // request returned 3.6h-spaced candles, useless for obs-based metrics) and 400s outright
  // on some mints. Keep the window narrow and walk it backwards.
  const PAGE_SECS = 1000 * 60;
  for (let p = 0; p < 400; p++) { // 400 × 1000 candles ≫ 150d; the floor check exits first
    if (timeTo <= floor) break; // walked past the horizon; another request would invert the range
    if (p) await sleep(PAGE_PAUSE_MS);
    const from = Math.max(floor, timeTo - PAGE_SECS);
    if (from >= timeTo) break;  // guard: Birdeye 400s on from >= to
    const items = await page(mint, from, timeTo);
    if (items === null) { console.error(`\n  ${mint.slice(0, 8)}… page ${p} gave up — keeping ${out.size} candles`); break; }
    if (!items.length) break;
    let oldest = Infinity;
    for (const it of items) {
      const ts = +it.unixTime, c = +it.c;
      if (Number.isFinite(ts) && Number.isFinite(c) && c > 0) out.set(ts, c);
      if (ts < oldest) oldest = ts;
    }
    process.stderr.write(`\r  ${mint.slice(0, 8)}… ${out.size} candles, back to ${new Date(oldest * 1000).toISOString().slice(0, 16)}   `);
    if (oldest <= floor) break;
    timeTo = oldest - 60; // step the window back one candle past the oldest seen
  }
  process.stderr.write("\n");
  return out;
}

(async () => {
  const series = new Map(); // mint → Map<ts, close>
  for (const m of MINTS) series.set(m, await fetchToken(m, DAYS));

  // Fold every token's candles into ts-keyed snapshot rows.
  const rows = new Map(); // ts → { mint: price }
  for (const [mint, ser] of series) {
    for (const [ts, px] of ser) {
      if (!rows.has(ts)) rows.set(ts, {});
      rows.get(ts)[mint] = px;
    }
  }

  if (MERGE) {
    // ts-union into the existing history, clamped to ITS span: a backfill must extend the
    // combined file's token coverage, never its time axis (a longer axis would silently
    // change every train/test split the last optimization was validated on).
    const lines = fs.readFileSync(MERGE, "utf8").trim().split("\n");
    const base = lines.map((l) => JSON.parse(l));
    const lo = base[0].ts, hi = base[base.length - 1].ts;
    let touched = 0, added = 0;
    for (const snap of base) {
      const extra = rows.get(snap.ts);
      if (!extra) continue;
      Object.assign(snap.prices, extra);
      touched++;
    }
    // Rows the base file has no timestamp for are dropped (clamped axis) — report the loss.
    for (const ts of rows.keys()) if (ts >= lo && ts <= hi && !base.some((b) => b.ts === ts)) added++;
    fs.copyFileSync(MERGE, MERGE + ".bak");
    fs.writeFileSync(MERGE + ".tmp", base.map((s) => JSON.stringify(s)).join("\n") + "\n");
    fs.renameSync(MERGE + ".tmp", MERGE);
    console.log(`merged into ${MERGE}: ${touched} rows enriched (${added} candle ts had no base row, dropped); backup at ${MERGE}.bak`);
    for (const [m, s] of series) console.log(`  ${m.slice(0, 8)}…: ${s.size} candles fetched`);
  } else {
    const sorted = [...rows.entries()].sort((a, b) => a[0] - b[0]);
    fs.writeFileSync(OUTPUT, sorted.map(([ts, prices]) => JSON.stringify({ ts, prices })).join("\n") + "\n");
    const span = sorted.length ? ((sorted[sorted.length - 1][0] - sorted[0][0]) / 86400).toFixed(1) : "0";
    console.log(`wrote ${sorted.length} snapshots (${span} days) → ${OUTPUT}`);
  }
})().catch((e) => { console.error("backfill failed:", e.message); process.exit(1); });
