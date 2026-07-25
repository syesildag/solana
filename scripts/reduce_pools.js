#!/usr/bin/env node
/*
 * reduce_pools.js — shrink pools.json to an ACTIVE liquid core that fits inside
 * a free/shared gRPC feed's account budget, while preserving arbitrage cycles.
 *
 * WHY: free Yellowstone tiers throttle large (200+ account) subscriptions,
 * starving the graph. A focused book of genuinely-active pools fits under the
 * cap and stays fresh.
 *
 * RANKING: by real on-chain transaction frequency (getSignaturesForAddress on
 * each pool's subscribed account — state_account for CL pools, vault_a for
 * AMM/DAMM), NOT DexScreener volume — DexScreener volume includes wash/fake
 * volume, so it picks pools that look liquid but never produce gRPC updates.
 * Txns on the subscribed account are exactly what the feed streams to us.
 *
 * SELECTION: keep the top pools by activity, then prune until every NON-HUB
 * token has >=2 kept venues (a degree-1 token is a dead-end — no cycle) and
 * only the USDC-connected component remains. pump_swap preserved for the watcher.
 *
 * USAGE:
 *   node scripts/reduce_pools.js               # report only
 *   node scripts/reduce_pools.js --apply       # back up + overwrite pools.json
 *   TARGET_POOLS=24 WINDOW_SECS=300 node scripts/reduce_pools.js
 *
 * NOTE: pools.json is auto-generated — this is a TEST filter. `fetch_all.js`
 * regenerates the FULL book (and wipes this). For a permanent cut, fold an
 * activity/volume floor into merge_pools.js.
 */
'use strict';
const fs = require('fs');
const path = require('path');

const POOLS_PATH = path.join(__dirname, '..', 'pools.json');
const REDUCED_PATH = path.join(__dirname, '..', 'pools.reduced.json');
const APPLY = process.argv.includes('--apply');
const TARGET = parseInt(process.env.TARGET_POOLS || '24', 10);      // candidate ceiling before pruning
const WINDOW = parseInt(process.env.WINDOW_SECS || '300', 10);      // activity window (s): txns in last N s
const BACKUP_DIR = process.env.SCRATCHPAD ||
  '/private/tmp/claude-501/-Users-serkan-Workspace-solana/e0865e7b-6c65-4df5-a5a6-e1ebf198c839/scratchpad';

// Pegged assets to EXCLUDE from the arb book: liquid staking tokens track a
// slow-moving SOL peg (their SOL↔LST rate IS the staking exchange rate), so
// cross-venue cycles through them can't clear multi-hop fees — dead weight that
// wastes the feed's account budget. They rank high on activity but have no edge.
const DENY_MINTS = new Set([
  'J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn', // jitoSOL
  'mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So',  // mSOL
  'bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1',  // bSOL
  'jupSoLaHXQiZZTSfEWMTRRgpnyFm8f6sZdosWBjx93v',  // JupSOL
  '5oVNBeEEQvYi1cX3ir8Dx5n1P7pdxydbGF2X4TxVusJm', // INF (Sanctum Infinity)
  'BonK1YhkXEGLZzwtcvRTip3gAL9nCeQD7ppZBLXhtTs',  // bonkSOL
]);
const USDC = 'EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v';
const SOL = 'So11111111111111111111111111111111111111112';
const HUBS = new Set([
  USDC,
  'So11111111111111111111111111111111111111112',  // SOL
  'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB',  // USDT
]);
const SYM = {
  [USDC]: 'USDC',
  'So11111111111111111111111111111111111111112': 'SOL',
  'Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB': 'USDT',
};
const short = (m) => SYM[m] || (m.slice(0, 4) + '…');
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function rpcUrl() {
  if (process.env.RPC_URL) return process.env.RPC_URL;
  const env = fs.readFileSync(path.join(__dirname, '..', '.env'), 'utf8');
  const m = env.match(/^\s*RPC_URL\s*=\s*(.+)\s*$/m);
  if (!m) throw new Error('RPC_URL not set (env or .env)');
  return m[1].trim();
}

async function rpc(url, method, params) {
  const res = await fetch(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }),
  });
  const j = await res.json();
  if (j.error) throw new Error(j.error.message || JSON.stringify(j.error));
  return j.result;
}

// Recent on-chain txn count on the account we actually subscribe to — the
// direct predictor of how often this pool will produce gRPC updates.
async function fetchActivity(pools, url) {
  const now = Math.floor(Date.now() / 1000);
  const act = new Map();
  let done = 0, failed = 0;
  for (const p of pools) {
    // Measure the account the streamer actually subscribes to per DEX:
    //   CL pools (Orca/Raydium CLMM/DLMM) → state_account
    //   Meteora DAMM → a_vault_lp (lp_index; vault_a is NOT subscribed for DAMM)
    //   Raydium AMM v4 → vault_a
    const acct = p.state_account
      || (p.dex === 'meteora_damm' && p.extra ? p.extra.a_vault_lp : null)
      || p.vault_a;
    let attempts = 0, ok = false;
    for (;;) {
      try {
        const sigs = await rpc(url, 'getSignaturesForAddress', [acct, { limit: 100 }]);
        act.set(p.id, sigs.filter((s) => s.blockTime && s.blockTime >= now - WINDOW).length);
        ok = true;
        break;
      } catch (e) {
        // Retry ANY error (rate-limit -32429/-32005, timeouts) with backoff so a
        // transient failure never silently drops a pool from the active set.
        if (attempts++ < 4) { await sleep(400 * attempts); continue; }
        act.set(p.id, -1); // unknown after retries — sorts below real zeros
        failed++;
        console.error(`  sig fetch failed ${p.id.slice(0, 8)}: ${e.message}`);
        break;
      }
    }
    if (++done % 20 === 0) console.error(`  ...${done}/${pools.length} probed`);
    await sleep(ok ? 350 : 0);
  }
  if (failed) console.error(`  ⚠️ ${failed} pools failed probing (marked unknown) — ranking may be off`);
  return act;
}

// Greedy diversity selection: fill the feed's account budget with the most-
// active pools, but cap venues-per-pair so redundant (and arb-efficient) major
// pairs like SOL/USDC don't crowd out volatile cross-venue tokens where real
// inefficiencies live. Skips probe-failed/dead pools (_act <= 0).
function selectDiverse(ranked) {
  const MAX_VENUES = parseInt(process.env.MAX_VENUES_PER_PAIR || '2', 10);
  const BUDGET = parseInt(process.env.ACCT_BUDGET || '30', 10);
  const pairKey = (p) => [p.token_a, p.token_b].slice().sort().join('/');
  const acctsOf = (p) => {
    const a = [];
    for (const k of ['vault_a', 'vault_b', 'state_account']) if (p[k]) a.push(p[k]);
    const ex = p.extra || {};
    for (const k of ['a_vault_lp', 'b_vault_lp']) if (ex[k]) a.push(ex[k]);
    return a;
  };
  // Only spend budget on cycle-able pools: a non-hub token must have >=2 venues
  // somewhere in the active set, else it's a guaranteed dead-end that would just
  // be pruned later (wasting the account budget on it).
  const active = ranked.filter((p) => p._act > 0);
  const gdeg = new Map();
  for (const p of active) for (const t of [p.token_a, p.token_b]) gdeg.set(t, (gdeg.get(t) || 0) + 1);
  const cycleable = (p) => [p.token_a, p.token_b].every((t) => HUBS.has(t) || (gdeg.get(t) || 0) >= 2);

  // Hub-hub pairs (SOL/USDC, SOL/USDT, USDC/USDT) are arb-efficient — spreads
  // < fees, no edge (proven: -2bps). Keep only HUB_CAP of them for base
  // USDC↔SOL plumbing; spend the rest of the budget on volatile cross-venue
  // tokens where inefficiencies actually live. SOL/USDC is force-first so USDC
  // is always connected.
  const HUB_CAP = parseInt(process.env.HUB_PAIR_CAP || '2', 10);
  const isHubHub = (p) => HUBS.has(p.token_a) && HUBS.has(p.token_b);
  const isSolUsdc = (p) => isHubHub(p) &&
    [p.token_a, p.token_b].includes(USDC) &&
    [p.token_a, p.token_b].includes('So11111111111111111111111111111111111111112');
  // Process SOL/USDC first (guarantees base connectivity), then everything else.
  const order = [...active.filter(isSolUsdc), ...active.filter((p) => !isSolUsdc(p))];

  const venue = new Map(); const accts = new Set(); const kept = []; let hubKept = 0;
  for (const p of order) {
    if (!cycleable(p)) continue;
    if (isHubHub(p) && hubKept >= HUB_CAP) continue;
    const key = pairKey(p);
    if ((venue.get(key) || 0) >= MAX_VENUES) continue;
    const add = acctsOf(p).filter((a) => !accts.has(a));
    if (accts.size + add.length > BUDGET) continue;
    kept.push(p); venue.set(key, (venue.get(key) || 0) + 1);
    if (isHubHub(p)) hubKept++;
    for (const a of add) accts.add(a);
  }
  return kept;
}

function pruneToCycles(pools) {
  let kept = pools.slice();
  for (;;) {
    const deg = new Map();
    for (const p of kept) for (const t of [p.token_a, p.token_b]) deg.set(t, (deg.get(t) || 0) + 1);
    const deadEnds = new Set([...deg].filter(([t, d]) => d < 2 && !HUBS.has(t)).map(([t]) => t));
    if (deadEnds.size === 0) break;
    const victimIdx = kept
      .map((p, i) => [i, p])
      .filter(([, p]) => deadEnds.has(p.token_a) || deadEnds.has(p.token_b))
      .sort((a, b) => a[1]._act - b[1]._act)[0][0]; // drop least-active offender
    kept.splice(victimIdx, 1);
  }
  const adj = new Map();
  const add = (a, b) => { (adj.get(a) || adj.set(a, []).get(a)).push(b); };
  for (const p of kept) { add(p.token_a, p.token_b); add(p.token_b, p.token_a); }
  // Keep only the component reachable from a hub. Seed from EVERY hub present: the arb
  // base is SOL, so a SOL-only component is legitimate and must not be discarded.
  const seen = new Set();
  const stack = [];
  for (const h of HUBS) if (adj.has(h)) { seen.add(h); stack.push(h); }
  while (stack.length) {
    for (const n of adj.get(stack.pop()) || []) if (!seen.has(n)) { seen.add(n); stack.push(n); }
  }
  return kept.filter((p) => seen.has(p.token_a) && seen.has(p.token_b));
}

(async () => {
  const raw = JSON.parse(fs.readFileSync(POOLS_PATH, 'utf8'));
  const all = Array.isArray(raw) ? raw : raw.pools;
  const pumpPools = all.filter((p) => p.dex === 'pump_swap'); // preserved for the watcher
  const isPegged = (p) => DENY_MINTS.has(p.token_a) || DENY_MINTS.has(p.token_b);
  const pegged = all.filter((p) => p.dex !== 'pump_swap' && isPegged(p));
  const pools = all.filter((p) => p.dex !== 'pump_swap' && !isPegged(p));
  console.log(`Read ${all.length} pools (${pools.length} arb-tradeable, ` +
    `${pegged.length} pegged-LST excluded, ${pumpPools.length} pump preserved). ` +
    `Probing on-chain activity (txns in last ${WINDOW}s)…`);

  const act = await fetchActivity(pools, rpcUrl());
  for (const p of pools) p._act = act.get(p.id) || 0;
  const ranked = pools.slice().sort((a, b) => b._act - a._act);
  const kept = pruneToCycles(selectDiverse(ranked));

  console.log(`\nKEPT ${kept.length} pools (activity-ranked, ≤${process.env.MAX_VENUES_PER_PAIR || 2}/pair, ` +
    `≤${process.env.ACCT_BUDGET || 30} accts), dropping ${pools.length - kept.length}:\n`);
  for (const p of kept.slice().sort((a, b) => b._act - a._act)) {
    console.log(`  ${short(p.token_a).padEnd(6)}/${short(p.token_b).padEnd(8)} ` +
      `${p.dex.padEnd(16)} ${String(p._act).padStart(4)} tx/${WINDOW}s  ${p.id.slice(0, 8)}`);
  }
  // Show the highest-activity pools we DROPPED (sanity: should be dupes/dead-ends only)
  const dropped = ranked.filter((p) => !kept.includes(p)).slice(0, 5);
  if (dropped.some((p) => p._act > 0)) {
    console.log(`\n  (dropped despite activity — dead-ends/over-ceiling:`);
    for (const p of dropped) if (p._act > 0)
      console.log(`     ${short(p.token_a)}/${short(p.token_b)} ${p._act} tx  ${p.id.slice(0, 8)}`);
    console.log(`  )`);
  }
  console.log(`\nSubscribed accounts: ${countAccounts(pools)} → ${countAccounts(kept)}`);

  const outPools = [...kept.map(strip), ...pumpPools];
  const out = Array.isArray(raw) ? outPools : { ...raw, pools: outPools };
  fs.writeFileSync(REDUCED_PATH, JSON.stringify(out, null, 2));
  console.log(`Wrote ${REDUCED_PATH}`);

  if (APPLY) {
    fs.mkdirSync(BACKUP_DIR, { recursive: true });
    const bak = path.join(BACKUP_DIR, 'pools.full.json');
    if (!fs.existsSync(bak)) fs.copyFileSync(POOLS_PATH, bak); // don't clobber the full-book backup
    fs.writeFileSync(POOLS_PATH, JSON.stringify(out, null, 2));
    console.log(`APPLIED: pools.json overwritten. Full-book backup: ${bak}`);
    console.log(`Restore: cp ${bak} ${POOLS_PATH}   (or: node scripts/fetch_all.js)`);
  } else {
    console.log(`\nReport only. Re-run with --apply to back up + overwrite pools.json.`);
  }
})();

function strip(p) { const { _act, _vol, ...rest } = p; return rest; }
function countAccounts(pools, opts = {}) {
  const countPumpSwap = opts.countPumpSwap === true;
  const s = new Set();
  for (const p of pools) {
    // PumpSwap vaults are only subscribed when the venue is tradeable
    // (ENABLE_PUMPSWAP_TRADING); otherwise they are pricing-only for the watcher.
    if (p.dex === 'pump_swap' && !countPumpSwap) continue;
    for (const k of ['vault_a', 'vault_b', 'state_account']) if (p[k]) s.add(p[k]);
    const ex = p.extra || {};
    for (const k of ['a_vault_lp', 'b_vault_lp']) if (ex[k]) s.add(ex[k]);
  }
  return s.size;
}

module.exports = { pruneToCycles, countAccounts, selectDiverse, HUBS, USDC, SOL };
