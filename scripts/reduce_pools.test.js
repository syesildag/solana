"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { pruneToCycles, countAccounts, HUBS } = require("./reduce_pools");

const SOL  = "So11111111111111111111111111111111111111112";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const AAA  = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const BBB  = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";

// Minimal pool config: only the fields the two functions read.
const pool = (id, token_a, token_b, act, extra = {}) => ({
  id, token_a, token_b, _act: act, dex: extra.dex || "raydium_amm_v4",
  vault_a: id + "-va", vault_b: id + "-vb", ...extra,
});

test("pruneToCycles drops a non-hub token with only one venue", () => {
  const pools = [pool("p1", SOL, AAA, 10)];            // AAA degree 1 → dead end
  assert.deepEqual(pruneToCycles(pools), []);
});

test("pruneToCycles keeps a non-hub token with two venues", () => {
  const pools = [pool("p1", SOL, AAA, 10), pool("p2", SOL, AAA, 9)];
  assert.equal(pruneToCycles(pools).length, 2);
});

test("pruneToCycles cascades: removing one pool can orphan another", () => {
  // AAA has 2 venues, but BBB has 1. Dropping BBB's pool leaves AAA with 1 → both go.
  const pools = [pool("p1", SOL, AAA, 10), pool("p2", AAA, BBB, 9)];
  assert.deepEqual(pruneToCycles(pools), []);
});

test("pruneToCycles keeps a SOL-only component (base is SOL, not USDC)", () => {
  // No USDC anywhere: seeding connectivity from USDC alone would wrongly drop everything.
  const pools = [pool("p1", SOL, AAA, 10), pool("p2", SOL, AAA, 9)];
  const kept = pruneToCycles(pools);
  assert.equal(kept.length, 2, "SOL-connected component must survive");
});

test("countAccounts counts pump_swap vaults when asked (tradeable venue)", () => {
  const pools = [pool("pump1", SOL, AAA, 5, { dex: "pump_swap" })];
  assert.equal(countAccounts(pools), 0, "default: pricing-only, not counted");
  assert.equal(countAccounts(pools, { countPumpSwap: true }), 2, "tradeable: both vaults count");
});

test("countAccounts dedups shared accounts and includes CL state + DAMM lp", () => {
  const pools = [
    pool("p1", SOL, AAA, 5, { state_account: "st1" }),
    pool("p2", SOL, BBB, 5, { extra: { a_vault_lp: "lp1", b_vault_lp: "lp2" } }),
    pool("p3", SOL, AAA, 5, { vault_a: "p1-va", vault_b: "p1-vb" }), // duplicate vaults
  ];
  // p1: va,vb,st1 = 3 | p2: va,vb,lp1,lp2 = 4 | p3: dupes = 0  → 7
  assert.equal(countAccounts(pools), 7);
});

test("HUBS contains both SOL and USDC", () => {
  assert.ok(HUBS.has(SOL) && HUBS.has(USDC));
});
