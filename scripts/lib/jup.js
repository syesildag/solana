"use strict";
// Shared Jupiter token-search helpers. Single source of truth for the verified-token
// gate used by add_momentum_token.js (manual) and scan_tokens.js (live discovery).

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

// Is `mint` a Jupiter-verified token? Searches by mint and matches the exact id.
// Fail-closed: any network/parse error returns false (an unverified token is skipped).
async function isVerifiedMint(mint) {
  try {
    const hit = (await search(mint)).find((t) => t.id === mint);
    return !!(hit && hit.isVerified);
  } catch (_) {
    return false;
  }
}

module.exports = { USDC_MINT, JUP_HOST, MINT_RE, search, isVerifiedMint };
