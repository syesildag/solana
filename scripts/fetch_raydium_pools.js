#!/usr/bin/env node
/**
 * Fetches Raydium AMM V4 pool configs via the Raydium API and writes raydium_pools.json.
 *
 * Usage:
 *   node scripts/fetch_raydium_pools.js [--output raydium_pools.json] [--rpc <url>]
 *   node scripts/fetch_raydium_pools.js --pools <addr,addr,…>   # ad-hoc, skips discovery
 *
 * Run this then `node scripts/merge_pools.js` to rebuild pools.json.
 */

const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const MINTS = {
  SOL:     "So11111111111111111111111111111111111111112",
  USDC:    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDT:    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
  RAY:     "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
  MSOL:    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",
  ETH:     "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
  BTC:     "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
  EURC:    "HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr",
  // Liquid Staking Tokens
  JITOSOL: "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn",
  BSOL:    "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1",
  STSOL:   "7dHbWXmci3dT8UFYWYZweBLXgycu7Y3iL6trKn1Y7ARj",
  // Meme / governance tokens
  BONK:     "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
  WIF:      "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",
  JUP:      "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",
  // Low-competition meme tokens: pump.fun graduates with Raydium+Orca coverage
  // but far fewer dedicated arb bots than SOL/USDC/JUP routes.
  POPCAT:   "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
  FARTCOIN: "9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump",
  // Momentum watch (added 2026-07-22 unvetted per user request; Jul-21 direct Raydium launch, THIN $11k liq)
  DAVINCI:  "5WZQjSbYd3zuzMzusJybgMMiVSNHmq44vgXuvQ3nHpXJ",
};

const RAYDIUM_PAIRS = [
  // Existing major pairs
  ["SOL","USDC"],["SOL","USDT"],["SOL","RAY"],["SOL","MSOL"],
  ["SOL","ETH"],["SOL","BTC"],["SOL","EURC"],
  ["USDC","RAY"],["USDT","RAY"],["USDC","MSOL"],["USDC","ETH"],["USDC","BTC"],["USDC","EURC"],
  // Long-tail: LSTs (low competition, arb windows persist longer)
  ["SOL","JITOSOL"],["SOL","BSOL"],["SOL","STSOL"],
  // Long-tail: meme / governance
  ["SOL","BONK"],["SOL","WIF"],["SOL","JUP"],["USDC","JUP"],["USDC","BONK"],
  // Low-competition: pump.fun graduates with multi-DEX coverage
  ["SOL","POPCAT"],["USDC","POPCAT"],
  ["SOL","FARTCOIN"],
  ["SOL","DAVINCI"],
];

const CLMM_PAIRS = [
  // Existing
  ["SOL","USDC"],["SOL","USDT"],["SOL","RAY"],["SOL","MSOL"],
  ["SOL","ETH"],["SOL","BTC"],
  ["USDC","USDT"],["USDC","ETH"],["USDC","BTC"],["USDC","RAY"],
  // Long-tail: LSTs on CLMM (concentrated liquidity → lower price impact for arb)
  ["SOL","JITOSOL"],["SOL","BSOL"],
  // Long-tail: meme tokens (high volatility → frequent arb opportunities)
  ["SOL","BONK"],["SOL","WIF"],["USDC","JUP"],
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
    `&poolType=standard&poolSortField=liquidity&sortType=desc&pageSize=5&page=1`;
  const data = await httpGet(url);
  const all = data?.data?.data ?? [];
  // Raydium AMM V4 AmmStatus: 6 = SwapOnly, 8 = Normal (full ops).
  // Values below 6 (Disabled=2, WithdrawOnly=3, etc.) reject swap instructions on-chain
  // with Custom(101) = AmmError::InvalidStatus. Skip them and fall through to the next
  // most-liquid pool that is actually swappable.
  const swappable = all.filter(p => p.status == null || p.status >= 6);
  const skippedCount = all.length - swappable.length;
  const candidate = swappable[0];
  if (!candidate) return null;
  const poolId = candidate.id;

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
    _skippedDisabled: skippedCount,  // stripped before writing; used for console output only
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

// ─── Direct decode by pool id (--pools override) ────────────────────────────

// Decodes one Raydium pool by address via the same `pools/key/ids` endpoint the
// discovery paths above use (fetchRaydium/fetchRaydiumClmm) — called directly with
// a known pool id instead of first resolving one from a mint pair via
// `pools/info/mint`. Pool type is inferred from the response shape: CLMM key data
// carries `config` (tradeFeeRate/tickSpacing + a sibling `observationId`); AMM V4
// carries `authority`/`openOrders`/`marketProgramId` (OpenBook market accounts).
// Returns null if the id doesn't resolve, or { _skip } if required fields are absent.
async function fetchById(poolId) {
  const kd = await httpGet(`https://api-v3.raydium.io/pools/key/ids?ids=${poolId}`);
  const k = (kd?.data ?? [])[0];
  if (!k) return null;

  if (k.config) {
    const required = ["vault", "config", "observationId"];
    const missing = required.filter(f => k[f] == null);
    if (missing.length) return { _skip: `missing: ${missing.join(", ")}` };
    return {
      id:            k.id ?? poolId,
      dex:           "raydium_clmm",
      token_a:       k.mintA?.address,
      token_b:       k.mintB?.address,
      vault_a:       k.vault.A,
      vault_b:       k.vault.B,
      fee_bps:       Math.round(k.config.tradeFeeRate / 100),
      state_account: k.id ?? poolId,
      extra: {
        clmm_amm_config:   k.config.id,
        clmm_observation:  k.observationId,
        clmm_tick_spacing: k.config.tickSpacing,
      },
    };
  }

  const required = ["authority","openOrders","targetOrders","marketProgramId",
    "marketId","marketBids","marketAsks","marketEventQueue","marketBaseVault",
    "marketQuoteVault","marketAuthority"];
  const missing = required.filter(f => !k[f]);
  if (missing.length) return { _skip: `missing: ${missing.join(", ")}` };

  return {
    id: k.id ?? poolId, dex: "raydium_amm_v4",
    token_a: k.mintA?.address, token_b: k.mintB?.address,
    vault_a:  k.vault.A,       vault_b:  k.vault.B,
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

// ─── Main ─────────────────────────────────────────────────────────────────────

(async () => {
  // --pools <addr,…>: decode exactly these addresses instead of running discovery.
  // Used by scan_arb_pools.js to decode a newly-discovered pool on demand.
  const cliPools = process.argv.includes("--pools")
    ? process.argv[process.argv.indexOf("--pools") + 1].split(",").map((s) => s.trim()).filter(Boolean)
    : null;

  const results = [];

  if (cliPools) {
    console.log("\n── Raydium (--pools decode override) ────────────────");
    for (const poolId of cliPools) {
      process.stdout.write(`  ${poolId.slice(0, 8)}… `);
      try {
        const cfg = await fetchById(poolId);
        if (!cfg)      { console.log("not found"); continue; }
        if (cfg._skip) { console.log(`⚠  ${cfg._skip}`); continue; }
        results.push(cfg);
        console.log(`✓  ${cfg.dex}  ${cfg.id}`);
      } catch (e) { console.log(`error: ${e.message}`); }
    }

    if (!results.length) { console.error("\nNo pools decoded."); process.exit(1); }
    fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
    console.log(`\nWrote ${results.length} Raydium pool(s) → ${OUTPUT}`);
    return;
  }

  console.log("\n── Raydium AMM V4 ───────────────────────────────────");
  for (const [a, b] of RAYDIUM_PAIRS) {
    process.stdout.write(`  ${a}/${b}… `);
    try {
      const cfg = await fetchRaydium(a, b);
      if (!cfg)       { console.log("no pool"); continue; }
      if (cfg._skip)  { console.log(`⚠  ${cfg._skip}`); continue; }
      const skipped = cfg._skippedDisabled; delete cfg._skippedDisabled;
      results.push(cfg);
      const skipNote = skipped ? `  (skipped ${skipped} disabled)` : "";
      console.log(`✓  ${cfg.id}${skipNote}`);
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
