#!/usr/bin/env python3
"""
claude_reaper.py — find and stop background work left behind by Claude Code sessions.

Generic: no skill or script has to register anything. Claude Code exports CLAUDE_CODE_SESSION_ID and
CLAUDE_PID into every command its Bash tool runs, and children inherit them — so every process can be
traced to the session that started it, and "is that session still alive?" is a liveness check on
CLAUDE_PID. (Observed on Claude Code 2.1.x; not documented — hence the parent-chain fallback below.)

  status [--json]                               background work grouped by process group: RUNNING / ORPHAN
  stop (PID|PGID)… | --orphans [--session ID] [--grace S] [--dry-run]
  hook-start                                    SessionStart hook: report orphans (→ Claude's context)
  hook-end                                      SessionEnd hook: apply the session_end policy

Opt-ins, set on the launching command (children inherit them):
  CLAUDE_REAPER_TIE=1        stop this work when its session ends (with session_end = "tied")
  CLAUDE_TASK_LABEL=text     label shown by status, e.g. "optimize-momentum-tokens:run_sweeps"

Config ~/.claude/claude_reaper.json (override: $CLAUDE_REAPER_CONFIG):
  {"never_kill": ["pattern", …], "session_end": "report" | "tied" | "all"}      default: report

Safety. "Work" is a process group whose every live member is either a tagged process or a Bash-tool
shell; a group shared with anything else — Claude Code itself, the IDE, the group an MCP server lives
in — is never shown or touched, nor any group with a member matching `never_kill`. Stops are
group-wide (a background task and everything it spawned share one group): SIGTERM, then SIGKILL after
the grace period. An untagged member (a shell wrapper, an env-scrubbing tool) is attributed through
its parent chain: Bash-tool shells run `…/.claude/shell-snapshots/snapshot-…`.

Portable: macOS reads tags with `ps -E`, Linux from /proc/<pid>/environ.
"""
import argparse
import json
import os
import re
import signal
import subprocess
import sys
import time
from pathlib import Path

CLAUDE_CMD = re.compile(r"(^|/)claude( |$)|@anthropic-ai/claude-code/|/claude-code/cli\.js")
BASH_TOOL_SHELL = "/.claude/shell-snapshots/snapshot-"
TAG_KEYS = ("CLAUDE_CODE_SESSION_ID", "CLAUDE_PID", "CLAUDE_REAPER_TIE", "CLAUDE_TASK_LABEL")
DEFAULT_CONFIG = {"never_kill": [], "session_end": "report"}


# ── configuration ────────────────────────────────────────────────────────────────────────

def config_path() -> Path:
    return Path(os.environ.get("CLAUDE_REAPER_CONFIG") or Path.home() / ".claude" / "claude_reaper.json")


def load_config() -> dict:
    cfg = dict(DEFAULT_CONFIG)
    try:
        cfg.update(json.loads(config_path().read_text()))
    except (OSError, ValueError):
        pass
    if cfg.get("session_end") not in ("report", "tied", "all"):
        cfg["session_end"] = "report"
    return cfg


def log_path() -> Path:
    return config_path().with_name("claude_reaper.log")


# ── process table and tags ───────────────────────────────────────────────────────────────

def read_processes() -> dict:
    """{pid: {pid, ppid, pgid, etime, cmd}} of live processes. Zombies (state Z) are gone: macOS even
    refuses killpg on a zombie-only group with EPERM. -A = every process on macOS and Linux (macOS
    `-e` means "show the environment"), -ww = never truncate, `args` = full command line in both."""
    out = subprocess.run(["ps", "-ww", "-A", "-o", "pid=,ppid=,pgid=,stat=,etime=,args="],
                         capture_output=True, text=True).stdout
    procs = {}
    for line in out.splitlines():
        parts = line.split(None, 5)
        if len(parts) < 6 or not parts[0].isdigit() or parts[3].startswith("Z"):
            continue
        pid = int(parts[0])
        procs[pid] = {"pid": pid, "ppid": int(parts[1]), "pgid": int(parts[2]), "etime": parts[4], "cmd": parts[5]}
    return procs


def parse_env_text(text: str) -> dict:
    """Tags from a `ps -E` line (command and environment space-joined)."""
    tags = {}
    for key in TAG_KEYS:
        m = re.search(rf"(?:^| ){key}=(\S*)", text)
        if m:
            tags[key] = m.group(1)
    return tags


def parse_proc_environ(raw: bytes) -> dict:
    """Tags from a Linux /proc/<pid>/environ blob (NUL-separated KEY=VALUE)."""
    tags = {}
    for item in raw.split(b"\0"):
        key, _, value = item.partition(b"=")
        k = key.decode(errors="replace")
        if k in TAG_KEYS:
            tags[k] = value.decode(errors="replace")
    return tags


def read_tags(procs: dict) -> dict:
    """{pid: tags} for processes carrying a Claude session tag (own processes only)."""
    tags = {}
    if sys.platform == "darwin":
        out = subprocess.run(["ps", "-ww", "-A", "-E", "-o", "pid=,args="], capture_output=True, text=True).stdout
        for line in out.splitlines():
            head, _, rest = line.strip().partition(" ")
            if head.isdigit() and int(head) in procs:
                t = parse_env_text(rest)
                if "CLAUDE_CODE_SESSION_ID" in t:
                    tags[int(head)] = t
    else:
        for pid in procs:
            try:
                t = parse_proc_environ(Path(f"/proc/{pid}/environ").read_bytes())
            except OSError:
                continue
            if "CLAUDE_CODE_SESSION_ID" in t:
                tags[pid] = t
    return tags


def is_claude(proc: dict) -> bool:
    return bool(proc) and bool(CLAUDE_CMD.search(proc["cmd"].split(" --")[0]))


def is_bash_tool_shell(proc: dict) -> bool:
    return BASH_TOOL_SHELL in proc["cmd"]


def owned(m: dict, in_group: dict, tags: dict) -> bool:
    """A member is Claude's if it is tagged, is a Bash-tool shell, or descends — within its own group —
    from one that is. The descent rule matters on macOS: SIP hides the environment of Apple system
    binaries (/bin/sh, zsh, sleep, tee …) even from their owner, so `sleep 3` in a Bash-tool loop
    shows no tag of its own."""
    seen, cur = set(), m
    while cur is not None and cur["pid"] not in seen:
        seen.add(cur["pid"])
        if cur["pid"] in tags or is_bash_tool_shell(cur):
            return True
        cur = in_group.get(cur["ppid"])
    return False


def claude_ancestor(pid: int, procs: dict):
    seen = set()
    while pid in procs and pid not in seen and pid > 1:
        seen.add(pid)
        if is_claude(procs[pid]):
            return pid
        pid = procs[pid]["ppid"]
    return None


def own_pgids() -> set:
    """This process's group and its ancestors' groups: never reported, never stopped."""
    procs = read_processes()
    pgids, pid = set(), os.getpid()
    while pid in procs and pid > 1:
        pgids.add(procs[pid]["pgid"])
        pid = procs[pid]["ppid"]
    pgids.add(os.getpgid(0))
    return pgids


# ── grouping ─────────────────────────────────────────────────────────────────────────────

def work_groups(procs: dict, tags: dict, exclude: set = frozenset()) -> list:
    """Process groups that are Claude background work, each classified RUNNING or ORPHAN."""
    by_pgid = {}
    for p in procs.values():
        by_pgid.setdefault(p["pgid"], []).append(p)
    groups = []
    for pgid, members in by_pgid.items():
        if pgid <= 1 or pgid in exclude:
            continue
        tagged = [m for m in members if m["pid"] in tags]
        shells = [m for m in members if is_bash_tool_shell(m)]
        if not tagged and not shells:
            continue
        in_group = {m["pid"]: m for m in members}
        foreign = [m for m in members if not owned(m, in_group, tags)]
        if foreign or any(is_claude(m) for m in members):
            continue  # shared with the IDE / Claude Code / an MCP server's group: not ours to judge
        sessions = sorted({tags[m["pid"]]["CLAUDE_CODE_SESSION_ID"] for m in tagged})
        owners = {int(tags[m["pid"]]["CLAUDE_PID"]) for m in tagged if tags[m["pid"]].get("CLAUDE_PID", "").isdigit()}
        alive = any(o in procs and is_claude(procs[o]) for o in owners)
        if not owners:  # untagged shell-only group: attribute through the parent chain
            alive = any(claude_ancestor(s["pid"], procs) for s in shells)
        workers = [m for m in members if not is_bash_tool_shell(m)] or members
        groups.append({
            "pgid": pgid, "state": "RUNNING" if alive else "ORPHAN", "sessions": sessions,
            "owners": sorted(owners), "members": len(members),
            "tied": any(tags.get(m["pid"], {}).get("CLAUDE_REAPER_TIE") == "1" for m in members),
            "label": next((tags[m["pid"]]["CLAUDE_TASK_LABEL"] for m in tagged if tags[m["pid"]].get("CLAUDE_TASK_LABEL")), ""),
            "age": max((m["etime"] for m in members), key=etime_seconds),
            "command": min(workers, key=lambda m: m["pid"])["cmd"],
        })
    return sorted(groups, key=lambda g: (g["state"] != "ORPHAN", g["pgid"]))


def etime_seconds(etime: str) -> int:
    days, _, rest = etime.rpartition("-")
    parts = [int(x) for x in rest.split(":")]
    while len(parts) < 3:
        parts.insert(0, 0)
    return (int(days) if days else 0) * 86400 + parts[0] * 3600 + parts[1] * 60 + parts[2]


def snapshot() -> list:
    procs = read_processes()
    return work_groups(procs, read_tags(procs), exclude=own_pgids())


def describe(g: dict) -> str:
    sess = ",".join(s[:8] for s in g["sessions"]) or "?"
    what = g["label"] or g["command"][:80]
    return (f"{g['state']:<7} group {g['pgid']} · {g['members']} proc · up {g['age']} · session {sess}"
            f"{' · tied' if g['tied'] else ''} · {what}")


# ── stopping ─────────────────────────────────────────────────────────────────────────────

def refusal(pgid: int, cfg: dict, exclude: set) -> str:
    """Why this group must NOT be stopped ("" = safe). Re-evaluated on a fresh process table."""
    procs = read_processes()
    members = [p for p in procs.values() if p["pgid"] == pgid]
    if pgid <= 1:
        return "system group"
    if pgid in exclude:
        return "the reaper's own group (or its caller's)"
    if not members:
        return "already gone"
    for m in members:
        if is_claude(m):
            return f"contains Claude Code (pid {m['pid']})"
        hit = next((pat for pat in cfg["never_kill"] if pat and pat in m["cmd"]), None)
        if hit:
            return f"never_kill match {hit!r}: {m['cmd'][:60]}"
    if not any(g["pgid"] == pgid for g in work_groups(procs, read_tags(procs))):
        return "not Claude background work (shared with, or made of, untagged processes)"
    return ""


def live_members(pgid: int) -> list:
    return [p for p in read_processes().values() if p["pgid"] == pgid]


def stop_group(pgid: int, cfg: dict, exclude: set, grace: float = 10.0, dry_run: bool = False) -> str:
    why = refusal(pgid, cfg, exclude)
    if why:
        return f"group {pgid}: REFUSED — {why}"
    if dry_run:
        return f"group {pgid}: would stop {len(live_members(pgid))} process(es)"
    try:
        os.killpg(pgid, signal.SIGTERM)
    except ProcessLookupError:
        return f"group {pgid}: already gone"
    deadline = time.time() + grace
    while time.time() < deadline:
        if not live_members(pgid):
            return f"group {pgid}: stopped (SIGTERM)"
        time.sleep(0.2)
    try:
        os.killpg(pgid, signal.SIGKILL)
    except ProcessLookupError:
        return f"group {pgid}: stopped (SIGTERM)"
    except PermissionError:  # only zombies left (macOS answers EPERM)
        left = live_members(pgid)
        return f"group {pgid}: stopped (SIGTERM)" if not left else f"group {pgid}: SIGKILL refused — {len(left)} left"
    return f"group {pgid}: killed (SIGKILL after {grace:.0f}s)"


def stop(targets=None, orphans=False, session=None, grace=10.0, dry_run=False) -> list:
    cfg, exclude = load_config(), own_pgids()
    procs = read_processes()
    groups = work_groups(procs, read_tags(procs), exclude)
    if targets:
        pgids = []
        for t in targets:  # a PID or a PGID
            pgids.append(procs[t]["pgid"] if t in procs else t)
    else:
        pgids = [g["pgid"] for g in groups if (not orphans or g["state"] == "ORPHAN")
                 and (session is None or session in g["sessions"])]
    return [stop_group(pg, cfg, exclude, grace, dry_run) for pg in dict.fromkeys(pgids)] or ["nothing to stop"]


# ── hooks ────────────────────────────────────────────────────────────────────────────────

def hook_start(payload: dict) -> str:
    """SessionStart: JSON only when there are orphans (silence otherwise)."""
    me = payload.get("session_id")
    orphans = [g for g in snapshot() if g["state"] == "ORPHAN"]
    if not orphans:
        return ""
    lines = [describe(g) + (" · (this session, before a resume)" if me in g["sessions"] else "") for g in orphans]
    ctx = ("Background work left by ended Claude Code sessions (claude_reaper):\n  " + "\n  ".join(lines) +
           "\nTell the user. Offer to stop them (python3 ~/.claude/hooks/claude_reaper.py stop --orphans, or "
           "stop <pgid>) or to let them finish; never stop one without asking. `status` shows the current state.")
    msg = f"⚠ {len(orphans)} orphaned background job group(s) from ended Claude sessions — ask Claude, or run claude_reaper.py status"
    return json.dumps({"systemMessage": msg, "hookSpecificOutput": {"hookEventName": "SessionStart",
                                                                     "additionalContext": ctx}})


def hook_end(payload: dict) -> list:
    """SessionEnd: policy report (default) logs the session's work; tied stops groups started with
    CLAUDE_REAPER_TIE=1; all stops every group of the session. Refusals always apply."""
    session = payload.get("session_id")
    if not session:
        return []
    cfg, exclude = load_config(), own_pgids()
    procs = read_processes()
    log = []
    for g in work_groups(procs, read_tags(procs), exclude):
        if session not in g["sessions"]:
            continue
        if cfg["session_end"] == "all" or (cfg["session_end"] == "tied" and g["tied"]):
            log.append(stop_group(g["pgid"], cfg, exclude, grace=5.0))
        else:
            log.append(f"left running: {describe(g)}")
    try:
        with open(log_path(), "a") as f:
            f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')} session {session} ended ({payload.get('reason', '?')}, "
                    f"policy {cfg['session_end']}): {'; '.join(log) or 'no background work'}\n")
    except OSError:
        pass
    return log


# ── main ─────────────────────────────────────────────────────────────────────────────────

def read_payload() -> dict:
    try:
        return json.loads(sys.stdin.read() or "{}") if not sys.stdin.isatty() else {}
    except ValueError:
        return {}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    st = sub.add_parser("status")
    st.add_argument("--json", action="store_true")
    sp = sub.add_parser("stop")
    sp.add_argument("targets", nargs="*", type=int, help="PIDs or PGIDs")
    sp.add_argument("--orphans", action="store_true")
    sp.add_argument("--session", default=None, help="only this session's groups")
    sp.add_argument("--grace", type=float, default=10.0)
    sp.add_argument("--dry-run", action="store_true")
    sub.add_parser("hook-start")
    sub.add_parser("hook-end")
    args = ap.parse_args()

    if args.cmd == "status":
        groups = snapshot()
        if args.json:
            print(json.dumps(groups, indent=2))
        else:
            print("\n".join(describe(g) for g in groups) or "no Claude background work")
    elif args.cmd == "stop":
        if not (args.targets or args.orphans or args.session):
            sys.exit("stop what? give PIDs/PGIDs, --orphans or --session ID")
        print("\n".join(stop(args.targets, args.orphans, args.session, args.grace, args.dry_run)))
    elif args.cmd == "hook-start":
        try:
            out = hook_start(read_payload())
        except Exception as ex:  # a hook must never break session start
            out = json.dumps({"systemMessage": f"claude_reaper hook-start failed: {ex}"})
        if out:
            print(out)
    elif args.cmd == "hook-end":
        try:
            hook_end(read_payload())
        except Exception as ex:  # never block a session exit
            print(f"claude_reaper hook-end: {ex}", file=sys.stderr)


if __name__ == "__main__":
    main()
