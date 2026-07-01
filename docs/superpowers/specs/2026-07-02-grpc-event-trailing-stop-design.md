# gRPC-driven event-based trailing stop with wick-confirmation — design

**Date:** 2026-07-02
**Status:** Approved (design).
**Scope:** Let the momentum trader's **exit** react to the sub-second on-chain (gRPC) price for held
tokens, with a dwell-based wick-confirmation guard. Opt-in, default-off. First gRPC feature that
changes a live trading *decision* — hence the conservative safety model. No entry changes.

## Problem

The momentum trader's exit (`maybe_exit`, `src/portfolio/momentum.rs`) runs on the fast
`MOMENTUM_POLL_SECS` ticker and **fetches held-token prices via REST** (`pricer::fetch_prices`) — it
does not use the gRPC feed at all. On a fast dump, a REST-lagged price trips the trailing stop *late*
and under-records the true peak, so the position gives back more than `trail%` before it exits. The
newly-shipped gRPC feed provides sub-second, quota-free, on-chain-truth prices for wired tokens, but
nothing routes them to the exit path.

Entry is a slow multi-hour signal — a fresher spot price does not improve it — so this feature targets
**exit only**.

## Goal

When enabled, exit-evaluate a held token the moment its gRPC price updates (event-driven), using the
fresh on-chain price, so the trailing stop trips promptly and tracks the true peak — while a
dwell-based wick-confirmation prevents a single-block on-chain price wick from whipsawing the position
out. Default-off; when off, the exit path is byte-identical to today. The fast poll ticker is retained
as a backstop so exits are never stranded if the gRPC stream stalls.

Non-goals: no entry changes; no change to REST exit when the flag is off; only wired (gRPC-priced) held
tokens get the fast path (others keep the REST ticker); dwell state is not persisted (a restart
re-evaluates fresh).

## Calibration (pre-implementation, throwaway analysis — not shipped)

Before building, analyze `assets/momentum_actions.jsonl`: for each `Exited`, compute give-back past the
stop level — `(peak·(1−trail%) − exit_price) / (peak·(trail%/100))` — grouped by `reason`. This
confirms the exit-lag give-back is real and sets the `MOMENTUM_STOP_CONFIRM_SECS` default. If give-back
is negligible, stop — the feature isn't worth the live-exit risk.

## Config (opt-in, default-off)

- `MOMENTUM_GRPC_EXIT: bool` ← `MOMENTUM_GRPC_EXIT` (default **`false`**). Master switch; requires
  `MOMENTUM_GRPC_PRICING` (no feed ⇒ no effect). Off ⇒ exit path byte-identical to today.
- `MOMENTUM_STOP_CONFIRM_SECS: u64` ← `MOMENTUM_STOP_CONFIRM_SECS` (default from calibration, ~3). Dwell
  window: price must stay below the stop this long before selling.

## Components / data flow

### 1. gRPC price into the exit path
`MomentumContext` gains `grpc_feed: Option<&GrpcFeed>`. In `maybe_exit`, per held mint: if the flag is
on AND a **fresh** gRPC price exists (within the feed's `momentum_grpc_stale_secs` window), use it;
otherwise the existing REST batch fetch (which now covers only the mints gRPC didn't price). Flag off ⇒
REST for all mints (today's path).

### 2. Wick-confirmation — pure predicate + in-memory dwell state
Pure fn (unit-tested):
```
enum ExitDecision { Sell, Arm, StayArmed, Disarm, Hold }
fn stop_decision(stop_hit: bool, armed_since: Option<Instant>, now: Instant, confirm_secs: u64) -> ExitDecision
```
- `stop_hit && armed_since.is_none()` → `Arm` (record now; do NOT sell).
- `stop_hit && armed && now − armed_since ≥ confirm_secs` → `Sell`.
- `stop_hit && armed && dwell not elapsed` → `StayArmed`.
- `!stop_hit && armed` → `Disarm` (price recovered above the stop — the wick reverted).
- else → `Hold`.

`stop_hit` is the existing `trailing_stop_hit(...)`/`trailing_stop_triggered(...)` predicate (unchanged).
The **peak still updates on every evaluation** regardless of arm state. When the flag is OFF, `maybe_exit`
uses the immediate sell-on-`stop_hit` behavior (today), with no dwell.

The watcher owns the dwell state: `HashMap<String /*mint*/, Instant /*armed_since*/>`. `maybe_exit`
reads it and returns per-position transitions (arm/disarm/sell); the watcher applies them. In-memory
only — a restart clears it and re-evaluates fresh (safe: a still-breached stop simply re-arms).

### 3. Event wiring (Notify)
`GrpcFeed` carries an `Arc<tokio::sync::Notify>`. The gRPC ingestion task, after updating a mint that is
in the **held set** (an `Arc<RwLock<HashSet<String>>>` refreshed each slow tick from
`state.positions`), calls `notify.notify_one()`. The watcher's `select!` gains an arm
`_ = feed.notify.notified() => { if MOMENTUM_GRPC_EXIT { maybe_exit(...) } }`. The existing fast
`fast_ticker` arm is **retained** and always runs `maybe_exit` (the backstop).

## Error handling / safety (load-bearing — this touches live sells)

- **Flag off** ⇒ byte-identical to today (no feed read, no dwell, no notify arm effect).
- **gRPC stall/disconnect** ⇒ no notifies, but the 1s `fast_ticker` still runs `maybe_exit` with REST
  fallback ⇒ exits never stranded. This is why the ticker is retained.
- **Stale gRPC price** ⇒ that mint uses REST.
- **Dwell shared across event + ticker eval** ⇒ if gRPC stops mid-dwell, the ticker re-evaluates with a
  REST price and the same `stop_decision` applies; dwell never delays a sell beyond `confirm_secs` of
  sustained breach.
- **No double-sell** ⇒ single exit path (`maybe_exit`); the watcher's `select!` serializes the ticker
  and event arms (never concurrent); state file saved atomically as today.
- **Halt semantics unchanged** ⇒ `maybe_exit` remains un-gated on `halted()` (a halted bot must still
  exit); only the price source + dwell change.

## Testing

- **`stop_decision` (pure):** arm-on-first-breach (no sell), sell-after-dwell-elapsed, stay-armed-before-
  dwell, disarm-on-recovery, hold-when-above; and the flag-off immediate-sell equivalence.
- **Exit price-source selection (pure/unit):** fresh gRPC preferred; stale/missing → REST fallback.
- **Notify/event wiring:** exercised by the paper smoke (not unit-testable).
- **Paper smoke (operator):** `DRY_RUN_MOMENTUM_TRADER=true` + `MOMENTUM_GRPC_EXIT=true` + a wired held
  position (e.g. adopt SLX) → logs show event-driven evaluation + arm/dwell/disarm, and NO live sells.

## Affected files

- `src/portfolio/mod.rs` — two config fields.
- `src/portfolio/grpc_pricer.rs` — `Notify` on `GrpcFeed`; held-set plumbing for notify-on-held-update.
- `src/bin/portfolio_watcher.rs` — the gRPC ingestion task fires the notify; the watcher `select!` adds
  the event arm + owns the dwell-state map + held-set refresh.
- `src/portfolio/momentum.rs` — `stop_decision` pure fn + tests; `MomentumContext.grpc_feed`;
  `maybe_exit` price-source preference + flag-switched dwell logic.
- `.env.example` — document the two vars + the paper-first guidance.
- No changes to `src/arbitrage/`, `src/graph/`, `src/streamer/`, `src/dex/`, `src/main.rs`, or the
  momentum ENTRY path.

## Autonomy / risk note

This is the first feature to alter the live exit decision. It is default-off, dwell-guarded, backed by
the retained poll ticker, and must be paper-tested (`DRY_RUN_MOMENTUM_TRADER`) before any live enable.
The calibration step gates whether it's even worth building.
