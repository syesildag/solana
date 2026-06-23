/**
 * klend-builder — Phase 2b.2 sidecar (build-vs-buy decision = BUY).
 *
 * The Rust pairs trader calls these endpoints to (a) read klend market/obligation
 * state and (b) build Kamino lending instructions; the bot then signs + submits.
 * We use the official @kamino-finance/klend-sdk so the SDK derives every account,
 * PDA and refresh ordering — hand-rolling those long, version-specific account
 * lists in Rust was rejected (see the plan's build-vs-buy note).
 *
 * The SDK is @solana/kit-native (web3.js v2): Address (not PublicKey), Rpc (not
 * Connection), bigint Slot, Option<Address> referrer. The "build unsigned, sign in
 * Rust" trick is `createNoopSigner(owner)` — it marks the owner as a required
 * signer in the produced instructions without holding a key; the bot supplies the
 * real signature. Instructions are returned as JSON (the bot assembles the tx,
 * merges ALTs and submits — same as it does for Jupiter's /swap-instructions).
 *
 * API surface TYPE-CONFIRMED against the installed klend-sdk@7.3.22: `npm run typecheck`
 * (tsc --noEmit) passes, and the committed package-lock.json pins that exact tree. So
 * every builder arg order + accessor below compiles against the real SDK types. What
 * remains are RUNTIME facts a type-check can't see (flagged `VERIFY:`): APY units
 * (fraction vs %), whether `getUserVanillaObligation` throws on a missing obligation,
 * and — the real one — whether the SDK's built instructions actually land on-chain.
 * Those need a live RPC + funded wallet (Phase 2b.3).
 */
import express from "express";
import {
  address,
  createSolanaRpc,
  createNoopSigner,
  isSignerRole,
  isWritableRole,
  none,
  type Address,
  type Instruction,
} from "@solana/kit";
import {
  KaminoMarket,
  KaminoAction,
  VanillaObligation,
  PROGRAM_ID,
} from "@kamino-finance/klend-sdk";

const PORT = Number(process.env.KLEND_BUILDER_PORT ?? 8181);
const RPC_URL = process.env.RPC_URL ?? "https://api.mainnet-beta.solana.com";
const MARKET_STR = process.env.KLEND_MARKET ?? "";
// Mainnet slot duration ≈ 450 ms; required 3rd positional arg to KaminoMarket.load.
const SLOT_DURATION_MS = Number(process.env.KLEND_SLOT_DURATION_MS ?? 450);
// klend builders fire ATA/obligation/LUT init ixs on first use; keep false so a
// fresh wallet self-initializes. Set both true once the obligation + LUT exist.
const INIT_USER_METADATA = { skipInitialization: false, skipLutCreation: false };
const EXTRA_CU = 1_000_000;

const rpc = createSolanaRpc(RPC_URL);

/** Decimal | bigint | number | null → number | null (klend returns decimal.js). */
function num(x: unknown): number | null {
  if (x == null) return null;
  if (typeof x === "number") return x;
  if (typeof x === "bigint") return Number(x);
  const maybe = x as { toNumber?: () => number };
  return typeof maybe.toNumber === "function" ? maybe.toNumber() : Number(x as never);
}

async function loadMarket(): Promise<KaminoMarket> {
  if (!MARKET_STR) throw new Error("KLEND_MARKET env not set");
  // load(rpc, marketAddress, recentSlotDurationMs, programId?, withReserves=true).
  // createSolanaRpc returns a superset of the api KaminoMarket needs.
  const market = await KaminoMarket.load(
    rpc as never,
    address(MARKET_STR),
    SLOT_DURATION_MS,
    PROGRAM_ID,
  );
  if (!market) throw new Error(`KaminoMarket.load returned null for ${MARKET_STR}`);
  return market;
}

/** kit Instruction → Rust-friendly JSON the bot deserializes into solana_sdk::Instruction. */
function ixToJson(ix: Instruction) {
  return {
    programId: ix.programAddress,
    accounts: [...(ix.accounts ?? [])].map((a) => ({
      pubkey: a.address,
      isSigner: isSignerRole(a.role),
      isWritable: isWritableRole(a.role),
    })),
    data: Buffer.from(ix.data ?? new Uint8Array()).toString("base64"),
  };
}

/** All instruction groups of a built action, in execution order. */
function actionToJson(act: KaminoAction) {
  return {
    computeBudgetIxs: act.computeBudgetIxs.map(ixToJson),
    setupIxs: act.setupIxs.map(ixToJson),
    inBetweenIxs: act.inBetweenIxs.map(ixToJson),
    lendingIxs: act.lendingIxs.map(ixToJson),
    cleanupIxs: act.cleanupIxs.map(ixToJson),
  };
}

const app = express();
app.use(express.json());

app.get("/health", (_req, res) => res.json({ ok: true, market: MARKET_STR, rpc: RPC_URL }));

/** GET /market → per-reserve borrow APY, liquidation threshold, available liquidity. */
app.get("/market", async (_req, res) => {
  try {
    const market = await loadMarket();
    const slot = await rpc.getSlot().send(); // bigint Slot
    const reserves: Record<string, unknown> = {};
    for (const r of market.getReserves()) {
      reserves[r.getTokenSymbol()] = {
        address: r.address, // the reserve account pubkey
        mint: r.getLiquidityMint(),
        // VERIFY units: totalBorrowAPY is a fraction (0.30 = 30%) — bot multiplies ×100.
        borrowApy: num(r.totalBorrowAPY(slot)),
        // stats.liquidationThreshold is a fraction 0–1 (= config.liquidationThresholdPct/100).
        liqThreshold: num(r.stats?.liquidationThreshold),
        // getLiquidityAvailableAmount is RAW base units; decimals lets the bot scale.
        availableLiquidityRaw: num(r.getLiquidityAvailableAmount()),
        decimals: num(r.stats?.decimals),
      };
    }
    res.json({ market: MARKET_STR, reserves });
  } catch (e) {
    res.status(500).json({ error: String(e) });
  }
});

/** GET /obligation?owner=<pubkey> → the user's vanilla-obligation health numbers. */
app.get("/obligation", async (req, res) => {
  try {
    const owner = address(String(req.query.owner));
    const market = await loadMarket();
    let ob: Awaited<ReturnType<KaminoMarket["getUserVanillaObligation"]>> | null = null;
    try {
      ob = await market.getUserVanillaObligation(owner);
    } catch {
      ob = null; // no obligation yet
    }
    if (!ob) return res.json({ exists: false });
    const s = ob.refreshedStats; // NB: refreshedStats, not `.stats`
    res.json({
      exists: true,
      address: ob.obligationAddress,
      userTotalDeposit: num(s.userTotalDeposit),
      userTotalBorrow: num(s.userTotalBorrow),
      borrowLimit: num(s.borrowLimit),
      loanToValue: num(s.loanToValue),
      liquidationLtv: num(s.liquidationLtv),
      netAccountValue: num(s.netAccountValue),
    });
  } catch (e) {
    res.status(500).json({ error: String(e) });
  }
});

/**
 * POST /build/:action  { owner, symbol, amount }  →  grouped instruction JSON.
 * action ∈ deposit | borrow | repay | withdraw. `amount` is a string in RAW base
 * units (lamports of the token). The bot flattens setup→inBetween→lending→cleanup,
 * adds its own compute budget, signs as `owner`, and submits.
 */
app.post("/build/:action", async (req, res) => {
  const { action } = req.params;
  const { owner, symbol, amount } = (req.body ?? {}) as {
    owner?: string;
    symbol?: string;
    amount?: string | number;
  };
  try {
    if (!owner || !symbol || amount == null) {
      return res.status(400).json({ error: "owner, symbol, amount are required" });
    }
    const market = await loadMarket();
    const reserve = market.getReserveBySymbol(symbol);
    if (!reserve) return res.status(400).json({ error: `no reserve for symbol '${symbol}'` });
    const mint: Address = reserve.getLiquidityMint();
    const ownerSigner = createNoopSigner(address(owner)); // build unsigned; bot signs
    const obligation = new VanillaObligation(PROGRAM_ID);
    const amt = String(amount);
    const slot = await rpc.getSlot().send();
    const useV2Ixs = true;
    const scopeRefresh = undefined;

    // Arg order TYPE-CONFIRMED against installed klend-sdk@7.3.22 (tsc --noEmit clean);
    // on-chain correctness still pending a live run (2b.3):
    //   deposit/borrow/withdraw: (market, amount, mint, owner, obligation, useV2Ixs,
    //       scopeRefresh, extraCU, includeAtaIxs, requestElevationGroup, initUserMetadata,
    //       referrer, currentSlot)
    //   repay:  currentSlot is a REQUIRED positional BEFORE payer:
    //       (market, amount, mint, owner, obligation, useV2Ixs, scopeRefresh, currentSlot,
    //        payer, extraCU, includeAtaIxs, requestElevationGroup, initUserMetadata, referrer)
    let act: KaminoAction;
    switch (action) {
      case "deposit":
        act = await KaminoAction.buildDepositTxns(
          market, amt, mint, ownerSigner, obligation, useV2Ixs, scopeRefresh,
          EXTRA_CU, true, false, INIT_USER_METADATA, none(), slot,
        );
        break;
      case "borrow":
        act = await KaminoAction.buildBorrowTxns(
          market, amt, mint, ownerSigner, obligation, useV2Ixs, scopeRefresh,
          EXTRA_CU, true, false, INIT_USER_METADATA, none(), slot,
        );
        break;
      case "repay":
        act = await KaminoAction.buildRepayTxns(
          market, amt, mint, ownerSigner, obligation, useV2Ixs, scopeRefresh,
          slot, ownerSigner, EXTRA_CU, true, false, INIT_USER_METADATA, none(),
        );
        break;
      case "withdraw":
        act = await KaminoAction.buildWithdrawTxns(
          market, amt, mint, ownerSigner, obligation, useV2Ixs, scopeRefresh,
          EXTRA_CU, true, false, INIT_USER_METADATA, none(), slot,
        );
        break;
      default:
        return res.status(400).json({ error: `unknown action '${action}'` });
    }
    res.json({ action, symbol, mint, amount: amt, ...actionToJson(act) });
  } catch (e) {
    res.status(500).json({ error: String(e) });
  }
});

app.listen(PORT, () => {
  console.log(`klend-builder on :${PORT} — market ${MARKET_STR || "(unset)"}, rpc ${RPC_URL}`);
  if (!MARKET_STR) console.warn("⚠️ KLEND_MARKET not set — set it to the xStocks lending market pubkey.");
});
