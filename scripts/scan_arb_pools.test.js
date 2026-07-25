"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { validateBook, bookChanged } = require("./scan_arb_pools");

const SOL = "So11111111111111111111111111111111111111112";
const AAA = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ok = (id, dex = "raydium_amm_v4", extra = {}) => ({
  id, dex, token_a: SOL, token_b: AAA, vault_a: id + "-va", vault_b: id + "-vb",
  fee_bps: 25, ...extra,
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
