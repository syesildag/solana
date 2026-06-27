"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { filterCandidates, rankSurvivors, mapTrendingToken, needsChange } = require("./scan_tokens");

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

test("admits a mid-volume liquid token (SLX-like) once paginated into view", () => {
  // Birdeye numbers: ~$1.7M vol / ~$427k liq, ratio 4 — clears both floors + the cap.
  // It was only ever excluded by the single top-50 fetch, which pagination removes.
  const rows = [row(BONK, "SLX", 1_700_000, 427_000, "Solstice")];
  assert.deepEqual(filterCandidates(rows, [], opts).map((r) => r.symbol), ["SLX"]);
});

// ── rankSurvivors (momentum ordering) ────────────────────────────────────────────
const sv = (symbol, vol24, change24h) => ({ symbol, vol24, change24h });

test("rank=volume orders by 24h volume desc (regression — ignores change)", () => {
  const out = rankSurvivors(
    [sv("SMALL", 1e6, 99), sv("BIG", 1e8, 1)],
    { rank: "volume", maxChangePct: 50 }
  );
  assert.deepEqual(out.map((s) => s.symbol), ["BIG", "SMALL"]);
});

test("rank=change sorts by change desc and surfaces a mover over a flat giant", () => {
  const out = rankSurvivors(
    [sv("GIANT", 1e8, 1), sv("KLED", 4e6, 28), sv("MID", 1e7, 12)],
    { rank: "change", maxChangePct: 50 }
  );
  assert.deepEqual(out.map((s) => s.symbol), ["KLED", "MID", "GIANT"]);
});

test("rank=change drops non-positive movers and applies the ceiling", () => {
  const out = rankSurvivors(
    [sv("UP", 1e6, 28), sv("DOWN", 1e6, -5), sv("FLAT", 1e6, 0), sv("PARABOLIC", 1e6, 120)],
    { rank: "change", maxChangePct: 50 }
  );
  assert.deepEqual(out.map((s) => s.symbol), ["UP"], "only the in-band up-mover survives");
});

test("rank=change with ceiling 0 keeps all positive movers (no upper bound)", () => {
  const out = rankSurvivors(
    [sv("A", 1e6, 200), sv("B", 1e6, 28), sv("C", 1e6, -1)],
    { rank: "change", maxChangePct: 0 }
  );
  assert.deepEqual(out.map((s) => s.symbol), ["A", "B"]);
});

test("rank=change drops survivors with no readable change24h", () => {
  const out = rankSurvivors(
    [sv("OK", 1e6, 10), sv("NULLCHG", 1e6, null), sv("NANCHG", 1e6, NaN)],
    { rank: "change", maxChangePct: 50 }
  );
  assert.deepEqual(out.map((s) => s.symbol), ["OK"]);
});

// ── mapTrendingToken (trending API → candidate row) ───────────────────────────────

test("mapTrendingToken maps Birdeye trending fields to the candidate row shape", () => {
  const t = {
    address: RAY, symbol: "RAY", name: "Raydium",
    volume24hUSD: 1_700_000, liquidity: 427_000, price24hChangePercent: 33.5,
  };
  assert.deepEqual(mapTrendingToken(t), {
    address: RAY, symbol: "RAY", name: "Raydium",
    v24hUSD: 1_700_000, liquidity: 427_000, change24h: 33.5,
  });
});

test("mapTrendingToken coerces missing numerics to 0 and non-finite change to null", () => {
  const out = mapTrendingToken({ address: BONK });
  assert.equal(out.symbol, "");
  assert.equal(out.name, "");
  assert.equal(out.v24hUSD, 0);
  assert.equal(out.liquidity, 0);
  assert.equal(out.change24h, null);
});

// ── needsChange (annotate-skip predicate) + change24h flow-through ─────────────────

test("needsChange is true only when change24h is non-finite", () => {
  assert.equal(needsChange({ change24h: 12.3 }), false);
  assert.equal(needsChange({ change24h: 0 }), false);
  assert.equal(needsChange({ change24h: null }), true);
  assert.equal(needsChange({ change24h: NaN }), true);
  assert.equal(needsChange({}), true);
});

test("filterCandidates preserves change24h on a surviving trending row", () => {
  const mapped = mapTrendingToken({
    address: BONK, symbol: "BONK", name: "Bonk",
    volume24hUSD: 2_000_000, liquidity: 800_000, price24hChangePercent: 22.5,
  });
  const out = filterCandidates([mapped], [], opts);
  assert.equal(out.length, 1);
  assert.equal(out[0].change24h, 22.5);
});
