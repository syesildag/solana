"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { filterCandidates } = require("./scan_tokens");

// Valid base58 mints (so they pass MINT_RE).
const RAY  = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";
const BONK = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
const WIF  = "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const opts = { minVolume: 250_000, minLiquidity: 200_000, maxRatio: 30 };

const row = (address, symbol, v24hUSD, liquidity, name = "") =>
  ({ address, symbol, name, v24hUSD, liquidity });

test("rejects wash-trade tokens by the vol/liq ratio cap", () => {
  const rows = [row(BONK, "SV151", 50_000_000, 164)]; // ratio ≈ 305,000×
  assert.equal(filterCandidates(rows, [], opts).length, 0);
});

test("denylists stablecoins by mint and by symbol (incl. USD suffix)", () => {
  const rows = [
    row(USDC, "USDC", 9_000_000, 5_000_000),     // by mint
    row(BONK, "wUSDT", 9_000_000, 5_000_000),    // usd prefix
    row(WIF, "JupUSD", 16_000_000, 13_000_000),  // usd suffix (real case seen live)
  ];
  assert.equal(filterCandidates(rows, [], opts).length, 0);
});

test("dedups mints already curated", () => {
  const rows = [row(RAY, "RAY", 2_000_000, 800_000)];
  assert.equal(filterCandidates(rows, [RAY], opts).length, 0);
});

test("rejects below the volume or liquidity floors", () => {
  const rows = [
    row(BONK, "LOWVOL", 100_000, 800_000),
    row(WIF, "LOWLIQ", 2_000_000, 50_000),
  ];
  assert.equal(filterCandidates(rows, [], opts).length, 0);
});

test("passes a clean liquid token and sorts survivors by volume desc", () => {
  const rows = [
    row(BONK, "BONK", 2_000_000, 800_000), // ratio 2.5
    row(WIF, "WIF", 5_000_000, 1_000_000), // ratio 5.0, higher volume
  ];
  const out = filterCandidates(rows, [], opts);
  assert.deepEqual(out.map((r) => r.symbol), ["WIF", "BONK"]);
});
