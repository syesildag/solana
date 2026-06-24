#!/usr/bin/env node
/**
 * Measure the REAL Jupiter price impact for each pairs-trader leg, so the
 * conservative PAIRS_SLIPPAGE_BPS guess can be replaced with a measurement.
 *
 * Why: the pairs backtest is razor-thin on cost — robustness collapses as the
 * per-leg cost rises (≈30 robust configs @ 5bps/leg → 3 @ 15bps → 0 @ 25bps).
 * The whole strategy's viability hinges on real execution cost, which this prints.
 *
 * For each unique token in pairs.json it round-trips USDC→token→USDC at the probe
 * notional and reads `priceImpactPct` from the same Jupiter v6 /quote the bot uses
 * (src/portfolio/jupiter.rs). A pairs round-trip crosses 4 legs — buy A + sell B to
 * open, sell A + buy B to close — so per-pair round-trip cost = (buy+sell of A) +
 * (buy+sell of B), and per-leg = that ÷ 4 (the backtest's --pair-cost-bps unit).
 *
 * Usage:
 *   node scripts/probe_pairs_impact.js
 *   PROBE_USDC=250 node scripts/probe_pairs_impact.js
 *   JUPITER_API_URL=https://lite-api.jup.ag/swap/v1 node scripts/probe_pairs_impact.js
 *
 * JUPITER_API_URL defaults to http://127.0.0.1:8080 (the bot's self-hosted Metis,
 * which serves /quote at the root). If Metis isn't running, point it at Jupiter's
 * public free endpoint https://lite-api.jup.ag/swap/v1 (the old quote-api.jup.ag/v6
 * host is deprecated). Both return the same outAmount + priceImpactPct fields.
 */
"use strict";
const fs = require("fs");
const path = require("path");

const USDC = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // 6 decimals
const USDC_DECIMALS = 6;
const BASE = process.env.JUPITER_API_URL || "http://127.0.0.1:8080";
const NOTIONAL = Number(process.env.PROBE_USDC || 100);
const PAIRS_PATH = process.env.PAIRS_PATH || path.join(__dirname, "..", "assets", "pairs.json");
const SLIPPAGE_BPS = 200; // just a tolerance for the quote; impact is reported regardless

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// One v6 /quote, matching the params the bot sends in src/portfolio/jupiter.rs.
async function quote(inputMint, outputMint, amountRaw) {
  const q = new URLSearchParams({
    inputMint,
    outputMint,
    amount: String(amountRaw),
    slippageBps: String(SLIPPAGE_BPS),
    onlyDirectRoutes: "false",
    asLegacyTransaction: "false",
  });
  const res = await fetch(`${BASE}/quote?${q}`);
  if (!res.ok) throw new Error(`/quote ${res.status}: ${(await res.text()).slice(0, 200)}`);
  const j = await res.json();
  return { outAmount: j.outAmount, impactBps: Number(j.priceImpactPct) * 10_000 };
}

// Buy ($NOTIONAL of USDC → token) then sell (the received token → USDC).
// Returns both impacts in bps; round-trip = buy + sell.
async function probeToken(symbol, mint) {
  const buy = await quote(USDC, mint, Math.round(NOTIONAL * 10 ** USDC_DECIMALS));
  await sleep(250);
  const sell = await quote(mint, USDC, buy.outAmount);
  return { symbol, buy: buy.impactBps, sell: sell.impactBps, roundTrip: buy.impactBps + sell.impactBps };
}

function verdict(perLegBps) {
  if (perLegBps <= 5) return "≈30 robust configs — healthy edge";
  if (perLegBps <= 10) return "≈7 robust configs — thin but real edge";
  if (perLegBps <= 15) return "≈3 robust configs — marginal";
  if (perLegBps <= 25) return "0 robust at funded cost — NO edge";
  return "well past breakeven — strategy not viable at this cost";
}

(async () => {
  const pairs = JSON.parse(fs.readFileSync(PAIRS_PATH, "utf8"));
  console.log(`Probing real Jupiter impact at $${NOTIONAL}/leg via ${BASE}`);
  console.log(`Pairs file: ${PAIRS_PATH}\n`);

  // Unique tokens across all pairs (dedupe by mint).
  const tokens = new Map();
  for (const p of pairs) {
    tokens.set(p.mint_a, p.symbol_a);
    tokens.set(p.mint_b, p.symbol_b);
  }

  const impact = new Map(); // symbol -> {buy, sell, roundTrip}
  console.log("Per-token impact (USDC→token→USDC):");
  console.log("  token        buy bps   sell bps   round-trip");
  console.log("  " + "-".repeat(48));
  for (const [mint, sym] of tokens) {
    try {
      const r = await probeToken(sym, mint);
      impact.set(sym, r);
      console.log(`  ${sym.padEnd(10)} ${r.buy.toFixed(1).padStart(8)} ${r.sell.toFixed(1).padStart(10)} ${r.roundTrip.toFixed(1).padStart(11)}`);
    } catch (e) {
      console.log(`  ${sym.padEnd(10)}   ERROR: ${e.message}`);
    }
    await sleep(250);
  }

  console.log("\nPer-pair round-trip cost (4 legs = both tokens' buy+sell):");
  console.log("  pair             total bps   per-leg bps   verdict");
  console.log("  " + "-".repeat(72));
  for (const p of pairs) {
    const a = impact.get(p.symbol_a), b = impact.get(p.symbol_b);
    if (!a || !b) {
      console.log(`  ${(p.symbol_a + "/" + p.symbol_b).padEnd(14)}   (incomplete — a leg failed to quote)`);
      continue;
    }
    const total = a.roundTrip + b.roundTrip;
    const perLeg = total / 4;
    console.log(`  ${(p.symbol_a + "/" + p.symbol_b).padEnd(14)} ${total.toFixed(1).padStart(10)} ${perLeg.toFixed(1).padStart(12)}    ${verdict(perLeg)}`);
  }
  console.log(`\nSet PAIRS_SLIPPAGE_BPS to the worst single-leg impact you see above (live charges it per close leg).`);
})().catch((e) => {
  console.error(`\nProbe failed: ${e.message}`);
  console.error(`If using the local Metis, ensure it's running (DRY_RUN=true cargo run --release --bin solana-mev),`);
  console.error(`or set JUPITER_API_URL to Jupiter's public endpoint: https://lite-api.jup.ag/swap/v1`);
  process.exit(1);
});
