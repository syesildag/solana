#!/usr/bin/env node
/**
 * Fetches live reserves / sqrt_price for every pool in pools.json and prints
 * a rate table showing token_a → token_b and token_b → token_a exchange rates.
 *
 * Rate sources:
 *   raydium_amm_v4  — SPL token amount from vault_a / vault_b (off 64)
 *   meteora_damm    — totalAmount from vault account (Borsh off 11), LP fraction
 *                     (a_vault_lp / b_vault_lp balances vs vault LP mint supply)
 *   orca_whirlpool  — sqrt_price_x64 from whirlpool state account (off 65, u128 LE)
 *
 * Usage:
 *   node scripts/show_rates.js
 *   RPC_URL=https://... node scripts/show_rates.js
 */
"use strict";
const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

const RPC      = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const POOLS    = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "pools.json"), "utf8"));
const BATCH_SZ = 100; // getMultipleAccounts limit

// ─── known token symbols ──────────────────────────────────────────────────────
const SYMBOLS = {
  "So11111111111111111111111111111111111111112":  "SOL",
  "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": "USDC",
  "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": "USDT",
  "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So": "mSOL",
  "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263": "BONK",
  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh": "BTC",
  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R": "RAY",
  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs": "ETH",
  "HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr": "EURC",
};
const sym = (mint) => SYMBOLS[mint] ?? mint.slice(0, 6);

// ─── token decimals ───────────────────────────────────────────────────────────
const DECIMALS = {
  "So11111111111111111111111111111111111111112":  9,
  "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": 6,
  "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB": 6,
  "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So": 9,
  "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263": 5,
  "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh": 6,
  "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R": 6,
  "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs": 8,
  "HzwqbKZw8HxMN6bF2yFZNrht3c2iXXzpKcFu7uBEDKtr": 6,
};
const dec = (mint) => 10 ** (DECIMALS[mint] ?? 9);

// ─── RPC helper ───────────────────────────────────────────────────────────────
function rpc(method, params) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
    const mod  = RPC.startsWith("https") ? https : http;
    const u    = new URL(RPC);
    const req  = mod.request(
      { hostname: u.hostname, port: u.port, path: u.pathname + u.search,
        method: "POST", headers: { "Content-Type": "application/json",
                                   "Content-Length": Buffer.byteLength(body) } },
      (r) => {
        const c = [];
        r.on("data", d => c.push(d));
        r.on("end", () => resolve(JSON.parse(Buffer.concat(c).toString())));
        r.on("error", reject);
      }
    );
    req.on("error", reject);
    req.write(body); req.end();
  });
}

async function getMultiple(pubkeys) {
  const result = {};
  for (let i = 0; i < pubkeys.length; i += BATCH_SZ) {
    const batch = pubkeys.slice(i, i + BATCH_SZ);
    const resp  = await rpc("getMultipleAccounts", [batch, { encoding: "base64" }]);
    resp.result.value.forEach((acc, j) => { result[batch[j]] = acc; });
  }
  return result;
}

// ─── parsers ──────────────────────────────────────────────────────────────────
function splAmount(data) {
  if (!data || data.length < 72) return null;
  return Number(data.readBigUInt64LE(64));
}

function meteoraVaultAmount(data) {
  if (!data || data.length < 19) return null;
  return Number(data.readBigUInt64LE(11));
}

function meteoraLpMint(data) {
  if (!data || data.length < 147) return null;
  return data.slice(115, 147).toString("base64"); // used only as map key
}

function splMintSupply(data) {
  if (!data || data.length < 44) return null;
  return Number(data.readBigUInt64LE(36));
}

// Orca whirlpool state: sqrt_price_x64 at offset 65, 16 bytes LE u128
// fee_rate at offset 45, 2 bytes LE (millionths)
function orcaSqrtPrice(data) {
  if (!data || data.length < 81) return null;
  // Read 128-bit little-endian as two 64-bit halves
  const lo = data.readBigUInt64LE(65);
  const hi = data.readBigUInt64LE(73);
  return Number((hi << 64n) | lo) / (2 ** 64);
}

// ─── main ─────────────────────────────────────────────────────────────────────
async function main() {
  // Collect all account pubkeys we need
  const needed = new Set();
  for (const p of POOLS) {
    if (p.dex === "orca_whirlpool") {
      needed.add(p.id); // state account = pool id for Orca
    } else if (p.dex === "meteora_damm") {
      needed.add(p.vault_a);
      needed.add(p.vault_b);
      needed.add(p.extra.a_vault_lp);
      needed.add(p.extra.b_vault_lp);
    } else {
      needed.add(p.vault_a);
      needed.add(p.vault_b);
    }
  }

  process.stderr.write(`Fetching ${needed.size} accounts…\n`);
  const accounts = await getMultiple([...needed]);

  // For Meteora: also need vault LP mint supplies
  // Vault LP mint is at offset 115 in vault account data
  const lpMintPubkeys = new Map(); // base58 pubkey string → vault pubkey
  for (const p of POOLS) {
    if (p.dex !== "meteora_damm") continue;
    for (const vaultKey of [p.vault_a, p.vault_b]) {
      const acc = accounts[vaultKey];
      if (!acc) continue;
      const data = Buffer.from(acc.data[0], "base64");
      if (data.length < 147) continue;
      // Extract LP mint pubkey as base58 using the b58enc logic
      const mintBytes = data.slice(115, 147);
      const b58 = encodeBase58(mintBytes);
      lpMintPubkeys.set(b58, vaultKey);
      needed.add(b58);
    }
  }

  if (lpMintPubkeys.size > 0) {
    process.stderr.write(`Fetching ${lpMintPubkeys.size} vault LP mint accounts…\n`);
    const mintAccs = await getMultiple([...lpMintPubkeys.keys()]);
    Object.assign(accounts, mintAccs);
    // Store mint pubkey → supply mapping
    for (const [mintKey] of lpMintPubkeys) {
      const acc = accounts[mintKey];
      if (!acc) continue;
      const data = Buffer.from(acc.data[0], "base64");
      accounts[mintKey + "__supply"] = splMintSupply(data);
    }
    // Store vault → lp mint key mapping for later lookup
    for (const [mintKey, vaultKey] of lpMintPubkeys) {
      accounts[vaultKey + "__lpMint"] = mintKey;
    }
  }

  // Print table
  const DEX_LABEL = {
    raydium_amm_v4: "Raydium",
    orca_whirlpool: "Orca   ",
    meteora_damm:   "Meteora",
  };

  const rows = [];
  for (const p of POOLS) {
    const pair = `${sym(p.token_a)}/${sym(p.token_b)}`;
    const dex  = DEX_LABEL[p.dex] ?? p.dex;
    let rateAtoB = null;
    let rateBtoA = null;
    let reserveA = null;
    let reserveB = null;

    if (p.dex === "orca_whirlpool") {
      const acc = accounts[p.id];
      if (acc) {
        const data = Buffer.from(acc.data[0], "base64");
        const sqrt = orcaSqrtPrice(data);
        if (sqrt !== null && sqrt > 0) {
          const fee = 1 - (p.fee_bps / 10_000);
          // sqrt_price is in raw units: token_b per token_a (no decimal adjustment here)
          // Adjust for decimals: price_human = sqrt^2 * dec(a) / dec(b)
          const rawPrice = sqrt * sqrt;
          rateAtoB = rawPrice * (dec(p.token_a) / dec(p.token_b)) * fee;
          rateBtoA = (1 / rawPrice) * (dec(p.token_b) / dec(p.token_a)) * fee;
        }
      }
    } else if (p.dex === "meteora_damm") {
      const vA = accounts[p.vault_a];
      const vB = accounts[p.vault_b];
      const lpA = accounts[p.extra.a_vault_lp];
      const lpB = accounts[p.extra.b_vault_lp];
      if (vA && vB && lpA && lpB) {
        const dA = Buffer.from(vA.data[0], "base64");
        const dB = Buffer.from(vB.data[0], "base64");
        const dLpA = Buffer.from(lpA.data[0], "base64");
        const dLpB = Buffer.from(lpB.data[0], "base64");
        const totalA = meteoraVaultAmount(dA);
        const totalB = meteoraVaultAmount(dB);
        const lpBalA = splAmount(dLpA);
        const lpBalB = splAmount(dLpB);
        const mintKeyA = accounts[p.vault_a + "__lpMint"];
        const mintKeyB = accounts[p.vault_b + "__lpMint"];
        const supplyA = mintKeyA ? accounts[mintKeyA + "__supply"] : null;
        const supplyB = mintKeyB ? accounts[mintKeyB + "__supply"] : null;
        if (totalA != null && totalB != null && lpBalA != null && lpBalB != null && supplyA && supplyB) {
          reserveA = totalA * lpBalA / supplyA;
          reserveB = totalB * lpBalB / supplyB;
        }
      }
      if (reserveA != null && reserveB != null && reserveA > 0 && reserveB > 0) {
        const fee = 1 - (p.fee_bps / 10_000);
        rateAtoB = (reserveB / reserveA) * (dec(p.token_a) / dec(p.token_b)) * fee;
        rateBtoA = (reserveA / reserveB) * (dec(p.token_b) / dec(p.token_a)) * fee;
      }
    } else {
      // raydium_amm_v4
      const vA = accounts[p.vault_a];
      const vB = accounts[p.vault_b];
      if (vA && vB) {
        const dA = Buffer.from(vA.data[0], "base64");
        const dB = Buffer.from(vB.data[0], "base64");
        reserveA = splAmount(dA);
        reserveB = splAmount(dB);
      }
      if (reserveA != null && reserveB != null && reserveA > 0 && reserveB > 0) {
        const fee = 1 - (p.fee_bps / 10_000);
        rateAtoB = (reserveB / reserveA) * (dec(p.token_a) / dec(p.token_b)) * fee;
        rateBtoA = (reserveA / reserveB) * (dec(p.token_b) / dec(p.token_a)) * fee;
      }
    }

    rows.push({ dex, pair, fee: p.fee_bps, rateAtoB, rateBtoA });
  }

  // Print
  const W = { dex: 7, pair: 14, fee: 6, rate: 14 };
  const header =
    "DEX     ".padEnd(W.dex + 1) +
    "Pair          ".padEnd(W.pair + 1) +
    "Fee   ".padStart(W.fee + 1) +
    "  A→B (human)".padStart(W.rate + 1) +
    "  B→A (human)".padStart(W.rate + 1);
  console.log(header);
  console.log("─".repeat(header.length));

  for (const r of rows) {
    const fmtRate = (v) => {
      if (v == null) return "—".padStart(W.rate);
      if (v < 0.000001) return "~0".padStart(W.rate);
      // Use scientific notation for very large or very small values
      if (v >= 1_000_000 || (v > 0 && v < 0.001))
        return v.toExponential(3).padStart(W.rate);
      return v.toFixed(6).padStart(W.rate);
    };
    console.log(
      r.dex.padEnd(W.dex + 1) +
      r.pair.padEnd(W.pair + 1) +
      `${r.fee}bps`.padStart(W.fee + 1) +
      "  " + fmtRate(r.rateAtoB) +
      "  " + fmtRate(r.rateBtoA)
    );
  }
}

// ─── base58 encoder ───────────────────────────────────────────────────────────
const BASE58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
function encodeBase58(buf) {
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
  return str + digits.reverse().map(x => BASE58[x]).join("");
}

main().catch(e => { console.error("Fatal:", e.message); process.exit(1); });
