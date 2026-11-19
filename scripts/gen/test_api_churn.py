"""Unit tests for scripts/gen/api_churn.py.

Run:  python3 -m unittest scripts/gen/test_api_churn.py

The fixture report is written from the documented text format of
cargo-semver-checks (a "Failed in:" block lists one item per line), not
captured from a run.
"""
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import api_churn as ac  # noqa: E402

TABLE = """\
| Package | Module | Tier | Why | Before Stable |
|---|---|---|---|---|
| tst-core | mpegts | Stable | w | — |
| tst-core | mpegts::demux::low_level | Internal | w | — |
| tst-core | klv::st0102 | Provisional | w | one minor |
| tst-core | transport | Stable | w | — |
| tst-srt | (crate) | Stable | w | — |
| tst-srt | shells | Provisional | w | one minor |
| tst-rtp | rtsp::server::mount_rules | Experimental | w | — |
| tst-rtp | rtsp | Provisional | w | one minor |
"""


def fixture(name):
    with open(os.path.join(HERE, "fixtures", name), encoding="utf-8") as f:
        return f.read()


class Parser(unittest.TestCase):
    def test_failed_in_blocks_give_item_paths(self):
        self.assertEqual(ac.failed_paths(fixture("semver-report-tst-core.txt")), [
            "tst_core::mpegts::mux::old_name",
            "tst_core::klv::st0102::gone",
            "tst_core::transport::OldStats",
            "tst_core::Loose"])

    def test_other_line_shapes(self):
        text = "Failed in:\n  variant Foo:Bar in /w/src/lib.rs:5\n  field Foo.x in file /w/a.rs:1\n\nDescription:\n  not an item\n"
        self.assertEqual(ac.failed_paths(text), ["Foo:Bar", "Foo.x"])

    def test_no_failed_block_no_paths(self):
        self.assertEqual(ac.failed_paths("Checked 3 checks: 3 pass, 0 fail\n"), [])

    def test_paths_command_prints_json(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            ac.main(["paths", os.path.join(HERE, "fixtures", "semver-report-tst-core.txt")])
        self.assertEqual(len(json.loads(out.getvalue())), 4)


class Tiers(unittest.TestCase):
    def setUp(self):
        self.rows = ac.classification(TABLE)

    def test_the_most_specific_row_wins(self):
        self.assertEqual(ac.tier_of(self.rows, "tst-core", "tst_core::mpegts::mux::old_name"), "stable")
        self.assertEqual(ac.tier_of(self.rows, "tst-core", "tst_core::mpegts::demux::low_level::X"), "internal")
        self.assertEqual(ac.tier_of(self.rows, "tst-rtp", "tst_rtp::rtsp::server::mount_rules::R"), "experimental")
        self.assertEqual(ac.tier_of(self.rows, "tst-rtp", "tst_rtp::rtsp::client::C"), "provisional")

    def test_a_crate_row_covers_every_module_but_a_named_one_beats_it(self):
        self.assertEqual(ac.tier_of(self.rows, "tst-srt", "tst_srt::socket::S"), "stable")
        self.assertEqual(ac.tier_of(self.rows, "tst-srt", "tst_srt::shells::open"), "provisional")

    def test_a_module_name_is_matched_by_whole_segments(self):
        with self.assertRaises(ac.ChurnError):
            ac.tier_of(self.rows, "tst-core", "tst_core::mpegtsx::Y")

    def test_an_unmapped_path_is_an_error_naming_it(self):
        for path in ("tst_core::nowhere::Z", "tst_core::Loose", "Foo:Bar"):
            with self.assertRaises(ac.ChurnError) as e:
                ac.tier_of(self.rows, "tst-core", path)
            self.assertIn(path, str(e.exception))


class Tally(unittest.TestCase):
    def report(self, **crate):
        return [dict({"crate": "tst-core", "exit": 100, "failed_checks": 2, "items": 4, "report": "r",
                      "analysed": True, "paths": ["tst_core::mpegts::a", "tst_core::mpegts::a",
                                                  "tst_core::klv::st0102::b", "tst_core::transport::c"]}, **crate)]

    def test_counts_distinct_paths_per_tier(self):
        self.assertEqual(ac.tally(self.report(), ac.classification(TABLE), "0.11.0"),
                         {"release": "0.11.0", "stable": 2, "provisional": 1, "experimental": 0, "internal": 0})

    def test_a_break_report_without_paths_is_an_error(self):
        rep = self.report()
        del rep[0]["paths"]
        with self.assertRaises(ac.ChurnError):
            ac.tally(rep, ac.classification(TABLE), "0.11.0")

    def test_a_clean_report_counts_nothing(self):
        rep = self.report(exit=0, failed_checks=0, items=0, paths=[])
        self.assertEqual(ac.tally(rep, ac.classification(TABLE), "0.11.0")["stable"], 0)


CHURN = """\
schema_version = 0

[[release]]
version = "0.10.0"
note = "per-crate failed checks only: tst-core 2, tst-hls 1; the record kept no item paths"

[[release]]
version = "0.11.0"
stable = 2
provisional = 1
experimental = 0
internal = 0
"""

PAGE = "# Page\n\nintro\n\n<!-- BEGIN GENERATED: api-churn -->\nstale\n<!-- END GENERATED: api-churn -->\n\nafter\n"


class Render(unittest.TestCase):
    def test_table_rows(self):
        out = ac.render_table(ac.parse_churn(CHURN))
        lines = out.splitlines()
        self.assertEqual(lines[0], "| Release | Basis | Items | Stable | Provisional | Experimental | Internal |")
        self.assertTrue(lines[2].startswith("| 0.10.0 | per-crate failed checks only: tst-core 2, tst-hls 1; the record kept no item paths | — | — | — | — | — |"))
        self.assertEqual(lines[3], "| 0.11.0 | items | 3 | 2 | 1 | 0 | 0 |")

    def test_a_row_with_neither_counts_nor_note_is_an_error(self):
        with self.assertRaises(ac.ChurnError):
            ac.parse_churn('[[release]]\nversion = "0.9.0"\n')

    def test_write_then_check_round_trips_and_drift_is_found(self):
        d = tempfile.mkdtemp()
        os.makedirs(os.path.join(d, "tests", "coverage"))
        os.makedirs(os.path.join(d, "docs", "reference"))
        with open(os.path.join(d, "tests", "coverage", "api-churn.toml"), "w") as f:
            f.write(CHURN)
        page = os.path.join(d, "docs", "reference", "api-stability.md")
        with open(page, "w") as f:
            f.write(PAGE)
        with self.assertRaises(SystemExit) as e, contextlib.redirect_stderr(io.StringIO()) as err:
            ac.main(["render", "--root", d, "--check"])
        self.assertEqual(e.exception.code, 1)
        self.assertIn("stale", err.getvalue())
        ac.main(["render", "--root", d, "--write"])
        ac.main(["render", "--root", d, "--check"])
        text = open(page).read()
        self.assertTrue(text.endswith("\nafter\n") and text.startswith("# Page\n\nintro"))
        with open(os.path.join(d, "tests", "coverage", "api-churn.toml"), "w") as f:
            f.write(CHURN.replace("stable = 2", "stable = 3"))
        with self.assertRaises(SystemExit):
            with contextlib.redirect_stderr(io.StringIO()):
                ac.main(["render", "--root", d, "--check"])

    def test_a_page_without_the_markers_is_an_error(self):
        d = tempfile.mkdtemp()
        os.makedirs(os.path.join(d, "tests", "coverage"))
        os.makedirs(os.path.join(d, "docs", "reference"))
        with open(os.path.join(d, "tests", "coverage", "api-churn.toml"), "w") as f:
            f.write(CHURN)
        with open(os.path.join(d, "docs", "reference", "api-stability.md"), "w") as f:
            f.write("# Page\n")
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            ac.main(["render", "--root", d, "--write"])


if __name__ == "__main__":
    unittest.main()
