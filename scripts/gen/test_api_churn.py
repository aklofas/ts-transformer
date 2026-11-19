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


def line(**kw):
    return dict({"line": "x", "lint": None, "file": None, "span_line": None, "name": None, "path": None}, **kw)


class Parser(unittest.TestCase):
    def lines(self):
        return ac.failed_lines(fixture("semver-report-tst-core.txt"))

    def test_every_line_of_every_block_is_kept(self):
        got = self.lines()
        self.assertEqual(len(got), 8)
        self.assertTrue(all(e["file"] for e in got))

    def test_each_template_shape_gives_its_span_file_and_path(self):
        got = {e["line"].split()[0] + ":" + e["line"].split()[1]: e for e in self.lines()}
        self.assertEqual(got["tst_core::mpegts::mux::push_klv:now"]["file"], "/work/crates/tst-core/src/mpegts/mux.rs")
        self.assertEqual(got["tst_core::mpegts::mux::push_klv:now"]["path"], "tst_core::mpegts::mux::push_klv")
        self.assertEqual(got["Transport::old_method,:previously"]["path"], "Transport::old_method")
        self.assertEqual(got["Transport::old_method,:previously"]["span_line"], 88)
        self.assertEqual(got["tst_core::klv::st0102::Set::encode:takes"]["file"],
                         "/work/baseline/crates/tst-core/src/klv/st0102/mod.rs")
        self.assertEqual(got["field:depth"]["path"], None)
        self.assertEqual(got["variant:EndReason:Reset"]["file"], "/work/crates/tst-core/src/transport.rs")
        self.assertEqual(got["trait:method"]["path"], "tst_core::transport::Transport::flush")

    def test_a_line_without_a_span_keeps_its_path(self):
        got = ac.failed_lines("Failed in:\n  function tst_core::mpegts::gone\n")
        self.assertEqual(got, [{"line": "function tst_core::mpegts::gone", "lint": None, "file": None,
                                "span_line": None, "name": "gone", "path": "tst_core::mpegts::gone"}])

    def test_a_line_carries_its_lint_and_item_name(self):
        got = {e["name"]: e for e in self.lines()}
        self.assertEqual(got["push_klv"]["lint"], "function_parameter_count_changed")
        self.assertEqual(got["Reset"]["lint"], "enum_variant_added")
        self.assertEqual(got["RawSendStats"]["lint"], "struct_missing")

    def test_no_failed_block_no_lines(self):
        self.assertEqual(ac.failed_lines("Checked 3 checks: 3 pass, 0 fail\n"), [])

    def test_the_comment_lines_of_the_fixture_are_outside_every_block(self):
        self.assertFalse(any(e["line"].startswith("#") for e in self.lines()))

    def test_paths_command_prints_json(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            ac.main(["paths", os.path.join(HERE, "fixtures", "semver-report-tst-core.txt")])
        self.assertEqual(len(json.loads(out.getvalue())), 8)


class FileModules(unittest.TestCase):
    def test_source_files_name_their_crate_and_module(self):
        self.assertEqual(ac.split_source("/w/crates/tst-core/src/mpegts/mux.rs"), ("tst-core", ("mpegts", "mux")))
        self.assertEqual(ac.split_source("/w/crates/tst-core/src/klv/st0102/mod.rs"), ("tst-core", ("klv", "st0102")))
        self.assertEqual(ac.split_source("crates/tst-core/src/lib.rs"), ("tst-core", ()))
        self.assertEqual(ac.split_source("src/transport.rs"), (None, ("transport",)))
        self.assertIsNone(ac.split_source("/w/build.rs"))

    def test_the_last_crates_directory_and_src_win(self):
        self.assertEqual(ac.split_source("/home/src/x/crates/tst-rtp/src/a.rs"), ("tst-rtp", ("a",)))
        self.assertEqual(ac.split_source("/home/src/x/src/a.rs"), (None, ("a",)))


class Tiers(unittest.TestCase):
    def setUp(self):
        self.rows = ac.classification(TABLE)

    def tier(self, package, **kw):
        return ac.tier_of(self.rows, package, line(**kw))

    def test_the_span_file_decides_and_the_most_specific_row_wins(self):
        self.assertEqual(self.tier("tst-core", file="/w/crates/tst-core/src/mpegts/mux.rs"), "stable")
        self.assertEqual(self.tier("tst-core", file="/w/crates/tst-core/src/mpegts/demux/low_level/x.rs"), "internal")
        self.assertEqual(self.tier("tst-core", file="/w/crates/tst-core/src/klv/st0102/mod.rs"), "provisional")
        self.assertEqual(self.tier("tst-rtp", file="/w/crates/tst-rtp/src/rtsp/server/mount_rules.rs"), "experimental")
        self.assertEqual(self.tier("tst-rtp", file="/w/crates/tst-rtp/src/rtsp/client.rs"), "provisional")

    def test_a_name_only_line_is_placed_by_its_file(self):
        self.assertEqual(self.tier("tst-core", line="Transport::old_method", path=None,
                                   file="/w/crates/tst-core/src/transport.rs"), "stable")

    def test_a_crate_row_covers_every_file_but_a_named_module_beats_it(self):
        self.assertEqual(self.tier("tst-srt", file="/w/crates/tst-srt/src/socket.rs"), "stable")
        self.assertEqual(self.tier("tst-srt", file="/w/crates/tst-srt/src/lib.rs"), "stable")
        self.assertEqual(self.tier("tst-srt", file="/w/crates/tst-srt/src/shells.rs"), "provisional")

    def test_a_module_is_matched_by_whole_segments(self):
        with self.assertRaises(ac.ChurnError):
            self.tier("tst-core", file="/w/crates/tst-core/src/mpegtsx.rs")

    def test_the_item_path_is_the_fallback_when_there_is_no_file(self):
        self.assertEqual(self.tier("tst-core", path="tst_core::mpegts::mux::f"), "stable")
        self.assertEqual(self.tier("tst-core", file="/w/crates/tst-core/src/lib.rs", path="tst_core::klv::st0102::f"),
                         "provisional")

    def test_a_line_neither_maps_is_an_error_naming_it(self):
        for kw in (dict(line="field x of struct S", file="/w/crates/tst-core/src/lib.rs"),
                   dict(line="function tst_core::nowhere::f", path="tst_core::nowhere::f"),
                   dict(line="bare line")):
            with self.assertRaises(ac.ChurnError) as e:
                self.tier("tst-core", **kw)
            self.assertIn(kw["line"], str(e.exception))


class Tally(unittest.TestCase):
    def crate(self, **kw):
        base = "/w/crates/tst-core/src/"
        breaks = [line(line="a", file=base + "mpegts/mux.rs", span_line=1),
                  line(line="b", file=base + "mpegts/mux.rs", span_line=1),  # the root re-export of a
                  line(line="c", file=base + "klv/st0102.rs", span_line=2),
                  line(line="d", file=base + "transport.rs", span_line=3),
                  line(line="e", file=base + "mpegts/demux/low_level/q.rs", span_line=4)]
        return [dict({"crate": "tst-core", "exit": 100, "failed_checks": 4, "items": 5, "report": "r",
                      "analysed": True, "breaks": breaks}, **kw)]

    def test_counts_per_tier_and_an_item_printed_per_importable_path_counts_once(self):
        self.assertEqual(ac.tally(self.crate(), ac.classification(TABLE), "0.11.0"),
                         {"release": "0.11.0", "stable": 2, "provisional": 1, "experimental": 0, "internal": 1})

    def tally_of(self, *breaks):
        rep = [{"crate": "tst-core", "exit": 100, "items": len(breaks), "breaks": list(breaks)}]
        return ac.tally(rep, ac.classification(TABLE), "x")

    SRC = "/w/crates/tst-core/src/transport.rs"

    def test_two_lints_on_one_item_count_twice(self):
        a = line(line="a", lint="function_parameter_count_changed", file=self.SRC, span_line=5, name="f")
        b = dict(a, lint="function_missing")
        self.assertEqual(self.tally_of(a, b)["stable"], 2)

    def test_two_variants_on_one_line_count_twice(self):
        a = line(line="variant K:A", lint="enum_variant_added", file=self.SRC, span_line=5, name="A")
        self.assertEqual(self.tally_of(a, dict(a, line="variant K:B", name="B"))["stable"], 2)

    def test_the_importable_path_lines_of_one_removed_item_count_once(self):
        a = line(line="struct tst_core::transport::S", lint="struct_missing", file=self.SRC, span_line=5, name="S")
        self.assertEqual(self.tally_of(a, dict(a, line="struct tst_core::S"))["stable"], 1)

    def test_a_span_in_another_crate_is_placed_by_that_crates_rows(self):
        rep = [{"crate": "tst-srt", "exit": 100, "items": 1, "breaks": [line(
            line="struct tst_srt::Moved, previously in file /w/crates/tst-core/src/klv/st0102.rs:7",
            file="/w/crates/tst-core/src/klv/st0102.rs", span_line=7, name="Moved")]}]
        # tst-srt's own (crate) row says Stable; the defining crate's row says Provisional.
        self.assertEqual(ac.tally(rep, ac.classification(TABLE), "x")["provisional"], 1)

    def test_a_crate_whose_lines_differ_in_number_from_its_items_is_an_error(self):
        with self.assertRaises(ac.ChurnError) as e:
            ac.tally(self.crate(items=6), ac.classification(TABLE), "0.11.0")
        self.assertIn("tst-core", str(e.exception))

    def test_a_break_report_without_lines_is_an_error(self):
        rep = self.crate()
        del rep[0]["breaks"]
        with self.assertRaises(ac.ChurnError):
            ac.tally(rep, ac.classification(TABLE), "0.11.0")

    def test_a_clean_report_counts_nothing(self):
        rep = self.crate(exit=0, failed_checks=0, items=0, breaks=[])
        self.assertEqual(ac.tally(rep, ac.classification(TABLE), "0.11.0")["stable"], 0)

    def test_the_fixture_report_maps_end_to_end(self):
        rows = ac.classification(TABLE.replace("| tst-core | transport |", "| tst-core | transport |")
                                 + "| tst-core | klv::st0102 | Provisional | w | one minor |\n")
        lines = ac.failed_lines(fixture("semver-report-tst-core.txt"))
        got = ac.tally([{"crate": "tst-core", "exit": 100, "items": len(lines), "breaks": lines}], rows, "x")
        self.assertEqual((got["stable"], got["provisional"]), (6, 1))
        self.assertEqual(sum(got[t] for t in ac.TIERS), 7)  # the two RawSendStats lines count once


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

    def test_the_items_cell_sums_all_four_tiers(self):
        text = CHURN.replace("internal = 0", "internal = 4")
        self.assertEqual(ac.render_table(ac.parse_churn(text)).splitlines()[3], "| 0.11.0 | items | 7 | 2 | 1 | 0 | 4 |")

    def test_the_tier_counts_are_all_or_none(self):
        with self.assertRaises(ac.ChurnError):
            ac.parse_churn('[[release]]\nversion = "0.9.0"\nstable = 1\nnote = "n"\n')

    def test_a_note_may_not_contain_a_pipe(self):
        with self.assertRaises(ac.ChurnError):
            ac.parse_churn('[[release]]\nversion = "0.9.0"\nnote = "a | b"\n')

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
