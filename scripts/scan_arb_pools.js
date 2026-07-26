#!/usr/bin/env node
/*
 * scan_arb_pools.js — discover trending, security-screened tokens that form EXECUTABLE
 * arb cycles, fit them into the gRPC account budget, and rewrite pools.json.
 *
 * Pipeline: discover (Birdeye + scan_tokens filters) → arb safety gate (freeze authority,
 * transfer hook) → resolve venues (DexScreener) → cycle-closure prune (non-hub token needs
 * ≥2 TRADEABLE venues, hub-connected) → budget prune (activity-ranked, protected core,
 * hysteresis) → decode via the per-DEX fetchers → atomic validated write.
 *
 * Exit: 0 = book changed & written | 10 = no change | other = failure (book untouched).
 *
 * Usage:
 *   node scripts/scan_arb_pools.js            # report only, writes nothing
 *   node scripts/scan_arb_pools.js --apply    # write pools.json (backup first)
 *
 * Design: docs/superpowers/specs/2026-07-25-dynamic-arb-pool-discovery-design.md
 */
"use strict";
require("./lib/load_env"); // auto-load repo-root .env (zero-dep dotenv equivalent) before any process.env read
const fs = require("fs");
const path = require("path");
const { execFileSync } = require("child_process");

const { pruneToCycles, countAccounts, HUBS, MAJORS, USDC } = require("./reduce_pools");
const { fetchMintSafety } = require("./lib/token_safety");
const { bestPoolPerVenue, tradeableVenueCount } = require("./lib/venues");
const { isProtected, selectBook } = require("./lib/book_budget");

const POOLS_PATH = path.join(__dirname, "..", "pools.json");
const TOKENS_PATH = process.env.MOMENTUM_TOKENS_PATH ||
  path.join(__dirname, "..", "assets", "momentum_tokens.json");

// LSTs: SOL↔LST rate is the staking rate — no arb edge across venues (mirrors the
// DENY_MINTS set in reduce_pools.js). Kept local so the scanner is self-contained.
const LST_DENY = new Set([
  "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn", // jitoSOL
  "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So",  // mSOL
  "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1",  // bSOL
  "jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v",  // JupSOL
  "BonK1YhkXEGLZzwtcvRTip3gAL9nCeQD7ppZBLXhtTs",  // bonkSOL
]);
const APPLY = process.argv.includes("--apply");
const num = (k, d) => Number(process.env[k] || d);
const CFG = {
  budget: num("ARB_ACCOUNT_BUDGET", 200),
  evictMargin: num("ARB_SCAN_EVICT_MARGIN", 1.25),
  activityWindow: num("ARB_ACTIVITY_WINDOW_SECS", 300),
  pumpTradeable: String(process.env.ENABLE_PUMPSWAP_TRADING || "false") === "true",
  volatilityWeight: num("ARB_VOLATILITY_WEIGHT", 1.0),
  // Activity multiplier for the USDC venues of a token that has ≥2 of them. Such a token
  // forms a 2-hop USDC→X→USDC cycle — the ONLY shape the no-tip raw-RPC path can land
  // (wallet-funded, non-native base, ≤1232B, no ALT). Everything else is 3-hop-via-SOL →
  // Jito → tip-auction. Boosting these venues into the book is how the scanner "takes the
  // raw-RPC fact into account". 1 = no preference (legacy behaviour).
  rawRpcBoost: num("ARB_RAW_RPC_BOOST", 3.0),
  // Raw-RPC FOCUS mode: rebuild the book around the no-tip 2-hop edge only. Protected core
  // shrinks to momentum-watcher pools + SOL/USDC/USDT hub pricing pools; the ONLY tokens
  // admitted are freshly-discovered movers with ≥2 liquid USDC venues (raw-RPC eligible).
  // Majors, general memecoins and the broad fetcher pins are dropped. Off = the legacy
  // general-arb book (majors + any cycle-closing discovery).
  rawFocus: String(process.env.ARB_RAW_RPC_FOCUS ?? "true") === "true",
  // Per-USDC-venue liquidity floor (USD) for raw-RPC eligibility — BOTH 2-hop legs must
  // clear it, so a thin USDC pool (whose DexScreener spread is a stale-price artifact, not a
  // real edge) cannot qualify a token. 0 = no floor.
  rawMinUsdcLiq: num("ARB_RAW_MIN_USDC_LIQ", 50000),
};

/** A token is raw-RPC 2-hop eligible when it has ≥2 TRADEABLE USDC venues that each clear the
 *  liquidity floor — the USDC→X→USDC shape the no-tip raw path lands. Pure; unit-tested.
 *  opts.minUsdcLiq (default 0) filters out thin legs whose spread is a stale-price artifact. */
function rawRpcEligible(venues, opts = {}) {
  const minLiq = Number(opts.minUsdcLiq) || 0;
  const usdc = venues.filter((v) => v.quoteMint === USDC && (Number(v.liquidityUsd) || 0) >= minLiq);
  return tradeableVenueCount(usdc, opts) >= 2;
}

// Budget-ranking activity score = 24h volume scaled up by short-window volatility
// (|1h price change|). Volume is the ANCHOR: it keeps a token fresh on the gRPC feed —
// thin tokens go stale and their cycles get gated_stale — so a volatile-but-low-volume
// token cannot leapfrog a high-volume one into a subscription slot it would only go stale
// in. Volatility is a BOUNDED multiplier marking where transient cross-venue arb edges
// appear (freshly-migrated pump movers rank up here). Missing change → factor 1 (pure
// volume). ARB_VOLATILITY_WEIGHT=0 restores the legacy pure-volume ranking exactly.
function actScore(volume24h, changePct) {
  const v = Number(volume24h) || 0;
  const w = CFG.volatilityWeight;
  if (!w || !Number.isFinite(Number(changePct))) return v;
  return v * (1 + w * Math.min(Math.abs(Number(changePct)) / 100, 2)); // cap the tail at a +200% move
}

/** Required fields per dex kind — mirrors check_extra in src/dex/mod.rs. */
const REQUIRED = {
  base: ["id", "dex", "token_a", "token_b", "vault_a", "vault_b"],
  // "meteora_dlmm" (not "dlmm") — matches fetch_meteora_dlmm.js's output and every DLMM
  // entry in pools.json; the wrong string here meant this check never fired for DLMM.
  clPools: new Set(["raydium_clmm", "orca_whirlpool", "meteora_dlmm"]),
  // Per-DEX `extra` completeness — the EXACT field names check_extra() in
  // src/dex/mod.rs requires for each DexKind this scanner can decode (Raydium AMM
  // v4/CLMM, Orca Whirlpool, Meteora DAMM/DLMM). Dex kinds this scanner never decodes
  // (phoenix, lifinity, invariant, saber, jupiter) are intentionally not listed here;
  // pump_swap keeps its own conditional (tradeable-only) check below instead of living
  // in this table, since it's the one dex whose requirement depends on runtime config.
  extra: {
    orca_whirlpool: ["tick_array_0", "tick_array_1", "tick_array_2", "oracle"],
    raydium_amm_v4: [
      "amm_authority", "open_orders", "target_orders", "market_program", "market",
      "market_bids", "market_asks", "market_event_queue", "market_coin_vault",
      "market_pc_vault", "market_vault_signer",
    ],
    raydium_clmm: ["clmm_amm_config", "clmm_tick_spacing"],
    meteora_dlmm: ["dlmm_bin_step"],
    meteora_damm: [
      "a_vault_lp", "b_vault_lp", "a_token_vault", "b_token_vault",
      "a_vault_lp_mint", "b_vault_lp_mint", "admin_token_fee_a", "admin_token_fee_b",
    ],
  },
};

// A field counts as present only if it has a real value — `0` is legitimate for a
// numeric extra field (e.g. clmm_tick_spacing), so this is deliberately not just `!v`.
const missingField = (v) => v === undefined || v === null || v === "";

function validateBook(pools, opts = {}) {
  const pumpTradeable = opts.pumpTradeable === true;
  const errors = [];
  if (!Array.isArray(pools) || pools.length === 0) {
    return { ok: false, errors: ["book is empty — refusing to write"] };
  }
  const seen = new Set();
  for (const p of pools) {
    const tag = (p && p.id) || "<no id>";
    for (const f of REQUIRED.base) if (missingField(p[f])) errors.push(`${tag}: missing ${f}`);
    if (REQUIRED.clPools.has(p.dex) && !p.state_account) errors.push(`${tag}: missing state_account (CL pool)`);
    const reqExtra = REQUIRED.extra[p.dex];
    if (reqExtra) {
      const ex = p.extra || {};
      for (const f of reqExtra) if (missingField(ex[f])) errors.push(`${tag}: missing extra.${f} (${p.dex})`);
    }
    // Only a TRADEABLE pump pool needs coin_creator (required by the swap builder); a
    // pricing-only pump pool (ENABLE_PUMPSWAP_TRADING=false, e.g. the momentum watcher's
    // pinned feed) legitimately ships with extra:{} — mirrors check_extra in
    // src/dex/mod.rs, which only enforces this when the pool is actually loaded to trade.
    if (p.dex === "pump_swap" && pumpTradeable && !(p.extra && p.extra.pumpswap_coin_creator)) {
      errors.push(`${tag}: missing extra.pumpswap_coin_creator (tradeable pump_swap)`);
    }
    if (seen.has(p.id)) errors.push(`${tag}: duplicate pool id`);
    seen.add(p.id);
  }
  return { ok: errors.length === 0, errors };
}

// Recursively rebuild an object with its keys sorted at EVERY nesting depth — unlike
// JSON.stringify(v, Object.keys(v).sort()), which applies that top-level key whitelist
// recursively and so silently collapses nested objects with different key sets (e.g. a
// pool's `extra`) down to `{}`, dropping their content from the comparison entirely.
function sortKeysDeep(v) {
  if (Array.isArray(v)) return v.map(sortKeysDeep);
  if (v && typeof v === "object") {
    const out = {};
    for (const k of Object.keys(v).sort()) out[k] = sortKeysDeep(v[k]);
    return out;
  }
  return v;
}

function bookChanged(oldPools, newPools) {
  const canon = (pools) =>
    JSON.stringify([...pools].map(sortKeysDeep).sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0)));
  return canon(oldPools) !== canon(newPools);
}

// Arb-scoped discovery-breadth overrides. ARB_SCAN_* vars are remapped onto the
// scan_tokens child's env for THE ARB SCANNER ONLY — the momentum watcher's hourly
// scan runs the same scan_tokens.js with the shared SCAN_*/MOMENTUM_SCAN_* settings
// and must not inherit arb-side widening (same scoping idea as the
// SCAN_MAX_TOP_HOLDERS_PCT override in main below). Unset/empty = no override.
const ARB_SCAN_MAP = {
  ARB_SCAN_SOURCE: "MOMENTUM_SCAN_SOURCE",
  ARB_SCAN_MIN_VOLUME: "SCAN_MIN_VOLUME",
  ARB_SCAN_MIN_LIQUIDITY: "SCAN_MIN_LIQUIDITY",
  ARB_SCAN_VERIFY_MAX: "SCAN_VERIFY_MAX",
  ARB_SCAN_MAX_PAGES: "SCAN_MAX_PAGES",
  ARB_SCAN_TRENDING_LIMIT: "MOMENTUM_SCAN_TRENDING_LIMIT",
};
function arbScanEnvOverrides(env) {
  const out = {};
  for (const [src, dst] of Object.entries(ARB_SCAN_MAP)) {
    if (env[src] != null && env[src] !== "") out[dst] = env[src];
  }
  return out;
}

module.exports = { validateBook, bookChanged, actScore, rawRpcEligible, arbScanEnvOverrides };

// ─── Pipeline (only when run directly) ───────────────────────────────────────
if (require.main === module) {
  main().catch((e) => { console.error("scan failed:", e.message); process.exit(1); });
}

async function main() {
  const rpcUrl = process.env.RPC_URL;
  if (!rpcUrl) throw new Error("RPC_URL is required");
  const current = JSON.parse(fs.readFileSync(POOLS_PATH, "utf8"));

  // 1. Discover candidates via the existing momentum scanner (--json prints survivors).
  // Disable the holder-concentration cap for the ARB path: it is a MOMENTUM guard (a whale
  // exit gaps a trailing stop), and atomic arb never holds the token across a price move —
  // it's in and out in one transaction. The arb-specific token risks (freeze authority, a
  // transfer hook that strands capital BETWEEN legs) are enforced separately by the
  // token_safety gate in step 2. The momentum trader keeps the cap via its own global
  // SCAN_MAX_TOP_HOLDERS_PCT; this override is scoped to the arb scanner's child only.
  const discovered = JSON.parse(
    execFileSync(process.execPath, [path.join(__dirname, "scan_tokens.js"), "--json"], {
      encoding: "utf8",
      env: { ...process.env, SCAN_MAX_TOP_HOLDERS_PCT: "0", ...arbScanEnvOverrides(process.env) },
    }) || "[]",
  );
  console.log(`discovered ${discovered.length} candidate token(s) from scan_tokens`);

  // 2. Arb safety gate.
  const safety = await fetchMintSafety(rpcUrl, discovered.map((t) => t.mint));
  const safe = discovered.filter((t) => {
    const s = safety.get(t.mint);
    if (!s || !s.safe) console.log(`  reject ${t.symbol}: ${(s && s.reasons.join("; ")) || "unknown"}`);
    return s && s.safe;
  });

  // 3. Resolve venues per survivor (DexScreener), keep tokens with ≥2 TRADEABLE venues.
  //    LSTs are excluded: their SOL↔LST rate IS the staking exchange rate, so a
  //    cross-venue cycle through them cannot clear multi-hop fees (dead weight).
  const candidatePools = [];
  for (const t of safe) {
    if (LST_DENY.has(t.mint)) { console.log(`  skip ${t.symbol}: LST (SOL-pegged, no arb edge)`); continue; }
    const pairs = await dexscreenerPairs(t.mint);
    const venues = bestPoolPerVenue(pairs, { quoteAllowlist: HUBS });
    if (tradeableVenueCount(venues, { pumpTradeable: CFG.pumpTradeable }) < 2) {
      console.log(`  skip ${t.symbol}: <2 tradeable venues (no cycle)`);
      continue;
    }
    // Raw-RPC 2-hop eligibility: recover the USDC pools bestPoolPerVenue drops. It keeps one
    // pool per DEX by 24h volume — usually the SOL one — so a SOL-dominant token's USDC pools
    // vanish (ANSEM: meteora/SOL wins over meteora/USDC). Re-resolve with a USDC-only allowlist
    // to get the best USDC pool per DEX back. A token with ≥2 tradeable USDC venues forms
    // USDC→X→USDC, the only cycle the no-tip raw path lands; boost those venues so BOTH win
    // budget slots and the 2-hop materialises (a single USDC venue yields only a 3-hop cycle).
    const usdcVenues = bestPoolPerVenue(pairs, { quoteAllowlist: new Set([USDC]) });
    const rawEligible = rawRpcEligible(usdcVenues, { pumpTradeable: CFG.pumpTradeable, minUsdcLiq: CFG.rawMinUsdcLiq });
    // Focus mode: only raw-RPC 2-hop-eligible movers are admitted; everything else is dropped.
    if (CFG.rawFocus && !rawEligible) {
      console.log(`  skip ${t.symbol}: not raw-RPC eligible (need ≥2 USDC venues ≥ $${CFG.rawMinUsdcLiq}) — focus mode`);
      continue;
    }
    const seen = new Set(venues.map((v) => v.pairAddress));
    // Focus mode books ONLY the USDC legs (the 2-hop cycle); a raw-eligible token's SOL
    // venues would only enable a tip-paying 3-hop and dilute the focus. Legacy mode keeps
    // both, recovering the USDC venues that bestPoolPerVenue's per-dex volume rule dropped.
    const merged = CFG.rawFocus
      ? usdcVenues
      : venues.concat(usdcVenues.filter((v) => !seen.has(v.pairAddress)));
    if (rawEligible) console.log(`  raw-RPC eligible: ${t.symbol} (${usdcVenues.length} USDC venues) — 2-hop, boosting USDC venues ×${CFG.rawRpcBoost}`);
    for (const v of merged) {
      const rawBoost = rawEligible && v.quoteMint === USDC ? CFG.rawRpcBoost : 1;
      candidatePools.push({ token: t, venue: v, rawBoost });
    }
  }

  // 4. Decode each candidate address into a PoolConfig via its fetcher (Task 5). A
  // successfully-decoded config can still be per-DEX incomplete (e.g. the upstream API
  // omits a field for one specific pool — observed live: a Raydium CLMM pool whose
  // `key/ids` response had no `tickSpacing`) — check it against the same completeness
  // rules validateBook enforces on the final book, and skip it here, exactly like a
  // decode failure, rather than let one bad candidate abort the whole scan at the final
  // validateBook gate (the design doc's own contract: "skipped with a logged reason,
  // never force-merged").
  const decoded = [];
  for (const c of candidatePools) {
    const cfg = decodeViaFetcher(c.venue);
    if (!cfg) { console.log(`  skip ${c.venue.pairAddress.slice(0, 8)}: decode failed`); continue; }
    const cv = validateBook([cfg], { pumpTradeable: CFG.pumpTradeable });
    if (!cv.ok) { console.log(`  skip ${cfg.id.slice(0, 8)}: ${cv.errors.join("; ")}`); continue; }
    decoded.push({ ...cfg, _act: actScore(c.venue.volume24h, c.venue.priceChangeH1) * (c.rawBoost || 1) });
  }

  // 5. Cycle-closure + budget. candidates = current NON-CORE pools that STILL close a
  // cycle, UNIONED with this scan's new discoveries — not just this scan's discoveries
  // alone. Otherwise every incumbent that isn't re-discovered on this exact scan is
  // silently dropped, and selectBook's eviction-margin hysteresis (which exists
  // precisely to defend incumbents against churn) never gets a chance to fire because
  // incumbents are never candidates in the first place.
  const pinnedIds = collectPinnedIds();
  const momentumPoolIds = collectMomentumPoolIds();
  const floorMints = collectRawFloorMints();
  const ctx = { pinnedIds, momentumPoolIds, hubs: HUBS, majors: MAJORS, rawFocus: CFG.rawFocus, floorMints, usdc: USDC };
  if (CFG.rawFocus) {
    console.log(`raw-RPC FOCUS mode: core = momentum + hub↔hub + ${floorMints.size} floor token(s); admitting raw-eligible movers, dropping majors/general/pins`);
  }
  const withAct = current.map((p) => ({ ...p, _act: p._act || 0 }));
  const core = withAct.filter((p) => isProtected(p, ctx));

  // Incumbents must clear the SAME cycle-closure bar as discoveries — a now-dead-end
  // incumbent (its only other venue vanished since the last scan) is not force-kept
  // just for being current.
  const currentNonCore = withAct.filter((p) => !core.some((c) => c.id === p.id));
  // Focus mode carries NO general incumbents forward — the book is core (momentum + hubs)
  // plus this scan's fresh raw-eligible discoveries only, so majors/general/old pins fall
  // away. Legacy mode unions surviving incumbents so hysteresis can defend them against churn.
  const carried = CFG.rawFocus ? [] : currentNonCore;
  const closed = pruneToCycles(core.concat(carried).concat(decoded));
  const closedIds = new Set(closed.map((p) => p.id));

  // candidates = carried incumbents UNION new discoveries (minus core). Dedup by id —
  // a still-trending token's pool can appear in both carried and decoded; keep the decoded
  // copy since it carries freshly re-decoded on-chain fields.
  const seen = new Set();
  const candidates = [...decoded, ...carried].filter((p) =>
    closedIds.has(p.id) && !core.some((c) => c.id === p.id) && !seen.has(p.id) && seen.add(p.id),
  );

  // Score surviving incumbents with fresh 24h volume + 1h volatility (same actScore as
  // discoveries) so selectBook's hysteresis ranks them fairly against discoveries.
  // pools.json never persists `_act` (stripped before the write below), so every incumbent
  // starts at the `withAct` placeholder of 0 — left uncorrected, hysteresis would look like
  // it always favors new discoveries regardless of true incumbent activity. Decoded
  // (newly-discovered) pools already carry a real `_act` from step 4 and are left untouched.
  const decodedIds = new Set(decoded.map((p) => p.id));
  const incumbentActivity = await dexscreenerActivity(
    candidates.filter((p) => !decodedIds.has(p.id)).map((p) => p.id),
  );
  const scoredCandidates = candidates.map((p) => {
    if (decodedIds.has(p.id)) return p; // discovery already carries a volatility-scored _act
    const a = incumbentActivity.get(p.id);
    return { ...p, _act: a ? actScore(a.volume, a.change) : 0 };
  });

  const sel = selectBook({
    core,
    candidates: scoredCandidates,
    incumbentIds: new Set(current.map((p) => p.id)),
    budget: CFG.budget,
    evictMargin: CFG.evictMargin,
    countPumpSwap: CFG.pumpTradeable,
  });

  const next = sel.kept.map(({ _act, ...rest }) => rest);
  const v = validateBook(next, { pumpTradeable: CFG.pumpTradeable });
  if (!v.ok) throw new Error(`validation failed:\n  ${v.errors.join("\n  ")}`);

  console.log(`\nbook: ${next.length} pools / ${sel.accounts} accounts (budget ${CFG.budget})`);
  for (const s of sel.skipped) console.log(`  skipped ${s.pool.id.slice(0, 8)}: ${s.reason}`);

  // Report drops explicitly — an operator watching the log should see incumbent
  // removals, not just an aggregate pool count that could be hiding a book collapse.
  const nextIds = new Set(next.map((p) => p.id));
  const dropped = current.filter((p) => !nextIds.has(p.id));
  console.log(`dropped ${dropped.length} incumbent(s): ${dropped.map((p) => p.id.slice(0, 8)).join(", ")}`);

  // Focus-mode safety: never collapse the book to core-only. If this scan surfaced no
  // raw-eligible targets (discovery rate-limited, or nothing qualified), writing would strip
  // every arb target — including working ones — and leave the bot idle. Keep the current book;
  // the next scan replaces targets once discovery recovers. (An operator who genuinely wants an
  // empty arb book can turn focus mode off and curate manually.)
  if (CFG.rawFocus) {
    // A "raw target" is any book pool that is neither a momentum pricing pool nor a hub↔hub
    // pool — i.e. a floor token's USDC leg or a freshly-admitted discovery. The guard refuses
    // to write a book with ZERO of them (that would strip every arb target and idle the bot);
    // floor tokens (assets/arb_raw_floor.json) keep the book non-empty across a rate-limited
    // 0-discovery scan.
    const isHubHub = (p) => HUBS.has(p.token_a) && HUBS.has(p.token_b);
    const targets = next.filter((p) => !momentumPoolIds.has(p.id) && !isHubHub(p));
    if (targets.length === 0) {
      console.log("raw-RPC focus: 0 raw targets this scan (no floor, no discovery) — leaving the current book unchanged (refusing to collapse)");
      process.exit(10);
    }
    const coreIds = new Set(core.map((p) => p.id));
    const admitted = next.filter((p) => !coreIds.has(p.id)).length;
    console.log(`raw-RPC focus: ${targets.length} raw target pool(s) in book (${admitted} freshly admitted, ${targets.length - admitted} floor)`);
  }

  if (!bookChanged(current, next)) { console.log("no change"); process.exit(10); }
  if (!APPLY) { console.log("report only — re-run with --apply to write"); process.exit(0); }

  fs.copyFileSync(POOLS_PATH, POOLS_PATH + ".bak");
  const tmp = POOLS_PATH + ".tmp";
  fs.writeFileSync(tmp, JSON.stringify(next, null, 2) + "\n");
  fs.renameSync(tmp, POOLS_PATH);      // atomic
  console.log(`wrote pools.json (backup at pools.json.bak)`);

  // Accumulate discovered tokens' symbols into a { mint: symbol } map the bot loads as a
  // display fallback (src/dex/types.rs mint_symbol) so discoveries log by name, not a mint
  // prefix. Merge (never shrink) so a token keeps its name across scans even once evicted.
  const SYMBOLS_PATH = path.join(__dirname, "..", "assets", "token_symbols.json");
  let symbols = {};
  try { symbols = JSON.parse(fs.readFileSync(SYMBOLS_PATH, "utf8")); } catch { /* first run — file absent */ }
  let addedSyms = 0;
  for (const t of discovered) {
    if (t.mint && t.symbol && symbols[t.mint] !== t.symbol) { symbols[t.mint] = t.symbol; addedSyms++; }
  }
  if (addedSyms) {
    fs.writeFileSync(SYMBOLS_PATH, JSON.stringify(symbols, null, 2) + "\n");
    console.log(`updated token_symbols.json (+${addedSyms}, ${Object.keys(symbols).length} total)`);
  }
  process.exit(0);
}

/** DexScreener pairs for one mint. */
function dexscreenerPairs(mint) {
  return httpJson(`https://api.dexscreener.com/latest/dex/tokens/${mint}`).then((d) => (d && d.pairs) || []);
}

/**
 * DexScreener 24h volume for a batch of pool/pair addresses, keyed by pairAddress. The
 * pairs endpoint accepts up to 30 comma-separated addresses per call, so chunk larger
 * incumbent sets. A failed chunk is logged and simply yields no entries for those
 * addresses — callers default missing lookups to 0 (low-priority, not dropped).
 */
async function dexscreenerActivity(addresses) {
  const act = new Map();
  for (let i = 0; i < addresses.length; i += 30) {
    const chunk = addresses.slice(i, i + 30);
    try {
      const d = await httpJson(`https://api.dexscreener.com/latest/dex/pairs/solana/${chunk.join(",")}`);
      for (const p of (d && d.pairs) || []) act.set(p.pairAddress, {
        volume: (p.volume && p.volume.h24) || 0,
        change: (p.priceChange && p.priceChange.h1) || 0,
      });
    } catch (e) {
      console.log(`  incumbent activity batch fetch failed (${chunk.length} pool(s)): ${e.message}`);
    }
  }
  return act;
}

/** Run the venue's fetcher with --pools <addr> and return the single decoded PoolConfig. */
function decodeViaFetcher(venue) {
  const script = {
    raydium: "fetch_raydium_pools.js", orca: "fetch_orca_pools.js",
    meteora: "fetch_meteora_dlmm.js", pumpswap: "fetch_pumpswap_pools.js",
  }[venue.dexId];
  if (!script) return null;
  const out = path.join(require("os").tmpdir(), `arbscan_${venue.pairAddress}.json`);
  try {
    execFileSync(process.execPath, [path.join(__dirname, script), "--pools", venue.pairAddress, "--output", out],
      { encoding: "utf8", env: process.env, stdio: "pipe" });
    const arr = JSON.parse(fs.readFileSync(out, "utf8"));
    return Array.isArray(arr) && arr.length ? arr[0] : null;
  } catch { return null; }
}

/** Pool addresses hard-pinned inside the fetchers (never evict these). */
function collectPinnedIds() {
  const ids = new Set();
  for (const f of ["fetch_pumpswap_pools.js", "fetch_meteora_dlmm.js", "fetch_orca_pools.js", "fetch_raydium_pools.js"]) {
    const src = fs.readFileSync(path.join(__dirname, f), "utf8");
    for (const m of src.matchAll(/"([1-9A-HJ-NP-Za-km-z]{32,44})"/g)) ids.add(m[1]);
  }
  return ids;
}

/** Pools the momentum watcher prices from — must survive every scan. */
function collectMomentumPoolIds() {
  try {
    return new Set(JSON.parse(fs.readFileSync(TOKENS_PATH, "utf8")).map((t) => t.pool).filter(Boolean));
  } catch { return new Set(); }
}

/** Designated raw-RPC FLOOR token mints (assets/arb_raw_floor.json) — their USDC legs survive
 *  every focus scan even when the token isn't a mover (a proven raw target discovery wouldn't
 *  re-surface). Entries are `{ mint }` objects or bare mint strings. Absent file → empty set. */
function collectRawFloorMints() {
  try {
    const raw = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "assets", "arb_raw_floor.json"), "utf8"));
    return new Set(raw.map((t) => (typeof t === "string" ? t : t && t.mint)).filter(Boolean));
  } catch { return new Set(); }
}

function httpJson(url) {
  const https = require("https");
  return new Promise((resolve, reject) => {
    https.get(url, (res) => {
      let b = ""; res.on("data", (c) => (b += c));
      res.on("end", () => { try { resolve(JSON.parse(b)); } catch (e) { reject(e); } });
    }).on("error", reject);
  });
}
