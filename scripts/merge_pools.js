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

const raydium  = load("raydium_pools.json");
const orca     = load("orca_pools.json");
const meteora  = load("meteora_pools.json");
const dlmm     = load("dlmm_pools.json");
const phoenix  = load("phoenix_pools.json");

const merged = [...raydium, ...orca, ...meteora, ...dlmm, ...phoenix];
fs.writeFileSync(path.join(ROOT, "pools.json"), JSON.stringify(merged, null, 2));
const ammV4  = raydium.filter(p => p.dex === "raydium_amm_v4").length;
const clmm   = raydium.filter(p => p.dex === "raydium_clmm").length;
console.log(
  `Merged → pools.json: Raydium ${raydium.length} (AMM V4: ${ammV4}, CLMM: ${clmm})` +
  ` + Orca ${orca.length} + Meteora DAMM ${meteora.length}` +
  ` + DLMM ${dlmm.length} + Phoenix ${phoenix.length}` +
  ` = ${merged.length} total`
);
