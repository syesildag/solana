#!/usr/bin/env node
/**
 * Merges all DEX pool configs into pools.json.
 * Run after all fetch_*.js scripts have completed.
 *
 * Usage:
 *   node scripts/merge_pools.js
 */
"use strict";
const fs   = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");

function load(file) {
  const p = path.join(ROOT, file);
  if (!fs.existsSync(p)) { console.warn(`  ⚠  ${file} not found — skipping`); return []; }
  return JSON.parse(fs.readFileSync(p, "utf8"));
}

const raydium   = load("raydium_pools.json");
const orca      = load("orca_pools.json");
const meteora   = load("meteora_pools.json");
const dlmm      = load("dlmm_pools.json");
const phoenix   = load("phoenix_pools.json");
const lifinity  = load("lifinity_pools.json");
const invariant = load("invariant_pools.json");
const saber     = load("saber_pools.json");
// pump_swap entries are PRICING-ONLY (portfolio-watcher gRPC feed); the arb bot's
// PoolRegistry skips them at load.
const pumpswap  = load("pumpswap_pools.json");

// Pools known to produce phantom prices or ProgramAccountNotFound in simulation.
// The list lives in lib/pool_blocklist.js so BOTH generators honor it (this merge and
// scan_arb_pools.js focus rewrites) — add entries there, with reason + date.
const { POOL_BLOCKLIST } = require("./lib/pool_blocklist");

const all    = [...raydium, ...orca, ...meteora, ...dlmm, ...phoenix, ...lifinity, ...invariant, ...saber, ...pumpswap];
const merged = all.filter(p => !POOL_BLOCKLIST.has(p.id));
if (all.length !== merged.length)
  console.log(`  Blocklist removed ${all.length - merged.length} pool(s): ${[...POOL_BLOCKLIST].filter(id => all.some(p => p.id === id)).join(", ")}`);
fs.writeFileSync(path.join(ROOT, "pools.json"), JSON.stringify(merged, null, 2));
const ammV4  = raydium.filter(p => p.dex === "raydium_amm_v4").length;
const clmm   = raydium.filter(p => p.dex === "raydium_clmm").length;
console.log(
  `Merged → pools.json: Raydium ${raydium.length} (AMM V4: ${ammV4}, CLMM: ${clmm})` +
  ` + Orca ${orca.length} + Meteora DAMM ${meteora.length}` +
  ` + DLMM ${dlmm.length} + Phoenix ${phoenix.length}` +
  ` + Lifinity ${lifinity.length} + Invariant ${invariant.length} + Saber ${saber.length}` +
  ` + PumpSwap ${pumpswap.length} (pricing-only)` +
  ` = ${merged.length} total`
);
