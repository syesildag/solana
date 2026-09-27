#!/usr/bin/env python3
"""
build_report.py — THE deliverable: <run>/REPORT.md, one markdown file listing, per token and
per trail %, the interesting combinations on every decision axis against the deployed params.

Assembles: run header (book, windows, costs, .env hash, git) → Summary table (one line per
token + the whole-book A/B) → every token's section (TRUST block, deployed profile, analyst
verdict, trail overview, one table per trail rung) → skipped tokens → the blind-spots footer.

Verdicts come from <run>/verdicts/<SYM>.json (analyst agents or inline analysis):
  {"token", "verdict": keep|change|paper-test|insufficient, "pick": {trail,min,lb,z,regime,fb}|null,
   "per_trail": {"<trail>": "one line"}, "rationale": "2–4 lines", "risks": [..]}

Usage:
  python3 build_report.py --run-dir assets/per_token_sweep_<date>
"""
import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

BLIND_SPOTS = """## Blind spots (fixed footer — read before acting)

- **No volume, no order flow** — price-only history; the sweep cannot see a pool draining.
- **No discovered/adopted mints** — only tokens in the tokens file are recorded or swept.
- **Cooldown is `.env`-global** (`MOMENTUM_REENTRY_COOLDOWN_SECS`) — not a per-token axis.
- **Cost is a one-moment Jupiter quote** at the live notional; thin pools move.
- **GeckoTerminal rows are sparse** (minutes with a trade); the live watcher forward-fills — thin tokens rank differently.
- **Isolated N=1** — each token owns its slot here; interactions live only in the whole-book A/B.
- **Backtests understate drawdown** and every axis is chosen in-sample over these windows: a change that is not `✓win` is a
  hypothesis — paper-test it (`DRY_RUN_MOMENTUM_TRADER=true`) before trusting it live.
"""


def money(v) -> str:
    return "—" if v is None else f"{v:+.2f}"


def freshness_drift(run_dir: Path, man: dict) -> list:
    """T2 at report time: a report is rebuilt from stored fragments, so freshness must be re-checked
    here too — otherwise a .env change after the jobs would still read 'fresh'."""
    import run_sweeps  # noqa: E402
    root = common.repo_root()
    try:
        now = run_sweeps.current_state(root, root / "assets" / "momentum_tokens.json", Path(man["book"]),
                                       common.read_env(root))
    except Exception as ex:
        return [f"unverifiable ({ex})"]
    return [k for k in run_sweeps.MANIFEST_KEYS if k != "tokens_sha" and man.get(k) != now[k]]


def pick_from_csvs(run_dir: Path, sym: str, pick: dict):
    """The analyst picked a cell that is not one of the listed rows: take its numbers straight from
    the job CSVs instead of silently showing a different row."""
    import per_trail_report as ptr  # noqa: E402
    try:
        jobs = ptr.load_jobs(run_dir, sym)
        back = ptr.back_windows(jobs, run_dir, sym)
        label = common.cell_label(float(pick["min"]), float(pick["trail"]), int(pick["lb"]),
                                  float(pick.get("z") or 0), pick.get("regime") == "exempt", float(pick.get("fb", 1.0)))
        rec = ptr.make_record(label, jobs, back)
    except (KeyError, SystemExit):
        return None
    return common.fmt_f64(pick["trail"]), {"axes": ["(pick outside the listed rows)"], "min_win": rec["min_win"],
                                           "sum_win": rec["sum_win"], "flags": []}


def trust_statuses(cand: dict) -> str:
    out = []
    for line in cand.get("trust", []):
        head = line.split("(")[0].split()
        if len(head) >= 3:
            out.append(f"{head[0]} {head[2]}")
    return " ".join(out) or "—"


def knobs_str(k: dict) -> str:
    z = "off" if not k.get("z") else common.fmt_f64(k["z"])
    return (f"{common.fmt_f64(k['min'])}·{common.fmt_f64(k['trail'])}·{k['lb']}·{z}·{k['regime']}·"
            f"{common.fmt_frac(k['fb'])}")


def same_knobs(row: dict, pick: dict) -> bool:
    """A pick names one exact cell: every swept knob must match, not just min/lb."""
    return (abs(float(row["min"]) - float(pick["min"])) < 1e-9 and int(row["lb"]) == int(pick["lb"])
            and abs(float(row["z"]) - float(pick.get("z") or 0)) < 1e-9 and row["regime"] == pick.get("regime")
            and abs(float(row["fb"]) - float(pick.get("fb", 1.0))) < 1e-9)


def best_alternative(cand: dict):
    """Without an analyst pick: the ✓win, robust, not-worse-tail row whose worst window BEATS the
    deployed one (best such). None ⇒ nothing beats deployed on maximin — say so, never promote a
    worse row as an 'alternative'."""
    floor = cand.get("deployed", {}).get("min_win")
    best = None
    for t, rows in cand.get("trails", {}).items():
        for r in rows:
            if "★" in r["flags"] or not r["window_ok"] or "non-robust" in r["flags"] or "worse-tail" in r["flags"]:
                continue
            if r["min_win"] is None or (floor is not None and r["min_win"] <= floor):
                continue
            if best is None or r["min_win"] > best[1]["min_win"]:
                best = (t, r)
    return best


def verdict_text(v: dict, cand: dict) -> str:
    if not v:
        return "**Verdict:** pending — no analyst verdict for this token yet."
    lines = [f"**Verdict: {v['verdict'].upper()}**" + (f" — pick trail {common.fmt_f64(v['pick']['trail'])}: "
                                                       f"`{knobs_str(v['pick'])}`" if v.get("pick") else "")]
    if v.get("rationale"):
        lines.append(v["rationale"])
    if v.get("risks"):
        lines.append("Risks: " + "; ".join(v["risks"]))
    if v.get("per_trail"):
        lines.append("Per trail: " + " · ".join(f"**{t}%** {txt}" for t, txt in v["per_trail"].items()))
    if v.get("pick"):
        entry = cand.get("_entry")
        if entry:
            import apply_params  # noqa: E402
            params = common.params_for_knobs(entry["params"], apply_params.to_knobs(v["pick"]))
            lines.append("Paste-ready `params`:\n```json\n" + json.dumps(params, indent=2) + "\n```")
    return "\n\n".join(lines)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--run-dir", required=True)
    args = ap.parse_args()
    run_dir = Path(args.run_dir)
    man = common.read_json(run_dir / "manifest.json")
    cov = common.read_json(run_dir / "coverage.json", {})
    if not man:
        sys.exit(f"no manifest in {run_dir}")
    entries = {e["symbol"]: e for e in common.load_tokens(run_dir / "deployed_tokens.json")}
    costs_doc = common.read_json(run_dir / "costs.json", {}).get("tokens", {})
    syms = sorted({p.name.split("_per_trail")[0] for p in run_dir.glob("*_per_trail.md")})

    any_win = next(iter(man.get("windows", {}).values()), {})
    win_desc = " | ".join(f"{w} {common.utc(a)[:10]}→{common.utc(b)[:10]}"
                          for w, (a, b) in ((w, any_win[w]) for w in any_win if w.startswith("f")))
    stale = freshness_drift(run_dir, man)
    head = [f"# Per-token sweep — {run_dir.name.replace('per_token_sweep_', '')}", "",
            f"Book `{Path(man['book']).name}` ({cov.get('span_days', '?')} d, {common.utc(cov.get('first', 0))} → "
            f"{common.utc(cov.get('last', 0))}) · **N=1 isolated** (own slot per token) · split "
            f"{man.get('train_frac', 0.7)} · windows {win_desc or '—'} · `.env` {man.get('env_hash')} · git "
            f"{man.get('git')}"
            + (f" · exported overrides {man['env_overrides']}" if man.get("env_overrides") else ""), ""]
    if stale:
        head += [f"> ⚠ **STALE:** {', '.join(stale)} changed since the jobs ran — every number below describes the "
                 f"OLD inputs. Re-run `run_sweeps.py` (new run dir) before acting on this report.", ""]

    toc = ["**Tokens:** " + " · ".join(f"[{s}](#{s.lower()})" for s in syms), ""]
    summary = ["## Summary", "",
               "| token | cost bps (quote) | deployed min·trail·lb·z·regime·fb | deployed min-win / Σ | "
               "best alternative (trail · axes) | Δ min-win / Δ Σ | TRUST | verdict |",
               "|---|---|---|---|---|---|---|---|"]
    bodies = []
    for s in syms:
        cand = common.read_json(run_dir / f"{s}_candidates.json", {})
        cand["_entry"] = entries.get(s)
        v = common.read_json(run_dir / "verdicts" / f"{s}.json")
        dep = cand.get("deployed", {})
        cq = costs_doc.get(s, {})
        cost_s = f"{man.get('costs', {}).get(s, '?')} ({cq['median_bps']:.2f})" if "median_bps" in cq else \
            str(man.get("costs", {}).get(s, "?"))
        if cand.get("suppressed"):
            summary.append(f"| [{s}](#{s.lower()}) | {cost_s} | — | — | — | — | {trust_statuses(cand)} | tables suppressed |")
        else:
            alt = None
            if v and v.get("pick"):
                t = common.fmt_f64(v["pick"]["trail"])
                alt = next(((t, r) for r in cand["trails"].get(t, []) if same_knobs(r["knobs"], v["pick"])), None)
                alt = alt or pick_from_csvs(run_dir, s, v["pick"])  # a pick outside the listed rows
            alt = alt or best_alternative(cand)
            alt_s, d_s = "— deployed is best by maximin", "—"
            if alt:
                t, r = alt
                alt_s = f"{t}% · {', '.join(a for a in r['axes'] if not a.startswith('consensus'))[:60]}"
                if dep.get("min_win") is not None and r["min_win"] is not None:
                    d_s = f"{r['min_win'] - dep['min_win']:+.2f} / {r['sum_win'] - dep['sum_win']:+.2f}"
            dk = dep.get("knobs", {})
            summary.append(
                f"| [{s}](#{s.lower()}) | {cost_s} | {knobs_str(dk) if dk else '—'} | "
                f"{money(dep.get('min_win'))} / {money(dep.get('sum_win'))} | {alt_s} | {d_s} | "
                f"{trust_statuses(cand)} | {(v or {}).get('verdict', 'pending')} |")
        frag = (run_dir / f"{s}_per_trail.md").read_text()
        frag = frag.replace(f"<!-- VERDICT:{s} -->", verdict_text(v, cand))
        bodies.append(frag)

    skipped = man.get("skipped", {})
    tail = []
    if skipped:
        tail += ["## Not swept", ""] + [f"- **{s}** — {st}: " + "; ".join(
            m for _, m in cov.get("tokens", {}).get(s, {}).get("t0", [])) for s, st in skipped.items()] + [""]
    ab = run_dir / "book_ab.md"
    summary += ["", ab.read_text() if ab.exists() else "_Whole-book A/B not run yet (book_ab.py)._", ""]

    report = run_dir / "REPORT.md"
    report.write_text("\n".join(head + toc + summary) + "\n" + "\n\n".join(bodies) + "\n" + "\n".join(tail)
                      + "\n" + BLIND_SPOTS)
    print(f"wrote {report}")


if __name__ == "__main__":
    main()
