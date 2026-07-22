"use strict";
// Shared Jupiter token-search helpers. Single source of truth for the verified-token
// gate used by add_momentum_token.js (manual) and scan_tokens.js (live discovery).

// Best-effort .env loader (repo root) so a manual `node scripts/...` run sees the same
// vars the Rust bins get via dotenvy (BIRDEYE_API_KEY, SCAN_*, MOMENTUM_*). Runs on
// require, before the consts below read process.env. Never overrides an already-set var
// (shell/inline wins), and silently no-ops if .env is missing or unreadable.
(function loadDotEnv() {
  try {
    const fs = require("fs");
    const path = require("path");
    const envPath = path.join(__dirname, "..", "..", ".env"); // scripts/lib/ → repo root
    if (!fs.existsSync(envPath)) return;
    for (const line of fs.readFileSync(envPath, "utf8").split("\n")) {
      const m = line.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*)$/);
      if (!m) continue; // skips blanks + comments (# ...)
      const key = m[1];
      if (process.env[key] !== undefined) continue;
      let val = m[2].trim();
      if ((val.startsWith('"') && val.endsWith('"')) || (val.startsWith("'") && val.endsWith("'"))) {
        val = val.slice(1, -1);
      }
      process.env[key] = val;
    }
  } catch (_) {
    /* best-effort */
  }
})();

const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

// The token search lives on the API host root, not under /swap/v1.
const JUP_HOST = (process.env.MOMENTUM_JUPITER_API_URL || "https://lite-api.jup.ag")
  .replace(/\/swap\/v1\/?$/, "")
  .replace(/\/$/, "");

// Solana addresses are 32–44 base58 chars (no 0, O, I, l).
const MINT_RE = /^[1-9A-HJ-NP-Za-km-z]{32,44}$/;

async function search(query) {
  const url = `${JUP_HOST}/tokens/v2/search?query=${encodeURIComponent(query)}`;
  const res = await fetch(url, { headers: { accept: "application/json" } });
  if (!res.ok) throw new Error(`Jupiter token search -> HTTP ${res.status} (${url})`);
  const body = await res.json();
  return Array.isArray(body) ? body : body.tokens || [];
}

// Full Jupiter token record for a verified mint (includes the `audit` block —
// topHoldersPercentage, mint/freeze authority flags — used by the scanner's
// holder-concentration gate). Returns null for unverified mints.
// Fail-closed: any network/parse error returns null (an unverified token is skipped).
async function getVerifiedToken(mint) {
  try {
    const hit = (await search(mint)).find((t) => t.id === mint);
    return hit && hit.isVerified ? hit : null;
  } catch (_) {
    return null;
  }
}

// Is `mint` a Jupiter-verified token? Searches by mint and matches the exact id.
async function isVerifiedMint(mint) {
  return !!(await getVerifiedToken(mint));
}

module.exports = { USDC_MINT, JUP_HOST, MINT_RE, search, isVerifiedMint, getVerifiedToken };
