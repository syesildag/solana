"""run_sweeps.py windows/extraction and ensure_history.py merge/statistics."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import common  # noqa: E402
import ensure_history as eh  # noqa: E402
import run_sweeps as rs  # noqa: E402

MINT = "Mint1111111111111111111111111111111111111111"


def write_jsonl(path: Path, rows):
    path.write_text("".join(json.dumps(r) + "\n" for r in rows))


class Windows(unittest.TestCase):
    def test_windows_tile_the_back_half_and_split_at_train_frac(self):
        w = rs.windows_for(0, 1000, 0.7, 5)
        self.assertEqual(w["split_ts"], 700)
        self.assertEqual(w["f0"], (0, 500))
        self.assertEqual([w[f"f{i}"][0] for i in range(1, 6)], [500, 600, 700, 800, 900])
        self.assertEqual(w["f5"][1], 1001, "the last window includes the last row")

    def test_exact_frac_lands_the_split_on_the_index(self):
        for n in (201, 1000, 216_000):
            for i in (1, 7, n // 2, n - 1):
                self.assertEqual(int(n * rs.exact_frac(n, i)), i)
        with self.assertRaises(ValueError):
            rs.exact_frac(10, 0)

    def test_window_files_start_with_one_prefix_row_then_the_window(self):
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            hist = td / "T.jsonl"
            write_jsonl(hist, [{"ts": 60 * i, "prices": {MINT: 1.0 + i / 1e4}} for i in range(3000)])
            wins = rs.windows_for(0, 60 * 2999, 0.7, 5)
            jobs, prints = rs.write_window_files(hist, "T", wins, 5, td, MINT)
            self.assertTrue(all(prints[f"f{i}"] > 0 for i in range(1, 6)), prints)
            for name in ("f1", "f2", "f3", "f4", "f5"):
                path, frac = jobs[name]
                rows = [json.loads(l) for l in path.read_text().splitlines()]
                a, b = wins[name]
                split = int(len(rows) * frac)
                self.assertEqual(split, 1)
                self.assertLess(rows[0]["ts"], a, "row 0 is the prefix, just before the window")
                self.assertTrue(all(a <= r["ts"] < b for r in rows[1:]))
            self.assertIn("full", jobs)
            self.assertEqual(jobs["cost3x"], jobs["full"])

    def test_extract_keeps_the_book_grid_with_only_the_mint_and_wsol(self):
        with tempfile.TemporaryDirectory() as td:
            td = Path(td)
            book = td / "book.jsonl"
            write_jsonl(book, [{"ts": 1, "prices": {MINT: 2.0, common.WSOL: 150.0, "Other": 9.0}},
                               {"ts": 2, "prices": {"Other": 9.0}},
                               {"ts": 3, "prices": {common.WSOL: 151.0}}])
            counts = rs.extract_per_token(book, [{"symbol": "T", "mint": MINT}], td / "h", {"T": 0})
            rows = [json.loads(l) for l in (td / "h" / "T.jsonl").read_text().splitlines()]
            self.assertEqual(counts["T"], 3, "every book row is kept: the snapshot grid must match the book")
            self.assertEqual(rows[0]["prices"], {MINT: 2.0, common.WSOL: 150.0})
            self.assertEqual(rows[1]["prices"], {}, "a row where only another token printed stays, empty")


class HistoryMerge(unittest.TestCase):
    def test_union_first_file_wins_and_plain_sol_is_stripped(self):
        with tempfile.TemporaryDirectory() as td:
            a, b = Path(td) / "a.jsonl", Path(td) / "b.jsonl"
            write_jsonl(a, [{"ts": 60, "prices": {"SOL": 150.0, common.WSOL: 150.0}}])
            write_jsonl(b, [{"ts": 60, "prices": {common.WSOL: 999.0, MINT: 2.0}}, {"ts": 120, "prices": {MINT: 0}}])
            rows = eh.union_rows([a, b])
            self.assertEqual(rows, [{"ts": 60, "prices": {common.WSOL: 150.0, MINT: 2.0}}])

    def test_glitch_rule_counts_spike_and_revert_only(self):
        self.assertEqual(eh.count_glitches([100, 100, 110, 100.5, 100]), 1, ">4% jump back within 1.5% in ≤15 obs")
        self.assertEqual(eh.count_glitches([100, 100, 110, 110, 111]), 0, "a real move does not revert")
        self.assertEqual(eh.count_glitches([100, 103, 100]), 0, "3% is under the 4% bar")

    def test_clean_pegged_touches_lst_class_only(self):
        lst = [100 + (i % 3) * 0.01 for i in range(60)]
        lst[30] = 110.0  # one glitch print on an LST-class series
        meme = [100 * (1.05 if i % 2 else 1.0) for i in range(60)]  # 5% swings: real volatility
        rows = [{"ts": i, "prices": {"LST": lst[i], "MEME": meme[i]}} for i in range(60)]
        removed = eh.clean_pegged(rows, ["LST", "MEME"])
        self.assertEqual(removed, {"LST": 1})
        self.assertNotIn("LST", rows[30]["prices"])
        self.assertTrue(all("MEME" in r["prices"] for r in rows), "a meme's swings are never cleaned")

    def test_cadence_shift(self):
        day = 86_400
        even = [{"ts": t} for t in range(0, 28 * day, 600)]
        self.assertLess(eh.cadence_shift(even), 1.2)
        dense = even + [{"ts": t} for t in range(14 * day, 21 * day, 300)]
        self.assertGreaterEqual(eh.cadence_shift(sorted(dense, key=lambda r: r["ts"])), 2.0)

    def test_t0_statuses(self):
        rep = {"first": 0, "last": 150 * 86_400, "truncation_signature": False, "has_plain_sol_key": False,
               "cadence_shift": 1.0, "age_days": 1.0}
        sol = {"prints": 1000, "first": 0, "last": rep["last"]}
        base = {"prints": 5000, "days": 150.0, "glitches": 0, "sanitizer_removed_pct": 0.0, "prints_per_day": 900.0,
                "starts_late_days": 0.0}
        self.assertEqual(eh.t0_verdict(base, sol, rep, 150, 7)[0], "OK")
        self.assertEqual(eh.t0_verdict(dict(base, days=80.0), sol, rep, 150, 7)[0], "SHORT")
        self.assertEqual(eh.t0_verdict(dict(base, days=41.0), sol, rep, 150, 7)[0], "INSUFFICIENT")
        self.assertEqual(eh.t0_verdict(base, sol, dict(rep, has_plain_sol_key=True), 150, 7)[0], "FAIL")
        self.assertEqual(eh.t0_verdict(base, sol, dict(rep, truncation_signature=True), 150, 7)[0], "FAIL")


class SelectRunnable(unittest.TestCase):
    def cov(self):
        return {"first": 0, "last": 1000, "tokens": {
            "OLD": {"status": "OK", "first": 0, "days": 150.0, "t0": []},
            "MID": {"status": "SHORT", "first": 400, "days": 80.0, "t0": []},
            "NEW": {"status": "INSUFFICIENT", "first": 700, "days": 53.7,
                    "t0": [["WARN", "sanitizer removed 8% of prints"], ["INFO", "53.7 d < 60 d: no tuning, keep deployed"]]},
            "BAD": {"status": "FAIL", "first": 0, "days": 150.0, "t0": [["FAIL", "plain SOL key"]]}}}

    def targets(self):
        return [{"symbol": s} for s in ("OLD", "MID", "NEW", "BAD")]

    def test_insufficient_is_skipped_by_default(self):
        runnable, starts, kwin, skipped = rs.select_runnable(self.cov(), self.targets(), 5)
        self.assertEqual([e["symbol"] for e in runnable], ["OLD", "MID"])
        self.assertEqual((starts, kwin), ({"OLD": 0, "MID": 400}, {"OLD": 5, "MID": 3}))
        self.assertEqual(skipped, {"NEW": "INSUFFICIENT", "BAD": "FAIL"})

    def test_override_sweeps_insufficient_as_short_and_says_so_in_t0(self):
        cov = self.cov()
        runnable, starts, kwin, skipped = rs.select_runnable(cov, self.targets(), 5, {"NEW", "BAD"})
        self.assertEqual([e["symbol"] for e in runnable], ["OLD", "MID", "NEW"])
        self.assertEqual((starts["NEW"], kwin["NEW"]), (700, 3), "own span, K=3 — the SHORT policy")
        self.assertEqual(skipped, {"BAD": "FAIL"}, "the override never revives a FAIL")
        t0 = cov["tokens"]["NEW"]["t0"]
        self.assertFalse(any("no tuning" in m for _, m in t0), "the contradicting INFO is replaced")
        self.assertTrue(any(lvl == "WARN" and "--allow-insufficient" in m for lvl, m in t0))
        self.assertIn(["WARN", "sanitizer removed 8% of prints"], t0, "existing warnings are kept")


if __name__ == "__main__":
    unittest.main()
