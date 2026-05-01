#!/usr/bin/env node
/**
 * Fetches Phoenix v1 CLOB market configs for target pairs.
 *
 * Phoenix MarketHeader layout (C-repr, no Anchor discriminator prefix):
 *   offset   0: discriminant        u64
 *   offset   8: status              u64
 *   offset  16: market_size_params  (3×u64 = 24 bytes)
 *   offset  40: base_params         TokenParams (24 + 32 + 32 = 88 bytes)
 *     off  40:   lot_size           u64
 *     off  48:   adjustment_factor  u64
 *     off  56:   dust_threshold     u64
 *     off  64:   vault_address      pubkey
 *     off  96:   mint_key           pubkey
 *   offset 128: base_lot_size       u64
 *   offset 136: quote_params        TokenParams (88 bytes)
 *     off 136:   lot_size           u64
 *     off 144:   adjustment_factor  u64
 *     off 152:   dust_threshold     u64
 *     off 160:   vault_address      pubkey
 *     off 192:   mint_key           pubkey
 *   offset 224: quote_lot_size      u64
 *   offset 232: tick_size           u64
 *   offset 240: authority           pubkey
 *   ...
 *
 * Usage:
 *   node scripts/fetch_phoenix.js [--output phoenix_pools.json]
 *   RPC_URL=https://... node scripts/fetch_phoenix.js
 */
"use strict";
const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const PHOENIX_PROGRAM = "PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY";
const RPC             = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const OUTPUT          = process.argv.includes("--output")
  ? process.argv[process.argv.indexOf("--output") + 1]
  : path.join(__dirname, "..", "phoenix_pools.json");

// MarketHeader field offsets
const OFF_BASE_VAULT  = 64;   // pubkey 32 bytes
const OFF_BASE_MINT   = 96;   // pubkey 32 bytes
const OFF_QUOTE_VAULT = 160;  // pubkey 32 bytes
const OFF_QUOTE_MINT  = 192;  // pubkey 32 bytes
const OFF_BASE_LOT    = 128;  // u64
const OFF_QUOTE_LOT   = 224;  // u64
const OFF_TICK_SIZE   = 232;  // u64 (quote atoms per base unit × 10^6 precision)

// Phoenix market accounts are large (order book data). Non-market program accounts
// (seats, vaults, etc.) are small. We filter by minimum data length to skip them.
const MIN_MARKET_DATA_LEN = 512;

const MINTS = {
  SOL:  "So11111111111111111111111111111111111111112",
  USDC: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
  USDT: "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",
  RAY:  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R",
  MSOL: "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",
  ETH:  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs",
  BTC:  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh",
};

// Phoenix markets: base is always the "asset" token, quote is always the "pricing" token.
// Each pair is searched both ways (A as base/quote).
const PHOENIX_PAIRS = [
  ["SOL","USDC"],["SOL","USDT"],
  ["BTC","USDC"],["ETH","USDC"],
  ["MSOL","SOL"],["RAY","USDC"],
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

// Returns Phoenix market accounts where base_mint = baseMint.
// Filters client-side for quote_mint match and minimum data size.
async function getPhoenixMarkets(baseMint, quoteMint) {
  const r = await rpc("getProgramAccounts", [
    PHOENIX_PROGRAM,
    {
      encoding:   "base64",
      dataSlice:  { offset: 0, length: 256 },  // only need the header
      filters: [
        { memcmp: { offset: OFF_BASE_MINT,  bytes: baseMint  } },
        { memcmp: { offset: OFF_QUOTE_MINT, bytes: quoteMint } },
      ],
    },
  ]);
  if (r?.error) throw new Error("getProgramAccounts error: " + JSON.stringify(r.error));
  if (!Array.isArray(r?.result)) return [];

  const results = [];
  for (const acc of r.result) {
    const raw = Buffer.from(acc.account.data[0], "base64");
    if (raw.length < MIN_MARKET_DATA_LEN) continue;

    const bMint  = b58enc(raw.slice(OFF_BASE_MINT,  OFF_BASE_MINT  + 32));
    const qMint  = b58enc(raw.slice(OFF_QUOTE_MINT, OFF_QUOTE_MINT + 32));
    const bVault = b58enc(raw.slice(OFF_BASE_VAULT,  OFF_BASE_VAULT  + 32));
    const qVault = b58enc(raw.slice(OFF_QUOTE_VAULT, OFF_QUOTE_VAULT + 32));
    const baseLot  = raw.readBigUInt64LE(OFF_BASE_LOT);
    const quoteLot = raw.readBigUInt64LE(OFF_QUOTE_LOT);
    const tickSize = raw.readBigUInt64LE(OFF_TICK_SIZE);

    if (bMint !== baseMint || qMint !== quoteMint) continue;  // double-check

    results.push({
      pubkey: acc.pubkey,
      baseMint: bMint, quoteMint: qMint,
      baseVault: bVault, quoteVault: qVault,
      baseLotSize: baseLot, quoteLotSize: quoteLot, tickSize,
    });
  }
  return results;
}

// ─── Main ─────────────────────────────────────────────────────────────────────

async function main() {
  const seen    = new Set();   // deduplicate by market pubkey
  const results = [];

  for (const [symA, symB] of PHOENIX_PAIRS) {
    const mintA = MINTS[symA], mintB = MINTS[symB];
    process.stdout.write(`  ${symA}/${symB}… `);

    let markets = [];
    let isForward = true;
    try {
      // Phoenix convention: base = token being priced, quote = pricing currency.
      // Try (A=base, B=quote) then (B=base, A=quote).
      const fwd = await getPhoenixMarkets(mintA, mintB);
      const rev = await getPhoenixMarkets(mintB, mintA);
      if (fwd.length > 0) { markets = fwd; isForward = true; }
      else if (rev.length > 0) { markets = rev; isForward = false; }
    } catch (e) {
      console.log(`error: ${e.message}`);
      continue;
    }

    if (markets.length === 0) { console.log("no market"); continue; }

    // If multiple markets exist for the same pair, take the first (Phoenix typically has one).
    const m = markets[0];
    if (seen.has(m.pubkey)) { console.log("duplicate"); continue; }
    seen.add(m.pubkey);

    // Normalise: token_a = base, token_b = quote (price of base in quote terms)
    const token_a = isForward ? m.baseMint  : m.quoteMint;
    const token_b = isForward ? m.quoteMint : m.baseMint;
    const vault_a = isForward ? m.baseVault : m.quoteVault;
    const vault_b = isForward ? m.quoteVault : m.baseVault;

    results.push({
      id:       m.pubkey,
      dex:      "phoenix",
      token_a,
      token_b,
      vault_a,
      vault_b,
      fee_bps:  10,  // Phoenix default taker fee; extracted from FeeModelConfigParams
      state_account: m.pubkey,
      extra: {
        phoenix_base_lot_size:  m.baseLotSize.toString(),
        phoenix_quote_lot_size: m.quoteLotSize.toString(),
        phoenix_tick_size:      m.tickSize.toString(),
      },
    });

    console.log(`✓  ${m.pubkey}`);
  }

  fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} Phoenix markets → ${OUTPUT}`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
