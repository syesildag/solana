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
 * MOMENTUM_JUPITER_API_URL,
 * MOMENTUM_SCAN_RANK ("volume" default | "change" — order survivors by 24h price-change),
 * MOMENTUM_SCAN_MAX_CHANGE_PCT (50; 24h-change ceiling when rank="change"; 0 = off).
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
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const OPTS = {
  minVolume: numEnv("SCAN_MIN_VOLUME", 250_000),
  minLiquidity: numEnv("SCAN_MIN_LIQUIDITY", 200_000),
  maxRatio: numEnv("SCAN_MAX_RATIO", 30),
  // Birdeye returns 50/page (its hard cap); page until volume drops below the floor
  // or this many pages. 15 → top ~750 by volume, deep enough to reach the $250k floor.
  maxPages: numEnv("SCAN_MAX_PAGES", 15),
  // Cap Jupiter verify calls (only the top-N survivors are ever kept downstream).
  verifyMax: numEnv("SCAN_VERIFY_MAX", 25),
  // Ordering of discovered candidates: "volume" (default — by 24h volume, the historical
  // behavior) or "change" (by Birdeye 24h price-change, within the band below — surfaces
  // hot movers instead of flat giants). The volume/liquidity/wash floors gate either way.
  rank: (process.env.MOMENTUM_SCAN_RANK || "volume").trim().toLowerCase(),
  // When rank="change": drop already-parabolic movers above this 24h % (likely exhausted /
  // would be rejected by the entry over-extension guard). 0 = no ceiling.
  maxChangePct: numEnv("MOMENTUM_SCAN_MAX_CHANGE_PCT", 50),
  // Discovery source: "trending" (default — /defi/token_trending, one call, carries 24h
  // change inline) or "volume" (the legacy paginated /defi/tokenlist path).
  source: (process.env.MOMENTUM_SCAN_SOURCE || "trending").trim().toLowerCase(),
  // How many trending tokens to request when source="trending".
  trendingLimit: numEnv("MOMENTUM_SCAN_TRENDING_LIMIT", 20),
};

// Stablecoins + wrapped SOL: never momentum candidates.
const DENY_MINTS = new Set([
  USDC_MINT,
  "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", // USDT
  "So11111111111111111111111111111111111111112",  // wSOL
]);
// Stablecoins/cash legs have no momentum. A "usd" anywhere in the SYMBOL (USDC,
// USDT, USDe, JupUSD, PYUSD, …) is a near-certain stable; likewise eur/dai/gyen.
const DENY_SYM_RE = /usd|eur|^dai$|gyen/i;

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

/**
 * PURE ordering of gated+verified survivors (network-free, deterministic). Unit-tested.
 *  - rank "volume": survivors by 24h volume (`vol24`) desc — the historical order.
 *  - rank "change": keep only up-movers within the band `0 < change24h <= ceiling`
 *    (ceiling = `maxChangePct`, or no upper bound when `maxChangePct <= 0`), then sort
 *    by `change24h` desc. Survivors missing a finite `change24h` are dropped (we couldn't
 *    read a momentum signal, so they can't be momentum-ranked).
 * The volume/liquidity/wash gate already ran in `filterCandidates`; this only reorders.
 */
function rankSurvivors(survivors, { rank, maxChangePct }) {
  if (rank !== "change") {
    return [...survivors].sort((a, b) => (+b.vol24 || 0) - (+a.vol24 || 0));
  }
  const ceiling = maxChangePct > 0 ? maxChangePct : Infinity;
  return survivors
    .filter((s) => Number.isFinite(s.change24h) && s.change24h > 0 && s.change24h <= ceiling)
    .sort((a, b) => b.change24h - a.change24h);
}

// Page through Birdeye's volume-sorted tokenlist (50/req — its hard cap) until a page's
// cheapest token drops below the volume floor (the list is desc, so every deeper token
// is below it too) or maxPages is hit. A single top-50 page only ever sees the
// multi-$M giants — paging is what lets the floor actually admit mid-volume names
// (tokenized stocks like SLX at ~$1.7M sit far below the #50 cutoff of ~$7M).
async function fetchBirdeyeTopVolume(minVolume, maxPages) {
  const key = process.env.BIRDEYE_API_KEY || "";
  if (!key) throw new Error("BIRDEYE_API_KEY is not set");
  const all = [];
  for (let page = 0; page < maxPages; page++) {
    const offset = page * 50;
    const url =
      `https://public-api.birdeye.so/defi/tokenlist` +
      `?sort_by=v24hUSD&sort_type=desc&offset=${offset}&limit=50`;
    const res = await fetch(url, {
      headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" },
    });
    if (!res.ok) {
      // Mid-pagination rate-limit: keep what we already have; only fail if page 0.
      if (all.length) break;
      throw new Error(`Birdeye tokenlist -> HTTP ${res.status}`);
    }
    const body = await res.json();
    const tokens = (body && body.data && body.data.tokens) || [];
    if (!tokens.length) break;
    for (const t of tokens) {
      all.push({
        address: t.address,
        symbol: t.symbol || "",
        name: t.name || "",
        v24hUSD: +t.v24hUSD || 0,
        liquidity: +t.liquidity || 0,
      });
    }
    if ((+tokens[tokens.length - 1].v24hUSD || 0) < minVolume) break; // past the floor
    await sleep(1000); // pace for Birdeye's free-tier rate limiter (paged calls get 401'd if too fast)
  }
  return all;
}

// Map one Birdeye `/defi/token_trending` token to the candidate-row shape used by
// filterCandidates. Trending carries 24h change inline (price24hChangePercent), so the
// change-rank path needs no extra per-mint fetch. Non-finite change → null (dropped by band).
function mapTrendingToken(t) {
  const c = +t.price24hChangePercent;
  return {
    address: t.address,
    symbol: t.symbol || "",
    name: t.name || "",
    v24hUSD: +t.volume24hUSD || 0,
    liquidity: +t.liquidity || 0,
    change24h: Number.isFinite(c) ? c : null,
  };
}

// Birdeye trending feed — a single call that returns hot movers with volume, liquidity, and
// 24h price-change inline (no pagination, no per-mint change fetch). Free-tier accessible.
async function fetchBirdeyeTrending(limit) {
  const key = process.env.BIRDEYE_API_KEY || "";
  if (!key) throw new Error("BIRDEYE_API_KEY is not set");
  const url =
    `https://public-api.birdeye.so/defi/token_trending` +
    `?sort_by=rank&sort_type=asc&offset=0&limit=${limit}`;
  const res = await fetch(url, {
    headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" },
  });
  if (!res.ok) throw new Error(`Birdeye token_trending -> HTTP ${res.status}`);
  const body = await res.json();
  const tokens = (body && body.data && body.data.tokens) || [];
  return tokens.map(mapTrendingToken);
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
  // Sequential + paced to respect the public Jupiter tier's rate limit (a 429 would
  // fail-closed and silently drop a real token, so pacing matters).
  const out = [];
  for (let i = 0; i < cands.length; i++) {
    if (i > 0) await sleep(120);
    if (await isVerifiedMint(cands[i].address)) out.push(cands[i]);
  }
  return out;
}

// A survivor still needs a per-mint change fetch only if it has no finite change24h yet
// (the volume path). Trending rows arrive with change24h inline, so they skip the fetch.
const needsChange = (s) => !Number.isFinite(s.change24h);

// Birdeye 24h price-change % for one mint (token_overview). Used only when ranking by
// change, for the top-N-by-volume verified survivors. Returns null on any error/missing
// so the caller drops it from the momentum band (a candidate with no readable signal).
async function fetchChange24h(mint) {
  const key = process.env.BIRDEYE_API_KEY || "";
  try {
    const res = await fetch(
      `https://public-api.birdeye.so/defi/token_overview?address=${mint}`,
      { headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" } }
    );
    if (!res.ok) return null;
    const body = await res.json();
    const c = body && body.data && body.data.priceChange24hPercent;
    return Number.isFinite(+c) ? +c : null;
  } catch {
    return null;
  }
}

// Annotate each survivor with `change24h` (sequential + paced, like verifyAll).
async function annotateChange24h(survivors) {
  for (let i = 0; i < survivors.length; i++) {
    if (i > 0) await sleep(120);
    survivors[i].change24h = await fetchChange24h(survivors[i].mint);
  }
  return survivors;
}

const fmtNum = (n) => Math.round(n).toLocaleString("en-US");

async function main() {
  const args = process.argv.slice(2);
  const asJson = args.includes("--json");
  const apply = args.includes("--apply");

  const rows = OPTS.source === "volume"
    ? await fetchBirdeyeTopVolume(OPTS.minVolume, OPTS.maxPages)
    : await fetchBirdeyeTrending(OPTS.trendingLimit);
  const filtered = filterCandidates(rows, curatedMintsFromFile(), OPTS);
  // Only verify the top-by-volume survivors — downstream keeps just the top-N anyway.
  const verified = await verifyAll(filtered.slice(0, OPTS.verifyMax));
  let survivors = verified.map((r) => ({
    symbol: r.symbol, mint: r.address, name: r.name, vol24: r.v24hUSD, liq: r.liquidity, change24h: r.change24h,
  }));
  // Momentum ordering: fetch 24h price-change for the (already top-by-volume) survivors,
  // then band + sort by it. Volume ordering needs no extra calls.
  if (OPTS.rank === "change") {
    await annotateChange24h(survivors.filter(needsChange));
  }
  survivors = rankSurvivors(survivors, OPTS);

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
  const order = OPTS.rank === "change"
    ? `change desc, band (0, ${OPTS.maxChangePct || "∞"}]%`
    : "volume desc";
  console.log(
    `Scanned ${rows.length} via ${OPTS.source} → ${filtered.length} passed filters → ` +
      `${survivors.length} kept (rank=${OPTS.rank}, ${order})`
  );
  for (const s of survivors) {
    const chg = Number.isFinite(s.change24h) ? `chg=${s.change24h >= 0 ? "+" : ""}${s.change24h.toFixed(1)}% ` : "";
    console.log(
      `  ${s.symbol.padEnd(10)} ${chg}vol=$${fmtNum(s.vol24)} liq=$${fmtNum(s.liq)} ` +
        `ratio=${(s.vol24 / Math.max(s.liq, 1)).toFixed(1)}  ${s.mint}`
    );
  }
}

module.exports = { filterCandidates, rankSurvivors, mapTrendingToken, needsChange };

if (require.main === module) {
  main().catch((e) => {
    console.error(`✗ ${e.message}`);
    process.exit(1);
  });
}
