#!/usr/bin/env node
"use strict";
/**
 * vet_token.js — vet a token for the momentum trader, end to end.
 *
 *   1. Resolve <ticker|name|mint> to a Jupiter-VERIFIED mint (reuses scripts/lib/jup).
 *   2. Backfill its 1-minute price history from Birdeye (the data the backtest needs).
 *   3. Build a self-contained temp history (candidate + SOL) — SOL is reused read-only
 *      from the live price_history.jsonl by nearest-minute match, so we never rewrite the
 *      live file or spawn a second watcher (both would race the running watcher).
 *   4. Run the momentum-sim grid (fixed-trail only, so winners are live-reproducible).
 *   5. Decide: LIQUID (>= $0.44M depth AND 0.5 <= vol/liq <= 30, the upper bound an
 *      anti-wash cap) AND VOLATILE (ann. vol >= floor) AND PnL-POSITIVE (>=1 robust
 *      config, best worst-slice > 0) -> qualifies. The liquidity gate runs first and
 *      short-circuits before the backfill+grid.
 *   6. With --add, append it to the curated list via scripts/add_momentum_token.js.
 *
 * Usage:
 *   node vet_token.js <ticker|name|mint> [--add] [--days N] [--vol-floor PCT] [--min-trades N]
 *                     [--min-liquidity USD] [--min-vol-liq-ratio R] [--max-vol-liq-ratio R] [--force]
 *
 * Default is vet-only (no list change). The live watcher backfills the token into the
 * REAL history on its next restart once it's curated.
 */
const fs = require("fs");
const path = require("path");
const os = require("os");
const { execFileSync } = require("child_process");

const REPO = execFileSync("git", ["rev-parse", "--show-toplevel"]).toString().trim();
process.chdir(REPO);
const { USDC_MINT, MINT_RE, search } = require(path.join(REPO, "scripts", "lib", "jup.js"));

const BIRDEYE_HISTORY_URL = "https://public-api.birdeye.so/defi/history_price";
const BIRDEYE_PRICE_URL = "https://public-api.birdeye.so/defi/price";
const GECKO_BASE = "https://api.geckoterminal.com/api/v2"; // CoinGecko's keyless DEX API
const PAGE_SECONDS = 1000 * 60; // mirror the Rust backfill page size (~16.7h)
const SOL_MINT = "So11111111111111111111111111111111111111112";
const MIN_OBS = 500; // below this the backtest is meaningless

// ── tiny .env loader so BIRDEYE_API_KEY etc. are available when run standalone ──
function loadEnv() {
  const p = path.join(REPO, ".env");
  if (!fs.existsSync(p)) return;
  for (const line of fs.readFileSync(p, "utf8").split("\n")) {
    const m = line.match(/^\s*([A-Z0-9_]+)\s*=\s*(.*?)\s*$/);
    if (m && !(m[1] in process.env)) process.env[m[1]] = m[2].replace(/^"(.*)"$/, "$1");
  }
}

function arg(flag, def) {
  const i = process.argv.indexOf(flag);
  return i >= 0 && process.argv[i + 1] ? process.argv[i + 1] : def;
}

async function resolveToken(query, force) {
  if (MINT_RE.test(query)) {
    const hit = (await search(query)).find((t) => t.id === query);
    return hit
      ? { symbol: hit.symbol, mint: query, name: hit.name, verified: !!hit.isVerified }
      : { symbol: `TOKEN-${query.slice(0, 4)}`, mint: query, name: null, verified: null };
  }
  const results = await search(query);
  if (!results.length) throw new Error(`no Jupiter token matches "${query}"`);
  const q = query.toUpperCase();
  const verified = results.filter((t) => t.isVerified);
  const pick =
    verified.find((t) => (t.symbol || "").toUpperCase() === q) ||
    verified.find((t) => (t.name || "").toUpperCase() === q) ||
    (force ? verified[0] || results[0] : null);
  if (!pick) {
    const lines = results.slice(0, 6)
      .map((t) => `    ${t.symbol} — ${t.name} [${t.isVerified ? "verified" : "UNVERIFIED"}] ${t.id}`)
      .join("\n");
    throw new Error(
      `no exact verified symbol/name match for "${query}" (avoids look-alike scams).\n` +
      `  Candidates:\n${lines}\n  Re-run with the exact mint, or append --force.`);
  }
  return { symbol: pick.symbol, mint: pick.id, name: pick.name, verified: !!pick.isVerified };
}

async function birdeyeHistory(apiKey, mint, fromTs, toTs) {
  const out = [];
  for (let from = fromTs; from < toTs; from += PAGE_SECONDS) {
    const to = Math.min(from + PAGE_SECONDS, toTs);
    if (out.length) await new Promise((r) => setTimeout(r, 1100)); // rate-limit, like the Rust path
    const url = `${BIRDEYE_HISTORY_URL}?address=${mint}&address_type=token&type=1m&time_from=${from}&time_to=${to}`;
    const res = await fetch(url, { headers: { "X-API-KEY": apiKey, "x-chain": "solana" } });
    if (!res.ok) {
      let msg = "";
      try { msg = (await res.json())?.message || ""; } catch { /* ignore */ }
      throw new Error(`Birdeye ${res.status}${msg ? ` — ${msg}` : ""}`);
    }
    const body = await res.json();
    const items = body?.data?.items || [];
    for (const it of items) {
      if (typeof it.unixTime === "number" && typeof it.value === "number" && it.value > 0)
        out.push({ ts: it.unixTime, p: it.value });
    }
  }
  out.sort((a, b) => a.ts - b.ts);
  return out;
}

// CoinGecko's keyless DEX API (GeckoTerminal). Birdeye fallback. Pool-based 1m OHLCV
// requested in USD — matching Birdeye's USD `value`, so the [{ts, p}] shape is identical
// and nothing downstream changes. Picks the deepest USD-reserve Solana pool for the mint
// (deepest liquidity ⇒ cleanest, most-continuous candles), then pages back via
// `before_timestamp` until `fromTs` is covered. `p` = the candle close.
// Keyless free tier is rate-limited (~30 req/min, with bursty 429s). Retry 429/5xx with
// backoff (honoring Retry-After) so one throttled page doesn't abort the whole backfill.
async function geckoFetch(url, tries = 5) {
  for (let i = 0; ; i++) {
    const res = await fetch(url, { headers: { accept: "application/json" } });
    if (res.ok) return res.json();
    if ((res.status === 429 || res.status >= 500) && i < tries - 1) {
      const ra = parseInt(res.headers.get("retry-after") || "", 10);
      // GeckoTerminal often sends `Retry-After: 0`; never trust a sub-floor value —
      // exponential backoff with a hard 5s floor so retries actually wait.
      const wait = Math.max(isFinite(ra) ? ra : 0, Math.min(2 ** i * 5, 60)) * 1000;
      console.warn(`  GeckoTerminal ${res.status} — backing off ${wait / 1000}s (retry ${i + 1}/${tries - 1})`);
      await new Promise((r) => setTimeout(r, wait));
      continue;
    }
    throw new Error(`GeckoTerminal ${res.status}`);
  }
}

async function geckoTerminalHistory(mint, fromTs, toTs) {
  const pools = (await geckoFetch(`${GECKO_BASE}/networks/solana/tokens/${mint}/pools?page=1`))?.data || [];
  if (!pools.length) throw new Error("no GeckoTerminal pools for this mint");
  pools.sort((a, b) =>
    parseFloat(b.attributes?.reserve_in_usd || 0) - parseFloat(a.attributes?.reserve_in_usd || 0));
  const top = pools[0].attributes;
  const pool = top.address;
  console.log(`  GeckoTerminal pool ${pool} (${top.name || "?"}, ` +
    `$${Math.round(parseFloat(top.reserve_in_usd || 0)).toLocaleString()} reserve)`);

  const byTs = new Map();
  let before = toTs;
  for (let page = 0; page < 60; page++) { // 60×1000 candles ≈ 41d — hard cap
    if (page) await new Promise((r) => setTimeout(r, 2100)); // free tier ~30 req/min
    const url = `${GECKO_BASE}/networks/solana/pools/${pool}/ohlcv/minute` +
      `?aggregate=1&limit=1000&currency=usd&before_timestamp=${before}`;
    const list = (await geckoFetch(url))?.data?.attributes?.ohlcv_list || [];
    if (!list.length) break;
    let oldest = Infinity;
    for (const row of list) {
      const ts = row[0], close = row[4];
      if (typeof ts === "number" && typeof close === "number" && close > 0) byTs.set(ts, close);
      if (typeof ts === "number" && ts < oldest) oldest = ts;
    }
    if (!isFinite(oldest) || oldest <= fromTs || oldest >= before) break; // covered, or no progress
    before = oldest;
  }
  return [...byTs.entries()]
    .filter(([ts]) => ts >= fromTs)
    .map(([ts, p]) => ({ ts, p }))
    .sort((a, b) => a.ts - b.ts);
}

// Current liquidity + 24h volume for the liquidity gate.
//
// Liquidity comes from Birdeye's /price?include_liquidity=true — the CHEAP endpoint
// that survives the compute-unit quota which kills /token_overview — so the figure
// stays on Birdeye's scale, which the $0.44M floor is calibrated to (Birdeye reads
// ~2-3x higher than GeckoTerminal/DexScreener). 24h volume comes from GeckoTerminal:
// /price carries no volume, and actual traded USD is far less source-dependent than
// liquidity (a marking choice), so mixing is safe — if anything it makes the vol/liq
// ratio slightly conservative (Birdeye's larger liquidity in the denominator).
// GeckoTerminal also backstops liquidity when Birdeye is fully unavailable (no key).
// Returns {liquidity, volume24, source} or null — null ⇒ caller REJECTs (don't add a
// token whose liquidity/turnover we couldn't confirm).
async function fetchLiquidity(apiKey, mint) {
  let birdeyeLiq = null;
  if (apiKey) {
    try {
      const res = await fetch(`${BIRDEYE_PRICE_URL}?address=${mint}&include_liquidity=true`,
        { headers: { "X-API-KEY": apiKey, "x-chain": "solana" } });
      if (res.ok) {
        const d = (await res.json())?.data;
        if (d && Number.isFinite(d.liquidity)) birdeyeLiq = +d.liquidity;
      } else {
        let msg = ""; try { msg = (await res.json())?.message || ""; } catch { /* ignore */ }
        console.warn(`  Birdeye price ${res.status}${msg ? ` — ${msg}` : ""} — using GeckoTerminal for liquidity too.`);
      }
    } catch (e) {
      console.warn(`  Birdeye price failed (${e.message}) — using GeckoTerminal for liquidity too.`);
    }
  }

  let gecko = null;
  try {
    const a = (await geckoFetch(`${GECKO_BASE}/networks/solana/tokens/${mint}`))?.data?.attributes;
    if (a && a.total_reserve_in_usd != null)
      gecko = { liquidity: parseFloat(a.total_reserve_in_usd) || 0, volume24: parseFloat(a.volume_usd?.h24) || 0 };
  } catch (e) {
    console.warn(`  GeckoTerminal token lookup failed (${e.message}).`);
  }

  if (birdeyeLiq != null) {
    // Birdeye-scale liquidity. Volume (for the ratio) must come from GeckoTerminal;
    // without it the turnover is unverifiable, so REJECT per policy.
    if (!gecko) return null;
    return { liquidity: birdeyeLiq, volume24: gecko.volume24, source: "Birdeye liq + GeckoTerminal vol" };
  }
  if (gecko) return { ...gecko, source: "GeckoTerminal" };
  return null;
}

function historyPath() {
  return process.env.HISTORY_PATH || path.join(REPO, "assets", "price_history.jsonl");
}

// SOL price keyed by minute bucket, read-only from the live history.
function loadSolByMinute() {
  const m = new Map();
  const p = historyPath();
  if (!fs.existsSync(p)) return m;
  for (const line of fs.readFileSync(p, "utf8").split("\n")) {
    if (!line) continue;
    let s; try { s = JSON.parse(line); } catch { continue; }
    const sol = s.prices && s.prices.SOL;
    if (typeof sol === "number" && sol > 0) m.set(Math.floor(s.ts / 60) * 60, sol);
  }
  return m;
}

// Fallback: read an existing candidate series from the live history (read-only) when
// Birdeye is unavailable or the token was already discovered/backfilled by the watcher.
function loadLocalSeries(mint) {
  const out = [];
  const p = historyPath();
  if (!fs.existsSync(p)) return out;
  for (const line of fs.readFileSync(p, "utf8").split("\n")) {
    if (!line) continue;
    let s; try { s = JSON.parse(line); } catch { continue; }
    const v = s.prices && s.prices[mint];
    if (typeof v === "number" && v > 0) out.push({ ts: s.ts, p: v });
  }
  out.sort((a, b) => a.ts - b.ts);
  return out;
}

function annualizedVolPct(points) {
  const rets = [];
  for (let i = 1; i < points.length; i++) {
    const r = Math.log(points[i].p / points[i - 1].p);
    if (isFinite(r) && Math.abs(r) < Math.log(8)) rets.push(r); // drop glitch ticks
  }
  if (rets.length < 2) return 0;
  const mean = rets.reduce((a, b) => a + b, 0) / rets.length;
  const sd = Math.sqrt(rets.reduce((a, b) => a + (b - mean) ** 2, 0) / (rets.length - 1));
  return sd * Math.sqrt(365 * 24 * 60) * 100; // per-minute -> annualized %
}

function ensureBinary() {
  const bin = path.join(REPO, "target", "release", "momentum-sim");
  if (!fs.existsSync(bin)) {
    console.log("Building momentum-sim (release)… first build can take a few minutes.");
    execFileSync("cargo", ["build", "--release", "--bin", "momentum-sim"], { stdio: "inherit" });
  }
  return bin;
}

function parseGrid(stdout, csvPath, minTrades) {
  const v = stdout.match(/VERDICT:\s*(\d+)\/(\d+) configs ROBUST/);
  const robust = v ? parseInt(v[1], 10) : 0;
  let bestWorst = -Infinity, bestTest = -Infinity, win = 0, dd = 0;
  const rows = fs.readFileSync(csvPath, "utf8").trim().split("\n");
  const head = rows[0].split(",");
  const col = (name) => head.indexOf(name);
  for (const line of rows.slice(1)) {
    const c = line.split(",");
    if (c[col("vol_stop_mode")] !== "off") continue;
    const test = parseFloat(c[col("net_pnl_test")]);
    const train = parseFloat(c[col("net_pnl_train")]);
    const tt = parseInt(c[col("n_trades_test")], 10);
    const tr = parseInt(c[col("n_trades_train")], 10);
    if (!(test > 0 && train > 0 && tt >= minTrades && tr >= minTrades)) continue;
    const worst = Math.min(test, train);
    if (worst > bestWorst) {
      bestWorst = worst; bestTest = test;
      win = parseFloat(c[col("win_rate_test")]); dd = parseFloat(c[col("max_dd_test")]);
    }
  }
  return { robust, bestWorst, bestTest, win, dd };
}

async function main() {
  loadEnv();
  const query = process.argv[2];
  if (!query || query.startsWith("--")) {
    console.error("Usage: node vet_token.js <ticker|name|mint> [--add] [--days N] [--vol-floor PCT] [--min-trades N] [--min-liquidity USD] [--min-vol-liq-ratio R] [--max-vol-liq-ratio R] [--force] [--source auto|birdeye|gecko]");
    process.exit(1);
  }
  const doAdd = process.argv.includes("--add");
  const force = process.argv.includes("--force");
  const days = parseInt(arg("--days", "30"), 10);
  const volFloor = parseFloat(arg("--vol-floor", "150"));
  const minTrades = parseInt(arg("--min-trades", "3"), 10);
  const minLiquidity = parseFloat(arg("--min-liquidity", "440000"));
  const minVolLiqRatio = parseFloat(arg("--min-vol-liq-ratio", "0.5"));
  const maxVolLiqRatio = parseFloat(arg("--max-vol-liq-ratio", "30")); // anti-wash cap (mirrors SCAN_MAX_RATIO)
  const sourceFlag = arg("--source", "auto").toLowerCase(); // auto | birdeye | gecko

  const apiKey = process.env.BIRDEYE_API_KEY;
  if (!apiKey && sourceFlag !== "gecko")
    console.warn("BIRDEYE_API_KEY not set — will backfill from GeckoTerminal (keyless) instead.");

  const tok = await resolveToken(query, force);
  if (tok.mint === USDC_MINT) throw new Error("USDC is the cash leg — never momentum-traded.");
  console.log(`Resolved: ${tok.symbol} — ${tok.name || "?"} [${tok.verified ? "verified" : tok.verified === false ? "UNVERIFIED" : "mint"}] ${tok.mint}`);
  if (tok.verified === false && !force)
    throw new Error("Resolved token is NOT Jupiter-verified. Pass the exact mint + --force to override.");

  // ── Liquidity gate — runs BEFORE the expensive backfill+grid so an illiquid token
  // is rejected fast (and without spending Birdeye history quota on it). Three checks:
  // a depth floor (tradeable size), a vol/liq turnover floor (not stale), and a
  // turnover cap (anti-wash — a token churning >30× its liquidity/day is almost
  // certainly wash-traded; mirrors SCAN_MAX_RATIO). Unverifiable liquidity REJECTs
  // rather than guesses, by policy. ──
  console.log("Checking liquidity / 24h volume…");
  const liqInfo = await fetchLiquidity(apiKey, tok.mint);
  const ratio = liqInfo && liqInfo.liquidity > 0 ? liqInfo.volume24 / liqInfo.liquidity : 0;
  const liquid = !!liqInfo && liqInfo.liquidity >= minLiquidity &&
    ratio >= minVolLiqRatio && ratio <= maxVolLiqRatio;
  if (liqInfo)
    console.log(`Liquidity (${liqInfo.source}): $${Math.round(liqInfo.liquidity).toLocaleString()}, ` +
      `24h vol $${Math.round(liqInfo.volume24).toLocaleString()}, vol/liq ${ratio.toFixed(2)}`);
  else
    console.log("Liquidity: UNAVAILABLE (Birdeye and GeckoTerminal both failed).");
  if (!liquid) {
    console.log("\n" + "=".repeat(64));
    console.log(`VERDICT for ${tok.symbol} (${tok.mint}):`);
    if (!liqInfo) {
      console.log("  liquid:      NO  (liquidity unverifiable — rejecting rather than guess)");
    } else {
      const why = liqInfo.liquidity < minLiquidity ? "too thin"
        : ratio > maxVolLiqRatio ? "wash-trade signal (turnover too high)"
        : "stale (turnover too low)";
      console.log(`  liquid:      NO  — ${why}  ($${Math.round(liqInfo.liquidity).toLocaleString()} vs floor ` +
        `$${minLiquidity.toLocaleString()}, vol/liq ${ratio.toFixed(2)} vs band ${minVolLiqRatio}–${maxVolLiqRatio})`);
    }
    console.log("  => REJECTED for the curated list (liquidity gate; backtest skipped)");
    console.log("=".repeat(64));
    return;
  }

  const now = Math.floor(Date.now() / 1000);
  const fromTs = now - days * 86400;
  let cand = [];
  let source = "";

  // Birdeye 1m first (unless --source gecko), then GeckoTerminal (keyless), then local.
  if (apiKey && sourceFlag !== "gecko") {
    console.log(`Backfilling ${days}d of 1m history from Birdeye…`);
    try {
      cand = await birdeyeHistory(apiKey, tok.mint, fromTs, now);
      source = "Birdeye 1m";
    } catch (e) {
      console.warn(`Birdeye backfill failed (${e.message}).`);
    }
  }
  if (cand.length < MIN_OBS && sourceFlag !== "birdeye") {
    console.log(`Backfilling ${days}d of 1m history from GeckoTerminal…`);
    try {
      const gecko = await geckoTerminalHistory(tok.mint, fromTs, now);
      if (gecko.length > cand.length) { cand = gecko; source = "GeckoTerminal 1m"; }
    } catch (e) {
      console.warn(`GeckoTerminal backfill failed (${e.message}).`);
    }
  }
  // Fall back to any history the watcher already has for this mint (read-only).
  if (cand.length < MIN_OBS) {
    const local = loadLocalSeries(tok.mint);
    if (local.length > cand.length) {
      cand = local;
      source = "existing local price_history.jsonl";
      console.warn(`Using ${cand.length} obs from local history instead.`);
    }
  }
  if (cand.length < MIN_OBS) {
    console.log(`\nInsufficient history: only ${cand.length} obs (<${MIN_OBS}) from Birdeye, GeckoTerminal, or local. ` +
      `Can't vet ${tok.symbol} — retry when a data source recovers, or once the watcher has backfilled it.`);
    process.exit(2);
  }

  const solByMin = loadSolByMinute();
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "vet-"));
  const histPath = path.join(tmp, "history.jsonl");
  const tokensPath = path.join(tmp, "tokens.json");
  const csvPath = path.join(tmp, "grid.csv");
  const ws = fs.createWriteStream(histPath);
  let solHits = 0;
  for (const { ts, p } of cand) {
    const prices = { [tok.mint]: p };
    // SOL is recorded sparsely (~every few min); search out to +-3min for the nearest bucket.
    let sol;
    for (let d = 0; d <= 180 && !sol; d += 60)
      sol = solByMin.get(ts - d) || solByMin.get(ts + d);
    if (sol) { prices.SOL = sol; solHits++; }
    ws.write(JSON.stringify({ ts, prices }) + "\n");
  }
  ws.end();
  await new Promise((r) => ws.on("finish", r));
  fs.writeFileSync(tokensPath, JSON.stringify([{ symbol: tok.symbol, mint: tok.mint, name: tok.name || tok.symbol }], null, 2) + "\n");

  const annVol = annualizedVolPct(cand);
  const spanH = ((cand.at(-1).ts - cand[0].ts) / 3600).toFixed(0);
  console.log(`History (${source}): ${cand.length} obs, ${spanH}h span, SOL co-located on ${(100 * solHits / cand.length).toFixed(0)}% of bars.`);
  console.log(`Annualized vol (spike-filtered): ${annVol.toFixed(0)}%`);

  const bin = ensureBinary();
  console.log("Running grid (fixed-trail only)…");
  // Regime OFF for vetting: SOL is too sparse in the temp history to gate reliably, and a
  // degenerate SOL series would manufacture spurious regime-gated "robust" configs. Regime
  // off is the honest baseline — if the token has an edge without it, that's a real signal.
  // (The live trader applies its own regime; optimize-momentum-config tunes it later.)
  const out = execFileSync(bin, ["run", "--history", histPath, "--tokens", tokensPath,
    "--no-vol-stops", "--min-trades", String(minTrades), "--top", "3", "--csv", csvPath],
    { encoding: "utf8" });
  const g = parseGrid(out, csvPath, minTrades);

  const volatile = annVol >= volFloor;
  const profitable = g.robust >= 1 && g.bestWorst > 0;
  const qualifies = liquid && volatile && profitable;

  console.log("\n" + "=".repeat(64));
  console.log(`VERDICT for ${tok.symbol} (${tok.mint}):`);
  console.log(`  liquid:      YES  ($${Math.round(liqInfo.liquidity).toLocaleString()} ` +
    `[${liqInfo.source}], vol/liq ${ratio.toFixed(2)})`);
  console.log(`  volatile:    ${volatile ? "YES" : "NO"}  (${annVol.toFixed(0)}% vs floor ${volFloor}%)`);
  console.log(`  pnl-positive: ${profitable ? "YES" : "NO"}  (${g.robust} robust configs; ` +
    `best worst-slice ${isFinite(g.bestWorst) ? g.bestWorst.toFixed(2) : "n/a"}, ` +
    `best test ${isFinite(g.bestTest) ? g.bestTest.toFixed(2) : "n/a"}, ` +
    `win ${g.win || 0}%, maxDD ${g.dd || 0}%)`);
  console.log(`  => ${qualifies ? "QUALIFIES" : "REJECTED"} for the curated list`);
  console.log("=".repeat(64));

  if (!qualifies) {
    console.log("\nNot added. (Need liquid AND volatile AND >=1 robust profitable config.)");
    return;
  }
  if (doAdd) {
    console.log("\nAdding to the curated list…");
    execFileSync("node", [path.join(REPO, "scripts", "add_momentum_token.js"), tok.mint, tok.symbol],
      { stdio: "inherit" });
    console.log("Done. Restart the portfolio-watcher so it backfills the token into the live history.");
  } else {
    console.log(`\nQualifies. Re-run with --add to append ${tok.symbol} to the curated list.`);
  }
  console.log(`\nCaveat: backtest on ${source} over a finite window — small trade counts and ` +
    "understated drawdown are common. Validate in paper mode before trusting it live.");
}

main().catch((e) => { console.error("\nError:", e.message); process.exit(1); });
