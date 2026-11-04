#!/usr/bin/env python3
"""
to_minutes.py — move ROW-lookback tokens to the time window their backtest actually scored.

A row lookback (`lookback_obs` without `lookback_min`) counts snapshots, and the two sides count
different things: live writes one snapshot per minute for every token, while the book holds the
token's own prints and the isolated sim keeps only (3·lb + 50) book rows (sim.rs
`trailing_window_snaps_for`). The same 480 rows were 8 h of JitoSOL live and ~25 h in the sim;
the live trader entered on short moves the backtest never scored (2026-11-03).

For each deployed row token in the book this measures the span of the window the sim used — its
last `lb` prints inside the newest (3·lb + 50) book rows — every 6 h over the token's back half,
and writes a CANDIDATE tokens file with `lookback_min` = the median span (rows kept as the
fallback). An active z-gate gets `entry_max_z_min` = its row count: the sim's row z counts book
rows (one per minute) and live counts one row per minute, so z@N rows is already N minutes on both
sides. The live file is never written — sweep the candidate (`run_sweeps.py --tokens-file`), then
apply the picks with `apply_params.py`.

There is no exact minutes equivalent of a row window (the row span drifts with trading activity,
JitoSOL matched 1/25 trades): the converted value is the sweep's starting point, not a deployable
config.

Usage:
  python3 to_minutes.py                                  # every deployed row token → candidate file
  python3 to_minutes.py --tokens HYPE,STRK [--book …] [--out …]
"""
import argparse
import bisect
import copy
import json
import statistics as st
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

SIM_WINDOW_SAFETY, SIM_WINDOW_PAD = 3, 50  # sim.rs WINDOW_SAFETY / WINDOW_PAD
MIN_POINTS = 121                           # the metric's own floor (SORTINO_MIN_OBS)
SAMPLE_EVERY_ROWS = 360                    # book rows are one minute apart ⇒ every 6 h


def round10(x: float) -> int:
    return int(x / 10 + 0.5) * 10


def measure(book: Path, entries: list, global_lb: int = 0) -> dict:
    """{symbol: stats} for the deployed row tokens with prints in `book` (others are absent). A token
    with no `lookback_obs` of its own (a fresh add/vet) ranks on `global_lb` = MOMENTUM_LOOKBACK_OBS."""
    targets = [e for e in common.deployed_targets(entries) if common.on_rows(e["params"])]
    want = {e["mint"]: e for e in targets}
    ts, idx = [], {m: [] for m in want}
    with book.open() as f:
        for line in f:
            r = json.loads(line)
            i = len(ts)
            ts.append(int(r["ts"]))
            for m in want.keys() & r.get("prices", {}).keys():
                idx[m].append(i)
    out = {}
    for m, e in want.items():
        rows = idx[m]
        if not rows:
            continue
        lb = int(e["params"].get("lookback_obs") or global_lb or 0)
        if lb <= 0:
            continue
        win = lb * SIM_WINDOW_SAFETY + SIM_WINDOW_PAD  # confirm lag 0, as in every live config
        spans, pts = [], []
        for i in range(rows[0] + (len(ts) - rows[0]) // 2, len(ts), SAMPLE_EVERY_ROWS):
            a = bisect.bisect_left(rows, max(0, i - win + 1))
            b = bisect.bisect_right(rows, i)
            if b - a < MIN_POINTS:
                continue
            spans.append((ts[i] - ts[rows[max(a, b - lb)]]) / 60)
            pts.append(min(b - a, lb))
        if not spans:
            continue
        q = st.quantiles(spans, n=10) if len(spans) >= 2 else [spans[0]] * 9
        out[e["symbol"]] = {
            "lb": lb, "lookback_min": round10(st.median(spans)), "median_min": st.median(spans),
            "p10_min": q[0], "p90_min": q[-1], "points": int(st.median(pts)),
            "capped": sum(p < lb for p in pts) / len(pts), "samples": len(spans),
        }
    return out


def convert(entries: list, measured: dict) -> tuple:
    """A copy of `entries` with every measured token on minutes; returns (entries, changed)."""
    out, changed = copy.deepcopy(entries), []
    for e in out:
        m = measured.get(e["symbol"])
        if not m:
            continue
        p = e["params"]
        p["lookback_min"] = m["lookback_min"]
        if (p.get("entry_max_z_obs") or 0) > 0 and (p.get("entry_max_z") or 0) > 0 and not p.get("entry_max_z_min"):
            p["entry_max_z_min"] = int(p["entry_max_z_obs"])
        changed.append(e["symbol"])
    return out, changed


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--book", default=None, help="history book (default: the newest price_history.book_*.jsonl)")
    ap.add_argument("--tokens-file", default="assets/momentum_tokens.json")
    ap.add_argument("--tokens", default=None, help="comma list (default: every deployed row token)")
    ap.add_argument("--out", default="assets/momentum_tokens.minutes_candidate.json")
    args = ap.parse_args()

    root = common.repo_root()
    tokens_file, out = root / args.tokens_file, root / args.out
    if out.resolve() == (root / "assets" / "momentum_tokens.json").resolve():
        sys.exit("refused — --out is the live tokens file; write a candidate and apply picks with apply_params.py")
    book = Path(args.book) if args.book else common.newest_book(root)
    if book is None:
        sys.exit("no history book — run ensure_history.py --build first")
    entries = common.load_tokens(tokens_file)
    only = [s.strip() for s in args.tokens.split(",")] if args.tokens else None
    rows = [e for e in common.deployed_targets(entries, only) if common.on_rows(e["params"])]
    if not rows:
        sys.exit("nothing to convert — every selected deployed token already has lookback_min")
    measured = measure(book, rows, int(common.read_env(root).get("MOMENTUM_LOOKBACK_OBS") or 0))

    print(f"book {book.name} — window the isolated sim scored each ROW token on (median over its back half)")
    print(f"  {'token':<9}{'rows':>5}  {'live':>6}  {'sim median':>10}  {'p10–p90':>13}  {'pts':>4}  {'capped':>6}  → lookback_min")
    for e in rows:
        s, m = e["symbol"], measured.get(e["symbol"])
        if not m:
            print(f"  {s:<9}  not measured — no window with ≥{MIN_POINTS} prints in this book (sweep it later)")
            continue
        print(f"  {s:<9}{m['lb']:>5}  {m['lb'] / 60:5.1f}h  {m['median_min'] / 60:9.1f}h  "
              f"{m['p10_min'] / 60:5.1f}–{m['p90_min'] / 60:5.1f}h  {m['points']:>4}  {m['capped']:>6.0%}  → {m['lookback_min']}")
    new, changed = convert(entries, measured)
    if not changed:
        sys.exit("nothing measurable to convert")
    out.write_text(json.dumps(new, indent=2, ensure_ascii=False) + "\n")
    shown = out.relative_to(root) if out.is_relative_to(root) else out
    z = [e["symbol"] for e in new if e["symbol"] in changed and e["params"].get("entry_max_z_min")]
    print(f"\nwrote candidate {shown} ({', '.join(changed)}); z window → minutes: {', '.join(z) or 'none'}")
    print("the live file is untouched. Next — sweep the candidate on a new run dir:\n"
          f"  python3 .claude/skills/optimize-momentum-tokens/scripts/run_sweeps.py --dry-run "
          f"--tokens {','.join(changed)} --tokens-file {shown} --run-dir <new>")


if __name__ == "__main__":
    main()
