#!/usr/bin/env node
/**
 * Runs all pool fetchers in sequence then merges them into pools.json.
 *
 * Equivalent to:
 *   node scripts/fetch_raydium_pools.js
 *   node scripts/fetch_orca_pools.js
 *   node scripts/fetch_meteora_pools.js
 *   node scripts/merge_pools.js
 *
 * Usage:
 *   node scripts/fetch_all.js
 *   RPC_URL=https://... node scripts/fetch_all.js
 */
"use strict";
const { spawnSync } = require("child_process");
const path = require("path");

const SCRIPTS = [
  "fetch_raydium_pools.js",
  "fetch_orca_pools.js",
  "fetch_meteora_pools.js",
  "merge_pools.js",
];

for (const script of SCRIPTS) {
  const full = path.join(__dirname, script);
  console.log(`\n${"─".repeat(60)}\n▶  ${script}\n${"─".repeat(60)}`);
  const result = spawnSync(process.execPath, [full], {
    stdio: "inherit",
    env: process.env,
  });
  if (result.status !== 0) {
    console.error(`\n✗  ${script} exited with code ${result.status}`);
    process.exit(result.status ?? 1);
  }
}

console.log("\n✓  All fetchers complete — pools.json is up to date.");
