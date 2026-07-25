"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { isProtected, selectBook } = require("./book_budget");

const SOL  = "So11111111111111111111111111111111111111112";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const AAA  = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const hubs = new Set([SOL, USDC]);

// Each pool contributes exactly 2 accounts (vault_a + vault_b).
const pool = (id, token_a, token_b, act) => ({
  id, token_a, token_b, _act: act, dex: "raydium_amm_v4",
  vault_a: id + "-va", vault_b: id + "-vb",
});
const ctx = { pinnedIds: new Set(["pinned1"]), momentumPoolIds: new Set(["mom1"]), hubs };

test("isProtected: hub-major pool, pinned address, momentum pool", () => {
  assert.equal(isProtected(pool("x", SOL, USDC, 1), ctx), true, "SOL/USDC major");
  assert.equal(isProtected(pool("pinned1", SOL, AAA, 1), ctx), true, "fetcher-pinned");
  assert.equal(isProtected(pool("mom1", SOL, AAA, 1), ctx), true, "momentum watcher pool");
  assert.equal(isProtected(pool("other", SOL, AAA, 1), ctx), false);
});

test("isProtected: curated majors are core legs, memecoins stay evictable", () => {
  const RAY  = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";
  const MSOL = "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So";
  const ctxM = { pinnedIds: new Set(), momentumPoolIds: new Set(), hubs, majors: new Set([RAY, MSOL]) };
  assert.equal(isProtected(pool("hub-major", SOL, RAY, 1), ctxM), true, "hub↔major (SOL/RAY) is a core leg");
  assert.equal(isProtected(pool("major-major", RAY, MSOL, 1), ctxM), true, "major↔major (RAY/mSOL) is a core leg");
  assert.equal(isProtected(pool("meme", SOL, AAA, 9e9), ctxM), false, "hub↔memecoin stays evictable regardless of activity");
  // backward-compat: without ctx.majors, only hub↔hub is protected (legacy hub-hub rule)
  assert.equal(isProtected(pool("hub-major", SOL, RAY, 1), ctx), false, "no ctx.majors ⇒ hub↔RAY not protected");
});

test("selectBook keeps all core and fills remaining budget by activity", () => {
  const core = [pool("core1", SOL, USDC, 1)];                 // 2 accounts
  const candidates = [pool("c-hi", SOL, AAA, 100), pool("c-lo", SOL, AAA, 1)];
  const r = selectBook({ core, candidates, incumbentIds: new Set(), budget: 4, evictMargin: 1.25, countPumpSwap: false });
  assert.deepEqual(r.kept.map((p) => p.id), ["core1", "c-hi"], "highest activity wins the last slot");
  assert.equal(r.accounts, 4);
});

test("selectBook never exceeds the account budget", () => {
  const core = [pool("core1", SOL, USDC, 1)];
  const candidates = [pool("c1", SOL, AAA, 9), pool("c2", SOL, AAA, 8)];
  const r = selectBook({ core, candidates, incumbentIds: new Set(), budget: 3, evictMargin: 1.25, countPumpSwap: false });
  assert.equal(r.kept.length, 1, "core only — no room for a 2-account candidate");
  assert.ok(r.accounts <= 3);
});

test("selectBook throws when the core alone exceeds the budget", () => {
  const core = [pool("core1", SOL, USDC, 1), pool("core2", SOL, USDC, 1)];
  assert.throws(
    () => selectBook({ core, candidates: [], incumbentIds: new Set(), budget: 2, evictMargin: 1.25, countPumpSwap: false }),
    /core .*exceeds/i,
  );
});

test("hysteresis: a marginally-better challenger cannot evict an incumbent", () => {
  const core = [];
  const incumbent = pool("inc", SOL, AAA, 100);
  const challenger = pool("new", SOL, AAA, 110);      // only 1.1x — below the 1.25 margin
  const r = selectBook({
    core, candidates: [challenger, incumbent], incumbentIds: new Set(["inc"]),
    budget: 2, evictMargin: 1.25, countPumpSwap: false,
  });
  assert.deepEqual(r.kept.map((p) => p.id), ["inc"], "incumbent holds the slot");
});

test("hysteresis: a decisively-better challenger does evict", () => {
  const core = [];
  const incumbent = pool("inc", SOL, AAA, 100);
  const challenger = pool("new", SOL, AAA, 500);      // 5x — clears the margin
  const r = selectBook({
    core, candidates: [challenger, incumbent], incumbentIds: new Set(["inc"]),
    budget: 2, evictMargin: 1.25, countPumpSwap: false,
  });
  assert.deepEqual(r.kept.map((p) => p.id), ["new"]);
  assert.deepEqual(r.evicted.map((p) => p.id), ["inc"]);
});
