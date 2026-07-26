"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { bestPoolPerVenue, tradeableVenueCount, quotePools, SUPPORTED_DEX_IDS } = require("./venues");

const SOL  = "So11111111111111111111111111111111111111112";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const OTHER = "9999999999999999999999999999999999999999999";
const hubs = new Set([SOL, USDC]);

const pair = (dexId, pairAddress, quote, vol, liq = 500_000) => ({
  dexId, pairAddress,
  quoteToken: { address: quote },
  volume: { h24: vol },
  liquidity: { usd: liq },
});

test("keeps the highest-24h-volume pool per venue", () => {
  const pairs = [
    pair("raydium", "ray-low", SOL, 1_000),
    pair("raydium", "ray-high", SOL, 900_000),
    pair("orca", "orca-1", SOL, 50_000),
  ];
  const out = bestPoolPerVenue(pairs, { quoteAllowlist: hubs });
  assert.equal(out.length, 2);
  assert.equal(out.find((v) => v.dexId === "raydium").pairAddress, "ray-high");
});

test("drops unsupported venues", () => {
  const pairs = [pair("someotherdex", "x-1", SOL, 999_999)];
  assert.deepEqual(bestPoolPerVenue(pairs, { quoteAllowlist: hubs }), []);
});

test("drops pairs whose quote side is not a hub", () => {
  const pairs = [pair("raydium", "ray-1", OTHER, 999_999)];
  assert.deepEqual(bestPoolPerVenue(pairs, { quoteAllowlist: hubs }), []);
});

test("tradeableVenueCount excludes pumpswap when the venue is not tradeable", () => {
  const venues = [{ dexId: "pumpswap" }, { dexId: "raydium" }];
  assert.equal(tradeableVenueCount(venues, { pumpTradeable: false }), 1);
  assert.equal(tradeableVenueCount(venues, { pumpTradeable: true }), 2);
});

test("a pumpswap-only token has <2 tradeable venues either way", () => {
  const venues = [{ dexId: "pumpswap" }];
  assert.ok(tradeableVenueCount(venues, { pumpTradeable: true }) < 2);
});

test("SUPPORTED_DEX_IDS covers exactly the decodable venues", () => {
  assert.deepEqual([...SUPPORTED_DEX_IDS].sort(), ["meteora", "orca", "pumpswap", "raydium"]);
});

// quotePools is POOL-level on purpose: two pools on the SAME dex form a valid
// QUOTE→X→QUOTE 2-hop (only same-POOL cycles are phantoms) — bestPoolPerVenue's
// dexId-keying collapsed them into one venue and under-admitted DLMM multi-bin tokens.
test("quotePools keeps multiple same-dex pools, volume-desc", () => {
  const pairs = [
    pair("meteora", "dlmm-bin20", USDC, 40_000, 60_000),
    pair("meteora", "dlmm-bin100", USDC, 90_000, 80_000),
    pair("raydium", "ray-1", USDC, 10_000, 30_000),
  ];
  const out = quotePools(pairs, { quoteMint: USDC, minLiq: 20_000 });
  assert.deepEqual(out.map((v) => v.pairAddress), ["dlmm-bin100", "dlmm-bin20", "ray-1"]);
});

test("quotePools filters quote mint, liquidity floor, unsupported dexes, dupes", () => {
  const pairs = [
    pair("raydium", "ray-usdc", USDC, 50_000, 100_000),
    pair("raydium", "ray-usdc", USDC, 50_000, 100_000),   // duplicate pairAddress
    pair("raydium", "ray-sol", SOL, 900_000, 900_000),    // wrong quote
    pair("orca", "orca-thin", USDC, 70_000, 5_000),       // below floor
    pair("someotherdex", "x-1", USDC, 999_999, 999_999),  // unsupported
  ];
  const out = quotePools(pairs, { quoteMint: USDC, minLiq: 20_000 });
  assert.deepEqual(out.map((v) => v.pairAddress), ["ray-usdc"]);
});

test("quotePools gates pumpswap on tradeability and honors the max cap", () => {
  const pairs = [
    pair("pumpswap", "pump-1", USDC, 800_000, 200_000),
    pair("raydium", "ray-1", USDC, 500_000, 200_000),
    pair("orca", "orca-1", USDC, 400_000, 200_000),
    pair("meteora", "met-1", USDC, 300_000, 200_000),
  ];
  const noPump = quotePools(pairs, { quoteMint: USDC });
  assert.ok(!noPump.some((v) => v.dexId === "pumpswap"), "pumpswap excluded by default");
  const withPump = quotePools(pairs, { quoteMint: USDC, pumpTradeable: true, max: 2 });
  assert.deepEqual(withPump.map((v) => v.pairAddress), ["pump-1", "ray-1"], "cap keeps top volume");
});
