#!/usr/bin/env node
/*
 * book_budget.js — choose the final pool book under a SUBSCRIBED-ACCOUNT budget.
 *
 * WHY a budget: free/shared Yellowstone tiers throttle large subscriptions and starve the
 * graph (the documented cause of phantom stale-edge cycles). Adding pools without a cap
 * makes freshness worse, so discovery REPLACES within a fixed budget.
 *
 * WHY hysteresis: every book change costs an ALT extension + a bot restart (which also
 * resets the LATENCY ring). A challenger must beat an incumbent by `evictMargin` to take
 * its slot, so the book does not thrash on noise.
 */
"use strict";
const { countAccounts } = require("../reduce_pools");

function isProtected(pool, ctx) {
  if (ctx.pinnedIds.has(pool.id)) return true;
  if (ctx.momentumPoolIds.has(pool.id)) return true;
  // Both tokens are hubs or curated blue-chips (LST/ETH/BTC/RAY/JUP/BONK) → a reliable arb
  // leg; never let a churny discovered token evict one for its slot. (ctx.majors optional so
  // older callers that pass only hubs keep the previous hub-hub behaviour.)
  const established = (m) => ctx.hubs.has(m) || (ctx.majors ? ctx.majors.has(m) : false);
  return established(pool.token_a) && established(pool.token_b);
}

function selectBook(args) {
  const { core, candidates, incumbentIds, budget, evictMargin, countPumpSwap } = args;
  const opts = { countPumpSwap };

  const coreAccounts = countAccounts(core, opts);
  if (coreAccounts > budget) {
    throw new Error(
      `protected core needs ${coreAccounts} accounts which exceeds ARB_ACCOUNT_BUDGET=${budget} — ` +
      `raise the budget or trim pinned pools`,
    );
  }

  // Incumbents first (hysteresis), then by activity desc.
  const ranked = candidates
    .filter((p) => !core.some((c) => c.id === p.id))
    .sort((a, b) => {
      const aIsIncumbent = incumbentIds.has(a.id);
      const bIsIncumbent = incumbentIds.has(b.id);
      // Incumbents come first regardless of activity
      if (aIsIncumbent !== bIsIncumbent) {
        return aIsIncumbent ? -1 : 1;
      }
      // Then by activity desc
      return b._act - a._act;
    });

  const kept = core.slice();
  const skipped = [];
  for (const cand of ranked) {
    const trial = kept.concat([cand]);
    if (countAccounts(trial, opts) <= budget) { kept.push(cand); continue; }

    // Budget full. A NON-incumbent may displace the weakest kept non-core incumbent only
    // if it beats it by the eviction margin.
    const weakest = kept
      .filter((p) => !core.some((c) => c.id === p.id))
      .sort((a, b) => a._act - b._act)[0];
    if (!incumbentIds.has(cand.id) && weakest && cand._act >= weakest._act * evictMargin) {
      const idx = kept.findIndex((p) => p.id === weakest.id);
      kept.splice(idx, 1);
      if (countAccounts(kept.concat([cand]), opts) <= budget) {
        kept.push(cand);
        skipped.push({ pool: weakest, reason: `evicted by ${cand.id} (activity ${cand._act} ≥ ${weakest._act}×${evictMargin})` });
        continue;
      }
      kept.splice(idx, 0, weakest); // restore — did not actually fit
    }
    skipped.push({ pool: cand, reason: "account budget full" });
  }

  const keptIds = new Set(kept.map((p) => p.id));
  const evicted = [...incumbentIds].filter((id) => !keptIds.has(id)).map((id) => ({ id }));
  return { kept, evicted, skipped, accounts: countAccounts(kept, opts) };
}

module.exports = { isProtected, selectBook };
