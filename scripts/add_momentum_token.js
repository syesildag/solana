#!/usr/bin/env node
/**
 * Add a token to the momentum trader's watch list (assets/momentum_tokens.json).
 *
 * Resolves a ticker, a name, or a raw mint address to a { symbol, name, mint }
 * entry via Jupiter's token search (the same venue the bot trades on), dedups by
 * mint, and appends it.
 *
 * Safety: for a ticker/name it only accepts a Jupiter-**verified** token (the top
 * verified match, preferring an exact symbol hit). This avoids name-copying scam
 * tokens. If nothing verified matches it refuses and prints candidates — pass the
 * mint directly, or --force to accept the top result anyway.
 *
 * Usage:
 *   node scripts/add_momentum_token.js <ticker | name | mint> [SYMBOL_OVERRIDE] [--force]
 *                                      [--pool <pool_id> --quote <USDC|SOL>]
 *
 * --pool + --quote (given together) wire the token for the opt-in gRPC price feed: the pool
 * must also exist in pools.json (raydium_amm_v4 supported so far) or it falls back to REST.
 * Passing --pool for a token already in the list updates its pool in place.
 *
 * Examples:
 *   node scripts/add_momentum_token.js MET                 # ticker -> Meteora
 *   node scripts/add_momentum_token.js "Jito Staked SOL"   # name   -> JitoSOL
 *   node scripts/add_momentum_token.js JUP
 *   node scripts/add_momentum_token.js Xs8S1uUs1zvS2p7iwtsG3b6fkhpvmwz4GYU3gWAmWHZ   # mint
 *   node scripts/add_momentum_token.js So111...112 WSOL    # mint + force the label
 *
 * Notes:
 *   - The watch list is read once at portfolio-watcher startup → restart it to
 *     pick up the new token.
 *   - Honors MOMENTUM_TOKENS_PATH (default assets/momentum_tokens.json) and
 *     MOMENTUM_JUPITER_API_URL (default https://lite-api.jup.ag).
 */
"use strict";
const fs = require("fs");
const path = require("path");

const { USDC_MINT, MINT_RE, search } = require("./lib/jup");

const TOKENS_PATH =
  process.env.MOMENTUM_TOKENS_PATH ||
  path.join(__dirname, "..", "assets", "momentum_tokens.json");

const fmt = (t) => `${t.symbol} — ${t.name} [${t.isVerified ? "verified" : "UNVERIFIED"}] ${t.id}`;

// Resolve a ticker/name to a single token, gating on verification.
async function resolveQuery(query, force) {
  const results = await search(query);
  if (!results.length) throw new Error(`no Jupiter token matches "${query}"`);

  const q = query.toUpperCase();
  const verified = results.filter((t) => t.isVerified);
  // Require an EXACT verified symbol or name match — a fuzzy "top verified
  // result" can be an unrelated (but verified) memecoin, e.g. "GROK" -> "Groks Dog".
  const exactSym = verified.filter((t) => (t.symbol || "").toUpperCase() === q);
  const exactName = verified.filter((t) => (t.name || "").toUpperCase() === q);
  const pick = exactSym[0] || exactName[0] || (force ? verified[0] || results[0] : null);

  if (!pick) {
    const lines = results.slice(0, 6).map((t) => "    " + fmt(t)).join("\n");
    throw new Error(
      `no exact verified symbol/name match for "${query}" — refusing (avoids look-alike scams).\n` +
        `  Candidates:\n${lines}\n` +
        `  Re-run with the exact mint address, or append --force to accept the top verified result.`
    );
  }
  return { symbol: pick.symbol, name: pick.name, mint: pick.id, verified: !!pick.isVerified };
}

// Resolve a raw mint: enrich symbol/name from Jupiter if known; otherwise accept
// the user-supplied mint as-is (an explicit mint is trusted input).
async function resolveMint(mint) {
  try {
    const hit = (await search(mint)).find((t) => t.id === mint);
    if (hit) return { symbol: hit.symbol, name: hit.name, mint, verified: !!hit.isVerified };
  } catch (_) {
    /* fall through to bare mint */
  }
  return { symbol: null, name: null, mint, verified: null };
}

function loadList() {
  if (!fs.existsSync(TOKENS_PATH)) return [];
  const raw = fs.readFileSync(TOKENS_PATH, "utf8").trim();
  if (!raw) return [];
  const parsed = JSON.parse(raw);
  if (!Array.isArray(parsed)) throw new Error(`${TOKENS_PATH} is not a JSON array`);
  return parsed;
}

async function main() {
  const args = process.argv.slice(2);
  const force = args.includes("--force");
  const flagVal = (name) => {
    const i = args.indexOf(name);
    return i >= 0 ? args[i + 1] : undefined;
  };
  const pool = flagVal("--pool");
  const quoteRaw = flagVal("--quote");
  // Positionals: everything except --force and the --pool/--quote flags + their values.
  const consumed = new Set();
  for (const f of ["--pool", "--quote"]) {
    const i = args.indexOf(f);
    if (i >= 0) consumed.add(i).add(i + 1);
  }
  const positional = args.filter((a, i) => a !== "--force" && !consumed.has(i));
  const [query, symbolOverride] = positional;

  if (!query || query === "-h" || query === "--help") {
    console.error(
      "Usage: node scripts/add_momentum_token.js <ticker | name | mint> [SYMBOL_OVERRIDE] [--force] [--pool <id> --quote <USDC|SOL>]"
    );
    process.exit(query ? 0 : 1);
  }

  // Optional gRPC-pricing wiring: --pool + --quote must be given together. The pool must
  // also exist in pools.json (raydium_amm_v4 for now) for the gRPC feed to price this token
  // on-chain; otherwise it falls back to REST. See MOMENTUM_GRPC_PRICING in .env.example.
  let quote;
  if (pool || quoteRaw) {
    if (!pool || !quoteRaw) throw new Error("--pool and --quote must be given together");
    if (!MINT_RE.test(pool)) throw new Error(`--pool "${pool}" is not a valid Solana pubkey`);
    quote = quoteRaw.toUpperCase();
    if (quote !== "USDC" && quote !== "SOL") {
      throw new Error(`--quote must be USDC or SOL (got "${quoteRaw}")`);
    }
  }

  const resolved = MINT_RE.test(query) ? await resolveMint(query) : await resolveQuery(query, force);

  const mint = resolved.mint;
  if (!MINT_RE.test(mint)) throw new Error(`resolved mint "${mint}" is not a valid Solana address`);
  if (mint === USDC_MINT) throw new Error("USDC is the cash leg and is never momentum-traded");
  if (resolved.verified === false) {
    console.warn(`⚠  ${resolved.symbol || mint} is NOT Jupiter-verified — proceeding because you gave the mint explicitly.`);
  }

  const symbol = symbolOverride || resolved.symbol || `TOKEN-${mint.slice(0, 4)}`;
  const entry = { symbol, mint };
  if (resolved.name) entry.name = resolved.name; // ignored by the Rust loader, kept for humans
  if (pool) {
    entry.pool = pool;
    entry.quote = quote;
  }

  const list = loadList();
  const existing = list.find((e) => e.mint === mint);
  if (existing) {
    // Already listed: if a pool was given and differs, update it in place; else no-op.
    if (pool && (existing.pool !== pool || existing.quote !== quote)) {
      existing.pool = pool;
      existing.quote = quote;
      fs.writeFileSync(TOKENS_PATH, JSON.stringify(list, null, 2) + "\n");
      console.log(`✓ Updated ${existing.symbol} gRPC pool → ${pool} (quote ${quote}).`);
      console.log("    Restart portfolio-watcher to pick it up.");
      return;
    }
    console.log(`• Already in the watch list: ${symbol} (${mint}) — nothing to do.`);
    return;
  }

  list.push(entry);
  fs.writeFileSync(TOKENS_PATH, JSON.stringify(list, null, 2) + "\n");

  console.log(`✓ Added ${symbol}${resolved.name ? ` — ${resolved.name}` : ""}${resolved.verified ? " [verified]" : ""}`);
  console.log(`    mint: ${mint}`);
  if (pool) console.log(`    gRPC pool: ${pool} (quote ${quote})`);
  console.log(`    file: ${TOKENS_PATH}  (${list.length} tokens)`);
  console.log("    Restart portfolio-watcher to pick it up.");
}

main().catch((e) => {
  console.error(`✗ ${e.message}`);
  process.exit(1);
});
