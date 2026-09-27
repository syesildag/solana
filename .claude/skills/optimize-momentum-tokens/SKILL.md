---
name: optimize-momentum-tokens
description: >-
  Use when the user wants to tune, re-optimize, sweep or refresh the PER-TOKEN `params` blocks in
  assets/momentum_tokens.json — for every deployed token or just one — e.g. "optimize the
  per-token params", "full grid scan for all deployed tokens", "re-tune HYPE", "best min_metric /
  trail / lookback for JitoSOL", "compare every trail % to the deployed params", "best SQN /
  maximin / least-drawdown combinations per trail", "KMNO was just added, sweep it", "the watch
  list changed, re-sweep" — even without the words grid, backtest or sweep. Ensures ≥150 days of
  history (fetched if missing), measures each token's own cost, runs isolated single-slot
  momentum-sim grids over disjoint time windows, and creates and presents one REPORT.md of the
  interesting combinations per token × trail % × decision axis versus the deployed params. Not for
  the global MOMENTUM_* knobs in .env (optimize-momentum-config) or vetting/adding a new mint
  (vet-momentum-token / add-momentum-token).
---

# Optimize Momentum Tokens

Per-token tuning for the momentum trader, end to end: history → cost → grid → windows →
verdicts → whole-book check → **one REPORT.md, created and presented** → apply only on an
explicit yes. Every rule here was a real failure in an earlier session; the scripts enforce
them so you do not have to remember them — read `references/` when a phase says so.

All scripts live in `.claude/skills/optimize-momentum-tokens/scripts/` (python3, stdlib only)
and are run from the repo root. `$S` below stands for that directory.

## Ground rules (why the scripts look the way they do)

- **Isolated N=1 per token.** Each grid job replays ONE token in its own one-token book with one
  slot — live runs `MOMENTUM_MAX_POSITIONS=10` for ~11 tokens, so each token effectively owns a
  slot and its own best params apply. Never run `--max-n 1` against the whole tokens file: that
  scores every cell under 11-way slot scarcity (winners flipped at N=2 on 2026-09-15).
  Interactions are measured once, by `book_ab.py`, at the live N.
- **The token's own cost.** `measure_costs.py` quotes a Jupiter round trip at the token's live
  notional — the route the trader executes — and each job runs at that `MOMENTUM_SLIPPAGE_BPS`.
- **≥150 days, never the live 30-day file**, built by `ensure_history.py` from pinned pools.
- **Never call `momentum-sim` by hand in this workflow.** Its loader truncates its input file
  when `HISTORY_MAX_SNAPSHOTS` is missing (chmod 444 does not stop it — 135 days were lost on
  2026-09-15). The scripts set it on every call, feed the sim run-dir copies only, and hold a
  machine-wide slot lock. If you must run it ad hoc, use `common.run_sim()`.
- **Disjoint windows, not nested cuts.** The 0.7 and 0.8 train fractions share ~77% of their
  test window — one observation, not two. Robustness = five disjoint back-half windows.
- **Full factorial**, never 1-D (three wrong answers on 2026-07-29).
- **Apply only on explicit confirmation; never restart the watcher** (the operator does).

## Workflow

Tell the user up front what will run and roughly how long (step 0 prints it).

**0. Preview (writes nothing).**
```bash
python3 $S/run_sweeps.py --dry-run [--tokens HYPE,ZEC]
```
Prints targets (watch-only tokens are skipped), the coverage/T0 table, per-token grid size, costs
("not measured yet" before step 2) and an ETA from previous runs; it never builds anything. If
there is no book yet, it says so — go to step 1.

**1. History** — report first, build only when needed:
```bash
python3 $S/ensure_history.py                 # T0 coverage of the newest price_history.book_*.jsonl
python3 $S/ensure_history.py --build         # fetch/top-up per-series raw files, merge a NEW book
```
Run `--build` with `run_in_background`: a first 150-day build is ~12 series × 2–40 min, one at a
time (GeckoTerminal 429s punish parallel fetches); later runs top up only the missing tail. A book
always holds every deployed series (`--tokens` only narrows the report). If T0 reports glitch
candidates on an LST (JitoSOL), rebuild with `--clean-pegged`. Read `references/history.md` if a
token is SHORT/INSUFFICIENT, starts late, or T0 warns.

**2. Costs:**
```bash
python3 $S/measure_costs.py --out assets/per_token_sweep_<date>/costs.json [--cost CATE=40]
```
Show the per-token table (median quote → integer sim bps, routes) to the user.

**3. Grid jobs** (background; resumable; safe to re-run):
```bash
python3 $S/run_sweeps.py -j 3 [--tokens …]
```
8 jobs per token — `full` (0.7 split), `f0` (front half), `f1..f5` (back-half windows),
`cost3x` — on one 2,160-cell grid (min ×{0.5…2} × trail 2,5,10,15,20,30∪deployed × lb × z ×
regime × fb). Each finished token gets `<SYM>_per_trail.md`, `<SYM>_candidates.json` and a
`<SYM>.done` marker; a failed job leaves its column blank (flagged in TRUST) and the run exits 1.
One `run_sweeps.py` per run dir (it takes a lock); re-running with a different layout (split,
windows, axes, costs) is refused — use a new `--run-dir`. First time on a new binary or book, prove the window shortcut once:
`python3 $S/run_sweeps.py --tokens HYPE --check-exact` (must print EXACT; if not, see
`references/windows.md`).

**4. Verdicts** — one `verdicts/<SYM>.json` per token (schema and analyst prompt in
`references/agents.md`). With ≥3 tokens, dispatch one read-only analyst agent per token as its
`.done` marker appears, in parallel with the remaining jobs; with 1–2 tokens, write them inline.
Either way the pick rule is the one in `references/agents.md` — no robust row that clears the
first four gates ⇒ `keep`.

**5. Whole-book check** (only if some verdict is `change`):
```bash
python3 $S/apply_params.py --run-dir <run> --from-verdicts    # → <run>/candidate_tokens.json (live file untouched)
python3 $S/book_ab.py --run-dir <run>                         # live N, one cost for both arms
```
A material additivity gap means the picks interact: split them into A/B/C/D arms before
recommending anything (`references/windows.md`).

**6. Create and present REPORT.md:**
```bash
python3 $S/build_report.py --run-dir <run>
```
Then, in chat: link it as `[REPORT.md](assets/per_token_sweep_<date>/REPORT.md)`, paste the
**Summary** table inline, state each token's verdict in one line, surface every TRUST FAIL/WARN
and the book A/B verdict, and end with a one-line offer to publish it as an artifact. Append one
line to `references/run-log.md`.

**7. Apply — only when the user explicitly says so:**
```bash
python3 $S/apply_params.py --run-dir <run> --from-verdicts --apply [--tag <name>]
```
Backs up to `assets/momentum_tokens.pre_<tag>_<ts>.bak`, merges ONLY the six swept knobs (every
other key kept, in order) and prints the diff. Tell the user to restart the watcher; record the
applied rows in memory (`project_momentum_met_bp_config`).

## Background jobs — see, stop, clean up

The long steps (`ensure_history.py --build`, `run_sweeps.py`, `book_ab.py`) run in the background and
may outlive the session on purpose (an overnight grid is fine). Finding and stopping them is generic,
not this skill's job: Claude Code tags every process its Bash tool starts (`CLAUDE_CODE_SESSION_ID`,
`CLAUDE_PID`), and the user-level **session reaper** reads those tags —

```bash
python3 ~/.claude/hooks/claude_reaper.py status                 # RUNNING vs ORPHAN (session gone), any project/skill
python3 ~/.claude/hooks/claude_reaper.py stop PGID | --orphans  # whole process group: SIGTERM, SIGKILL after 10 s
```

- Its `SessionStart` hook reports orphans into a new session: tell the user and offer to stop them or
  let resumable jobs finish — never stop one without asking. `SessionEnd` only logs by default
  (`~/.claude/claude_reaper.json` `session_end`: report | tied | all).
- Prefix a job with `CLAUDE_REAPER_TIE=1` to have it stopped when its session ends (policy `tied`).
- The scripts label their children (`CLAUDE_TASK_LABEL=optimize-momentum-tokens:…`) and forward SIGTERM
  to them (`common.install_cascade`), so stopping `run_sweeps.py` stops its sims. Stopping is safe:
  jobs are resumable, locks are `flock`, history/CSV writes are atomic.
- Not installed on this machine (no `~/.claude/hooks/claude_reaper.py`)? Run
  `python3 .claude/hooks/install_claude_reaper.py` once — macOS and Linux.

## What REPORT.md contains

Built by `build_report.py`; do not hand-edit numbers into it.

1. Header — book, span, N=1 isolated, windows, `.env` hash, git.
2. **Summary** — per token: cost (quote), deployed knobs, deployed worst-window/Σ, best
   alternative (trail · axes), Δ, TRUST, verdict; then the whole-book A/B table.
3. Per token — `TRUST` block (T0 data · T1 incumbent · T2 fresh · T3 slices · T4 units · T5
   tripwire; a FAIL on T0–T2 suppresses the tables), deployed profile, verdict + paste-ready
   params, **trail overview**, then **one table per trail rung**: DEPLOYED@T plus the top
   combination of every decision axis — max test P&L, best worst-slice P&L, best worst-slice $/h,
   least drawdown, best SQN, maximin, window-robust best Σ, evenest, smallest worst trade,
   cost-robust (3×), Pareto, consensus — one row per combination with an "axes won" column,
   windows f0…f5, tail, flags. Rungs where the trail never binds print as `≡ trail X`.
4. Not swept (INSUFFICIENT/FAIL tokens) and the fixed blind-spots footer.

How to read it — trust gates, flags, axes, and the judgment calls the scripts cannot make:
`references/reading-rules.md` (read it before writing any verdict).

## Troubleshooting

| symptom | cause → fix |
|---|---|
| `old CSV layout (no worst_train…)` | binary predates the 2026-09-27 CSV columns → `cargo build --release --bin momentum-sim`, re-run jobs |
| `was produced under different inputs` | `.env` / tokens / book / binary changed since that run dir was made → use a new `--run-dir` (results would mix) |
| T0 FAIL `plain "SOL" key` | a book not built by `ensure_history.py` → rebuild; the key triggers the mis-calibrated pegged pass |
| token `starts late` | young token, young pinned pool, or a partial GT fetch → `references/history.md` |
| `check-exact … MISMATCH` | sanitizer edge effect at a window start → `references/windows.md` fallback |
| TRIPWIRE abort | the canonical book changed mid-run → restore from `assets/history_backups/*.gz` first |

## References

- `references/reading-rules.md` — trust gates, flags, axes, verdict rules, what the sweep cannot see.
- `references/windows.md` — time-split construction, maximin, rule 2b, nested cuts, book A/B arms.
- `references/history.md` — coverage gates, fetch/merge recipe, hazards, short-history policy.
- `references/agents.md` — when to fan out, analyst prompt, verdict JSON schema.
- `references/run-log.md` — one line per run; earlier verification results.
