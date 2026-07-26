#!/usr/bin/env node
/*
 * backfill_token_programs.js — stamp extra.token_program_a/b for Token-2022 mints in pools.json.
 *
 * The wallet-funded raw-RPC path derives AND creates each ATA under the mint's OWN token program
 * (Pool::token_program_for → extra.token_program_a/b, default classic spl_token). Graduated
 * pump.fun tokens (ANSEM, PUMP, …) are Token-2022; with the field unset the raw tx reverts
 * `IncorrectProgramId` at the intermediate ATA CreateIdempotent. The DLMM/Orca/Raydium fetchers
 * don't emit it (only fetch_pumpswap_pools.js does), so this pass backfills it by reading each
 * mint's on-chain owner. Classic mints are left unset (Rust defaults to classic).
 *
 * Usage:
 *   node scripts/backfill_token_programs.js            # report only, writes nothing
 *   node scripts/backfill_token_programs.js --apply    # write pools.json
 */
"use strict";
require("./lib/load_env");
const fs = require("fs");
const path = require("path");

const TOKEN_2022 = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const POOLS_PATH = path.join(__dirname, "..", "pools.json");

/** Stamp extra.token_program_a/b for any pool side whose mint owner is Token-2022. Classic
 *  mints are left unset (the Rust loader defaults to classic). Pure given `owners` (mint→owner);
 *  returns the number of pool sides changed. */
function applyTokenPrograms(pools, owners) {
  let changed = 0;
  for (const p of pools) {
    const ex = p.extra || (p.extra = {});
    for (const [mint, field] of [[p.token_a, "token_program_a"], [p.token_b, "token_program_b"]]) {
      if (owners.get(mint) === TOKEN_2022 && ex[field] !== TOKEN_2022) { ex[field] = TOKEN_2022; changed++; }
    }
  }
  return changed;
}

/** mint → owning program, via batched getMultipleAccounts (100/call). */
async function fetchMintOwners(mints, rpcUrl) {
  const owners = new Map();
  for (let i = 0; i < mints.length; i += 100) {
    const chunk = mints.slice(i, i + 100);
    const res = await rpc(rpcUrl, "getMultipleAccounts", [chunk, { encoding: "base64" }]);
    (res.value || []).forEach((v, j) => { if (v && v.owner) owners.set(chunk[j], v.owner); });
  }
  return owners;
}

module.exports = { applyTokenPrograms, fetchMintOwners, TOKEN_2022 };

if (require.main === module) {
  main().catch((e) => { console.error("backfill failed:", e.message); process.exit(1); });
}

async function main() {
  const rpcUrl = process.env.RPC_URL;
  if (!rpcUrl) throw new Error("RPC_URL is required");
  const pools = JSON.parse(fs.readFileSync(POOLS_PATH, "utf8"));
  const mints = [...new Set(pools.flatMap((p) => [p.token_a, p.token_b]).filter(Boolean))];
  const owners = await fetchMintOwners(mints, rpcUrl);
  const t22 = [...owners].filter(([, o]) => o === TOKEN_2022).map(([m]) => m);
  console.log(`token-2022 mints in book: ${t22.length}${t22.length ? " (" + t22.map((m) => m.slice(0, 8)).join(", ") + ")" : ""}`);

  const changed = applyTokenPrograms(pools, owners);
  console.log(`token_program stamped on ${changed} pool side(s)`);
  if (!changed) { console.log("no change"); process.exit(0); }
  if (!process.argv.includes("--apply")) { console.log("report only — re-run with --apply to write"); process.exit(0); }

  fs.copyFileSync(POOLS_PATH, POOLS_PATH + ".bak");
  const tmp = POOLS_PATH + ".tmp";
  fs.writeFileSync(tmp, JSON.stringify(pools, null, 2) + "\n");
  fs.renameSync(tmp, POOLS_PATH);
  console.log("wrote pools.json (backup at pools.json.bak)");
}

function rpc(url, method, params) {
  const https = require("https");
  const u = new URL(url);
  const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
  return new Promise((resolve, reject) => {
    const req = https.request({
      hostname: u.hostname, port: u.port || 443, path: u.pathname + u.search, method: "POST",
      headers: { "content-type": "application/json", "content-length": Buffer.byteLength(body) },
    }, (res) => {
      let b = ""; res.on("data", (c) => (b += c));
      res.on("end", () => { try { const j = JSON.parse(b); j.error ? reject(new Error(j.error.message)) : resolve(j.result); } catch (e) { reject(e); } });
    });
    req.on("error", reject); req.write(body); req.end();
  });
}
