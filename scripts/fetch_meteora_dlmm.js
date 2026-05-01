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

// Min lamports in reserve_x vault to consider the pool liquid enough.
const DLMM_MIN_RESERVE = 5_000_000;  // 0.005 SOL or ~0.005 USDC (effectively excludes empty pools)

const MINTS = {
  SOL:  "So11111111111111111111111111111111111111112",
  USDC: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDT: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
  RAY:  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
  MSOL: "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",
  ETH:  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
  BTC:  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
};

// Target pairs in (X, Y) order, tried both ways
const DLMM_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],["SOL","MSOL"],
  ["SOL","BTC"],["SOL","ETH"],
  ["USDC","USDT"],["USDC","RAY"],["USDC","BTC"],["USDC","ETH"],
];

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

function rpc(method, params) {
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

    // Fetch reserve_x balances for top candidates (cap at 5 to limit RPC calls)
    const top = candidates.slice(0, 5);
    const balances = await Promise.all(top.map(c => getTokenBalance(c.reserveX).catch(() => 0n)));

    // Filter by minimum reserve, then pick highest by balance
    const liquid = top.map((c, i) => ({ ...c, balance: balances[i] }))
      .filter(c => c.balance >= BigInt(DLMM_MIN_RESERVE));

    if (liquid.length === 0) { console.log(`no liquid pools (${top.length} found, all below min reserve)`); continue; }

    // Pick highest balance
    liquid.sort((a, b) => (b.balance > a.balance ? 1 : b.balance < a.balance ? -1 : 0));
    const best = liquid[0];

    // Normalise: token_a = tokenX, token_b = tokenY
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

    const balSOL = Number(best.balance) / 1e9;
    console.log(`✓  ${best.pubkey}  binStep=${best.binStep}  fee=${best.feeBps}bps  reserveX=${balSOL.toFixed(3)}`);
  }

  fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} DLMM pools → ${OUTPUT}`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
