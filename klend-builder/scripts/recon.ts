/**
 * THROWAWAY recon (Phase 0 gate for the liquidation bot). Measures whether a Kamino
 * lending market has any liquidatable / near-liquidation borrowers — cheaply — before
 * we build the detection subsystem. Read-only; no writes.
 *
 *   RPC_URL=https://... npx tsx scripts/recon.ts <marketPubkey> [maxBatches]
 *
 * Prints: total obligations, # with debt, and a health-factor histogram
 * (hf = liquidationLtv / loanToValue; hf<1 ⇒ liquidatable now). For obligations with
 * hf<1.1 it dumps the largest collateral + debt token, so we can eyeball whether the
 * seizable collateral is liquid (the AVGOx lesson).
 */
import { createSolanaRpc, address } from "@solana/kit";
import { KaminoMarket, PROGRAM_ID } from "@kamino-finance/klend-sdk";

const RPC_URL = process.env.RPC_URL ?? "https://api.mainnet-beta.solana.com";
const MARKET = process.argv[2];
const MAX_BATCHES = process.argv[3] ? Number(process.argv[3]) : Infinity; // batches of ~100
const SLOT_DURATION_MS = Number(process.env.KLEND_SLOT_DURATION_MS ?? 450);

if (!MARKET) {
  console.error("usage: tsx scripts/recon.ts <marketPubkey> [maxBatches]");
  process.exit(1);
}

const num = (x: unknown): number => {
  if (x == null) return 0;
  const m = x as { toNumber?: () => number };
  return typeof m.toNumber === "function" ? m.toNumber() : Number(x as never);
};

(async () => {
  const rpc = createSolanaRpc(RPC_URL);
  console.log(`Loading market ${MARKET} via ${new URL(RPC_URL).host} ...`);
  const market = await KaminoMarket.load(rpc as never, address(MARKET), SLOT_DURATION_MS, PROGRAM_ID);
  if (!market) throw new Error(`KaminoMarket.load returned null for ${MARKET}`);

  // mint → symbol map for the leg dump
  const symbolByMint = new Map<string, string>();
  for (const r of market.getReserves()) symbolByMint.set(String(r.getLiquidityMint()), r.getTokenSymbol());
  console.log(`Reserves: ${market.getReserves().length} — scanning obligations...\n`);

  const buckets = { lt1_0: 0, lt1_05: 0, lt1_1: 0, lt1_5: 0, gte1_5: 0, noDebt: 0 };
  let total = 0, withDebt = 0;
  const nearLiq: string[] = [];

  // Single bulk getProgramAccounts (one request) — avoids the per-batch 429s.
  const all = await market.getAllObligationsForMarket();
  for (const ob of all) {
    total++;
    const s = ob.refreshedStats;
    const ltv = num(s.loanToValue);
    const liqLtv = num(s.liquidationLtv);
    const borrow = num(s.userTotalBorrow);
    if (borrow <= 0 || ltv <= 0) { buckets.noDebt++; continue; }
    withDebt++;
    const hf = liqLtv / ltv; // <1 ⇒ liquidatable
    if (hf < 1.0) buckets.lt1_0++;
    else if (hf < 1.05) buckets.lt1_05++;
    else if (hf < 1.1) buckets.lt1_1++;
    else if (hf < 1.5) buckets.lt1_5++;
    else buckets.gte1_5++;

    if (hf < 1.1 && nearLiq.length < 30) {
      const top = (ps: { mintAddress: unknown; marketValueRefreshed: unknown }[]) =>
        ps.map((p) => ({ sym: symbolByMint.get(String(p.mintAddress)) ?? String(p.mintAddress).slice(0, 6), usd: num(p.marketValueRefreshed) }))
          .sort((a, b) => b.usd - a.usd)[0];
      const col = top(ob.getDeposits() as never[]);
      const debt = top(ob.getBorrows() as never[]);
      nearLiq.push(
        `  hf=${hf.toFixed(3)} ${String(ob.obligationAddress).slice(0, 8)}… ` +
        `collateral=${col?.sym}($${col?.usd.toFixed(0)}) debt=${debt?.sym}($${debt?.usd.toFixed(0)})`,
      );
    }
  }

  console.log(`\n===== ${MARKET} =====`);
  console.log(`total obligations:     ${total}`);
  console.log(`  with debt:           ${withDebt}`);
  console.log(`  no debt / collateral-only: ${buckets.noDebt}`);
  console.log(`health-factor histogram (debt-bearing only):`);
  console.log(`  hf < 1.00 (LIQUIDATABLE NOW): ${buckets.lt1_0}`);
  console.log(`  hf < 1.05:                    ${buckets.lt1_05}`);
  console.log(`  hf < 1.10:                    ${buckets.lt1_1}`);
  console.log(`  hf < 1.50:                    ${buckets.lt1_5}`);
  console.log(`  hf >= 1.50:                   ${buckets.gte1_5}`);
  if (nearLiq.length) {
    console.log(`\nnear-liquidation obligations (hf<1.1), largest legs:`);
    console.log(nearLiq.join("\n"));
  }
})().catch((e) => {
  console.error(`recon failed: ${e?.message ?? e}`);
  process.exit(1);
});
