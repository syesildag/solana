"""claude_reaper.py: tag parsing (macOS ps -E and Linux /proc), group classification and safety on
synthetic tables, and real tagged process groups for status / stop / hooks. Real-process tests use a
unique fake session id and scope every stop to it, so they can never touch real work."""
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import claude_reaper as cr  # noqa: E402


def proc(pid, ppid, pgid, cmd, etime="01:00"):
    return {"pid": pid, "ppid": ppid, "pgid": pgid, "etime": etime, "cmd": cmd}


def tag(session, claude_pid, **extra):
    t = {"CLAUDE_CODE_SESSION_ID": session, "CLAUDE_PID": str(claude_pid)}
    t.update(extra)
    return t


SHELL = "/bin/zsh -c source /Users/u/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true && eval 'x'"


class Parsing(unittest.TestCase):
    def test_macos_ps_e_line(self):
        t = cr.parse_env_text("node scripts/x.js --days 150 PATH=/bin CLAUDE_CODE_SESSION_ID=abc-1 "
                              "CLAUDE_PID=88771 CLAUDE_TASK_LABEL=omt:grid HOME=/Users/u")
        self.assertEqual(t, {"CLAUDE_CODE_SESSION_ID": "abc-1", "CLAUDE_PID": "88771", "CLAUDE_TASK_LABEL": "omt:grid"})

    def test_linux_proc_environ(self):
        raw = b"PATH=/usr/bin\0CLAUDE_CODE_SESSION_ID=abc-1\0CLAUDE_PID=4242\0CLAUDE_REAPER_TIE=1\0"
        self.assertEqual(cr.parse_proc_environ(raw), {"CLAUDE_CODE_SESSION_ID": "abc-1", "CLAUDE_PID": "4242",
                                                      "CLAUDE_REAPER_TIE": "1"})

    def test_etime(self):
        self.assertEqual(cr.etime_seconds("06-16:06:42"), 6 * 86400 + 16 * 3600 + 6 * 60 + 42)
        self.assertEqual(cr.etime_seconds("02:05"), 125)

    def test_claude_detection_macos_and_linux(self):
        for cmd in ("/x/anthropic.claude-code-2.1.283-darwin-arm64/resources/native-binary/claude --output-format x",
                    "node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js --verbose", "/home/u/.local/bin/claude"):
            self.assertTrue(cr.is_claude(proc(1, 0, 1, cmd)), cmd)
        self.assertFalse(cr.is_claude(proc(1, 0, 1, SHELL)), "~/.claude/… paths are not Claude Code")


class Classification(unittest.TestCase):
    PROCS = {
        100: proc(100, 50, 50, "/x/native-binary/claude --output-format stream-json"),   # Claude Code, IDE group
        50: proc(50, 1, 50, "/Applications/Visual Studio Code.app/Contents/MacOS/Code"),
        101: proc(101, 100, 50, "node desktop-commander"),                                # MCP server, IDE group
        200: proc(200, 100, 200, SHELL),                                                  # live Bash-tool task
        201: proc(201, 200, 200, "node scripts/backfill_history.js"),
        300: proc(300, 1, 300, "target/release/momentum-sim per-token-sweep"),            # owner gone
        400: proc(400, 1, 400, SHELL),                                                    # untagged shell, parent gone
        500: proc(500, 1, 500, "python3 unrelated.py"),                                   # not Claude's
    }
    TAGS = {
        101: tag("S", 100), 201: tag("S", 100, CLAUDE_TASK_LABEL="omt:fetch"),
        300: tag("OLD", 999, CLAUDE_REAPER_TIE="1"),
    }

    def groups(self):
        return {g["pgid"]: g for g in cr.work_groups(self.PROCS, self.TAGS)}

    def test_live_task_is_running_and_labelled(self):
        g = self.groups()[200]
        self.assertEqual((g["state"], g["sessions"], g["label"]), ("RUNNING", ["S"], "omt:fetch"))
        self.assertIn("backfill_history", g["command"], "labelled by its worker, not the shell")

    def test_dead_owner_is_orphan(self):
        g = self.groups()[300]
        self.assertEqual((g["state"], g["tied"]), ("ORPHAN", True))

    def test_untagged_shell_is_attributed_through_its_parents(self):
        self.assertEqual(self.groups()[400]["state"], "ORPHAN")

    def test_system_binary_below_a_bash_tool_shell_is_owned(self):
        # the 2026-09-20 orphan: a Bash-tool zsh (parent gone) looping `sleep 3` — neither shows a tag
        procs = {600: proc(600, 1, 600, SHELL), 601: proc(601, 600, 600, "sleep 3")}
        gs = {g["pgid"]: g for g in cr.work_groups(procs, {})}
        self.assertEqual(gs[600]["state"], "ORPHAN")

    def test_group_led_by_an_untagged_non_shell_is_foreign(self):
        procs = {700: proc(700, 1, 700, "/bin/sh -c x"), 701: proc(701, 700, 700, "sleep 3")}
        self.assertEqual(cr.work_groups(procs, {}), [])

    def test_ide_group_with_a_tagged_mcp_server_is_never_work(self):
        gs = self.groups()
        self.assertNotIn(50, gs, "tagged ≠ ours: the IDE's group holds untagged processes and Claude Code")
        self.assertNotIn(500, gs)


class RealGroups(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.cfg = Path(self.tmp.name) / "claude_reaper.json"
        self.cfg.write_text(json.dumps({"never_kill": ["never-kill-me"], "session_end": "tied"}))
        os.environ["CLAUDE_REAPER_CONFIG"] = str(self.cfg)
        self.session = f"test-{uuid.uuid4()}"
        self.children = []

    def tearDown(self):
        for c in self.children:
            try:
                os.killpg(c.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
            c.wait()
        os.environ.pop("CLAUDE_REAPER_CONFIG", None)
        self.tmp.cleanup()

    # Python, not /bin/sh + sleep: macOS SIP hides the environment of Apple system binaries, so only
    # non-system binaries show their tags (Linux shows every own process's /proc/<pid>/environ).
    CHILD = "import subprocess, sys, time; subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)']); time.sleep(30)"

    def spawn(self, tied=False, marker="", session=None):
        # CLAUDE_PID = this test process: alive but not Claude Code ⇒ the group is an orphan
        env = dict(os.environ, CLAUDE_CODE_SESSION_ID=session or self.session, CLAUDE_PID=str(os.getpid()),
                   CLAUDE_TASK_LABEL="reaper-test", CLAUDE_REAPER_TIE="1" if tied else "0")
        c = subprocess.Popen([sys.executable, "-c", self.CHILD + marker], env=env, start_new_session=True,
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.children.append(c)
        deadline = time.time() + 5
        while len(cr.live_members(c.pid)) < 2:
            self.assertLess(time.time(), deadline, "dummy group never formed")
            time.sleep(0.05)
        return c

    def group(self, pgid):
        return next((g for g in cr.snapshot() if g["pgid"] == pgid), None)

    def test_status_sees_a_tagged_group_as_an_orphan(self):
        c = self.spawn()
        g = self.group(c.pid)
        self.assertIsNotNone(g)
        self.assertEqual((g["state"], g["label"], g["sessions"]), ("ORPHAN", "reaper-test", [self.session]))

    def test_stop_scoped_to_a_session_kills_the_whole_group(self):
        c = self.spawn()
        out = cr.stop(orphans=True, session=self.session, grace=5)
        self.assertTrue(any("stopped" in o or "killed" in o for o in out), out)
        c.wait(timeout=5)
        self.assertEqual(cr.live_members(c.pid), [])

    def test_never_kill_is_refused(self):
        c = self.spawn(marker="  # never-kill-me")
        out = cr.stop(targets=[c.pid], grace=1)
        self.assertIn("REFUSED", out[0])
        self.assertTrue(cr.live_members(c.pid))

    def test_session_end_tied_policy_stops_only_tied_groups_of_that_session(self):
        tied, untied = self.spawn(tied=True), self.spawn(tied=False)
        other = self.spawn(tied=True, session=f"test-{uuid.uuid4()}")
        cr.hook_end({"session_id": self.session, "reason": "prompt_input_exit"})
        tied.wait(timeout=6)
        self.assertEqual(cr.live_members(tied.pid), [])
        self.assertTrue(cr.live_members(untied.pid), "not tied ⇒ outlives the session")
        self.assertTrue(cr.live_members(other.pid), "another session's work is never touched")
        self.assertIn(self.session, cr.log_path().read_text())

    def test_session_end_without_an_id_stops_nothing(self):
        c = self.spawn(tied=True)
        self.assertEqual(cr.hook_end({}), [])
        self.assertTrue(cr.live_members(c.pid))

    def test_hook_start_reports_the_orphan(self):
        c = self.spawn()
        out = json.loads(cr.hook_start({"session_id": self.session}))
        ctx = out["hookSpecificOutput"]["additionalContext"]
        self.assertIn(f"group {c.pid}", ctx)
        self.assertIn("this session, before a resume", ctx)


if __name__ == "__main__":
    unittest.main()
