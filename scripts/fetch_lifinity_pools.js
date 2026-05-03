#!/usr/bin/env node
/**
 * Fetches Lifinity v2 AMM pool configs and writes lifinity_pools.json.
 *
 * Lifinity is an oracle-anchored AMM — the pool price tracks a Pyth oracle feed
 * rather than a reserve ratio. Arb opportunities arise when the oracle price has
 * moved but the pool's on-chain price hasn't updated yet (typically 1–5 seconds).
 *
 * Pool state layout (after 8-byte Anchor discriminator):
 *   offset   9: amm_config   Pubkey (32 bytes)  — fee + config account
 *   offset  74: oracle       Pubkey (32 bytes)  — Pyth price account
 *   offset 138: token_0_vault Pubkey (32 bytes)
 *   offset 170: token_1_vault Pubkey (32 bytes)
 *   offset 202: token_0_mint  Pubkey (32 bytes)
 *   offset 234: token_1_mint  Pubkey (32 bytes)
 *   offset 273: price         u64 (f64 bits)    — oracle price, token_b per token_a
 *
 * ⚠️  Offsets above are inferred from the Lifinity v2 IDL. Verify with:
 *     solana account <pool_id> --output json | python3 -c "
 *     import base64,json,struct,sys
 *     d=base64.b64decode(json.load(sys.stdin)['account']['data'][0])
 *     for o in range(0,len(d),32): print(o, d[o:o+4].hex())"
 *
 * Usage:
 *   node scripts/fetch_lifinity_pools.js
 *   RPC_URL=https://... node scripts/fetch_lifinity_pools.js
 */
"use strict";
const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const RPC_URL     = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const OUTPUT_FILE = path.join(__dirname, "..", "lifinity_pools.json");

// Lifinity v2 pool addresses.
// Find active pools at: https://lifinity.io/pools or query getProgramAccounts
// filtered by the Lifinity v2 program: EewxydAPCCVuNEyrVN68PuSadk86C9UoExahSbBPGxHA
//
// ⚠️ These addresses need to be verified on-chain before use. The bot will
// validate them at startup via check_extra — missing accounts cause a hard error.
const TARGET_POOLS = [
  // Populate from: https://lifinity.io/pools (high-TVL pools listed there)
  // Example (verify before use):
  // "2MDCFzBeKQFQirrKcBiEFvASTCm2fBvQ64pCXASwfUSK",  // SOL/USDC
  // "6FqLYjHHMiC9yAyPJpxgkRgAFgVtfUcZPi5tK5pN3K8a",  // SOL/USDT
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
    console.warn("⚠  No Lifinity pools configured. Populate TARGET_POOLS in fetch_lifinity_pools.js");
    console.warn("   Find pools at: https://lifinity.io/pools");
    fs.writeFileSync(OUTPUT_FILE, JSON.stringify([], null, 2));
    console.log("Wrote 0 Lifinity pools → lifinity_pools.json");
    return;
  }

  console.log(`Fetching ${TARGET_POOLS.length} Lifinity pool accounts...`);
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
    if (data.length < 270) {
      console.warn(`  SKIP ${addr}: data too short (${data.length} bytes, expected ≥270)`);
      return;
    }

    // ⚠️ These offsets need verification — see file header
    const ammConfig  = bs58encode(data.slice(9,  41));
    const oracle     = bs58encode(data.slice(74, 106));
    const vaultA     = bs58encode(data.slice(138, 170));
    const vaultB     = bs58encode(data.slice(170, 202));
    const mintA      = bs58encode(data.slice(202, 234));
    const mintB      = bs58encode(data.slice(234, 266));

    console.log(`  ✓ ${addr.slice(0,8)}  A=${mintA.slice(0,8)} B=${mintB.slice(0,8)}`);

    pools.push({
      id:      addr,
      dex:     "lifinity",
      token_a: mintA,
      token_b: mintB,
      vault_a: vaultA,
      vault_b: vaultB,
      fee_bps: 10, // default; overridden at runtime from state account
      state_account: addr,
      extra: {
        clmm_amm_config: ammConfig,
        oracle,
      },
    });
  });

  fs.writeFileSync(OUTPUT_FILE, JSON.stringify(pools, null, 2));
  console.log(`\nWrote ${pools.length} Lifinity pools → lifinity_pools.json`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
