#!/usr/bin/env node
/**
 * Fetches Invariant CLMM pool configs and writes invariant_pools.json.
 *
 * Invariant uses Q64.64 sqrt_price_x64 at the same offset as Orca Whirlpool,
 * so the Rust bot reuses the Orca quote path for graph edge computation.
 *
 * Pool state layout (after 8-byte Anchor discriminator):
 *   offset 41: tick_spacing        u16    (2 bytes)
 *   offset 45: fee_rate            u16    (2 bytes) — in hundredths of a bip
 *   offset 65: sqrt_price          u128   (16 bytes, Q64.64)
 *   offset 81: tick_current_index  i32    (4 bytes)
 *   offset 101: token_mint_a       Pubkey (32 bytes)
 *   offset 133: token_vault_a      Pubkey (32 bytes)
 *   offset 181: token_mint_b       Pubkey (32 bytes)   ← Note: gap at 165–181
 *   offset 213: token_vault_b      Pubkey (32 bytes)   ← (fee growth accumulators)
 *   offset 261: oracle             Pubkey (32 bytes)
 *
 * ⚠️  Invariant may use a slightly different layout from Orca Whirlpool despite
 * being derived from the same spec. Verify offsets against:
 *   https://github.com/invariant-labs/protocol-solana
 *
 * Usage:
 *   node scripts/fetch_invariant_pools.js
 *   RPC_URL=https://... node scripts/fetch_invariant_pools.js
 */
"use strict";
const https  = require("https");
const http   = require("http");
const crypto = require("crypto");
const fs     = require("fs");
const path   = require("path");

const RPC_URL      = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const OUTPUT_FILE  = path.join(__dirname, "..", "invariant_pools.json");
const INV_PROGRAM  = "HyaB3W9q6XdA5xwpU4XnSZV94htfmbmqJXZcEbRaJutt";
const TICK_ARRAY_SIZE = 88;

// Invariant pool addresses to fetch.
// Find pools at: https://invariant.app/liquidity or via getProgramAccounts
// ⚠️ Verify these addresses before enabling live trading.
const TARGET_POOLS = [
  // Populate from the Invariant UI or program account query:
  // const { Connection } = require("@solana/web3.js");
  // connection.getProgramAccounts(new PublicKey(INV_PROGRAM), { filters: [...] })
];

const BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
const BASE58_MAP = new Uint8Array(256).fill(255);
for (let i = 0; i < BASE58_ALPHABET.length; i++) BASE58_MAP[BASE58_ALPHABET.charCodeAt(i)] = i;

function bs58decode(str) {
  const bytes = [];
  for (const ch of str) {
    let carry = BASE58_MAP[ch.charCodeAt(0)];
    for (let i = 0; i < bytes.length; i++) {
      carry += bytes[i] * 58;
      bytes[i] = carry & 0xff;
      carry >>= 8;
    }
    while (carry > 0) { bytes.push(carry & 0xff); carry >>= 8; }
  }
  for (const ch of str) { if (ch !== "1") break; bytes.push(0); }
  return new Uint8Array(bytes.reverse());
}

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

async function findPDA(seeds, programId) {
  const programIdBytes = bs58decode(programId);
  for (let nonce = 255; nonce >= 0; nonce--) {
    const toHash = Buffer.concat([
      ...seeds,
      Buffer.from([nonce]),
      programIdBytes,
      Buffer.from("ProgramDerivedAddress"),
    ]);
    const hash = crypto.createHash("sha256").update(toHash).digest();
    return [bs58encode(hash), nonce];
  }
  throw new Error("Could not find PDA");
}

function tickIndexToStartTick(tickIndex, tickSpacing) {
  const ticks = TICK_ARRAY_SIZE * tickSpacing;
  return Math.floor(tickIndex / ticks) * ticks;
}

async function getTickArrayPDA(startTick, poolAddr) {
  const poolBytes = bs58decode(poolAddr);
  const startTickBuf = Buffer.alloc(4);
  startTickBuf.writeInt32LE(startTick, 0);
  return findPDA([Buffer.from("tick_array"), poolBytes, startTickBuf], INV_PROGRAM);
}

async function getOraclePDA(poolAddr) {
  const poolBytes = bs58decode(poolAddr);
  return findPDA([Buffer.from("oracle"), poolBytes], INV_PROGRAM);
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
    console.warn("⚠  No Invariant pools configured. Populate TARGET_POOLS in fetch_invariant_pools.js");
    console.warn("   Find pools at: https://invariant.app/liquidity");
    fs.writeFileSync(OUTPUT_FILE, JSON.stringify([], null, 2));
    console.log("Wrote 0 Invariant pools → invariant_pools.json");
    return;
  }

  console.log(`Fetching ${TARGET_POOLS.length} Invariant pool accounts...`);
  const resp = await rpcPost(RPC_URL, {
    jsonrpc: "2.0", id: 1, method: "getMultipleAccounts",
    params: [TARGET_POOLS, { encoding: "base64" }],
  });
  if (resp.error) throw new Error("RPC error: " + JSON.stringify(resp.error));

  const pools = [];
  for (let i = 0; i < TARGET_POOLS.length; i++) {
    const addr = TARGET_POOLS[i];
    const acc  = resp.result.value[i];
    if (!acc) { console.warn(`  SKIP ${addr}: not found`); continue; }

    const data = Buffer.from(acc.data[0], "base64");
    if (data.length < 270) { console.warn(`  SKIP ${addr}: too short`); continue; }

    const tickSpacing = data.readUInt16LE(41);
    const feeRate     = data.readUInt16LE(45);
    const tick        = data.readInt32LE(81);
    const mintA       = bs58encode(data.slice(101, 133));
    const vaultA      = bs58encode(data.slice(133, 165));
    const mintB       = bs58encode(data.slice(181, 213));
    const vaultB      = bs58encode(data.slice(213, 245));
    const feeBps      = Math.round(feeRate / 100);

    const ticks = TICK_ARRAY_SIZE * tickSpacing;
    const st0 = Math.floor(tick / ticks) * ticks;
    const [ta0] = await getTickArrayPDA(st0, addr);
    const [ta1] = await getTickArrayPDA(st0 + ticks, addr);
    const [ta2] = await getTickArrayPDA(st0 - ticks, addr);
    const [oracle] = await getOraclePDA(addr);

    console.log(`  ✓ ${addr.slice(0,8)}  A=${mintA.slice(0,8)} B=${mintB.slice(0,8)} fee=${feeBps}bps ts=${tickSpacing}`);

    pools.push({
      id:            addr,
      dex:           "invariant",
      token_a:       mintA,
      token_b:       mintB,
      vault_a:       vaultA,
      vault_b:       vaultB,
      fee_bps:       feeBps,
      state_account: addr,
      extra: {
        clmm_tick_spacing: tickSpacing,
        oracle,
        tick_array_0: ta0,
        tick_array_1: ta1,
        tick_array_2: ta2,
      },
    });
  }

  fs.writeFileSync(OUTPUT_FILE, JSON.stringify(pools, null, 2));
  console.log(`\nWrote ${pools.length} Invariant pools → invariant_pools.json`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
