"use strict";
/*
 * load_env.js — zero-dependency .env loader (stand-in for `require('dotenv').config()`).
 *
 * The repo's scripts deliberately avoid npm dependencies (see the hand-rolled bs58 in
 * fetch_pumpswap_pools.js), so rather than pull the `dotenv` package we replicate its
 * one behaviour we need: read the repo-root .env and populate process.env for any key
 * that is not already set. The real environment wins over the file — identical to dotenv
 * and to the Rust bot's dotenvy — so `RPC_URL=… node script.js` still overrides.
 *
 * Requiring this module runs it once as a side effect (like `dotenv.config()`); the
 * loader is also exported for explicit/tested use.
 */
const fs = require("fs");
const path = require("path");

// scripts/lib/load_env.js → ../../ = repo root
const ENV_PATH = path.join(__dirname, "..", "..", ".env");

function loadEnv(file = ENV_PATH) {
  let text;
  try {
    text = fs.readFileSync(file, "utf8");
  } catch {
    return; // no .env present — nothing to load, not an error
  }
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq === -1) continue;
    let key = line.slice(0, eq).trim();
    if (key.startsWith("export ")) key = key.slice(7).trim(); // tolerate `export KEY=…`
    if (!key || key in process.env) continue; // existing environment wins (dotenv semantics)
    let val = line.slice(eq + 1).trim();
    // Strip a single pair of matching surrounding quotes; leave the value otherwise
    // intact (do NOT strip inline '#' — secrets like SMTP passwords may contain it).
    const q = val[0];
    if (val.length >= 2 && (q === '"' || q === "'") && val[val.length - 1] === q) {
      val = val.slice(1, -1);
    }
    process.env[key] = val;
  }
}

loadEnv();                 // run on require, like dotenv.config()
module.exports = loadEnv;  // also callable explicitly (and unit-testable)
