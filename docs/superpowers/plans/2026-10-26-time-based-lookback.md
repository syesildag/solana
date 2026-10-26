# Time-based Lookback Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Per-token `lookback_min` / `entry_max_z_min` so the live trader and `momentum-sim` score a token over the same span of time whatever the row density, with zero behaviour change for tokens that don't opt in.

**Architecture:** A small new module `src/portfolio/lookback.rs` owns the window rule (`Lookback::{Obs, Minutes}`, coverage check, z-score over a window). `momentum::rank_candidates` (already shared by live and sim) and both z-gate call paths resolve each token's lookback through it. The sim's trailing deque also retains wall-clock time, live cold backfill covers the largest minutes window, and the per-token sweep and the tuning skill's Python carry the unit in the label value (`lb=3170m`).

**Tech Stack:** Rust (tokio watcher, `momentum-sim` clap CLI, rayon), Python 3 skill scripts with pytest.

**Spec:** `docs/superpowers/specs/2026-10-26-time-based-lookback-design.md`

## Global Constraints

- A token without `lookback_min` / `entry_max_z_min` behaves byte-identically, live and in every sim subcommand (Task 5 proves it).
- `COVER = 0.75`: a minutes window is valid only if its oldest point is at or before `now − 0.75·L·60`.
- Existing point floors stay on top of the time rule: metric needs `SORTINO_MIN_OBS` (120) returns; z needs 30 points.
- `entry_max_z_min > 0` enables the z-gate even when `entry_max_z_obs` is 0; absent ⇒ today's resolution.
- Labels: rows `lb=480`, `z=1.5@480`; minutes `lb=3170m`, `z=1.5@3170m`. No `lbm=` key.
- Never run `cargo fmt` / `rustfmt` on a file. Tests go in the existing bottom-of-file `#[cfg(test)] mod tests`.
- Every sim run on a history file sets `HISTORY_MAX_SNAPSHOTS=100000000` in the same environment.
- Build only `--bin momentum-sim` / `--lib` during development; never restart the running `portfolio-watcher`.
- Commits: only if the operator asks. Otherwise skip the commit steps and leave the tree dirty for review.

## Review Focus

1. **Sleep hole inside a covered window:** a 48 h window whose middle 15 h is empty must still score (it's covered); a window whose only points are the last 2 h must read as warming. Test in Task 1.
2. **Minutes window too short for the point floor on a sparse token:** must warm (no score) rather than score on a few points, and the sweep must say *insufficient* rather than report 0 trades. Tests in Task 2 and Task 6.
3. **`entry_max_z_obs: 0` plus `entry_max_z_min: 2880`:** the gate must be ON. Test in Task 1 (`resolve_z`).
4. **Deque starvation in the sim:** a 3,000-minute window on 1-minute snapshots exceeds the row-sized deque (`(lb+lag)·3+50`), so it must not be truncated. Test in Task 4.
5. **A pick on rows applied to a token that is on minutes** (or the reverse): `params_for_knobs` must remove the other unit's key so it cannot keep overriding. Test in Task 7.

---

### Task 1: `lookback` module and the two new params

**Files:**
- Create: `src/portfolio/lookback.rs`
- Modify: `src/portfolio/mod.rs` (add `pub mod lookback;` in alphabetical order, after `liquidation_state`)
- Modify: `src/portfolio/momentum_universe.rs` (`TokenParams`, after `lookback_obs`; parse test next to `token_params_parse_lookback_obs`)

**Interfaces:**
- Produces:
  - `TokenParams.lookback_min: Option<u32>`, `TokenParams.entry_max_z_min: Option<u32>`: both `#[serde(default, skip_serializing_if = "Option::is_none")]`, each with a doc comment pointing at the spec.
  - `pub enum Lookback { Obs(usize), Minutes(u32) }` (`Debug, Clone, Copy, PartialEq, Eq`)
  - `pub const COVER: f64 = 0.75;` and `pub const Z_MIN_POINTS: usize = 30;`
  - `pub fn window(series: &[(u64, f64)], now: u64, lb: Lookback) -> Option<&[(u64, f64)]>`: `Obs(n)` returns the last `n` points, or the whole series if shorter (exactly today's `rank_candidates` slice; never `None`). `Minutes(m)` returns the points with `ts > now − m·60`, or `None` if that set is empty or its first point is later than `now − COVER·m·60`. `Obs(0)` / `Minutes(0)` return `None`.
  - `pub fn resolve_metric(p: Option<&TokenParams>, global_obs: usize) -> Lookback`: `lookback_min > 0` ⇒ `Minutes`, else `Obs(lookback_obs ?? global_obs)`.
  - `pub fn resolve_z(p: Option<&TokenParams>, global_obs: usize) -> Lookback`: `entry_max_z_min > 0` ⇒ `Minutes`, else `Obs(entry_max_z_obs ?? global_obs)` (so `Obs(0)` = gate off).
  - `pub fn zscore(points: &[(u64, f64)]) -> Option<f64>`: population z of the last price vs the window mean; `None` below `Z_MIN_POINTS` or when σ < 1e-12. The same arithmetic as `momentum::entry_dip_z` today.
  - `impl Lookback { pub fn is_off(&self) -> bool }`: true for `Obs(0)` / `Minutes(0)`.

- [ ] **Step 1: Write the failing tests** in `lookback.rs`'s `#[cfg(test)] mod tests`. The series helper is `fn per_min(n: u64, t0: u64) -> Vec<(u64, f64)>` (one point per 60 s, price `100 + i`).

```rust
#[test] fn obs_window_is_todays_tail() {
    let s = per_min(500, 0);
    assert_eq!(window(&s, s.last().unwrap().0, Lookback::Obs(480)).unwrap().len(), 480);
    assert_eq!(window(&s[..100], 5940, Lookback::Obs(480)).unwrap().len(), 100); // shorter ⇒ whole series
}
#[test] fn minutes_equals_obs_at_one_row_per_minute() {
    let s = per_min(1000, 0); let now = s.last().unwrap().0;
    assert_eq!(window(&s, now, Lookback::Minutes(480)).unwrap(), window(&s, now, Lookback::Obs(480)).unwrap());
}
#[test] fn uncovered_window_is_warming() {
    let s = per_min(120, 0); // 2 h of data
    assert!(window(&s, s.last().unwrap().0, Lookback::Minutes(2880)).is_none());
}
#[test] fn hole_inside_covered_window_still_scores() {
    let mut s = per_min(600, 0);                       // 10 h
    s.extend(per_min(600, 600 * 60 + 15 * 3600));      // 15 h sleep hole, then 10 h
    let w = window(&s, s.last().unwrap().0, Lookback::Minutes(48 * 60));
    assert!(w.is_some() && w.unwrap().len() == 1200);
}
#[test] fn just_woke_after_long_sleep_is_warming() {
    let mut s = per_min(600, 0);
    s.extend(per_min(120, 600 * 60 + 15 * 3600));      // only 2 h since wake
    assert!(window(&s, s.last().unwrap().0, Lookback::Minutes(8 * 60)).is_none());
}
#[test] fn resolve_z_minutes_enables_gate_even_when_obs_zero() {
    let p = TokenParams { entry_max_z_obs: Some(0), entry_max_z_min: Some(2880), ..Default::default() };
    assert_eq!(resolve_z(Some(&p), 480), Lookback::Minutes(2880));
    let off = TokenParams { entry_max_z_obs: Some(0), ..Default::default() };
    assert!(resolve_z(Some(&off), 480).is_off());
    assert_eq!(resolve_z(None, 480), Lookback::Obs(480));
}
#[test] fn resolve_metric_prefers_minutes() {
    let p = TokenParams { lookback_obs: Some(480), lookback_min: Some(3170), ..Default::default() };
    assert_eq!(resolve_metric(Some(&p), 121), Lookback::Minutes(3170));
    assert_eq!(resolve_metric(None, 121), Lookback::Obs(121));
}
#[test] fn zscore_matches_entry_dip_z_floor() {
    assert!(zscore(&per_min(29, 0)).is_none());
    assert!(zscore(&per_min(40, 0)).unwrap() > 0.0);
}
```

Also in `momentum_universe.rs`: `token_params_parse_minutes_fields` parses `{"lookback_min":3170,"entry_max_z_min":2880}` into `Some(3170)` / `Some(2880)`, and an absent field into `None`.

- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --lib lookback` and `cargo test --lib token_params_parse_minutes_fields`. Expected: compile errors (module and fields don't exist).
- [ ] **Step 3: Implement** the module and the fields per the Interfaces block. Write `window` with `partition_point` on `ts` (the series is oldest-first).
- [ ] **Step 4: Run the tests.** Run: `cargo test --lib lookback && cargo test --lib momentum_universe`. Expected: all pass.
- [ ] **Step 5: Commit** (only if asked): `feat(momentum): lookback module + per-token lookback_min/entry_max_z_min params`

### Task 2: Ranking uses the resolved lookback (live and sim share it)

**Files:**
- Modify: `src/portfolio/momentum.rs` (`rank_candidates`, ~line 1468; tests at the bottom)

**Interfaces:**
- Consumes: `lookback::{resolve_metric, window, Lookback}` (Task 1).
- Produces: no signature change to `rank_candidates`. For a `Minutes` token: `now = history.back().ts`; the window comes from `lookback::window`; `None` ⇒ the token is skipped (warming, exactly like `compute_metrics` returning `None` today); `metric_is_fading` gets `window.len()` as its row lookback.

- [ ] **Step 1: Write the failing tests** (momentum.rs tests; build a `VecDeque<PriceSnapshot>` with `snap(ts, "AAA", price)`):
  - `rank_minutes_token_uses_time_window`: 3,000 one-minute snapshots of a rising series, token params `lookback_min: Some(1440)` ⇒ the `AAA` candidate has `obs == 1439`.
  - `rank_minutes_token_warms_when_uncovered`: 200 snapshots, `lookback_min: Some(2880)` ⇒ `AAA` is absent from the result.
  - `rank_minutes_token_below_point_floor_warms`: 200 snapshots spaced 600 s apart (sparse), `lookback_min: Some(600)` (10 h ≈ 60 points < 121) ⇒ `AAA` absent.
  - `rank_obs_token_unchanged`: the existing per-token `lookback_obs: Some(240)` case still yields `obs == 239`.
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --lib rank_minutes_token`. Expected: FAIL (field ignored, obs ≠ 1439 / token present).
- [ ] **Step 3: Implement** in `rank_candidates`: replace the `tok_lookback` / slice block with `resolve_metric(w.params.as_ref(), lookback)` + `lookback::window(&series_ts, now, lb)`. Keep everything after the window unchanged.
- [ ] **Step 4: Run the tests.** Run: `cargo test --lib momentum::`. Expected: all pass, including every pre-existing test.
- [ ] **Step 5: Commit** (only if asked): `feat(momentum): rank on the per-token time window when lookback_min is set`

### Task 3: z-gate on the resolved lookback (live and both sim paths)

**Files:**
- Modify: `src/portfolio/momentum.rs`: `entry_dip_z` / `entry_overbought` (~361–384), `entry_max_z_obs_for` (~5038), the rank-panel z column (~1813), `try_open_position`'s overbought check (~3607–3613)
- Modify: `src/portfolio/sim.rs`: `replay_multi`'s per-token z check (~1139 resolver, ~1648 call); add `fn token_series_since(snapshots: &[PriceSnapshot], i: usize, mint: &str, since_ts: u64) -> Vec<(u64, f64)>`

**Interfaces:**
- Consumes: `lookback::{resolve_z, window, zscore, Lookback}`.
- Produces:
  - `pub fn entry_z(history: &VecDeque<PriceSnapshot>, mint: &str, lb: Lookback) -> Option<f64>`: `Obs(n)` ⇒ today's `entry_dip_z(history, mint, n)` result exactly; `Minutes` ⇒ `zscore(window(own (ts, price) series, history.back().ts, lb)?)`. `entry_dip_z` stays as is (the dip gate still uses it).
  - `entry_overbought(history, mint, lb: Lookback, max_z) -> bool`: `!lb.is_off() && entry_z(..).is_some_and(|z| z > max_z)`.
  - `entry_max_z_obs_for` is replaced by `entry_z_lookback_for(watched, mint, global_obs) -> Lookback`, wrapping `resolve_z`.
  - Sim: in `replay_multi`, a `Minutes` z lookback evaluates `zscore(window(&token_series_since(snapshots, i, mint, now − m·60), now, lb)?)` with `now = snapshots[i].ts`. `Obs` keeps calling `token_dip_z` unchanged. The single-position `replay_with_regime` path keeps its global `params.entry_max_z_obs` (it reads no per-token z today).
  - Log line: the overbought message prints `over {n}obs` or `over {h:.1}h`.

- [ ] **Step 1: Write the failing tests:**
  - momentum.rs `entry_overbought_minutes_window`: a rising one-per-minute deque of 600 points; `Lookback::Minutes(480)` with `max_z = 1.0` blocks; `Lookback::Obs(0)` never blocks; a 100-point deque with `Minutes(480)` (uncovered) never blocks.
  - momentum.rs `entry_z_obs_matches_entry_dip_z`: for `Obs(40)` on the existing `rising` fixture, `entry_z == entry_dip_z`.
  - sim.rs `z_minutes_live_sim_parity`: the same 600 one-minute prices as `Vec<PriceSnapshot>` (sim) and `VecDeque` (live); the sim's minutes z at `i = 599` equals `momentum::entry_z(.., Minutes(300))` to 1e-12.
  - sim.rs `replay_multi_minutes_z_gate_blocks_extended_entry`: copy the existing `entry_max_z_obs: 0` exemption test (~line 6855) and set `entry_max_z_min: Some(60)` instead. The entry that test shows is admitted when exempt must now be blocked (0 trades).
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --lib entry_overbought_minutes_window z_minutes_live_sim_parity replay_multi_minutes_z_gate`. Expected: compile errors / FAIL.
- [ ] **Step 3: Implement** per the Interfaces block. Update the existing `entry_overbought_*` and `entry_max_z_obs_for` tests to the new signatures, with the same assertions.
- [ ] **Step 4: Run the tests.** Run: `cargo test --lib momentum:: && cargo test --lib sim::`. Expected: all pass.
- [ ] **Step 5: Commit** (only if asked): `feat(momentum): z-gate on the per-token time window (live + sim parity)`

### Task 4: Sim deque keeps wall-clock time; live backfill covers the minutes window

**Files:**
- Modify: `src/portfolio/sim.rs` `ranked_stream` (~665)
- Modify: `src/portfolio/watcher.rs` (both `backfill_watched_cold` call sites, ~215 and ~230)
- Modify: `src/portfolio/lookback.rs`: add `pub fn max_minutes(watched: &[WatchedToken]) -> u32` (max of `lookback_min` and `entry_max_z_min` across watched; 0 if none)

**Interfaces:**
- Consumes: `lookback::max_minutes`.
- Produces: `ranked_stream` pops the front only while `deque.len() > win` **and** (`max_min == 0` or `front.ts + max_min·60 + 3600 < snap.ts`). Watcher: the backfill argument becomes `cfg.momentum_lookback_obs.max(lookback::max_minutes(&watched) as usize)` (the callee already treats the value as minutes and caps at 7 days).

- [ ] **Step 1: Write the failing test** sim.rs `ranked_stream_feeds_minutes_window_beyond_row_deque`: copy `ranked_stream_feeds_per_token_lookback_larger_than_global`, but with 4,000 snapshots at 60 s spacing, global `lookback_obs = 121`, token `lookback_min: Some(3000)` ⇒ the last candidate has `obs == 2999` (the row-sized deque would hold only `121·3+50 = 413`).
- [ ] **Step 2: Run it to verify it fails.** Run: `cargo test --lib ranked_stream_feeds_minutes_window`. Expected: FAIL (`AAA` absent or obs ≈ 412).
- [ ] **Step 3: Implement** the deque rule, `max_minutes`, and the two watcher call sites.
- [ ] **Step 4: Run the tests.** Run: `cargo test --lib sim:: && cargo build --release --bin portfolio-watcher`. Expected: tests pass, watcher compiles. Don't start it.
- [ ] **Step 5: Commit** (only if asked): `feat(momentum): sim deque and live backfill cover lookback_min`

### Task 5: No-opt-in regression proof

**Files:** none modified. Inputs frozen on 2026-10-26 in the session scratchpad `golden/`: `history.jsonl` (last 86,400 rows of `assets/price_history.book_20261025.jsonl`, sha256 prefix `d818475ac0be021e`), `tokens.json` (deployed file, `b54011354d06cac6`), `env.snapshot` (`e9a92329995ec6b2`), `baseline_maxn.txt` (pre-change binary output).

- [ ] **Step 1: Check the inputs haven't drifted.** Run: `shasum -a 256 golden/history.jsonl golden/tokens.json` and `diff golden/env.snapshot .env`. Expected: same prefixes, no diff. If `.env` changed, re-run the baseline with `golden/momentum-sim.baseline` first.
- [ ] **Step 2: Rebuild and replay.** Run: `cargo build --release --bin momentum-sim`, then the same command as the baseline (`maxn-compare --history golden/history.jsonl --tokens golden/tokens.json --train-frac 0.7 --metric slope_r2 --lookback 480 --trail 10 --min-metric 99 --trade-usdc 100 --regime-obs 480 --regime-mode trend --regime-trend-min 0 --stagnation-hours 96 --stagnation-band-pct 2 --max-n 10 --dump-trades`, with `HISTORY_MAX_SNAPSHOTS=100000000`) into `golden/after_maxn.txt`.
- [ ] **Step 3: Diff.** Run: `diff golden/baseline_maxn.txt golden/after_maxn.txt`. Expected: **no output**. Any diff is a goal-3 violation: fix it before continuing.
- [ ] **Step 4: Full suite and lint.** Run: `cargo test --lib && cargo clippy --lib --bin momentum-sim 2>&1 | grep -E "^(warning|error)" | sort | uniq -c`. Expected: tests pass; no new warning in the touched files compared with `git stash`-free HEAD (compare by file name).

### Task 6: `per-token-sweep` minutes axis

**Files:**
- Modify: `src/bin/momentum_sim.rs`: `PerTokenSweep` args (~651) and `PerTokenSweepArgs`; `per_token_sweep` grid (~2496–2545); `sweep_label_to_params_json` (~2665); `fmt_token_params` (~1997); a CLI parse test in the bin's test module.

**Interfaces:**
- Consumes: `TokenParams.lookback_min`, `entry_max_z_min`; `lookback::window`.
- Produces:
  - Args: `#[arg(long, value_delimiter = ',')] lookback_mins: Option<Vec<u32>>` (when set it replaces `--lookbacks` for the run; `ensure!` that both weren't given explicitly) and `#[arg(long)] entry_max_z_min: Option<u32>` (when set, z cells write `entry_max_z_min` instead of `entry_max_z_obs`; z-off cells write `entry_max_z_obs = Some(0)` and `entry_max_z_min = None`).
  - The grid iterates a `Vec<Lookback>`. A minutes cell sets `p.lookback_min = Some(m)`; a rows cell sets `p.lookback_obs = Some(lb)` and `p.lookback_min = None`.
  - Labels: `lb={lb}` / `lb={m}m`; `z={z}@{obs}` / `z={z}@{m}m` / `off`.
  - Insufficient rule: before building a minutes stream, compute the median point count of the target token's `window(.., Minutes(m))` at every 60th snapshot of the train slice. If it's below `SORTINO_MIN_OBS + 1`, print `lb={m}m insufficient: median {n} points < 121 on this history` and skip that lookback (no cells, no CSV rows).
  - `sweep_label_to_params_json`: `lb=<n>m` ⇒ `lookback_min = Some(n)` (keep `lookback_obs`); `lb=<n>` ⇒ `lookback_obs = Some(n)`, `lookback_min = None`; `z=<z>@<n>m` ⇒ `entry_max_z = z`, `entry_max_z_min = Some(n)`; `z=off` ⇒ also clears `entry_max_z_min`. `inc_val("lb")` / `inc_val("z")` render the incumbent in the same unit-suffixed form.
  - `fmt_token_params` prints `lbm={v}` and `zmin={v}` when set (header only; absent ⇒ unchanged output).

- [ ] **Step 1: Write the failing tests** (bin test module):
  - `sweep_label_minutes_roundtrip`: `sweep_label_to_params_json("min=3.6 trail=10 lb=3170m z=1@2880m regime=gated fb=1", &inc)` parses to JSON with `lookback_min == 3170`, `entry_max_z_min == 2880`, `entry_max_z == 1`.
  - `sweep_label_rows_clears_minutes`: an incumbent with `lookback_min: Some(3170)` and label `lb=480` ⇒ `lookback_obs == 480` and no `lookback_min` key.
  - `cli_parses_lookback_mins`: `Cli::try_parse_from(["momentum-sim","per-token-sweep","--lookback-mins","1440,2880","--entry-max-z-min","2880"])` gives `Some(vec![1440,2880])` and `Some(2880)`.
- [ ] **Step 2: Run them to verify they fail.** Run: `cargo test --bin momentum-sim sweep_label_minutes cli_parses_lookback_mins`. Expected: FAIL / compile error.
- [ ] **Step 3: Implement** per the Interfaces block.
- [ ] **Step 4: Run the tests, plus a smoke run.** Run: `cargo test --bin momentum-sim`. Then `HISTORY_MAX_SNAPSHOTS=100000000 ./target/release/momentum-sim per-token-sweep --history golden/history.jsonl --tokens golden/tokens.json --token HYPE --max-n 1 --lookback-mins 480,1440,3170 --entry-max-z-min 3170 --min-metrics 3.6564 --trails 10 --entry-max-zs 0,1 --fade-fracs 0.75 --no-regime-sweep --top 3`. Expected: tests pass; the smoke output has rows labelled `lb=1440m` / `lb=3170m` and `z=1@3170m`, plus an `lb=480m insufficient` line if HYPE's book density can't fill 8 h.
- [ ] **Step 5: Commit** (only if asked): `feat(momentum-sim): per-token-sweep --lookback-mins / --entry-max-z-min`

### Task 7: Tuning-skill Python understands unit-suffixed `lb` / `z`

**Files:**
- Modify: `.claude/skills/optimize-momentum-tokens/scripts/common.py`: `SWEPT_KEYS`, `_typed`, `cell_label`, `deployed_knobs`, `axes_for`, `params_for_knobs`; add `norm_lb`, `lb_minutes`, `DEFAULT_LOOKBACK_MINS`
- Modify: `run_sweeps.py` `sweep_args` (~223) and the `--lookbacks` CLI parse (~320/370); `book_ab.py` `one_cell_axes` (~38); `apply_params.py` (~44); `build_report.py` (~63, ~89); `per_trail_report.py` (~299, ~561, ~742, ~750)
- Test: `.claude/skills/optimize-momentum-tokens/scripts/tests/test_common.py`

**Interfaces:**
- Produces (common.py):
  - `norm_lb(v) -> int | str`: `480`, `"480"` ⇒ `480`; `"3170m"` ⇒ `"3170m"`; anything else raises `ValueError`. Every former `int(pick["lb"])` / `int(k["lb"])` becomes `norm_lb(...)`.
  - `lb_minutes(v) -> int | None`: `"3170m"` ⇒ `3170`, rows ⇒ `None`.
  - `DEFAULT_LOOKBACK_MINS = ["480m", "960m", "1440m", "2160m", "2880m", "4320m"]`.
  - `SWEPT_KEYS` gains `"lookback_min"` and `"entry_max_z_min"`.
  - `deployed_knobs`: `lb` = `f"{lookback_min}m"` when `lookback_min` is set, else today's int; `z_obs` = `f"{entry_max_z_min}m"` when set (and the z gate counts as on), else today's int.
  - `cell_label(..., lb, ..., z_obs)` prints the values as given (`lb=3170m`, `z=1@3170m`).
  - `axes_for`: when the deployed `lb` is minutes, the lookback defaults are `DEFAULT_LOOKBACK_MINS` ∪ deployed. Mixing rows and minutes in one axis raises `ValueError`. Sort with `key=lambda v: lb_minutes(v) or v`.
  - `params_for_knobs`: minutes `lb` ⇒ set `lookback_min`, leave `lookback_obs`; rows `lb` ⇒ set `lookback_obs`, pop `lookback_min`. Minutes `z_obs` with z>0 ⇒ set `entry_max_z_min` + `entry_max_z`; rows z>0 ⇒ today's keys and pop `entry_max_z_min`; z off ⇒ `entry_max_z_obs = 0`, pop `entry_max_z` and `entry_max_z_min`.
  - `run_sweeps.sweep_args`: minutes lookbacks ⇒ `--lookback-mins 3170,...` (suffix stripped) instead of `--lookbacks`; a minutes deployed `z_obs` ⇒ `--entry-max-z-min N` instead of `--entry-max-z-obs`.

- [ ] **Step 1: Write the failing tests** in `test_common.py`:

```python
def test_norm_lb():
    assert common.norm_lb(480) == 480 and common.norm_lb("480") == 480 and common.norm_lb("3170m") == "3170m"
def test_deployed_knobs_minutes():
    k = common.deployed_knobs({"min_metric": 3.6, "lookback_obs": 480, "lookback_min": 3170,
                               "entry_max_z_obs": 0, "entry_max_z_min": 2880, "entry_max_z": 1.0}, {})
    assert k["lb"] == "3170m" and k["z_obs"] == "2880m" and k["z"] == 1.0
def test_cell_label_and_parse_minutes():
    lab = common.cell_label(3.6, 10, "3170m", 1.0, False, 1.0, z_obs="2880m")
    assert "lb=3170m" in lab and "z=1@2880m" in lab
    assert common.parse_label(lab)["lb"] == "3170m"
def test_params_for_knobs_switches_unit():
    dep = {"min_metric": 3.6, "lookback_obs": 480, "lookback_min": 3170}
    rows = common.params_for_knobs(dep, {"min": 3.6, "trail": 10, "lb": 480, "z": 0, "regime_exempt": False, "fb": 1})
    assert rows["lookback_obs"] == 480 and "lookback_min" not in rows
    mins = common.params_for_knobs({"min_metric": 3.6, "lookback_obs": 480},
                                   {"min": 3.6, "trail": 10, "lb": "1440m", "z": 0, "regime_exempt": False, "fb": 1})
    assert mins["lookback_min"] == 1440 and mins["lookback_obs"] == 480
def test_axes_for_minutes_defaults():
    ax = common.axes_for({"min_metric": 3.6, "lookback_min": 3170}, {})
    assert "3170m" in ax["lookbacks"] and all(isinstance(v, str) for v in ax["lookbacks"])
def test_sweep_args_minutes():
    ax = common.axes_for({"min_metric": 3.6, "lookback_min": 3170}, {})
    args = [str(a) for a in run_sweeps.sweep_args("HYPE", "h.jsonl", "t.json", 0.7, ax, "o.csv")]
    assert "--lookback-mins" in args and "--lookbacks" not in args
```

- [ ] **Step 2: Run them to verify they fail.** Run: `cd .claude/skills/optimize-momentum-tokens/scripts && python3 -m pytest tests/test_common.py -q`. Expected: the new tests FAIL; the old ones pass.
- [ ] **Step 3: Implement** per the Interfaces block, including every listed `int(...lb)` call site in the other scripts.
- [ ] **Step 4: Run the whole skill suite.** Run: `python3 -m pytest tests -q`. Expected: all pass, the pre-existing tests unchanged.
- [ ] **Step 5: Docs, folded in:** one paragraph in `CLAUDE.md` under the momentum section (the two new params, `COVER`, the label convention, a pointer to the spec), and one line in the skill's `references/reading-rules.md` ("`lb=…m` rows are time windows; an `insufficient` lookback is a data-density verdict, not a 0-trade result"). Update `references/history.md`'s "negligible for liquid tokens" sentence with the measured HYPE/JitoSOL Spearman (0.31 / 0.27 rows vs 0.94 time).
- [ ] **Step 6: Commit** (only if asked): `feat(skill): optimize-momentum-tokens understands time-window lookbacks`

---

**Out of plan (spec phases B–D, operator-gated):** converting HYPE/JitoSOL/BP/wNEAR/MET to minutes on the book (equivalence A/B), re-tuning them, deploying, and the post-deploy agreement check. Those change live config and need the operator.
