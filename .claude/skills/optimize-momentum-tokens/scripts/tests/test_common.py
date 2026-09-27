"""common.py: Rust-identical labels, deployed knobs, axes, params merge."""
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import common  # noqa: E402

JITO = {"entry_max_z_obs": 480, "fade_bar": 2.86875, "lookback_obs": 720, "max_run_pct": 0, "min_metric": 3.825,
        "regime_exit_obs": 0, "regime_filter": True, "trade_usdc": 500, "trail_pct": 2, "entry_max_z": 1}
ZEC = {"entry_max_z_obs": 0, "fade_bar": 2.1938, "lookback_obs": 480, "max_run_pct": 0, "min_metric": 4.3875,
       "regime_filter": False, "trade_usdc": 500, "trail_pct": 5}
KMNO = {"entry_max_z_obs": 0, "lookback_obs": 240, "min_metric": 148.5, "regime_filter": True, "trail_pct": 10.0}


class RustFormatting(unittest.TestCase):
    def test_rust_round_is_half_away_from_zero(self):
        self.assertEqual(common.rust_round(2.5), 3)
        self.assertEqual(common.rust_round(-2.5), -3)
        self.assertEqual(common.rust_round(0.4999), 0)

    def test_min_axis_matches_the_binary(self):
        # Produced by target/release/momentum-sim on 2026-09-27 for JitoSOL (bar 3.825).
        self.assertEqual(common.min_axis(3.825), [1.9125, 2.8688, 3.825, 5.7375, 7.65])

    def test_f64_display(self):
        self.assertEqual([common.fmt_f64(v) for v in (10.0, 2.0, 1.5, 3.6564, 148.5)],
                         ["10", "2", "1.5", "3.6564", "148.5"])

    def test_fmt_frac_mirrors_rust(self):
        self.assertEqual([common.fmt_frac(v) for v in (1.0, 0.75, 0.5, 0.749_993_1, -0.5, -0.00001)],
                         ["1", "0.75", "0.5", "0.75", "-0.5", "0"])

    def test_deployed_label_is_the_real_grid_label(self):
        # The exact label the binary wrote for JitoSOL's deployed cell.
        self.assertEqual(common.deployed_label(JITO, {}), "min=3.825 trail=2 lb=720 z=1@480 regime=gated fb=0.75")

    def test_parse_label_round_trip_and_family_sets(self):
        k = common.parse_label(common.cell_label(4.3875, 5, 480, 0.0, True, 0.5))
        self.assertEqual(k, {"min": 4.3875, "trail": 5.0, "lb": 480, "z": 0.0, "regime": "exempt", "fb": 0.5})
        fam = common.parse_label("min=1 trail={10;15} lb=240 z=1.5@480 regime=gated fb=1")
        self.assertEqual(fam["trail"], [10.0, 15.0])
        self.assertEqual(fam["z"], 1.5)


class DeployedKnobsAndAxes(unittest.TestCase):
    def test_zec_z_off_and_rounded_fade_fraction(self):
        k = common.deployed_knobs(ZEC, {})
        self.assertEqual((k["z"], k["regime_exempt"], common.fmt_frac(k["fb"])), (0.0, True, "0.5"))

    def test_no_fade_bar_means_the_entry_bar(self):
        self.assertEqual(common.deployed_knobs(KMNO, {})["fb"], 1.0)

    def test_axes_always_contain_every_deployed_value(self):
        p = dict(KMNO, trail_pct=7, lookback_obs=900)
        a = common.axes_for(p, {})
        self.assertIn(7.0, a["trails"])
        self.assertIn(900, a["lookbacks"])
        self.assertIn(148.5, a["mins"])
        self.assertEqual(a["n_cells"], len(a["mins"]) * len(a["trails"]) * len(a["lookbacks"]) * len(a["zs"]) * 2
                         * len(a["fracs"]))

    def test_axes_warn_when_the_sim_cannot_replay_the_deployed_z_gate(self):
        a = common.axes_for({"min_metric": 5.0, "trail_pct": 10, "lookback_obs": 240}, {})
        self.assertTrue(any("entry_max_z_obs" in w for w in a["warnings"]))


class ParamsMerge(unittest.TestCase):
    def test_only_the_six_knobs_change_and_order_is_kept(self):
        out = common.params_for_knobs(JITO, {"min": 2.8688, "trail": 5, "lb": 480, "z": 0.0, "regime_exempt": True,
                                             "fb": 1.0})
        self.assertEqual(out["trade_usdc"], 500)
        self.assertEqual(out["regime_exit_obs"], 0)
        self.assertEqual(out["max_run_pct"], 0)
        self.assertNotIn("fade_bar", out, "fb = 1 ⇒ the entry bar ⇒ no fade_bar key")
        self.assertNotIn("entry_max_z", out)
        self.assertEqual(out["entry_max_z_obs"], 0)
        self.assertIs(out["regime_filter"], False)
        self.assertEqual(list(out)[:4], ["entry_max_z_obs", "lookback_obs", "max_run_pct", "min_metric"])

    def test_fade_bar_is_absolute_round4_of_frac_times_min(self):
        out = common.params_for_knobs(ZEC, {"min": 3.2906, "trail": 5, "lb": 480, "z": 1.5, "regime_exempt": False,
                                            "fb": 0.75})
        self.assertEqual(out["fade_bar"], common.round4(0.75 * 3.2906))
        self.assertEqual((out["entry_max_z_obs"], out["entry_max_z"]), (480, 1.5))


class Targets(unittest.TestCase):
    def test_watch_only_and_param_less_entries_are_skipped(self):
        entries = [{"symbol": "A", "mint": "a", "params": {"min_metric": 100000}},
                   {"symbol": "B", "mint": "b"},
                   {"symbol": "C", "mint": "c", "params": {"min_metric": 3.0}}]
        self.assertEqual([e["symbol"] for e in common.deployed_targets(entries)], ["C"])
        self.assertEqual(common.deployed_targets(entries, ["x"]), [])


if __name__ == "__main__":
    unittest.main()
