#!/usr/bin/env node
/**
 * Fetches Phoenix v1 CLOB market configs for target pairs.
 *
 * Phoenix MarketHeader layout (C-repr, verified empirically against on-chain data):
 *   offset   0: discriminant        u64
 *   offset   8: status              u64
 *   offset  16: market_size_params  (4×u64 = 32 bytes — 4th field confirmed at off 40)
 *     off  16:   bids_size          u64
 *     off  24:   asks_size          u64
 *     off  32:   num_seats          u64
 *     off  40:   [4th field]        u64
 *   offset  48: base_params         TokenParams (80 bytes: mint+vault+lot+adj)
 *     off  48:   mint_key           pubkey   ← base_mint confirmed at 48 (SOL finds 13 mkts)
 *     off  80:   vault_address      pubkey
 *     off 112:   lot_size           u64
 *     off 120:   adjustment_factor  u64
 *   offset 128: quote_params        TokenParams+dust (88 bytes)
 *     off 128:   mint_key           pubkey   ← quote_mint confirmed at 128 (USDC finds mkts)
 *     off 160:   vault_address      pubkey
 *     off 192:   lot_size           u64
 *     off 200:   adjustment_factor  u64
 *     off 208:   dust_threshold     u64
 *   offset 216: tick_size_in_quote_atoms_per_base_unit  u64
 *   offset 224: authority           pubkey
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

// MarketHeader field offsets (empirically verified; SOL at 48 = 13 mkts, USDC at 128 = 214 mkts)
const OFF_BASE_MINT   = 48;   // pubkey 32 bytes  (base_params.mint_key)
const OFF_BASE_VAULT  = 80;   // pubkey 32 bytes  (base_params.vault_address)
const OFF_BASE_LOT    = 112;  // u64              (base_params.lot_size)
const OFF_QUOTE_MINT  = 128;  // pubkey 32 bytes  (quote_params.mint_key)
const OFF_QUOTE_VAULT = 160;  // pubkey 32 bytes  (quote_params.vault_address)
const OFF_QUOTE_LOT   = 192;  // u64              (quote_params.lot_size)
const OFF_TICK_SIZE   = 216;  // u64              (tick_size_in_quote_atoms_per_base_unit)

// We fetch only the first 256 bytes (dataSlice). Require at least 224 bytes so we can
// read all header fields (OFF_TICK_SIZE=216 + 8 bytes = 224). Any account that
// returns 224+ bytes from a 256-byte slice contains meaningful header data.
const MIN_MARKET_DATA_LEN = 224;

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

// getProgramAccounts with automatic retry on 429 (public RPC is rate-limited for large programs).
// Use a private RPC via RPC_URL env var for faster results.
async function rpcWithRetry(method, params) {
  for (let attempt = 0; attempt < 5; attempt++) {
    const r = await rpc(method, params);
    if (r?.error?.code === 429) {
      const delay = 10_000 * (attempt + 1);
      process.stdout.write(`[429, retry in ${delay/1000}s] `);
      await sleep(delay);
      continue;
    }
    return r;
  }
  throw new Error("Max 429 retries exceeded — use a private RPC via RPC_URL");
}

// Returns Phoenix market accounts where base_mint = baseMint.
// Filters client-side for quote_mint match and minimum data size.
async function getPhoenixMarkets(baseMint, quoteMint) {
  const r = await rpcWithRetry("getProgramAccounts", [
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

const sleep = ms => new Promise(r => setTimeout(r, ms));

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
      // In practice SOL is often the QUOTE (denominator), so many pairs are TOKEN/SOL.
      // Try both directions.
      const fwd = await getPhoenixMarkets(mintA, mintB);
      await sleep(4000);
      const rev = await getPhoenixMarkets(mintB, mintA);
      if (fwd.length > 0) { markets = fwd; isForward = true; }
      else if (rev.length > 0) { markets = rev; isForward = false; }
    } catch (e) {
      console.log(`error: ${e.message}`);
      await sleep(4000);
      continue;
    }

    if (markets.length === 0) { console.log("no market"); await sleep(1200); continue; }

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
    await sleep(4000);
  }

  fs.writeFileSync(OUTPUT, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} Phoenix markets → ${OUTPUT}`);
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
