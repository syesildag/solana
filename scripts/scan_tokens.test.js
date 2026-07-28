"use strict";
const { test } = require("node:test");
const assert = require("node:assert");
const { filterCandidates, classifyCandidates, rankSurvivors, mapTrendingToken, needsChange, verifyAll } = require("./scan_tokens");

// verifyAll: SCAN_REQUIRE_JUP_VERIFY=false skips the Jupiter verification gate entirely
// (no fetch, no audit) — the on-chain token_safety screen downstream still runs and is
// the authoritative trap check (freeze authority / transfer hook / frozen state).
test("verifyAll: requireJupVerified=false passes candidates through without fetching", async () => {
  const cands = [{ address: "MintA", symbol: "AAA" }, { address: "MintB", symbol: "BBB" }];
  const neverFetch = async () => { throw new Error("must not call Jupiter when verification is skipped"); };
  const out = await verifyAll(cands, { requireJupVerified: false, maxTopHoldersPct: 0 }, neverFetch);
  assert.deepEqual(out, cands);
});

test("verifyAll: requireJupVerified=true drops candidates the fetcher rejects", async () => {
  const cands = [{ address: "MintA", symbol: "AAA" }, { address: "MintB", symbol: "BBB" }];
  const onlyB = async (addr) => (addr === "MintB" ? { id: addr, audit: {} } : null);
  const out = await verifyAll(cands, { requireJupVerified: true, maxTopHoldersPct: 0 }, onlyB);
  assert.deepEqual(out.map((c) => c.symbol), ["BBB"]);
});

// Valid base58 mints (so they pass MINT_RE).
const RAY  = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R";
const BONK = "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263";
const WIF  = "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm";
const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const opts = { minVolume: 250_000, minLiquidity: 200_000, minRatio: 0.5, maxRatio: 30 };

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

test("rejects stale tokens below the vol/liq ratio floor", () => {
  // $300k vol / $1M liq = ratio 0.3 — clears volume+liquidity floors but barely traded.
  const rows = [row(BONK, "STALE", 300_000, 1_000_000)];
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

// ── classifyCandidates (filter funnel diagnostics) ───────────────────────────────
// Same gates as filterCandidates, but every drop carries a stage + reason so the
// hourly scan can explain WHY a candidate died instead of silently shrinking.

test("classifyCandidates: passed matches filterCandidates exactly", () => {
  const rows = [
    row(BONK, "BONK", 2_000_000, 800_000),
    row(WIF, "wUSDT", 9_000_000, 5_000_000),
    row(RAY, "RAY", 2_000_000, 800_000),
  ];
  const { passed } = classifyCandidates(rows, [RAY], opts);
  assert.deepEqual(passed, filterCandidates(rows, [RAY], opts));
});

test("classifyCandidates: each drop carries its stage and a human-readable reason", () => {
  const rows = [
    row("not-a-mint!", "BAD", 9_000_000, 5_000_000),      // invalid mint
    row(USDC, "USDC", 9_000_000, 5_000_000),               // denylist
    row(RAY, "RAY", 2_000_000, 800_000),                   // curated dup
    row(BONK, "LOWVOL", 100_000, 800_000),                 // volume floor
    row(WIF, "WASHY", 50_000_000, 300_000),                // wash ratio cap (~167×)
  ];
  const { passed, drops } = classifyCandidates(rows, [RAY], opts);
  assert.equal(passed.length, 0);
  assert.deepEqual(
    drops.map((d) => [d.symbol, d.stage]),
    [
      ["BAD", "mint"],
      ["USDC", "deny"],
      ["RAY", "curated"],
      ["LOWVOL", "floors"],
      ["WASHY", "wash"],
    ]
  );
  for (const d of drops) assert.ok(d.reason && typeof d.reason === "string");
});

test("classifyCandidates: stale ratio-floor drop is a wash-stage drop with the ratio in the reason", () => {
  const { drops } = classifyCandidates([row(BONK, "STALE", 300_000, 1_000_000)], [], opts);
  assert.equal(drops.length, 1);
  assert.equal(drops[0].stage, "wash");
  assert.match(drops[0].reason, /0\.3/);
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

// ── auditRejectReason: holder-concentration + authority rug guard ──────────────
const { auditRejectReason } = require("./scan_tokens");

const tok = (audit) => ({ id: RAY, symbol: "X", isVerified: true, audit });

test("audit gate rejects top-holder concentration above the cap (SOLANGELES case)", () => {
  const r = auditRejectReason(tok({ topHoldersPercentage: 45.26, mintAuthorityDisabled: true, freezeAuthorityDisabled: true }), 30);
  assert.match(r, /45\.3% > 30% cap/);
});

test("audit gate passes a well-distributed token (world case, 14.5%)", () => {
  assert.equal(auditRejectReason(tok({ topHoldersPercentage: 14.46, mintAuthorityDisabled: true, freezeAuthorityDisabled: true }), 30), null);
});

test("audit gate fails closed on missing audit or missing concentration data", () => {
  assert.equal(auditRejectReason({ id: RAY, isVerified: true }, 30), "no audit data");
  assert.equal(auditRejectReason(tok({ mintAuthorityDisabled: true }), 30), "no top-holders data");
});

test("audit gate rejects live mint/freeze authority regardless of concentration", () => {
  assert.match(auditRejectReason(tok({ topHoldersPercentage: 5, mintAuthorityDisabled: false }), 30), /mint authority/);
  assert.match(auditRejectReason(tok({ topHoldersPercentage: 5, mintAuthorityDisabled: true, freezeAuthorityDisabled: false }), 30), /freeze authority/);
});

test("audit gate cap=0 disables the concentration check but keeps authority checks", () => {
  assert.equal(auditRejectReason(tok({ topHoldersPercentage: 90, mintAuthorityDisabled: true, freezeAuthorityDisabled: true }), 0), null);
  assert.match(auditRejectReason(tok({ topHoldersPercentage: 90, mintAuthorityDisabled: false }), 0), /mint authority/);
});

// ── pickGrpcPools: dynamic-wiring pool picker (top-N gRPC-priceable venues) ──────
const { pickGrpcPools } = require("./scan_tokens");

const pair = (dexId, pairAddress, volH24, quoteSym) =>
  ({ dexId, pairAddress, volume: { h24: volH24 }, quoteToken: { symbol: quoteSym } });

test("pickGrpcPools returns the top gRPC-priceable venues, volume-ranked, each with its dex", () => {
  const pairs = [
    pair("pumpswap", RAY, 100_000, "SOL"),
    pair("meteora", BONK, 900_000, "SOL"),   // highest — first
    pair("orca", WIF, 500_000, "USDC"),
  ];
  assert.deepEqual(pickGrpcPools(pairs), [
    { pool: BONK, quote: "SOL", dex: "meteora" },
    { pool: WIF, quote: "USDC", dex: "orca" },
    { pool: RAY, quote: "SOL", dex: "pumpswap" },
  ]);
});

test("pickGrpcPools includes a non-pumpswap venue and caps at 3", () => {
  const pairs = [
    pair("raydium", RAY, 900_000, "SOL"),
    pair("orca", BONK, 800_000, "SOL"),
    pair("meteora", WIF, 700_000, "SOL"),
    pair("pumpswap", "Pmp1111111111111111111111111111111111111111", 600_000, "SOL"), // 4th — dropped by the cap
  ];
  const got = pickGrpcPools(pairs);
  assert.equal(got.length, 3, "capped at GRPC_POOLS_MAX");
  assert.deepEqual(got.map((p) => p.dex), ["raydium", "orca", "meteora"]);
});

test("pickGrpcPools skips non-gRPC venues", () => {
  const pairs = [
    pair("lifinity", RAY, 900_000, "SOL"),   // not gRPC-priceable — skipped
    pair("orca", BONK, 100_000, "SOL"),      // only eligible venue
  ];
  assert.deepEqual(pickGrpcPools(pairs), [{ pool: BONK, quote: "SOL", dex: "orca" }]);
});

test("pickGrpcPools normalizes quote and rejects exotic quotes", () => {
  assert.deepEqual(pickGrpcPools([pair("pumpswap", RAY, 1, "USDC")]), [{ pool: RAY, quote: "USDC", dex: "pumpswap" }]);
  assert.equal(pickGrpcPools([pair("pumpswap", RAY, 1, "ORE")]), null);
});

test("pickGrpcPools handles empty/malformed input", () => {
  assert.equal(pickGrpcPools([]), null);
  assert.equal(pickGrpcPools(undefined), null);
  assert.equal(pickGrpcPools([{ dexId: "pumpswap" }]), null); // no pairAddress
});
