#!/usr/bin/env python3
"""
install_claude_reaper.py — install the generic session reaper as a USER-level Claude Code hook, so
every project and skill on this machine gets orphan reports. Run once per machine (macOS or Linux):

  python3 .claude/hooks/install_claude_reaper.py [--dry-run]      # install / update (idempotent)
  python3 .claude/hooks/install_claude_reaper.py --uninstall      # remove the hooks and the copy

What it does:
  1. copies .claude/hooks/claude_reaper.py → ~/.claude/hooks/claude_reaper.py
  2. MERGES a SessionStart (startup|resume) and a SessionEnd hook into ~/.claude/settings.json —
     every other setting and hook is preserved; a backup is written first; an unreadable settings
     file aborts the install instead of being overwritten (a broken file disables ALL its settings)
  3. merges this repo's never-kill patterns into ~/.claude/claude_reaper.json (the live trading
     processes must never be stopped, even if a Claude session started them); session_end stays
     "report" unless you change it
"""
import argparse
import json
import shutil
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
HOME_CLAUDE = Path.home() / ".claude"
TARGET = HOME_CLAUDE / "hooks" / "claude_reaper.py"
SETTINGS = HOME_CLAUDE / "settings.json"
CONFIG = HOME_CLAUDE / "claude_reaper.json"
# this repo's processes that must survive any session cleanup
NEVER_KILL = ["solana-mev", "portfolio-watcher", "portfolio_watcher", "metis-binary"]
HOOKS = {
    "SessionStart": {"matcher": "startup|resume",
                     "command": 'python3 "$HOME/.claude/hooks/claude_reaper.py" hook-start 2>/dev/null || true',
                     "timeout": 15},
    "SessionEnd": {"matcher": None,
                   "command": 'python3 "$HOME/.claude/hooks/claude_reaper.py" hook-end 2>/dev/null || true',
                   "timeout": 30},
}


def ours(hook: dict) -> bool:
    return "claude_reaper.py" in hook.get("command", "")


def load_settings() -> dict:
    if not SETTINGS.exists():
        return {}
    try:
        data = json.loads(SETTINGS.read_text())
    except ValueError as ex:
        sys.exit(f"{SETTINGS} is not valid JSON ({ex}) — fix it first; nothing was changed")
    if not isinstance(data, dict):
        sys.exit(f"{SETTINGS} is not a JSON object — nothing was changed")
    return data


def merged_settings(data: dict, uninstall: bool) -> dict:
    hooks = data.setdefault("hooks", {})
    for event, spec in HOOKS.items():
        groups = [g for g in hooks.get(event, []) if isinstance(g, dict)]
        for g in groups:  # drop any earlier copy of our entry (update in place)
            g["hooks"] = [h for h in g.get("hooks", []) if not ours(h)]
        groups = [g for g in groups if g.get("hooks")]
        if not uninstall:
            entry = {"hooks": [{"type": "command", "command": spec["command"], "timeout": spec["timeout"]}]}
            if spec["matcher"]:
                entry = {"matcher": spec["matcher"], **entry}
            groups.append(entry)
        if groups:
            hooks[event] = groups
        else:
            hooks.pop(event, None)
    if not hooks:
        data.pop("hooks", None)
    return data


def write_json(path: Path, obj):
    tmp = path.with_suffix(path.suffix + ".tmp")
    tmp.write_text(json.dumps(obj, indent=2) + "\n")
    json.loads(tmp.read_text())  # never leave an unparsable settings file behind
    tmp.replace(path)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--uninstall", action="store_true")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    before = load_settings()
    after = merged_settings(json.loads(json.dumps(before)), args.uninstall)
    cfg = json.loads(CONFIG.read_text()) if CONFIG.exists() else {"never_kill": [], "session_end": "report"}
    if not args.uninstall:
        cfg["never_kill"] = sorted(set(cfg.get("never_kill", [])) | set(NEVER_KILL))
        cfg.setdefault("session_end", "report")

    print(f"{'would ' if args.dry_run else ''}{'uninstall' if args.uninstall else 'install'}:")
    print(f"  script   {TARGET} ({'remove' if args.uninstall else 'copy from ' + str(HERE / 'claude_reaper.py')})")
    print(f"  settings {SETTINGS}: hooks {sorted(after.get('hooks', {}))} (other keys kept: "
          f"{sorted(k for k in after if k != 'hooks') or 'none'})")
    if not args.uninstall:
        print(f"  config   {CONFIG}: never_kill {cfg['never_kill']} · session_end {cfg['session_end']}")
    if args.dry_run:
        return

    if SETTINGS.exists():
        backup = SETTINGS.with_name(f"settings.json.bak.{time.strftime('%Y%m%d_%H%M%S')}")
        shutil.copyfile(SETTINGS, backup)
        print(f"  backup   {backup}")
    HOME_CLAUDE.mkdir(parents=True, exist_ok=True)
    write_json(SETTINGS, after)
    if args.uninstall:
        TARGET.unlink(missing_ok=True)
    else:
        TARGET.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(HERE / "claude_reaper.py", TARGET)
        TARGET.chmod(0o755)
        write_json(CONFIG, cfg)
    print("done. Hooks load when a settings file changes on disk in a watched directory; if a running session "
          "does not pick them up, open /hooks once or restart it.")


if __name__ == "__main__":
    main()
