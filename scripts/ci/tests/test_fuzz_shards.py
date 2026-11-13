"""Unit tests for scripts/ci/fuzz_shards.py (the fuzz leg's shards and its merged result).

Run:  python3 -m unittest scripts/ci/tests/test_fuzz_shards.py
"""
import contextlib
import io
import json
import os
import random
import re
import sys
import tempfile
import unittest

if sys.version_info < (3, 11):
    raise unittest.SkipTest("fuzz_shards needs Python >= 3.11 (tomllib)")

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(os.path.dirname(HERE)))
sys.path.insert(0, os.path.dirname(HERE))
import fuzz_shards as fs  # noqa: E402


def manifest(targets):
    return "schema_version = 0\n" + "".join(
        '\n[[target]]\ncrate = "%s"\nname = "%s"\n' % t for t in targets)


def make_root(targets, crates=None, text=None):
    """A tree with a manifest for `targets` and a fuzz workspace for each of `crates`
    (default: every crate the targets name)."""
    root = tempfile.mkdtemp()
    os.makedirs(os.path.join(root, "tests/coverage"))
    with open(os.path.join(root, fs.MANIFEST), "w") as f:
        f.write(manifest(targets) if text is None else text)
    for c in sorted({c for c, _ in targets} if crates is None else crates):
        os.makedirs(os.path.join(root, "crates", c, "fuzz"))
        open(os.path.join(root, "crates", c, "fuzz", "Cargo.toml"), "w").close()
    return root


MIXED = ([("tst-core", "t%02d" % i) for i in range(11)] + [("tst-rtp", "r%d" % i) for i in range(4)]
         + [("tst-srt", "handshake")] + [("tst-webrtc", "w%d" % i) for i in range(2)])


class LoadTests(unittest.TestCase):
    def test_targets_come_back_sorted_by_crate_then_name_whatever_the_file_order(self):
        shuffled = list(MIXED)
        random.Random(7).shuffle(shuffled)
        self.assertEqual(fs.load_targets(make_root(shuffled)), sorted(MIXED))

    def test_a_missing_manifest_is_an_error(self):
        root = tempfile.mkdtemp()
        with self.assertRaisesRegex(fs.ShardError, "cannot read"):
            fs.load_targets(root)

    def test_an_empty_manifest_is_an_error(self):
        for text in ("", "schema_version = 0\n", "target = []\n"):
            with self.subTest(text=text), self.assertRaisesRegex(fs.ShardError, "declares no fuzz target"):
                fs.load_targets(make_root([], text=text))

    def test_a_manifest_that_is_not_toml_is_an_error(self):
        with self.assertRaisesRegex(fs.ShardError, "cannot read"):
            fs.load_targets(make_root([], text="[[target]\n"))

    def test_a_duplicate_target_is_an_error(self):
        root = make_root([("tst-core", "a"), ("tst-core", "b"), ("tst-core", "a")])
        with self.assertRaisesRegex(fs.ShardError, "tst-core/a is declared twice"):
            fs.load_targets(root)

    def test_the_same_name_in_two_crates_is_two_targets(self):
        root = make_root([("tst-core", "a"), ("tst-rtp", "a")])
        self.assertEqual(fs.load_targets(root), [("tst-core", "a"), ("tst-rtp", "a")])

    def test_a_crate_without_a_fuzz_workspace_is_an_error(self):
        root = make_root([("tst-core", "a"), ("tst-gone", "b")], crates=["tst-core"])
        with self.assertRaisesRegex(fs.ShardError, "crates/tst-gone/fuzz"):
            fs.load_targets(root)

    def test_a_row_without_a_crate_or_a_name_is_an_error(self):
        for row in ('crate = "tst-core"\n', 'name = "a"\n', 'crate = "tst-core"\nname = 3\n'):
            with self.subTest(row=row), self.assertRaisesRegex(fs.ShardError, "needs a crate and a name"):
                fs.load_targets(make_root([], crates=["tst-core"], text="[[target]]\n" + row))

    def test_a_name_that_is_not_a_plain_word_is_an_error(self):
        for crate, name in (("tst-core", "a b"), ("tst-core", "../x"), ("tst core", "a"), ("tst-core", "")):
            text = '[[target]]\ncrate = "%s"\nname = "%s"\n' % (crate, name)
            with self.subTest(crate=crate, name=name), self.assertRaises(fs.ShardError):
                fs.load_targets(make_root([], crates=["tst-core"], text=text))


class DealTests(unittest.TestCase):
    def assert_exact_cover(self, targets, shards):
        dealt = fs.deal(targets, shards)
        flat = [t for s in dealt for t in s]
        # Every target in exactly one shard, in the sorted order.
        self.assertEqual(flat, targets)
        self.assertEqual(len(flat), len(set(flat)))
        self.assertEqual(len(dealt), min(shards, len(targets)))
        self.assertTrue(all(dealt), "an empty shard was dealt")
        sizes = [len(s) for s in dealt]
        self.assertLessEqual(max(sizes) - min(sizes), 1)
        return dealt

    def test_the_real_manifest_is_covered_exactly_for_every_shard_count(self):
        targets = fs.load_targets(REPO)
        self.assertGreater(len(targets), 0)
        for shards in range(1, len(targets) + 4):
            with self.subTest(shards=shards):
                self.assert_exact_cover(targets, shards)

    def test_a_crate_stays_in_consecutive_shards(self):
        dealt = self.assert_exact_cover(sorted(MIXED), 4)
        for crate in {c for c, _ in MIXED}:
            held = [i for i, s in enumerate(dealt) if any(c == crate for c, _ in s)]
            self.assertEqual(held, list(range(held[0], held[-1] + 1)))

    def test_one_target_is_one_shard(self):
        self.assertEqual(fs.deal([("tst-core", "a")], 8), [[("tst-core", "a")]])

    def test_fewer_targets_than_shards_gives_one_target_each_and_no_empty_shard(self):
        targets = sorted(MIXED)[:3]
        self.assertEqual(self.assert_exact_cover(targets, 8), [[t] for t in targets])

    def test_no_targets_or_no_shards_is_an_error(self):
        with self.assertRaisesRegex(fs.ShardError, "no fuzz target"):
            fs.deal([], 8)
        for shards in (0, -1):
            with self.subTest(shards=shards), self.assertRaisesRegex(fs.ShardError, "shard count"):
                fs.deal(sorted(MIXED), shards)


class PlanTests(unittest.TestCase):
    def test_the_matrix_names_each_shard_its_count_and_its_crates(self):
        p = fs.plan(make_root(MIXED), shards=4, seconds=60, max_minutes=90)
        self.assertEqual([e["shard"] for e in p["include"]], [1, 2, 3, 4])
        self.assertEqual({e["of"] for e in p["include"]}, {4})
        self.assertEqual(sum(e["targets"] for e in p["include"]), len(MIXED))
        self.assertEqual(p["include"][0]["crates"], "tst-core")
        self.assertEqual(p["include"][3]["crates"], "tst-rtp, tst-srt, tst-webrtc")

    def test_of_is_the_number_of_shards_dealt_not_the_number_asked_for(self):
        p = fs.plan(make_root(MIXED[:2]), shards=8, seconds=60, max_minutes=90)
        self.assertEqual([(e["shard"], e["of"]) for e in p["include"]], [(1, 2), (2, 2)])

    def test_the_estimate_is_the_fuzzing_time_plus_the_setup_and_one_build_per_crate(self):
        shard = [("tst-core", "a"), ("tst-core", "b"), ("tst-rtp", "c")]
        self.assertEqual(fs.estimate_minutes(shard, 600),
                         30 + fs.SETUP_MINUTES + 2 * fs.BUILD_MINUTES_PER_CRATE)

    def test_a_shard_estimated_over_the_limit_is_refused_with_the_remedy(self):
        root = make_root(MIXED)
        with self.assertRaisesRegex(fs.ShardError, r"shard 1 of 2 .* about \d+ min.* limit of 90 .*more shards"):
            fs.plan(root, shards=2, seconds=600, max_minutes=90)
        # The same layout at a tenth of the time fits.
        fs.plan(root, shards=2, seconds=60, max_minutes=90)

    def test_seconds_below_one_is_an_error(self):
        for seconds in (0, -5):
            with self.subTest(seconds=seconds), self.assertRaisesRegex(fs.ShardError, "seconds per target"):
                fs.plan(make_root(MIXED), shards=4, seconds=seconds, max_minutes=90)

    def test_the_real_manifest_fits_the_workflow_s_own_settings_at_the_release_budget(self):
        with open(os.path.join(REPO, ".github/workflows/fuzz.yml")) as f:
            text = f.read()
        shards = int(re.search(r'^  FUZZ_SHARDS: "(\d+)"$', text, re.M).group(1))
        limit = int(re.search(r'^  FUZZ_SHARD_MAX_MINUTES: "(\d+)"$', text, re.M).group(1))
        timeout = int(re.search(r"^    timeout-minutes: (\d+) # one shard$", text, re.M).group(1))
        # The plan refuses a shard before the job timeout would cut it.
        self.assertLess(limit, timeout)
        p = fs.plan(REPO, shards=shards, seconds=600, max_minutes=limit)
        self.assertEqual(sum(e["targets"] for e in p["include"]), len(fs.load_targets(REPO)))


def shard_result(shard, of, seconds, targets, **outcomes):
    """A per-shard result file's content; `outcomes` maps a target name to
    (outcome, artefacts) for the targets that did not end `ok`."""
    rows = []
    for crate, name in targets:
        outcome, artefacts = outcomes.get(name, ("ok", 0))
        rows.append({"crate": crate, "name": name, "outcome": outcome, "artefacts": artefacts,
                     "elapsed_s": seconds + 3})
    return {"shard": shard, "shards": of, "seconds_per_target": seconds, "targets": rows}


class MergeTests(unittest.TestCase):
    def setUp(self):
        self.root = make_root(MIXED)
        self.dealt = fs.deal(sorted(MIXED), 4)
        self.results = tempfile.mkdtemp()

    def write(self, shard, seconds=60, **outcomes):
        doc = shard_result(shard, len(self.dealt), seconds, self.dealt[shard - 1], **outcomes)
        self.put(shard, doc)
        return doc

    def put(self, shard, doc):
        with open(os.path.join(self.results, "fuzz-shard-%d.json" % shard), "w") as f:
            json.dump(doc, f) if not isinstance(doc, str) else f.write(doc)

    def merge(self, seconds=60):
        return fs.merge(self.root, 4, seconds, self.results)

    def test_every_shard_clean_merges_to_the_manifest_count_and_no_problem(self):
        for k in (1, 2, 3, 4):
            self.write(k)
        merged, problems = self.merge()
        self.assertEqual(problems, [])
        self.assertEqual(merged, {"targets_run": len(MIXED), "seconds_per_target": 60, "crashes": 0,
                                  "shards": 4, "crates": ["tst-core", "tst-rtp", "tst-srt", "tst-webrtc"]})

    def test_no_result_file_at_all_writes_nothing_and_is_a_problem(self):
        for results in (self.results, os.path.join(self.results, "absent")):
            with self.subTest(results=results):
                merged, problems = fs.merge(self.root, 4, 60, results)
                self.assertIsNone(merged)
                self.assertEqual(len(problems), 4)
                self.assertIn("shard 1 of 4 left no result", problems[0])

    def test_a_missing_shard_is_a_problem_and_its_targets_are_not_counted(self):
        for k in (1, 2, 4):
            self.write(k)
        merged, problems = self.merge()
        self.assertEqual(merged["targets_run"], len(MIXED) - len(self.dealt[2]))
        self.assertTrue(any("shard 3 of 4 left no result" in p for p in problems), problems)
        self.assertTrue(any("ran %d of %d declared targets" % (merged["targets_run"], len(MIXED)) in p
                            for p in problems), problems)

    def test_a_crash_is_counted_ran_and_named(self):
        for k in (1, 2, 3, 4):
            self.write(k, **({"t01": ("crash", 2)} if k == 1 else {}))
        merged, problems = self.merge()
        self.assertEqual(merged["targets_run"], len(MIXED))
        self.assertEqual(merged["crashes"], 2)
        self.assertEqual(problems, ["tst-core/t01 (shard 1 of 4): crash, 2 artefact file(s)"])

    def test_artefact_files_of_a_target_that_ended_ok_are_not_crashes(self):
        # libFuzzer also writes artefacts that are not failures (a slow unit).
        for k in (1, 2, 3, 4):
            self.write(k, **({"t01": ("ok", 3)} if k == 1 else {}))
        merged, problems = self.merge()
        self.assertEqual((merged["targets_run"], merged["crashes"], problems), (len(MIXED), 0, []))

    def test_a_target_that_errored_or_ended_early_did_not_run(self):
        for outcome in ("error", "short"):
            with self.subTest(outcome=outcome):
                for k in (1, 2, 3, 4):
                    self.write(k, **({"t01": (outcome, 0)} if k == 1 else {}))
                merged, problems = self.merge()
                self.assertEqual(merged["targets_run"], len(MIXED) - 1)
                self.assertEqual(merged["crashes"], 0)
                self.assertIn("tst-core/t01 (shard 1 of 4): %s" % outcome, problems)

    def test_a_shard_that_ran_nothing_is_a_problem(self):
        for k in (1, 2, 3, 4):
            self.write(k, **({n: ("error", 0) for _, n in self.dealt[1]} if k == 2 else {}))
        _, problems = self.merge()
        self.assertIn("shard 2 of 4 ran none of its %d targets" % len(self.dealt[1]), problems)

    def test_an_outcome_the_merge_does_not_know_did_not_run(self):
        for k in (1, 2, 3, 4):
            self.write(k, **({"t01": ("fine", 0)} if k == 1 else {}))
        merged, problems = self.merge()
        self.assertEqual(merged["targets_run"], len(MIXED) - 1)
        self.assertIn("tst-core/t01 (shard 1 of 4): fine", problems)

    def test_a_shard_whose_targets_are_not_the_planned_ones_is_a_problem(self):
        for k in (1, 2, 3, 4):
            self.write(k)
        doc = shard_result(2, 4, 60, self.dealt[1][:-1])
        self.put(2, doc)
        merged, problems = self.merge()
        missing = "%s/%s" % self.dealt[1][-1]
        self.assertTrue(any("shard 2 of 4" in p and "missing " + missing in p for p in problems), problems)
        self.assertEqual(merged["targets_run"], len(MIXED) - 1)
        # A target the plan gave to another shard is not counted where it does not belong.
        doc = shard_result(2, 4, 60, self.dealt[1] + self.dealt[0][:1])
        self.put(2, doc)
        merged, problems = self.merge()
        self.assertTrue(any("shard 2 of 4" in p and "unplanned tst-core/t00" in p for p in problems), problems)
        self.assertEqual(merged["targets_run"], len(MIXED))

    def test_a_target_listed_twice_in_a_shard_is_counted_once(self):
        for k in (1, 2, 3, 4):
            self.write(k)
        self.put(2, shard_result(2, 4, 60, self.dealt[1] + self.dealt[1][:1]))
        merged, problems = self.merge()
        self.assertEqual(merged["targets_run"], len(MIXED))
        self.assertTrue(any("shard 2 of 4" in p and "twice" in p for p in problems), problems)

    def test_the_seconds_are_the_minimum_over_shards_and_a_short_shard_is_a_problem(self):
        for k in (1, 2, 3, 4):
            self.write(k, seconds=20 if k == 3 else 600)
        merged, problems = self.merge(seconds=600)
        self.assertEqual(merged["seconds_per_target"], 20)
        self.assertEqual(problems, ["shard 3 of 4 ran 20 s per target, not the 600 asked for"])

    def test_a_result_from_another_layout_is_a_problem(self):
        for k in (1, 2, 3, 4):
            self.write(k)
        self.put(2, dict(shard_result(2, 5, 60, self.dealt[1])))
        _, problems = self.merge()
        self.assertTrue(any("shard 2 of 4" in p and "says it is shard 2 of 5" in p for p in problems), problems)

    def test_a_result_file_for_a_shard_that_was_not_planned_is_a_problem(self):
        for k in (1, 2, 3, 4):
            self.write(k)
        self.put(5, shard_result(5, 4, 60, self.dealt[0]))
        merged, problems = self.merge()
        self.assertEqual(merged["targets_run"], len(MIXED))
        self.assertEqual(problems, ["fuzz-shard-5.json is not one of the 4 planned shards"])

    def test_an_unreadable_result_is_a_problem_not_a_pass(self):
        for k in (1, 2, 3, 4):
            self.write(k)
        for bad in ("{", "[]", json.dumps({"shard": 2, "shards": 4, "seconds_per_target": 60}),
                    json.dumps({"shard": 2, "shards": 4, "seconds_per_target": "60", "targets": []}),
                    json.dumps({"shard": 2, "shards": 4, "seconds_per_target": 60,
                                "targets": [{"crate": "tst-core"}]}),
                    json.dumps({"shard": 2, "shards": 4, "seconds_per_target": 60, "targets": ["t05"]})):
            with self.subTest(bad=bad):
                self.put(2, bad)
                merged, problems = self.merge()
                self.assertTrue(any("fuzz-shard-2.json" in p and "unreadable" in p for p in problems), problems)
                self.assertEqual(merged["targets_run"], len(MIXED) - len(self.dealt[1]))


class CliTests(unittest.TestCase):
    def run_cli(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                rc = fs.main(argv)
            except SystemExit as e:
                rc = e.code
        return rc, out.getvalue(), err.getvalue()

    def test_plan_prints_the_matrix_on_stdout_and_the_table_on_stderr(self):
        root = make_root(MIXED)
        rc, out, err = self.run_cli(["plan", "--root", root, "--shards", "4", "--seconds", "600",
                                     "--max-minutes", "90"])
        self.assertEqual(rc, 0, err)
        self.assertEqual(len(out.splitlines()), 1)
        self.assertEqual(len(json.loads(out)["include"]), 4)
        self.assertIn("shard 1 of 4: 5 targets, about", err)
        self.assertIn("tst-core/t00", err)
        self.assertIn("%d targets in 4 shards at 600 s each" % len(MIXED), err)

    def test_targets_prints_one_shard_s_crate_and_name_per_line(self):
        root = make_root(MIXED)
        lines = []
        for k in (1, 2, 3, 4):
            rc, out, err = self.run_cli(["targets", "--root", root, "--shards", "4", "--shard", str(k)])
            self.assertEqual(rc, 0, err)
            lines += out.splitlines()
        self.assertEqual(lines, ["%s %s" % t for t in sorted(MIXED)])

    def test_a_shard_outside_the_plan_is_an_error(self):
        root = make_root(MIXED)
        for k in ("0", "5"):
            rc, out, err = self.run_cli(["targets", "--root", root, "--shards", "4", "--shard", k])
            self.assertEqual((rc, out), (1, ""))
            self.assertIn("there is no shard %s" % k, err)

    def test_an_empty_manifest_fails_every_command(self):
        root = make_root([], text="schema_version = 0\n")
        for argv in (["plan", "--shards", "4", "--seconds", "60", "--max-minutes", "90"],
                     ["targets", "--shards", "4", "--shard", "1"],
                     ["merge", "--shards", "4", "--seconds", "60", "--results", root, "--out",
                      os.path.join(root, "fuzz.json")]):
            rc, out, err = self.run_cli(argv[:1] + ["--root", root] + argv[1:])
            self.assertEqual((rc, out), (1, ""), argv)
            self.assertIn("declares no fuzz target", err)
        self.assertFalse(os.path.exists(os.path.join(root, "fuzz.json")))

    def test_merge_writes_the_file_and_fails_when_there_is_a_problem(self):
        root = make_root(MIXED)
        results, out_file = tempfile.mkdtemp(), os.path.join(tempfile.mkdtemp(), "fuzz.json")
        dealt = fs.deal(sorted(MIXED), 4)
        for k in (1, 2, 3):
            with open(os.path.join(results, "fuzz-shard-%d.json" % k), "w") as f:
                json.dump(shard_result(k, 4, 60, dealt[k - 1]), f)
        argv = ["merge", "--root", root, "--shards", "4", "--seconds", "60", "--results", results,
                "--out", out_file]
        rc, _, err = self.run_cli(argv)
        self.assertEqual(rc, 1)
        self.assertIn("shard 4 of 4 left no result", err)
        with open(out_file) as f:
            self.assertEqual(json.load(f)["targets_run"], len(MIXED) - len(dealt[3]))
        with open(os.path.join(results, "fuzz-shard-4.json"), "w") as f:
            json.dump(shard_result(4, 4, 60, dealt[3]), f)
        rc, out, err = self.run_cli(argv)
        self.assertEqual(rc, 0, err)
        self.assertIn("%d of %d declared targets ran for 60 s each with 0 crash(es)" % (len(MIXED), len(MIXED)), out)

    def test_merge_with_no_result_writes_no_file(self):
        root = make_root(MIXED)
        out_file = os.path.join(tempfile.mkdtemp(), "fuzz.json")
        rc, _, _ = self.run_cli(["merge", "--root", root, "--shards", "4", "--seconds", "60",
                                 "--results", tempfile.mkdtemp(), "--out", out_file])
        self.assertEqual(rc, 1)
        self.assertFalse(os.path.exists(out_file))


if __name__ == "__main__":
    unittest.main()
