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


class TimeWindowKnobs(unittest.TestCase):
    """`lookback_min` / `entry_max_z_min`: the unit rides in the label value (`lb=3170m`, `z=1@2880m`)."""
    MIN_TOKEN = {"min_metric": 3.6, "lookback_obs": 480, "lookback_min": 3170,
                 "entry_max_z_obs": 0, "entry_max_z_min": 2880, "entry_max_z": 1.0}

    def test_norm_lb(self):
        self.assertEqual(common.norm_lb(480), 480)
        self.assertEqual(common.norm_lb("480"), 480)
        self.assertEqual(common.norm_lb("3170m"), "3170m")
        with self.assertRaises(ValueError):
            common.norm_lb("abc")

    def test_deployed_knobs_minutes(self):
        k = common.deployed_knobs(self.MIN_TOKEN, {})
        self.assertEqual(k["lb"], "3170m")
        self.assertEqual(k["z_obs"], "2880m")
        self.assertEqual(k["z"], 1.0)

    def test_cell_label_and_parse_minutes(self):
        lab = common.cell_label(3.6, 10, "3170m", 1.0, False, 1.0, z_obs="2880m")
        self.assertIn("lb=3170m", lab)
        self.assertIn("z=1@2880m", lab)
        self.assertEqual(common.parse_label(lab)["lb"], "3170m")
        self.assertEqual(common.deployed_label(self.MIN_TOKEN, {}),
                         "min=3.6 trail=5 lb=3170m z=1@2880m regime=gated fb=1")

    def test_params_for_knobs_switches_unit(self):
        rows = common.params_for_knobs({"min_metric": 3.6, "lookback_obs": 480, "lookback_min": 3170},
                                       {"min": 3.6, "trail": 10, "lb": 480, "z": 0, "regime_exempt": False, "fb": 1})
        self.assertEqual(rows["lookback_obs"], 480)
        self.assertNotIn("lookback_min", rows)
        mins = common.params_for_knobs({"min_metric": 3.6, "lookback_obs": 480},
                                       {"min": 3.6, "trail": 10, "lb": "1440m", "z": 0, "regime_exempt": False, "fb": 1})
        self.assertEqual(mins["lookback_min"], 1440)
        self.assertEqual(mins["lookback_obs"], 480)

    def test_params_for_knobs_z_keeps_the_deployed_unit(self):
        # No z_obs in the knobs (a pick names z only): the deployed window's unit is kept.
        p = common.params_for_knobs(self.MIN_TOKEN, {"min": 3.6, "trail": 10, "lb": "3170m", "z": 1.5,
                                                     "regime_exempt": False, "fb": 1})
        self.assertEqual((p["entry_max_z_min"], p["entry_max_z"]), (2880, 1.5))
        off = common.params_for_knobs(self.MIN_TOKEN, {"min": 3.6, "trail": 10, "lb": "3170m", "z": 0,
                                                       "regime_exempt": False, "fb": 1})
        self.assertEqual(off["entry_max_z_obs"], 0)
        self.assertNotIn("entry_max_z_min", off)
        self.assertNotIn("entry_max_z", off)

    def test_axes_for_minutes_defaults(self):
        ax = common.axes_for({"min_metric": 3.6, "lookback_min": 3170}, {})
        self.assertIn("3170m", ax["lookbacks"])
        self.assertTrue(all(isinstance(v, str) for v in ax["lookbacks"]))
        self.assertEqual(ax["lookbacks"], sorted(ax["lookbacks"], key=common.lb_minutes))
        with self.assertRaises(ValueError):
            common.axes_for({"min_metric": 3.6, "lookback_min": 3170}, {}, lookbacks=[480])

    def test_sweep_args_minutes(self):
        import run_sweeps  # noqa: E402
        ax = common.axes_for(self.MIN_TOKEN, {})
        args = [str(a) for a in run_sweeps.sweep_args("HYPE", "h.jsonl", "t.json", 0.7, ax, "o.csv")]
        self.assertIn("--lookback-mins", args)
        self.assertNotIn("--lookbacks", args)
        self.assertEqual(args[args.index("--entry-max-z-min") + 1], "2880")
        self.assertNotIn("--entry-max-z-obs", args)
        rows = [str(a) for a in run_sweeps.sweep_args("ZEC", "h.jsonl", "t.json", 0.7, common.axes_for(ZEC, {}), "o.csv")]
        self.assertIn("--lookbacks", rows)
        self.assertIn("--entry-max-z-obs", rows)

    def test_z_window_in_labels(self):
        self.assertEqual(common.z_window_in_labels(["INCUMBENT", "min=1 trail=5 lb=3170m z=off regime=gated fb=1",
                                                    "min=1 trail=5 lb=3170m z=1@2880m regime=gated fb=1"]), "2880m")
        self.assertEqual(common.z_window_in_labels(["min=1 trail=5 lb=480 z=off regime=gated fb=1"]), common.Z_OBS)

    def test_minutes_token_without_z_window_sweeps_z_on_minutes(self):
        # A token on a minutes lookback with its z-gate off (BP in the migration table): turning z on
        # must not put a 480-ROW z window back on a migrated token — the window follows the lookback.
        bp = {"min_metric": 27.8, "lookback_obs": 720, "lookback_min": 1360, "entry_max_z_obs": 0}
        self.assertEqual(common.deployed_z_window(bp), "1360m")
        p = common.params_for_knobs(bp, {"min": 27.8, "trail": 15, "lb": "1360m", "z": 1.0,
                                         "regime_exempt": False, "fb": 1})
        self.assertEqual(p["entry_max_z_min"], 1360)
        self.assertEqual(p["entry_max_z_obs"], 0, "the row window is left as it was (off)")
        self.assertEqual(common.deployed_z_window(ZEC), common.Z_OBS, "rows tokens unchanged")

    def test_apply_params_to_knobs_accepts_minutes(self):
        import apply_params  # noqa: E402
        k = apply_params.to_knobs({"min": 3.6, "trail": 10, "lb": "3170m", "z": 1, "regime": "gated", "fb": 1})
        self.assertEqual(k["lb"], "3170m")
        self.assertIsNone(k["z_obs"], "no z_obs in the pick ⇒ params_for_knobs keeps the deployed unit")


if __name__ == "__main__":
    unittest.main()
