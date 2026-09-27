#!/usr/bin/env python3
"""
book_ab.py — the one place interactions are measured: the whole book at the LIVE N, the
candidate tokens file (per-token picks) against the deployed file, on the full split and on
the same calendar windows as the per-token jobs.

Why it exists: the grid scores each token alone (N=1, its own slot). Live runs N slots for
all tokens, so per-token winners can interact (slot/capital). Per-token changes from separate
sweeps are never applied without a whole-book check (the 2026-09-15 A/B/C/D lesson).

Each run is a 1-cell per-token-sweep whose INCUMBENT row is the whole book at that file's
params. One cost for both arms (the book replay applies a single MOMENTUM_SLIPPAGE_BPS): the
notional-weighted mean of the per-token costs, rounded up — this measures the Δ from
interaction, not absolute P&L.

Usage:
  python3 book_ab.py --run-dir <run> [--candidate <run>/candidate_tokens.json] [--max-n 10] [-j 2]
"""
import argparse
import concurrent.futures as cf
import json
import math
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402
import run_sweeps  # noqa: E402


def back_window_names(jobs) -> list:
    """The back-half windows only (f1..fK) — never `full` (which also starts with "f") or `f0`.
    The additivity check sums exactly these, like per_trail_report.delta_sum_for does."""
    return [w for w in jobs if re.fullmatch(r"f[1-9][0-9]*", w)]


def one_cell_axes(entry: dict, env: dict) -> dict:
    k = common.deployed_knobs(entry["params"], env)
    return {"mins": [common.round4(k["min"])], "trails": [k["trail"]], "lookbacks": [k["lb"]],
            "zs": [k["z"]], "fracs": [float(common.fmt_frac(k["fb"]))]}


def book_window_files(book: Path, wins: dict, out_dir: Path) -> dict:
    out_dir.mkdir(parents=True, exist_ok=True)
    rows = [json.dumps(r, separators=(",", ":")) for r in common.iter_jsonl(book)]
    ts = [json.loads(r)["ts"] for r in rows]
    n = len(rows)
    full = out_dir / "book.full.jsonl"
    if not full.exists():
        full.write_text("\n".join(rows) + "\n")
    files = {"full": (full, run_sweeps.exact_frac(n, max(1, sum(1 for t in ts if t < wins["split_ts"]))))}
    for name, span in wins.items():
        if name == "split_ts":  # a timestamp, not a window
            continue
        a, b = span
        idx = [i for i, t in enumerate(ts) if a <= t < b]
        if len(idx) < 200:
            continue
        lo = idx[0]
        body = rows[max(0, lo - 1): idx[-1] + 1]
        p = out_dir / f"book.{name}.jsonl"
        p.write_text("\n".join(body) + "\n")
        files[name] = (p, run_sweeps.exact_frac(len(body), 1))
    return files


def run_arm(root, arm, tokens_path, files, env, cost, max_n, work, slots, guard) -> dict:
    entries = [e for e in common.load_tokens(tokens_path) if e.get("params") and not common.is_watch_only(e)]
    target = entries[0]
    axes = one_cell_axes(target, env)
    res = {}

    def job(name):
        hist, frac = files[name]
        csvp = work / f"{arm}.{name}.csv"
        sargs = run_sweeps.sweep_args(target["symbol"], hist, tokens_path, frac, axes, csvp)
        i = sargs.index("--max-n")
        sargs[i + 1] = str(max_n)
        sargs.append("--no-regime-sweep")
        rc = common.run_sim(root, sargs, work / f"{arm}.{name}.txt", {"MOMENTUM_SLIPPAGE_BPS": cost},
                            slots=slots, guard=guard)
        if rc != 0 and common.STOPPING.is_set():
            raise SystemExit(143)  # stopped on purpose — not a failure to report
        if rc != 0:
            sys.exit(f"book A/B {arm} {name} failed — see {work / (arm + '.' + name + '.txt')}")
        inc = next(r for r in common.read_sweep_csv(csvp) if r["cell"] == "INCUMBENT")
        return name, inc

    with cf.ThreadPoolExecutor(max_workers=slots) as pool:
        for name, inc in pool.map(job, list(files)):
            res[name] = inc
    return res


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--run-dir", required=True)
    ap.add_argument("--candidate", default=None)
    ap.add_argument("--max-n", type=int, default=None, help="default: live MOMENTUM_MAX_POSITIONS from the manifest")
    ap.add_argument("-j", type=int, default=2)
    args = ap.parse_args()

    root = common.repo_root()
    run_dir = Path(args.run_dir)
    man = common.read_json(run_dir / "manifest.json")
    if not man:
        sys.exit(f"no manifest in {run_dir}")
    env = common.read_env(root)
    book = Path(man["book"])
    cand = Path(args.candidate) if args.candidate else run_dir / "candidate_tokens.json"
    if not cand.exists():
        sys.exit(f"no {cand} — write it with apply_params.py --run-dir {run_dir} --from-verdicts")
    deployed = run_dir / "deployed_tokens.json"
    max_n = args.max_n or int(man.get("live_max_positions", 1))

    # T2 at A/B time: the per-token jobs are only comparable with an A/B run on the same inputs.
    now = run_sweeps.current_state(root, root / "assets" / "momentum_tokens.json", book, env)
    drift = [k for k in run_sweeps.MANIFEST_KEYS if k != "tokens_sha" and man.get(k) != now[k]]
    if drift:
        sys.exit(f"inputs changed since the per-token jobs ran ({', '.join(drift)}) — re-run run_sweeps.py first")
    # The "whole book" must contain every token of both files, or the A/B silently compares a subset.
    present = set()
    for row in common.iter_jsonl(book):
        present.update(row["prices"])
    wanted = {e["mint"]: e["symbol"] for path in (deployed, cand) for e in common.load_tokens(path)
              if e.get("params") and not common.is_watch_only(e)}
    absent = sorted(sym for mint, sym in wanted.items() if mint not in present)
    if absent:
        sys.exit(f"the book lacks {absent} — rebuild it with ensure_history.py --build (all deployed series)")

    costs, weights = man.get("costs", {}), {}
    default_notional = float(env.get("MOMENTUM_TRADE_USDC", 100))
    for e in common.load_tokens(deployed):
        if e["symbol"] in costs:
            weights[e["symbol"]] = float((e.get("params") or {}).get("trade_usdc", default_notional))
    cost = math.ceil(sum(costs[s] * w for s, w in weights.items()) / sum(weights.values())) if weights else \
        int(float(env.get("MOMENTUM_SLIPPAGE_BPS", 50)))

    cov = common.read_json(run_dir / "coverage.json")
    k = max(man.get("k", {}).values() or [5])
    wins = run_sweeps.windows_for(cov["first"], cov["last"], man.get("train_frac", 0.7), k)
    work = run_dir / "book_ab"
    files = book_window_files(book, wins, work / "hist")
    guard = common.Tripwire(book)
    common.install_cascade(f"optimize-momentum-tokens:book_ab:{run_dir.name}")
    print(f"book A/B: N={max_n}, cost {cost} bps/leg (notional-weighted), jobs {list(files)}")
    arms = {arm: run_arm(root, arm, path, files, env, cost, max_n, work, args.j, guard)
            for arm, path in (("deployed", deployed), ("candidate", cand))}

    back = back_window_names(files)
    rows, dsum = [], 0.0
    for name in files:
        d, c = arms["deployed"][name], arms["candidate"][name]
        dv, cv = (d["pnl_test"], c["pnl_test"])
        rows.append({"job": name, "deployed": dv, "candidate": cv, "delta": cv - dv,
                     "deployed_train": d["pnl_train"] if name == "full" else None,
                     "candidate_train": c["pnl_train"] if name == "full" else None,
                     "worst_deployed": d["worst_test"], "worst_candidate": c["worst_test"]})
        if name in back:
            dsum += cv - dv
    import per_trail_report  # noqa: E402
    per_token, own_span = 0.0, []
    for p in sorted(run_dir.glob("verdicts/*.json")):
        v = json.loads(p.read_text())
        if v.get("verdict") != "change" or not v.get("pick"):
            continue
        if man.get("k", {}).get(v["token"], k) != k:  # SHORT token: its own-span windows ≠ the book's
            own_span.append(v["token"])
            continue
        per_token += per_trail_report.delta_sum_for(run_dir, v["token"], v["pick"])
    result = {"max_n": max_n, "cost_bps": cost, "rows": rows, "book_delta_sum": dsum,
              "per_token_delta_sum": per_token, "additivity_gap": dsum - per_token,
              "excluded_from_additivity": own_span}
    common.write_json(run_dir / "book_ab.json", result)
    md = [f"Whole-book A/B at N={max_n} (cost {cost} bps/leg for both arms):", "",
          "| window | deployed | candidate | Δ | worst dep / cand |", "|---|---|---|---|---|"]
    for r in rows:
        md.append(f"| {r['job']} | {r['deployed']:+.2f} | {r['candidate']:+.2f} | {r['delta']:+.2f} | "
                  f"{r['worst_deployed']:+.2f} / {r['worst_candidate']:+.2f} |")
    md += ["", f"Back-window Δ Σ: book {dsum:+.2f} vs Σ per-token picks {per_token:+.2f} → additivity gap "
               f"{dsum - per_token:+.2f}" + ("  ⚠ material: split the changes into A/B/C/D arms before applying"
                                             if abs(dsum - per_token) > max(5.0, 0.25 * abs(per_token)) else "")]
    if own_span:
        md.append(f"Not in the additivity sum (SHORT, own-span windows): {', '.join(own_span)} — their effect is "
                  f"inside the book Δ only.")
    (run_dir / "book_ab.md").write_text("\n".join(md) + "\n")
    print("\n".join(md))


if __name__ == "__main__":
    main()
