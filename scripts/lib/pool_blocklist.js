#!/usr/bin/env node
/*
 * pool_blocklist.js — pool IDs permanently excluded from pools.json by EVERY generator
 * (merge_pools.js full-fetch merges AND scan_arb_pools.js focus-mode rewrites; the scanner
 * used to bypass the old merge_pools-local list entirely, which is how a blocklisted-in-
 * spirit pool could ride back in via discovery).
 *
 * Add a pool here when it produces phantom prices, unfillable markers, or simulation
 * failures that survive across fetch runs. Keep the reason + date in the comment.
 */
"use strict";

const POOL_BLOCKLIST = new Set([
  "FpjYwNjCStVE2Rvk9yVZsV46YwgNTFjp7ktJUDcZdyyk", // SOL/JUP DLMM — phantom active_bin, ProgramAccountNotFound in sim
  "9CopBY6iQBaZKAhhQANfy7g4VXZkx9zKm8AisPd5Ufay", // SOL/USDT DAMM — zero output at all probe sizes (empty LP vaults)
  "B5EwJVDuAauzUEEdwvbuXzbFFgEYnUqqS37TUM1c4PQA", // SOL/BTC Orca Whirlpool — tick arrays don't exist on-chain (tick=-91142, arrays generated for wrong price range)
  "9nfomE7jP17PqEc91ohSzPsrRiK7LX3La1rDarMJDcj9", // WBTC/SOL DAMM — $1.5k-liq husk: price permanently ~20bps displaced, floods BF with ghost cycles that die at the impact cap (105bps at min probe; 2026-07-05)
  // Single-venue dead-ends: each token has exactly ONE pool, so it can never close an arb
  // cycle — pure subscribed-account ballast that only adds graph noise (removed 2026-07-26).
  "Sgo6roPnWxZUtDHKBeJkxVyUVWYcGwZh5hgX6w6pXHH",  // SLX/USDC Orca — SLX single-venue dead-end
  "6qz7THwQvcjF3HyDGLuKaLBUk6EyJKeZXZMWLAeiwfjd", // BP/USDC DLMM — BP single-venue dead-end
  "AQR7642dfSmQwNgyeCio61c8jTNhpW3QirUyouthXigq", // ARX/SOL DLMM — ARX single-venue dead-end
  "C7hF6MvQwErhsf1KrFvnKzdArb9PsofFiwZdipo9c7cz", // ORE/USDC DLMM — ORE single-venue dead-end
  "DXfnX2oCJAcfBC8A7MB1UamcrT9eeERxWP2RduHkrbN", // HYPE/USDC DLMM — price-outlier: marker pinned ~100bps above every other HYPE venue ($60.38 vs ~$59.8, 41/59 balanced $212k so NOT a husk) but the bin-walk finds the fill ~100bps worse than the marker → permanent +76bps mirage cycles flooding BF, unfillable in practice (2026-07-27)
]);

module.exports = { POOL_BLOCKLIST };
