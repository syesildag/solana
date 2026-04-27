#!/usr/bin/env node
/**
 * Fetches Raydium AMM V4 pool configs via the Raydium v3 REST API and writes
 * pools.json in the format expected by this bot's PoolConfig schema.
 *
 * Usage:
 *   node scripts/fetch_pools.js [--output pools.json]
 */

const https = require("https");
const fs    = require("fs");
const path  = require("path");

// ─── Token mints ──────────────────────────────────────────────────────────────
const MINTS = {
  SOL:  "So11111111111111111111111111111111111111112",
  USDC: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDT: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
  RAY:  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
  MSOL: "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",
  BONK: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
  ETH:  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs", // Wormhole wETH
  BTC:  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh", // Wormhole wBTC
  EURC: "HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr", // Circle EURC
};

const TARGET_PAIRS = [
  ["SOL",  "USDC"],
  ["SOL",  "USDT"],
  ["SOL",  "RAY"],
  ["SOL",  "MSOL"],
  ["SOL",  "BONK"],
  ["SOL",  "ETH"],
  ["SOL",  "BTC"],
  ["USDC", "RAY"],
  ["USDT", "RAY"],
  ["USDC", "MSOL"],
  ["USDC", "ETH"],
  ["USDC", "BTC"],
  ["USDC", "EURC"],
  ["ETH",  "BTC"],
];

const OUTPUT_FILE = process.argv.includes("--output")
  ? process.argv[process.argv.indexOf("--output") + 1]
  : path.join(__dirname, "..", "raydium_pools.json");

// ─── HTTP helper ──────────────────────────────────────────────────────────────

function get(url) {
  return new Promise((resolve, reject) => {
    const req = https.get(url, { timeout: 30_000 }, (res) => {
      if ([301, 302, 307, 308].includes(res.statusCode) && res.headers.location)
        return get(res.headers.location).then(resolve, reject);
      if (res.statusCode !== 200)
        return reject(new Error(`HTTP ${res.statusCode} — ${url}`));
      const chunks = [];
      res.on("data", (c) => chunks.push(c));
      res.on("end", () => {
        try { resolve(JSON.parse(Buffer.concat(chunks).toString("utf8"))); }
        catch (e) { reject(e); }
      });
      res.on("error", reject);
    });
    req.on("error", reject);
    req.on("timeout", () => { req.destroy(); reject(new Error("Timeout: " + url)); });
  });
}

// ─── Step 1: find pool ID for a pair ─────────────────────────────────────────

async function findPoolId(symA, symB) {
  const url =
    `https://api-v3.raydium.io/pools/info/mint` +
    `?mint1=${MINTS[symA]}&mint2=${MINTS[symB]}` +
    `&poolType=standard&poolSortField=liquidity&sortType=desc&pageSize=3&page=1`;
  const data = await get(url);
  const pools = data?.data?.data ?? [];
  // Prefer the highest-liquidity standard (AMM V4) pool
  return pools[0]?.id ?? null;
}

// ─── Step 2: get full pool keys ───────────────────────────────────────────────

async function fetchPoolKeys(poolId) {
  const url = `https://api-v3.raydium.io/pools/key/ids?ids=${poolId}`;
  const data = await get(url);
  return (data?.data ?? [])[0] ?? null;
}

// ─── Map to PoolConfig ────────────────────────────────────────────────────────

function toPoolConfig(k) {
  return {
    id:      k.id,
    dex:     "raydium_amm_v4",
    token_a: k.mintA.address,
    token_b: k.mintB.address,
    vault_a: k.vault.A,
    vault_b: k.vault.B,
    fee_bps: 25,
    extra: {
      amm_authority:       k.authority         ?? null,
      open_orders:         k.openOrders        ?? null,
      target_orders:       k.targetOrders      ?? null,
      market_program:      k.marketProgramId   ?? null,
      market:              k.marketId          ?? null,
      market_bids:         k.marketBids        ?? null,
      market_asks:         k.marketAsks        ?? null,
      market_event_queue:  k.marketEventQueue  ?? null,
      market_coin_vault:   k.marketBaseVault   ?? null,
      market_pc_vault:     k.marketQuoteVault  ?? null,
      market_vault_signer: k.marketAuthority   ?? null,
    },
  };
}

// ─── Main ─────────────────────────────────────────────────────────────────────

(async () => {
  const results = [];

  for (const [symA, symB] of TARGET_PAIRS) {
    process.stdout.write(`  ${symA}/${symB}… `);
    try {
      const poolId = await findPoolId(symA, symB);
      if (!poolId) { console.log("no pool found"); continue; }

      const keys = await fetchPoolKeys(poolId);
      if (!keys) { console.log("no keys found"); continue; }

      const cfg = toPoolConfig(keys);
      const nullKeys = Object.entries(cfg.extra).filter(([, v]) => v === null).map(([k]) => k);
      if (nullKeys.length) {
        console.log(`⚠ missing: ${nullKeys.join(", ")} — skipped`);
        continue;
      }
      results.push(cfg);
      console.log(`✓  ${poolId}`);
    } catch (e) {
      console.log(`error: ${e.message}`);
    }
  }

  if (results.length === 0) {
    console.error("\nNo pools fetched.");
    process.exit(1);
  }

  fs.writeFileSync(OUTPUT_FILE, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} pools → ${OUTPUT_FILE}`);
})().catch((err) => {
  console.error("Fatal:", err.message);
  process.exit(1);
});
