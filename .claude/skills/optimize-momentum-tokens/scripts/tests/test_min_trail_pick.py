"""The min-trail pick (reading-rules §5): the lowest trail whose rung holds a family clearing gates
1–6 against the deployed config — unit tests on hand-built families, then synthetic run dirs."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
sys.path.insert(0, str(Path(__file__).resolve().parent))
import apply_params  # noqa: E402
import build_report  # noqa: E402
import common  # noqa: E402
import per_trail_report as ptr  # noqa: E402
import run_sweeps  # noqa: E402
import test_report_apply as tra  # noqa: E402  (CSV fixture helpers only)

# Stand-in for the operator's in-rung policy, so these tests pin the rule, not the ranking.
MAXIMIN = lambda f: (-f["min_win"], -f["sum_win"])  # noqa: E731

DEPLOYED = {"min_metric": 4.0, "trail_pct": 10, "lookback_obs": 240, "entry_max_z_obs": 0, "regime_filter": True,
            "trade_usdc": 250}
DEP = dict(train=100, test=50, tr=20, te=10, wins=[10, 12, 8, -2, 15], f0=30, c3=30, wtr=-8, best=10, open_te=0)
LOSER = dict(train=20, test=5, tr=10, te=5, wins=[-10, 5, -3, 2, 1], f0=3, c3=-5, wtr=-8, best=4, open_te=0)
TIGHT_WIN = dict(train=105, test=55, tr=25, te=12, wins=[3, 4, 5, 6, 7], f0=10, c3=20, wtr=-6, best=10, open_te=0)
LOOSE_WIN = dict(train=110, test=70, tr=25, te=12, wins=[15, 10, 12, 11, 9], f0=25, c3=40, wtr=-6, best=10, open_te=0)


def fam(**kw):
    base = dict(robust=True, complete=True, min_win=1.0, sum_win=5.0, pos=5, lump=0.3, train=10.0, test=5.0,
                tr_trades=10, te_trades=10, worst_all=-1.0, cost3x=2.0)
    base.update(kw)
    return base


def build_run(base: Path, deployed: dict, outcome, trails=(5.0, 10.0), mins=(2.0, 3.0, 4.0, 8.0),
              lbs=(240, 480)) -> Path:
    """A one-token run dir whose CSVs come from outcome(min, trail, lb, regime) → tra.csv_row fields.
    mins mirror a real ×0.5/×0.75/×1/×2 axis: the deployed bar 4 is interior, as the grid builds it."""
    run = base / "per_token_sweep_2026-01-01"
    (run / "jobs").mkdir(parents=True)
    book = base / "book.jsonl"
    book.write_text('{"ts":1,"prices":{}}\n')
    root = common.repo_root()
    env = common.read_env(root)
    common.write_json(run / "deployed_tokens.json", [{"symbol": "TST", "mint": tra.MINT, "params": deployed}])
    state = run_sweeps.current_state(root, root / "assets" / "momentum_tokens.json", book, env)
    common.write_json(run / "manifest.json", dict(state, book=str(book), costs={"TST": 2}, cost_mult=3,
                                                  momentum_env=common.momentum_env(env),
                                                  axes={"TST": {"mins": list(mins), "lookbacks": list(lbs)}}))
    common.write_json(run / "coverage.json", {"tokens": {"TST": {"status": "OK", "t0": [], "days": 150,
                                                                 "prints_per_day": 900, "glitches": 0,
                                                                 "sanitizer_removed_pct": 0}}})
    dk = common.deployed_knobs(deployed, env)
    inc = outcome(dk["min"], dk["trail"], dk["lb"], "exempt" if dk["regime_exempt"] else "gated")
    for job in ["full", "f0", "cost3x"] + tra.BACK:
        lines = [tra.HEADER, tra.csv_row("INCUMBENT", inc, job)]
        for mn in mins:
            for t in trails:
                for lb in lbs:
                    for rg in ("gated", "exempt"):
                        lines.append(tra.csv_row(common.cell_label(mn, t, lb, 0.0, rg == "exempt", 1.0),
                                                 outcome(mn, t, lb, rg), job))
        (run / "jobs" / f"TST.{job}.csv").write_text("\n".join(lines) + "\n")
    return run


class Rule(unittest.TestCase):
    def test_gates_are_counted_in_order(self):
        dep = fam(min_win=-2.0, worst_all=-8.0, train=100.0)
        self.assertEqual(ptr.gates_cleared(fam(min_win=5.0, worst_all=-1.0, train=200.0), dep, 5), 6)
        self.assertEqual(ptr.gates_cleared(fam(min_win=5.0, lump=0.9, train=200.0), dep, 5), 1, "✓win is gate 2")
        self.assertEqual(ptr.gates_cleared(fam(min_win=5.0, worst_all=-9.0, train=200.0), dep, 5), 3, "tail is 4")
        self.assertEqual(ptr.gates_cleared(fam(min_win=5.0, train=99.0), dep, 5), 4, "train is 5")
        self.assertEqual(ptr.gates_cleared(fam(min_win=5.0, train=200.0, cost3x=0.0), dep, 5), 5, "@3× > 0 is 6")
        self.assertEqual(ptr.gates_cleared(dep, dep, 5), 6, "the deployed config ties itself on gates 3–5")

    def test_the_lowest_winning_trail_beats_a_looser_rung_with_a_better_worst_window(self):
        dep = fam(min_win=-2.0, train=10.0)
        tight, loose = fam(min_win=1.0), fam(min_win=9.0, sum_win=50.0)
        got = ptr.min_trail_pick([(10.0, [loose]), (5.0, [tight]), (2.0, [fam(robust=False)])], dep, 5,
                                 lambda f: 0, MAXIMIN)
        self.assertEqual((got[0], got[1], got[2]), (5.0, tight, 1))

    def test_exact_ties_go_to_the_family_changing_fewest_knobs(self):
        dep = fam(min_win=-2.0, train=10.0)
        a, b = fam(), fam()
        changed = {id(a): 2, id(b): 0}
        got = ptr.min_trail_pick([(5.0, [a, b])], dep, 5, lambda f: changed[id(f)], MAXIMIN)
        self.assertIs(got[1], b)

    def test_no_winner_anywhere_is_none(self):
        dep = fam(min_win=5.0)
        self.assertIsNone(ptr.min_trail_pick([(5.0, [fam(min_win=1.0)]), (10.0, [fam(lump=0.9)])], dep, 5,
                                             lambda f: 0, MAXIMIN))


class OperatorRankingContract(unittest.TestCase):
    def test_a_dominating_winner_ranks_first(self):
        # The operator writes rung_rank_key; any policy must honour this contract.
        worse = fam(min_win=1.0, sum_win=20.0, lump=0.4, worst_all=-5.0, dd_all=9.0, train=50.0, test=40.0,
                    sqn_ws=1.0, cost3x=30.0)
        better = fam(min_win=4.0, sum_win=60.0, lump=0.3, worst_all=-1.0, dd_all=3.0, train=80.0, test=70.0,
                     sqn_ws=2.0, cost3x=60.0)
        self.assertLess(ptr.rung_rank_key(better), ptr.rung_rank_key(worse))
        self.assertEqual(ptr.rung_rank_key(better), ptr.rung_rank_key(dict(better)), "deterministic")

    def test_best_pnl_policy_on_the_bp_case(self):
        # Operator decision 2026-10-03 ("best pnl"): highest Σ of the back windows, to the cent.
        rows = {"0.5": fam(min_win=-6.9036, sum_win=41.3074, test=55.57),
                "0.75": fam(min_win=-6.9045, sum_win=46.8019, test=61.61),
                "1": fam(min_win=-7.5826, sum_win=45.8764, test=61.89)}
        self.assertEqual(min(rows, key=lambda fb: ptr.rung_rank_key(rows[fb])), "0.75")
        noise = fam(min_win=-1.20, sum_win=10.003, test=5.0), fam(min_win=-1.00, sum_win=10.001, test=5.0)
        self.assertLess(ptr.rung_rank_key(noise[1]), ptr.rung_rank_key(noise[0]),
                        "a sub-cent Σ gap is a tie, decided by the worst window")


class _RunCase(unittest.TestCase):
    deployed = DEPLOYED

    @staticmethod
    def outcome(mn, t, lb, rg):
        raise NotImplementedError

    @classmethod
    def setUpClass(cls):
        cls.td = tempfile.TemporaryDirectory()
        cls.run_dir = build_run(Path(cls.td.name), cls.deployed, cls.outcome)
        cls.md = ptr.build_fragment(cls.run_dir, "TST", rank_key=MAXIMIN).read_text()
        cls.cand = json.loads((cls.run_dir / "TST_candidates.json").read_text())
        cls.mt = cls.cand["min_trail"]

    @classmethod
    def tearDownClass(cls):
        cls.td.cleanup()


class LowerRungWins(_RunCase):
    """Deployed trail 10. Trail 5 holds a gate-clearing min-3 family; trail 10 holds one with a far
    better worst window (+9 vs +3) — the rule still picks trail 5."""

    @staticmethod
    def outcome(mn, t, lb, rg):
        if rg == "exempt":
            return LOSER
        if mn == 4.0:
            return DEP if t == 10.0 else dict(DEP, wtr=-30)  # the tighter trail cuts one trade at −30
        if mn == 3.0:
            return TIGHT_WIN if t == 5.0 else LOSER
        if mn == 2.0:
            return LOOSE_WIN if t == 10.0 else LOSER
        return LOSER

    def test_picks_the_trail_5_winner(self):
        p = self.mt["pick"]
        self.assertEqual(self.mt["verdict"], "change")
        self.assertEqual((p["trail"], p["knobs"]["min"], p["knobs"]["lb"], p["knobs"]["regime"]),
                         (5.0, 3.0, 240, "gated"), "inert lb resolves to the deployed 240")
        self.assertEqual((p["params"]["trail_pct"], p["params"]["min_metric"], p["params"]["trade_usdc"]), (5, 3.0, 250))
        best_loose = max(r["min_win"] for r in self.cand["trails"]["10"] if r["window_ok"])
        self.assertGreater(best_loose, p["min_win"], "the looser rung really had the better worst window")

    def test_ladder_explains_the_rungs_up_to_the_pick(self):
        self.assertEqual([(r["trail"], r["winners"]) for r in self.mt["ladder"]], [(5.0, 1)])
        self.assertIn("### Min-trail pick", self.md)
        self.assertIn("CHANGE", self.md)

    def test_the_pick_row_is_tagged_in_its_rung_table(self):
        tagged = [r for r in self.cand["trails"]["5"] if "min-trail pick" in r["axes"]]
        self.assertEqual(len(tagged), 1)
        self.assertEqual(tagged[0]["knobs"]["min"], 3.0)


class DeployedIsTheLowestWinner(_RunCase):
    deployed = dict(DEPLOYED, trail_pct=5)

    @staticmethod
    def outcome(mn, t, lb, rg):
        if rg == "exempt":
            return LOSER
        if mn == 4.0:
            return DEP
        return LOOSE_WIN if (mn == 2.0 and t == 10.0) else LOSER

    def test_keep(self):
        self.assertEqual(self.mt["verdict"], "keep")
        self.assertEqual(self.mt["pick"]["mark"], "★")
        self.assertEqual(self.mt["pick"]["trail"], 5.0)


class IdenticalRungsResolveDown(_RunCase):
    """Trail 5 ≡ trail 10 (deployed): the tables keep trail 10 as the class representative, but the
    pick takes the lowest rung of the class — a trail-only change with an identical backtest."""

    @staticmethod
    def outcome(mn, t, lb, rg):
        return LOSER if rg == "exempt" else (DEP if mn == 4.0 else LOSER)

    def test_pick_is_dep_at_5(self):
        p = self.mt["pick"]
        self.assertEqual((p["trail"], p["mark"], self.mt["verdict"]), (5.0, "dep@T", "change"))
        self.assertEqual(p["knobs"]["min"], 4.0)
        self.assertEqual(p["params"]["trail_pct"], 5)
        self.assertIn("### Trail 5 % — ≡ trail 10", self.md, "the class is still shown under the deployed trail")


class SharedOpenPosition(_RunCase):
    """The JitoSOL 2026-10-02 case: a pure trail change whose only open position at the slice end is
    the one the deployed config holds too. With Δ ≈ 0 the old straddle test (|open| > $1) flagged it."""

    @staticmethod
    def outcome(mn, t, lb, rg):
        return LOSER if rg == "exempt" else (dict(DEP, open_te=-16.98) if mn == 4.0 else LOSER)

    def test_an_open_position_shared_with_deployed_is_no_straddle(self):
        p = self.mt["pick"]
        self.assertEqual((p["trail"], p["mark"]), (5.0, "dep@T"))
        self.assertFalse([fl for fl in p["flags"] if fl.startswith("straddle(")], p["flags"])
        self.assertEqual(self.mt["verdict"], "change")


class StraddleFlag(unittest.TestCase):
    """row_flags directly. Slice P&L is closed trades only; marking both sides to market moves the
    row's Δ by (open − deployed open). Only a move AGAINST the row means its advantage is overstated."""

    def flags(self, open_te, dep_open_te, test=50.4):
        dep = dict(fam(min_win=-2.0, train=100.0, test=50.0), win_test=60, worst_all=-8.0, best_tr=10.0,
                   best_te=10.0, open_tr=0.0, open_te=dep_open_te)
        f = dict(dep, test=test, open_te=open_te, values={k: [0] for k in ptr.KNOBS})
        return [fl for fl in ptr.row_flags(f, dep, [], {a: False for a in ptr.ALL_AXES},
                                           {"min": set(), "lb": set()}, 5, None) if fl.startswith("straddle(")]

    def test_shared_open_is_not_flagged(self):
        self.assertEqual(self.flags(-16.98, -16.98), [])

    def test_an_open_that_overstates_the_row_is_flagged(self):
        self.assertEqual(len(self.flags(-16.98, 0.0)), 1, "the row hides a loss the deployed does not hold")
        self.assertEqual(len(self.flags(0.0, 16.98)), 1, "the deployed holds a gain its slice P&L leaves out")

    def test_an_open_that_understates_the_row_is_not_flagged(self):
        self.assertEqual(self.flags(0.0, -16.98), [], "the deployed hides a loss (CATE −11.26, 2026-10-02)")
        self.assertEqual(self.flags(16.98, 0.0), [], "the row holds a gain its slice P&L leaves out")

    def test_a_large_delta_absorbs_a_small_open_difference(self):
        self.assertEqual(self.flags(-5.0, 0.0, test=150.0), [], "|Δopen| 5 ≤ 25% of a Δtest of 100")


class NothingClears(_RunCase):
    @staticmethod
    def outcome(mn, t, lb, rg):
        return LOSER

    def test_keep_with_no_pick_and_a_full_ladder(self):
        self.assertEqual(self.mt["verdict"], "keep")
        self.assertIsNone(self.mt["pick"])
        self.assertEqual([r["trail"] for r in self.mt["ladder"]], [5.0, 10.0])
        self.assertTrue(all(r["winners"] == 0 and "✓win" in r["stops_at"] for r in self.mt["ladder"]))


class DeployedNeverTradedTrain(_RunCase):
    @staticmethod
    def outcome(mn, t, lb, rg):
        return dict(DEP, tr=0) if (mn == 4.0 and rg == "gated") else LOSER

    def test_insufficient(self):
        self.assertEqual(self.mt["verdict"], "insufficient")
        self.assertIsNone(self.mt["pick"])


class Enforcement(unittest.TestCase):
    def setUp(self):
        self.td = tempfile.TemporaryDirectory()
        self.run = Path(self.td.name)
        (self.run / "verdicts").mkdir()
        self.rule = {"trail": 5.0, "min": 3.0, "lb": 240, "z": 0.0, "regime": "gated", "fb": 1.0}
        common.write_json(self.run / "TST_candidates.json", {"min_trail": {"verdict": "change",
                                                                           "pick": {"knobs": self.rule}}})

    def tearDown(self):
        self.td.cleanup()

    def verdict(self, pick, verdict="change"):
        common.write_json(self.run / "verdicts" / "TST.json", {"token": "TST", "verdict": verdict, "pick": pick})
        return apply_params.rule_mismatches(self.run, apply_params.choices_from_verdicts(self.run))

    def test_a_change_must_apply_the_rule_pick(self):
        self.assertEqual(self.verdict(dict(self.rule, z=0, trail=5)), [])
        self.assertEqual(len(self.verdict(dict(self.rule, trail=10.0))), 1, "another trail is refused")
        self.assertEqual(self.verdict(dict(self.rule, trail=10.0), verdict="paper-test"), [], "only changes apply")

    def test_a_change_where_the_rule_found_nothing_is_refused(self):
        common.write_json(self.run / "TST_candidates.json", {"min_trail": {"verdict": "keep", "pick": None}})
        self.assertEqual(len(self.verdict(self.rule)), 1)

    def test_runs_before_the_rule_are_not_checked(self):
        common.write_json(self.run / "TST_candidates.json", {"trails": {}})
        self.assertEqual(self.verdict(dict(self.rule, trail=10.0)), [])

    def test_report_flags_a_verdict_that_differs_from_the_rule(self):
        cand = {"min_trail": {"verdict": "change", "pick": {"knobs": self.rule}}}
        self.assertIsNone(build_report.rule_disagreement({"verdict": "change", "pick": dict(self.rule)}, cand))
        self.assertIsNotNone(build_report.rule_disagreement({"verdict": "change", "pick": dict(self.rule, lb=480)}, cand))
        self.assertIsNone(build_report.rule_disagreement({"verdict": "keep", "pick": None},
                                                         {"min_trail": {"verdict": "keep", "pick": None}}))
        self.assertIsNotNone(build_report.rule_disagreement({"verdict": "keep", "pick": None}, cand),
                             "declining a clean change is a disagreement too")
        flagged = {"min_trail": {"verdict": "paper-test", "pick": {"knobs": self.rule}}}
        self.assertIsNone(build_report.rule_disagreement({"verdict": "keep", "pick": None}, flagged),
                          "paper-test → keep is the analyst's gate-7 call")
        self.assertIsNone(build_report.rule_disagreement({"verdict": "change", "pick": dict(self.rule)}, flagged),
                          "paper-test → change (with a reason) is too")


if __name__ == "__main__":
    unittest.main()
