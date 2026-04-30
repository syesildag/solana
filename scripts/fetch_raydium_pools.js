#!/usr/bin/env node
/**
 * Fetches Raydium AMM V4 pool configs via the Raydium API and writes raydium_pools.json.
 *
 * Usage:
 *   node scripts/fetch_raydium_pools.js [--output raydium_pools.json] [--rpc <url>]
 *
 * Run this then `node scripts/merge_pools.js` to rebuild pools.json.
 */

const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const MINTS = {
  SOL:  "So11111111111111111111111111111111111111112",
  USDC: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDT: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
  RAY:  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
  MSOL: "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",
  ETH:  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
  BTC:  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
  EURC: "HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr",
};

const RAYDIUM_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],["SOL","RAY"],["SOL","MSOL"],
  ["SOL","ETH"],["SOL","BTC"],["SOL","EURC"],
  ["USDC","RAY"],["USDT","RAY"],["USDC","MSOL"],["USDC","ETH"],["USDC","BTC"],["USDC","EURC"],
];

const CLMM_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],["SOL","RAY"],["SOL","MSOL"],
  ["SOL","ETH"],["SOL","BTC"],
  ["USDC","USDT"],["USDC","ETH"],["USDC","BTC"],["USDC","RAY"],
];

// Only include CLMM pools with at least this much TVL.
// Low-TVL pools are rarely traded and carry stale sqrt_price, causing phantom arb cycles.
const CLMM_MIN_TVL = 500_000;

const OUTPUT = process.argv.includes("--output")
  ? process.argv[process.argv.indexOf("--output") + 1]
  : path.join(__dirname, "..", "raydium_pools.json");

const RPC = process.argv.includes("--rpc")
  ? process.argv[process.argv.indexOf("--rpc") + 1]
  : "https://api.mainnet-beta.solana.com";

// ─── Helpers ──────────────────────────────────────────────────────────────────

const BS58_ALPHA = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
function bs58(buf) {
  let n = BigInt("0x" + buf.toString("hex"));
  let s = "";
  while (n > 0n) { s = BS58_ALPHA[Number(n % 58n)] + s; n /= 58n; }
  for (let i = 0; i < buf.length && buf[i] === 0; i++) s = "1" + s;
  return s;
}

function httpGet(url) {
  return new Promise((resolve, reject) => {
    const mod = url.startsWith("https") ? https : http;
    const req = mod.get(url, { timeout: 30_000 }, (res) => {
      if ([301,302,307,308].includes(res.statusCode) && res.headers.location)
        return httpGet(res.headers.location).then(resolve, reject);
      if (res.statusCode !== 200)
        return reject(new Error(`HTTP ${res.statusCode} — ${url}`));
      const c = [];
      res.on("data", d => c.push(d));
      res.on("end", () => resolve(JSON.parse(Buffer.concat(c).toString("utf8"))));
      res.on("error", reject);
    });
    req.on("error", reject);
    req.on("timeout", () => { req.destroy(); reject(new Error("Timeout: " + url)); });
  });
}

// ─── Raydium AMM V4 ──────────────────────────────────────────────────────────


async function fetchRaydium(symA, symB) {
  const url = `https://api-v3.raydium.io/pools/info/mint` +
    `?mint1=${MINTS[symA]}&mint2=${MINTS[symB]}` +
    `&poolType=standard&poolSortField=liquidity&sortType=desc&pageSize=3&page=1`;
  const data = await httpGet(url);
  const poolId = (data?.data?.data ?? [])[0]?.id;
  if (!poolId) return null;

  const kd = await httpGet(`https://api-v3.raydium.io/pools/key/ids?ids=${poolId}`);
  const k  = (kd?.data ?? [])[0];
  if (!k) return null;

  const required = ["authority","openOrders","targetOrders","marketProgramId",
    "marketId","marketBids","marketAsks","marketEventQueue","marketBaseVault",
    "marketQuoteVault","marketAuthority"];
  const missing = required.filter(f => !k[f]);
  if (missing.length) return { _skip: `missing: ${missing.join(", ")}` };

  return {
    id: k.id, dex: "raydium_amm_v4",
    token_a: k.mintA.address, token_b: k.mintB.address,
    vault_a:  k.vault.A,      vault_b:  k.vault.B,
    fee_bps: 25,
    extra: {
      amm_authority:       k.authority,
      open_orders:         k.openOrders,
      target_orders:       k.targetOrders,
      market_program:      k.marketProgramId,
      market:              k.marketId,
      market_bids:         k.marketBids,
      market_asks:         k.marketAsks,
      market_event_queue:  k.marketEventQueue,
      market_coin_vault:   k.marketBaseVault,
      market_pc_vault:     k.marketQuoteVault,
      market_vault_signer: k.marketAuthority,
    },
  };
}

// ─── Raydium CLMM ────────────────────────────────────────────────────────────

async function fetchRaydiumClmm(symA, symB) {
  const url = `https://api-v3.raydium.io/pools/info/mint` +
    `?mint1=${MINTS[symA]}&mint2=${MINTS[symB]}` +
    `&poolType=concentrated&poolSortField=liquidity&sortType=desc&pageSize=10&page=1`;
  const data = await httpGet(url);
  const best = (data?.data?.data ?? []).find(p => (p.tvl ?? 0) >= CLMM_MIN_TVL);
  if (!best) return null;
  const poolId = best.id;

  const kd = await httpGet(`https://api-v3.raydium.io/pools/key/ids?ids=${poolId}`);
  const k  = (kd?.data ?? [])[0];
  if (!k) return null;

  const required = ["vault", "config", "observationId"];
  const missing = required.filter(f => k[f] == null);
  if (missing.length) return { _skip: `missing: ${missing.join(", ")}` };

  return {
    id:            poolId,
    dex:           "raydium_clmm",
    token_a:       k.mintA?.address ?? MINTS[symA],
    token_b:       k.mintB?.address ?? MINTS[symB],
    vault_a:       k.vault.A,
    vault_b:       k.vault.B,
    fee_bps:       Math.round(k.config.tradeFeeRate / 100),
    state_account: poolId,
    extra: {
      clmm_amm_config:   k.config.id,
      clmm_observation:  k.observationId,
      clmm_tick_spacing: k.config.tickSpacing,
    },
    _tvl: best.tvl,  // stripped before writing; used for console output only
  };
}

// ─── Main ─────────────────────────────────────────────────────────────────────

(async () => {
  const results = [];

  console.log("\n── Raydium AMM V4 ───────────────────────────────────");
  for (const [a, b] of RAYDIUM_PAIRS) {
    process.stdout.write(`  ${a}/${b}… `);
    try {
      const cfg = await fetchRaydium(a, b);
      if (!cfg)       { console.log("no pool"); continue; }
      if (cfg._skip)  { console.log(`⚠  ${cfg._skip}`); continue; }
      results.push(cfg);
      console.log(`✓  ${cfg.id}`);
    } catch (e) { console.log(`error: ${e.message}`); }
  }

  console.log("\n── Raydium CLMM ─────────────────────────────────────");
  for (const [a, b] of CLMM_PAIRS) {
    process.stdout.write(`  ${a}/${b}… `);
    try {
      const cfg = await fetchRaydiumClmm(a, b);
      if (!cfg)       { console.log(`no pool (TVL < $${CLMM_MIN_TVL.toLocaleString()})`); continue; }
      if (cfg._skip)  { console.log(`⚠  ${cfg._skip}`); continue; }
      const tvl = cfg._tvl; delete cfg._tvl;
      results.push(cfg);
      console.log(`✓  ${cfg.id}  tvl=$${Math.round(tvl ?? 0).toLocaleString()}`);
    } catch (e) { console.log(`error: ${e.message}`); }
  }

  if (!results.length) { console.error("\nNo pools."); process.exit(1); }

  const ammCount  = results.filter(p => p.dex === "raydium_amm_v4").length;
  const clmmCount = results.filter(p => p.dex === "raydium_clmm").length;
  fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} Raydium pools → ${OUTPUT}  (AMM V4: ${ammCount}, CLMM: ${clmmCount})`);
})().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
