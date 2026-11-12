"""Unit tests for scripts/dev/bench_ir.py (the instruction-count judge).

Run: python3 -m unittest scripts/dev/test_bench_ir.py
"""
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bench_ir  # noqa: E402

ACCEPTED = """
[[accepted]]
workload = "decode"
since = "v1"
max_growth_percent = 26
reason = "second walk"

[[accepted]]
workload = "decode"
since = "v0"
max_growth_percent = 90
reason = "history"
"""


def status(results, accepted_text="", old="v1", threshold=3.0):
    workloads, passed = bench_ir.judge(results, bench_ir.load_accepted(accepted_text, old), threshold)
    return {w["name"]: w["status"] for w in workloads}, passed


class Judge(unittest.TestCase):
    def test_growth_within_the_threshold_passes(self):
        st, passed = status([("decode", 100, 100_000, 103_000), ("mux", 10, 5_000, 4_000)])
        self.assertEqual(st, {"decode": "ok", "mux": "ok"})
        self.assertTrue(passed)

    def test_growth_over_the_threshold_fails(self):
        st, passed = status([("decode", 100, 100_000, 103_100), ("mux", 10, 5_000, 5_000)])
        self.assertEqual(st, {"decode": "over the threshold", "mux": "ok"})
        self.assertFalse(passed)

    def test_an_accepted_row_for_this_base_passes_up_to_its_limit(self):
        st, passed = status([("decode", 100, 100_000, 125_000)], ACCEPTED)
        self.assertEqual(st, {"decode": "accepted"})
        self.assertTrue(passed)
        st, passed = status([("decode", 100, 100_000, 126_000)], ACCEPTED)
        self.assertEqual(st, {"decode": "accepted"})
        st, passed = status([("decode", 100, 100_000, 127_000)], ACCEPTED)
        self.assertEqual(st, {"decode": "over the accepted 26 %"})
        self.assertFalse(passed)

    def test_a_row_for_another_base_is_ignored(self):
        st, passed = status([("decode", 100, 100_000, 125_000)], ACCEPTED, old="v2")
        self.assertEqual(st, {"decode": "over the threshold"})
        self.assertFalse(passed)

    def test_a_row_naming_no_workload_of_the_run_is_an_error(self):
        with self.assertRaises(bench_ir.InputError):
            status([("mux", 10, 5_000, 5_000)], ACCEPTED)

    def test_no_old_count_fails_unless_the_star_row_accepts_it(self):
        st, passed = status([("decode", 100, None, 125_000)])
        self.assertEqual(st, {"decode": "not compared"})
        self.assertFalse(passed)
        star = '[[accepted]]\nworkload = "*"\nsince = "v1"\nreason = "Muxer::pull was renamed"\n'
        st, passed = status([("decode", 100, None, 125_000)], star)
        self.assertEqual(st, {"decode": "not compared, accepted"})
        self.assertTrue(passed)

    def test_a_star_row_is_stale_once_the_old_side_was_counted(self):
        star = '[[accepted]]\nworkload = "*"\nsince = "v1"\nreason = "Muxer::pull was renamed"\n'
        with self.assertRaises(bench_ir.InputError):
            status([("decode", 100, 100_000, 100_000)], star)

    def test_malformed_rows_are_refused(self):
        for text in ('[[accepted]]\nworkload = "decode"\nsince = "v1"\nmax_growth_percent = 5\n',
                     '[[accepted]]\nworkload = "decode"\nsince = "v1"\nreason = "x"\n',
                     ACCEPTED + '[[accepted]]\nworkload = "decode"\nsince = "v1"\nmax_growth_percent = 5\nreason = "x"\n'):
            with self.assertRaises(bench_ir.InputError):
                bench_ir.load_accepted(text, "v1")

    def test_results_parse_and_reject_garbage(self):
        self.assertEqual(bench_ir.parse_results("a\t10\t\t50\nb\t1\t7\t8\n"), [("a", 10, None, 50), ("b", 1, 7, 8)])
        for text in ("", "a\t10\t5\n", "a\tx\t5\t6\n", "a\t0\t5\t6\n", "a\t10\t0\t6\n", "a\t10\t5\t0\n"):
            with self.assertRaises(bench_ir.InputError):
                bench_ir.parse_results(text)

    def test_the_table_lists_the_largest_growth_first_and_the_reason(self):
        workloads, passed = bench_ir.judge([("mux", 10, 5_000, 5_000), ("decode", 100, 100_000, 125_000)],
                                           bench_ir.load_accepted(ACCEPTED, "v1"))
        text = bench_ir.render(workloads, passed, "v1", "HEAD", 3.0).splitlines()
        self.assertIn("decode", text[1])
        self.assertIn("accepted: second walk", text[2])
        self.assertEqual(text[-1], "verdict: pass")


class Main(unittest.TestCase):
    def run_main(self, results, accepted=""):
        with tempfile.TemporaryDirectory() as tmp:
            paths = {n: os.path.join(tmp, n) for n in ("results.tsv", "accepted.toml", "out.json")}
            with open(paths["results.tsv"], "w") as f:
                f.write(results)
            with open(paths["accepted.toml"], "w") as f:
                f.write(accepted)
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                rc = bench_ir.main(["--results", paths["results.tsv"], "--old", "v1", "--new", "HEAD",
                                    "--accepted", paths["accepted.toml"], "--json", paths["out.json"]])
            record = None
            if os.path.exists(paths["out.json"]):
                with open(paths["out.json"]) as f:
                    record = json.load(f)
            return rc, record

    def test_exit_codes_and_the_json_record(self):
        rc, record = self.run_main("decode\t100\t100000\t101000\n")
        self.assertEqual(rc, 0)
        self.assertEqual(record["verdict"], "pass")
        self.assertEqual(record["old"]["ref"], "v1")
        self.assertEqual(record["workloads"][0]["delta_percent"], 1.0)
        rc, record = self.run_main("decode\t100\t100000\t110000\n")
        self.assertEqual(rc, 1)
        self.assertEqual(record["verdict"], "fail")
        rc, record = self.run_main("decode\t100\t\n")
        self.assertEqual((rc, record), (2, None))
        rc, record = self.run_main("decode\t100\t100000\t101000\n", "[[accepted]]\nworkload = 3\n")
        self.assertEqual((rc, record), (2, None))


if __name__ == "__main__":
    unittest.main()
