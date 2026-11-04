"""MOMENTUM_LOOKBACK_OBS reaches only tokens without their own lookback; rows-vs-minutes warnings."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import optimize_momentum as om  # noqa: E402


def tok(sym, **params):
    return {"symbol": sym, "mint": sym, "params": {"min_metric": 1.0, **params}}


class LookbackReach(unittest.TestCase):
    def reach(self, entries):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "tokens.json"
            p.write_text(json.dumps(entries))
            return om.lookback_reach(str(p))

    def test_own_lookbacks_and_rows(self):
        inherit, rows = self.reach([
            tok("MIN", lookback_obs=480, lookback_min=1440),
            tok("ROW", lookback_obs=240),
            tok("BARE"),
            {"symbol": "WATCH", "mint": "W", "params": {"min_metric": 100000.0}},
            {"symbol": "NOPARAMS", "mint": "N"},
        ])
        self.assertEqual(inherit, ["BARE"])
        self.assertEqual(rows, ["ROW", "BARE"])

    def test_lookback_held_when_no_token_inherits_it(self):
        cur = {"MOMENTUM_LOOKBACK_OBS": "480", "MOMENTUM_TRAIL_PCT": "10"}
        win = {"MOMENTUM_LOOKBACK_OBS": "720", "MOMENTUM_TRAIL_PCT": "15"}
        changes, held = om.proposed_changes(list(cur), win, cur, inherit=[])
        self.assertEqual(changes, {"MOMENTUM_TRAIL_PCT": "15"})
        self.assertEqual(held, {"MOMENTUM_LOOKBACK_OBS": ("480", "720")})

    def test_lookback_changes_when_a_token_inherits_it(self):
        cur = {"MOMENTUM_LOOKBACK_OBS": "480"}
        changes, held = om.proposed_changes(list(cur), {"MOMENTUM_LOOKBACK_OBS": "720"}, cur, inherit=["BARE"])
        self.assertEqual(changes, {"MOMENTUM_LOOKBACK_OBS": "720"})
        self.assertEqual(held, {})


if __name__ == "__main__":
    unittest.main()
