#!/usr/bin/env python3
"""
apply_params.py — chosen combinations → a candidate tokens file; with --apply, the live file.

Only the six swept knobs change (min_metric, trail_pct, lookback_obs, entry_max_z_obs +
entry_max_z, regime_filter, fade_bar); every other key of the entry — pool/quote/name, and the
operator-set params like trade_usdc, regime_exit_obs, max_run_pct — is preserved, in order.
`per-token-tune --apply` is NOT used: it replaces params wholesale and drops lb/z/regime_exit.

Backups go to assets/ (momentum_tokens.json is not in git, and the old skill's scratchpad
backups disappeared with their sessions). The watcher is never restarted by this script —
params load at startup, so the operator restarts it.

Usage:
  python3 apply_params.py --run-dir <run> --from-verdicts            # → <run>/candidate_tokens.json
  python3 apply_params.py --run-dir <run> --choices picks.json       # {SYM: {trail,min,lb,z,regime,fb}} — override

--from-verdicts applies only 'change' verdicts, and refuses one whose pick is not the token's
min-trail pick (<SYM>_candidates.json → min_trail.pick); --choices is the operator's explicit override.
  python3 apply_params.py --run-dir <run> --from-verdicts --apply    # backup + write the live file
"""
import argparse
import difflib
import json
import shutil
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402


def choices_from_verdicts(run_dir: Path) -> dict:
    out = {}
    for p in sorted((run_dir / "verdicts").glob("*.json")):
        v = json.loads(p.read_text())
        if v.get("verdict") == "change" and v.get("pick"):
            out[v["token"]] = v["pick"]
    return out


def to_knobs(pick: dict) -> dict:
    return {"min": float(pick["min"]), "trail": float(pick["trail"]), "lb": int(pick["lb"]),
            "z": float(pick.get("z") or 0.0), "z_obs": int(pick.get("z_obs") or common.Z_OBS),
            "regime_exempt": pick.get("regime") == "exempt", "fb": float(pick.get("fb", 1.0))}


def same_pick(a: dict, b: dict) -> bool:
    """Two picks name the same grid cell: all six swept knobs, the trail included."""
    ka, kb = to_knobs(a), to_knobs(b)
    return (all(abs(ka[x] - kb[x]) < 1e-9 for x in ("min", "trail", "z", "fb"))
            and (ka["lb"], ka["regime_exempt"]) == (kb["lb"], kb["regime_exempt"]))


def rule_mismatches(run_dir: Path, choices: dict) -> list:
    """A 'change' verdict must apply the min-trail pick its run computed (reading-rules §5): another
    row, or a change where the rule found no winner, is refused — a deliberate operator override goes
    through --choices instead. Runs made before the rule (no `min_trail` block) are not checked."""
    bad = []
    for sym, pick in sorted(choices.items()):
        mt = common.read_json(run_dir / f"{sym}_candidates.json", {}).get("min_trail")
        if mt is None:
            continue
        rule = (mt.get("pick") or {}).get("knobs")
        if rule is None:
            bad.append(f"{sym}: verdict says change, but no trail rung holds a winner (rule: {mt.get('verdict')})")
        elif not same_pick(pick, rule):
            bad.append(f"{sym}: verdict pick {json.dumps(pick)} ≠ min-trail pick {json.dumps(rule)}")
    return bad


def merge(entries: list, choices: dict) -> tuple:
    out, changed = [], []
    for e in entries:
        e2 = dict(e)
        if e["symbol"] in choices:
            e2["params"] = common.params_for_knobs(e.get("params") or {}, to_knobs(choices[e["symbol"]]))
            changed.append(e["symbol"])
        out.append(e2)
    unknown = sorted(set(choices) - {e["symbol"] for e in entries})
    if unknown:
        sys.exit(f"choices for tokens not in the file: {unknown}")
    return out, changed


def dumps(entries: list) -> str:
    return json.dumps(entries, indent=2, ensure_ascii=False) + "\n"


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--run-dir", required=True)
    src = ap.add_mutually_exclusive_group(required=True)
    src.add_argument("--choices", help="JSON {SYM: {trail, min, lb, z, regime, fb}}")
    src.add_argument("--from-verdicts", action="store_true", help="use <run>/verdicts/*.json picks (verdict=change)")
    ap.add_argument("--out", default=None, help="candidate file (default <run>/candidate_tokens.json)")
    ap.add_argument("--apply", action="store_true", help="back up and write assets/momentum_tokens.json")
    ap.add_argument("--tag", default="sweep", help="backup name tag")
    args = ap.parse_args()

    root = common.repo_root()
    run_dir = Path(args.run_dir)
    choices = choices_from_verdicts(run_dir) if args.from_verdicts else json.loads(Path(args.choices).read_text())
    bad = rule_mismatches(run_dir, choices) if args.from_verdicts else []
    if bad:
        sys.exit("refused — a 'change' verdict must apply its token's min-trail pick (reading-rules §5):\n  "
                 + "\n  ".join(bad) + "\nFix the verdict, or pass a deliberate override with --choices.")
    if not choices:
        print("no changes chosen — nothing to write (every verdict is keep/paper-test/insufficient)")
        return

    live = root / "assets" / "momentum_tokens.json"
    base_path = live if args.apply else run_dir / "deployed_tokens.json"
    base_text = base_path.read_text()
    if args.apply:
        man = common.read_json(run_dir / "manifest.json", {})
        if man.get("tokens_sha") and man["tokens_sha"] != common.sha256_file(live)[:12]:
            print("WARNING: momentum_tokens.json changed since the sweep ran — only the six swept knobs of the "
                  "chosen tokens are replaced; every other change in the live file is kept.", file=sys.stderr)
    new_entries, changed = merge(json.loads(base_text), choices)
    new_text = dumps(new_entries)
    sys.stdout.writelines(difflib.unified_diff(base_text.splitlines(True), new_text.splitlines(True),
                                               fromfile=str(base_path), tofile="(new)"))
    if args.apply:
        bak = root / "assets" / f"momentum_tokens.pre_{args.tag}_{time.strftime('%Y%m%d_%H%M%S')}.bak"
        shutil.copyfile(live, bak)
        tmp = live.with_suffix(".json.tmp")
        tmp.write_text(new_text)
        tmp.replace(live)
        print(f"\nbacked up → {bak}\nwrote {live} ({', '.join(changed)}). "
              f"Params load at startup: restart the watcher yourself when ready.")
    else:
        out = Path(args.out) if args.out else run_dir / "candidate_tokens.json"
        out.write_text(new_text)
        print(f"\nwrote candidate {out} ({', '.join(changed)}) — the live file is untouched")


if __name__ == "__main__":
    main()
