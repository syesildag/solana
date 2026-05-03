#!/usr/bin/env node
/**
 * Fetches Saber StableSwap pool configs and writes saber_pools.json.
 *
 * Saber uses the same Curve StableSwap invariant as Meteora DAMM stable pools.
 * Reserves are read from SPL token vault accounts (byte offset 64), same as
 * Raydium AMM V4 — no state account subscription is needed.
 *
 * SwapInfo account layout (after 1-byte tag):
 *   offset   1: is_initialized     bool
 *   offset   2: token_a_mint       Pubkey (32 bytes)
 *   offset  34: token_b_mint       Pubkey (32 bytes)
 *   offset  66: token_a_vault      Pubkey (32 bytes)
 *   offset  98: token_b_vault      Pubkey (32 bytes)
 *   offset 131: swap_authority     Pubkey (32 bytes)  — used as amm_authority
 *   offset 163: admin_fee_a        Pubkey (32 bytes)
 *   offset 195: admin_fee_b        Pubkey (32 bytes)
 *   offset 227: amp                u64    (8 bytes)   — amplification coefficient
 *   offset 235: fees               struct (various)
 *
 * Usage:
 *   node scripts/fetch_saber_pools.js
 *   RPC_URL=https://... node scripts/fetch_saber_pools.js
 */
"use strict";
const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const RPC_URL     = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const OUTPUT_FILE = path.join(__dirname, "..", "saber_pools.json");

// Saber SwapInfo pool addresses.
// Saber pool list: https://github.com/saber-hq/saber-registry-dist/blob/master/data/pools-info.mainnet.json
// High-TVL stable pairs to watch:
const TARGET_POOLS = [
  "YAkoNb6HromicpLUBfeyPrJkFuqFdpLNWWL2a4tNsZY",  // USDC/USDT  (sUSDC/sUSDT)
  "2p7nYbtPBgtmY69NsE8DAW6szpRJn7tQvDnqvoEWQvjY",  // whETH/weETH
  // Saber also has many LST/SOL pairs — add here once verified:
  // "xxx",  // jitoSOL/SOL (if Saber has this pair)
  // "xxx",  // mSOL/SOL
];

const BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
function bs58encode(bytes) {
  const digits = [0];
  for (const byte of bytes) {
    let carry = byte;
    for (let i = 0; i < digits.length; i++) {
      carry += digits[i] << 8;
      digits[i] = carry % 58;
      carry = Math.floor(carry / 58);
    }
    while (carry > 0) { digits.push(carry % 58); carry = Math.floor(carry / 58); }
  }
  let result = "";
  for (const b of bytes) { if (b !== 0) break; result += "1"; }
  return result + digits.reverse().map(d => BASE58_ALPHABET[d]).join("");
}

function rpcPost(url, body) {
  return new Promise((resolve, reject) => {
    const data = JSON.stringify(body);
    const parsed = new URL(url);
    const lib = parsed.protocol === "https:" ? https : http;
    const req = lib.request({
      hostname: parsed.hostname,
      port: parsed.port || (parsed.protocol === "https:" ? 443 : 80),
      path: parsed.pathname + parsed.search,
      method: "POST",
      headers: { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(data) },
      timeout: 30_000,
    }, res => {
      const chunks = [];
      res.on("data", c => chunks.push(c));
      res.on("end", () => {
        try { resolve(JSON.parse(Buffer.concat(chunks).toString())); }
        catch (e) { reject(e); }
      });
      res.on("error", reject);
    });
    req.on("error", reject);
    req.on("timeout", () => { req.destroy(); reject(new Error("RPC timeout")); });
    req.write(data);
    req.end();
  });
}

async function main() {
  if (TARGET_POOLS.length === 0) {
    console.warn("⚠  No Saber pools configured. Populate TARGET_POOLS in fetch_saber_pools.js");
    fs.writeFileSync(OUTPUT_FILE, JSON.stringify([], null, 2));
    console.log("Wrote 0 Saber pools → saber_pools.json");
    return;
  }

  console.log(`Fetching ${TARGET_POOLS.length} Saber pool accounts...`);
  const resp = await rpcPost(RPC_URL, {
    jsonrpc: "2.0", id: 1, method: "getMultipleAccounts",
    params: [TARGET_POOLS, { encoding: "base64" }],
  });
  if (resp.error) throw new Error("RPC error: " + JSON.stringify(resp.error));

  const pools = [];
  resp.result.value.forEach((acc, i) => {
    const addr = TARGET_POOLS[i];
    if (!acc) { console.warn(`  SKIP ${addr}: account not found`); return; }

    const data = Buffer.from(acc.data[0], "base64");
    if (data.length < 240) {
      console.warn(`  SKIP ${addr}: data too short (${data.length} bytes, expected ≥240)`);
      return;
    }

    const mintA       = bs58encode(data.slice(2,  34));
    const mintB       = bs58encode(data.slice(34, 66));
    const vaultA      = bs58encode(data.slice(66, 98));
    const vaultB      = bs58encode(data.slice(98, 130));
    const authority   = bs58encode(data.slice(131, 163));
    const adminFeeA   = bs58encode(data.slice(163, 195));
    const adminFeeB   = bs58encode(data.slice(195, 227));
    // amp stored as u64 LE at offset 227; typical values 100–2000
    const amp = data.length >= 235
      ? Number(data.readBigUInt64LE(227))
      : 100;

    console.log(`  ✓ ${addr.slice(0,8)}  A=${mintA.slice(0,8)} B=${mintB.slice(0,8)} amp=${amp}`);

    pools.push({
      id:      addr,
      dex:     "saber",
      token_a: mintA,
      token_b: mintB,
      vault_a: vaultA,
      vault_b: vaultB,
      fee_bps: 4, // Saber typical stable fee: 0.04%
      stable:  true,
      extra: {
        amm_authority:     authority,
        admin_token_fee_a: adminFeeA,
        admin_token_fee_b: adminFeeB,
        damm_amp:          amp,
      },
    });
  });

  fs.writeFileSync(OUTPUT_FILE, JSON.stringify(pools, null, 2));
  console.log(`\nWrote ${pools.length} Saber pools → saber_pools.json`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
