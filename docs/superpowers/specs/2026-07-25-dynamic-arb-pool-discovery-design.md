# Dynamic arb pool discovery (offline periodic re-scan) — design

**Date:** 2026-07-25
**Status:** design approved, not yet implemented

## Problem

The arb bot's pool set is static. `pools.json` is generated offline by
`scripts/fetch_all.js` (Raydium/Orca top-N by liquidity, Meteora DLMM + PumpSwap from
hand-pinned address lists), loaded once at startup into `PoolRegistry`, and never changes
until someone re-runs the fetchers, re-runs `--init-alt`, and restarts. New liquid or
trending tokens never enter the arb graph, so the bot only ever hunts cycles among pools a
human curated at some point in the past.

Now that PumpSwap is a **tradeable** venue (Phase 2, validated 2026-07-25), graduated
pump.fun tokens can close real cycles (e.g. PumpSwap + Raydium), which materially widens
the arb surface — but only if such tokens can get *into* the book automatically.

**Goal:** a periodic, offline re-scan that discovers trending/liquid tokens, admits only
those that (a) pass security screening and (b) actually form executable cycles, fits them
into the gRPC feed's account budget, and refreshes the book — with **no changes to the arb
binary's hot path**.

## Constraints that shape the design

1. **Feed account budget is hard.** Free/shared Yellowstone tiers throttle large
   subscriptions (~200+ accounts) and starve the graph — this is the documented July-5
   root cause of phantom edges (`docs/`, `reduce_pools.js` header). Discovery must
   therefore **replace within a budget**, never grow unbounded.
2. **Flagship isolation.** The arb bot is the "admiral ship"; side features must not touch
   its hot path. All *discovery/selection logic* lives outside the binary; the only
   in-binary addition is a SIGHUP reload handler (startup/shutdown plumbing, no per-cycle
   work) — see Architecture for why that beats an external process supervisor here.
3. **Wash volume lies.** DexScreener/Birdeye volume includes wash trading; pools that look
   liquid can emit zero gRPC updates. Ranking must use real on-chain activity.
4. **Arb-specific token risk.** In a *cycle*, a freezable token or a Token-2022
   transfer-hook can trap capital between legs (honeypot) — a risk the momentum trader's
   pricing-only checks never face.
5. **Momentum escort must keep working.** The watcher prices its tokens from pricing-only
   PumpSwap entries in the same `pools.json`; a scan must not unwire them.

## What already exists (reuse, don't rebuild)

| Existing | Reused for |
|---|---|
| `scripts/scan_tokens.js` | trending/volume feed + stables/wrapped drop, volume & liquidity floors, anti-wash vol/liq ratio cap, Jupiter-verified gate, top-holder concentration cap (`SCAN_MAX_TOP_HOLDERS_PCT`) |
| `scripts/reduce_pools.js` | **cycle-closure prune** (non-hub token needs ≥2 venues; keep only the hub-connected component), **activity ranking** via `getSignaturesForAddress` on the subscribed account, venue-per-pair cap, LST exclusion |
| per-DEX fetchers (`fetch_pumpswap_pools.js`, `fetch_raydium_pools.js`, …) | decoding a pool address into the `PoolConfig` schema (vaults, `extra`, `coin_creator`, token programs, vault↔mint cross-check) |
| `merge_pools.js`, `create_atas.js`, `--init-alt` | writing the book, ATAs, ALT extension |

This feature is therefore mostly **orchestration + a multi-venue resolver**, not new
algorithms.

## Architecture

Two new scripts plus **one small, self-contained addition to the bot** (a SIGHUP
self-restart handler — see below). Nothing is added to the hot loop itself: no per-cycle
work, no new state in the evaluator/submitter path.

```
scripts/scan_arb_pools.js      # the brain: discover → screen → resolve → close → budget → decode → write
scripts/arb_refresh_loop.sh    # thin trigger: scan --apply → --init-alt-only → kill -HUP the bot
src/main.rs (small)            # SIGHUP → drain in-flight → exec() self (same PID, same terminal)
```

### Why SIGHUP self re-exec rather than an external restart

The operator runs the bot by hand in a terminal and watches its logs live. An external
supervisor would start the replacement **detached from that terminal**, killing the live
log view and re-parenting the bot to the refresh loop. A self re-exec
(`std::os::unix::process::CommandExt::exec`) replaces the process image in place: **same
PID, same terminal, same stdout**, and it works identically no matter how the bot was
launched (terminal now, systemd on a VPS later) — so the automation never needs to know
about process management.

Mechanics: on `SIGHUP`, set a flag; the main loop finishes/abandons nothing mid-flight but
**waits for the in-flight submission guard to clear** (bounded wait, then proceeds), then
`exec`s the same binary with the same args. Because `exec` replaces the image, there is no
fork, no orphan, and no double-subscription window.

**Accepted limitation:** a self-restarting process cannot recover from *its own* failed
startup — if the new book somehow fails to load, the bot exits and nothing restarts it.
The scanner's atomic pre-validation (below) makes that unlikely, and the optional
keep-alive loop (Out of scope → follow-on) closes it when desired.

**Also note:** every restart resets the in-memory `LATENCY` ring (which needs one
continuous run to reach n≥10 for a verdict) and any pending raw-RPC outcome monitors. This
is why the "no change ⇒ no restart" rule matters and why the cadence is hours, not minutes.

### Pipeline (`scan_arb_pools.js`)

1. **Discover** — trending/liquid candidate tokens from the Birdeye feed, with
   `scan_tokens.js`'s filters (drop stables/wrapped, volume floor, liquidity floor,
   anti-wash vol/liq ratio band, Jupiter-verified only, top-holder % cap).
2. **Security gate (arb-specific, on-chain per survivor)**
   - `freezeAuthority` **must be disabled** (else a leg can be frozen mid-cycle);
   - **no Token-2022 `TransferHook` extension** (hook can block the second leg = honeypot);
   - `mintAuthority` recorded in the report (not an automatic reject).
   Rejections are logged with the reason.
3. **Resolve venues** — for each survivor, enumerate pools across all supported venues
   (Raydium AMM v4/CLMM, Orca Whirlpool, Meteora DAMM/DLMM, PumpSwap) via DexScreener
   pairs, keeping each venue's best pool by 24h volume (existing convention).
4. **Cycle-closure prune** — hubs = **SOL + USDC**. Iterate to fixpoint: drop any non-hub
   token with <2 kept venues, then drop anything not in the hub-connected component
   (a removal can orphan a neighbour, so repeat). Consequence: a fresh pump token with
   only its PumpSwap pool is dropped **by construction**; a graduated one
   (PumpSwap + Raydium/Orca) is admitted and yields a real SOL→X→SOL cycle.
   **If `ENABLE_PUMPSWAP_TRADING` is off**, PumpSwap pools do not count as a tradeable
   venue for closure (they may still be written as pricing-only) — otherwise the book
   would promise cycles the bot cannot execute.
5. **Budget prune** — cap on **subscribed accounts** (not pool count), `ARB_ACCOUNT_BUDGET`
   default **200**:
   - **Protected core (always kept), defined concretely as:** (a) any pool whose token
     pair is SOL/USDC or SOL/USDT (the hub majors), (b) any pool address listed in a
     fetcher's pinned array (`TARGET_POOLS`, `DLMM_PINNED`, …), and (c) any pool
     referenced by a `pool` field in `assets/momentum_tokens.json`. Protected accounts
     count against the budget but are never evicted; if the core alone exceeds the budget,
     the scan aborts with a clear error (operator must raise the budget or trim pins);
   - **Discovery slots:** remainder filled by **on-chain activity rank**;
   - **venue-per-pair cap** so redundant majors can't crowd out cross-venue tokens;
   - **hysteresis:** a challenger must beat an incumbent by a margin to evict it, so the
     book doesn't thrash every scan (each churn costs an ALT extension + restart).
   - LST exclusion retained (SOL↔LST rate is the staking rate; can't clear multi-hop fees).
6. **Decode** — run the matching per-DEX decoder for each kept address to emit a full
   `PoolConfig` entry. Requires a small addition: a `--pools <addr,…>` override on the
   non-pump fetchers (only `fetch_pumpswap_pools.js` has one today). Any pool that fails
   decode or a vault↔mint cross-check is **skipped with a logged reason**, never
   force-merged.
7. **Write (atomic, validated)** — build into a temp file → validate → back up current →
   atomic rename. See safety below.

### Trigger loop (`arb_refresh_loop.sh`)

Periodic (default ~6 h, configurable): `scan_arb_pools.js --apply` → on "changed" exit
status: `--init-alt-only` (extend-and-exit — plain `--init-alt` also **starts the bot**,
which would spawn a duplicate process here) → `kill -HUP <bot pid>` (found via `pgrep -f
solana-mev`, or a PID file if one exists). Deliberately dumb: no filtering logic, no
knowledge of pools, and — thanks to self re-exec — **no process management**. If no bot
process is running, it logs that and does nothing (the book and ALT are still updated for
the next manual start).

**Exit-status contract** (the loop's only interface to the scanner):
`0` = book changed and written (proceed to ALT + HUP); `10` = no change (do nothing);
any other non-zero = failure (do nothing, alert).

## Safety model

- **Never write a broken book.** Validation before rename: schema-complete entries,
  per-DEX `check_extra` completeness, non-empty, protected core present. Failure ⇒ abort,
  keep old book, non-zero exit, **no restart**.
- **No-change ⇒ no-op.** If the new book is byte-identical to the current one, skip
  `--init-alt-only` and the restart entirely.
- **ALT is append-only.** Extend with new accounts; evicted pools simply leave dead ALT
  entries (harmless — an existing ALT is a superset). If `--init-alt-only` fails, **do
  not send the HUP**: old book + old ALT remain consistent and the bot keeps running the
  book it already loaded.
- **Restart is drain-gated, not health-gated.** The bot waits for the in-flight
  submission guard to clear before `exec`ing, so a refresh never interrupts a submission
  in progress. Rollback is *not* available from inside the process (see the accepted
  limitation above) — the defense is the scanner's pre-rename validation, which means the
  bot only ever HUPs onto a book that already passed schema + `check_extra` + core-present
  checks. The pre-refresh backup remains on disk for manual rollback.
- **Momentum isolation.** Watcher pricing pools are in the protected core.
- **DRY_RUN/report mode** (`--report`): prints the would-be diff (added / evicted / kept,
  resulting account count, per-rejection reasons) and writes nothing. This is how the
  first several scans are inspected before enabling automation.

### Failure modes

| Failure | Behavior |
|---|---|
| Birdeye/DexScreener rate-limit or outage | abort scan, keep current book (never a partial book) |
| Helius `-32429` during activity ranking | backoff/retry; if still failing, keep incumbent book |
| Pool decode / vault↔mint mismatch | skip that pool, log reason, continue |
| Validation failure | abort before rename; old book intact |
| `--init-alt-only` failure | no HUP sent; book+ALT stay consistent, bot keeps its loaded book |
| No bot process running | log and skip the HUP; book+ALT ready for next manual start |
| Bot exits on a bad book after HUP | not self-recoverable by design; backup book is on disk for manual restore, and the optional keep-alive loop covers it |
| SIGHUP arrives mid-submission | drain: wait for the in-flight guard to clear (bounded), then `exec` |

## Testing

Pure-logic units (Node, no network):

- **cycle-closure:** degree-1 drop; iterative orphan cascade; hub-connectivity filter;
  PumpSwap-only token dropped; graduated (2-venue) token kept; PumpSwap not counted as
  tradeable when the flag is off.
- **budget:** protected core always retained; account cap never exceeded; venue-per-pair
  cap enforced; hysteresis prevents churn on a marginal challenger.
- **security gate:** freeze-authority-enabled rejected; transfer-hook rejected; clean
  token accepted; reasons recorded.
- **validation:** malformed/incomplete entry aborts the write; byte-identical book ⇒
  no-op path.

Rust side (`cargo test --bin solana-mev`): the SIGHUP path is split so the decision is
pure and testable — a `restart_requested` flag + a `should_reexec(flag, in_flight)`
predicate (true only when requested AND not in-flight, or when the bounded drain wait has
elapsed). The `exec` call itself is not unit-tested; it is verified manually once by
HUPping a running bot and confirming same-PID reload with the new pool count.

Acceptance: run `--report` against live APIs and confirm the diff is sane (majors kept,
plausible discoveries, account count ≤ budget, rejections explained) before the first
`--apply`.

## Out of scope (explicit)

- Live hot-reload of the graph/ALT without restart (riskier Phase 2; requires touching the
  running money loop).
- Any change to the arb bot's **hot loop** (the SIGHUP handler + `exec` is startup/shutdown
  plumbing, not per-cycle work).

### Optional follow-on (not part of this spec)

A ~10-line **keep-alive loop** (`if ! pgrep -f solana-mev; then start it; fi` on a timer).
Independently valuable — today nothing notices or restarts the bot if it dies overnight —
and it is what closes the "bot exits on a bad book" gap that self re-exec cannot cover.
Deliberately separate so it can be adopted whenever, without coupling to discovery.
- Automatic tuning of the security thresholds (operator-set env knobs).
- New DEX integrations (only currently-supported venues are resolved).

## Env knobs (new)

| Var | Default | Purpose |
|---|---|---|
| `ARB_ACCOUNT_BUDGET` | `200` | Max subscribed accounts in the generated book |
| `ARB_SCAN_INTERVAL_SECS` | `21600` | Supervisor cadence (~6 h) |
| `ARB_SCAN_EVICT_MARGIN` | `1.25` | Hysteresis: challenger must beat incumbent activity by this factor |
| `ARB_ACTIVITY_WINDOW_SECS` | `300` | Activity-ranking window |
| `ARB_REEXEC_DRAIN_SECS` | `30` | Bounded wait for the in-flight submission guard to clear before `exec` on SIGHUP |
| (reused) `SCAN_MIN_VOLUME`, `SCAN_MIN_LIQUIDITY`, `SCAN_MIN_RATIO`, `SCAN_MAX_RATIO`, `SCAN_MAX_TOP_HOLDERS_PCT` | — | Discovery/security floors from `scan_tokens.js` |
