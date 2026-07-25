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

test("validateBook requires pumpswap_coin_creator for a tradeable pump pool", () => {
  const r = validateBook([ok("pump1", "pump_swap")]);
  assert.equal(r.ok, false);
  assert.match(r.errors.join(" "), /coin_creator/);
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
