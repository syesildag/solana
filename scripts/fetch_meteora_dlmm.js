#!/usr/bin/env node
/**
 * Fetches top Meteora DLMM (concentrated-bin) pools for target pairs.
 *
 * LbPair account layout (all little-endian, after 8-byte Anchor discriminator):
 *   offset   8: StaticParameters (32 bytes)
 *     off   8: base_factor       u16
 *     off  34: base_fee_power_factor u8
 *   offset  40: VariableParameters (32 bytes)
 *   offset  72: bump_seed        u8
 *   offset  73: bin_step_seed    u8[2]
 *   offset  75: pair_type        u8
 *   offset  76: active_id        i32
 *   offset  80: bin_step         u16
 *   offset  88: token_x_mint     pubkey (32 bytes)
 *   offset 120: token_y_mint     pubkey (32 bytes)
 *   offset 152: reserve_x        pubkey (SPL token vault, 32 bytes)
 *   offset 184: reserve_y        pubkey (SPL token vault, 32 bytes)
 *
 * base_fee_bps = base_factor * bin_step * 10 * 10^base_fee_power_factor / 1e5
 *
 * Usage:
 *   node scripts/fetch_meteora_dlmm.js [--output dlmm_pools.json]
 *   RPC_URL=https://... node scripts/fetch_meteora_dlmm.js
 */
"use strict";
const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const DLMM_PROGRAM  = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const RPC           = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const OUTPUT        = process.argv.includes("--output")
  ? process.argv[process.argv.indexOf("--output") + 1]
  : path.join(__dirname, "..", "dlmm_pools.json");

// LbPair field offsets
const OFF_BASE_FACTOR    = 8;
const OFF_BFPF           = 34;   // base_fee_power_factor u8
const OFF_ACTIVE_ID      = 76;   // i32
const OFF_BIN_STEP       = 80;   // u16
const OFF_TOKEN_X        = 88;   // pubkey 32 bytes
const OFF_TOKEN_Y        = 120;  // pubkey 32 bytes
const OFF_RESERVE_X      = 152;  // pubkey 32 bytes
const OFF_RESERVE_Y      = 184;  // pubkey 32 bytes

// Min raw token units required in BOTH reserve_x and reserve_y vaults.
// Requiring both sides >= threshold excludes:
//   - completely one-sided (swept empty) out-of-range pools
//   - dust pools with stale active_id and negligible liquidity
// 100_000_000 = 0.1 SOL (9 dec) or 100 USDC/USDT (6 dec).
const DLMM_MIN_RESERVE = 100_000_000;

const MINTS = {
  SOL:  "So11111111111111111111111111111111111111112",
  USDC: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDT: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
  RAY:  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
  MSOL: "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",
  ETH:  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
  BTC:  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
  BONK:     "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263",
  WIF:      "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm",
  JUP:      "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN",
  POPCAT:   "7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr",
  FARTCOIN: "9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump",
};

// Target pairs in (X, Y) order, tried both ways
const DLMM_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],["SOL","MSOL"],
  ["SOL","BTC"],["SOL","ETH"],
  ["USDC","USDT"],["USDC","RAY"],["USDC","BTC"],["USDC","ETH"],
  // High-volume meme/governance tokens — primary source of cross-DEX price dislocations
  ["SOL","BONK"],["SOL","WIF"],["SOL","JUP"],
  ["USDC","BONK"],["USDC","WIF"],["USDC","JUP"],
  // Low-competition: pump.fun graduates with Raydium+Orca coverage
  ["SOL","POPCAT"],["USDC","POPCAT"],
  ["SOL","FARTCOIN"],
];

// Pinned lb_pair addresses fetched directly by pubkey, bypassing pair discovery.
// The getProgramAccounts pair-scan sorts by bin_step and keeps only the 5 smallest,
// which misses the liquid pool for tokens that have many dust pools at tiny bin steps
// (e.g. these momentum names, whose real pools sit at bin_step 20/50/80). Addresses
// are the highest-liquidity gRPC-priceable (DLMM) pool per token, from DexScreener.
const DLMM_PINNED = [
  "AsSyvUnbfaZJPRrNh3kUuvZTeHKoMVWEoHz86f4Q5D9x", // MET/SOL   binStep=20 liq~$933K vol/day~$3.6M
  "6qz7THwQvcjF3HyDGLuKaLBUk6EyJKeZXZMWLAeiwfjd", // BP/USDC   binStep=50 liq~$2.2M vol/day~$2.1M
  "AQR7642dfSmQwNgyeCio61c8jTNhpW3QirUyouthXigq", // ARX/SOL   binStep=80 liq~$127K vol/day~$44K
  "C7hF6MvQwErhsf1KrFvnKzdArb9PsofFiwZdipo9c7cz", // ORE/USDC  binStep=50 liq~$402K vol/day~$524K
  "ANCx141SujgVdbKz9NTEH8F38qWsnyyXsVju64aU3qLB", // HYPE/USDC binStep=20 liq~$5.7M vol/day~$13.1M
];

// Pairs with multiple coexisting liquid DLMM pools at different bin steps.
// Keeping top 2 doubles arbitrage surface at minimal graph cost.
const MULTI_POOL_PAIRS = new Set([
  "SOL/BONK","SOL/WIF","SOL/JUP","USDC/BONK","USDC/WIF","USDC/JUP",
]);

// ─── Helpers ──────────────────────────────────────────────────────────────────

const BS58_ALPHA = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

function b58enc(buf) {
  const digits = [0];
  for (const byte of buf) {
    let carry = byte;
    for (let i = 0; i < digits.length; i++) {
      carry += digits[i] << 8;
      digits[i] = carry % 58;
      carry = Math.floor(carry / 58);
    }
    while (carry > 0) { digits.push(carry % 58); carry = Math.floor(carry / 58); }
  }
  let str = "";
  for (const byte of buf) { if (byte !== 0) break; str += "1"; }
  return str + digits.reverse().map(x => BS58_ALPHA[x]).join("");
}

function rpcOnce(method, params) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
    const url  = new URL(RPC);
    const mod  = url.protocol === "https:" ? https : http;
    const req  = mod.request(
      { hostname: url.hostname, path: url.pathname + url.search,
        method: "POST", timeout: 60_000,
        headers: { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) } },
      (r) => {
        const c = []; r.on("data", d => c.push(d));
        r.on("end", () => {
          try { resolve(JSON.parse(Buffer.concat(c).toString())); }
          catch (e) { reject(new Error("bad JSON: " + e.message)); }
        });
        r.on("error", reject);
      }
    );
    req.on("error", reject);
    req.on("timeout", () => { req.destroy(); reject(new Error("RPC timeout")); });
    req.write(body); req.end();
  });
}

// Retries with exponential backoff on 429. getProgramAccounts on the public RPC
// is aggressively rate-limited; up to 5 retries with 5s/10s/20s/40s/60s waits.
async function rpc(method, params, attempt = 0) {
  const res = await rpcOnce(method, params);
  // Public RPCs signal rate-limiting as HTTP 429 (code 429) OR the JSON-RPC
  // custom code -32429 (Helius) — retry on either.
  const rateLimited = res?.error?.code === 429 || res?.error?.code === -32429;
  if (rateLimited && attempt < 5) {
    const delay = Math.min(5_000 * Math.pow(2, attempt), 60_000);
    process.stderr.write(`  429 on ${method} — retrying in ${delay/1000}s (attempt ${attempt+1}/5)\n`);
    await sleep(delay);
    return rpc(method, params, attempt + 1);
  }
  return res;
}

// Returns SPL token account balance (u64 lamports), or 0 if fetch fails.
async function getTokenBalance(pubkey) {
  const r = await rpc("getTokenAccountBalance", [pubkey, { commitment: "processed" }]);
  const raw = r?.result?.value?.amount;
  return raw ? BigInt(raw) : 0n;
}

// Returns { pubkey, data: Buffer } for each LbPair where tokenX = mintX AND tokenY = mintY.
async function getDlmmPairs(mintX, mintY) {
  const r = await rpc("getProgramAccounts", [
    DLMM_PROGRAM,
    {
      encoding: "base64",
      filters: [
        { memcmp: { offset: OFF_TOKEN_X, bytes: mintX } },
        { memcmp: { offset: OFF_TOKEN_Y, bytes: mintY } },
      ],
    },
  ]);
  if (r?.error)  throw new Error("getProgramAccounts error: " + JSON.stringify(r.error));
  if (!Array.isArray(r?.result)) return [];
  return r.result.map(acc => ({
    pubkey: acc.pubkey,
    data: Buffer.from(acc.account.data[0], "base64"),
  }));
}

function parseLbPair(pubkey, data) {
  if (data.length < 216) return null;  // need at least through reserve_y
  const baseFactor    = data.readUInt16LE(OFF_BASE_FACTOR);
  const bfpf          = data.readUInt8(OFF_BFPF);
  const activeId      = data.readInt32LE(OFF_ACTIVE_ID);
  const binStep       = data.readUInt16LE(OFF_BIN_STEP);
  const tokenX        = b58enc(data.slice(OFF_TOKEN_X,    OFF_TOKEN_X + 32));
  const tokenY        = b58enc(data.slice(OFF_TOKEN_Y,    OFF_TOKEN_Y + 32));
  const reserveX     = b58enc(data.slice(OFF_RESERVE_X,  OFF_RESERVE_X + 32));
  const reserveY     = b58enc(data.slice(OFF_RESERVE_Y,  OFF_RESERVE_Y + 32));

  // baseFee = baseFactor * binStep * 10 * 10^bfpf / FEE_PRECISION(1e9) → bps = * 10000
  const feeBps = Math.round(baseFactor * binStep * 10 * Math.pow(10, bfpf) / 1e5);

  return { pubkey, tokenX, tokenY, reserveX, reserveY, binStep, feeBps, activeId };
}

const sleep = ms => new Promise(r => setTimeout(r, ms));

// ─── Main ─────────────────────────────────────────────────────────────────────

async function main() {
  const results = [];

  for (const [symA, symB] of DLMM_PAIRS) {
    const mintA = MINTS[symA], mintB = MINTS[symB];
    process.stdout.write(`  ${symA}/${symB}… `);

    // Try both token orderings (DLMM pairs have tokenX < tokenY by pubkey, but we search both)
    let candidates = [];
    try {
      const fwd = await getDlmmPairs(mintA, mintB);
      const rev = await getDlmmPairs(mintB, mintA);
      candidates = [...fwd, ...rev].map(({ pubkey, data }) => parseLbPair(pubkey, data)).filter(Boolean);
    } catch (e) {
      console.log(`error: ${e.message}`);
      continue;
    }

    if (candidates.length === 0) { console.log("no pools"); continue; }

    // Sort by bin_step ascending (smaller = more concentrated = better price signal)
    candidates.sort((a, b) => a.binStep - b.binStep);

    // Fetch BOTH reserve vault balances for top candidates (cap at 5 to limit RPC calls).
    // Checking both sides rejects out-of-range pools where one vault is swept empty.
    const top = candidates.slice(0, 5);
    const [balancesX, balancesY] = await Promise.all([
      Promise.all(top.map(c => getTokenBalance(c.reserveX).catch(() => 0n))),
      Promise.all(top.map(c => getTokenBalance(c.reserveY).catch(() => 0n))),
    ]);

    const min_reserve = BigInt(DLMM_MIN_RESERVE);
    const liquid = top
      .map((c, i) => ({ ...c, balX: balancesX[i], balY: balancesY[i] }))
      .filter(c => c.balX >= min_reserve && c.balY >= min_reserve);

    if (liquid.length === 0) { console.log(`no liquid pools (${top.length} found, all below min reserve or one-sided)`); continue; }

    // Sort by deepest two-sided liquidity; meme-token pairs get top 2 pools since
    // multiple bin-step pools coexist with distinct liquidity profiles.
    liquid.sort((a, b) => {
      const minA = a.balX < a.balY ? a.balX : a.balY;
      const minB = b.balX < b.balY ? b.balX : b.balY;
      return minB > minA ? 1 : minB < minA ? -1 : 0;
    });
    const maxPicks = MULTI_POOL_PAIRS.has(`${symA}/${symB}`) ? 2 : 1;

    // For multi-pick pairs, discard any secondary pool whose mid-price deviates
    // more than 5% from the primary pool. A stale DLMM pool (no recent trades)
    // keeps a frozen active_id that can be far from market, creating phantom arb
    // cycles that flood the log and waste BF evaluations.
    const impliedPrice = p => (1 + p.binStep / 10_000) ** p.activeId;
    const refPrice = impliedPrice(liquid[0]);
    const priceConsistent = p => Math.abs(impliedPrice(p) / refPrice - 1) < 0.05;
    const selected = liquid.slice(0, maxPicks).filter((p, i) => i === 0 || priceConsistent(p));

    const fmtBal = n => (Number(n) / 1e9).toFixed(3);
    for (const best of selected) {
      const isForward = best.tokenX === mintA;
      const token_a = isForward ? best.tokenX : best.tokenY;
      const token_b = isForward ? best.tokenY : best.tokenX;
      const vault_a = isForward ? best.reserveX : best.reserveY;
      const vault_b = isForward ? best.reserveY : best.reserveX;

      results.push({
        id:            best.pubkey,
        dex:           "meteora_dlmm",
        token_a,
        token_b,
        vault_a,
        vault_b,
        fee_bps:       best.feeBps,
        state_account: best.pubkey,
        extra: {
          dlmm_bin_step: best.binStep,
        },
      });

      console.log(`✓  ${best.pubkey}  binStep=${best.binStep}  fee=${best.feeBps}bps  resX=${fmtBal(best.balX)}  resY=${fmtBal(best.balY)}`);
    }

    await sleep(1200);  // avoid 429 on public RPC
  }

  // Pinned pools: fetch each lb_pair directly by address (no discovery/min-reserve gate).
  const fmtBal = n => (Number(n) / 1e9).toFixed(3);
  for (const addr of DLMM_PINNED) {
    process.stdout.write(`  pinned ${addr}… `);
    if (results.some(r => r.id === addr)) { console.log("already present — skip"); continue; }
    try {
      const r = await rpc("getAccountInfo", [addr, { encoding: "base64" }]);
      const val = r?.result?.value;
      if (!val) { console.log("not found"); continue; }
      if (val.owner !== DLMM_PROGRAM) { console.log(`wrong owner ${val.owner}`); continue; }
      const best = parseLbPair(addr, Buffer.from(val.data[0], "base64"));
      if (!best) { console.log("unparseable"); continue; }
      const [balX, balY] = await Promise.all([
        getTokenBalance(best.reserveX).catch(() => 0n),
        getTokenBalance(best.reserveY).catch(() => 0n),
      ]);
      results.push({
        id:            best.pubkey,
        dex:           "meteora_dlmm",
        token_a:       best.tokenX,
        token_b:       best.tokenY,
        vault_a:       best.reserveX,
        vault_b:       best.reserveY,
        fee_bps:       best.feeBps,
        state_account: best.pubkey,
        extra:         { dlmm_bin_step: best.binStep },
      });
      console.log(`✓  binStep=${best.binStep}  fee=${best.feeBps}bps  resX=${fmtBal(balX)}  resY=${fmtBal(balY)}`);
    } catch (e) {
      console.log(`error: ${e.message}`);
    }
    await sleep(1200);
  }

  fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} DLMM pools → ${OUTPUT}`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
