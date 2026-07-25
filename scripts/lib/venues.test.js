"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { bestPoolPerVenue, tradeableVenueCount, SUPPORTED_DEX_IDS } = require("./venues");

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
