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

module.exports = { SUPPORTED_DEX_IDS, bestPoolPerVenue, tradeableVenueCount };
