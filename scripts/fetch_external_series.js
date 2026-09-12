#!/usr/bin/env node
/**
 * fetch_external_series.js — pull the pre-registered EXTERNAL series (2026-09-12 plan:
 * "external-state entry gate") into assets/external_series.jsonl for `momentum-sim
 * external-diag`. One row per point: {"ts": <unix seconds>, "key": "BTC", "value": 1.23}.
 *
 * Every row is stamped at the moment the value was KNOWABLE, not the moment it describes,
 * so no consumer can leak the future by joining on ts:
 *   - BTC / ETH   Coinbase Exchange hourly candles (keyless, ≤300/call) → candle CLOSE
 *                 (bucket start + 3600). Kraken daily is the fallback (close = start + 86400).
 *   - DGS10 / DFF FRED fredgraph.csv (keyless, daily; H.15 posts ~21:15Z) → obs date + 1 day 00:00Z.
 *   - DTWEXBGS    FRED broad dollar index (H.10 is a WEEKLY release)      → obs date + 7 days 00:00Z.
 *   - FUND_HYPE / FUND_ZEC  Bybit v5 linear funding history (keyless, 200/call, 8-hourly)
 *                 → settlement timestamp (the rate is fixed at settlement).
 *   Round 2 (same day, operator asked for more sources):
 *   - OI_HYPE / OI_ZEC     Bybit v5 open-interest history, 1 h (cursor-paginated) → bar timestamp.
 *   - LSR_HYPE / LSR_ZEC   Bybit v5 long/short ACCOUNT ratio (buyRatio), 1 h → bar timestamp.
 *   - ETHBTC / SOLBTC      Coinbase hourly candles of the ratio products → candle CLOSE.
 *   - VOL_<SYMBOL>         GeckoTerminal hourly OHLCV USD volume of the token's own pool
 *                          (pool from assets/momentum_tokens.json `pool`) → candle CLOSE.
 *   - FNG                  alternative.me Fear & Greed (daily) → day + 1 d 00:00Z.
 *   - HLFEES_HYPE          DefiLlama Hyperliquid daily fees → day + 1 d 00:00Z.
 *   - VIX / SPX            Yahoo Finance daily closes (^VIX, ^GSPC) → day + 1 d 00:00Z.
 *
 * Idempotent: merges into the existing file on (ts, key), rewrites sorted.
 *
 * Usage:
 *   node scripts/fetch_external_series.js [--from 2026-02-01] [--to 2026-09-12T00:00:00Z]
 *                                         [--keys BTC,ETH,DGS10,DFF,DTWEXBGS,FUND_HYPE,FUND_ZEC]
 *                                         [--output assets/external_series.jsonl]
 */
"use strict";
const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
function argVal(flag, dflt) {
  const i = process.argv.indexOf(flag);
  return i >= 0 ? process.argv[i + 1] : dflt;
}
const FROM_S = Math.floor(Date.parse(argVal("--from", "2026-02-01T00:00:00Z")) / 1000);
const TO_S = Math.floor(Date.parse(argVal("--to", new Date().toISOString())) / 1000);
const OUTPUT = argVal("--output", path.join(ROOT, "assets", "external_series.jsonl"));
const ALL_KEYS = [
  "BTC", "ETH", "DGS10", "DFF", "DTWEXBGS", "FUND_HYPE", "FUND_ZEC",
  "OI_HYPE", "OI_ZEC", "LSR_HYPE", "LSR_ZEC", "ETHBTC", "SOLBTC",
  "VOL_HYPE", "VOL_ZEC", "VOL_JitoSOL", "FNG", "HLFEES_HYPE", "VIX", "SPX",
];
const TOKENS_PATH = argVal("--tokens", path.join(ROOT, "assets", "momentum_tokens.json"));
const KEYS = (argVal("--keys", ALL_KEYS.join(",")) || "").split(",").map((s) => s.trim()).filter(Boolean);
const PAUSE_MS = 300;
const DAY = 86_400;

if (!Number.isFinite(FROM_S) || !Number.isFinite(TO_S)) {
  console.error("bad --from/--to (want ISO dates)");
  process.exit(2);
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** GET with the repo's usual 429/5xx backoff; returns the Response (caller parses). */
async function get(url, tries = 5) {
  for (let i = 0; ; i++) {
    const res = await fetch(url, { headers: { accept: "application/json,text/csv,*/*", "user-agent": "solana-mev-research/1.0" } });
    if (res.ok) return res;
    // GeckoTerminal (like Birdeye) signals its free-tier rate limit as 401 as often as 429.
    const rateLimited = res.status === 429 || (res.status === 401 && url.includes("geckoterminal"));
    if ((rateLimited || res.status >= 500) && i < tries - 1) {
      const ra = parseInt(res.headers.get("retry-after") || "", 10);
      const wait = Math.max(Number.isFinite(ra) ? ra : 0, Math.min(2 ** i * 5, 60)) * 1000;
      console.warn(`  ${res.status} from ${new URL(url).host} — backing off ${wait / 1000}s`);
      await sleep(wait);
      continue;
    }
    throw new Error(`${res.status} ${url}`);
  }
}

// ── Coinbase hourly (fallback Kraken daily) ──────────────────────────────────────────
async function coinbaseHourly(product, fromS, toS) {
  const out = new Map(); // closeTs → close
  const STEP = 250 * 3600; // ≤300 candles per request
  for (let start = fromS; start < toS; start += STEP) {
    const end = Math.min(start + STEP, toS);
    const url =
      `https://api.exchange.coinbase.com/products/${product}/candles?granularity=3600` +
      `&start=${new Date(start * 1000).toISOString()}&end=${new Date(end * 1000).toISOString()}`;
    const rows = await (await get(url)).json();
    if (!Array.isArray(rows)) throw new Error(`coinbase ${product}: unexpected body`);
    for (const r of rows) {
      const [t, , , , close] = r; // [time, low, high, open, close, volume], time = bucket START
      if (Number.isFinite(t) && Number.isFinite(close) && close > 0) out.set(t + 3600, close);
    }
    await sleep(PAUSE_MS);
  }
  return out;
}

async function krakenDaily(pair, fromS) {
  const url = `https://api.kraken.com/0/public/OHLC?pair=${pair}&interval=1440&since=${fromS}`;
  const body = await (await get(url)).json();
  const key = Object.keys(body.result || {}).find((k) => k !== "last");
  const out = new Map();
  for (const r of body.result?.[key] || []) {
    const t = +r[0], close = parseFloat(r[4]);
    if (Number.isFinite(t) && close > 0) out.set(t + DAY, close); // daily close = start + 1d
  }
  return out;
}

async function crypto(key, fromS, toS) {
  const product = key === "BTC" ? "BTC-USD" : "ETH-USD";
  try {
    const m = await coinbaseHourly(product, fromS - 8 * DAY, toS); // 8 d of warm-up for the 168 h window
    if (m.size > 0) return m;
    console.warn(`  ${key}: Coinbase returned no candles — falling back to Kraken daily`);
  } catch (e) {
    console.warn(`  ${key}: Coinbase failed (${e.message}) — falling back to Kraken daily`);
  }
  return krakenDaily(key === "BTC" ? "XBTUSD" : "ETHUSD", fromS - 40 * DAY);
}

// ── FRED daily CSV ────────────────────────────────────────────────────────────────────
async function fred(seriesId, lagDays, fromS) {
  const text = await (await get(`https://fred.stlouisfed.org/graph/fredgraph.csv?id=${seriesId}`)).text();
  const out = new Map();
  for (const line of text.split("\n").slice(1)) {
    const [date, raw] = line.trim().split(",");
    if (!date || raw === undefined || raw === "" || raw === ".") continue; // holidays / not yet published
    const v = parseFloat(raw);
    const obs = Math.floor(Date.parse(`${date}T00:00:00Z`) / 1000);
    if (!Number.isFinite(v) || !Number.isFinite(obs)) continue;
    const knownAt = obs + lagDays * DAY; // publication lag: the value is unknowable before this
    if (knownAt >= fromS - 40 * DAY) out.set(knownAt, v);
  }
  return out;
}

// ── Bybit funding history ─────────────────────────────────────────────────────────────
async function bybitFunding(symbol, fromS, toS) {
  const out = new Map();
  let endMs = toS * 1000;
  for (let page = 0; page < 60; page++) {
    const url =
      `https://api.bybit.com/v5/market/funding/history?category=linear&symbol=${symbol}&limit=200&endTime=${endMs}`;
    const body = await (await get(url)).json();
    if (body.retCode !== 0) throw new Error(`bybit ${symbol}: ${body.retMsg}`);
    const list = body.result?.list || [];
    if (!list.length) break;
    let oldest = Infinity;
    for (const e of list) {
      const ms = +e.fundingRateTimestamp, rate = parseFloat(e.fundingRate);
      if (!Number.isFinite(ms) || !Number.isFinite(rate)) continue;
      out.set(Math.floor(ms / 1000), rate);
      oldest = Math.min(oldest, ms);
    }
    if (!Number.isFinite(oldest) || oldest / 1000 <= fromS - 30 * DAY || oldest >= endMs) break;
    endMs = oldest - 1;
    await sleep(PAUSE_MS);
  }
  return out;
}

// ── Bybit open interest (1 h, cursor pagination) ─────────────────────────────────────
async function bybitOpenInterest(symbol, fromS, toS) {
  const out = new Map();
  let cursor = "";
  for (let page = 0; page < 80; page++) {
    const url =
      `https://api.bybit.com/v5/market/open-interest?category=linear&symbol=${symbol}&intervalTime=1h&limit=200` +
      `&startTime=${(fromS - 8 * DAY) * 1000}&endTime=${toS * 1000}` + (cursor ? `&cursor=${encodeURIComponent(cursor)}` : "");
    const body = await (await get(url)).json();
    if (body.retCode !== 0) throw new Error(`bybit OI ${symbol}: ${body.retMsg}`);
    const list = body.result?.list || [];
    for (const e of list) {
      const ms = +e.timestamp, oi = parseFloat(e.openInterest);
      if (Number.isFinite(ms) && Number.isFinite(oi)) out.set(Math.floor(ms / 1000), oi);
    }
    cursor = body.result?.nextPageCursor || "";
    if (!list.length || !cursor) break;
    await sleep(PAUSE_MS);
  }
  return out;
}

// ── Bybit long/short account ratio (1 h) ─────────────────────────────────────────────
async function bybitLongShort(symbol, fromS, toS) {
  const out = new Map();
  let endMs = toS * 1000;
  for (let page = 0; page < 40; page++) {
    const url =
      `https://api.bybit.com/v5/market/account-ratio?category=linear&symbol=${symbol}&period=1h&limit=500` +
      `&startTime=${(fromS - 8 * DAY) * 1000}&endTime=${endMs}`;
    const body = await (await get(url)).json();
    if (body.retCode !== 0) throw new Error(`bybit LSR ${symbol}: ${body.retMsg}`);
    const list = body.result?.list || [];
    if (!list.length) break;
    let oldest = Infinity;
    for (const e of list) {
      const ms = +e.timestamp, r = parseFloat(e.buyRatio);
      if (Number.isFinite(ms) && Number.isFinite(r)) out.set(Math.floor(ms / 1000), r);
      oldest = Math.min(oldest, ms);
    }
    if (!Number.isFinite(oldest) || oldest / 1000 <= fromS - 8 * DAY || oldest >= endMs) break;
    endMs = oldest - 1;
    await sleep(PAUSE_MS);
  }
  return out;
}

// ── GeckoTerminal hourly pool volume (USD) ───────────────────────────────────────────
function poolFor(symbol) {
  const toks = JSON.parse(fs.readFileSync(TOKENS_PATH, "utf8"));
  const t = (Array.isArray(toks) ? toks : toks.tokens || []).find((x) => x.symbol === symbol);
  if (!t?.pool) throw new Error(`no pool for ${symbol} in ${TOKENS_PATH}`);
  return t.pool;
}
async function geckoHourlyVolume(pool, fromS, toS) {
  // The keyless GT API serves ONLY the last 180 days ("401 … past 180 days with Public API");
  // stop there and KEEP what was fetched — the diag's as-of mask reads `true` before the first
  // state, so a series that starts 11 days into the history is usable, one that failed is not.
  const floor = Math.max(fromS - 8 * DAY, Math.floor(Date.now() / 1000) - 179 * DAY);
  const out = new Map();
  let before = toS;
  for (let page = 0; page < 12; page++) {
    const url =
      `https://api.geckoterminal.com/api/v2/networks/solana/pools/${pool}/ohlcv/hour?aggregate=1&limit=1000&currency=usd&before_timestamp=${before}`;
    let res;
    for (let attempt = 0; ; attempt++) {
      res = await fetch(url, { headers: { accept: "application/json" } });
      if (res.status !== 429 || attempt >= 4) break;
      const wait = Math.min(2 ** attempt * 5, 60) * 1000;
      console.warn(`  429 from GT — backing off ${wait / 1000}s`);
      await sleep(wait);
    }
    if (!res.ok) {
      const msg = (await res.text()).slice(0, 160);
      console.warn(`  GT ${res.status} at before=${before} — keeping ${out.size} candles (${msg})`);
      break;
    }
    const body = await res.json();
    const list = body?.data?.attributes?.ohlcv_list || [];
    if (!list.length) break;
    let oldest = Infinity;
    for (const r of list) {
      const t = +r[0], vol = parseFloat(r[5]);
      if (Number.isFinite(t) && Number.isFinite(vol)) out.set(t + 3600, vol); // candle close
      oldest = Math.min(oldest, t);
    }
    if (!Number.isFinite(oldest) || oldest <= floor || oldest >= before) break;
    before = oldest;
    await sleep(6_500); // GT free tier ~30 req/min on paper; be gentle
  }
  return out;
}

// ── daily sentiment / fundamentals / equities (all stamped day + 1 d 00:00Z) ─────────
async function fearGreed(fromS) {
  const body = await (await get("https://api.alternative.me/fng/?limit=0&format=json")).json();
  const out = new Map();
  for (const e of body.data || []) {
    const t = +e.timestamp, v = parseFloat(e.value);
    if (Number.isFinite(t) && Number.isFinite(v) && t + DAY >= fromS - 40 * DAY) out.set(t + DAY, v);
  }
  return out;
}
async function llamaFees(protocol, fromS) {
  const body = await (await get(`https://api.llama.fi/summary/fees/${protocol}?dataType=dailyFees`)).json();
  const out = new Map();
  for (const [t, v] of body.totalDataChart || []) {
    if (Number.isFinite(t) && Number.isFinite(v) && t + DAY >= fromS - 40 * DAY) out.set(t + DAY, v);
  }
  return out;
}
async function yahooDaily(symbol, fromS) {
  const url = `https://query1.finance.yahoo.com/v8/finance/chart/${encodeURIComponent(symbol)}?range=2y&interval=1d`;
  const body = await (await get(url)).json();
  const r = body?.chart?.result?.[0];
  if (!r) throw new Error(`yahoo ${symbol}: ${JSON.stringify(body?.chart?.error || body).slice(0, 120)}`);
  const closes = r.indicators?.quote?.[0]?.close || [];
  const out = new Map();
  (r.timestamp || []).forEach((t, i) => {
    const v = closes[i];
    if (!Number.isFinite(t) || !Number.isFinite(v)) return;
    const day = Math.floor(t / DAY) * DAY; // session date (UTC day) → known next midnight
    if (day + DAY >= fromS - 40 * DAY) out.set(day + DAY, v);
  });
  return out;
}

// ── main ──────────────────────────────────────────────────────────────────────────────
(async () => {
  const fetchers = {
    BTC: () => crypto("BTC", FROM_S, TO_S),
    ETH: () => crypto("ETH", FROM_S, TO_S),
    DGS10: () => fred("DGS10", 1, FROM_S),
    DFF: () => fred("DFF", 1, FROM_S),
    DTWEXBGS: () => fred("DTWEXBGS", 7, FROM_S),
    FUND_HYPE: () => bybitFunding("HYPEUSDT", FROM_S, TO_S),
    FUND_ZEC: () => bybitFunding("ZECUSDT", FROM_S, TO_S),
    OI_HYPE: () => bybitOpenInterest("HYPEUSDT", FROM_S, TO_S),
    OI_ZEC: () => bybitOpenInterest("ZECUSDT", FROM_S, TO_S),
    LSR_HYPE: () => bybitLongShort("HYPEUSDT", FROM_S, TO_S),
    LSR_ZEC: () => bybitLongShort("ZECUSDT", FROM_S, TO_S),
    ETHBTC: () => coinbaseHourly("ETH-BTC", FROM_S - 15 * DAY, TO_S),
    SOLBTC: () => coinbaseHourly("SOL-BTC", FROM_S - 15 * DAY, TO_S),
    VOL_HYPE: () => geckoHourlyVolume(poolFor("HYPE"), FROM_S, TO_S),
    VOL_ZEC: () => geckoHourlyVolume(poolFor("ZEC"), FROM_S, TO_S),
    VOL_JitoSOL: () => geckoHourlyVolume(poolFor("JitoSOL"), FROM_S, TO_S),
    FNG: () => fearGreed(FROM_S),
    HLFEES_HYPE: () => llamaFees("hyperliquid", FROM_S),
    VIX: () => yahooDaily("^VIX", FROM_S),
    SPX: () => yahooDaily("^GSPC", FROM_S),
  };

  // Existing rows first (idempotent merge on ts|key).
  const rows = new Map();
  if (fs.existsSync(OUTPUT)) {
    for (const line of fs.readFileSync(OUTPUT, "utf8").split("\n")) {
      if (!line.trim()) continue;
      try {
        const r = JSON.parse(line);
        if (Number.isFinite(r.ts) && r.key && Number.isFinite(r.value)) rows.set(`${r.ts}|${r.key}`, r);
      } catch { /* skip */ }
    }
    console.log(`existing: ${rows.size} rows in ${path.relative(ROOT, OUTPUT)}`);
  }

  for (const key of KEYS) {
    const f = fetchers[key];
    if (!f) { console.warn(`unknown key ${key} — skipped`); continue; }
    process.stdout.write(`${key}: fetching… `);
    try {
      const m = await f();
      let n = 0;
      for (const [ts, value] of m) {
        if (ts > TO_S) continue;
        rows.set(`${ts}|${key}`, { ts, key, value });
        n++;
      }
      const tss = [...m.keys()].sort((a, b) => a - b);
      const span = tss.length ? `${new Date(tss[0] * 1000).toISOString().slice(0, 16)} → ${new Date(tss[tss.length - 1] * 1000).toISOString().slice(0, 16)}` : "—";
      console.log(`${n} points (${span})`);
    } catch (e) {
      console.log(`FAILED: ${e.message}`);
    }
    await sleep(PAUSE_MS);
  }

  const sorted = [...rows.values()].sort((a, b) => a.ts - b.ts || a.key.localeCompare(b.key));
  fs.mkdirSync(path.dirname(OUTPUT), { recursive: true });
  const tmp = `${OUTPUT}.tmp`;
  fs.writeFileSync(tmp, sorted.map((r) => JSON.stringify(r)).join("\n") + "\n");
  fs.renameSync(tmp, OUTPUT);
  const perKey = {};
  for (const r of sorted) perKey[r.key] = (perKey[r.key] || 0) + 1;
  console.log(`wrote ${sorted.length} rows → ${path.relative(ROOT, OUTPUT)}`, perKey);
})().catch((e) => { console.error(e); process.exit(1); });
