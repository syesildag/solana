#!/usr/bin/env node
/**
 * backfill_history.js — build an EXTENDED price-history file for backtests.
 *
 * Fetches old 1-minute closes from GeckoTerminal (keyless; ~6 months of minute
 * candles) for one or more tokens, then splices the live price_history.jsonl on
 * top (live data wins from its first timestamp onward). The live file is only
 * ever READ — the output is a separate file for momentum-sim, loaded past the
 * 43 200-snapshot default via the HISTORY_MAX_SNAPSHOTS override, e.g.:
 *
 *   node scripts/backfill_history.js --days 150
 *   HISTORY_MAX_SNAPSHOTS=300000 ./target/release/momentum-sim run \
 *     --history assets/price_history.extended.jsonl ...
 *
 * Defaults backfill JitoSOL (keyed by mint, as the sim expects) and SOL (keyed
 * "SOL" + the WSOL mint — the regime gate reads prices["SOL"]).
 *
 * Usage:
 *   node scripts/backfill_history.js [--days N] [--output FILE]
 *                                    [--tokens MINT[:KEY][,MINT[:KEY]…]]
 *                                    [--no-splice] [--forward-fill]
 *
 * --forward-fill makes the output match how the LIVE watcher records history, which
 * matters for any token that stops trading for long stretches (tokenized equities
 * overnight and at weekends, thin memecoins generally). GeckoTerminal only emits a
 * minute candle when a trade happened, so a raw backfill has GAPS; the live watcher
 * instead carries the last known price forward on every ~60 s tick
 * ("Carry forward last known prices for any mint missing from this tick",
 * portfolio/watcher.rs). Those two shapes are NOT interchangeable in a backtest:
 *   - gaps      ⇒ few of the token's own prints inside a lookback window, so it can
 *                 fail the SORTINO_MIN_OBS(120) floor and go silently unrankable;
 *   - flat fill ⇒ the window is full, the token ranks, its slope_r2 collapses toward 0,
 *                 and `is_stale_ts` sees a frozen price and raises the closed-market
 *                 flag — exactly what happens live.
 * Only fills inside each key's own [first, last] candle span, so a token is never
 * given a price before it existed.
 */
"use strict";

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
const GECKO = "https://api.geckoterminal.com/api/v2";
const SOL_MINT = "So11111111111111111111111111111111111111112";
const JITOSOL_MINT = "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn";
const PAGE_PAUSE_MS = 2_100; // GT free tier ~30 req/min

function argVal(flag, dflt) {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : dflt;
}

const DAYS = parseInt(argVal("--days", "150"), 10);
const OUTPUT = argVal("--output", path.join(ROOT, "assets", "price_history.extended.jsonl"));
const LIVE = path.join(ROOT, "assets", "price_history.jsonl");
const SPLICE = !process.argv.includes("--no-splice");
const FORWARD_FILL = process.argv.includes("--forward-fill");
const MINUTE_MS = 60_000;

// mint[:snapshotKey[:poolAddress]] list; key defaults to the mint itself; pool
// (optional) pins the OHLCV source instead of auto-picking.
const TOKENS = (argVal("--tokens", `${JITOSOL_MINT},${SOL_MINT}:SOL`) || "")
  .split(",").map((s) => s.trim()).filter(Boolean)
  .map((spec) => {
    const [mint, key, pool] = spec.split(":");
    return { mint, key: key || mint, pool: pool || null };
  });

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

// Pick by 24h VOLUME, not reserve: candle continuity needs trades, and the
// deepest-TVL pool can be a barely-trading vault (that's how a SOL backfill once
// came back with 3 candles). Reserve is only the tiebreak.
async function busiestPool(mint) {
  const pools = (await gecko(`${GECKO}/networks/solana/tokens/${mint}/pools?page=1`))?.data || [];
  if (!pools.length) throw new Error(`no GT pools for ${mint}`);
  pools.sort((a, b) =>
    (parseFloat(b.attributes?.volume_usd?.h24 || 0) - parseFloat(a.attributes?.volume_usd?.h24 || 0)) ||
    (parseFloat(b.attributes?.reserve_in_usd || 0) - parseFloat(a.attributes?.reserve_in_usd || 0)));
  return pools[0].attributes.address;
}

/** Page minute closes back to fromTs. Returns Map<minuteTs, close>. */
async function backfillMint(mint, fromTs, toTs, pinnedPool) {
  const pool = pinnedPool || await busiestPool(mint);
  console.log(`  ${mint.slice(0, 8)}… via pool ${pool.slice(0, 8)}…`);
  const byTs = new Map();
  let before = toTs;
  const maxPages = Math.ceil(((toTs - fromTs) / 60_000 / 1000) * 1.5) + 5;
  for (let page = 0; page < maxPages; page++) {
    if (page) await sleep(PAGE_PAUSE_MS);
    const url = `${GECKO}/networks/solana/pools/${pool}/ohlcv/minute` +
      `?aggregate=1&limit=1000&currency=usd&before_timestamp=${Math.floor(before / 1000)}`;
    let list;
    try {
      list = (await gecko(url))?.data?.attributes?.ohlcv_list || [];
    } catch (e) {
      // A long run must survive one bad page: keep what we have instead of
      // discarding hundreds of already-fetched pages.
      console.warn(`\n  page ${page} failed after retries (${e.message}) — keeping ${byTs.size} candles fetched so far`);
      break;
    }
    if (!list.length) break;
    let oldest = Infinity;
    for (const row of list) {
      const ts = row[0] * 1000, close = row[4];
      if (typeof row[0] === "number" && typeof close === "number" && close > 0) byTs.set(ts, close);
      if (row[0] * 1000 < oldest) oldest = row[0] * 1000;
    }
    process.stdout.write(`\r  ${mint.slice(0, 8)}… ${byTs.size} candles, back to ${new Date(oldest).toISOString().slice(0, 16)}   `);
    if (!isFinite(oldest) || oldest <= fromTs || oldest >= before) break;
    before = oldest;
  }
  console.log("");
  return byTs;
}

// Drop isolated glitch closes: a print that jumps >GLITCH_PCT off its previous
// neighbor and then reverts at least half-way back on the very next print is a bad
// GT candle, not a market move. (2026-07-11: a single +51% JitoSOL print at
// 04-30 22:02 sailed under momentum-sim's 8× spike filter and manufactured a fake
// +40% backtest mega-win — filter at the source so no consumer ever sees it.)
const GLITCH_PCT = 0.20;
function dropIsolatedGlitches(byTs, mint) {
  const entries = [...byTs.entries()].sort((a, b) => a[0] - b[0]);
  let dropped = 0;
  for (let i = 1; i + 1 < entries.length; i++) {
    const [, p0] = entries[i - 1], [ts, p1] = entries[i], [, p2] = entries[i + 1];
    const jump = p1 / p0 - 1, revert = p2 / p1 - 1;
    if (Math.abs(jump) > GLITCH_PCT && jump * revert < 0 && Math.abs(revert) > Math.abs(jump) / 2) {
      byTs.delete(ts);
      dropped++;
      console.warn(`  ${mint.slice(0, 8)}… dropped glitch print ${p0.toFixed(2)} -> ${p1.toFixed(2)} (${(jump * 100).toFixed(0)}%) at ${new Date(ts).toISOString().slice(0, 16)}`);
    }
  }
  return dropped;
}

// Densify `grid` to a one-row-per-minute cadence, carrying each key's last known price
// forward into minutes where GeckoTerminal emitted no candle (see --forward-fill above).
// A key is only filled inside its own first/last candle span — never back-dated to before
// the token existed. Returns the number of values filled.
function forwardFill(grid) {
  const span = new Map(); // key → [firstTs, lastTs]
  for (const ts of [...grid.keys()].sort((a, b) => a - b)) {
    for (const k of Object.keys(grid.get(ts))) {
      const s = span.get(k);
      if (!s) span.set(k, [ts, ts]);
      else s[1] = ts;
    }
  }
  if (!span.size) return 0;
  const lo = Math.min(...[...span.values()].map((s) => s[0]));
  const hi = Math.max(...[...span.values()].map((s) => s[1]));
  const last = new Map();
  let filled = 0;
  for (let ts = lo; ts <= hi; ts += MINUTE_MS) {
    let row = grid.get(ts);
    if (!row) { row = {}; grid.set(ts, row); }
    for (const [k, [first, lastTs]] of span) {
      if (row[k] != null) { last.set(k, row[k]); continue; }
      if (ts < first || ts > lastTs) continue; // outside this key's life — leave absent
      const v = last.get(k);
      if (v != null) { row[k] = v; filled++; }
    }
  }
  return filled;
}

(async () => {
  const now = Date.now();
  const fromTs = now - DAYS * 86_400_000;
  console.log(`Backfilling ${DAYS}d of 1m closes for ${TOKENS.length} token(s) from GeckoTerminal…`);

  // minuteTs → { key: price }
  const grid = new Map();
  for (const t of TOKENS) {
    const series = await backfillMint(t.mint, fromTs, now, t.pool);
    dropIsolatedGlitches(series, t.mint);
    for (const [ts, p] of series) {
      let row = grid.get(ts);
      if (!row) { row = {}; grid.set(ts, row); }
      row[t.key] = p;
      // SOL is stored under BOTH "SOL" and the WSOL mint in live snapshots.
      if (t.mint === SOL_MINT) row[SOL_MINT] = p;
    }
  }

  if (FORWARD_FILL) {
    const before = grid.size;
    const filled = forwardFill(grid);
    console.log(
      `Forward-filled ${filled} value(s) across ${grid.size - before} added minute row(s) ` +
      `(live-parity shape: frozen price carried forward, not a gap)`
    );
  }

  // Splice: backfill strictly OLDER than the live file's first snapshot, then live
  // lines verbatim (live fidelity wins; the live file is never written).
  let liveLines = [];
  let liveStart = Infinity;
  if (SPLICE && fs.existsSync(LIVE)) {
    liveLines = fs.readFileSync(LIVE, "utf8").split("\n").filter((l) => l.trim());
    for (const l of liveLines) {
      try { liveStart = Math.min(liveStart, JSON.parse(l).ts); break; } catch {}
    }
    console.log(`Splicing live history (${liveLines.length} lines, starts ${new Date(liveStart * 1000).toISOString().slice(0, 16)})`);
  }

  const backfill = [...grid.entries()]
    .map(([ts, prices]) => ({ ts: Math.floor(ts / 1000), prices }))
    .filter((s) => s.ts < liveStart)
    .sort((a, b) => a.ts - b.ts);

  const out = fs.createWriteStream(OUTPUT);
  for (const s of backfill) out.write(JSON.stringify(s) + "\n");
  for (const l of liveLines) out.write(l + "\n");
  await new Promise((r) => out.end(r));

  const total = backfill.length + liveLines.length;
  console.log(`Wrote ${total} snapshots (${backfill.length} backfilled + ${liveLines.length} live) → ${OUTPUT}`);
  console.log(`Run sims with: HISTORY_MAX_SNAPSHOTS=${Math.max(total + 10_000, 50_000)} ./target/release/momentum-sim run --history ${OUTPUT} …`);
})().catch((e) => {
  console.error(`\nbackfill failed: ${e.message}`);
  process.exit(1);
});
