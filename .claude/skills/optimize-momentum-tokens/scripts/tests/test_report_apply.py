"""per_trail_report.py flags/axes on a synthetic run dir, apply_params.py merge, cost probe math."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import apply_params  # noqa: E402
import common  # noqa: E402
import measure_costs  # noqa: E402
import per_trail_report as ptr  # noqa: E402
import run_sweeps  # noqa: E402

MINT = "TstMint111111111111111111111111111111111111"
HEADER = ("token,cell,pnl_train,pnl_test,trades_train,trades_test,win_test,hold_h_train,hold_h_test,std_test,"
          "worst_test,true_dd_test,token_pnl_test,worst_train,true_dd_train,std_train,best_train,best_test,"
          "open_train,open_test")
DEPLOYED = {"min_metric": 4.0, "trail_pct": 5, "lookback_obs": 240, "entry_max_z_obs": 0, "regime_filter": True,
            "trade_usdc": 250}
BACK = ["f1", "f2", "f3", "f4", "f5"]


def outcome(mn, lb, regime):
    """Designed outcomes: deployed-like (min 4 gated: lb inert), a thin-train edge winner (min 2 lb 240),
    a ✓win maximin winner (min 2 lb 480), a cost-fragile straddling exempt family."""
    if regime == "exempt":
        return dict(train=20, test=5, tr=10, te=5, wins=[-10, 5, -3, 2, 1], f0=3, c3=-5, wtr=-8, best=4, open_te=30)
    if mn == 4.0:
        return dict(train=100, test=50, tr=20, te=10, wins=[10, 12, 8, -2, 15], f0=30, c3=30, wtr=-8, best=10, open_te=0)
    if lb == 240:
        return dict(train=60, test=80, tr=30, te=15, wins=[20, -5, 25, 10, 5], f0=20, c3=10, wtr=-20, best=50, open_te=0)
    return dict(train=90, test=60, tr=25, te=12, wins=[15, 10, 12, 11, 9], f0=25, c3=40, wtr=-6, best=10, open_te=0)


def csv_row(label, o, job):
    if job == "full" or job == "cost3x":
        test = o["test"] if job == "full" else o["c3"]
        train = o["train"]
    elif job == "f0":
        train, test = 0.0, o["f0"]
    else:
        train, test = 0.0, o["wins"][int(job[1]) - 1]
    return (f"TST,{label},{train},{test},{o['tr']},{o['te']},60,100,50,5.0,-5,6,{test},{o['wtr']},9,4.0,"
            f"{o['best']},{o['best'] if job == 'full' else 1},0,{o['open_te'] if job == 'full' else 0}")


class SyntheticRun(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.td = tempfile.TemporaryDirectory()
        base = Path(cls.td.name)
        run = base / "per_token_sweep_2026-01-01"
        (run / "jobs").mkdir(parents=True)
        book = base / "book.jsonl"
        book.write_text('{"ts":1,"prices":{}}\n')
        root = common.repo_root()
        env = common.read_env(root)
        entry = {"symbol": "TST", "mint": MINT, "params": DEPLOYED}
        common.write_json(run / "deployed_tokens.json", [entry])
        state = run_sweeps.current_state(root, root / "assets" / "momentum_tokens.json", book, env)
        common.write_json(run / "manifest.json", dict(state, book=str(book), costs={"TST": 2}, cost_mult=3,
                                                      momentum_env=common.momentum_env(env),
                                                      axes={"TST": {"mins": [2.0, 4.0], "lookbacks": [240, 480]}}))
        common.write_json(run / "coverage.json", {"tokens": {"TST": {"status": "OK", "t0": [], "days": 150,
                                                                     "prints_per_day": 900, "glitches": 0,
                                                                     "sanitizer_removed_pct": 0}}})
        for job in ["full", "f0", "cost3x"] + BACK:
            lines = [HEADER, csv_row("INCUMBENT", outcome(4.0, 240, "gated"), job)]
            for mn in (2.0, 4.0):
                for t in (5.0, 10.0):  # trail 10 is identical to trail 5 everywhere → "≡ trail 5"
                    for lb in (240, 480):
                        for rg in ("gated", "exempt"):
                            lines.append(csv_row(common.cell_label(mn, t, lb, 0.0, rg == "exempt", 1.0),
                                                 outcome(mn, lb, rg), job))
            (run / "jobs" / f"TST.{job}.csv").write_text("\n".join(lines) + "\n")
        cls.run_dir = run
        cls.md = ptr.build_fragment(run, "TST").read_text()
        cls.cand = json.loads((run / "TST_candidates.json").read_text())

    @classmethod
    def tearDownClass(cls):
        cls.td.cleanup()

    def row(self, **knobs):
        for r in self.cand["trails"]["5"]:
            if all(r["knobs"][k] == v for k, v in knobs.items()):
                return r
        self.fail(f"no row with {knobs}: {[r['knobs'] for r in self.cand['trails']['5']]}")

    def test_trust_passes_and_tables_render(self):
        self.assertIn("T1 incumbent PASS", self.md)
        self.assertIn("### Trail 5 % (deployed)", self.md)
        self.assertIn("≡ trail 5", self.md, "an identical rung collapses instead of repeating")

    def test_deployed_family_is_inert_on_lb_and_resolves_to_the_deployed_value(self):
        r = self.row(min=4.0, regime="gated")
        self.assertIn("★", r["flags"])
        self.assertIn("inert:lb", " ".join(r["flags"]))
        self.assertEqual(r["knobs"]["lb"], 240)
        self.assertEqual(r["params"]["trade_usdc"], 250, "non-swept fields survive into the paste-ready params")

    def test_maximin_winner_is_window_robust_and_above_deployed(self):
        r = self.row(min=2.0, lb=480, regime="gated")
        self.assertIn("maximin (time split)", r["axes"])
        self.assertIn("✓win", r["flags"])
        self.assertIn("▲", r["flags"])
        self.assertIn("edge:min", r["flags"])

    def test_max_test_row_is_flagged_thin_train_tail_and_one_trade(self):
        r = self.row(min=2.0, lb=240, regime="gated")
        self.assertIn("max test P&L", r["axes"])
        flags = " ".join(r["flags"])
        for f in ("thin-train", "worse-tail", "1-trade(test"):
            self.assertIn(f, flags)

    def test_exempt_family_is_not_interesting_but_its_flags_are_right(self):
        # It wins no axis, is in no top-3 twice and is Pareto-dominated, so it is (correctly)
        # not listed; its flags are still checked directly.
        self.assertFalse(any(r["knobs"]["regime"] == "exempt" for r in self.cand["trails"]["5"]))
        jobs = ptr.load_jobs(self.run_dir, "TST")
        back = ptr.back_windows(jobs)
        inc = ptr.make_record("INCUMBENT", jobs, back)
        cells = [ptr.make_record(lbl, jobs, back) for lbl in jobs["full"] if lbl != "INCUMBENT"]
        fams = ptr.families([c for c in cells if c["knobs"]["trail"] == 5.0])
        exempt = next(f for f in fams if f["values"]["regime"] == ["exempt"])
        flags = " ".join(ptr.row_flags(exempt, inc, [], {a: False for a in ptr.ALL_AXES},
                                       {"min": {2.0, 4.0}, "lb": {240, 480}}, len(back), False))
        self.assertIn("cost-fragile", flags)
        self.assertIn("straddle", flags)
        self.assertIn("inert:min,lb", flags)
        self.assertNotIn("✓win", flags)

    def test_delta_sum_comes_from_the_csvs(self):
        pick = {"min": 2.0, "trail": 5, "lb": 480, "z": 0, "regime": "gated", "fb": 1.0}
        self.assertAlmostEqual(ptr.delta_sum_for(self.run_dir, "TST", pick), 57 - 43)


class Apply(unittest.TestCase):
    def test_merge_touches_only_the_chosen_token_and_its_six_knobs(self):
        entries = [{"symbol": "A", "mint": "a", "pool": "p", "params": dict(DEPLOYED, regime_exit_obs=480)},
                   {"symbol": "B", "mint": "b", "params": {"min_metric": 9.0}}]
        out, changed = apply_params.merge(entries, {"A": {"min": 2.0, "trail": 10, "lb": 480, "z": 1.5,
                                                          "regime": "exempt", "fb": 0.75}})
        self.assertEqual(changed, ["A"])
        self.assertEqual(out[1], entries[1])
        p = out[0]["params"]
        self.assertEqual((p["regime_exit_obs"], p["trade_usdc"], out[0]["pool"]), (480, 250, "p"))
        self.assertEqual((p["min_metric"], p["trail_pct"], p["lookback_obs"]), (2.0, 10, 480))
        self.assertEqual((p["entry_max_z_obs"], p["entry_max_z"], p["regime_filter"]), (480, 1.5, False))
        self.assertEqual(p["fade_bar"], 1.5)

    def test_unknown_tokens_are_refused(self):
        with self.assertRaises(SystemExit):
            apply_params.merge([{"symbol": "A", "mint": "a", "params": {}}], {"Z": {"min": 1, "trail": 1, "lb": 1}})


class CostProbe(unittest.TestCase):
    def test_round_trip_math_is_per_leg_and_fee_inclusive(self):
        calls = []

        def fake(base, inp, out, amount, retries=4):
            calls.append((inp, out, amount))
            if inp == common.USDC_MINT:
                return {"outAmount": "5000", "routePlan": [{"swapInfo": {"label": "Meteora DLMM"}}]}
            return {"outAmount": str(int(amount * 199.96)), "routePlan": [{"swapInfo": {"label": "Raydium"}}]}

        orig = measure_costs.jupiter_quote
        measure_costs.jupiter_quote = fake
        try:
            bps, labels = measure_costs.round_trip_bps("x", "M", 1.0)  # $1 → 5000 raw → 999.8… back
        finally:
            measure_costs.jupiter_quote = orig
        self.assertEqual(calls[0][2], 1_000_000)
        self.assertAlmostEqual(bps, (1 - 999_800 / 1_000_000) / 2 * 1e4, places=6)
        self.assertEqual(labels, ["Meteora DLMM", "Raydium"])

    def test_policy_contract(self):
        # The operator writes choose_sim_cost_bps; any policy must honour this contract.
        f = measure_costs.choose_sim_cost_bps
        for samples in ([0.18, 0.2, 0.19], [2.57] * 5, [40.0, 55.0, 38.0]):
            got = f(samples)
            self.assertIsInstance(got, int)
            self.assertGreaterEqual(got, 1, "the sim can never be cheaper than 1 bps/leg")
        self.assertGreaterEqual(f([x * 2 for x in (3.0, 3.2, 2.9)]), f([3.0, 3.2, 2.9]), "monotone in cost")


if __name__ == "__main__":
    unittest.main()


class ReviewFixes(unittest.TestCase):
    """Regression tests for the 2026-09-27 code review."""

    def test_book_ab_back_windows_exclude_full_and_f0(self):
        import book_ab
        self.assertEqual(book_ab.back_window_names(["full", "f0", "f1", "f2", "f10", "cost3x"]), ["f1", "f2", "f10"])

    def _fam(self, **kw):
        base = dict(robust=True, complete=True, min_win=1.0, sum_win=5.0, pos=5, lump=0.3, train=10.0, test=5.0,
                    tr_trades=10, te_trades=10, worst_slice=5.0, dd_all=3.0, sqn_ws=1.0, rate_ws=0.1,
                    worst_all=-1.0, cost3x=2.0, std_te=1.0)
        base.update(kw)
        return base

    def test_a_config_that_never_trades_cannot_win_maximin(self):
        idle = self._fam(robust=False, min_win=0.0, sum_win=0.0, pos=0, tr_trades=0, te_trades=0, train=0.0, test=0.0)
        trader = self._fam(min_win=-2.0)
        ranked = ptr.rank_axis([idle, trader], "maximin (time split)", trader, 5)
        self.assertEqual(ranked, [trader])

    def test_resolve_returns_a_real_member(self):
        m1 = {"knobs": {"min": 8.0, "lb": 240, "z": 0.0, "regime": "gated", "fb": 1.0}}
        m2 = {"knobs": {"min": 8.0, "lb": 480, "z": 0.0, "regime": "exempt", "fb": 1.0}}
        fam = {"members": [m1, m2]}
        dep = {"min": 8.0, "lb": 480, "z": 0.0, "regime": "gated", "fb": 1.0}
        got = ptr.resolve(fam, dep)
        self.assertIn(got, [m1["knobs"], m2["knobs"]], "never a combination no member has")


class DeployedTrailInsideAnIdenticalClass(unittest.TestCase):
    """Deployed trail 10 has the same outcomes as trail 5: trail 10 must represent the class."""

    def test_deployed_trail_keeps_its_value(self):
        global DEPLOYED
        saved = DEPLOYED
        try:
            DEPLOYED = dict(saved, trail_pct=10)
            SyntheticRun.setUpClass()
            md, cand = SyntheticRun.md, SyntheticRun.cand
        finally:
            DEPLOYED = saved
            SyntheticRun.tearDownClass()
        self.assertIn("### Trail 5 % — ≡ trail 10", md)
        self.assertIn("### Trail 10 % (deployed) — also ≡ trail 5", md)
        self.assertIn("10", cand["trails"])
        self.assertNotIn("5", cand["trails"])
        star = [r for r in cand["trails"]["10"] if "★" in r["flags"]]
        self.assertEqual(len(star), 1)
        self.assertEqual(star[0]["params"]["trail_pct"], 10, "an inert trail keeps its deployed value")
