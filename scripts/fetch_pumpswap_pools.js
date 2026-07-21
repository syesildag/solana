#!/usr/bin/env node
/**
 * Fetches PumpSwap (pump.fun AMM) pool configs for pinned pool addresses and writes
 * them in the bot's PoolConfig schema (dex: "pump_swap").
 *
 * PRICING-ONLY VENUE: these entries exist so the portfolio-watcher's gRPC feed can
 * price momentum tokens whose main liquidity is on pumpswap (vault-reserve CP math).
 * The arb bot's PoolRegistry skips dex="pump_swap" at load — it never trades them.
 *
 * PumpSwap "Pool" account layout (anchor; PRIMARY candidate, cross-checked at runtime):
 *   +0    discriminator            [u8;8]
 *   +8    pool_bump                u8
 *   +9    index                    u16
 *   +11   creator                  Pubkey (32)
 *   +43   base_mint                Pubkey (32)
 *   +75   quote_mint               Pubkey (32)
 *   +107  lp_mint                  Pubkey (32)
 *   +139  pool_base_token_account  Pubkey (32)
 *   +171  pool_quote_token_account Pubkey (32)
 *   +203  lp_supply                u64
 *   [+211 coin_creator             Pubkey (32) — newer pools]
 *
 * The offsets are NOT trusted blindly: for every pool we fetch both decoded vault
 * accounts and require each to be an SPL token account whose mint matches the decoded
 * base/quote mint. If the primary layout fails that cross-check we retry with the
 * ALTERNATE layout (no creator field: mints at +11/+43, vaults at +75/+107 shifted
 * −32) and error loudly if neither validates — a silent mis-decode would feed the
 * watcher prices from the wrong accounts.
 *
 * Usage:
 *   node scripts/fetch_pumpswap_pools.js
 *   node scripts/fetch_pumpswap_pools.js --pools <addr,addr,…>   # ad-hoc, skips pinned list
 *   node scripts/fetch_pumpswap_pools.js --output out.json
 */

"use strict";

const https = require("https");
const http  = require("http");
const fs    = require("fs");
const path  = require("path");

// ─── Config ───────────────────────────────────────────────────────────────────

const RPC_URL = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";
const OUTPUT_FILE = process.argv.includes("--output")
  ? process.argv[process.argv.indexOf("--output") + 1]
  : path.join(__dirname, "..", "pumpswap_pools.json");

const PUMPSWAP_PROGRAM = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const TOKEN_PROGRAMS = new Set([
  "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", // SPL Token
  "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb", // Token-2022
]);

// ── Pinned pools (portfolio-watcher gRPC pricing; keep in sync with the `pool`
//    fields in assets/momentum_tokens.json — same convention as fetch_orca_pools.js) ──
const TARGET_POOLS = [
  // (add pools here when a vetted momentum token's best venue is pumpswap;
  //  vet-momentum-token picks the pool address via GeckoTerminal at --add time)
  "2uF4Xh61rDwxnG9woyxsVQP7zuA6kLFpb3NvnRQeoiSd", // PUMP/USDC  tvl=$12.9M — seed/reference pool (layout verified 2026-07-04)
  "636bkx7Ugs6Vdb9FhAJdwFdi4afupHDarrTW2nTVuEag", // Agamemnon/SOL — momentum watch (added 2026-07-19; pump.fun day-0 launch)
  "HuqmPUBBdq8w56Y6WGd8LiMbf4zgYXS2ACzLZs8MYLna", // BULLCAT/SOL — momentum watch (added 2026-07-19; Jul-14 pump.fun launch, Jupiter-verified)
  "5PGhKctym6odbHGo2tKMST2AjmJsb2uZBQrKkn4ZuFT5", // Jimothy/SOL — momentum watch (added 2026-07-19; Jul-16 pump.fun launch, unverified)
  "9nbVEMyVgqhDkATKGaQrW3vVuSprQjZo24oaLYA7nchi", // Jimhood/SOL — momentum watch (added 2026-07-21; Jul-20 pump.fun launch, unverified)
  "5dvo7afWw1xVLcZzqofokjaEpsEzBb3UukYoEnFi6Le5", // Chonketha/SOL — momentum watch (added 2026-07-21; Jul-18 pump.fun launch, unverified)
  "EE3zk9Fxp9guair2xeReFxf4TsEXeZFFuWETRna2PkcV", // TOESCOIN/SOL — momentum watch (added 2026-07-21; May-19 launch, Jupiter-verified)
];

// PumpSwap fee: 20 bps LP + 5 bps protocol ≈ 25 bps total. Creator-fee pools may
// differ by a few bps — pricing-only usage, so ±5 bps is immaterial.
const FEE_BPS = 25;

// ─── RPC helper ───────────────────────────────────────────────────────────────

function rpcPost(url, body) {
  return new Promise((resolve, reject) => {
    const mod = url.startsWith("https") ? https : http;
    const req = mod.request(url, {
      method: "POST",
      headers: { "content-type": "application/json" },
    }, (res) => {
      let buf = "";
      res.on("data", (c) => (buf += c));
      res.on("end", () => {
        try { resolve(JSON.parse(buf)); } catch (e) { reject(e); }
      });
    });
    req.on("error", reject);
    req.end(JSON.stringify(body));
  });
}

async function getAccount(address) {
  const res = await rpcPost(RPC_URL, {
    jsonrpc: "2.0", id: 1, method: "getAccountInfo",
    params: [address, { encoding: "base64" }],
  });
  const info = res?.result?.value;
  if (!info) return null;
  return { data: Buffer.from(info.data[0], "base64"), owner: info.owner };
}

// ─── Base58 (encode only — we only turn raw pubkeys into strings) ─────────────

const BASE58_ALPHABET = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

function bs58encode(bytes) {
  const digits = [0];
  for (const byte of bytes) {
    let carry = byte;
    for (let i = 0; i < digits.length; i++) {
      carry += digits[i] << 8;
      digits[i] = carry % 58;
      carry = (carry / 58) | 0;
    }
    while (carry > 0) { digits.push(carry % 58); carry = (carry / 58) | 0; }
  }
  let out = "";
  for (const byte of bytes) { if (byte !== 0) break; out += "1"; }
  for (let i = digits.length - 1; i >= 0; i--) out += BASE58_ALPHABET[digits[i]];
  return out;
}

// ─── Layout decode + on-chain cross-check ─────────────────────────────────────

/** Decode a Pool account at the given mint offset (43 = primary layout with a
 *  creator field before the mints; 11 = alternate layout without it). */
function decodeAt(data, mintOff) {
  const pk = (off) => bs58encode(data.slice(off, off + 32));
  return {
    baseMint:   pk(mintOff),
    quoteMint:  pk(mintOff + 32),
    lpMint:     pk(mintOff + 64),
    baseVault:  pk(mintOff + 96),
    quoteVault: pk(mintOff + 128),
  };
}

/** An SPL token account's mint is bytes 0..32; owner must be a token program. */
async function vaultMatches(vaultAddr, expectedMint) {
  const acct = await getAccount(vaultAddr);
  if (!acct || !TOKEN_PROGRAMS.has(acct.owner) || acct.data.length < 72) return false;
  return bs58encode(acct.data.slice(0, 32)) === expectedMint;
}

async function decodeAndVerify(address, data) {
  for (const [label, mintOff] of [["primary(+43)", 43], ["alternate(+11)", 11]]) {
    const d = decodeAt(data, mintOff);
    if (await vaultMatches(d.baseVault, d.baseMint) &&
        await vaultMatches(d.quoteVault, d.quoteMint)) {
      return { ...d, layout: label };
    }
  }
  throw new Error(
    `pool ${address}: neither layout candidate passes the vault↔mint cross-check — ` +
    "the PumpSwap account layout has changed; fix decodeAt() before trusting output"
  );
}

// ─── Main ─────────────────────────────────────────────────────────────────────

(async () => {
  const cliPools = process.argv.includes("--pools")
    ? process.argv[process.argv.indexOf("--pools") + 1].split(",").map((s) => s.trim()).filter(Boolean)
    : null;
  const targets = cliPools ?? TARGET_POOLS;

  const results = [];
  for (const address of targets) {
    process.stdout.write(`  ${address.slice(0, 8)}… `);
    try {
      const acct = await getAccount(address);
      if (!acct) { console.log("account not found"); continue; }
      if (acct.owner !== PUMPSWAP_PROGRAM) {
        console.log(`SKIP — owner ${acct.owner.slice(0, 8)}… is not the pumpswap program`);
        continue;
      }
      const pool = await decodeAndVerify(address, acct.data);
      results.push({
        id:      address,
        dex:     "pump_swap",
        token_a: pool.baseMint,
        token_b: pool.quoteMint,
        vault_a: pool.baseVault,
        vault_b: pool.quoteVault,
        fee_bps: FEE_BPS,
        extra:   {},
      });
      console.log(`✓  base=${pool.baseMint.slice(0, 6)} quote=${pool.quoteMint.slice(0, 6)} [${pool.layout}]`);
    } catch (e) {
      console.log(`error: ${e.message}`);
      process.exitCode = 1; // loud failure — a mis-decoded pool must not slip into pools.json
    }
  }

  fs.writeFileSync(OUTPUT_FILE, JSON.stringify(results, null, 2));
  console.log(`\nWrote ${results.length} PumpSwap pool(s) → ${OUTPUT_FILE}`);
  if (targets.length === 0) {
    console.log("  (no pools pinned — add addresses to TARGET_POOLS or pass --pools)");
  }
})();
