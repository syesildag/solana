"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { validateBook, bookChanged, actScore } = require("./scan_arb_pools");

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
