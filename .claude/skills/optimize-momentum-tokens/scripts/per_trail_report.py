#!/usr/bin/env python3
"""
per_trail_report.py — one token's jobs → TRUST block + per-trail decision-axis tables.

Joins the token's job CSVs (full split, f0, back windows f1..fK, cost3x) by cell label, then
for EACH trail rung lists the interesting combinations next to the deployed params:
DEPLOYED@T (the deployed knobs at that trail) plus the top family of every decision axis —
the split objectives of optimize-momentum-config (max test, worst-slice P&L, worst-slice $/h,
least drawdown, SQN, Pareto, consensus), the time-split axes (maximin, window-robust Σ,
evenest), the operator's drawdown-first tail axis and cost-robustness at 3× cost.

Trust is part of the output, not the reader's memory: the TRUST block (T0–T5) and the per-row
flags encode optimize-momentum-config's "Rules that decide whether the table is trustworthy".
A FAIL on T0–T2 suppresses the token's tables.

Usage:
  python3 per_trail_report.py --run-dir assets/per_token_sweep_<date> [--token HYPE]
"""
import argparse
import math
import re
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

JOBS = ["full", "f0", "f1", "f2", "f3", "f4", "f5", "cost3x"]
KNOBS = ("min", "lb", "z", "regime", "fb")  # trail is the stratum
MIN_TRADES = 3
FEW_TRADES = 8
SPLIT_AXES = ("max test P&L", "best worst-slice P&L", "best worst-slice $/h", "least drawdown", "best SQN")
TIME_AXES = ("maximin (time split)", "window-robust best Σ", "evenest (time split)")
OTHER_AXES = ("smallest worst trade", "cost-robust (3×)")
ALL_AXES = SPLIT_AXES + TIME_AXES + OTHER_AXES


# ── loading & joining ────────────────────────────────────────────────────────────────────

def load_jobs(run_dir: Path, sym: str) -> dict:
    out = {}
    for job in JOBS:
        p = run_dir / "jobs" / f"{sym}.{job}.csv"
        if p.exists():
            out[job] = {r["cell"]: r for r in common.read_sweep_csv(p)}
    if "full" not in out:
        sys.exit(f"{sym}: no full-split job CSV in {run_dir / 'jobs'}")
    if "INCUMBENT" not in out["full"]:
        sys.exit(f"{sym}: CSV has no INCUMBENT row — rebuild momentum-sim and re-run the jobs")
    return out


def back_windows(jobs: dict, run_dir: Path = None, sym: str = None) -> list:
    """The back windows this token was PLANNED with (manifest), not whichever CSVs exist: a failed
    or missing window must show up as missing — silently shrinking K turns the 4/5 rule into 3/4."""
    if run_dir is not None and sym is not None:
        planned = common.read_json(Path(run_dir) / "manifest.json", {}).get("windows", {}).get(sym)
        if planned:
            return sorted((w for w in planned if re.fullmatch(r"f[1-9][0-9]*", w)), key=lambda w: int(w[1:]))
    return sorted((w for w in jobs if re.fullmatch(r"f[1-9][0-9]*", w)), key=lambda w: int(w[1:]))


def rate(pnl: float, hours: float) -> float:
    return pnl / hours if hours > 0 else 0.0


def sqn(pnl: float, n: int, std: float) -> float:
    return pnl / (math.sqrt(n) * std) if n >= 2 and std > 0 else 0.0


def make_record(label: str, jobs: dict, back: list) -> dict:
    r = jobs["full"][label]
    rec = {
        "label": label,
        "knobs": common.parse_label(label) if label != "INCUMBENT" else None,
        "train": r["pnl_train"], "test": r["pnl_test"],
        "tr_trades": r["trades_train"], "te_trades": r["trades_test"], "win_test": r["win_test"],
        "hold_tr": r["hold_h_train"], "hold_te": r["hold_h_test"],
        "std_tr": r["std_train"], "std_te": r["std_test"],
        "best_tr": r["best_train"], "best_te": r["best_test"],
        "open_tr": r["open_train"], "open_te": r["open_test"],
        "win": {}, "cost3x": None,
    }
    worsts = [r["worst_train"], r["worst_test"]]
    dds = [r["true_dd_train"], r["true_dd_test"]]
    for w in ["f0"] + back:
        wr = jobs.get(w, {}).get(label)
        if wr is not None:
            rec["win"][w] = wr["pnl_test"]
            worsts.append(wr["worst_test"])
            dds.append(wr["true_dd_test"])
    if "cost3x" in jobs and label in jobs["cost3x"]:
        rec["cost3x"] = jobs["cost3x"][label]["pnl_test"]
    b = [rec["win"][w] for w in back if w in rec["win"]]
    rec["back"] = b
    rec["complete"] = len(b) == len(back) and len(back) > 0
    rec["min_win"] = min(b) if b else None
    rec["sum_win"] = sum(b) if b else 0.0
    rec["pos"] = sum(1 for x in b if x > 0)
    rec["lump"] = (max(b) / rec["sum_win"]) if b and rec["sum_win"] > 0 else math.inf
    rec["worst_all"] = min(worsts)
    rec["dd_all"] = max(dds)
    rec["worst_slice"] = min(rec["train"], rec["test"])
    rec["sqn_ws"] = min(sqn(rec["train"], rec["tr_trades"], rec["std_tr"]),
                        sqn(rec["test"], rec["te_trades"], rec["std_te"]))
    rec["rate_ws"] = min(rate(rec["train"], rec["hold_tr"]), rate(rec["test"], rec["hold_te"]))
    rec["robust"] = (rec["train"] > 0 and rec["test"] > 0
                     and rec["tr_trades"] >= MIN_TRADES and rec["te_trades"] >= MIN_TRADES)
    return rec


def window_ok(rec: dict, k: int) -> bool:
    """Rule 2b: ≥ K−1 of K back windows positive (4/5 at K=5) AND best window < 50% of Σ."""
    return rec["complete"] and rec["sum_win"] > 0 and rec["pos"] >= k - 1 and rec["lump"] < 0.5


def outcome_key(rec: dict) -> tuple:
    """Identical outcomes everywhere ⇒ one family (the sweep's own rounding, extended to windows)."""
    return (round(rec["train"] * 100), round(rec["test"] * 100), rec["tr_trades"], rec["te_trades"],
            round(rec["hold_te"] * 10),
            tuple(round(rec["win"].get(w, 0.0) * 100) for w in sorted(rec["win"])),
            None if rec["cost3x"] is None else round(rec["cost3x"] * 100))


# ── families & axes ──────────────────────────────────────────────────────────────────────

def families(cells: list) -> list:
    groups = {}
    for c in cells:
        groups.setdefault(outcome_key(c), []).append(c)
    fams = []
    for members in groups.values():
        f = dict(members[0])
        f["members"] = members
        f["values"] = {k: sorted({m["knobs"][k] for m in members}) for k in KNOBS}
        fams.append(f)
    return fams


def rank_axis(fams: list, axis: str, dep: dict, k: int) -> list:
    """Families ordered best-first for one axis (eligible ones only)."""
    robust = [f for f in fams if f["robust"]]
    if axis == "max test P&L":
        return sorted(robust, key=lambda f: (-f["test"], -f["train"]))
    if axis == "best worst-slice P&L":
        return sorted(robust, key=lambda f: (-f["worst_slice"], -f["test"]))
    if axis == "best worst-slice $/h":
        # An isolated $/h is mirage-prone (the best rate barely trades): require real activity.
        floor = max(FEW_TRADES, dep["tr_trades"] // 2), max(FEW_TRADES, dep["te_trades"] // 2)
        ok = [f for f in robust if f["tr_trades"] >= floor[0] and f["te_trades"] >= floor[1]]
        return sorted(ok, key=lambda f: (-f["rate_ws"], -f["test"]))
    if axis == "least drawdown":
        return sorted(robust, key=lambda f: (f["dd_all"], -f["test"]))
    if axis == "best SQN":
        return sorted(robust, key=lambda f: (-f["sqn_ws"], -f["test"]))
    if axis == "maximin (time split)":
        # robust too: a config that never trades has a "worst window" of 0 and would beat any
        # deployed config with one losing window
        return sorted([f for f in fams if f["complete"] and f["robust"]], key=lambda f: (-f["min_win"], -f["sum_win"]))
    if axis == "window-robust best Σ":
        return sorted([f for f in fams if window_ok(f, k)], key=lambda f: (-f["sum_win"], -f["min_win"]))
    if axis == "evenest (time split)":
        return sorted([f for f in fams if window_ok(f, k)], key=lambda f: (f["lump"], -f["sum_win"]))
    if axis == "smallest worst trade":
        return sorted([f for f in fams if f["sum_win"] > 0 and f["robust"]],
                      key=lambda f: (-f["worst_all"], -f["sum_win"]))
    if axis == "cost-robust (3×)":
        return sorted([f for f in robust if f["cost3x"] is not None], key=lambda f: (-f["cost3x"], -f["test"]))
    raise ValueError(axis)


def pareto(fams: list) -> list:
    """Robust families not beaten on both worst-slice P&L (↑) and test trade-σ (↓), smoothest first."""
    robust = [f for f in fams if f["robust"]]
    front = [f for f in robust
             if not any(g is not f and g["worst_slice"] >= f["worst_slice"] and g["std_te"] <= f["std_te"]
                        and (g["worst_slice"] > f["worst_slice"] or g["std_te"] < f["std_te"]) for g in robust)]
    return sorted(front, key=lambda f: f["std_te"])[:5]


def resolve(fam: dict, dep_knobs: dict) -> dict:
    """Rule 3 (inert knobs keep the deployed value), done on MEMBERS: the family member that matches
    the deployed knobs on the most knobs. Resolving each inert knob on its own could combine values
    no member has — a cell that was never replayed."""
    scored = [(sum(m["knobs"][k] == dep_knobs.get(k) for k in KNOBS), -i, m) for i, m in enumerate(fam["members"])]
    best = max(scored, key=lambda x: (x[0], x[1]))[2]
    return {k: best["knobs"][k] for k in KNOBS}


def dep_knob_values(params: dict, env: dict) -> dict:
    k = common.deployed_knobs(params, env)
    return {"min": common.round4(k["min"]), "lb": k["lb"], "z": k["z"],
            "regime": "exempt" if k["regime_exempt"] else "gated", "fb": float(common.fmt_frac(k["fb"]))}


# ── flags ────────────────────────────────────────────────────────────────────────────────

def row_flags(f: dict, dep: dict, axes_top1: list, axes_top3: dict, axis_bounds: dict, k: int,
              dep_mark: str = None) -> list:
    """dep_mark: "★" = the deployed config itself (deployed trail); "dep@T" = the deployed knobs at
    another trail (a pure trail change — a real alternative, so it is flagged like any other row)."""
    fl = []
    is_dep_row = dep_mark == "★"
    if dep_mark:
        fl.append(dep_mark)
    if window_ok(f, k):
        fl.append("✓win")
    if f["min_win"] is not None and dep["min_win"] is not None and f["min_win"] > dep["min_win"] + 1e-9:
        fl.append("▲")
    if f["train"] > dep["train"] and f["test"] > dep["test"]:
        fl.append("both↑")
    others = [a for a, n in axes_top3.items() if n and a not in axes_top1]
    if len(axes_top1) == 1 and not others:
        fl.append("specialist")
    if f["test"] > dep["test"] and f["train"] < dep["train"]:
        fl.append("thin-train")
    if f["tr_trades"] < MIN_TRADES:
        fl.append("test-carried")
    if min(f["tr_trades"], f["te_trades"]) < FEW_TRADES or (f["win_test"] >= 100 and f["te_trades"] <= 5):
        fl.append("few-trades")
    if f["worst_all"] < dep["worst_all"] - 0.005:
        fl.append("worse-tail")
    if f["cost3x"] is not None and f["cost3x"] <= 0:
        fl.append("cost-fragile")
    for sl, pnl, best in (("train", f["train"], f["best_tr"]), ("test", f["test"], f["best_te"])):
        if pnl > 0 and best >= 0.5 * pnl:
            fl.append(f"1-trade({sl}: resid {pnl - best:+.2f})")
    if not is_dep_row:
        for sl, opn, delta in (("test", f["open_te"], f["test"] - dep["test"]),
                               ("train", f["open_tr"], f["train"] - dep["train"])):
            if abs(opn) > max(1.0, 0.25 * abs(delta)):
                fl.append(f"straddle({sl} open {opn:+.2f})")
    if axes_top1 and not is_dep_row:  # a winner sitting on the grid boundary may be clipped
        for knob in ("min", "lb"):
            vals = f["values"][knob]
            if len(vals) == 1 and vals[0] in axis_bounds[knob]:
                fl.append(f"edge:{knob}")
    inert = [kk for kk in KNOBS if len(f["values"][kk]) > 1]
    if inert:
        fl.append("inert:" + ",".join(inert))
    if not f["robust"]:
        fl.append("non-robust")
    return fl


# ── formatting ───────────────────────────────────────────────────────────────────────────

def money(v) -> str:
    return "—" if v is None else f"{v:+.2f}"


def fam_label(f: dict, dep_min: float) -> str:
    def show(k, vals):
        if k == "min":
            s = [f"{common.fmt_f64(v)}(×{v / dep_min:.2f})" if dep_min else common.fmt_f64(v) for v in vals]
        elif k == "z":
            s = ["off" if v == 0 else common.fmt_f64(v) for v in vals]
        elif k == "fb":
            s = [common.fmt_frac(v) for v in vals]
        else:
            s = [str(v) for v in vals]
        return s[0] if len(s) == 1 else "{" + ",".join(s) + "}"
    return " · ".join(f"{k} {show(k, f['values'][k])}" for k in KNOBS)


def table_row(f: dict, axes_won: list, dep: dict, back: list, flags: list, dep_min: float) -> str:
    wins = " | ".join(money(f["win"].get(w)) for w in ["f0"] + back)
    lump = "—" if f["lump"] == math.inf else (">999%" if f["lump"] > 9.99 else f"{100 * f['lump']:.0f}%")
    return (f"| {fam_label(f, dep_min)} | {' · '.join(axes_won) or '—'} | {money(f['train'])} | {money(f['test'])} | "
            f"{money(f['test'] - dep['test'])} | {wins} | {money(f['min_win'])} | {money(f['sum_win'])} | {lump} | "
            f"{f['worst_all']:+.2f} | {f['dd_all']:.2f} | {f['sqn_ws']:.2f} | {f['rate_ws']:+.3f} | "
            f"{money(f['cost3x'])} | {f['tr_trades']}/{f['te_trades']} | {' '.join(flags)} |")


# ── trust ────────────────────────────────────────────────────────────────────────────────

def trust_block(run_dir: Path, sym: str, entry: dict, env: dict, inc: dict, dep_at_trail: dict) -> tuple:
    """(lines, suppress) — suppress=True on a FAIL of T0, T1 or T2."""
    cov = common.read_json(run_dir / "coverage.json", {})
    man = common.read_json(run_dir / "manifest.json", {})
    lines, suppress = [], False
    st = cov.get("tokens", {}).get(sym)
    if st is None:  # never PASS a token nobody checked
        st = {"t0": [("FAIL", "no coverage entry for this token in coverage.json")]}
    t0_fail = [m for lvl, m in st.get("t0", []) if lvl == "FAIL"]
    t0_warn = [m for lvl, m in st.get("t0", []) if lvl == "WARN"]
    prints = common.read_json(run_dir / "window_prints.json", {}).get(sym, {})
    for w in sorted(prints, key=lambda w: (len(w), w)):
        if w.startswith("f") and w != "f0" and prints[w] == 0:
            t0_fail.append(f"window {w} has no {sym} prints — its P&L would be a meaningless 0")
        elif w.startswith("f") and prints[w] < 100:
            t0_warn.append(f"window {w} has only {prints[w]} {sym} prints")
    missing = [w for w in (["full", "f0"] + back_windows({}, run_dir, sym) + ["cost3x"])
               if not (run_dir / "jobs" / f"{sym}.{w}.csv").exists()]
    if missing:
        t0_warn.append(f"job(s) missing: {', '.join(missing)} — those columns are blank and the time-split "
                       f"axes exclude every row (re-run run_sweeps.py)")
    t0 = "FAIL" if t0_fail else ("WARN" if t0_warn else "PASS")
    suppress |= t0 == "FAIL"
    lines.append(f"T0 data {t0} ({st.get('days', '?')} d, {st.get('prints_per_day', '?')} prints/d, "
                 f"{st.get('glitches', '?')} glitches, sanitizer −{st.get('sanitizer_removed_pct', '?')}%)"
                 + ("".join(f"\n       · {m}" for m in t0_fail + t0_warn)))
    # T1 — the deployed grid cell must reproduce the exact INCUMBENT row.
    if dep_at_trail is None:
        lines.append("T1 incumbent FAIL (the deployed knobs are not a grid cell — check the axes)")
        suppress = True
    else:
        same = (abs(dep_at_trail["train"] - inc["train"]) <= 0.01 and abs(dep_at_trail["test"] - inc["test"]) <= 0.01
                and dep_at_trail["tr_trades"] == inc["tr_trades"] and dep_at_trail["te_trades"] == inc["te_trades"])
        if same:
            lines.append("T1 incumbent PASS (DEPLOYED@deployed-trail ≡ INCUMBENT)")
        elif not fade_bar_matches_grid(entry["params"], env):
            lines.append(f"T1 incumbent WARN (grid cell differs from INCUMBENT by the rounded fade bar: "
                         f"test {dep_at_trail['test']:+.2f} vs {inc['test']:+.2f}; INCUMBENT stays the baseline)")
        else:
            lines.append(f"T1 incumbent FAIL (grid cell test {dep_at_trail['test']:+.2f} vs INCUMBENT "
                         f"{inc['test']:+.2f} — the sweep is not replaying the deployed config)")
            suppress = True
    # T2 — inputs unchanged since the jobs ran.
    try:
        import run_sweeps  # noqa: E402
        now = run_sweeps.current_state(common.repo_root(), common.repo_root() / "assets" / "momentum_tokens.json",
                                       Path(man["book"]), env)
        drift = [k for k in run_sweeps.MANIFEST_KEYS if man.get(k) != now[k]]
    except Exception as ex:  # a missing book etc. is itself a freshness failure
        drift = [f"unverifiable ({ex})"]
    if drift and drift != ["tokens_sha"]:
        lines.append(f"T2 fresh FAIL ({', '.join(drift)} changed since the jobs ran — re-run)")
        suppress = True
    elif drift:
        lines.append("T2 fresh WARN (momentum_tokens.json changed since the run — deployed params may differ)")
    else:
        lines.append("T2 fresh PASS")
    # T3 — slices carry the deployed config.
    if inc["tr_trades"] == 0:
        lines.append("T3 slices FAIL-soft (deployed made no train-slice trade — no params from this run)")
    elif min(inc["tr_trades"], inc["te_trades"]) < MIN_TRADES:
        lines.append(f"T3 slices WARN (deployed {inc['tr_trades']}/{inc['te_trades']} trades < {MIN_TRADES} in a slice)")
    else:
        lines.append(f"T3 slices PASS ({inc['tr_trades']}/{inc['te_trades']} trades)")
    # T4 — unit scale vs the previous run.
    prev = previous_manifest(run_dir)
    keys = ("MOMENTUM_RANK_METRIC", "MOMENTUM_LOOKBACK_OBS")
    changed = [k for k in keys if prev and prev.get("momentum_env", {}).get(k) != man.get("momentum_env", {}).get(k)]
    unreach = inc_last_window_trades(run_dir, sym, inc) == 0
    t4 = "WARN" if changed or unreach else "PASS"
    lines.append(f"T4 units {t4}" + (f" ({', '.join(changed)} changed since the previous run: deployed bars are in "
                                     f"the old units — re-derive every token)" if changed else "")
                 + (" (unreachable? DEPLOYED made 0 trades in the last window)" if unreach else ""))
    lines.append("T5 tripwire PASS (checked after each of this token's jobs; any change aborts the run)")
    return lines, suppress


def fade_bar_matches_grid(params: dict, env: dict) -> bool:
    """Does the deployed fade_bar equal the grid's own round4(frac × min)? If not (e.g. JitoSOL's
    hand-set 2.86875 vs the grid's 2.8688), a small T1 difference is rounding, not a bug."""
    fb = params.get("fade_bar")
    if fb is None:
        return True
    k = common.deployed_knobs(params, env)
    grid_bar = common.round4(float(common.fmt_frac(k["fb"])) * common.round4(k["min"]))
    return abs(float(fb) - grid_bar) < 1e-9


def inc_last_window_trades(run_dir: Path, sym: str, inc: dict) -> int:
    last = max((w for w in inc["win"] if w != "f0"), default=None)
    if last is None:
        return -1
    p = run_dir / "jobs" / f"{sym}.{last}.csv"
    for r in common.read_sweep_csv(p):
        if r["cell"] == "INCUMBENT":
            return r["trades_test"]
    return -1


def previous_manifest(run_dir: Path):
    runs = sorted(p for p in run_dir.parent.glob("per_token_sweep_*") if (p / "manifest.json").exists())
    earlier = [p for p in runs if p.name < run_dir.name]
    return common.read_json(earlier[-1] / "manifest.json") if earlier else None


# ── the fragment ─────────────────────────────────────────────────────────────────────────

def build_fragment(run_dir: Path, sym: str) -> Path:
    run_dir = Path(run_dir)
    env = common.read_env(common.repo_root())
    man = common.read_json(run_dir / "manifest.json", {})
    entry = next(e for e in common.load_tokens(run_dir / "deployed_tokens.json") if e["symbol"] == sym)
    params = entry["params"]
    jobs = load_jobs(run_dir, sym)
    back = back_windows(jobs, run_dir, sym)
    k = len(back)
    inc = make_record("INCUMBENT", jobs, back)
    cells = [make_record(lbl, jobs, back) for lbl in jobs["full"] if lbl != "INCUMBENT"]
    dep_vals = dep_knob_values(params, env)
    dep_trail = float(common.deployed_knobs(params, env)["trail"])
    by_label = {c["label"]: c for c in cells}
    dep_label_at = lambda t: common.cell_label(dep_vals["min"], t, dep_vals["lb"], dep_vals["z"],
                                               dep_vals["regime"] == "exempt", dep_vals["fb"])
    axes_meta = man.get("axes", {}).get(sym, {})
    # min: both ends can clip the optimum. lb: only the top — below the 240 floor a lookback soon
    # hits the 120-obs minimum a token needs to rank at all, so the bottom is not a real edge.
    axis_bounds = {"min": {min(axes_meta.get("mins", [0])), max(axes_meta.get("mins", [0]))},
                   "lb": {max(axes_meta.get("lookbacks", [0]))}}
    trust, suppress = trust_block(run_dir, sym, entry, env, inc, by_label.get(dep_label_at(dep_trail)))
    cost = man.get("costs", {}).get(sym, "?")
    fixed = {kk: v for kk, v in params.items() if kk not in common.SWEPT_KEYS}

    out = [f"## {sym} — deployed min {common.fmt_f64(dep_vals['min'])} · trail {common.fmt_f64(dep_trail)} · "
           f"lb {dep_vals['lb']} · z {'off' if dep_vals['z'] == 0 else common.fmt_f64(dep_vals['z'])} · "
           f"{dep_vals['regime']} · fb {common.fmt_frac(dep_vals['fb'])}",
           f"cost {cost} bps/leg (@3× = {int(-(-int(cost) * man.get('cost_mult', 3) // 1)) if cost != '?' else '?'}) · "
           f"fixed (not swept): {' · '.join(f'{kk} {v}' for kk, v in fixed.items()) or '—'}",
           "", "```", "TRUST  " + "\n       ".join(trust), "```", ""]
    out.append(f"Deployed: train {money(inc['train'])} / test {money(inc['test'])} · windows "
               + " ".join(f"{w} {money(inc['win'].get(w))}" for w in ["f0"] + back)
               + f" · min {money(inc['min_win'])} · Σ {money(inc['sum_win'])} · worst {inc['worst_all']:+.2f} · "
               f"trueDD {inc['dd_all']:.2f} · trades {inc['tr_trades']}/{inc['te_trades']}")
    out.append("")
    out.append("<!-- VERDICT:" + sym + " -->")
    out.append("")
    if suppress:
        out.append("**Tables suppressed:** a T0–T2 gate failed (see TRUST). Fix the cause and re-run; numbers "
                   "from an untrustworthy table are not shown.")
        path = run_dir / f"{sym}_per_trail.md"
        path.write_text("\n".join(out) + "\n")
        common.write_json(run_dir / f"{sym}_candidates.json", {"token": sym, "suppressed": True, "trust": trust})
        return path

    trails = sorted({c["knobs"]["trail"] for c in cells})
    per_trail, outcome_maps = {}, {}
    for t in trails:
        tc = [c for c in cells if c["knobs"]["trail"] == t]
        outcome_maps[t] = {tuple(c["knobs"][kk] for kk in KNOBS): outcome_key(c) for c in tc}
        per_trail[t] = tc
    # Rungs with identical outcomes for every combination form a class (the trail never binds
    # between them). The DEPLOYED trail represents its class, so an inert trail keeps its deployed
    # value in every row and paste-ready JSON (rule 3); otherwise the lowest rung does.
    classes = []
    for t in trails:
        for cls in classes:
            if outcome_maps[t] == outcome_maps[cls[0]]:
                cls.append(t)
                break
        else:
            classes.append([t])
    rep, peers = {}, {}
    for cls in classes:
        r = next((x for x in cls if abs(x - dep_trail) < 1e-9), cls[0])
        for x in cls:
            rep[x] = r
        peers[r] = [x for x in cls if x != r]

    overview = ["### Trail overview", "",
                "| trail | deployed@T train/test | deployed@T min/Σ | ✓win pool | maximin family (min-win) | "
                "best-SQN family | maximin Δ Σ vs deployed |", "|---|---|---|---|---|---|---|"]
    sections, cand = [], {"token": sym, "deployed": {"knobs": dict(dep_vals, trail=dep_trail),
                                                     "train": inc["train"], "test": inc["test"],
                                                     "min_win": inc["min_win"], "sum_win": inc["sum_win"],
                                                     "worst_all": inc["worst_all"]}, "trails": {}}
    for t in trails:
        mark = " (deployed)" if abs(t - dep_trail) < 1e-9 else ""
        if rep[t] != t:
            overview.append(f"| {common.fmt_f64(t)}{mark} | ≡ trail {common.fmt_f64(rep[t])} | | | | | |")
            sections.append(f"### Trail {common.fmt_f64(t)} %{mark} — ≡ trail {common.fmt_f64(rep[t])} "
                            f"(identical outcomes for every combination: the trail never binds here)\n")
            continue
        also = (" — also ≡ trail " + ", ".join(common.fmt_f64(x) for x in peers[t])) if peers.get(t) else ""
        fams = families(per_trail[t])
        dep_cell = by_label.get(dep_label_at(t))
        dep_fam = next((f for f in fams if dep_cell is not None and dep_cell in f["members"]), None)
        ranked = {a: rank_axis(fams, a, inc, k) for a in ALL_AXES}
        # an axis with ≤ 3 eligible families has everyone in its "top 3" — it cannot vote
        top3 = {a: ({id(f) for f in r[:3]} if len(r) > 3 else set()) for a, r in ranked.items()}
        chosen = []
        if dep_fam is not None:
            chosen.append(dep_fam)
        for a in ALL_AXES:
            if ranked[a]:
                chosen.append(ranked[a][0])
        consensus = [f for f in fams if sum(id(f) in s for s in top3.values()) >= 2]
        front = pareto(fams)
        seen, rows = set(), []
        for f in chosen + consensus + front:
            if id(f) in seen:
                continue
            seen.add(id(f))
            won = [a for a in ALL_AXES if ranked[a] and ranked[a][0] is f]
            n3 = sum(id(f) in s for s in top3.values())
            tags = list(won)
            if n3 >= 2:
                tags.append(f"consensus[{n3}]")
            if f in front:
                tags.append("pareto")
            dep_mark = ("★" if abs(t - dep_trail) < 1e-9 else "dep@T") if f is dep_fam else None
            flags = row_flags(f, inc, won, {a: id(f) in top3[a] for a in ALL_AXES}, axis_bounds, k, dep_mark)
            rows.append((f, tags, flags))
        rows.sort(key=lambda x: (x[0] is not dep_fam, -len([t_ for t_ in x[1] if not t_.startswith("pareto")]),
                                 -x[0]["worst_slice"]))
        pool = sum(1 for f in fams if window_ok(f, k))
        mm = ranked["maximin (time split)"][0] if ranked["maximin (time split)"] else None
        bs = ranked["best SQN"][0] if ranked["best SQN"] else None
        overview.append(
            f"| {common.fmt_f64(t)}{mark} | "
            f"{money(dep_fam['train']) + ' / ' + money(dep_fam['test']) if dep_fam else '—'} | "
            f"{money(dep_fam['min_win']) + ' / ' + money(dep_fam['sum_win']) if dep_fam else '—'} | {pool} | "
            f"{fam_label(mm, dep_vals['min']) + ' (' + money(mm['min_win']) + ')' if mm else '—'} | "
            f"{fam_label(bs, dep_vals['min']) if bs else '—'} | "
            f"{money(mm['sum_win'] - inc['sum_win']) if mm else '—'} |")
        hdr = ("| combination | axes won | train | test | Δtest | " + " | ".join(["f0"] + back)
               + " | min | Σ | lump | worst | trueDD | SQN | $/h | @3× | trades | flags |")
        sep = "|" + "---|" * (hdr.count("|") - 1)
        sec = [f"### Trail {common.fmt_f64(t)} %{mark}{also}", "", hdr, sep]
        sec += [table_row(f, tags, inc, back, flags, dep_vals["min"]) for f, tags, flags in rows]
        sec.append("")
        sec.append(knob_frequency(per_trail[t], dep_vals["min"]))
        sec.append("")
        sections.append("\n".join(sec))
        cand["trails"][common.fmt_f64(t)] = [{
            "combination": fam_label(f, dep_vals["min"]), "axes": tags, "flags": flags,
            "knobs": dict(resolve(f, dep_vals), trail=t),
            "params": common.params_for_knobs(params, _to_param_knobs(resolve(f, dep_vals), t)),
            "train": f["train"], "test": f["test"], "min_win": f["min_win"], "sum_win": f["sum_win"],
            "worst_all": f["worst_all"], "cost3x": f["cost3x"], "window_ok": window_ok(f, k),
        } for f, tags, flags in rows]
    out += overview + [""] + sections
    path = run_dir / f"{sym}_per_trail.md"
    path.write_text("\n".join(out) + "\n")
    cand["trust"] = trust
    cand["pickable"] = not any("FAIL-soft" in line for line in trust)
    common.write_json(run_dir / f"{sym}_candidates.json", cand)
    return path


def delta_sum_for(run_dir: Path, sym: str, pick: dict) -> float:
    """Σ over the back windows of (pick − INCUMBENT) for one token, straight from the job CSVs —
    the per-token side of book_ab.py's additivity check (never trusted from an agent's JSON)."""
    jobs = load_jobs(Path(run_dir), sym)
    back = back_windows(jobs, run_dir, sym)
    missing = [w for w in back if w not in jobs]
    if missing:
        raise KeyError(f"{sym}: window job(s) {missing} missing — re-run run_sweeps.py")
    label = common.cell_label(float(pick["min"]), float(pick["trail"]), int(pick["lb"]), float(pick.get("z") or 0),
                              pick.get("regime") == "exempt", float(pick.get("fb", 1.0)))
    if label not in jobs["full"]:
        raise KeyError(f"{sym}: pick {label!r} is not a grid cell")
    return sum(jobs[w][label]["pnl_test"] - jobs[w]["INCUMBENT"]["pnl_test"] for w in back)


def _to_param_knobs(v: dict, trail: float) -> dict:
    return {"min": v["min"], "trail": trail, "lb": v["lb"], "z": v["z"], "z_obs": common.Z_OBS,
            "regime_exempt": v["regime"] == "exempt", "fb": v["fb"]}


def knob_frequency(cells: list, dep_min: float) -> str:
    """Which knob values dominate the top quartile by maximin at this trail (load-bearing knobs)."""
    ranked = sorted([c for c in cells if c["complete"] and c["robust"]], key=lambda c: (-c["min_win"], -c["sum_win"]))
    top = ranked[: max(1, len(ranked) // 4)]
    if not top:
        return "Knob frequency: — (no robust cell with every window)"
    parts = []
    for kk in KNOBS:
        counts = {}
        for c in top:
            counts[c["knobs"][kk]] = counts.get(c["knobs"][kk], 0) + 1
        v, n = max(counts.items(), key=lambda x: x[1])
        vs = f"×{v / dep_min:.2f}" if kk == "min" and dep_min else ("off" if kk == "z" and v == 0 else str(v))
        parts.append(f"{kk} {vs} {100 * n / len(top):.0f}%")
    return f"Knob frequency in the top quartile by maximin ({len(top)} cells): " + " · ".join(parts)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--run-dir", required=True)
    ap.add_argument("--token", default=None, help="one symbol (default: every token with a full job)")
    args = ap.parse_args()
    run_dir = Path(args.run_dir)
    syms = [args.token] if args.token else sorted({p.name.split(".")[0] for p in (run_dir / "jobs").glob("*.full.csv")})
    for s in syms:
        print(f"wrote {build_fragment(run_dir, s)}")


if __name__ == "__main__":
    main()
