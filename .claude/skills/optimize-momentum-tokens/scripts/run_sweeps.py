#!/usr/bin/env python3
"""
run_sweeps.py — every per-token grid job: isolated single-slot (N=1) replays at each token's
own measured cost, over one train/test split plus disjoint calendar windows.

Per token (8 independent jobs, all on the SAME 2,160-cell grid so their CSVs join by label):
  full    0.7 train/test split of the token's history          → the split objectives
  f0      the front half, as one window                        → tail coverage + early regime
  f1..f5  five equal blocks tiling the back half                → the time-split axes (rule 2b)
  cost3x  the full split at 3× the token's cost                 → cost-robustness (tight trails)

Why each window is its own small file: per_token_sweep replays the train and the test slice
independently (split_at, then a separate ranked_stream + regime_mask_for per slice), so a
window's test result depends only on the window's rows. A one-row prefix + the window, with
train_frac landing the split exactly on the window's first row, reproduces the `rows[:b]`
method's test numbers (verify with --check-exact) at a fraction of the compute.

Isolation (operator decision 2026-09-27): each job's book is a ONE-entry tokens file with
--max-n 1 — the token owns its slot, as it effectively does live at N=10. Interactions are
checked once, by book_ab.py at the live N.

Safety: the sim only ever reads files inside the run dir (never the canonical book), always
with HISTORY_MAX_SNAPSHOTS set, under a machine-wide slot lock; a tripwire on the canonical
book aborts the run if it changes.

Usage:
  python3 run_sweeps.py --dry-run                       # targets, coverage, costs, job count, ETA
  python3 run_sweeps.py [-j 3] [--tokens HYPE,ZEC]      # run (resumable) — use run_in_background
  python3 run_sweeps.py --tokens HYPE --check-exact     # prove the window shortcut on real cells
"""
import argparse
import concurrent.futures as cf
import fcntl
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402
import ensure_history  # noqa: E402

DAY = 86_400
ALL_JOBS = ["full", "f0", "f1", "f2", "f3", "f4", "f5", "cost3x"]
MANIFEST_KEYS = ("env_hash", "tokens_sha", "book_sha", "binary_mtime")
PER_TOKEN_META = ("windows", "k", "axes", "costs")


# ── windows ──────────────────────────────────────────────────────────────────────────────

def windows_for(t0: int, t1: int, train_frac: float, k: int) -> dict:
    """Calendar boundaries: split for `full`, front half `f0`, K equal back-half blocks."""
    mid = t0 + (t1 - t0) / 2.0
    out = {"split_ts": t0 + train_frac * (t1 - t0), "f0": (t0, mid)}
    for i in range(1, k + 1):
        a = mid + (i - 1) * (t1 - mid) / k
        b = mid + i * (t1 - mid) / k if i < k else t1 + 1
        out[f"f{i}"] = (a, b)
    return out


def select_runnable(cov: dict, targets: list, n_windows: int, allow_insufficient=()) -> tuple:
    """(runnable, starts, kwin, skipped). An INSUFFICIENT token named in `allow_insufficient` is swept
    as SHORT (own span, K=3) and its T0 gains a WARN, so the override is printed in every TRUST block."""
    t0 = cov["first"]
    runnable, starts, kwin = [], {}, {}
    for e in targets:
        st = cov["tokens"][e["symbol"]]
        if st["status"] == "INSUFFICIENT" and e["symbol"] in allow_insufficient:
            st["status"] = "SHORT"
            st["t0"] = [n for n in st.get("t0", []) if not (n[0] == "INFO" and "no tuning" in n[1])] + [
                ("WARN", f"INSUFFICIENT ({st['days']} d < 60 d), swept on operator override "
                         f"(--allow-insufficient): own-span K=3, every verdict is a paper-test hypothesis")]
        if st["status"] in ("OK", "SHORT"):
            runnable.append(e)
            starts[e["symbol"]] = st["first"] if st["status"] == "SHORT" else t0
            kwin[e["symbol"]] = 3 if st["status"] == "SHORT" else n_windows
    skipped = {e["symbol"]: cov["tokens"][e["symbol"]]["status"] for e in targets if e not in runnable}
    return runnable, starts, kwin, skipped


def exact_frac(n_rows: int, split_index: int) -> float:
    """train_frac such that the sim's `(n × frac) as usize` is exactly `split_index`."""
    if not 0 < split_index < n_rows:
        raise ValueError(f"split index {split_index} outside (0, {n_rows})")
    return (split_index + 0.5) / n_rows


def fmt_frac_arg(f: float) -> str:
    return repr(float(f))


# ── per-token inputs ─────────────────────────────────────────────────────────────────────

def extract_per_token(book: Path, targets: list, hist_dir: Path, starts: dict) -> dict:
    """One pass over the book: per token, EVERY book row from the token's own start, with prices
    limited to its mint and WSOL (a row may be empty). Keeping the book's snapshot grid matters:
    lookback_obs, the regime mask and the rows where stops are checked all count rows. Measured
    2026-09-27 on HYPE (book3full): dropping rows where only other tokens printed overstated the
    token's in-book test P&L by +23% (+92.90 vs +75.30); on the book grid the gap is +5% (+79.39),
    the residual book_ab.py measures. The book grid is also closer to live (95% vs 89% of minutes)."""
    hist_dir.mkdir(parents=True, exist_ok=True)
    handles = {e["symbol"]: open(hist_dir / f"{e['symbol']}.jsonl", "w") for e in targets}
    counts = {s: 0 for s in handles}
    try:
        for row in common.iter_jsonl(book):
            ts, prices = row["ts"], row["prices"]
            for e in targets:
                sym, mint = e["symbol"], e["mint"]
                if ts < starts[sym]:
                    continue
                keep = {k: prices[k] for k in (mint, common.WSOL) if k in prices}
                handles[sym].write(json.dumps({"ts": ts, "prices": keep}, separators=(",", ":")) + "\n")
                counts[sym] += 1
    finally:
        for h in handles.values():
            h.close()
    return counts


def write_window_files(hist: Path, sym: str, wins: dict, k: int, out_dir: Path, mint: str = None) -> tuple:
    """({job: (history file, exact train_frac)}, {window: token prints}). Windows get a one-row prefix
    so the test slice starts exactly on the window's first row. The print counts feed T0: a window
    without the token's own prints would score every cell a meaningless 0."""
    parsed = list(common.iter_jsonl(hist))
    rows = [json.dumps(r, separators=(",", ":")) for r in parsed]
    ts = [r["ts"] for r in parsed]
    n = len(rows)
    jobs, prints = {}, {}
    split_i = sum(1 for t in ts if t < wins["split_ts"])
    jobs["full"] = (hist, exact_frac(n, max(1, min(split_i, n - 1))))
    jobs["cost3x"] = jobs["full"]
    for name in ["f0"] + [f"f{i}" for i in range(1, k + 1)]:
        a, b = wins[name]
        idx = [i for i, t in enumerate(ts) if a <= t < b]
        prints[name] = sum(1 for i in idx if mint in parsed[i]["prices"]) if mint else len(idx)
        if len(idx) < 200:
            continue  # per_token_sweep refuses < 200 snapshots; the report shows the window as missing
        lo = idx[0]
        body = rows[lo - 1: idx[-1] + 1] if lo > 0 else rows[lo: idx[-1] + 1]
        prefix = 1  # f0 at the file start: its first row doubles as the prefix
        p = out_dir / f"{sym}.{name}.jsonl"
        p.write_text("\n".join(body) + "\n")
        jobs[name] = (p, exact_frac(len(body), prefix))
    return jobs, prints


def load_costs(run_dir: Path, costs_path, uniform_bps, required: bool = True) -> dict:
    if uniform_bps is not None:
        return {"_uniform": int(uniform_bps)}
    p = Path(costs_path) if costs_path else run_dir / "costs.json"
    if not p.exists():
        if not required:
            return {}
        sys.exit(f"no {p} — run measure_costs.py --out {p} first (or pass --cost-bps N for a smoke run)")
    return {sym: rec["used_bps"] for sym, rec in json.loads(p.read_text())["tokens"].items()}


def cost_for(costs: dict, sym: str, env: dict, required: bool = True):
    if "_uniform" in costs:
        return costs["_uniform"]
    if sym not in costs and not required:
        return None
    if sym not in costs:
        sys.exit(f"{sym} missing from costs.json — re-run measure_costs.py for it")
    return int(costs[sym])


# ── manifest ─────────────────────────────────────────────────────────────────────────────

def git_sha(root: Path) -> str:
    r = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=root, capture_output=True, text=True)
    return r.stdout.strip()


def current_state(root: Path, tokens_file: Path, book: Path, env: dict) -> dict:
    return {
        "env_hash": common.env_hash(env),
        "tokens_sha": common.sha256_file(tokens_file)[:12],
        "book_sha": common.sha256_file(book)[:12],
        "binary_mtime": int(common.sim_binary(root).stat().st_mtime),
    }


def init_run_dir(root: Path, run_dir: Path, tokens_file: Path, book: Path, env: dict, meta: dict) -> dict:
    run_dir.mkdir(parents=True, exist_ok=True)
    state = current_state(root, tokens_file, book, env)
    mpath = run_dir / "manifest.json"
    old = common.read_json(mpath)
    meta = json.loads(json.dumps(meta))  # tuples → lists, so a resumed run compares like with like
    if old:
        drift = [k for k in MANIFEST_KEYS if old.get(k) != state[k]]
        drift += [k for k in ("train_frac", "cost_mult") if old.get(k) != meta.get(k)]
        for key in PER_TOKEN_META:
            drift += [f"{key}[{sym}]" for sym, v in meta.get(key, {}).items()
                      if sym in old.get(key, {}) and old[key][sym] != v]
        if drift:
            sys.exit(f"{run_dir} was produced under different inputs or layout ({', '.join(drift)}). "
                     f"Results would mix — use a new --run-dir.")
        added = False
        for key in PER_TOKEN_META + ("skipped",):  # a token swept later joins the same run dir
            for sym, v in meta.get(key, {}).items():
                if sym not in old.setdefault(key, {}):
                    old[key][sym] = v
                    added = True
        if added:
            common.write_json(mpath, old)
        return old
    shutil.copyfile(tokens_file, run_dir / "deployed_tokens.json")
    manifest = dict(state, git=git_sha(root), created=common.utc(time.time()), book=str(book),
                    momentum_env=common.momentum_env(env), **meta)
    common.write_json(mpath, manifest)
    return manifest


# ── jobs ─────────────────────────────────────────────────────────────────────────────────

def sweep_args(sym: str, hist: Path, tokens_json: Path, frac: float, axes: dict, csv_path: Path) -> list:
    j = lambda xs: ",".join(common.fmt_f64(x) for x in xs)
    return ["per-token-sweep", "--history", hist, "--tokens", tokens_json, "--token", sym,
            "--max-n", "1", "--train-frac", fmt_frac_arg(frac),
            "--min-metrics", j(axes["mins"]), "--trails", j(axes["trails"]),
            "--lookbacks", ",".join(str(x) for x in axes["lookbacks"]),
            "--entry-max-zs", j(axes["zs"]), "--entry-max-z-obs", str(common.Z_OBS),
            "--fade-fracs", ",".join(common.fmt_frac(x) for x in axes["fracs"]),
            "--top", "5", "--csv", csv_path]


_SHA_CACHE = {}


def job_spec(args: list, cost: int, hist: Path) -> str:
    """Identity of a job = its arguments, its cost and the CONTENT of its input (not mtime:
    window files are re-created on every resume, and must not force a re-run)."""
    key = str(hist)
    if key not in _SHA_CACHE:
        _SHA_CACHE[key] = common.sha256_file(Path(hist))[:16]
    blob = json.dumps({"args": [str(a) for a in args], "cost": cost, "hist": _SHA_CACHE[key]})
    return common.sha256_bytes(blob.encode())[:16]


def check_exact(root: Path, e: dict, hist: Path, wins: dict, window: str, axes: dict, cost: int,
                work: Path, guard) -> bool:
    """Prove the window shortcut on real cells: the prefix+window file must give the same TEST
    columns as the `rows[:b]` + train_frac a/b method used on 2026-09-15."""
    rows = [json.dumps(r, separators=(",", ":")) for r in common.iter_jsonl(hist)]
    ts = [json.loads(r)["ts"] for r in rows]
    a, b = wins[window]
    ia, ib = sum(1 for t in ts if t < a), sum(1 for t in ts if t < b)
    variants = {"window": (rows[ia - 1: ib], 1), "prefix": (rows[:ib], ia)}
    work.mkdir(parents=True, exist_ok=True)
    tj = work / "tokens.json"
    common.write_json(tj, [e])
    results = {}
    for name, (body, split_i) in variants.items():
        hp = work / f"{name}.jsonl"
        hp.write_text("\n".join(body) + "\n")
        csvp = work / f"{name}.csv"
        rc = common.run_sim(root, sweep_args(e["symbol"], hp, tj, exact_frac(len(body), split_i), axes, csvp),
                            work / f"{name}.txt", {"MOMENTUM_SLIPPAGE_BPS": cost}, slots=1, guard=guard)
        if rc != 0:
            sys.exit(f"check-exact: {name} run failed — see {work / (name + '.txt')}")
        results[name] = {r["cell"]: r for r in common.read_sweep_csv(csvp)}
    cols = ("pnl_test", "trades_test", "worst_test", "true_dd_test", "best_test", "open_test")
    worst = 0.0
    for cell, r in results["window"].items():
        o = results["prefix"].get(cell)
        if o is None:
            print(f"  {cell}: missing in the prefix run")
            return False
        for c in cols:
            worst = max(worst, abs(float(r[c]) - float(o[c])))
    ok = worst <= 0.01
    print(f"check-exact {e['symbol']} {window}: {len(results['window'])} cells, max |Δ| over test columns "
          f"= {worst:.4f} → {'EXACT (≤ $0.01)' if ok else 'MISMATCH — use the prefix method'}")
    return ok


class Progress:
    def __init__(self, path: Path):
        self.path, self.lock = path, threading.Lock()

    def log(self, msg: str):
        line = f"{common.utc(time.time())} {msg}"
        with self.lock:
            with open(self.path, "a") as f:
                f.write(line + "\n")
            print(line, flush=True)


def estimate_minutes(run_root: Path, total_rows_cells: float, j: int) -> float:
    """Wall minutes: total rows×cells ÷ (median per-job rate from earlier progress logs × j jobs in
    parallel). Rates are logged per job while the queue runs, so they already include contention."""
    rates = []
    for log in run_root.glob("per_token_sweep_*/progress.log"):
        for line in log.read_text().splitlines():
            if " rate=" in line:
                try:
                    rates.append(float(line.split(" rate=")[1].split()[0]))
                except ValueError:
                    pass
    return total_rows_cells / (sorted(rates)[len(rates) // 2] * max(1, j) * 60.0) if rates else float("nan")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--book", default=None)
    ap.add_argument("--run-dir", default=None)
    ap.add_argument("--tokens-file", default="assets/momentum_tokens.json")
    ap.add_argument("--tokens", default=None)
    ap.add_argument("--costs", default=None, help="costs.json (default <run-dir>/costs.json)")
    ap.add_argument("--cost-bps", type=int, default=None, help="uniform cost for smoke runs (skips costs.json)")
    ap.add_argument("--cost-mult", type=float, default=3.0)
    ap.add_argument("--trails", default=None)
    ap.add_argument("--lookbacks", default=None)
    ap.add_argument("--zs", default=None)
    ap.add_argument("--fracs", default=None)
    ap.add_argument("--min-mults", default=None)
    ap.add_argument("--train-frac", type=float, default=0.7)
    ap.add_argument("--windows", type=int, default=5)
    ap.add_argument("--days", type=int, default=150)
    ap.add_argument("--allow-insufficient", default=None,
                    help="comma list: sweep these INSUFFICIENT (<60 d) tokens anyway, as SHORT (own span, K=3); "
                         "the override is written into their T0 as a WARN")
    ap.add_argument("--jobs", default=",".join(ALL_JOBS))
    ap.add_argument("-j", type=int, default=3)
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--keep-windows", action="store_true")
    ap.add_argument("--no-report", action="store_true", help="skip per_trail_report after each token")
    ap.add_argument("--check-exact", action="store_true",
                    help="compare the window shortcut with the rows[:b] method on real cells, then exit")
    ap.add_argument("--check-window", default="f3")
    args = ap.parse_args()

    fl = lambda s, cast=float: [cast(x) for x in s.split(",")] if s else None
    root = common.repo_root()
    env = common.read_env(root)
    tokens_file = root / args.tokens_file
    entries = common.load_tokens(tokens_file)
    only = [s.strip() for s in args.tokens.split(",")] if args.tokens else None
    targets = common.deployed_targets(entries, only)
    if not targets:
        sys.exit("no deployed targets")
    book = Path(args.book) if args.book else common.newest_book(root)
    if book is None:
        sys.exit("no history book — run ensure_history.py --build first")
    common.ensure_binary(root, build=not args.dry_run)  # a preview never builds
    run_dir = Path(args.run_dir) if args.run_dir else root / "assets" / f"per_token_sweep_{time.strftime('%Y-%m-%d')}"
    wanted_jobs = [j for j in args.jobs.split(",") if j in ALL_JOBS]

    # Coverage first (T0): it decides who is swept, and on which span.
    cov = ensure_history.coverage(root, book, targets, args.days, 7.0, build=not args.dry_run)
    ensure_history.print_coverage(cov)
    t1 = cov["last"]
    allow = {s.strip() for s in args.allow_insufficient.split(",")} if args.allow_insufficient else set()
    runnable, starts, kwin, skipped = select_runnable(cov, targets, args.windows, allow)
    for s in sorted(allow & {e["symbol"] for e in runnable}):
        print(f"⚠ {s}: {cov['tokens'][s]['days']} d is INSUFFICIENT (< 60 d) — swept anyway on --allow-insufficient "
              f"(own span, K=3)")

    costs = load_costs(run_dir, args.costs, args.cost_bps, required=not args.dry_run)
    overrides = {k: v for k, v in os.environ.items() if k.startswith("MOMENTUM_") and k != "MOMENTUM_SLIPPAGE_BPS"}
    if overrides:
        print(f"⚠ exported MOMENTUM_* overrides apply to every sim call (recorded in the manifest): {overrides}")
    axes = {e["symbol"]: common.axes_for(e["params"], env, fl(args.trails), fl(args.lookbacks, int),
                                         fl(args.zs), fl(args.fracs), fl(args.min_mults)) for e in runnable}
    wins = {e["symbol"]: windows_for(starts[e["symbol"]], t1, args.train_frac, kwin[e["symbol"]]) for e in runnable}

    print(f"\n{len(runnable)} token(s) to sweep, skipped: {skipped or 'none'}")
    total = 0.0
    for e in runnable:
        s = e["symbol"]
        a = axes[s]
        # a per-token file keeps every book row from the token's start (the book's grid), so its
        # size is the book's row count over the token's span — not the token's own prints
        st = cov["tokens"][s]
        span_frac = min(1.0, st.get("days", 0) / max(cov["span_days"], 1e-9))
        rows = int(cov["rows"] * span_frac)
        total += a["n_cells"] * rows * (2 + 1 + 1)  # full + cost3x + (f0 + back windows ≈ 1 full)
        c = cost_for(costs, s, env, required=not args.dry_run)
        print(f"  {s:<9} cost {c if c is not None else '— (not measured yet: step 2)'} bps · {a['n_cells']} cells · trails {a['trails']} · "
              f"K={kwin[s]}{'  ⚠ ' + '; '.join(a['warnings']) if a['warnings'] else ''}")
    eta = estimate_minutes(root / "assets", total, args.j)
    print(f"jobs: {len(runnable) * len(wanted_jobs)} · ETA "
          f"{'unknown (no previous runs — the first run measures it)' if eta != eta else f'~{eta:.0f} min at -j {args.j}'}")
    if args.dry_run:
        return

    common.install_cascade(f"optimize-momentum-tokens:run_sweeps:{run_dir.name}")  # SIGTERM stops the sims too
    if args.check_exact:
        if not re.fullmatch(r"f[1-9][0-9]*", args.check_window):
            sys.exit("--check-window must be a back window (f1..fK): f0 starts at the file start, full is no window")
        guard = common.Tripwire(book)
        work = run_dir / "check_exact"
        hist_dir = work / "hist"
        counts = extract_per_token(book, runnable, hist_dir, starts)
        ok = True
        for e in runnable:
            s = e["symbol"]
            k = common.deployed_knobs(e["params"], env)
            small = dict(axes[s], mins=[common.round4(k["min"])], lookbacks=[k["lb"]], zs=[k["z"]],
                         fracs=[float(common.fmt_frac(k["fb"]))])  # all trails, deployed elsewhere
            ok &= check_exact(root, e, hist_dir / f"{s}.jsonl", wins[s], args.check_window, small,
                              cost_for(costs, s, env), work / s, guard)
        sys.exit(0 if ok else 1)

    manifest = init_run_dir(root, run_dir, tokens_file, book, env, {
        "train_frac": args.train_frac, "windows": {s: {k: v for k, v in w.items()} for s, w in wins.items()},
        "k": kwin, "axes": {s: {k: v for k, v in a.items() if k != "warnings"} for s, a in axes.items()},
        "costs": {e["symbol"]: cost_for(costs, e["symbol"], env) for e in runnable},
        "cost_mult": args.cost_mult, "skipped": skipped, "live_max_positions":
        int(env.get("MOMENTUM_MAX_POSITIONS", 1)), "env_overrides": overrides,
    })
    run_lock = open(run_dir / ".run.lock", "w")  # one run_sweeps per run dir, or window cleanup races live jobs
    try:
        fcntl.flock(run_lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        sys.exit(f"another run_sweeps.py is using {run_dir}")
    for s, a in axes.items():  # a resumed run must keep its grid, or the job CSVs stop joining
        old = manifest.get("axes", {}).get(s)
        new = {k: v for k, v in a.items() if k != "warnings"}
        if old is not None and old != new:
            sys.exit(f"{s}: axes differ from this run dir's manifest — use a new --run-dir")
    old_cov = common.read_json(run_dir / "coverage.json", {})  # merge per token: never drop another token's T0
    cov = dict(cov, tokens=dict(old_cov.get("tokens", {}), **cov["tokens"]))
    common.write_json(run_dir / "coverage.json", cov)
    guard = common.Tripwire(book)
    progress = Progress(run_dir / "progress.log")
    tok_dir, hist_dir, job_dir = run_dir / "tokens", run_dir / "hist", run_dir / "jobs"
    for d in (tok_dir, hist_dir, job_dir):
        d.mkdir(exist_ok=True)
    missing = [e for e in runnable if not (hist_dir / f"{e['symbol']}.jsonl").exists()]
    if missing:  # the manifest pins the book, so an existing extraction is still valid
        counts = extract_per_token(book, missing, hist_dir, starts)
        progress.log(f"extracted per-token history: {counts}")

    plan = []  # (sym, job, args, cost, hist)
    for e in runnable:
        s = e["symbol"]
        tj = tok_dir / f"{s}.json"
        common.write_json(tj, [e])
        files, prints = write_window_files(hist_dir / f"{s}.jsonl", s, wins[s], kwin[s], hist_dir, e["mint"])
        wp = common.read_json(run_dir / "window_prints.json", {})
        wp[s] = prints
        common.write_json(run_dir / "window_prints.json", wp)
        for job in wanted_jobs:
            if job not in files:
                progress.log(f"{s} {job}: window has < 200 rows — skipped")
                continue
            hist, frac = files[job]
            cost = manifest["costs"][s] * (args.cost_mult if job == "cost3x" else 1)
            cost = int(-(-cost // 1))  # ceil: the sim's knob is an integer
            csvp = job_dir / f"{s}.{job}.csv"
            plan.append((s, job, sweep_args(s, hist, tj, frac, axes[s], csvp), cost, hist))

    rayon = max(1, (os.cpu_count() or 4) // max(1, args.j))
    remaining = {e["symbol"]: sum(1 for p in plan if p[0] == e["symbol"]) for e in runnable}
    lock = threading.Lock()
    failures = []

    def run_one(item):
        if common.STOPPING.is_set():  # a stop signal arrived: queued jobs become no-ops
            return
        s, job, sargs, cost, hist = item
        csvp, txt = job_dir / f"{s}.{job}.csv", job_dir / f"{s}.{job}.txt"
        specp = job_dir / f"{s}.{job}.spec"
        spec = job_spec(sargs, cost, hist)
        if csvp.exists() and specp.exists() and specp.read_text().strip() == spec:
            progress.log(f"{s} {job}: up to date — skipped")
        else:
            csvp.unlink(missing_ok=True)  # a failed re-run must leave NO stale CSV behind
            specp.unlink(missing_ok=True)
            t = time.time()
            rc = common.run_sim(root, sargs, txt, {"MOMENTUM_SLIPPAGE_BPS": cost}, slots=args.j,
                                rayon_threads=rayon, guard=guard)
            dt = time.time() - t
            if rc != 0 or not csvp.exists():
                progress.log(f"{s} {job}: FAILED rc={rc} — see {txt}")
                with lock:
                    failures.append(f"{s}.{job}")
            else:
                specp.write_text(spec + "\n")
                rows = sum(1 for _ in open(hist))
                cells = axes[s]["n_cells"]
                progress.log(f"{s} {job}: {dt:.0f}s rows={rows} cells={cells} "
                             f"rate={rows * cells / max(dt, 1e-9):.0f} rows·cells/s")
        with lock:
            remaining[s] -= 1
            done = remaining[s] == 0
        if done and not args.no_report and not common.STOPPING.is_set() and (job_dir / f"{s}.full.csv").exists():
            import per_trail_report  # noqa: E402  (lazy: only needed once a token completes)
            per_trail_report.build_fragment(run_dir, s)  # missing windows are flagged in its TRUST block
            (run_dir / f"{s}.done").write_text(common.utc(time.time()) + "\n")
            progress.log(f"{s}: all jobs done → {run_dir / (s + '_per_trail.md')}")

    with cf.ThreadPoolExecutor(max_workers=max(1, args.j)) as pool:
        list(pool.map(run_one, plan))
    guard.check()
    if not args.keep_windows:
        for p in hist_dir.glob("*.f*.jsonl"):
            p.unlink()
    if failures:
        progress.log(f"DONE WITH FAILURES ({len(failures)}): {', '.join(failures)} → {run_dir}")
        sys.exit(1)
    progress.log(f"ALL-DONE → {run_dir}")


if __name__ == "__main__":
    main()
