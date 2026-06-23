# Pairs trader — operations runbook

Operating guide for the market-neutral xStocks pairs trader. Companion to the design
reference [pairs-trader.md](pairs-trader.md) and the build plan
[superpowers/plans/2026-06-21-onchain-pairs-trader.md](superpowers/plans/2026-06-21-onchain-pairs-trader.md).

> **Status (2026-06-23).** Phases **2a–2c + the 2d risk layer are built and paper-safe**:
> signal, paper open/close orchestration, the klend sidecar + borrowability/APY/health
> gate, and the risk layer (halt kill-switch + live-only loss breaker). **Live on-chain
> execution (2d.2) is NOT wired yet** — `open_pair`/`close_pair` `bail!` when
> `DRY_RUN_PAIRS_TRADER=false`. Keep it in paper until the funded 2b.3 proof passes.

---

## 1. What runs where

| Component | What it is | How to run |
|---|---|---|
| **portfolio-watcher** | The bot process that ticks the momentum **and** pairs traders off the shared price loop. | `cargo run --release --bin portfolio-watcher` |
| **klend-builder** | Node sidecar that reads Kamino market/obligation state and builds klend instructions. Needed only when the gate is enabled. | `cd klend-builder && npm install && npm start` |
| **momentum-sim** | Backtest harness — used here to re-validate the pairs edge as history grows. | `cargo run --release --bin momentum-sim -- run --strategy pairs …` |

**Files the pairs trader owns** (paths are the `PAIRS_*` defaults):

| File | Purpose |
|---|---|
| `assets/pairs.json` | The pair list (`{symbol_a,mint_a,symbol_b,mint_b}`). |
| `assets/pairs_state.json` | Current open position + per-pair cooldowns + **closed-trade log (the P&L record)**. |
| `assets/pairs_halt.json` | Halt marker. Present ⇒ no new opens. Shared format with the momentum halt file. |

---

## 2. Configuration reference (`PAIRS_*`)

| Env var | Default | Meaning |
|---|---|---|
| `ENABLE_PAIRS_TRADER` | `false` | Master switch. |
| `DRY_RUN_PAIRS_TRADER` | `true` | Paper mode. **Leave `true` until 2d.2 lands** (live `bail!`s today). |
| `PAIRS_PATH` | `assets/pairs.json` | Pair list. |
| `PAIRS_LOOKBACK_OBS` | `240` | z-score window over `ln(A/B)`. |
| `PAIRS_Z_ENTRY` / `_Z_EXIT` / `_Z_STOP` | `2.0 / 0.5 / 4.5` | Open at \|z\|≥entry (and <stop); close at \|z\|≤exit or ≥stop. |
| `PAIRS_TRADE_USDC` | `50` | Notional per leg (dollar-neutral). **Start tiny when going live.** |
| `PAIRS_REENTRY_COOLDOWN_SECS` | `3600` | Per-pair cooldown after a close. |
| `PAIRS_MAX_TRADES_PER_DAY` | `6` | Daily open cap (rolling 24 h). |
| `PAIRS_MAX_BORROW_APY_PCT` | `30` | Reject opens whose short-leg borrow APY exceeds this. |
| `PAIRS_MIN_HEALTH_FACTOR` | `1.5` | Floor for the cross-margin health gate (and the live de-risk monitor). |
| `PAIRS_MAX_LOSS_USDC` | `0` | Loss circuit breaker. `0` = off. **LIVE only** — paper never halts. |
| `PAIRS_SLIPPAGE_BPS` | `50` | Per-leg slippage cap. |
| `PAIRS_KLEND_SIDECAR_URL` | *(empty)* | Sidecar URL. Empty ⇒ borrowability/APY/health gate **disabled** (pure paper). Set to enforce it. |
| `PAIRS_KLEND_BUILDER_DIR` | *(empty)* | Set ⇒ watcher auto-launches the sidecar from this dir at startup + stops it at exit (and defaults the URL). Unset ⇒ run the sidecar yourself. |

**The xStocks market (resolved + verified on-chain):**
- `KLEND_MARKET = 5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua` ("xStocks Market", **not**
  the Main Market). Market ALT `8ofreL6hKfEet1DnhHVGvCTnSdz4pg85PpbuCUHnEcKm`.
- All four xStocks **and** USDC are reserves in this one market → cross-margin works.

---

## 3. Running it (paper, today)

One-time: `cd klend-builder && npm install` (the committed lockfile pins farms-sdk 3.2.24).
Then pick **A** (auto-launch) or **B** (manual).

**A — auto-launch the sidecar (recommended).** Set `PAIRS_KLEND_BUILDER_DIR` and the watcher
spawns the sidecar at startup (pointed at the bot's `RPC_URL` + the xStocks market) and stops
it at exit — one process to run:

```bash
ENABLE_PAIRS_TRADER=true \
DRY_RUN_PAIRS_TRADER=true \
PAIRS_KLEND_BUILDER_DIR=./klend-builder \
cargo run --release --bin portfolio-watcher
# logs: "Launched klend-builder sidecar … on :8181" then "klend-builder ready"
```
(`PAIRS_KLEND_SIDECAR_URL` defaults to `http://127.0.0.1:8181` when the dir is set.)

**B — run the sidecar yourself** (leave `PAIRS_KLEND_BUILDER_DIR` unset):
```bash
# shell 1
cd klend-builder && RPC_URL="<your-rpc>" KLEND_MARKET="5wJeMrUYECGq41fxRESKALVcHnNX26TAWy4W98yULsua" npm start
# shell 2
ENABLE_PAIRS_TRADER=true DRY_RUN_PAIRS_TRADER=true \
PAIRS_KLEND_SIDECAR_URL=http://127.0.0.1:8181 \
cargo run --release --bin portfolio-watcher
```

**C — pure paper, no gate** (leave both unset): runs without the borrowability/APY/health
gate — fine for signal-only observation, but it will **not** block a "short GOOGLx" open.

Gate behavior:
- `PAIRS_KLEND_SIDECAR_URL` set ⇒ each tick fetches live reserves and runs the gate before a
  paper open; a blocked open logs e.g. `skip NVDAx/GOOGLx — preflight ShortNotBorrowable("GOOGLx")`.
- **Gate-on + sidecar unreachable ⇒ fail-safe:** no opens that tick (logged as a warning).

> **Shutdown is fail-closed (live).** On **Ctrl-C or SIGTERM** (`systemctl stop` /
> supervisors) the watcher stops the auto-launched sidecar and, **in live mode**, halts the
> pairs trader (writes the halt file) — a restart won't auto-resume real opening until you
> `rm assets/pairs_halt.json`. **Paper auto-resumes** (no halt written); the sidecar is
> stopped either way.

> ⚠️ **Dependency pin.** klend-builder requires `@kamino-finance/farms-sdk@3.2.24` exactly
> (pinned via `overrides`; klend-sdk@7.3.22 breaks on 3.2.25+). If `npm start` crashes with
> `MODULE_NOT_FOUND … @codegen/farms/programId`, the pin was lost — reinstall from the
> committed `package-lock.json`.

---

## 4. The kill switch & loss breaker

**Manual halt (works in paper and live):**
```bash
echo '{"ts":0,"reason":"manual halt"}' > assets/pairs_halt.json   # stop opening
rm assets/pairs_halt.json                                         # re-arm
```
While the file exists, every tick logs `no opens — Halted` and opens nothing. **Closes are
never blocked** — a held position stays exitable. (Same `HaltRecord` format as the momentum
halt file, so one convention covers both traders.)

**Automatic loss breaker (live only):** set `PAIRS_MAX_LOSS_USDC=N`. After each close, if
cumulative realized P&L ≤ −N, the trader writes the halt file itself and logs
`LOSS HALT — …`. Paper losses never trip it. Re-arm by deleting the halt file (and
investigating why).

---

## 5. Setting the borrow-APY cap from live data

The short leg accrues Kamino borrow funding, so `PAIRS_MAX_BORROW_APY_PCT` must reflect
reality. Read the live rate from the sidecar:

```bash
curl -s localhost:8181/market | jq '.reserves | map_values(.borrowApy)'
# borrowApy is a FRACTION: 0.034 = 3.4%. The trader multiplies ×100 internally.
```
Or read it off [app.kamino.finance](https://app.kamino.finance) for the xStocks market. Set
the cap with headroom above the typical rate (xStocks have run ~3–5%; the `30` default is a
generous ceiling, not a target).

---

## 6. Re-validating the edge as history grows

The recorded sample is short and NVDA-dispersion-heavy. Re-run the backtest periodically,
**feeding it the live funding rate**, and only keep trading if the edge survives:

```bash
# Convert the live short-leg APY to the sim's bps-per-day funding input:
#   bps/day ≈ APY% × 100 / 365   (e.g. 30% APY → 8.2 ; 4% APY → 1.1)
cargo run --release --bin momentum-sim -- run \
  --strategy pairs \
  --pair-funding-bps-day 8.2 \
  --pair-cost-bps 50
```
A robust result is profitable in **both** train and test slices with enough trades. If the
edge only survives at unrealistically low funding, stand down — don't trade it.

---

## 7. Per-reserve facts (xStocks market, verify before relying)

Caps and APYs move; re-check via the sidecar `/market`. As of 2026-06-23:

| Token | Borrowable? | Borrow cap | Notes |
|---|---|---|---|
| NVDAx | yes | 500 | Common short leg. |
| SPYx | yes | 5 000 | |
| QQQx | yes | 1 000 | |
| **GOOGLx** | **NO (cap 0)** | 0 | **Collateral-only.** The strategy can long GOOGLx but **never short it** — the gate auto-blocks "short GOOGLx" opens, so the GOOGLx/NVDAx pair only trades the short-NVDAx direction. |
| USDC | yes | 16 M | Collateral + borrowable. |

The gate reads these live (`borrowable` = borrow cap > 0), so a future cap change is handled
automatically — but if you add a **new** pair, sanity-check both legs in `/market` first.

---

## 8. Scale-up checklist (after going live)

Raise size only on evidence, never on impatience:

1. Start at `PAIRS_TRADE_USDC=5` (the canary).
2. Watch **≥ N clean open→close round-trips** (suggest N ≥ 5) where wallet balances,
   `pairs_state.json`, and the on-chain obligation all reconcile with the logs.
3. Check **slippage** on each leg vs `PAIRS_SLIPPAGE_BPS` — xStock DEX pools are thinner than
   majors; a leg that repeatedly nears the cap means you're too big for the pool.
4. Mind the **borrow cap**: your short notional in tokens must stay well under the reserve cap
   (NVDAx is only 500 tokens) and leave room for other borrowers.
5. Only then raise `PAIRS_TRADE_USDC`, and re-check 2–4 at the new size.
6. Keep `PAIRS_MAX_LOSS_USDC` set to a number you're willing to lose while validating.

---

## 9. Going live (2d.2 — NOT done yet)

Before flipping `DRY_RUN_PAIRS_TRADER=false`:

1. **2b.3 funded proof first** — manually drive one deposit → borrow → repay → withdraw on
   tiny funds via the sidecar + wallet, confirm it lands and cross-margin health behaves.
2. **2d.2 live wiring** — give each `open_pair`/`close_pair` step its real body (execute
   `swap_leg`, `KlendClient.build` → sign → submit, `rollback_plan` on failure) and wire
   `should_derisk` to live `read_obligation_health` (force-close below the floor).

Until both are done, live mode intentionally errors — that's the safety seam, not a bug.

---

## 10. Incident response

| Symptom | Action |
|---|---|
| Want to stop **now** | `echo '{"ts":0,"reason":"stop"}' > assets/pairs_halt.json`. Opens halt next tick; an open position still closes on signal. |
| Loss breaker fired | Halt file present + `LOSS HALT` in logs. Investigate the trade log in `pairs_state.json` before deleting the halt to re-arm. |
| Sidecar down (gate on) | Trader logs `klend gate on but /market failed; no opens this tick` and opens nothing (fail-safe). Restart the sidecar. |
| Held position, can't close | Closes ignore the halt and the cost gate. If `close_pair` keeps deferring, it's a missing price (logged) — it retries next tick. |
| Health falling (live) | Once 2d.2 is wired, the de-risk monitor force-closes below `PAIRS_MIN_HEALTH_FACTOR`. Manually, close by halting opens and letting the signal exit, or close the obligation directly. |
| Edge looks dead | Re-run §6 with the current funding rate; if it fails both slices, halt and stand down. |
