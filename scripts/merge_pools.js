#!/usr/bin/env node
/**
 * Merges Raydium AMM V4 and Orca Whirlpool pool configs into a single pools.json.
 * Run after fetch_pools.js and fetch_orca_pools.js have both completed.
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

const raydium = load("pools.json");
const orca    = load("orca_pools.json");

const merged = [...raydium, ...orca];
fs.writeFileSync(path.join(ROOT, "pools.json"), JSON.stringify(merged, null, 2));
console.log(`Merged → pools.json: ${raydium.length} Raydium + ${orca.length} Orca = ${merged.length} total pools`);
