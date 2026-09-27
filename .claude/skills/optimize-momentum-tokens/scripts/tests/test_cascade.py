"""common.install_cascade / run_child: stopping a script stops what it started (real processes)."""
import os
import signal
import subprocess
import sys
import textwrap
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parents[1]

JOB = textwrap.dedent('''
    import sys, threading, time
    sys.path.insert(0, {scripts!r})
    import common
    common.install_cascade("omt-test")
    threading.Thread(target=lambda: common.run_child(["sleep", "30"]), daemon=True).start()
    time.sleep(30)
''')


class Cascade(unittest.TestCase):
    def test_sigterm_stops_the_children_and_labels_them(self):
        job = subprocess.Popen([sys.executable, "-c", JOB.format(scripts=str(HERE))], start_new_session=True)
        try:
            deadline, kids = time.time() + 5, []
            while not kids:
                self.assertLess(time.time(), deadline, "the child never started")
                out = subprocess.run(["ps", "-A", "-o", "pid=,ppid=,args="], capture_output=True, text=True).stdout
                kids = [int(l.split()[0]) for l in out.splitlines() if l.split()[1:2] == [str(job.pid)] and "sleep" in l]
                time.sleep(0.05)
            os.kill(job.pid, signal.SIGTERM)
            self.assertEqual(job.wait(timeout=10), 128 + signal.SIGTERM)
            time.sleep(0.2)
            for k in kids:
                with self.assertRaises(ProcessLookupError, msg="the child must die with its parent"):
                    os.kill(k, 0)
        finally:
            try:
                os.killpg(job.pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass

    def test_no_child_starts_after_a_stop_signal(self):
        sys.path.insert(0, str(HERE))
        import common
        common.STOPPING.set()
        try:
            with self.assertRaises(common.Stopping):
                common.run_child(["true"])
        finally:
            common.STOPPING.clear()


if __name__ == "__main__":
    unittest.main()
