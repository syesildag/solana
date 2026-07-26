#!/usr/bin/env node
"use strict";
/**
 * Generic liquid-token scanner for the momentum trader's live discovery overlay.
 *
 * Source: MOMENTUM_SCAN_SOURCE=trending (default, Birdeye /defi/token_trending feed,
 * limited to MOMENTUM_SCAN_TRENDING_LIMIT top-20) | volume (legacy /defi/tokenlist
 * paginated scan). Filters: drop stables/wrapped → drop already-curated → volume &
 * liquidity floors + anti-wash vol/liq ratio cap → Jupiter-verified only → emit.
 *
 * Output modes:
 *   --json    print [{symbol, mint, name, vol24, liq, change24h}] (volume-sorted) to stdout.
 *             THE LIVE PATH — the portfolio-watcher spawns `node scan_tokens.js --json`.
 *             Never writes any file.
 *   --apply   append new survivors to MOMENTUM_TOKENS_PATH (manual one-off only).
 *   (none)    human-readable table.
 *
 * Env: BIRDEYE_API_KEY (required), SCAN_MIN_VOLUME (250000), SCAN_MIN_LIQUIDITY
 * (440000), SCAN_MIN_RATIO (0.5; anti-stale vol/liq floor), SCAN_MAX_RATIO (30;
 * anti-wash vol/liq cap), SCAN_LIMIT (100), MOMENTUM_TOKENS_PATH,
 * MOMENTUM_JUPITER_API_URL,
 * MOMENTUM_SCAN_RANK ("volume" default | "change" — order survivors by 24h price-change),
 * MOMENTUM_SCAN_MAX_CHANGE_PCT (50; change ceiling when rank="change"; 0 = off),
 * MOMENTUM_SCAN_CHANGE_WINDOW ("24h" default; "1h"/"2h"/"4h"/"8h" rank survivors by
 * Birdeye priceChange<window>Percent instead — "4h" matches the live trader's
 * return-over-LOOKBACK_OBS(240) metric, at one extra Birdeye call per survivor),
 * SCAN_MAX_TOP_HOLDERS_PCT (30; reject when Jupiter audit.topHoldersPercentage exceeds
 * this — whale-concentration rug guard; 0 = off. Mint/freeze authority must not be
 * explicitly enabled either), SCAN_POOL_ENRICH_MAX (5; top-N survivors get a DexScreener
 * best-pool lookup — pumpswap pools are emitted as pool/quote for dynamic gRPC wiring; 0 = off).
 */
require("./lib/load_env"); // auto-load repo-root .env (RPC_URL for the on-chain safety screen, Birdeye key, …)
const fs = require("fs");
const path = require("path");
const { USDC_MINT, MINT_RE, getVerifiedToken } = require("./lib/jup");
const { fetchMintSafety } = require("./lib/token_safety");

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
  minLiquidity: numEnv("SCAN_MIN_LIQUIDITY", 440_000),
  minRatio: numEnv("SCAN_MIN_RATIO", 0.5),
  maxRatio: numEnv("SCAN_MAX_RATIO", 30),
  // Birdeye returns 50/page (its hard cap); page until volume drops below the floor
  // or this many pages. 15 → top ~750 by volume, deep enough to reach the $250k floor.
  maxPages: numEnv("SCAN_MAX_PAGES", 15),
  // Cap Jupiter verify calls (only the top-N survivors are ever kept downstream).
  verifyMax: numEnv("SCAN_VERIFY_MAX", 25),
  // Holder-concentration ceiling (Jupiter audit.topHoldersPercentage): a token whose
  // top-10 holders own more than this % of supply is one whale-exit away from a dump
  // the trailing stop gaps through. 45%+ concentrations passed every price gate before
  // this existed (SOLANGELES incident, 2026-07-22). 0 disables the gate.
  maxTopHoldersPct: numEnv("SCAN_MAX_TOP_HOLDERS_PCT", 30),
  // Ordering of discovered candidates: "volume" (default — by 24h volume, the historical
  // behavior) or "change" (by Birdeye 24h price-change, within the band below — surfaces
  // hot movers instead of flat giants). The volume/liquidity/wash floors gate either way.
  rank: (process.env.MOMENTUM_SCAN_RANK || "volume").trim().toLowerCase(),
  // When rank="change": drop already-parabolic movers above this % (likely exhausted /
  // would be rejected by the entry over-extension guard). 0 = no ceiling.
  maxChangePct: numEnv("MOMENTUM_SCAN_MAX_CHANGE_PCT", 50),
  // Horizon for the change ranking. "24h" (default) uses the inline trending field —
  // zero extra calls but stale for the trader's purpose: the live entry metric is
  // `return` over LOOKBACK_OBS (240 obs ≈ 4h), so a 12h-old pump tops the 24h ranking
  // while its 4h return is already flat. "4h" (or 1h/2h/8h) fetches Birdeye's
  // priceChange<window>Percent per survivor instead — one paced call each, aligning
  // discovery with what the trader can actually enter.
  changeWindow: (process.env.MOMENTUM_SCAN_CHANGE_WINDOW || "24h").trim().toLowerCase(),
  // Discovery source: "trending" (default — /defi/token_trending, one call, carries 24h
  // change inline) or "volume" (the legacy paginated /defi/tokenlist path).
  source: (process.env.MOMENTUM_SCAN_SOURCE || "trending").trim().toLowerCase(),
  // How many trending tokens to request when source="trending".
  trendingLimit: numEnv("MOMENTUM_SCAN_TRENDING_LIMIT", 20),
  // How many top survivors get a DexScreener best-pool lookup so the watcher can
  // gRPC-wire them dynamically (spec 2026-07-22). Only pumpswap SOL/USDC pools are
  // wireable; others stay REST. 0 disables enrichment entirely.
  poolEnrichMax: numEnv("SCAN_POOL_ENRICH_MAX", 5),
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
      const ratio = vol / liq;
      // floor rejects stale/untraded names, cap rejects wash trades. minRatio
      // defaults to 0 (no floor) so callers that omit it keep the old behavior.
      return ratio >= (opts.minRatio || 0) && ratio <= opts.maxRatio;
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

/**
 * PURE safety gate on a Jupiter token record's `audit` block. Returns a human-readable
 * reject reason, or null if the token passes. The scanner auto-watches whatever survives,
 * so this fails CLOSED on missing audit data: a token we can't assess is a token we
 * don't auto-watch (manual adds via add_momentum_token.js are unaffected).
 * Authority flags reject only on explicit `false` (true = renounced/disabled = safe;
 * absent = not reported by this listing, covered by the concentration number instead).
 */
function auditRejectReason(token, maxTopHoldersPct) {
  const audit = token && token.audit;
  if (!audit || typeof audit !== "object") return "no audit data";
  if (audit.mintAuthorityDisabled === false) return "mint authority still enabled";
  if (audit.freezeAuthorityDisabled === false) return "freeze authority still enabled";
  const pct = audit.topHoldersPercentage;
  if (maxTopHoldersPct > 0) {
    if (!Number.isFinite(+pct)) return "no top-holders data";
    if (+pct > maxTopHoldersPct) {
      return `top-10 holders own ${(+pct).toFixed(1)}% > ${maxTopHoldersPct}% cap`;
    }
  }
  return null;
}

// DexScreener dexIds the momentum gRPC pricer can decode (see feed_setup.rs): pumpswap
// (CP, vault reserves) + raydium / orca / meteora (CL, state account sqrt_price). The
// "raydium"/"meteora" ids are coarse (AMM-vs-CLMM, DLMM-vs-DAMM), but the matching fetcher
// auto-detects and a wrong guess just fails decode → REST fallback (safe by construction).
const GRPC_DEX_IDS = new Set(["pumpswap", "raydium", "orca", "meteora"]);

/**
 * PURE pool picker for scan discoveries: from a DexScreener `pairs` array, return
 * {pool, quote, dex} for the HIGHEST-24h-VOLUME pair on a gRPC-priceable venue
 * (GRPC_DEX_IDS) with a SOL/USDC quote — the venues the watcher can decode+wire
 * dynamically. Volume, never liquidity, picks the pool (fake-TVL rule). `dex` tells the
 * watcher which fetcher decodes it. Null = token stays REST-priced.
 */
function pickBestGrpcPool(pairs) {
  if (!Array.isArray(pairs) || pairs.length === 0) return null;
  const eligible = (pairs || []).filter((p) => {
    if (!p || !GRPC_DEX_IDS.has(p.dexId)) return false;
    if (!MINT_RE.test(p.pairAddress || "")) return false;
    const q = ((p.quoteToken && p.quoteToken.symbol) || "").toUpperCase();
    return q === "SOL" || q === "WSOL" || q === "USDC";
  });
  if (eligible.length === 0) return null;
  const best = eligible.sort(
    (a, b) => ((b.volume && +b.volume.h24) || 0) - ((a.volume && +a.volume.h24) || 0)
  )[0];
  const q = ((best.quoteToken && best.quoteToken.symbol) || "").toUpperCase();
  return { pool: best.pairAddress, quote: q === "USDC" ? "USDC" : "SOL", dex: best.dexId };
}

// Annotate the top survivors with a dynamically wireable pool. Best-effort per
// token: any fetch/parse failure leaves the row pool-less (REST-priced) — the
// scan itself never fails because of enrichment.
async function annotatePools(survivors, maxN) {
  for (let i = 0; i < Math.min(survivors.length, maxN); i++) {
    if (i > 0) await sleep(250);
    const s = survivors[i];
    try {
      const res = await fetch(`https://api.dexscreener.com/latest/dex/tokens/${s.mint}`, {
        headers: { accept: "application/json" },
      });
      if (!res.ok) continue;
      const body = await res.json();
      const picked = pickBestGrpcPool((body && body.pairs) || []);
      if (picked) {
        s.pool = picked.pool;
        s.quote = picked.quote;
        s.dex = picked.dex;
      } else {
        console.error(`  scan: ${s.symbol} no gRPC-priceable SOL/USDC pool — REST-priced`);
      }
    } catch (_) { /* REST-priced */ }
  }
  return survivors;
}

async function verifyAll(cands) {
  // Sequential + paced to respect the public Jupiter tier's rate limit (a 429 would
  // fail-closed and silently drop a real token, so pacing matters).
  const out = [];
  for (let i = 0; i < cands.length; i++) {
    if (i > 0) await sleep(120);
    const tok = await getVerifiedToken(cands[i].address);
    if (!tok) continue; // unverified or fetch failed — fail-closed as before
    const reject = auditRejectReason(tok, OPTS.maxTopHoldersPct);
    if (reject) {
      console.error(`  scan: ${cands[i].symbol || cands[i].address} REJECTED — ${reject}`);
      continue;
    }
    out.push(cands[i]);
  }
  return out;
}

// A survivor still needs a per-mint change fetch only if it has no finite change24h yet
// (the volume path). Trending rows arrive with change24h inline, so they skip the fetch.
const needsChange = (s) => !Number.isFinite(s.change24h);

// Birdeye price-change % for one mint over `window` (token_overview field
// priceChange<window>Percent, e.g. 4h → priceChange4hPercent). Used only when ranking
// by change, for the top-N-by-volume verified survivors. Returns null on any
// error/missing so the caller drops it from the momentum band (a candidate with no
// readable signal at this horizon is not a momentum candidate).
async function fetchChange(mint, window) {
  const key = process.env.BIRDEYE_API_KEY || "";
  try {
    const res = await fetch(
      `https://public-api.birdeye.so/defi/token_overview?address=${mint}`,
      { headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" } }
    );
    if (!res.ok) return null;
    const body = await res.json();
    const c = body && body.data && body.data[`priceChange${window}Percent`];
    return Number.isFinite(+c) ? +c : null;
  } catch {
    return null;
  }
}

// Annotate each survivor with `change24h` (the ranking field — named for its historical
// default; it holds the `changeWindow` horizon). Sequential + paced, like verifyAll.
async function annotateChange(survivors, window) {
  for (let i = 0; i < survivors.length; i++) {
    if (i > 0) await sleep(120);
    survivors[i].change24h = await fetchChange(survivors[i].mint, window);
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
  // Momentum ordering: fetch price-change for the (already top-by-volume) survivors,
  // then band + sort by it. Volume ordering needs no extra calls. A non-24h window
  // invalidates the inline trending 24h numbers — refetch every survivor at the
  // configured horizon so the ranking field is horizon-consistent.
  if (OPTS.rank === "change") {
    if (OPTS.changeWindow !== "24h") survivors.forEach((s) => { s.change24h = null; });
    await annotateChange(survivors.filter(needsChange), OPTS.changeWindow);
  }
  survivors = rankSurvivors(survivors, OPTS);

  // On-chain Token-2022 safety screen. A HELD momentum position can be trapped exactly like
  // arb capital between legs: a transfer hook can block the sell, defaultAccountState=frozen
  // traps the fill, a live freeze authority can freeze it. Freeze authority is also flagged by
  // the Jupiter audit above (auditRejectReason); this adds the authoritative on-chain read plus
  // the two Token-2022 traps the audit does not surface. RPC_URL is present in every real
  // invocation (arb scanner child / momentum watcher); if absent, warn and skip rather than
  // reject the whole discovery — no worse than the prior no-check behaviour.
  const safetyRpc = process.env.RPC_URL;
  if (safetyRpc && survivors.length) {
    const safety = await fetchMintSafety(safetyRpc, survivors.map((s) => s.mint));
    survivors = survivors.filter((s) => {
      const info = safety.get(s.mint);
      if (info && info.safe) return true;
      console.error(`  scan: ${s.symbol} REJECTED — ${(info && info.reasons.join("; ")) || "mint safety unknown"}`);
      return false;
    });
  } else if (!safetyRpc) {
    console.error("  scan: RPC_URL unset — skipping on-chain token-2022 safety screen");
  }

  if (OPTS.poolEnrichMax > 0) {
    await annotatePools(survivors, OPTS.poolEnrichMax);
  }

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

module.exports = { filterCandidates, rankSurvivors, mapTrendingToken, needsChange, auditRejectReason, pickBestGrpcPool };

if (require.main === module) {
  main().catch((e) => {
    console.error(`✗ ${e.message}`);
    process.exit(1);
  });
}
