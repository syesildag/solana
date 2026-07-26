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
// Add a pool ID here to permanently exclude it from pools.json across all fetch runs.
const POOL_BLOCKLIST = new Set([
  "FpjYwNjCStVE2Rvk9yVZsV46YwgNTFjp7ktJUDcZdyyk", // SOL/JUP DLMM — phantom active_bin, ProgramAccountNotFound in sim
  "9CopBY6iQBaZKAhhQANfy7g4VXZkx9zKm8AisPd5Ufay", // SOL/USDT DAMM — zero output at all probe sizes (empty LP vaults)
  "B5EwJVDuAauzUEEdwvbuXzbFFgEYnUqqS37TUM1c4PQA",  // SOL/BTC Orca Whirlpool — tick arrays don't exist on-chain (tick=-91142, arrays generated for wrong price range)
  "9nfomE7jP17PqEc91ohSzPsrRiK7LX3La1rDarMJDcj9", // WBTC/SOL DAMM — $1.5k-liq husk: price permanently ~20bps displaced, floods BF with ghost cycles that die at the impact cap (105bps at min probe; 2026-07-05)
  // Single-venue dead-ends: each token has exactly ONE pool, so it can never close an arb
  // cycle — pure subscribed-account ballast that only adds graph noise (removed 2026-07-26).
  "Sgo6roPnWxZUtDHKBeJkxVyUVWYcGwZh5hgX6w6pXHH", // SLX/USDC Orca — SLX single-venue dead-end
  "6qz7THwQvcjF3HyDGLuKaLBUk6EyJKeZXZMWLAeiwfjd", // BP/USDC DLMM — BP single-venue dead-end
  "AQR7642dfSmQwNgyeCio61c8jTNhpW3QirUyouthXigq", // ARX/SOL DLMM — ARX single-venue dead-end
  "C7hF6MvQwErhsf1KrFvnKzdArb9PsofFiwZdipo9c7cz", // ORE/USDC DLMM — ORE single-venue dead-end
]);

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
