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
 * MOMENTUM_SCAN_RANK ("volume" default | "change" — order survivors by 24h price-change |
 *   "slope" — order by GT-OHLCV ln-slope×R² over MOMENTUM_SCAN_CHANGE_WINDOW, positive only:
 *   the trader ranks entries by slope_r2, so discovery hands it slope-positive candidates,
 *   not price-up-but-rolling-over ones; SCAN_SLOPE_MAX (6) bounds the paced GT fetches),
 * MOMENTUM_SCAN_MAX_CHANGE_PCT (50; change ceiling when rank="change"; 0 = off),
 * MOMENTUM_SCAN_CHANGE_WINDOW ("24h" default; "1h"/"2h"/"4h"/"8h" rank survivors by
 * Birdeye priceChange<window>Percent instead — "4h" matches the live trader's
 * return-over-LOOKBACK_OBS(240) metric, at one extra Birdeye call per survivor),
 * SCAN_MAX_TOP_HOLDERS_PCT (30; reject when Jupiter audit.topHoldersPercentage exceeds
 * this — whale-concentration rug guard; 0 = off. Mint/freeze authority must not be
 * explicitly enabled either), SCAN_MIN_ORGANIC_SCORE (20; reject when Jupiter's
 * organicScore falls below this — bot-farmed/wash-volume guard; 0 = off),
 * SCAN_POOL_ENRICH_MAX (5; top-N survivors get a DexScreener
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
  // Require Jupiter's verified-token flag (default true). false = skip the Jupiter
  // verification fetch AND its audit gates (top-holders) entirely. The on-chain
  // token_safety screen still runs downstream and is the authoritative trap check
  // (freeze authority, transfer hook, frozen default state) — what's lost is only
  // Jupiter's curation signal, which arb (atomic in-and-out) doesn't need.
  requireJupVerified: String(process.env.SCAN_REQUIRE_JUP_VERIFY ?? "true") === "true",
  // Holder-concentration ceiling (Jupiter audit.topHoldersPercentage): a token whose
  // top-10 holders own more than this % of supply is one whale-exit away from a dump
  // the trailing stop gaps through. 45%+ concentrations passed every price gate before
  // this existed (SOLANGELES incident, 2026-07-22). 0 disables the gate.
  maxTopHoldersPct: numEnv("SCAN_MAX_TOP_HOLDERS_PCT", 30),
  // Organic-volume floor (Jupiter organicScore, 0–100): a token whose volume is
  // manufactured — Sybil buy-bots painting the chart — sails through every size/shape
  // gate above (GDWR/NVDA incidents, 2026-08-08: $6M+ daily volume, organicScore 0,
  // $1 of organic buys). Calibration: JitoSOL 96, WIF 80, W 52, borderline memes ~40,
  // bot farms 0 — so 20 kills the manufactured class without curating real tokens.
  // 0 disables the gate.
  minOrganicScore: numEnv("SCAN_MIN_ORGANIC_SCORE", 20),
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
  // rank="slope": how many top-volume finalists get a GT OHLCV slope fetch (2.1s paced each).
  slopeMax: +(process.env.SCAN_SLOPE_MAX || 6),
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
 * PURE filter funnel (network-free, deterministic) — same gates as the historical
 * filterCandidates, but every drop is classified with a stage + human-readable reason
 * so the hourly scan can explain WHY the candidate set shrank. Unit-tested.
 * Stages: mint (malformed address) → deny (stable/wrapped) → curated (already watched)
 * → floors (volume/liquidity) → wash (vol/liq ratio outside [minRatio, maxRatio]).
 * Returns { passed (volume-desc sorted), drops: [{symbol, address, stage, reason}] }.
 */
function classifyCandidates(rows, curatedMints, opts) {
  const curated = new Set(curatedMints);
  const passed = [];
  const drops = [];
  const drop = (r, stage, reason) =>
    drops.push({ symbol: r.symbol || r.address || "?", address: r.address, stage, reason });
  for (const r of rows) {
    if (!r || !MINT_RE.test(r.address || "")) {
      drop(r || {}, "mint", "malformed mint address");
      continue;
    }
    if (DENY_MINTS.has(r.address) || DENY_SYM_RE.test(r.symbol || "")) {
      drop(r, "deny", "stablecoin/wrapped denylist");
      continue;
    }
    if (curated.has(r.address)) {
      drop(r, "curated", "already on the curated watch list");
      continue;
    }
    const vol = +r.v24hUSD || 0;
    const liq = +r.liquidity || 0;
    if (vol < opts.minVolume || liq < opts.minLiquidity) {
      drop(r, "floors", `vol=$${Math.round(vol).toLocaleString("en-US")} liq=$${Math.round(liq).toLocaleString("en-US")} below floors ($${opts.minVolume.toLocaleString("en-US")}/$${opts.minLiquidity.toLocaleString("en-US")})`);
      continue;
    }
    const ratio = vol / liq;
    // floor rejects stale/untraded names, cap rejects wash trades. minRatio
    // defaults to 0 (no floor) so callers that omit it keep the old behavior.
    if (ratio < (opts.minRatio || 0) || ratio > opts.maxRatio) {
      drop(r, "wash", `vol/liq ratio ${ratio.toFixed(1)} outside [${opts.minRatio || 0}, ${opts.maxRatio}]`);
      continue;
    }
    passed.push(r);
  }
  passed.sort((a, b) => (+b.v24hUSD || 0) - (+a.v24hUSD || 0));
  return { passed, drops };
}

/**
 * PURE filter (network-free, deterministic) — denylist + dedup-vs-curated +
 * floors + anti-wash ratio, sorted by 24h volume desc. Thin wrapper over
 * classifyCandidates for callers that don't need drop diagnostics. Unit-tested.
 */
function filterCandidates(rows, curatedMints, opts) {
  return classifyCandidates(rows, curatedMints, opts).passed;
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
  // rank "slope": keep only tokens whose recent ln-price regression slope×R² is POSITIVE
  // (annotateSlope fills `slopeScore` from GT OHLCV), sorted best-trend first. A token
  // whose 24h change is up but whose slope is negative (pumped, now rolling over —
  // the Jimothy case, 2026-07-28: +change but sl=-109.95) never deserves a watch slot:
  // the trader ranks entries by slope_r2, so discovery should hand it slope-positive
  // candidates, not change-positive ones.
  if (rank === "slope") {
    return survivors
      .filter((s) => Number.isFinite(s.slopeScore) && s.slopeScore > 0)
      .sort((a, b) => b.slopeScore - a.slopeScore);
  }
  if (rank !== "change") {
    return [...survivors].sort((a, b) => (+b.vol24 || 0) - (+a.vol24 || 0));
  }
  const ceiling = maxChangePct > 0 ? maxChangePct : Infinity;
  return survivors
    .filter((s) => Number.isFinite(s.change24h) && s.change24h > 0 && s.change24h <= ceiling)
    .sort((a, b) => b.change24h - a.change24h);
}

/** Clenow-style momentum score over a close series: OLS slope of ln(price) vs time,
 *  annualized, × R² — the same semantics (sign + ordering, comparable scale) as the
 *  trader's slope_r2 ranking metric, so discovery and entry ranking finally agree on
 *  what "trending" means. `dtSecs` = seconds per observation. Pure; unit-tested.
 *  Returns null when the series is too short to regress. */
function slopeR2(closes, dtSecs) {
  const n = Array.isArray(closes) ? closes.length : 0;
  if (n < 3 || !Number.isFinite(dtSecs) || dtSecs <= 0) return null;
  const ys = closes.map((c) => Math.log(c));
  if (ys.some((y) => !Number.isFinite(y))) return null;
  const xm = (n - 1) / 2;
  const ym = ys.reduce((a, b) => a + b, 0) / n;
  let sxy = 0, sxx = 0, syy = 0;
  for (let i = 0; i < n; i++) {
    const dx = i - xm, dy = ys[i] - ym;
    sxy += dx * dy; sxx += dx * dx; syy += dy * dy;
  }
  if (sxx === 0 || syy === 0) return 0; // flat series — no trend either way
  const slope = sxy / sxx;              // ln-price per observation
  const r2 = (sxy * sxy) / (sxx * syy);
  return slope * (31_536_000 / dtSecs) * r2; // annualized × R²
}

/** "4h" → 4, "30m" → 0.5, "24h" → 24; unknown → fallback. */
function windowHours(window, fallback = 4) {
  const m = /^(\d+)(m|h)$/.exec(String(window || "").trim());
  if (!m) return fallback;
  return m[2] === "m" ? +m[1] / 60 : +m[1];
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

// KEYLESS fallback discovery — GeckoTerminal top pools by 24h volume, aggregated per base
// token into the same row shape as the Birdeye paths. Exists because Birdeye signals monthly
// CU-quota exhaustion as HTTP 400 ("Compute units usage limit exceeded") on every non-trivial
// endpoint (tokenlist/trending/ohlcv all die at once, observed 2026-07-30), which otherwise
// kills BOTH discovery paths until the billing anchor. GT is keyless (~30 req/min): page
// sequentially with generous pacing — a burst of parallel GT calls exhausted ITS quota once
// too (2026-07-28), so this deliberately stays slow.
//
// Approximation note: Birdeye's tokenlist reports token-level totals across all venues; this
// aggregates only the top-N pools, so a token whose volume is scattered across many small
// pools under-counts. Fine for discovery — the floors downstream want big movers anyway.
async function fetchGeckoTopVolume(minVolume, maxPages) {
  const byMint = new Map(); // mint → row
  // GT's volume-desc pool sort is DOMINATED by ghost pools: fresh pump pools reporting
  // $100M+ "volume" on sub-dollar reserves (observed: $119M on $0.0004). Any volume they
  // contribute poisons the per-token aggregate, so pools below a real-depth floor are
  // refused OUTRIGHT — their volume never counts. $25k is far under every downstream
  // liquidity floor, so this cannot hide a genuine candidate.
  const MIN_POOL_RESERVE_USD = 25_000;
  const pages = 10; // ghosts consume most of the early pages; walk deeper than Birdeye needed
  for (let page = 1; page <= pages; page++) {
    if (page > 1) await sleep(2500); // keyless pacing — GT 429s fast when its quota is warm
    const url =
      `https://api.geckoterminal.com/api/v2/networks/solana/pools` +
      `?sort=h24_volume_usd_desc&page=${page}&include=base_token`;
    let body;
    try {
      let res = await fetch(url, { headers: { accept: "application/json" } });
      if (res.status === 429) {
        console.error(`  … GeckoTerminal p${page} 429 — cooling 20s, one retry`);
        await sleep(20_000);
        res = await fetch(url, { headers: { accept: "application/json" } });
      }
      if (!res.ok) {
        console.error(`  ✗ GeckoTerminal pools p${page} -> HTTP ${res.status} — keeping ${byMint.size} tokens`);
        break; // keep what we have; only page 1 failing yields an empty (caller errors)
      }
      body = await res.json();
    } catch (e) {
      console.error(`  ✗ GeckoTerminal pools p${page} -> ${e.message} — keeping ${byMint.size} tokens`);
      break;
    }
    const pools = (body && body.data) || [];
    if (!pools.length) break;
    // included[] carries the token objects the pools reference.
    const tokens = new Map();
    for (const inc of body.included || []) {
      if (inc.type === "token") tokens.set(inc.id, inc.attributes || {});
    }
    let pageMax = 0;
    for (const p of pools) {
      const a = p.attributes || {};
      const vol = +((a.volume_usd || {}).h24) || 0;
      const reserve = +a.reserve_in_usd || 0;
      if (reserve < MIN_POOL_RESERVE_USD) continue; // ghost pool — see MIN_POOL_RESERVE_USD
      pageMax = Math.max(pageMax, vol);
      const baseId = (((p.relationships || {}).base_token || {}).data || {}).id;
      const tok = baseId ? tokens.get(baseId) : null;
      const mint = tok && tok.address;
      if (!mint) continue;
      const chg = +((a.price_change_percentage || {}).h24);
      const row = byMint.get(mint) || {
        address: mint,
        symbol: (tok.symbol || "").trim(),
        name: (tok.name || "").trim(),
        v24hUSD: 0,
        liquidity: 0,
        change24h: Number.isFinite(chg) ? chg : null,
      };
      row.v24hUSD += vol;
      row.liquidity += reserve;
      byMint.set(mint, row);
    }
    // Pools are volume-desc: once a page's REAL pools all sit under the floor, deeper pages
    // cannot create a new qualifying token. A page of only ghosts (pageMax 0) says nothing —
    // keep walking.
    if (pageMax > 0 && pageMax < minVolume) break;
  }
  const rows = [...byMint.values()].sort((x, y) => y.v24hUSD - x.v24hUSD);
  // GT's walk only sees a token's TOP pools, so summed reserves systematically UNDER-count
  // token-level liquidity — majors then fail the vol/liq anti-wash ratio as false positives
  // (observed: 15/25 wash-dropped). DexScreener's per-mint pool list is token-level and
  // keyless: replace `liquidity` with its Σ over the token's real pools for the head of the
  // list (the only rows with a chance downstream). Volume stays GT's — it was measured on
  // depth-filtered pools and is the ranking axis.
  const enrich = rows.slice(0, 30);
  for (const r of enrich) {
    await sleep(350); // DexScreener free-tier pacing
    try {
      const res = await fetch(`https://api.dexscreener.com/latest/dex/tokens/${r.address}`, {
        headers: { accept: "application/json" },
      });
      if (!res.ok) continue; // keep the GT estimate
      const pairs = ((await res.json()) || {}).pairs || [];
      const seen = new Set();
      let liq = 0;
      for (const pr of pairs) {
        if (!pr || seen.has(pr.pairAddress)) continue;
        seen.add(pr.pairAddress);
        liq += +((pr.liquidity || {}).usd) || 0;
      }
      if (liq > 0) r.liquidity = liq;
    } catch {
      /* keep the GT estimate */
    }
  }
  console.error(`  ✓ GeckoTerminal fallback: ${rows.length} tokens from top pools (liq enriched via DexScreener)`);
  return rows;
}

// Discovery with quota resilience: try the configured Birdeye source first; on ANY Birdeye
// failure (400 CU-quota, 401/429 rate-limit, 5xx) fall back to the keyless GeckoTerminal
// list so the scanner degrades instead of dying. The trending source falls back to the same
// volume-shaped list — "hot movers by volume" is the honest keyless approximation of it.
async function fetchDiscoveryRows(opts) {
  try {
    return opts.source === "volume"
      ? await fetchBirdeyeTopVolume(opts.minVolume, opts.maxPages)
      : await fetchBirdeyeTrending(opts.trendingLimit);
  } catch (e) {
    console.error(`  ✗ ${e.message} — falling back to keyless GeckoTerminal discovery`);
    return fetchGeckoTopVolume(opts.minVolume, opts.maxPages);
  }
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
function auditRejectReason(token, maxTopHoldersPct, minOrganicScore = 0) {
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
  // organicScore lives on the token root, not in `audit`. Sybil farms defeat the
  // concentration cap above by distributing supply in uniform tranches (GDWR held
  // "0.96% top-holders" across 19 identical wallets) — the volume they manufacture
  // is what this floor catches.
  if (minOrganicScore > 0) {
    const score = +(token && token.organicScore);
    if (!Number.isFinite(score)) return "no organic score";
    if (score < minOrganicScore) {
      return `organic score ${score.toFixed(1)} < ${minOrganicScore} floor (bot-farmed volume)`;
    }
  }
  return null;
}

// DexScreener dexIds the momentum gRPC pricer can decode (see feed_setup.rs): pumpswap
// (CP, vault reserves) + raydium / orca / meteora (CL, state account sqrt_price). The
// "raydium"/"meteora" ids are coarse (AMM-vs-CLMM, DLMM-vs-DAMM), but the matching fetcher
// auto-detects and a wrong guess just fails decode → REST fallback (safe by construction).
const GRPC_DEX_IDS = new Set(["pumpswap", "raydium", "orca", "meteora"]);
// Max gRPC-priceable venues emitted per discovery: the best plus a couple of fallbacks, so
// the watcher can wire a decodable venue even when the top one won't decode (e.g. a legacy
// Orca AMM that DexScreener still labels "orca"). Kept small — each venue is an extra gRPC sub.
const GRPC_POOLS_MAX = 3;

/**
 * PURE pool picker for scan discoveries: from a DexScreener `pairs` array, return the top
 * GRPC_POOLS_MAX gRPC-priceable pools (on GRPC_DEX_IDS venues with a SOL/USDC quote) as
 * [{pool, quote, dex}, …] ranked by 24h volume desc (fake-TVL rule; deduped by pool). `dex`
 * tells the watcher which fetcher decodes each; it wires the decodable ones and RESTs the
 * token only if none decode. Null = no gRPC-priceable venue → REST-priced.
 */
function pickGrpcPools(pairs) {
  if (!Array.isArray(pairs) || pairs.length === 0) return null;
  const eligible = pairs
    .filter((p) => {
      if (!p || !GRPC_DEX_IDS.has(p.dexId)) return false;
      if (!MINT_RE.test(p.pairAddress || "")) return false;
      const q = ((p.quoteToken && p.quoteToken.symbol) || "").toUpperCase();
      return q === "SOL" || q === "WSOL" || q === "USDC";
    })
    .sort((a, b) => ((b.volume && +b.volume.h24) || 0) - ((a.volume && +a.volume.h24) || 0));
  const out = [];
  const seen = new Set();
  for (const p of eligible) {
    if (seen.has(p.pairAddress)) continue;
    seen.add(p.pairAddress);
    const q = ((p.quoteToken && p.quoteToken.symbol) || "").toUpperCase();
    out.push({ pool: p.pairAddress, quote: q === "USDC" ? "USDC" : "SOL", dex: p.dexId });
    if (out.length >= GRPC_POOLS_MAX) break;
  }
  return out.length ? out : null;
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
      const picked = pickGrpcPools((body && body.pairs) || []);
      if (picked) {
        s.pools = picked; // [{pool, quote, dex}, …] — top-N gRPC-priceable venues (best first)
      } else {
        console.error(`  scan: ${s.symbol} no gRPC-priceable SOL/USDC pool — REST-priced`);
      }
    } catch (_) { /* REST-priced */ }
  }
  return survivors;
}

async function verifyAll(cands, opts = OPTS, _getTok = getVerifiedToken) {
  // SCAN_REQUIRE_JUP_VERIFY=false: skip the whole gate (no fetch, no audit). The on-chain
  // token_safety screen downstream remains the authoritative trap check.
  if (!opts.requireJupVerified) return cands.slice();
  // Sequential + paced to respect the public Jupiter tier's rate limit (a 429 would
  // fail-closed and silently drop a real token, so pacing matters).
  const out = [];
  for (let i = 0; i < cands.length; i++) {
    if (i > 0) await sleep(120);
    const tok = await _getTok(cands[i].address);
    if (!tok) {
      // Fail-closed as before, but say so: this branch also fires on a Jupiter
      // fetch failure/429, which used to silently eat a real candidate.
      console.error(`  scan: ${cands[i].symbol || cands[i].address} DROPPED — not Jupiter-verified (or verify fetch failed)`);
      continue;
    }
    const reject = auditRejectReason(tok, opts.maxTopHoldersPct, opts.minOrganicScore);
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
async function fetchChange(mint, window, symbol = mint) {
  const key = process.env.BIRDEYE_API_KEY || "";
  // Birdeye's free tier signals rate-limiting as 401 (same as the paginated
  // tokenlist path) — one paced retry rescues the reading instead of silently
  // feeding the change-band a null and killing the candidate.
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const res = await fetch(
        `https://public-api.birdeye.so/defi/token_overview?address=${mint}`,
        { headers: { "X-API-KEY": key, "x-chain": "solana", accept: "application/json" } }
      );
      if (!res.ok) {
        console.error(`  scan: ${symbol} change fetch → HTTP ${res.status}${attempt === 0 ? " — retrying" : ""}`);
        if (attempt === 0) { await sleep(2000); continue; }
        return null;
      }
      const body = await res.json();
      const c = body && body.data && body.data[`priceChange${window}Percent`];
      return Number.isFinite(+c) ? +c : null;
    } catch (e) {
      console.error(`  scan: ${symbol} change fetch failed (${e.message})${attempt === 0 ? " — retrying" : ""}`);
      if (attempt === 0) { await sleep(2000); continue; }
      return null;
    }
  }
  return null;
}

// Annotate each survivor with `change24h` (the ranking field — named for its historical
// default; it holds the `changeWindow` horizon). Sequential + paced at the same 1 s the
// paginated Birdeye fetcher needs — 120 ms got the whole tail 401'd (rate-limited), which
// nulled every reading and let the change-band silently kill all but the first survivor.
async function annotateChange(survivors, window) {
  for (let i = 0; i < survivors.length; i++) {
    if (i > 0) await sleep(1000);
    survivors[i].change24h = await fetchChange(survivors[i].mint, window, survivors[i].symbol);
  }
  return survivors;
}

// GeckoTerminal keyless 5-minute OHLCV for one pool → slope×R² over the scan window.
// GT (not Birdeye) on purpose: keyless, pool-addressed (we already resolved each
// survivor's best pool), and it spends none of the rate-limited Birdeye quota the
// discovery funnel itself needs. ~30 req/min free tier → 2.1s pacing, one retry.
async function fetchSlopeScore(pool, hours, symbol = pool) {
  const limit = Math.min(1000, Math.max(6, Math.ceil(hours * 12))); // 5m candles per window
  const url =
    `https://api.geckoterminal.com/api/v2/networks/solana/pools/${pool}/ohlcv/minute` +
    `?aggregate=5&limit=${limit}&currency=usd`;
  for (let attempt = 0; attempt < 2; attempt++) {
    try {
      const res = await fetch(url, { headers: { accept: "application/json" } });
      if (!res.ok) {
        if (attempt === 0 && (res.status === 429 || res.status >= 500)) { await sleep(2100); continue; }
        console.error(`  scan: ${symbol} slope fetch → HTTP ${res.status}`);
        return null;
      }
      const list = (await res.json())?.data?.attributes?.ohlcv_list || [];
      const closes = [...list]
        .sort((a, b) => a[0] - b[0]) // GT returns newest-first; regress oldest→newest
        .map((r) => +r[4])
        .filter((c) => Number.isFinite(c) && c > 0);
      if (closes.length < 6) return null; // <30min of data — not regressable
      return slopeR2(closes, 300);
    } catch (e) {
      if (attempt === 0) { await sleep(2100); continue; }
      console.error(`  scan: ${symbol} slope fetch failed (${e.message})`);
      return null;
    }
  }
  return null;
}

// Annotate the top-`slopeMax` survivors (already volume-ordered) with `slopeScore` over
// the MOMENTUM_SCAN_CHANGE_WINDOW horizon. Requires pools (annotatePools runs first in
// slope mode); a survivor without a pool or candles keeps slopeScore=null and is dropped
// by the slope band with a logged reason.
async function annotateSlope(survivors, { changeWindow, slopeMax }) {
  const hours = windowHours(changeWindow, 4);
  const targets = survivors.slice(0, slopeMax);
  for (let i = 0; i < targets.length; i++) {
    const s = targets[i];
    const pool = (s.pools && s.pools[0] && s.pools[0].pool) || s.pool || null;
    if (!pool) { s.slopeScore = null; continue; }
    if (i > 0) await sleep(2100);
    s.slopeScore = await fetchSlopeScore(pool, hours, s.symbol);
    if (Number.isFinite(s.slopeScore)) {
      console.error(`  scan: ${s.symbol} slope[${changeWindow}] = ${s.slopeScore.toFixed(1)}`);
    }
  }
  return survivors;
}

const fmtNum = (n) => Math.round(n).toLocaleString("en-US");

async function main() {
  const args = process.argv.slice(2);
  const asJson = args.includes("--json");
  const apply = args.includes("--apply");

  const rows = await fetchDiscoveryRows(OPTS);
  const { passed: filtered, drops } = classifyCandidates(rows, curatedMintsFromFile(), OPTS);
  // Funnel diagnostics → stderr (stdout is reserved for --json). The stage summary
  // always prints; per-drop reasons print when the drop list is small (trending source)
  // or SCAN_DEBUG=1 (the paginated volume source can drop hundreds of floor rows).
  {
    const byStage = {};
    for (const d of drops) byStage[d.stage] = (byStage[d.stage] || 0) + 1;
    const stages = Object.entries(byStage).map(([s, n]) => `${s}=${n}`).join(" ");
    console.error(
      `  scan funnel: ${OPTS.source} rows=${rows.length} → passed=${filtered.length}` +
        (drops.length ? ` (dropped: ${stages})` : "")
    );
    if (drops.length && (drops.length <= 25 || process.env.SCAN_DEBUG === "1")) {
      for (const d of drops) console.error(`  scan: ${d.symbol} dropped [${d.stage}] — ${d.reason}`);
    } else if (drops.length) {
      console.error(`  scan: ${drops.length} drop reasons suppressed — set SCAN_DEBUG=1 to list them`);
    }
  }
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
    // Birdeye token_overview only carries priceChange<X>Percent for these horizons —
    // any other window nulls EVERY reading and the change-band empties the scan.
    const KNOWN_WINDOWS = new Set(["1m", "5m", "30m", "1h", "2h", "4h", "8h", "24h"]);
    if (!KNOWN_WINDOWS.has(OPTS.changeWindow)) {
      console.error(
        `  scan: WARNING — MOMENTUM_SCAN_CHANGE_WINDOW="${OPTS.changeWindow}" is not a Birdeye horizon ` +
          `(${[...KNOWN_WINDOWS].join("/")}); every candidate will drop with "no change reading"`
      );
    }
    if (OPTS.changeWindow !== "24h") survivors.forEach((s) => { s.change24h = null; });
    await annotateChange(survivors.filter(needsChange), OPTS.changeWindow);
  }
  // rank=slope: pools must be resolved BEFORE ranking (GT OHLCV is pool-addressed), then
  // each finalist gets a slopeScore over the scan window. The later pool-enrich call is
  // skipped — pools are already annotated for at least as many survivors.
  let poolsPreAnnotated = false;
  if (OPTS.rank === "slope") {
    await annotatePools(survivors, Math.max(OPTS.poolEnrichMax, OPTS.slopeMax));
    poolsPreAnnotated = true;
    await annotateSlope(survivors, OPTS);
  }
  const preRank = survivors;
  survivors = rankSurvivors(survivors, OPTS);
  // rank=change silently bands out non-up-movers — say which and why.
  if (OPTS.rank === "change" && survivors.length < preRank.length) {
    const kept = new Set(survivors.map((s) => s.mint));
    for (const s of preRank.filter((p) => !kept.has(p.mint))) {
      const why = !Number.isFinite(s.change24h)
        ? `no ${OPTS.changeWindow} change reading`
        : s.change24h <= 0
          ? `${OPTS.changeWindow} change ${s.change24h.toFixed(1)}% not an up-move`
          : `${OPTS.changeWindow} change +${s.change24h.toFixed(1)}% above ${OPTS.maxChangePct}% ceiling (parabolic)`;
      console.error(`  scan: ${s.symbol} dropped [change-band] — ${why}`);
    }
  }
  // rank=slope silently bands out non-trending tokens — say which and why.
  if (OPTS.rank === "slope" && survivors.length < preRank.length) {
    const kept = new Set(survivors.map((s) => s.mint));
    for (const s of preRank.filter((p) => !kept.has(p.mint))) {
      const why = !(s.pools && s.pools.length) && !s.pool
        ? "no gRPC-priceable pool for OHLCV"
        : !Number.isFinite(s.slopeScore)
          ? "no candle data for slope"
          : `slope[${OPTS.changeWindow}] ${s.slopeScore.toFixed(1)} ≤ 0 (up on the day, not trending)`;
      console.error(`  scan: ${s.symbol} dropped [slope-band] — ${why}`);
    }
  }

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

  if (OPTS.poolEnrichMax > 0 && !poolsPreAnnotated) {
    await annotatePools(survivors, OPTS.poolEnrichMax);
  }

  // End of funnel — one stderr line even in --json mode, so the live watcher log
  // always shows what survived (or that nothing did).
  console.error(
    survivors.length
      ? `  scan kept ${survivors.length}: ${survivors.map((s) => s.symbol).join(", ")}`
      : "  scan kept 0 — every candidate dropped (reasons above)"
  );

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

module.exports = { filterCandidates, classifyCandidates, rankSurvivors, mapTrendingToken, needsChange, auditRejectReason, pickGrpcPools, verifyAll, slopeR2, windowHours };

if (require.main === module) {
  main().catch((e) => {
    console.error(`✗ ${e.message}`);
    process.exit(1);
  });
}
