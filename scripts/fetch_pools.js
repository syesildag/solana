#!/usr/bin/env node
/**
 * Fetches pool configs from Raydium AMM V4 and Orca Whirlpool, writes pools.json.
 *
 * Cross-DEX coverage is essential for negative cycles:
 *   Raydium (25 bps) + Orca (1–30 bps) for the same pairs = structural price gaps.
 *
 * Usage:
 *   node scripts/fetch_pools.js [--output pools.json] [--rpc <url>]
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
  BONK: "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
  ETH:  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
  BTC:  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
};

const RAYDIUM_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],["SOL","RAY"],["SOL","MSOL"],
  ["SOL","BONK"],["SOL","ETH"],["SOL","BTC"],
  ["USDC","RAY"],["USDT","RAY"],["USDC","MSOL"],["USDC","ETH"],["USDC","BTC"],
];

const ORCA_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],["SOL","MSOL"],["SOL","ETH"],
  ["USDC","USDT"],["USDC","ETH"],["USDC","MSOL"],
];

const OUTPUT = process.argv.includes("--output")
  ? process.argv[process.argv.indexOf("--output") + 1]
  : path.join(__dirname, "..", "pools.json");

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

function rpcPost(method, params) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ jsonrpc:"2.0", id:1, method, params });
    const mod  = RPC.startsWith("https") ? https : http;
    const u    = new URL(RPC);
    const req  = mod.request(
      { hostname:u.hostname, port:u.port, path:u.pathname+u.search,
        method:"POST", headers:{"Content-Type":"application/json"} },
      (res) => {
        const c = [];
        res.on("data", d => c.push(d));
        res.on("end", () => resolve(JSON.parse(Buffer.concat(c).toString("utf8"))));
        res.on("error", reject);
      }
    );
    req.on("error", reject);
    req.write(body); req.end();
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

// ─── Orca Whirlpool ───────────────────────────────────────────────────────────
//
// Whirlpool state layout (after 8-byte discriminator):
//   8:   whirlpools_config (32)
//  40:   whirlpool_bump (1)
//  41:   tick_spacing (2)
//  43:   tick_spacing_seed (2)
//  45:   fee_rate (2) — stored in millionths (400 = 0.04% = 4 bps)
//  47:   protocol_fee_rate (2)
//  49:   liquidity (16)
//  65:   sqrt_price (16)
//  81:   tick_current_index (4)
//  85:   protocol_fee_owed_a (8)
//  93:   protocol_fee_owed_b (8)
// 101:   token_mint_a (32)
// 133:   token_vault_a (32)  ← subscribe to this for reserve changes
// 165:   fee_growth_global_a (16)
// 181:   token_mint_b (32)
// 213:   token_vault_b (32)  ← subscribe to this for reserve changes

let _orcaList = null;
async function getOrcaList() {
  if (!_orcaList) {
    const d = await httpGet("https://api.mainnet.orca.so/v1/whirlpool/list");
    _orcaList = d.whirlpools ?? [];
  }
  return _orcaList;
}

async function fetchOrca(symA, symB) {
  const mintA = MINTS[symA];
  const mintB = MINTS[symB];
  const list  = await getOrcaList();

  const candidates = list.filter(p =>
    (p.tokenA?.mint === mintA && p.tokenB?.mint === mintB) ||
    (p.tokenA?.mint === mintB && p.tokenB?.mint === mintA)
  ).sort((a, b) => (b.tvl ?? 0) - (a.tvl ?? 0));

  if (!candidates.length) return null;
  const best = candidates[0];

  // Fetch vault addresses directly from on-chain Whirlpool state account
  const ai = await rpcPost("getAccountInfo", [best.address, { encoding: "base64" }]);
  const raw = ai?.result?.value?.data?.[0];
  if (!raw) return null;
  const data = Buffer.from(raw, "base64");
  if (data.length < 245) return null;

  // fee_rate is in millionths (1_000_000 = 100%)
  const feeRateMicro = data.readUInt16LE(45);
  const feeBps = Math.round(feeRateMicro / 100); // convert to bps

  const mintAOnChain  = bs58(data.slice(101, 133));
  const vaultAOnChain = bs58(data.slice(133, 165));
  const mintBOnChain  = bs58(data.slice(181, 213));
  const vaultBOnChain = bs58(data.slice(213, 245));

  // Orient token_a / token_b to match caller's symA/symB
  const aIsA = mintAOnChain === mintA;

  return {
    id:            best.address,
    dex:           "orca_whirlpool",
    token_a:       aIsA ? mintAOnChain  : mintBOnChain,
    token_b:       aIsA ? mintBOnChain  : mintAOnChain,
    vault_a:       aIsA ? vaultAOnChain : vaultBOnChain,
    vault_b:       aIsA ? vaultBOnChain : vaultAOnChain,
    fee_bps:       feeBps,
    // Pool address IS the state account — subscribe to it for sqrt_price updates
    state_account: best.address,
    extra: {
      // Tick arrays depend on current price; derive them at swap-execution time
      tick_array_0: null,
      tick_array_1: null,
      tick_array_2: null,
      oracle:       null,
    },
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

  console.log("\n── Orca Whirlpool ────────────────────────────────────");
  for (const [a, b] of ORCA_PAIRS) {
    process.stdout.write(`  ${a}/${b}… `);
    try {
      const cfg = await fetchOrca(a, b);
      if (!cfg) { console.log("no pool"); continue; }
      results.push(cfg);
      console.log(`✓  ${cfg.id}  fee=${cfg.fee_bps}bps  vault_a=${cfg.vault_a.slice(0,8)}…`);
    } catch (e) { console.log(`error: ${e.message}`); }
  }

  if (!results.length) { console.error("\nNo pools."); process.exit(1); }

  const byDex = results.reduce((m, p) => { m[p.dex] = (m[p.dex]||0)+1; return m; }, {});
  console.log(`\nTotal: ${results.length} pools — ${JSON.stringify(byDex)}`);
  console.log("Cross-DEX cycles now searchable:");
  console.log("  SOL→USDC(Raydium 25bps)→SOL(Orca 4bps)  — 2-hop");
  console.log("  SOL→USDT(Raydium)→USDC(Orca stable)→SOL — 3-hop");
  console.log("  SOL→mSOL(Raydium)→SOL(Orca)              — 2-hop LST");

  fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} pools → ${OUTPUT}`);
})().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
