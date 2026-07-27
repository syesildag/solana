#!/usr/bin/env node
/*
 * venues.js — map a token's DexScreener pairs to candidate pools per venue.
 *
 * Ranking is by 24h VOLUME, not liquidity: fake-TVL pools exist (see the DexScreener
 * pricing notes), and volume is the honest signal for "this pool actually trades".
 *
 * The precise `dex` kind (raydium_amm_v4 vs raydium_clmm, meteora_damm vs dlmm) is NOT
 * decided here — DexScreener's dexId is too coarse. The per-DEX decoder settles it.
 */
"use strict";

/** DexScreener dexIds the bot has a decoder + swap builder for. */
const SUPPORTED_DEX_IDS = new Set(["raydium", "orca", "meteora", "pumpswap"]);

function bestPoolPerVenue(pairs, opts) {
  const allow = opts.quoteAllowlist;
  const best = new Map();
  for (const p of pairs || []) {
    if (!SUPPORTED_DEX_IDS.has(p.dexId)) continue;
    const quoteMint = p.quoteToken && p.quoteToken.address;
    if (!quoteMint || !allow.has(quoteMint)) continue; // must close back to a hub
    const volume24h = (p.volume && p.volume.h24) || 0;
    const prev = best.get(p.dexId);
    if (!prev || volume24h > prev.volume24h) {
      best.set(p.dexId, {
        dexId: p.dexId,
        pairAddress: p.pairAddress,
        quoteMint,
        liquidityUsd: (p.liquidity && p.liquidity.usd) || 0,
        volume24h,
        priceChangeH1: (p.priceChange && p.priceChange.h1) || 0, // short-window volatility proxy for arb ranking
      });
    }
  }
  return [...best.values()];
}

function tradeableVenueCount(venues, opts) {
  const pumpTradeable = opts.pumpTradeable === true;
  const ids = new Set();
  for (const v of venues) {
    if (v.dexId === "pumpswap" && !pumpTradeable) continue;
    ids.add(v.dexId);
  }
  return ids.size;
}

/** Min-side value share of a pair: the smaller side's USD value / total USD. A healthy
 *  pool sits near 0.5; a one-sided husk is ~0 (observed: HYPE DLMM DXfnX2oC — $86 of
 *  HYPE vs $125k USDC, whose off-market marker price flooded BF with mirage cycles).
 *  Computed from DexScreener's per-side fields (liquidity.base × priceUsd vs
 *  liquidity.usd). Returns null when the fields are absent so callers can pass rather
 *  than reject on missing data. */
function minSideShare(p) {
  const usd = p.liquidity && Number(p.liquidity.usd);
  const base = p.liquidity && Number(p.liquidity.base);
  const price = Number(p.priceUsd);
  if (!usd || usd <= 0 || !Number.isFinite(base) || !Number.isFinite(price)) return null;
  const baseUsd = base * price;
  return Math.min(baseUsd, Math.max(usd - baseUsd, 0)) / usd;
}

/** ALL supported-DEX pools quoted in opts.quoteMint with liq ≥ opts.minLiq, volume-desc,
 *  deduped by pairAddress, capped at opts.max. POOL-level, not one-per-dex: two pools on
 *  the SAME dex form a valid QUOTE→X→QUOTE 2-hop (only same-POOL cycles are phantoms), so
 *  bestPoolPerVenue's dexId-keying under-counted eligibility (a DLMM token with two USDC
 *  bin-step pools looked like one venue) and under-booked admitted tokens' quote legs.
 *  pumpswap pools are excluded unless opts.pumpTradeable (pricing-only pools can't close
 *  a cycle). opts.minSideShare (0–0.5, optional) rejects one-sided husks: total liquidity
 *  can clear minLiq while one side is dust — an unfillable marker price that only
 *  manufactures mirage cycles. Pairs missing per-side data pass (the runtime bin-walk
 *  quote still protects execution). */
function quotePools(pairs, opts) {
  const quote = opts.quoteMint;
  const minLiq = Number(opts.minLiq) || 0;
  const pumpTradeable = opts.pumpTradeable === true;
  const out = [];
  const seen = new Set();
  for (const p of pairs || []) {
    if (!SUPPORTED_DEX_IDS.has(p.dexId)) continue;
    if (p.dexId === "pumpswap" && !pumpTradeable) continue;
    const quoteMint = p.quoteToken && p.quoteToken.address;
    if (quoteMint !== quote) continue;
    const liquidityUsd = (p.liquidity && p.liquidity.usd) || 0;
    if (liquidityUsd < minLiq) continue;
    if (opts.minSideShare) {
      const share = minSideShare(p);
      if (share !== null && share < opts.minSideShare) continue;
    }
    if (seen.has(p.pairAddress)) continue;
    seen.add(p.pairAddress);
    out.push({
      dexId: p.dexId,
      pairAddress: p.pairAddress,
      quoteMint,
      liquidityUsd,
      volume24h: (p.volume && p.volume.h24) || 0,
      priceChangeH1: (p.priceChange && p.priceChange.h1) || 0,
    });
  }
  out.sort((a, b) => b.volume24h - a.volume24h);
  return opts.max ? out.slice(0, opts.max) : out;
}

module.exports = { SUPPORTED_DEX_IDS, bestPoolPerVenue, tradeableVenueCount, quotePools, minSideShare };
