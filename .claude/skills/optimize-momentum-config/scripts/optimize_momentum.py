#!/usr/bin/env python3
"""
optimize_momentum.py — run the momentum-sim grid over the curated token universe,
pick the best fixed-trail config, compare it head-to-head against the config currently
in .env, and (optionally) write the winning values back into .env.

Why this shape:
- The live momentum trader honors a FIXED-% trailing stop only (it has no vol-stop env
  knob), so we run the grid with --no-vol-stops. That guarantees the chosen config is
  faithfully reproducible live — a backtest winner that relied on an ATR/sigma stop would
  look good on paper but the live trader couldn't replicate it.
- We auto-tune the 11 knobs the grid optimizes AND the live trader reads: RANK_METRIC,
  MIN_METRIC, TRAIL_PCT, LOOKBACK_OBS, MAX_RUN_PCT, ROTATE_MARGIN, the regime trio
  REGIME_MODE/REGIME_OBS/REGIME_TREND_MIN, and the overbought z-gate pair
  ENTRY_MAX_Z_OBS/ENTRY_MAX_Z. Regime is a FULL grid dimension (off + level
  windows 240/480/720 + trend windows 240/480/720 × train-quantile thresholds), the
  z-gate is swept by default (off + thresholds 1.0/1.5/2.0 over 480 obs — ~4x grid,
  disable with --entry-max-z-obs 0), and the winner's gates are applied together with
  the config — an edge and its gates are selected as a unit, so writing one without
  the other would deploy an untested combination.
- Selection objective (default: pnl-per-hold): the winner is the robust config with the
  best WORST-SLICE $/hour-deployed — min(rate_train, rate_test) where rate = net_pnl /
  hold_hours — the most capital-efficient dependable config. Pass --objective net-pnl to
  instead rank by worst-slice absolute net P&L (the most total money; may hold capital
  far longer to get it — the objective comparison in the output always shows both sides).
- Execution assumptions come from .env: the grid's base_params reads MOMENTUM_SLIPPAGE_BPS
  and MOMENTUM_MAX_COST_BPS, so the scan optimizes at the fills you've configured live.
  Both are printed in the run banner for transparency.
- Default is preview-only. Pass --apply to write .env (after backing up to .env.bak).

Usage:
  python3 optimize_momentum.py                    # preview (no writes)
  python3 optimize_momentum.py --apply            # back up .env, then write the winner
  python3 optimize_momentum.py --min-trades 5     # stricter robustness gate
  python3 optimize_momentum.py --objective pnl-per-hold   # opt-in capital-efficiency selection
"""
import argparse
import csv
import os
import re
import shutil
import subprocess
import sys
import tempfile

# The 11 knobs we auto-tune: CSV column -> .env variable. Regime is a full grid
# dimension (Off + Level windows + Trend windows × train-quantile thresholds) and
# the winner's regime IS applied — a config's edge and its regime gate are selected
# together, so writing one without the other would deploy an untested combination.
# The overbought z-gate pair is managed the same way (2026-07-12): the clean-data
# winner used z≤1.5@480, so a grid that can't sweep it can neither find that family
# nor fairly replay the CURRENT config against it.
MANAGED = [
    ("metric",            "MOMENTUM_RANK_METRIC"),
    ("min_metric",        "MOMENTUM_MIN_METRIC"),
    ("trail_pct",         "MOMENTUM_TRAIL_PCT"),
    ("lookback_obs",      "MOMENTUM_LOOKBACK_OBS"),
    ("max_run_pct",       "MOMENTUM_MAX_RUN_PCT"),
    ("rotate_margin",     "MOMENTUM_ROTATE_MARGIN"),
    ("regime_mode",       "MOMENTUM_REGIME_MODE"),
    ("regime_filter_obs", "MOMENTUM_REGIME_OBS"),
    ("regime_threshold",  "MOMENTUM_REGIME_TREND_MIN"),
    ("entry_max_z_obs",   "MOMENTUM_ENTRY_MAX_Z_OBS"),
    ("entry_max_z",       "MOMENTUM_ENTRY_MAX_Z"),
]


def repo_root():
    out = subprocess.run(["git", "rev-parse", "--show-toplevel"],
                         capture_output=True, text=True)
    return out.stdout.strip() if out.returncode == 0 else os.getcwd()


def ensure_binary(root):
    binp = os.path.join(root, "target", "release", "momentum-sim")
    if not os.path.exists(binp):
        print("Building momentum-sim (release)… first build can take a few minutes.")
        r = subprocess.run(["cargo", "build", "--release", "--bin", "momentum-sim"],
                           cwd=root)
        if r.returncode != 0:
            sys.exit("Build failed — fix the compile error and retry.")
    return binp


def fmt(col, val):
    """Format a CSV value the way .env conventionally writes it."""
    s = str(val).strip()
    if col == "metric":
        return s
    try:
        f = float(s)
    except ValueError:
        return s
    if col in ("min_metric", "regime_threshold"):  # keep precision for thresholds
        return f"{f:.4f}".rstrip("0").rstrip(".") if "." in f"{f:.4f}" else f"{f:.4f}"
    if f == int(f):                       # whole number -> int (20.0 -> 20)
        return str(int(f))
    return s


def run_grid(binp, root, tokens, csv_path, min_trades, objective, dump_trades=True,
             entry_max_z_obs=0, entry_max_zs=None):
    # momentum-sim only knows net-pnl|pnl-per-hold (its objective just orders the printed
    # table); the pareto/SQN selection happens here in the script, off the CSV.
    sim_objective = "net-pnl" if objective == "pareto" else objective
    cmd = [binp, "run", "--tokens", tokens, "--no-vol-stops",
           "--objective", sim_objective,
           "--regime-obs", "0,240,480,720", "--regime-trend-obs", "240,480,720",
           "--min-trades", str(min_trades), "--top", "5", "--csv", csv_path]
    # Optional overbought entry-gate sweep. The grid always includes the gate-off variant,
    # so passing these only widens the search; the winner may or may not use the gate.
    if entry_max_z_obs > 0 and entry_max_zs:
        cmd += ["--entry-max-z-obs", str(entry_max_z_obs), "--entry-max-zs", entry_max_zs]
    if dump_trades:
        cmd.append("--dump-trades")
    print("Running grid (fixed-trail only):\n  " + " ".join(cmd) + "\n")
    proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr[-2000:])
        sys.exit("momentum-sim run failed.")
    # Lift the per-trade listing out of the tool output so we can show it at the very end
    # (the user wants the trade list last), instead of buried mid-stream in the tail.
    trades = ""
    m = re.search(r"\n=== TRADES —.*?(?=\nFull grid \()", proc.stdout, re.S)
    if m:
        trades = m.group(0).strip()
    shown = proc.stdout.replace(m.group(0), "\n") if m else proc.stdout
    sys.stdout.write(shown[-4000:])  # tail of the tool's own verdict/table
    return proc.stdout, trades


def parse_best_block(stdout):
    """Pull the tool's authoritative 'paste into .env' block + its held-out PnL."""
    best = {}
    for m in re.finditer(r"^\s*(MOMENTUM_[A-Z_]+)=([^#\s]+)", stdout, re.M):
        best[m.group(1)] = m.group(2)
    pnl = re.search(r"Best by held-out net P&L \(\+?(-?[\d.]+) USDC test, \+?(-?[\d.]+) train\)",
                    stdout)
    verdict = re.search(r"VERDICT:\s*(\d+)/(\d+) configs ROBUST", stdout)
    return best, pnl, verdict


def read_env_vars(env_path, names):
    cur = {}
    if not os.path.exists(env_path):
        return cur
    with open(env_path) as f:
        for line in f:
            mm = re.match(r"\s*([A-Z_]+)\s*=\s*(.*?)\s*$", line)
            if mm and mm.group(1) in names:
                cur[mm.group(1)] = mm.group(2)
    return cur


def find_current_row(csv_path, cur):
    """Find the grid row that best matches the current .env config (fixed-trail), so we
    can show a fair head-to-head. min_metric is quantile-derived in the grid, so we match
    the other knobs exactly and pick the closest swept threshold."""
    def fnum(x):
        try: return float(x)
        except (TypeError, ValueError): return None
    want = {
        "metric": cur.get("MOMENTUM_RANK_METRIC"),
        "trail": fnum(cur.get("MOMENTUM_TRAIL_PCT")),
        "lookback": fnum(cur.get("MOMENTUM_LOOKBACK_OBS")),
        "maxrun": fnum(cur.get("MOMENTUM_MAX_RUN_PCT")),
        "rotate": fnum(cur.get("MOMENTUM_ROTATE_MARGIN")),
        "min": fnum(cur.get("MOMENTUM_MIN_METRIC")),
        "regime_mode": (cur.get("MOMENTUM_REGIME_MODE") or "").lower() or None,
        "regime_obs": fnum(cur.get("MOMENTUM_REGIME_OBS")),
        "regime_thr": fnum(cur.get("MOMENTUM_REGIME_TREND_MIN")),
        "z_obs": fnum(cur.get("MOMENTUM_ENTRY_MAX_Z_OBS")) or 0.0,
        "z": fnum(cur.get("MOMENTUM_ENTRY_MAX_Z")),
    }
    best_row, best_d = None, None
    with open(csv_path) as f:
        for row in csv.DictReader(f):
            if row["vol_stop_mode"] != "off":
                continue
            if want["metric"] and row["metric"] != want["metric"]:
                continue
            # Regime is a grid dimension: match mode+window exactly so the CURRENT row
            # is the same gate the live trader runs, not an arbitrary regime variant.
            if want["regime_mode"] and row.get("regime_mode", "").lower() != want["regime_mode"]:
                continue
            if want["regime_obs"] is not None and fnum(row.get("regime_filter_obs")) != want["regime_obs"]:
                continue
            # Overbought z-gate is a grid dimension too: match the window exactly (0 = off)
            # so the CURRENT row replays with the same gate the live trader runs.
            if (fnum(row.get("entry_max_z_obs")) or 0.0) != want["z_obs"]:
                continue
            for k, col in (("trail", "trail_pct"), ("lookback", "lookback_obs"),
                           ("maxrun", "max_run_pct"), ("rotate", "rotate_margin")):
                if want[k] is not None and fnum(row[col]) != want[k]:
                    break
            else:
                # Thresholds are quantile-derived in the grid: pick the closest, min_metric first.
                d = (abs(fnum(row["min_metric"]) - (want["min"] or 0.0)),
                     abs((fnum(row.get("entry_max_z")) or 0.0) - (want["z"] or 0.0))
                     if want["z_obs"] else 0.0,
                     abs((fnum(row.get("regime_threshold")) or 0.0) - (want["regime_thr"] or 0.0)))
                if best_d is None or d < best_d:
                    best_d, best_row = d, row
    return best_row


def rate(row, slc):
    """$/hour-deployed for one slice; 0.0 when never in market (or old CSV w/o the column)."""
    pnl = float(row[f"net_pnl_{slc}"])
    hold = float(row.get(f"hold_hours_{slc}") or 0.0)
    return pnl / hold if hold > 0 else 0.0


def sqn(row, slc):
    """System Quality Number for one slice: sqrt(n) * mean(trade P&L) / std(trade P&L).
    The Pareto objective's scalarization — high when profits are BOTH large and evenly
    distributed across trades; a config carried by one outlier scores low. std is floored
    so a (rare) perfectly-uniform profitable config doesn't divide by zero."""
    n = int(row[f"n_trades_{slc}"])
    if n < 2:
        return 0.0
    mean = float(row[f"net_pnl_{slc}"]) / n
    std = max(float(row.get(f"pnl_std_{slc}") or 0.0), 0.01)
    return (n ** 0.5) * mean / std


def worst_slice(row, objective):
    """The selection key: min over train/test in the objective's units."""
    if objective == "pnl-per-hold":
        return min(rate(row, "test"), rate(row, "train"))
    if objective == "pareto":
        return min(sqn(row, "test"), sqn(row, "train"))
    return min(float(row["net_pnl_test"]), float(row["net_pnl_train"]))


def pareto_frontier(rows):
    """Non-dominated set on (maximize worst-slice net P&L, minimize max-slice trade-σ),
    sorted money-first. Printed so the smoothness-vs-money trade is always visible."""
    def key(r):
        return (min(float(r["net_pnl_test"]), float(r["net_pnl_train"])),
                max(float(r.get("pnl_std_test") or 0.0), float(r.get("pnl_std_train") or 0.0)))
    frontier = []
    for r in rows:
        pnl_r, std_r = key(r)
        if not any((key(o)[0] >= pnl_r and key(o)[1] < std_r) or
                   (key(o)[0] > pnl_r and key(o)[1] <= std_r) for o in rows):
            frontier.append(r)
    # Dedupe by (pnl, std) point; keep highest-SQN representative per point.
    seen = {}
    for r in frontier:
        k = key(r)
        if k not in seen or worst_slice(r, "pareto") > worst_slice(seen[k], "pareto"):
            seen[k] = r
    return sorted(seen.values(), key=lambda r: -key(r)[0])


def perf(row):
    if not row:
        return "  (current config not represented in the grid — threshold outside swept range)"
    hold_te = float(row.get("hold_hours_test") or 0.0)
    # Honest mark-to-market drawdown (% of account equity, unrealized included). Falls back
    # to the legacy realized-profit dd only for old CSVs without the mtm_dd_test column.
    mtm = row.get("mtm_dd_test")
    if mtm in (None, "", "NaN", "nan"):
        mtm = row.get("max_dd_test")
    dd_str = f"{float(mtm):.1f}%" if mtm not in (None, "", "NaN", "nan") else "n/a"
    return (f"  test {float(row['net_pnl_test']):+.2f} | train {float(row['net_pnl_train']):+.2f} "
            f"| worst {min(float(row['net_pnl_test']), float(row['net_pnl_train'])):+.2f} "
            f"| $/h te {rate(row, 'test'):+.3f} tr {rate(row, 'train'):+.3f} (in-mkt {hold_te:.1f}h) "
            f"| trades {row['n_trades_test']}/{row['n_trades_train']} "
            f"| win {float(row['win_rate_test']):.0f}% | mtmDD {dd_str}")


def apply_env(env_path, changes):
    shutil.copy2(env_path, env_path + ".bak")
    with open(env_path) as f:
        lines = f.readlines()
    done = set()
    for i, line in enumerate(lines):
        mm = re.match(r"(\s*)([A-Z_]+)(\s*=\s*).*?(\s*)$", line)
        if mm and mm.group(2) in changes:
            key = mm.group(2)
            lines[i] = f"{mm.group(1)}{key}{mm.group(3)}{changes[key]}\n"
            done.add(key)
    # Any managed var missing from .env: append it.
    missing = [k for k in changes if k not in done]
    if missing:
        lines.append("\n# Added by optimize-momentum-config\n")
        for k in missing:
            lines.append(f"{k}={changes[k]}\n")
    with open(env_path, "w") as f:
        f.writelines(lines)


def run_per_token(binp, root, tokens, min_trades, apply):
    """Optimize the per-token params by invoking the `per-token-tune` subcommand (reuses the
    Rust engine: per-token grid + 3-arm validation, and with --apply writes the best
    {min_metric,trail_pct,max_run_pct} per token into momentum_tokens.json). Relays output.
    Note: per-token-tune re-grids the global config internally for its A/B validation arms,
    so the global grid runs twice in a full invocation — fast and keeps both tools
    self-contained."""
    cmd = [binp, "per-token-tune", "--tokens", tokens, "--min-trades", str(min_trades)]
    if apply:
        cmd.append("--apply")
    proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
    sys.stdout.write(proc.stdout)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr[-2000:])
        print("\n(per-token-tune failed; the global .env optimization above is unaffected.)")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tokens", default="assets/momentum_tokens.json")
    ap.add_argument("--env", default=".env")
    ap.add_argument("--min-trades", type=int, default=3)
    ap.add_argument("--csv", default=None, help="grid CSV output path (default: temp file)")
    ap.add_argument("--apply", action="store_true", help="write .env (backs up to .env.bak)")
    ap.add_argument("--per-token", action="store_true",
                    help="ALSO optimize per-token params and (with --apply) write them into "
                         "momentum_tokens.json. OFF by default — per-token tuning overfits this "
                         "sample (NOT SUPPORTED at single-slot AND hold-all); the default is "
                         "global .env only and never touches momentum_tokens.json.")
    ap.add_argument("--no-trades", action="store_true",
                    help="skip the per-trade listing of the winning config (shown by default).")
    ap.add_argument("--entry-max-z-obs", type=int, default=480,
                    help="overbought entry-gate window (obs) to sweep (default 480 — the gate "
                         "is a managed knob and swept by default; 0 disables the dimension, "
                         "~4x faster grid).")
    ap.add_argument("--entry-max-zs", default="1.0,1.5,2.0",
                    help="comma-separated overbought-gate z thresholds to sweep. The gate-off "
                         "variant is always included, so this only widens the search.")
    ap.add_argument("--objective", default="pareto",
                    choices=["pareto", "net-pnl", "pnl-per-hold"],
                    help="winner selection: pareto (default; best worst-slice SQN = "
                         "sqrt(n)*mean/std of per-trade P&L — maximum profit with minimum "
                         "variance between gains; the (P&L, trade-σ) Pareto frontier is "
                         "printed so the pure-money alternative stays visible), net-pnl "
                         "(best worst-slice absolute P&L), or pnl-per-hold (best "
                         "worst-slice $/hour-deployed).")
    args = ap.parse_args()

    root = repo_root()
    os.chdir(root)
    binp = ensure_binary(root)
    csv_path = args.csv or os.path.join(tempfile.gettempdir(), "momentum_grid.csv")

    # Surface the objective + execution assumptions before the run. Slippage/cost are NOT
    # overridden here: the grid's base_params reads MOMENTUM_SLIPPAGE_BPS / MOMENTUM_MAX_COST_BPS
    # straight from .env (via dotenv), so the scan optimizes at the fills you've configured live.
    # (Measured Jupiter round-trip costs for the current liquid names are ~0-3 bps, so the
    # configured tolerance is already the conservative bound — no extra margin is applied.)
    envcfg = read_env_vars(args.env, ["MOMENTUM_SLIPPAGE_BPS", "MOMENTUM_MAX_COST_BPS"])
    obj_desc = ("total net P&L (worst-slice)" if args.objective == "net-pnl"
                else "$/hour-deployed — capital efficiency, NOT total P&L")
    print(f"Objective: {args.objective} — {obj_desc}")
    print(f"Execution (from {args.env}): slippage={envcfg.get('MOMENTUM_SLIPPAGE_BPS', '(unset → sim default)')}bps "
          f"max_cost={envcfg.get('MOMENTUM_MAX_COST_BPS', '(unset → sim default)')}bps\n")

    stdout, trades_txt = run_grid(binp, root, args.tokens, csv_path, args.min_trades,
                                  args.objective,
                                  dump_trades=not args.no_trades,
                                  entry_max_z_obs=args.entry_max_z_obs,
                                  entry_max_zs=args.entry_max_zs)
    best_block, pnl, verdict = parse_best_block(stdout)

    # Build the winning values for the 6 managed knobs (prefer CSV's exact best row so
    # formatting/values are authoritative, fall back to the stdout paste block).
    winner, win_row = {}, None
    with open(csv_path) as f:
        reader = csv.DictReader(f)
        if args.objective == "pnl-per-hold" and "hold_hours_test" not in (reader.fieldnames or []):
            sys.exit("\nCSV lacks hold_hours columns — the momentum-sim binary is stale. "
                     "Rebuild it: cargo build --release --bin momentum-sim")
        rows = [r for r in reader if r["vol_stop_mode"] == "off"
                and float(r["net_pnl_test"]) > 0 and float(r["net_pnl_train"]) > 0
                and int(r["n_trades_test"]) >= args.min_trades
                and int(r["n_trades_train"]) >= args.min_trades]
    if not rows:
        sys.exit("\nNo robust fixed-trail config found (profitable in BOTH slices). "
                 "Leaving .env untouched. Try a longer price history or --min-trades 2.")
    # Winner = best worst-slice value in the objective's units (matches the sim's own
    # `dependability` ranking, restricted to the fixed-trail rows the live trader can run).
    if args.objective == "pareto" and "pnl_std_test" not in rows[0]:
        sys.exit("\nCSV lacks pnl_std columns — the momentum-sim binary is stale. "
                 "Rebuild it: cargo build --release --bin momentum-sim")
    win_row = max(rows, key=lambda r: worst_slice(r, args.objective))
    for col, env in MANAGED:
        winner[env] = fmt(col, win_row[col])

    names = [env for _, env in MANAGED]
    cur = read_env_vars(args.env, names)
    cur_row = find_current_row(csv_path, cur)

    print("\n" + "=" * 70)
    if verdict:
        print(f"ROBUST configs: {verdict.group(1)}/{verdict.group(2)} "
              f"(profitable in BOTH train+test, >={args.min_trades} trades each, fixed-trail)")
    sel_desc = {"pnl-per-hold": "worst-slice $/hour-deployed (capital efficiency, N=1)",
                "pareto": "worst-slice SQN (max P&L, min variance between gains)",
                "net-pnl": "worst-slice net P&L"}[args.objective]
    print(f"\nHEAD-TO-HEAD (held-out slice; winner selected by {sel_desc}):")
    print("CURRENT (.env):")
    print(perf(cur_row))
    print("BEST (grid):")
    print(perf(win_row))
    print(f"  regime: {win_row['regime_mode']} (obs={win_row['regime_filter_obs']}, "
          f"thr={float(win_row.get('regime_threshold') or 0):.4f}) — MANAGED; applied with the config")
    # Overbought entry gate is MANAGED like the regime: swept as a grid dimension and
    # the winner's choice (including "off") is applied with the config.
    if int(win_row.get("entry_max_z_obs") or 0) > 0:
        print(f"  overbought gate: z<={float(win_row['entry_max_z']):.2f} over "
              f"{win_row['entry_max_z_obs']}obs — MANAGED; applied with the config")
    elif args.entry_max_z_obs > 0:
        print("  overbought gate: OFF in the winning config — the sweep tried it and it "
              "didn't help out-of-sample (MANAGED; off is applied)")

    # The (worst-slice P&L, max-slice trade-σ) Pareto frontier: the money pole and the
    # smoothness pole are structurally opposed (a runner IS variance) — always show both
    # so the winner's position on that trade is explicit.
    if args.objective == "pareto":
        front = pareto_frontier(rows)[:6]
        print("\nPARETO FRONTIER (worst-slice P&L vs max-slice trade-σ):")
        for r in front:
            mark = "  <== selected (best SQN)" if r is win_row else ""
            zdesc = (f"/z {float(r['entry_max_z']):.1f}@{r['entry_max_z_obs']}"
                     if int(r.get("entry_max_z_obs") or 0) > 0 else "/z off")
            print(f"  {r['metric']}/min {float(r['min_metric']):.4f}/trail {r['trail_pct']}"
                  f"/lb {r['lookback_obs']}/regime {r['regime_mode']}@{r['regime_filter_obs']}{zdesc}: "
                  f"pnl {min(float(r['net_pnl_test']), float(r['net_pnl_train'])):+.2f}, "
                  f"σ {max(float(r.get('pnl_std_test') or 0), float(r.get('pnl_std_train') or 0)):.1f}, "
                  f"SQN {worst_slice(r, 'pareto'):.2f}{mark}")
        if win_row not in front:
            print(f"  (selected winner is SQN-best off-frontier: "
                  f"SQN {worst_slice(win_row, 'pareto'):.2f})")

    print("\nPROPOSED .env CHANGES:")
    changes = {}
    for env in names:
        new = winner[env]
        old = cur.get(env, "(unset)")
        if str(old) != str(new):
            changes[env] = new
            print(f"  {env}: {old} -> {new}")
    if not changes:
        print("  none — current .env already matches the grid's best config.")

    # Guard: only apply if the winner genuinely beats the incumbent out-of-sample,
    # measured in the selection objective's own units.
    if changes and cur_row:
        unit = {"pnl-per-hold": "$/h", "pareto": "SQN", "net-pnl": "USDC"}[args.objective]
        cur_worst = worst_slice(cur_row, args.objective)
        new_worst = worst_slice(win_row, args.objective)
        if new_worst <= cur_worst:
            print(f"\nNOTE: best worst-slice ({new_worst:+.3f} {unit}) does NOT beat current "
                  f"({cur_worst:+.3f} {unit}). Consider keeping the current config.")
        # Secondary, informational: absolute-P&L cost of a non-money objective's pick.
        if args.objective in ("pnl-per-hold", "pareto"):
            cur_pnl = min(float(cur_row["net_pnl_test"]), float(cur_row["net_pnl_train"]))
            new_pnl = min(float(win_row["net_pnl_test"]), float(win_row["net_pnl_train"]))
            if new_pnl < cur_pnl:
                print(f"NOTE: the $/h winner's worst-slice P&L ({new_pnl:+.2f}) is below the "
                      f"current config's ({cur_pnl:+.2f}) — it trades absolute money for "
                      f"capital efficiency.")

    if args.apply and changes:
        apply_env(args.env, changes)
        print(f"\nApplied. Backup written to {args.env}.bak")
    elif changes:
        print("\nPreview only. Re-run with --apply to write .env (a .env.bak is created).")

    # ── Per-token optimization (momentum_tokens.json) — OPT-IN only ────────────
    # Default: global .env only. Per-token tuning overfits this sample (the 3-arm
    # validation is NOT SUPPORTED at single-slot AND hold-all), so it never runs unless
    # the operator explicitly asks for it; momentum_tokens.json is left untouched.
    if args.per_token:
        print("\n" + "=" * 70)
        print("PER-TOKEN OPTIMIZATION (momentum_tokens.json) — best {min_metric, trail, "
              "max_run} per token + 3-arm validation [opt-in via --per-token]:")
        run_per_token(binp, root, args.tokens, args.min_trades, args.apply)

    # ── Trade-by-trade listing of the winning config (entry/exit, token, P&L) ──
    if trades_txt:
        print("\n" + "=" * 70)
        print("TRADE LIST — most-dependable robust config "
              "(regime-off single-slot replay; the knobs the optimizer writes to .env):")
        print(trades_txt)

    print("\nCaveat: this is a backtest optimum on a finite history (often small trade "
          "counts, understated drawdown). The global .env config AND the per-token params "
          "are hypotheses to validate in paper mode (the multi-slot trader consumes "
          "per-token params), not proven edges.")


if __name__ == "__main__":
    main()
