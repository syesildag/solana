#!/usr/bin/env node
/**
 * Fetches Meteora DAMM v1 pool configs and resolves vault accounts via RPC.
 *
 * DAMM pool state layout (after 8-byte Anchor discriminator):
 *   offset  8: lp_mint        (Pubkey, 32 bytes)
 *   offset 40: token_a_mint   (Pubkey, 32 bytes)
 *   offset 72: token_b_mint   (Pubkey, 32 bytes)
 *   offset104: a_vault        (Meteora vault account, 32 bytes)
 *   offset136: b_vault        (Meteora vault account, 32 bytes)
 *
 * pool.vault_a / pool.vault_b are Meteora vault accounts (not SPL token accounts).
 * reserve_a / reserve_b are parsed from vault.totalAmount at Borsh offset 11.
 *
 * Usage:
 *   node scripts/fetch_meteora_pools.js
 */
"use strict";
const https = require("https");
const fs    = require("fs");
const path  = require("path");

const RPC = process.env.RPC_URL || "https://api.mainnet-beta.solana.com";

// Curve stable-swap pools: use the StableSwap (Curve) invariant, not x*y=k.
// `stable: true` triggers Curve math in the Rust bot; `damm_amp` is the
// amplification coefficient (100 is the Meteora default).
//
// Only include TRUE stablecoin pairs (value pegged 1:1).
// LST/SOL pools (e.g. SOL/mSOL) use a Meteora "virtual price" multiplier
// that scales reserves by the staking exchange rate before applying the Curve
// invariant. Without that multiplier we'd compute rate≈1.009 for SOL→mSOL
// when the real rate is 1/1.375≈0.727, generating phantom 38% profit cycles.
// LST/SOL support requires reading the virtual_price_r from pool state — TODO.
// Fallback amp values — overridden by on-chain pool state parsing below.
// Layout: curveType byte at offset 874 (1=Stable), amp u64 at offset 875.
const STABLE_POOLS = new Map([
  ["HcjZvfeSNJbNkfLD4eEcRBr96AD3w1GpmMppaeRZf7ur", 1000],  // SOL/mSOL  (virtual_price_r fetched at bot startup)
  ["32D4zRxNc1EssbJieVHfPhZM3rH6CzfUPrWUuWxD9prG", 8000],  // USDC/USDT
  ["EMyXvKEi9izVMMsJPaSx8SZzoW69brf9MDPMEbwKDCvF", 8000],  // USDT/USDC
]);

// Target DAMM v1 pools (by address), curated for SOL/USDC/BTC/BONK/USDT/mSOL pairs.
const TARGET_POOLS = [
  "HcjZvfeSNJbNkfLD4eEcRBr96AD3w1GpmMppaeRZf7ur",  // SOL/mSOL  tvl=6.2M
  "32D4zRxNc1EssbJieVHfPhZM3rH6CzfUPrWUuWxD9prG",  // USDC/USDT tvl=2.4M
  "EMyXvKEi9izVMMsJPaSx8SZzoW69brf9MDPMEbwKDCvF",  // USDT/USDC tvl=199K
  "9CopBY6iQBaZKAhhQANfy7g4VXZkx9zKm8AisPd5Ufay",  // SOL/USDT  tvl=98K
  "5NQTw1WqVEt6wP1LmohsrYDyJp2NDipdv6eULVNByXMb",  // BTC/USDC  tvl=113K
  "9nfomE7jP17PqEc91ohSzPsrRiK7LX3La1rDarMJDcj9",  // BTC/SOL   tvl=9K
  "6SWtsTzXrurtVWZdEHvnQdE9oM8tTtyg8rfEo3b4nM93",  // SOL/USDC  tvl=18K
];

// ─── helpers ────────────────────────────────────────────────────────────────

function rpc(method, params) {
  return new Promise((resolve, reject) => {
    const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
    const url  = new URL(RPC);
    const req  = https.request(
      {
        hostname: url.hostname,
        path:     url.pathname + url.search,
        method:   "POST",
        headers:  { "Content-Type": "application/json", "Content-Length": Buffer.byteLength(body) },
      },
      (r) => {
        const chunks = [];
        r.on("data", (c) => chunks.push(c));
        r.on("end",  () => resolve(JSON.parse(Buffer.concat(chunks).toString())));
        r.on("error", reject);
      }
    );
    req.on("error", reject);
    req.write(body);
    req.end();
  });
}

const BASE58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

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
  return str + digits.reverse().map((x) => BASE58[x]).join("");
}

// ─── main ────────────────────────────────────────────────────────────────────

async function main() {
  console.error(`Fetching ${TARGET_POOLS.length} Meteora DAMM pool accounts via RPC...`);

  const resp = await rpc("getMultipleAccounts", [TARGET_POOLS, { encoding: "base64" }]);
  if (resp.error) throw new Error("RPC error: " + JSON.stringify(resp.error));

  const pools = [];

  resp.result.value.forEach((acc, i) => {
    const addr = TARGET_POOLS[i];
    if (!acc) {
      console.error(`  SKIP ${addr}: account not found`);
      return;
    }

    const data = Buffer.from(acc.data[0], "base64");
    if (data.length < 168) {
      console.error(`  SKIP ${addr}: data too short (${data.length})`);
      return;
    }

    const tokenAMint        = b58enc(data.slice(40, 72));
    const tokenBMint        = b58enc(data.slice(72, 104));
    const aVault            = b58enc(data.slice(104, 136));
    const bVault            = b58enc(data.slice(136, 168));
    const aVaultLp          = b58enc(data.slice(168, 200));  // pool's LP token acct in vault A
    const bVaultLp          = b58enc(data.slice(200, 232));  // pool's LP token acct in vault B
    const adminTokenFeeA    = b58enc(data.slice(232, 264));  // admin fee account for token A
    const adminTokenFeeB    = b58enc(data.slice(264, 296));  // admin fee account for token B

    // Parse amp from pool state: curveType discriminant=1 (Stable) at offset 874, amp u64 at 875.
    let chainAmp = null;
    if (STABLE_POOLS.has(addr) && data.length >= 883 && data[874] === 1) {
      chainAmp = Number(data.readBigUInt64LE(875));
    }
    const amp = chainAmp || STABLE_POOLS.get(addr);
    if (chainAmp) {
      console.error(`  OK ${addr}  A=${tokenAMint.slice(0,8)} B=${tokenBMint.slice(0,8)} vA=${aVault.slice(0,8)} vB=${bVault.slice(0,8)} lpA=${aVaultLp.slice(0,8)} lpB=${bVaultLp.slice(0,8)} amp=${chainAmp}`);
    } else {
      console.error(`  OK ${addr}  A=${tokenAMint.slice(0,8)} B=${tokenBMint.slice(0,8)} vA=${aVault.slice(0,8)} vB=${bVault.slice(0,8)} lpA=${aVaultLp.slice(0,8)} lpB=${bVaultLp.slice(0,8)}`);
    }

    pools.push({
      id:       addr,
      dex:      "meteora_damm",
      token_a:  tokenAMint,
      token_b:  tokenBMint,
      vault_a:  aVault,
      vault_b:  bVault,
      fee_bps:  25,
      ...(STABLE_POOLS.has(addr) && { stable: true }),
      _aVault:  aVault,   // temp: used below to fetch vault accounts
      _bVault:  bVault,
      extra: {
        a_vault_lp:       aVaultLp,
        b_vault_lp:       bVaultLp,
        admin_token_fee_a: adminTokenFeeA,
        admin_token_fee_b: adminTokenFeeB,
        ...(STABLE_POOLS.has(addr) && { damm_amp: amp }),
      },
    });
  });

  // ── Fetch vault accounts to get token_vault and lp_mint for each vault ──────
  // Meteora vault state (after 8-byte discriminator):
  //   off  8: enabled (u8) + bumps (2 bytes)
  //   off 11: totalAmount (u64)
  //   off 19: tokenVault (Pubkey, 32 bytes)  ← SPL token account holding underlying tokens
  //   off 51: feeVault (Pubkey, 32 bytes)
  //   off 83: tokenMint (Pubkey, 32 bytes)
  //   off115: lpMint   (Pubkey, 32 bytes)    ← LP mint whose supply we track

  const uniqueVaults = [...new Set(pools.flatMap(p => [p._aVault, p._bVault]))];
  console.error(`\nFetching ${uniqueVaults.length} vault accounts for token_vault + lp_mint...`);
  const vaultResp = await rpc("getMultipleAccounts", [uniqueVaults, { encoding: "base64" }]);
  if (vaultResp.error) throw new Error("RPC error: " + JSON.stringify(vaultResp.error));

  const vaultInfo = {}; // vaultPubkey → { tokenVault, lpMint }
  vaultResp.result.value.forEach((acc, i) => {
    const key = uniqueVaults[i];
    if (!acc) { console.error(`  SKIP vault ${key}: not found`); return; }
    const d = Buffer.from(acc.data[0], "base64");
    if (d.length < 147) { console.error(`  SKIP vault ${key}: too short`); return; }
    vaultInfo[key] = {
      tokenVault: b58enc(d.slice(19, 51)),
      lpMint:     b58enc(d.slice(115, 147)),
    };
    console.error(`  vault ${key.slice(0,8)}  tokenVault=${vaultInfo[key].tokenVault.slice(0,8)} lpMint=${vaultInfo[key].lpMint.slice(0,8)}`);
  });

  // Attach vault-derived fields to each pool's extra and strip temp keys
  for (const pool of pools) {
    const va = vaultInfo[pool._aVault];
    const vb = vaultInfo[pool._bVault];
    if (va) {
      pool.extra.a_token_vault  = va.tokenVault;
      pool.extra.a_vault_lp_mint = va.lpMint;
    }
    if (vb) {
      pool.extra.b_token_vault  = vb.tokenVault;
      pool.extra.b_vault_lp_mint = vb.lpMint;
    }
    delete pool._aVault;
    delete pool._bVault;
  }

  const outPath = path.join(__dirname, "..", "meteora_pools.json");
  fs.writeFileSync(outPath, JSON.stringify(pools, null, 2));
  console.error(`\nWrote ${pools.length} pools → meteora_pools.json`);
}

main().catch((e) => { console.error(e); process.exit(1); });
