"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { validateBook, bookChanged, actScore, rawRpcEligible, arbScanEnvOverrides, resolveRawQuote } = require("./scan_arb_pools");

// resolveRawQuote: the scanner builds the book for the BOT's base token — the raw-RPC
// 2-hop quote follows BASE_MINT exactly like src/dex/types.rs resolve_base_token
// (unset → native SOL). Unknown mints throw, mirroring the bot's startup failure.
test("resolveRawQuote follows BASE_MINT semantics (unset → SOL)", () => {
  const SOL_MINT = "So11111111111111111111111111111111111111112";
  const USDC_MINT_ = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
  assert.deepEqual(resolveRawQuote(undefined), { mint: SOL_MINT, symbol: "SOL" });
  assert.deepEqual(resolveRawQuote(""), { mint: SOL_MINT, symbol: "SOL" });
  assert.deepEqual(resolveRawQuote(SOL_MINT), { mint: SOL_MINT, symbol: "SOL" });
  assert.deepEqual(resolveRawQuote(USDC_MINT_), { mint: USDC_MINT_, symbol: "USDC" });
  assert.throws(() => resolveRawQuote("SomeUnknownMint1111111111111111111111111111"), /BASE_MINT/);
});

// mergeFloorCandidates: floor tokens are RE-ACQUIRED each scan, not merely protected —
// a proven raw target that isn't trending never re-surfaces via discovery, so one apply
// under a different quote (or a fetch hiccup) would evict it forever (ANSEM, 2026-07-26).
test("mergeFloorCandidates appends floor tokens missing from discovery, dedupes by mint", () => {
  const { mergeFloorCandidates } = require("./scan_arb_pools");
  const discovered = [{ symbol: "PUMP", mint: "MintPump" }];
  const floor = [{ symbol: "ANSEM", mint: "MintAnsem" }, { symbol: "PUMP", mint: "MintPump" }];
  const out = mergeFloorCandidates(discovered, floor);
  assert.deepEqual(out.map((t) => t.symbol), ["PUMP", "ANSEM"], "ANSEM appended, PUMP not duplicated");
  assert.equal(out[1].floor, true, "floor-sourced candidates are marked");
  assert.deepEqual(mergeFloorCandidates(discovered, []), discovered, "no floor entries → unchanged");
});

test("rawRpcEligible: quoteMint=SOL counts SOL venues, not USDC ones", () => {
  const SOL_MINT = "So11111111111111111111111111111111111111112";
  const vl = (dexId, quoteMint, liquidityUsd) => ({ dexId, quoteMint, liquidityUsd });
  const solVenues = [vl("raydium", SOL_MINT, 100000), vl("orca", SOL_MINT, 80000)];
  const usdcVenues = [vl("raydium", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 100000),
                      vl("orca", "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", 80000)];
  assert.equal(rawRpcEligible(solVenues, { pumpTradeable: false, quoteMint: SOL_MINT }), true,
    "2 SOL venues → eligible under a SOL quote");
  assert.equal(rawRpcEligible(usdcVenues, { pumpTradeable: false, quoteMint: SOL_MINT }), false,
    "USDC venues do not count under a SOL quote");
  assert.equal(rawRpcEligible(usdcVenues, { pumpTradeable: false }), true,
    "no quoteMint → USDC default (backward compatible)");
});

// Pool-level eligibility: two distinct pools on the SAME dex form a valid 2-hop —
// only same-POOL cycles are phantoms — so dexId-keyed counting under-admitted tokens
// whose USDC liquidity lives in two DLMM bin-step pools (or two whirlpools).
test("rawRpcEligible counts distinct POOLS, not distinct dexes", () => {
  const USDC_ = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
  const v = (dexId, pairAddress, liquidityUsd) => ({ dexId, pairAddress, quoteMint: USDC_, liquidityUsd });
  const sameDex = [v("meteora", "bin20", 60_000), v("meteora", "bin100", 80_000)];
  assert.equal(rawRpcEligible(sameDex, { pumpTradeable: false, minUsdcLiq: 20_000 }), true,
    "two same-dex pools → eligible");
  const samePool = [v("meteora", "bin20", 60_000), v("meteora", "bin20", 60_000)];
  assert.equal(rawRpcEligible(samePool, { pumpTradeable: false, minUsdcLiq: 20_000 }), false,
    "the same pool twice is NOT a cycle");
  const pumpLeg = [v("pumpswap", "pmp", 60_000), v("meteora", "bin20", 60_000)];
  assert.equal(rawRpcEligible(pumpLeg, { pumpTradeable: false, minUsdcLiq: 20_000 }), false,
    "pumpswap leg needs pumpTradeable");
  assert.equal(rawRpcEligible(pumpLeg, { pumpTradeable: true, minUsdcLiq: 20_000 }), true);
});

// arbScanEnvOverrides: ARB_SCAN_* env vars widen the ARB scanner's discovery child only —
// the momentum watcher's hourly scan (same scan_tokens.js, same SCAN_*/MOMENTUM_SCAN_* envs)
// must never see them.
test("arbScanEnvOverrides: maps ARB_SCAN_* onto the child scan env", () => {
  const out = arbScanEnvOverrides({
    ARB_SCAN_SOURCE: "volume",
    ARB_SCAN_MIN_VOLUME: "150000",
    ARB_SCAN_VERIFY_MAX: "50",
    ARB_SCAN_REQUIRE_JUP_VERIFY: "false",
    ARB_SCAN_RANK: "volume",
    SCAN_MIN_VOLUME: "250000", // non-ARB vars pass through untouched (not remapped)
  });
  assert.deepEqual(out, {
    MOMENTUM_SCAN_SOURCE: "volume",
    SCAN_MIN_VOLUME: "150000",
    SCAN_VERIFY_MAX: "50",
    SCAN_REQUIRE_JUP_VERIFY: "false",
    MOMENTUM_SCAN_RANK: "volume",
  });
});

test("arbScanEnvOverrides: no ARB_SCAN_* vars → no overrides (momentum settings inherited)", () => {
  assert.deepEqual(arbScanEnvOverrides({ SCAN_MIN_LIQUIDITY: "200000" }), {});
  assert.deepEqual(arbScanEnvOverrides({ ARB_SCAN_SOURCE: "" }), {}, "empty string is not an override");
});

// rawRpcEligible: ≥2 tradeable USDC venues → 2-hop USDC→X→USDC (the no-tip raw-RPC shape).
const USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const SOL_MINT = "So11111111111111111111111111111111111111112";
const venue = (dexId, quoteMint) => ({ dexId, quoteMint });
test("rawRpcEligible: needs ≥2 tradeable USDC venues", () => {
  const two = [venue("raydium", USDC_MINT), venue("orca", USDC_MINT), venue("meteora", SOL_MINT)];
  assert.equal(rawRpcEligible(two, { pumpTradeable: false }), true, "2 USDC venues → eligible");
  const one = [venue("raydium", USDC_MINT), venue("orca", SOL_MINT)];
  assert.equal(rawRpcEligible(one, { pumpTradeable: false }), false, "1 USDC venue → not eligible (3-hop only)");
  // a pumpswap USDC venue only counts when pump trading is enabled
  const pump = [venue("pumpswap", USDC_MINT), venue("orca", USDC_MINT)];
  assert.equal(rawRpcEligible(pump, { pumpTradeable: false }), false, "pumpswap not tradeable → only 1 counts");
  assert.equal(rawRpcEligible(pump, { pumpTradeable: true }), true, "pumpswap tradeable → 2 count");
});

test("rawRpcEligible: minUsdcLiq floor drops thin USDC legs (stale-spread artifacts)", () => {
  const vl = (dexId, quoteMint, liquidityUsd) => ({ dexId, quoteMint, liquidityUsd });
  const venues = [vl("raydium", USDC_MINT, 100000), vl("orca", USDC_MINT, 10000), vl("meteora", USDC_MINT, 80000)];
  // floor 50k: raydium(100k) + meteora(80k) clear it, orca(10k) doesn't → 2 remain → eligible
  assert.equal(rawRpcEligible(venues, { pumpTradeable: false, minUsdcLiq: 50000 }), true, "2 legs above floor → eligible");
  // floor 90k: only raydium(100k) clears → 1 remains → not eligible
  assert.equal(rawRpcEligible(venues, { pumpTradeable: false, minUsdcLiq: 90000 }), false, "only 1 leg above floor → not eligible");
  // no floor → all 3 count (unchanged legacy behaviour)
  assert.equal(rawRpcEligible(venues, { pumpTradeable: false }), true, "no floor → legacy count");
});

// actScore ranks arb candidates by 24h volume anchored, short-window volatility as a
// bounded multiplier (default ARB_VOLATILITY_WEIGHT=1.0; these hold for any weight > 0).
test("actScore: volume anchored, volatility a bounded multiplier", () => {
  assert.equal(actScore(1000, 0), 1000, "zero volatility → pure volume");
  assert.equal(actScore(1000, NaN), 1000, "missing change → pure-volume fallback");
  assert.equal(actScore(0, 500), 0, "no volume → zero regardless of volatility");
  assert.ok(actScore(1000, 50) > 1000, "positive volatility boosts volume");
  assert.equal(actScore(1000, 500), actScore(1000, 200), "tail capped at a +200% move");
  assert.equal(actScore(1000, -80), actScore(1000, 80), "downside volatility counts (|change|)");
});

const SOL = "So11111111111111111111111111111111111111112";
const AAA = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";

// Complete `extra` fixtures per dex — matches the field names in scan_arb_pools.js's
// REQUIRED.extra table (mirroring check_extra in src/dex/mod.rs), so `ok()` produces a
// well-formed pool by default for any dex kind and tests can delete a single field to
// isolate one missing-field assertion.
const DEFAULT_EXTRA = {
  raydium_amm_v4: {
    amm_authority: "AUTH", open_orders: "OO", target_orders: "TO", market_program: "MP",
    market: "MKT", market_bids: "BIDS", market_asks: "ASKS", market_event_queue: "EQ",
    market_coin_vault: "MCV", market_pc_vault: "MPV", market_vault_signer: "MVS",
  },
  orca_whirlpool: { tick_array_0: "TA0", tick_array_1: "TA1", tick_array_2: "TA2", oracle: "ORC" },
  raydium_clmm: { clmm_amm_config: "CFG", clmm_tick_spacing: 60 },
  meteora_dlmm: { dlmm_bin_step: 10 },
  meteora_damm: {
    a_vault_lp: "AVLP", b_vault_lp: "BVLP", a_token_vault: "ATV", b_token_vault: "BTV",
    a_vault_lp_mint: "AVLM", b_vault_lp_mint: "BVLM", admin_token_fee_a: "ATFA", admin_token_fee_b: "ATFB",
  },
};

const ok = (id, dex = "raydium_amm_v4", extra = {}) => ({
  id, dex, token_a: SOL, token_b: AAA, vault_a: id + "-va", vault_b: id + "-vb",
  fee_bps: 25, extra: { ...(DEFAULT_EXTRA[dex] || {}), ...extra },
});

test("validateBook accepts a well-formed book", () => {
  assert.deepEqual(validateBook([ok("p1")]), { ok: true, errors: [] });
});

test("validateBook rejects an empty book", () => {
  const r = validateBook([]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /empty/i);
});

test("validateBook rejects a pool missing a vault", () => {
  const bad = ok("p1");
  delete bad.vault_b;
  const r = validateBook([bad]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /vault_b/);
});

test("validateBook requires state_account for concentrated-liquidity pools", () => {
  const r = validateBook([ok("clmm1", "raydium_clmm")]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /state_account/);
});

test("validateBook rejects a DLMM pool missing state_account", () => {
  const r = validateBook([ok("d1", "meteora_dlmm")]);   // ok() gives no state_account
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /state_account/);
});

test("validateBook accepts a complete Orca pool", () => {
  const p = ok("orca1", "orca_whirlpool");
  p.state_account = "SA1";
  assert.deepEqual(validateBook([p]), { ok: true, errors: [] });
});

test("validateBook rejects an Orca pool missing extra.oracle", () => {
  const p = ok("orca1", "orca_whirlpool");
  p.state_account = "SA1";   // isolate the assertion to the missing extra.oracle field
  delete p.extra.oracle;
  const r = validateBook([p]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /oracle/);
});

test("validateBook accepts a pricing-only pump pool without coin_creator (pump trading off)", () => {
  const r = validateBook([ok("pump1", "pump_swap")]); // no opts → pumpTradeable false
  assert.equal(r.ok, true);
});

test("validateBook requires pumpswap_coin_creator for a TRADEABLE pump pool", () => {
  const r = validateBook([ok("pump1", "pump_swap")], { pumpTradeable: true });
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /coin_creator/);
});

test("validateBook accepts a tradeable pump pool WITH coin_creator present", () => {
  const p = { ...ok("pump1", "pump_swap"), extra: { pumpswap_coin_creator: "CC1" } };
  assert.equal(validateBook([p], { pumpTradeable: true }).ok, true);
});

test("bookChanged ignores ordering", () => {
  const a = [ok("p1"), ok("p2")];
  const b = [ok("p2"), ok("p1")];
  assert.equal(bookChanged(a, b), false);
});

test("bookChanged detects an added or removed pool", () => {
  assert.equal(bookChanged([ok("p1")], [ok("p1"), ok("p2")]), true);
  assert.equal(bookChanged([ok("p1"), ok("p2")], [ok("p1")]), true);
});

test("bookChanged detects a change confined to extra", () => {
  const a = [{ ...ok("p1"), extra: { oracle: "O1" } }];
  const b = [{ ...ok("p1"), extra: { oracle: "O2" } }];
  assert.equal(bookChanged(a, b), true);
});
