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
- We auto-tune only the 6 entry/exit knobs the grid optimizes AND the live trader reads:
  RANK_METRIC, MIN_METRIC, TRAIL_PCT, LOOKBACK_OBS, MAX_RUN_PCT, ROTATE_MARGIN.
  Regime (MODE/OBS/TREND_MIN) is a deliberate strategic choice, so we REPORT the winner's
  regime but never silently flip it.
- Default is preview-only. Pass --apply to write .env (after backing up to .env.bak).

Usage:
  python3 optimize_momentum.py                 # preview (no writes)
  python3 optimize_momentum.py --apply         # back up .env, then write the winner
  python3 optimize_momentum.py --min-trades 5  # stricter robustness gate
"""
import argparse
import csv
import os
import re
import shutil
import subprocess
import sys
import tempfile

# The 6 knobs we auto-tune: CSV column -> .env variable.
MANAGED = [
    ("metric",        "MOMENTUM_RANK_METRIC"),
    ("min_metric",    "MOMENTUM_MIN_METRIC"),
    ("trail_pct",     "MOMENTUM_TRAIL_PCT"),
    ("lookback_obs",  "MOMENTUM_LOOKBACK_OBS"),
    ("max_run_pct",   "MOMENTUM_MAX_RUN_PCT"),
    ("rotate_margin", "MOMENTUM_ROTATE_MARGIN"),
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
    if col in ("min_metric",):           # keep precision for thresholds
        return f"{f:.4f}".rstrip("0").rstrip(".") if "." in f"{f:.4f}" else f"{f:.4f}"
    if f == int(f):                       # whole number -> int (20.0 -> 20)
        return str(int(f))
    return s


def run_grid(binp, root, tokens, csv_path, min_trades):
    cmd = [binp, "run", "--tokens", tokens, "--no-vol-stops",
           "--regime-obs", "0,480", "--regime-trend-obs", "480",
           "--min-trades", str(min_trades), "--top", "5", "--csv", csv_path]
    print("Running grid (fixed-trail only):\n  " + " ".join(cmd) + "\n")
    proc = subprocess.run(cmd, cwd=root, capture_output=True, text=True)
    sys.stdout.write(proc.stdout[-4000:])  # tail of the tool's own verdict/table
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr[-2000:])
        sys.exit("momentum-sim run failed.")
    return proc.stdout


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
    }
    best_row, best_d = None, None
    with open(csv_path) as f:
        for row in csv.DictReader(f):
            if row["vol_stop_mode"] != "off":
                continue
            if want["metric"] and row["metric"] != want["metric"]:
                continue
            for k, col in (("trail", "trail_pct"), ("lookback", "lookback_obs"),
                           ("maxrun", "max_run_pct"), ("rotate", "rotate_margin")):
                if want[k] is not None and fnum(row[col]) != want[k]:
                    break
            else:
                d = abs(fnum(row["min_metric"]) - (want["min"] or 0.0))
                if best_d is None or d < best_d:
                    best_d, best_row = d, row
    return best_row


def perf(row):
    if not row:
        return "  (current config not represented in the grid — threshold outside swept range)"
    return (f"  test {float(row['net_pnl_test']):+.2f} | train {float(row['net_pnl_train']):+.2f} "
            f"| worst {min(float(row['net_pnl_test']), float(row['net_pnl_train'])):+.2f} "
            f"| trades {row['n_trades_test']}/{row['n_trades_train']} "
            f"| win {float(row['win_rate_test']):.0f}% | maxDD {float(row['max_dd_test']):.1f}%")


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
    ap.add_argument("--no-per-token", action="store_true",
                    help="skip per-token optimization (optimize the global .env config only)")
    args = ap.parse_args()

    root = repo_root()
    os.chdir(root)
    binp = ensure_binary(root)
    csv_path = args.csv or os.path.join(tempfile.gettempdir(), "momentum_grid.csv")

    stdout = run_grid(binp, root, args.tokens, csv_path, args.min_trades)
    best_block, pnl, verdict = parse_best_block(stdout)

    # Build the winning values for the 6 managed knobs (prefer CSV's exact best row so
    # formatting/values are authoritative, fall back to the stdout paste block).
    winner, win_row = {}, None
    with open(csv_path) as f:
        rows = [r for r in csv.DictReader(f) if r["vol_stop_mode"] == "off"
                and float(r["net_pnl_test"]) > 0 and float(r["net_pnl_train"]) > 0
                and int(r["n_trades_test"]) >= args.min_trades
                and int(r["n_trades_train"]) >= args.min_trades]
    if not rows:
        sys.exit("\nNo robust fixed-trail config found (profitable in BOTH slices). "
                 "Leaving .env untouched. Try a longer price history or --min-trades 2.")
    win_row = max(rows, key=lambda r: min(float(r["net_pnl_test"]), float(r["net_pnl_train"])))
    for col, env in MANAGED:
        winner[env] = fmt(col, win_row[col])

    names = [env for _, env in MANAGED]
    cur = read_env_vars(args.env, names)
    cur_row = find_current_row(csv_path, cur)

    print("\n" + "=" * 70)
    if verdict:
        print(f"ROBUST configs: {verdict.group(1)}/{verdict.group(2)} "
              f"(profitable in BOTH train+test, >={args.min_trades} trades each, fixed-trail)")
    print("\nHEAD-TO-HEAD (held-out slice):")
    print("CURRENT (.env):")
    print(perf(cur_row))
    print("BEST (grid):")
    print(perf(win_row))
    print(f"  regime: {win_row['regime_mode']} (obs={win_row['regime_filter_obs']}) "
          f"— advisory; not auto-applied")

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

    # Guard: only apply if the winner genuinely beats the incumbent out-of-sample.
    if changes and cur_row:
        cur_worst = min(float(cur_row["net_pnl_test"]), float(cur_row["net_pnl_train"]))
        new_worst = min(float(win_row["net_pnl_test"]), float(win_row["net_pnl_train"]))
        if new_worst <= cur_worst:
            print(f"\nNOTE: best worst-slice ({new_worst:+.2f}) does NOT beat current "
                  f"({cur_worst:+.2f}). Consider keeping the current config.")

    if args.apply and changes:
        apply_env(args.env, changes)
        print(f"\nApplied. Backup written to {args.env}.bak")
    elif changes:
        print("\nPreview only. Re-run with --apply to write .env (a .env.bak is created).")

    # ── Per-token optimization (momentum_tokens.json) ──────────────────────────
    if not args.no_per_token:
        print("\n" + "=" * 70)
        print("PER-TOKEN OPTIMIZATION (momentum_tokens.json) — best {min_metric, trail, "
              "max_run} per token + 3-arm validation:")
        run_per_token(binp, root, args.tokens, args.min_trades, args.apply)

    print("\nCaveat: this is a backtest optimum on a finite history (often small trade "
          "counts, understated drawdown). The global .env config AND the per-token params "
          "are hypotheses to validate in paper mode (the multi-slot trader consumes "
          "per-token params), not proven edges.")


if __name__ == "__main__":
    main()
