"""Minutes-only lookbacks: to_minutes.py conversion, the run_sweeps / apply_params rows guards.

A row lookback counts snapshots. Live writes one per minute for every token; the book holds the
token's own prints, and the isolated sim keeps only (3·lb + 50) book rows — so the same row count
spans different time live and in the backtest (JitoSOL 2026-11-03: 8 h live, ~25 h in the sim).
"""
import contextlib
import io
import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import apply_params  # noqa: E402
import common  # noqa: E402
import run_sweeps  # noqa: E402
import to_minutes  # noqa: E402

T0 = 1_780_000_000
ROWS = 3 * 1440  # three days of one-row-per-minute book


def entry(sym, **params):
    return {"symbol": sym, "mint": f"{sym}Mint", "params": {"min_metric": 1.0, "trail_pct": 10, **params}}


def write_book(path: Path, every: dict):
    """Book rows one minute apart; mint `m` prints on every `every[m]`-th row."""
    with path.open("w") as f:
        for i in range(ROWS):
            prices = {m: 1.0 + i * 1e-4 for m, k in every.items() if i % k == 0}
            f.write(json.dumps({"ts": T0 + 60 * i, "prices": prices}) + "\n")


class ToMinutes(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.book = Path(cls.tmp.name) / "book.jsonl"
        write_book(cls.book, {"DENSEMint": 1, "HALFMint": 2, "SPARSEMint": 5, "ZTOKMint": 1,
                              "MINSMint": 1, "WATCHMint": 1, "BAREMint": 2})
        cls.entries = [
            entry("DENSE", lookback_obs=240, entry_max_z_obs=0),
            entry("HALF", lookback_obs=240),
            entry("SPARSE", lookback_obs=240),
            entry("ZTOK", lookback_obs=240, entry_max_z_obs=480, entry_max_z=1),
            entry("MINS", lookback_obs=480, lookback_min=1440),
            {"symbol": "WATCH", "mint": "WATCHMint", "params": {"min_metric": 100000.0, "lookback_obs": 240}},
            entry("NOBOOK", lookback_obs=240),
            entry("BARE"),  # a fresh add/vet token: no lookback of its own ⇒ the global row count
        ]
        cls.measured = to_minutes.measure(cls.book, cls.entries, global_lb=240)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_dense_rows_equal_minutes(self):
        self.assertEqual(self.measured["DENSE"]["lookback_min"], 240)

    def test_half_cadence_doubles_the_span(self):
        self.assertEqual(self.measured["HALF"]["lookback_min"], 480)

    def test_sparse_token_is_capped_by_the_sim_deque(self):
        # 240 prints at one per 5 min would span 1200 min, but the sim keeps 3·240+50 = 770 rows.
        m = self.measured["SPARSE"]
        self.assertAlmostEqual(m["lookback_min"], 770, delta=10)
        self.assertLess(m["points"], 240)
        self.assertGreater(m["capped"], 0.9)

    def test_token_without_its_own_lookback_uses_the_global_rows(self):
        self.assertEqual(self.measured["BARE"]["lb"], 240)
        self.assertEqual(self.measured["BARE"]["lookback_min"], 480)

    def test_only_deployed_row_tokens_in_the_book_are_measured(self):
        self.assertNotIn("MINS", self.measured)    # already on minutes
        self.assertNotIn("WATCH", self.measured)   # watch-only: never swept
        self.assertNotIn("NOBOOK", self.measured)  # no prints in this book

    def test_convert_sets_minutes_and_keeps_rows_as_fallback(self):
        new, changed = to_minutes.convert(self.entries, self.measured)
        by = {e["symbol"]: e["params"] for e in new}
        self.assertEqual(changed, ["DENSE", "HALF", "SPARSE", "ZTOK", "BARE"])
        self.assertEqual(by["HALF"]["lookback_min"], 480)
        self.assertEqual(by["HALF"]["lookback_obs"], 240)
        self.assertEqual(by["MINS"], self.entries[4]["params"])
        self.assertNotIn("lookback_min", by["NOBOOK"])

    def test_convert_moves_an_active_z_gate_to_the_same_minutes(self):
        # The sim's row z counts BOOK rows (one per minute) and live counts one row per minute:
        # z@480 rows is already 480 minutes on both sides.
        new, _ = to_minutes.convert(self.entries, self.measured)
        by = {e["symbol"]: e["params"] for e in new}
        self.assertEqual(by["ZTOK"]["entry_max_z_min"], 480)
        self.assertNotIn("entry_max_z_min", by["DENSE"])  # z-gate off stays off

    def test_convert_does_not_mutate_its_input(self):
        to_minutes.convert(self.entries, self.measured)
        self.assertNotIn("lookback_min", self.entries[0]["params"])


class RowsGuards(unittest.TestCase):
    def test_on_rows(self):
        self.assertTrue(common.on_rows({"lookback_obs": 480}))
        self.assertTrue(common.on_rows({}))
        self.assertFalse(common.on_rows({"lookback_obs": 480, "lookback_min": 1440}))

    def test_rows_targets_skips_watch_only(self):
        entries = [entry("A", lookback_obs=480), entry("B", lookback_min=600),
                   {"symbol": "W", "mint": "W", "params": {"min_metric": 100000.0, "lookback_obs": 480}}]
        self.assertEqual(common.rows_targets(entries), ["A"])

    def test_default_minutes_axis_reaches_dense_tokens(self):
        for v in ("240m", "360m", "720m", "4320m"):
            self.assertIn(v, common.DEFAULT_LOOKBACK_MINS)

    def _run_sweeps(self, entries, *extra):
        with tempfile.TemporaryDirectory() as d:
            tf = Path(d) / "tokens.json"
            tf.write_text(json.dumps(entries))
            argv = ["run_sweeps.py", "--tokens-file", str(tf), "--dry-run", "--book", str(Path(d) / "none.jsonl"),
                    *extra]
            err = io.StringIO()
            with mock.patch.object(sys, "argv", argv), contextlib.redirect_stdout(io.StringIO()), \
                    contextlib.redirect_stderr(err):
                try:
                    run_sweeps.main()
                except SystemExit as e:
                    return str(e.code)
                except OSError:  # past the guard: the dummy book does not exist
                    return None
            return None

    def test_run_sweeps_refuses_a_rows_token(self):
        msg = self._run_sweeps([entry("HYPE", lookback_obs=480), entry("JitoSOL", lookback_min=1440)])
        self.assertIsNotNone(msg)
        self.assertIn("HYPE", msg)
        self.assertNotIn("JitoSOL", msg)
        self.assertIn("to_minutes.py", msg)

    def test_run_sweeps_allow_rows_gets_past_the_guard(self):
        msg = self._run_sweeps([entry("HYPE", lookback_obs=480)], "--allow-rows")
        self.assertFalse(msg and "to_minutes.py" in msg, msg)

    def _apply(self, lb, *extra, z=0, deployed=None, z_win=None):
        with tempfile.TemporaryDirectory() as d:
            run = Path(d)
            deployed = deployed or entry("HYPE", lookback_obs=480, lookback_min=1480)
            (run / "deployed_tokens.json").write_text(json.dumps([deployed]))
            if z_win:
                (run / "manifest.json").write_text(json.dumps({"axes": {"HYPE": {"z_win": z_win}}}))
            picks = run / "picks.json"
            picks.write_text(json.dumps({"HYPE": {"trail": 10, "min": 3.0, "lb": lb, "z": z, "regime": "exempt",
                                                  "fb": 1}}))
            argv = ["apply_params.py", "--run-dir", str(run), "--choices", str(picks), *extra]
            with mock.patch.object(sys, "argv", argv), contextlib.redirect_stdout(io.StringIO()), \
                    contextlib.redirect_stderr(io.StringIO()):
                try:
                    apply_params.main()
                except SystemExit as e:
                    return str(e.code), None
            return None, json.loads((run / "candidate_tokens.json").read_text())

    def test_apply_refuses_a_rows_pick(self):
        msg, _ = self._apply(480)
        self.assertIsNotNone(msg)
        self.assertIn("HYPE", msg)

    def test_apply_accepts_a_minutes_pick(self):
        msg, out = self._apply("1440m")
        self.assertIsNone(msg, msg)
        self.assertEqual(out[0]["params"]["lookback_min"], 1440)

    def test_apply_takes_the_z_window_the_run_swept(self):
        # A pick turning z on names no window; the live row (rows, z off) would fall back to 480 rows,
        # but the run swept z at its own window — that window is what must be written.
        live_row = entry("HYPE", lookback_obs=240, entry_max_z_obs=0)
        msg, out = self._apply("280m", z=1, deployed=live_row, z_win="280m")
        self.assertIsNone(msg, msg)
        self.assertEqual(out[0]["params"]["entry_max_z_min"], 280)

    def test_apply_allow_rows_writes_rows(self):
        msg, out = self._apply(480, "--allow-rows")
        self.assertIsNone(msg, msg)
        self.assertNotIn("lookback_min", out[0]["params"])


class CarryConversion(unittest.TestCase):
    """A run swept from a to_minutes candidate measured the CONVERTED baseline, so a keep verdict means that
    baseline. --apply builds on the live file, which still holds rows: without the carry, every token that
    is not changed would stay on rows (2026-11-03, the 11-token migration)."""

    ROWS = {"HYPE": entry("HYPE", lookback_obs=480, entry_max_z_obs=480, entry_max_z=1),
            "ZEC": entry("ZEC", lookback_obs=240, entry_max_z_obs=0)}
    CONV = {"HYPE": {"lookback_min": 1480, "entry_max_z_min": 480}, "ZEC": {"lookback_min": 280}}

    def _run(self, verdicts, *extra, live_edit=None):
        with tempfile.TemporaryDirectory() as d:
            root, run = Path(d), Path(d) / "run"
            (root / "assets").mkdir()
            (run / "verdicts").mkdir(parents=True)
            live = [json.loads(json.dumps(self.ROWS[s])) for s in ("HYPE", "ZEC")]
            if live_edit:
                live_edit(live)
            (root / "assets" / "momentum_tokens.json").write_text(json.dumps(live))
            measured = [{**self.ROWS[s], "params": {**self.ROWS[s]["params"], **self.CONV[s]}} for s in ("HYPE", "ZEC")]
            (run / "deployed_tokens.json").write_text(json.dumps(measured))
            (run / "manifest.json").write_text(json.dumps({"axes": {"HYPE": {"z_win": "480m"}, "ZEC": {"z_win": "280m"}}}))
            for s, v in verdicts.items():
                (run / "verdicts" / f"{s}.json").write_text(json.dumps({"token": s, **v}))
            out = io.StringIO()
            argv = ["apply_params.py", "--run-dir", str(run), "--from-verdicts", *extra]
            with mock.patch.object(common, "repo_root", return_value=root), mock.patch.object(sys, "argv", argv), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
                try:
                    apply_params.main()
                except SystemExit as e:
                    return str(e.code), None, out.getvalue()
            written = {e["symbol"]: e["params"] for e in json.loads((root / "assets" / "momentum_tokens.json").read_text())}
            return None, written, out.getvalue()

    def test_keep_verdicts_carry_the_measured_minutes_on_apply(self):
        msg, live, _ = self._run({"HYPE": {"verdict": "keep", "pick": None},
                                  "ZEC": {"verdict": "keep", "pick": None}}, "--apply")
        self.assertIsNone(msg, msg)
        self.assertEqual(live["HYPE"]["lookback_min"], 1480)
        self.assertEqual(live["HYPE"]["entry_max_z_min"], 480)
        self.assertEqual(live["ZEC"]["lookback_min"], 280)
        self.assertNotIn("entry_max_z_min", live["ZEC"])
        self.assertEqual(live["ZEC"]["lookback_obs"], 240)  # rows kept as the fallback

    def test_a_change_and_a_carry_in_one_apply(self):
        pick = {"trail": 20, "min": 1.0, "lb": "1480m", "z": 1, "regime": "exempt", "fb": 0.5}
        msg, live, _ = self._run({"HYPE": {"verdict": "change", "pick": pick},
                                  "ZEC": {"verdict": "keep", "pick": None}}, "--apply")
        self.assertIsNone(msg, msg)
        self.assertEqual(live["HYPE"]["trail_pct"], 20)
        self.assertEqual(live["ZEC"]["lookback_min"], 280)

    def test_refuses_to_carry_onto_params_changed_since_the_run(self):
        def edit(live):
            live[1]["params"]["trail_pct"] = 30  # ZEC re-tuned by hand after the sweep
        msg, _, _ = self._run({"HYPE": {"verdict": "keep", "pick": None},
                               "ZEC": {"verdict": "keep", "pick": None}}, "--apply", live_edit=edit)
        self.assertIsNotNone(msg)
        self.assertIn("ZEC", msg)
        self.assertNotIn("HYPE", msg.split("—", 1)[-1].split(".")[0])

    def test_preview_names_the_tokens_apply_will_carry(self):
        msg, _, out = self._run({"HYPE": {"verdict": "keep", "pick": None},
                                 "ZEC": {"verdict": "keep", "pick": None}})
        self.assertIsNone(msg, msg)
        self.assertIn("HYPE", out)
        self.assertIn("ZEC", out)
        self.assertIn("carry", out)


if __name__ == "__main__":
    unittest.main()
