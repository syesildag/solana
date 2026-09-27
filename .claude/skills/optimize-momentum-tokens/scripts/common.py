#!/usr/bin/env python3
"""
common.py — shared helpers for the optimize-momentum-tokens skill scripts.

Everything that must be identical across scripts lives here, so a rule is encoded once:
- the safe way to call momentum-sim (HISTORY_MAX_SNAPSHOTS on EVERY call, a slot lock so
  parallel agents never oversubscribe the 10 cores, run-dir inputs only);
- the per-token-sweep cell label, byte-for-byte as the Rust formats it (`fmt_frac`, `round4`,
  f64 Display), because the CSVs of different jobs are joined on that label;
- the deployed ("incumbent") knob values and the grid axes that always contain them;
- the run manifest that makes a stale sweep detectable (.env / tokens / binary drift).

stdlib only, like optimize-momentum-config/scripts/optimize_momentum.py.
"""
import atexit
import contextlib
import csv
import fcntl
import hashlib
import json
import math
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path

WSOL = "So11111111111111111111111111111111111111112"
USDC_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
SOL_POOL = "58oQChx4yWmvKdwLLZzBi4ChoCc2fqCUWBkwMihLYQo2"  # Raydium v4 SOL/USDC — always pin for SOL

# Far above any real file: the loader rewrites (truncates!) its input when the cap resolves
# to 43,200 (the fallback for unset/0/junk), and silently trims when the cap is below the row
# count. One constant, set on every sim call, in the same environment as the binary.
HISTORY_CAP = "100000000"
TRUNCATION_SIGNATURE_ROWS = 43_200

WATCH_ONLY_MIN_METRIC = 10_000.0  # watch-only sentinel in momentum_tokens.json is 100000
DEFAULT_TRAILS = [2.0, 5.0, 10.0, 15.0, 20.0, 30.0]
DEFAULT_LOOKBACKS = [240, 480, 720, 1440]
DEFAULT_ZS = [0.0, 1.0, 1.5]  # 0 = z-gate off
DEFAULT_FADE_FRACS = [1.0, 0.75, 0.5]
DEFAULT_MIN_MULTS = [0.5, 0.75, 1.0, 1.5, 2.0]
Z_OBS = 480  # per-token-sweep sweeps z at one window (--entry-max-z-obs)
SWEPT_KEYS = ("min_metric", "trail_pct", "lookback_obs", "entry_max_z_obs", "entry_max_z",
              "regime_filter", "fade_bar")

SLOT_DIR_NAME = ".momentum_sim_slots"


# ── paths ────────────────────────────────────────────────────────────────────────────────

def repo_root() -> Path:
    """The repo this skill lives in — independent of the caller's cwd (hooks may run from anywhere):
    $CLAUDE_PROJECT_DIR if it holds this skill, else git from the script's own directory, else the
    path this file sits at (<repo>/.claude/skills/optimize-momentum-tokens/scripts/common.py)."""
    here = Path(__file__).resolve()
    proj = os.environ.get("CLAUDE_PROJECT_DIR")
    if proj and (Path(proj) / ".claude" / "skills" / "optimize-momentum-tokens").is_dir():
        return Path(proj)
    out = subprocess.run(["git", "rev-parse", "--show-toplevel"], capture_output=True, text=True, cwd=here.parent)
    return Path(out.stdout.strip()) if out.returncode == 0 else here.parents[4]


def sim_binary(root: Path) -> Path:
    return root / "target" / "release" / "momentum-sim"


def newest_source_mtime(root: Path) -> float:
    return max((p.stat().st_mtime for p in (root / "src").rglob("*.rs")), default=0.0)


def ensure_binary(root: Path, build: bool = True) -> Path:
    """The release binary, rebuilt when missing or older than any src/**/*.rs (a rebuild is not
    a code change; a stale binary would silently replay yesterday's sim)."""
    binp = sim_binary(root)
    stale = not binp.exists() or binp.stat().st_mtime < newest_source_mtime(root)
    if stale:
        if not build:
            sys.exit(f"{binp} is missing or older than src/ — run: cargo build --release --bin momentum-sim")
        print("building momentum-sim (release)…", file=sys.stderr)
        r = subprocess.run(["cargo", "build", "--release", "--bin", "momentum-sim"], cwd=root)
        if r.returncode != 0:
            sys.exit("cargo build failed")
    return binp


# ── .env / tokens ────────────────────────────────────────────────────────────────────────

def read_env(root: Path) -> dict:
    """Plain KEY=VALUE lines of .env (repo convention: comments on their own lines, no inline
    comments). Values are strings; surrounding quotes stripped."""
    env = {}
    p = root / ".env"
    if not p.exists():
        return env
    for line in p.read_text().splitlines():
        s = line.strip()
        if not s or s.startswith("#") or "=" not in s:
            continue
        k, v = s.split("=", 1)
        env[k.strip()] = v.strip().strip('"').strip("'")
    return env


def momentum_env(env: dict) -> dict:
    return {k: v for k, v in sorted(env.items()) if k.startswith("MOMENTUM_")}


def sha256_bytes(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()


def sha256_file(path: Path, chunk: int = 1 << 20) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while True:
            b = f.read(chunk)
            if not b:
                break
            h.update(b)
    return h.hexdigest()


def env_hash(env: dict) -> str:
    return sha256_bytes(json.dumps(momentum_env(env), sort_keys=True).encode())[:12]


def load_tokens(path: Path) -> list:
    return json.loads(Path(path).read_text())


def is_watch_only(entry: dict) -> bool:
    p = entry.get("params") or {}
    mm = p.get("min_metric")
    return mm is not None and float(mm) >= WATCH_ONLY_MIN_METRIC


def deployed_targets(entries: list, only=None) -> list:
    """Tokens the skill tunes: a `params` block, not watch-only; `only` = symbols/mints filter."""
    want = {s.lower() for s in only} if only else None
    out = []
    for e in entries:
        if not e.get("params") or is_watch_only(e):
            continue
        if want and e["symbol"].lower() not in want and e["mint"].lower() not in want:
            continue
        out.append(e)
    return out


# ── Rust-identical number formatting (labels are the join key between job CSVs) ─────────

def rust_round(x: float) -> float:
    """f64::round — half away from zero (Python's round() is banker's rounding)."""
    return math.floor(x + 0.5) if x >= 0 else -math.floor(-x + 0.5)


def round4(v: float) -> float:
    return rust_round(v * 10_000.0) / 10_000.0


def fmt_f64(v: float) -> str:
    """Rust `{}` Display of an f64 for the magnitudes that occur here: shortest round-trip
    digits, no trailing `.0` (10.0 → "10", 1.5 → "1.5", 3.6564 → "3.6564")."""
    v = float(v)
    if v == int(v) and abs(v) < 1e15:
        return str(int(v))
    s = repr(v)
    if "e" in s or "E" in s:  # never expected at these magnitudes; keep it loud
        raise ValueError(f"unexpected exponent formatting for {v!r}")
    return s


def fmt_frac(f: float) -> str:
    """Mirror of momentum_sim.rs `fmt_frac`: 4 decimals, trailing zeros trimmed, -0 → 0."""
    s = f"{f:.4f}".rstrip("0").rstrip(".")
    return "0" if s == "-0" else s


def min_axis(bar: float, mults=DEFAULT_MIN_MULTS) -> list:
    """per-token-sweep's own default rounding: (bar × f × 10000).round() / 10000."""
    return [rust_round(bar * f * 10_000.0) / 10_000.0 for f in mults]


def cell_label(mn: float, trail: float, lb: int, z: float, regime_exempt: bool, fb: float,
               z_obs: int = Z_OBS) -> str:
    """The per-token-sweep label (`momentum_sim.rs` grid closure), for matching CSV rows."""
    zs = f"{fmt_f64(z)}@{z_obs}" if z > 0 else "off"
    return (f"min={fmt_f64(mn)} trail={fmt_f64(trail)} lb={int(lb)} z={zs} "
            f"regime={'exempt' if regime_exempt else 'gated'} fb={fmt_frac(fb)}")


def parse_label(label: str) -> dict:
    """`min=… trail=… lb=… z=…@480|off regime=gated|exempt fb=…` → typed knobs. Family sets
    (`{a;b}` in the CSV, `{a,b}` in the text) are returned as sorted lists."""
    out = {}
    for part in label.split():
        if "=" not in part:
            continue
        k, raw = part.split("=", 1)
        vals = raw[1:-1].replace(";", ",").split(",") if raw.startswith("{") else [raw]
        typed = [_typed(k, v) for v in vals]
        out[k] = typed if len(typed) > 1 else typed[0]
    return out


def _typed(k: str, v: str):
    if k in ("min", "trail", "fb"):
        return float(v)
    if k == "lb":
        return int(v)
    if k == "z":
        return 0.0 if v == "off" else float(v.split("@")[0])
    return v  # regime


# ── deployed knobs and axes ──────────────────────────────────────────────────────────────

def deployed_knobs(params: dict, env: dict) -> dict:
    """The incumbent's six swept knobs, with the global fallbacks the live trader uses."""
    # fallbacks = the live trader's own defaults (src/portfolio/mod.rs) for a missing .env value
    mn = float(params.get("min_metric", env.get("MOMENTUM_MIN_METRIC", 0.5)))
    trail = float(params.get("trail_pct", env.get("MOMENTUM_TRAIL_PCT", 5)))
    lb = int(params.get("lookback_obs", env.get("MOMENTUM_LOOKBACK_OBS", 121)))
    z_obs = params.get("entry_max_z_obs")
    z = 0.0 if z_obs in (0, None) or params.get("entry_max_z") is None else float(params["entry_max_z"])
    fade_bar = params.get("fade_bar")
    fb = (float(fade_bar) / mn) if (fade_bar is not None and mn > 0) else 1.0
    return {
        "min": mn, "trail": trail, "lb": lb, "z": z,
        "z_obs": int(z_obs) if z_obs not in (None, 0) else 0,
        "regime_exempt": params.get("regime_filter") is False,
        "fb": fb,
    }


def deployed_label(params: dict, env: dict, trail=None) -> str:
    k = deployed_knobs(params, env)
    return cell_label(round4(k["min"]), k["trail"] if trail is None else trail, k["lb"], k["z"],
                      k["regime_exempt"], float(fmt_frac(k["fb"])))


def axes_for(params: dict, env: dict, trails=None, lookbacks=None, zs=None, fracs=None,
             min_mults=None) -> dict:
    """Grid axes for one token: the defaults ∪ every deployed value, so DEPLOYED@T is a real
    cell at every trail and an inert set can always resolve to the deployed value."""
    k = deployed_knobs(params, env)
    uniq = lambda xs: sorted(set(xs))
    t = uniq(list(trails or DEFAULT_TRAILS) + [k["trail"]])
    lbs = uniq(list(lookbacks or DEFAULT_LOOKBACKS) + [k["lb"]])
    z = uniq(list(zs or DEFAULT_ZS) + [k["z"]])
    fr = uniq([float(fmt_frac(x)) for x in list(fracs or DEFAULT_FADE_FRACS) + [k["fb"]]])
    mins = uniq(min_axis(k["min"], min_mults or DEFAULT_MIN_MULTS))
    warnings = []
    if k["z"] > 0 and k["z_obs"] != Z_OBS:
        warnings.append(f"deployed z window {k['z_obs']} ≠ swept {Z_OBS}: DEPLOYED is not a grid cell")
    if "entry_max_z_obs" not in params:
        warnings.append("no explicit entry_max_z_obs: the sim hard-codes the global z-gate off (base_params)")
    return {"mins": mins, "trails": t, "lookbacks": lbs, "zs": z, "fracs": fr, "warnings": warnings,
            "n_cells": len(mins) * len(t) * len(lbs) * len(z) * 2 * len(fr)}


def params_for_knobs(params: dict, knobs: dict) -> dict:
    """Deployed params with ONLY the six swept knobs replaced (key order preserved; fade_bar
    absolute = round4(frac × min), removed at frac 1; z off ⇒ entry_max_z_obs 0, no entry_max_z)."""
    p = dict(params)
    mn = float(knobs["min"])
    p["min_metric"] = mn
    p["trail_pct"] = _num(knobs["trail"])
    p["lookback_obs"] = int(knobs["lb"])
    if float(knobs["z"]) > 0:
        p["entry_max_z_obs"] = int(knobs.get("z_obs") or Z_OBS)
        p["entry_max_z"] = _num(knobs["z"])
    else:
        p["entry_max_z_obs"] = 0
        p.pop("entry_max_z", None)
    p["regime_filter"] = not bool(knobs["regime_exempt"])
    fb = float(knobs["fb"])
    if abs(fb - 1.0) < 1e-9:
        p.pop("fade_bar", None)
    else:
        p["fade_bar"] = round4(fb * mn)
    return p


def _num(v):
    v = float(v)
    return int(v) if v == int(v) else v


# ── running the sim safely ───────────────────────────────────────────────────────────────

@contextlib.contextmanager
def sim_slot(root: Path, slots: int, poll: float = 1.0):
    """Hold one of `slots` machine-wide flock slots while a sim runs. Shared by run_sweeps.py,
    book_ab.py and any analyst agent, so concurrent callers queue instead of oversubscribing."""
    d = root / "assets" / SLOT_DIR_NAME
    d.mkdir(parents=True, exist_ok=True)
    while True:
        if STOPPING.is_set():
            raise Stopping()
        for i in range(max(1, slots)):
            fh = open(d / f"slot{i}.lock", "w")
            try:
                fcntl.flock(fh, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                fh.close()
                continue
            try:
                yield i
            finally:
                fcntl.flock(fh, fcntl.LOCK_UN)
                fh.close()
            return
        time.sleep(poll)


def sim_env(extra: dict = None, rayon_threads: int = None) -> dict:
    env = dict(os.environ)
    env["HISTORY_MAX_SNAPSHOTS"] = HISTORY_CAP
    if rayon_threads:
        env["RAYON_NUM_THREADS"] = str(rayon_threads)
    for k, v in (extra or {}).items():
        env[k] = str(v)
    return env


def run_sim(root: Path, args: list, stdout_path: Path, extra_env: dict = None, slots: int = 1,
            rayon_threads: int = None, guard: "Tripwire" = None) -> int:
    """momentum-sim <args> > stdout_path, under a slot lock, with the history cap set. The
    tripwire (if given) is checked after the call: the canonical book must be untouched."""
    binp = sim_binary(root)
    stdout_path.parent.mkdir(parents=True, exist_ok=True)
    try:
        with sim_slot(root, slots):
            with open(stdout_path, "w") as out:
                rc = run_child([str(binp)] + [str(a) for a in args], cwd=root, stdout=out,
                               stderr=subprocess.STDOUT, env=sim_env(extra_env, rayon_threads))
    except Stopping:
        return -signal.SIGTERM
    if guard is not None:
        guard.check()
    return rc


# ── cascade: stopping a long-running script stops everything it started ────────────────
# Discovery of background work is generic and lives outside this skill: Claude Code tags every
# process its Bash tool launches (CLAUDE_CODE_SESSION_ID / CLAUDE_PID), and the user-level session
# reaper (.claude/hooks/claude_reaper.py, installed by install_claude_reaper.py) finds, reports and
# stops it. The one thing a script must still do itself is forward termination to its children.

STOPPING = threading.Event()  # set by a stop signal: no new child may start after it
_children = set()
_children_lock = threading.Lock()


class Stopping(Exception):
    """Raised instead of starting new work once a stop signal has arrived."""


def install_cascade(label: str):
    """SIGTERM/SIGINT/SIGHUP, and interpreter exit, terminate every child this process started.
    `label` is exported as CLAUDE_TASK_LABEL, so the reaper's `status` names what the children
    belong to (children inherit it; nothing is registered anywhere)."""
    os.environ["CLAUDE_TASK_LABEL"] = label
    atexit.register(_on_exit)
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, _on_signal)


def _on_exit():
    STOPPING.set()
    terminate_children()


def _on_signal(signum, frame):
    STOPPING.set()
    terminate_children()
    raise SystemExit(128 + signum)


def terminate_children(grace: float = 5.0):
    """SIGTERM every live child, SIGKILL whatever is still alive after `grace` seconds."""
    with _children_lock:
        live = [p for p in _children if p.poll() is None]
    for p in live:
        try:
            p.terminate()
        except ProcessLookupError:
            pass
    deadline = time.time() + grace
    for p in live:
        try:
            p.wait(timeout=max(0.0, deadline - time.time()))
        except subprocess.TimeoutExpired:
            p.kill()


def run_child(cmd: list, **kw) -> int:
    """subprocess.run replacement whose child is tracked for the cascade. Refuses to start once a
    stop signal arrived (and stops a child that raced past the check)."""
    if STOPPING.is_set():
        raise Stopping()
    p = subprocess.Popen(cmd, **kw)
    with _children_lock:
        _children.add(p)
    try:
        if STOPPING.is_set():
            p.terminate()
        return p.wait()
    finally:
        with _children_lock:
            _children.discard(p)


class Tripwire:
    """Size+mtime of a file that must never change during a run (the canonical book)."""

    def __init__(self, path: Path):
        self.path = Path(path)
        st = self.path.stat()
        self.sig = (st.st_size, st.st_mtime_ns)

    def check(self):
        st = self.path.stat()
        if (st.st_size, st.st_mtime_ns) != self.sig:
            sys.exit(f"TRIPWIRE: {self.path} changed during the run — stop and restore it from "
                     f"assets/history_backups/ before trusting anything")


# ── CSV / JSON helpers ───────────────────────────────────────────────────────────────────

NUMERIC = {"pnl_train", "pnl_test", "win_test", "hold_h_train", "hold_h_test", "std_test",
           "worst_test", "true_dd_test", "token_pnl_test", "worst_train", "true_dd_train",
           "std_train", "best_train", "best_test", "open_train", "open_test"}
INTEGER = {"trades_train", "trades_test"}
EXTENDED_COLUMNS = ("worst_train", "true_dd_train", "std_train", "best_train", "best_test",
                    "open_train", "open_test")


def read_sweep_csv(path: Path) -> list:
    rows = []
    with open(path, newline="") as f:
        for r in csv.DictReader(f):
            for k in list(r):
                if k in NUMERIC:
                    r[k] = float(r[k])
                elif k in INTEGER:
                    r[k] = int(r[k])
            rows.append(r)
    if rows and not all(c in rows[0] for c in EXTENDED_COLUMNS):
        sys.exit(f"{path}: old CSV layout (no {EXTENDED_COLUMNS[0]}…). Rebuild momentum-sim "
                 f"(cargo build --release --bin momentum-sim) and re-run the jobs.")
    return rows


def write_json(path: Path, obj):
    path = Path(path)
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, indent=2) + "\n")
    tmp.replace(path)


def read_json(path: Path, default=None):
    p = Path(path)
    return json.loads(p.read_text()) if p.exists() else default


def iter_jsonl(path: Path):
    with open(path) as f:
        for line in f:
            line = line.strip()
            if line:
                yield json.loads(line)


def newest_book(root: Path):
    """Most recently written book (mtime: a name sort puts `_10` before `_9`)."""
    books = sorted((root / "assets").glob("price_history.book_*.jsonl"), key=lambda p: p.stat().st_mtime)
    return books[-1] if books else None


def utc(ts: float) -> str:
    return time.strftime("%Y-%m-%d %H:%M", time.gmtime(ts))
