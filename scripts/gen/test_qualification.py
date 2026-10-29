"""Unit tests for scripts/gen/qualification.py.

Run:  python3 -m unittest scripts/gen/test_qualification.py
"""
import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

if sys.version_info < (3, 11):
    raise unittest.SkipTest("qualification needs Python >= 3.11 (tomllib)")

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import qualification as q  # noqa: E402

SHA = "1a2b3c4d" + "0" * 32
SUBS = {
    "crates/srt-sys/vendor/srt": {"sha": "a" * 40, "describe": "v1.5.7"},
    "crates/rist-sys/vendor/librist": {"sha": "b" * 40, "describe": "v0.2.20"},
    "crates/mbedtls-src/vendor/mbedtls": {"sha": "c" * 40, "describe": "mbedtls-3.6.7"},
}
CONTEXT = {
    "label": "main at 1a2b3c4d",
    "source": {"sha": SHA, "ref": "refs/heads/main", "dirty": False, "submodules": SUBS},
    "toolchain": {"rust": "rustc 1.85.1", "nightly_public_api": "nightly-2026-07-03",
                  "cbindgen": "0.29.2", "tools": {"abidiff": "2.4.0"}},
    "host": {"runner": "ubuntu-24.04", "arch": "x86_64", "kernel": "6.8.0"},
}
LEGS = {n: {"conclusion": "success", "run_url": "https://example.test/run/" + str(i)}
        for i, n in enumerate(["interop", "jvm-jar", "python-wheels", "c-abi", "fuzz"])}
LEGS["sanitizers"] = {"conclusion": "success", "run_url": "https://example.test/run/san"}
CI = {"conclusion": "success", "run_url": "https://example.test/run/ci"}
RESULTS = {"summary": {"total": 159, "pass": 94, "fail": 0, "expected_unsupported": 65,
                       "skipped_tool_missing": 0, "stale_expectations": []},
           "inventory": {"shape": "full", "declared_cells": 159, "expected_full_cells": 159,
                         "allowed_skips": []},
           "meta": {"au_sizes": "realistic"}}
INVENTORY = {"tools": {"ffmpeg": "7.1", "tsp": "3.40", "gst-launch-1.0": "1.24", "vlc": "3.0", "mpv": "0.37"}}
FUZZ = {"targets_run": 37, "seconds_per_target": 60, "crashes": 0}
ABI = {"major": 0, "minor": 25, "symbols": 505, "previous_tag": "v0.8.0",
       "previous_source": "built: no package for v0.8.0", "abidiff_exit": 4, "verdict": "compatible"}
SEMVER = [{"crate": "tst-core", "exit": 0, "failed_checks": 0, "items": 0, "report": "r", "analysed": True}]
SAN_JOBS = ["asan", "asan-native", "tsan", "tsan-native"]

FUZZ_TOML = "schema_version = 0\n" + "".join(
    '\n[[target]]\ncrate = "tst-core"\nname = "t%d"\n' % i for i in range(37))
ARCHIVES = """\
schema_version = 0

[corpus]
expected_files = 260

[[archive]]
id = "soak-x"
kind = "soak"
tree = "%s"
overall_pass = false
note = "The SRT leg passed; the RIST leg failed attribution."
date = "2026-10-04"
expected_duration_s = 259200
""" % ("d" * 40)


def slurp(path):
    with open(path) as f:
        return f.read()


def spit(path, text):
    with open(path, "w") as f:
        f.write(text)


def put(path, obj):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        if isinstance(obj, str):
            f.write(obj)
        else:
            json.dump(obj, f)


def build(tmp, mutate=None, archives=ARCHIVES):
    """A complete artefact directory + repo root; `mutate(files)` edits the dict first."""
    files = {
        "context.json": dict(CONTEXT),
        "legs.json": json.loads(json.dumps(LEGS)),
        "ci.json": dict(CI),
        "interop-evidence/results.json": json.loads(json.dumps(RESULTS)),
        "interop-evidence/inventory.json": dict(INVENTORY),
        "fuzz.json": dict(FUZZ),
        "abi.json": dict(ABI),
        "semver-checks.json": json.loads(json.dumps(SEMVER)),
    }
    for j in SAN_JOBS:
        files["sanitizer-%s/sanitizer-%s.json" % (j, j)] = {"job": j, "conclusion": "success"}
    if mutate:
        mutate(files)
    art = os.path.join(tmp, "art")
    for rel, obj in files.items():
        if obj is not None:
            put(os.path.join(art, rel), obj)
    root = os.path.join(tmp, "root")
    put(os.path.join(root, "tests/coverage/fuzz-targets.toml"), FUZZ_TOML)
    put(os.path.join(root, "tests/coverage/evidence-archives.toml"), archives)
    return art, root


def collect(mutate=None, release=False, archives=ARCHIVES):
    tmp = tempfile.mkdtemp()
    art, root = build(tmp, mutate, archives)
    return q.collect(art, root, release=release)


def leg(rec, name):
    return next(l for l in rec["legs"] if l["name"] == name)


class VerdictTests(unittest.TestCase):
    def test_complete_fixture_is_go(self):
        rec = collect()
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(rec["verdict_reasons"], [])
        self.assertEqual(rec["schema_version"], 0)
        self.assertEqual(rec["interop"]["shape"], "full-159")
        self.assertEqual(rec["fuzz"]["targets_declared"], 37)
        self.assertEqual(rec["natives"], {"libsrt": "1.5.7", "librist": "0.2.20", "mbedtls": "3.6.7"})
        self.assertEqual(len(rec["archives"]), 1)
        self.assertEqual(rec["peers"]["ffmpeg"], "7.1")

    def test_skipped_required_leg_is_no_go_and_named(self):
        def m(f):
            f["legs.json"]["jvm-jar"]["conclusion"] = "skipped"
        rec = collect(m)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("jvm-jar" in r and "skipped" in r for r in rec["verdict_reasons"]))

    def test_cancelled_required_leg_is_no_go(self):
        def m(f):
            f["sanitizer-tsan/sanitizer-tsan.json"]["conclusion"] = "cancelled"
        rec = collect(m)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("sanitizers/tsan" in r and "cancelled" in r for r in rec["verdict_reasons"]))

    def test_cancelled_sanitizers_workflow_is_no_go(self):
        def m(f):
            f["legs.json"]["sanitizers"]["conclusion"] = "cancelled"
        rec = collect(m)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertEqual(leg(rec, "sanitizers")["conclusion"], "cancelled")

    def test_missing_interop_artefact_is_missing_and_no_go(self):
        def m(f):
            f["interop-evidence/results.json"] = None
        rec = collect(m)
        self.assertEqual(leg(rec, "interop")["conclusion"], "missing")
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any(r.startswith("interop") for r in rec["verdict_reasons"]))

    def test_missing_sanitizer_artefact_is_missing_and_no_go(self):
        def m(f):
            f["sanitizer-asan-native/sanitizer-asan-native.json"] = None
        rec = collect(m)
        self.assertEqual(leg(rec, "sanitizers/asan-native")["conclusion"], "missing")
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("sanitizers/asan-native" in r for r in rec["verdict_reasons"]))

    def test_census_short_of_declared_is_no_go(self):
        def m(f):
            f["interop-evidence/results.json"]["summary"]["total"] = 158
        rec = collect(m)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("158" in r and "159" in r for r in rec["verdict_reasons"]))

    def test_subset_shape_stale_skips_failures_are_no_go(self):
        cases = [
            lambda f: f["interop-evidence/results.json"]["inventory"].update(shape="subset"),
            lambda f: f["interop-evidence/results.json"]["summary"].update(fail=1),
            lambda f: f["interop-evidence/results.json"]["summary"].update(skipped_tool_missing=2),
            lambda f: f["interop-evidence/results.json"]["summary"].update(
                stale_expectations=[{"id": "x"}]),
        ]
        for m in cases:
            self.assertEqual(collect(m)["verdict"], "NO-GO")

    def test_semver_checks_failure_never_flips_the_verdict(self):
        def m(f):
            f["legs.json"]["semver-checks"] = {"conclusion": "failure", "run_url": ""}
            f["semver-checks.json"][0].update(exit=1, failed_checks=3, analysed=False)
        rec = collect(m)
        self.assertEqual(rec["verdict"], "GO")
        self.assertFalse(leg(rec, "semver-checks")["required"])
        self.assertEqual(rec["semver_checks"][0]["breaking"], 3)

    def test_fuzz_crash_or_short_run_is_no_go(self):
        self.assertEqual(collect(lambda f: f["fuzz.json"].update(crashes=1))["verdict"], "NO-GO")
        self.assertEqual(collect(lambda f: f["fuzz.json"].update(targets_run=36))["verdict"], "NO-GO")

    def test_absent_ci_is_missing_no_go(self):
        def m(f):
            f["ci.json"] = None
        rec = collect(m)
        self.assertEqual(leg(rec, "ci (on this sha)")["conclusion"], "missing")
        self.assertEqual(rec["verdict"], "NO-GO")

    def test_fuzz_leg_without_a_conclusion_is_missing_even_with_a_clean_fuzz_json(self):
        def m(f):
            del f["legs.json"]["fuzz"]
        rec = collect(m)
        self.assertEqual(leg(rec, "fuzz")["conclusion"], "missing")
        self.assertEqual(rec["verdict"], "NO-GO")


class HoleTests(unittest.TestCase):
    def test_dirty_tree_is_no_go(self):
        def m(f):
            f["context.json"]["source"] = dict(CONTEXT["source"], dirty=True)
        rec = collect(m)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("uncommitted" in r for r in rec["verdict_reasons"]))

    def test_undeclared_census_is_no_go(self):
        def m(f):
            r = f["interop-evidence/results.json"]
            del r["summary"]["total"]
            del r["inventory"]["expected_full_cells"]
        rec = collect(m)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("undeclared" in r for r in rec["verdict_reasons"]))

    def test_missing_inventory_is_missing_interop(self):
        def m(f):
            f["interop-evidence/inventory.json"] = None
        rec = collect(m)
        self.assertEqual(leg(rec, "interop")["conclusion"], "missing")
        self.assertEqual(rec["verdict"], "NO-GO")

    def test_abi_missing_or_not_compatible_is_no_go(self):
        rec = collect(lambda f: f.__setitem__("abi.json", None))
        self.assertEqual(leg(rec, "c-abi")["conclusion"], "missing")
        self.assertEqual(rec["verdict"], "NO-GO")
        rec = collect(lambda f: f["abi.json"].update(verdict="breaking"))
        self.assertEqual(leg(rec, "c-abi")["conclusion"], "failure")
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertTrue(any("breaking" in r for r in rec["verdict_reasons"]))

    def test_abi_exit_zero_and_four_are_both_go(self):
        for code in (0, 4):
            self.assertEqual(collect(lambda f: f["abi.json"].update(abidiff_exit=code))["verdict"], "GO")


class RenderTests(unittest.TestCase):
    def setUp(self):
        self.rec = collect()

    def test_block_heading_and_census(self):
        text = q.render_block(self.rec)
        self.assertEqual(text.splitlines()[0], "## Evidence for the main at 1a2b3c4d tree")
        self.assertIn("159 / 94 / 0 / 65 / 0", text)
        self.assertIn("GO", text)
        self.assertIn("overall_pass=false", text)
        self.assertIn("The SRT leg passed; the RIST leg failed attribution.", text)

    def test_abi_line_carries_how_the_previous_library_was_obtained(self):
        for how in ("attested", "built: attestation verify failed", "built: no package for v0.8.0"):
            rec = collect(lambda f: f["abi.json"].update(previous_source=how))
            self.assertEqual(rec["abi"]["previous_source"], how)
            self.assertIn("compared with v0.8.0 (%s): abidiff exit 4" % how, q.render_block(rec))

    def test_no_go_block_names_the_leg(self):
        def m(f):
            f["legs.json"]["jvm-jar"]["conclusion"] = "skipped"
        text = q.render_block(collect(m))
        self.assertIn("NO-GO", text)
        self.assertIn("jvm-jar", text)

    def _page(self, tmp, block=None):
        page = os.path.join(tmp, "docs/project/validation-evidence.md")
        body = "# Evidence\n\nintro\n\n"
        if block is not None:
            body += block + "\n\n"
        body += "## Evidence for the 0.8.0 tree\n\nold text\n"
        put(page, body)
        exit_page = os.path.join(tmp, q.EXIT_PAGE)
        put(exit_page, "# Exit\n\n" + q.exit_wrapped(None, [], [], None) + "\n\nrest\n")
        return page

    def test_check_fails_when_the_exit_criteria_block_is_stale_and_write_fixes_it(self):
        tmp = tempfile.mkdtemp()
        self._page(tmp)
        jf = os.path.join(tmp, "q.json")
        put(jf, self.rec)
        rc, _, err = self.run_cli(["render", "--json", jf, "--root", tmp, "--write"])
        self.assertEqual(rc, 0, err)
        exit_page = os.path.join(tmp, q.EXIT_PAGE)
        self.assertIn("**The 72-hour soak today.**", slurp(exit_page))
        self.assertEqual(self.run_cli(["render", "--json", jf, "--root", tmp, "--check"])[0], 0)
        spit(exit_page, slurp(exit_page).replace("Status:", "State:", 1))
        rc, _, err = self.run_cli(["render", "--json", jf, "--root", tmp, "--check"])
        self.assertEqual(rc, 1)
        self.assertIn(q.EXIT_PAGE, err)

    def run_cli(self, argv):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                rc = q.main(argv)
            except SystemExit as e:
                rc = e.code
        return rc, out.getvalue(), err.getvalue()

    def test_write_inserts_above_the_old_block_then_check_passes(self):
        tmp = tempfile.mkdtemp()
        page = self._page(tmp)
        jf = os.path.join(tmp, "q.json")
        put(jf, self.rec)
        rc, _, err = self.run_cli(["render", "--json", jf, "--root", tmp, "--write"])
        self.assertEqual(rc, 0, err)
        text = slurp(page)
        self.assertLess(text.index(q.BEGIN), text.index("## Evidence for the 0.8.0 tree"))
        self.assertIn("## Evidence for the main at 1a2b3c4d tree", text)
        rc, _, _ = self.run_cli(["render", "--json", jf, "--root", tmp, "--check"])
        self.assertEqual(rc, 0)
        spit(page, text.replace("159 / 94", "159 / 95"))
        rc, _, err = self.run_cli(["render", "--json", jf, "--root", tmp, "--check"])
        self.assertEqual(rc, 1)

    def test_check_refuses_a_no_go_or_pr_labelled_record(self):
        tmp = tempfile.mkdtemp()
        self._page(tmp)
        jf = os.path.join(tmp, "q.json")
        for patch in ({"verdict": "NO-GO"}, {"label": "PR " + "#12 at dab1a51a"}):
            put(jf, dict(self.rec, **patch))
            rc, _, err = self.run_cli(["render", "--json", jf, "--root", tmp, "--check"])
            self.assertEqual(rc, 1)
            self.assertIn("must hold a GO record", err)

    def test_check_on_empty_block_without_json_skips(self):
        tmp = tempfile.mkdtemp()
        self._page(tmp, q.EMPTY_BLOCK)
        rc, out, _ = self.run_cli(["render", "--json", os.path.join(tmp, "none.json"),
                                   "--root", tmp, "--check"])
        self.assertEqual(rc, 0)
        self.assertEqual(len(out.strip().splitlines()), 1)
        self.assertIn("SKIP", out)

    def test_check_without_json_fails_on_a_non_empty_block(self):
        tmp = tempfile.mkdtemp()
        self._page(tmp, q.BEGIN + "\n## stale\n" + q.END)
        rc, _, _ = self.run_cli(["render", "--json", os.path.join(tmp, "none.json"),
                                 "--root", tmp, "--check"])
        self.assertEqual(rc, 1)

    def test_write_without_json_writes_the_empty_block(self):
        tmp = tempfile.mkdtemp()
        page = self._page(tmp)
        rc, _, _ = self.run_cli(["render", "--json", os.path.join(tmp, "none.json"),
                                 "--root", tmp, "--write"])
        self.assertEqual(rc, 0)
        self.assertIn(q.EMPTY_BLOCK, slurp(page))


class ArchiveTests(unittest.TestCase):
    def test_validate_archives(self):
        good = q.parse_archives(ARCHIVES)
        self.assertEqual(q.validate_archives(good), [])
        bad = q.parse_archives(ARCHIVES + ARCHIVES.split("\n", 5)[5].replace("soak-x", "soak-x"))
        self.assertTrue(any("duplicate" in p for p in q.validate_archives(bad)))
        bad_kind = q.parse_archives(ARCHIVES.replace('"soak"', '"drill"'))
        self.assertTrue(any("kind" in p for p in q.validate_archives(bad_kind)))
        bad_tree = q.parse_archives(ARCHIVES.replace("d" * 40, "abc"))
        self.assertTrue(any("tree" in p for p in q.validate_archives(bad_tree)))


    def test_validate_archives_rejects_bad_legs(self):
        def problems(extra, kind="soak"):
            rows = archives_toml(archive_row("r", kind, SHA, counts=COUNTS if kind == "corpus" else None))
            rows += extra
            return q.validate_archives(q.parse_archives(rows))
        self.assertEqual(problems('legs = ["srt", "whep"]\n'), [])
        self.assertTrue(any("legs" in p for p in problems('legs = ["srt"]\n', "corpus")))
        self.assertTrue(any("legs" in p for p in problems('legs = ["srt", "dash"]\n')))
        self.assertTrue(any("legs" in p for p in problems('legs = ["srt", "srt"]\n')))
        self.assertTrue(any("legs" in p for p in problems('legs = "srt"\n')))
        self.assertTrue(any("legs" in p for p in problems('legs = [1]\n')))

    def test_soak_row_requires_expected_duration(self):
        missing = q.parse_archives(ARCHIVES.replace("expected_duration_s = 259200\n", ""))
        self.assertTrue(any("expected_duration_s" in p for p in q.validate_archives(missing)))
        self.assertEqual(q.validate_archives(q.parse_archives(ARCHIVES)), [])
        zero = q.parse_archives(ARCHIVES.replace("259200", "0"))
        self.assertTrue(any("expected_duration_s" in p for p in q.validate_archives(zero)))
        bad = q.parse_archives(ARCHIVES.replace('kind = "soak"', 'kind = "stress"'))
        self.assertTrue(any("expected_duration_s" in p for p in q.validate_archives(bad)))

    def test_render_names_the_declared_soak_duration(self):
        rec = collect()
        line = [l for l in q.render_block(rec).splitlines() if "`soak-x`" in l][0]
        self.assertIn("259200 s declared", line)


COUNTS = dict(files=260, identical=258, changed_explained=2, invariant_failures=0,
              parse_failures=0, replay_files=12, replay_paths=5, replay_failed=0)


def archive_row(id, kind, tree, overall_pass=True, date="2026-10-18", duration=259200, counts=None, legs=None):
    row = ('\n[[archive]]\nid = "%s"\nkind = "%s"\ntree = "%s"\noverall_pass = %s\n'
           'note = "n"\ndate = "%s"\n' % (id, kind, tree, "true" if overall_pass else "false", date))
    if kind == "soak":
        row += "expected_duration_s = %d\n" % duration
        if legs is not None:
            row += "legs = [%s]\n" % ", ".join('"%s"' % l for l in legs)
    for k, v in (counts or {}).items():
        row += "%s = %s\n" % (k, str(v).lower() if isinstance(v, bool) else v)
    return row


FIVE = q.RELEASE_SOAK_LEGS


def archives_toml(*rows):
    return "schema_version = 0\n\n[corpus]\nexpected_files = %d\n" % COUNTS["files"] + "".join(rows)


NO_SOAK = archives_toml(archive_row("corpus-rel", "corpus", SHA))
# The older soak row comes LAST in the file: the newest archive is chosen by date.
RELEASE_OK = archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE),
                           archive_row("corpus-rel", "corpus", SHA, counts=COUNTS),
                           archive_row("soak-old", "soak", "d" * 40, False, "2026-10-04", 3600))
OLD_TREE = "e" * 40
SOAKED_EARLIER = archives_toml(archive_row("soak-rel", "soak", OLD_TREE, legs=FIVE),
                               archive_row("corpus-rel", "corpus", OLD_TREE, counts=COUNTS))


def git(cwd, *args):
    env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
               GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.test",
               GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.test")
    out = subprocess.run(["git", "-c", "commit.gpgsign=false", "-c", "core.hooksPath=/dev/null"]
                         + list(args), cwd=cwd, env=env, check=True, capture_output=True, text=True)
    return out.stdout.strip()


class CorpusCountTests(unittest.TestCase):
    def rows(self, kind="corpus", **counts):
        return q.parse_archives(archives_toml(archive_row("c", kind, SHA, counts=counts)))

    def test_all_eight_validate(self):
        self.assertEqual(q.validate_archives(self.rows(**COUNTS)), [])

    def test_partial_counts_name_the_missing_fields(self):
        probs = q.validate_archives(self.rows(files=3))
        self.assertTrue(any("replay_failed" in p and "identical" in p for p in probs), probs)

    def test_counts_belong_to_corpus_rows(self):
        probs = q.validate_archives(self.rows(kind="stress", replay_failed=0))
        self.assertTrue(any("belongs to corpus rows only" in p for p in probs), probs)

    def test_a_bool_or_negative_count_is_rejected(self):
        self.assertTrue(q.validate_archives(self.rows(**dict(COUNTS, files=True))))
        self.assertTrue(q.validate_archives(self.rows(**dict(COUNTS, files=-1))))

    def test_release_vm_exposes_note_and_render_prints_the_corpus_line(self):
        rows = archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE),
                             archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        rec = collect(release=True, archives=rows)
        self.assertEqual(rec["release_vm"]["corpus"]["note"], "n")
        text = q.render_block(rec)
        self.assertIn("Release-only checks", text)
        self.assertIn("real-capture corpus: n (files 260, identical 258, changed+explained 2, "
                      "invariant failures 0, parse failures 0; replay 12 files \u00d7 5 paths, 0 failed)", text)

    def test_non_release_record_prints_neither(self):
        text = q.render_block(collect())
        self.assertNotIn("Release-only checks", text)
        self.assertNotIn("real-capture corpus:", text)


class ReleaseModeTests(unittest.TestCase):
    def test_mid_cycle_record_is_go_without_a_soak(self):
        rec = collect()
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertIs(rec["release_vm"]["release_mode"], False)
        self.assertEqual(rec["release_vm"]["reasons"], [])
        with mock.patch.object(q, "changed_paths", side_effect=AssertionError("git called")):
            rec = collect(archives=NO_SOAK)
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertIsNone(rec["release_vm"]["soak"])

    def test_release_mode_requires_a_soak_archive_for_the_tree(self):
        rec = collect(release=True, archives=NO_SOAK)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("soak: no 72-hour soak archive for this tree (%s)" % SHA[:12], rec["verdict_reasons"])

    def test_release_mode_requires_the_five_soak_legs(self):
        corpus = archive_row("corpus-rel", "corpus", SHA, counts=COUNTS)
        for legs in (None, [l for l in FIVE if l != "whep"]):
            rec = collect(release=True, archives=archives_toml(
                archive_row("soak-rel", "soak", SHA, legs=legs), corpus))
            self.assertEqual(rec["verdict"], "NO-GO")
            self.assertTrue(any(r.startswith("soak: archive soak-rel ran legs") and "needs" in r
                                for r in rec["verdict_reasons"]), rec["verdict_reasons"])
        rec = collect(release=True, archives=archives_toml(
            archive_row("soak-rel", "soak", SHA, legs=FIVE), corpus))
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(rec["release_vm"]["soak"]["legs"], list(FIVE))
        self.assertIn("legs srt, rist, rtsp-publish, rtsp-publish-es, whep", q.render_block(rec))

    def test_release_mode_passes_with_matching_soak_and_corpus(self):
        with mock.patch.object(q, "changed_paths", side_effect=AssertionError("git called")):
            rec = collect(release=True, archives=RELEASE_OK)
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertIs(rec["release_vm"]["release_mode"], True)
        self.assertEqual(rec["release_vm"]["soak"]["id"], "soak-rel")
        self.assertEqual(rec["release_vm"]["corpus"]["id"], "corpus-rel")
        self.assertEqual(rec["release_vm"]["reasons"], [])

    def test_release_mode_accepts_a_soaked_tree_with_only_doc_changes_since(self):
        with mock.patch.object(q, "changed_paths", return_value=[]) as cp, \
                mock.patch.object(q, "changed_release_documents", return_value=["CHANGELOG.md"]) as cd:
            rec = collect(release=True, archives=SOAKED_EARLIER)
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(cp.call_args.args[1:], (OLD_TREE, SHA))
        self.assertEqual(cd.call_args.args[1:], (OLD_TREE, SHA))
        self.assertEqual(rec["release_vm"]["soak"]["changed_paths"], 0)
        self.assertEqual(rec["release_vm"]["soak"]["equivalence"]["changed_release_documents"], ["CHANGELOG.md"])

    def test_release_mode_rejects_a_soaked_tree_with_code_changes_since(self):
        with mock.patch.object(q, "changed_paths", return_value=["crates/tst-core/src/lib.rs"]):
            rec = collect(release=True, archives=SOAKED_EARLIER)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn(
            "soak: archive soak-rel is superseded by 1 changed path(s) outside the release documents",
            rec["verdict_reasons"])

    def test_release_mode_rejects_a_failed_or_short_soak(self):
        failed = archives_toml(archive_row("soak-rel", "soak", SHA, overall_pass=False, legs=FIVE),
                               archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        rec = collect(release=True, archives=failed)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("soak: archive soak-rel recorded overall_pass=false", rec["verdict_reasons"])
        short = archives_toml(archive_row("soak-rel", "soak", SHA, duration=3600, legs=FIVE),
                              archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        rec = collect(release=True, archives=short)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("soak: archive soak-rel declares 3600 s, not 259200", rec["verdict_reasons"])

    def test_release_mode_requires_the_corpus_archive(self):
        rec = collect(release=True, archives=archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE)))
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("corpus: no corpus archive for the soaked tree %s" % SHA[:12], rec["verdict_reasons"])
        failed = archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE),
                               archive_row("corpus-rel", "corpus", SHA, overall_pass=False))
        rec = collect(release=True, archives=failed)
        self.assertIn("corpus: archive corpus-rel recorded overall_pass=false", rec["verdict_reasons"])

    def test_release_mode_requires_the_counts_on_the_cited_corpus_row(self):
        bare = archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE),
                             archive_row("corpus-rel", "corpus", SHA))
        rec = collect(release=True, archives=bare)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("corpus: archive corpus-rel records no counts; the release gate needs the "
                      "eight the corpus tool's archive row carries", rec["verdict_reasons"])
        # A legacy row without counts stays valid for a tree no release cites.
        self.assertEqual(q.validate_archives(q.parse_archives(bare)), [])
        mid = collect(archives=bare)
        self.assertEqual(mid["verdict"], "GO", mid["verdict_reasons"])

    def test_unresolvable_soak_tree_is_no_go(self):
        with mock.patch.object(q, "changed_paths", side_effect=q.QualificationError("fatal: bad object")):
            rec = collect(release=True, archives=SOAKED_EARLIER)
            self.assertEqual(rec["verdict"], "NO-GO")
            self.assertIn("soak: git could not resolve %s: fatal: bad object" % OLD_TREE, rec["verdict_reasons"])
            mid = collect(archives=SOAKED_EARLIER)
        self.assertEqual(mid["verdict"], "GO", mid["verdict_reasons"])
        self.assertEqual(mid["release_vm"]["reasons"], [])

    def test_changed_paths_runs_git(self):
        with tempfile.TemporaryDirectory() as d:
            git(d, "init", "-q", "-b", "main")
            spit(os.path.join(d, "README"), "a\n")
            git(d, "add", "README")
            git(d, "commit", "-q", "-m", "A")
            a = git(d, "rev-parse", "HEAD")
            os.makedirs(os.path.join(d, "crates"))
            spit(os.path.join(d, "crates/x.rs"), "fn x() {}\n")
            git(d, "add", "crates/x.rs")
            git(d, "commit", "-q", "-m", "B")
            b = git(d, "rev-parse", "HEAD")
            git(d, "checkout", "-q", a)
            os.makedirs(os.path.join(d, "docs"))
            spit(os.path.join(d, "docs/y.md"), "y\n")
            git(d, "add", "docs/y.md")
            git(d, "commit", "-q", "-m", "C")
            c = git(d, "rev-parse", "HEAD")
            self.assertEqual(q.changed_paths(d, a, b), ["crates/x.rs"])
            self.assertEqual(q.changed_paths(d, a, c), [])
            with self.assertRaises(q.QualificationError) as e:
                q.changed_paths(d, "f" * 40, b)
            self.assertIn("bad object", str(e.exception))

    def test_changed_paths_compares_the_whole_tree(self):
        with tempfile.TemporaryDirectory() as d:
            git(d, "init", "-q", "-b", "main")
            for f in ("Cargo.lock", "rust-toolchain.toml", "CHANGELOG.md"):
                spit(os.path.join(d, f), "1\n")
            os.makedirs(os.path.join(d, "docs"))
            spit(os.path.join(d, "docs/p.md"), "1\n")
            git(d, "add", ".")
            git(d, "commit", "-q", "-m", "A")
            a = git(d, "rev-parse", "HEAD")

            def commit_change(f, text="2\n"):
                git(d, "checkout", "-q", a)
                os.makedirs(os.path.dirname(os.path.join(d, f)) or d, exist_ok=True)
                spit(os.path.join(d, f), text)
                git(d, "add", f)
                git(d, "commit", "-q", "-m", "x")
                return git(d, "rev-parse", "HEAD")
            self.assertEqual(q.changed_paths(d, a, commit_change("Cargo.lock")), ["Cargo.lock"])
            self.assertEqual(q.changed_paths(d, a, commit_change("rust-toolchain.toml")),
                             ["rust-toolchain.toml"])
            self.assertEqual(q.changed_paths(d, a, commit_change("CHANGELOG.md")), [])
            self.assertEqual(q.changed_paths(d, a, commit_change("docs/p.md")), [])
            self.assertEqual(q.changed_paths(d, a, commit_change("tests/coverage/evidence-archives.toml")), [])
            git(d, "checkout", "-q", a)
            git(d, "update-index", "--add", "--cacheinfo", "160000,%s,vendor/sub" % a)
            git(d, "commit", "-q", "-m", "gitlink")
            self.assertEqual(q.changed_paths(d, a, git(d, "rev-parse", "HEAD")), ["vendor/sub"])

    def test_changed_paths_counts_the_whep_player_as_code(self):
        with tempfile.TemporaryDirectory() as d:
            git(d, "init", "-q", "-b", "main")
            os.makedirs(os.path.join(d, "docs/webrtc/test"))
            for f in ("docs/webrtc/whep-player.js", "docs/webrtc/test/player.test.js"):
                spit(os.path.join(d, f), "1\n")
            git(d, "add", ".")
            git(d, "commit", "-q", "-m", "A")
            a = git(d, "rev-parse", "HEAD")
            for f in ("docs/webrtc/whep-player.js", "docs/webrtc/test/player.test.js"):
                spit(os.path.join(d, f), "2\n")
            git(d, "commit", "-q", "-am", "B")
            b = git(d, "rev-parse", "HEAD")
            self.assertEqual(sorted(q.changed_paths(d, a, b)),
                             ["docs/webrtc/test/player.test.js", "docs/webrtc/whep-player.js"])
        with mock.patch.object(q, "changed_paths", return_value=["docs/webrtc/whep-player.js"]):
            rec = collect(release=True, archives=SOAKED_EARLIER)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn(
            "soak: archive soak-rel is superseded by 1 changed path(s) outside the release documents",
            rec["verdict_reasons"])

    def test_changed_paths_allows_markdown_under_docs_webrtc(self):
        with tempfile.TemporaryDirectory() as d:
            git(d, "init", "-q", "-b", "main")
            os.makedirs(os.path.join(d, "docs/webrtc"))
            spit(os.path.join(d, "docs/webrtc/README.md"), "1\n")
            git(d, "add", ".")
            git(d, "commit", "-q", "-m", "A")
            a = git(d, "rev-parse", "HEAD")
            spit(os.path.join(d, "docs/webrtc/README.md"), "2\n")
            git(d, "commit", "-q", "-am", "B")
            self.assertEqual(q.changed_paths(d, a, git(d, "rev-parse", "HEAD")), [])

    def test_changed_paths_sees_a_rename_into_docs(self):
        with tempfile.TemporaryDirectory() as d:
            git(d, "init", "-q", "-b", "main")
            os.makedirs(os.path.join(d, "crates"))
            os.makedirs(os.path.join(d, "docs"))
            spit(os.path.join(d, "crates/x.rs"), "fn x() {}\n" * 20)
            git(d, "add", ".")
            git(d, "commit", "-q", "-m", "A")
            a = git(d, "rev-parse", "HEAD")
            git(d, "mv", "crates/x.rs", "docs/x.rs")
            git(d, "commit", "-q", "-m", "move")
            self.assertEqual(q.changed_paths(d, a, git(d, "rev-parse", "HEAD")), ["crates/x.rs"])
        with mock.patch.object(q, "changed_paths", return_value=["crates/x.rs"]):
            rec = collect(release=True, archives=SOAKED_EARLIER)
        self.assertEqual(rec["verdict"], "NO-GO")

    def test_release_mode_rejects_a_one_path_corpus_row(self):
        for bad in (dict(COUNTS, replay_paths=1), dict(COUNTS, replay_files=0),
                    dict(COUNTS, replay_failed=1)):
            rows = archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE),
                                 archive_row("corpus-rel", "corpus", SHA, counts=bad))
            rec = collect(release=True, archives=rows)
            self.assertEqual(rec["verdict"], "NO-GO", bad)
            self.assertTrue(any(r.startswith("corpus: archive corpus-rel") for r in rec["verdict_reasons"]), bad)

    def test_a_newer_failed_soak_on_the_same_tree_is_a_reason(self):
        for dur in (3600, 259200):
            rows = archives_toml(archive_row("soak-72", "soak", SHA, date="2026-10-17", legs=FIVE),
                                 archive_row("soak-bad", "soak", SHA, date="2026-10-18",
                                             duration=dur, overall_pass=False),
                                 archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
            rec = collect(release=True, archives=rows)
            self.assertEqual(rec["verdict"], "NO-GO", dur)
            # A newer failed 72-hour row is itself the chosen archive.
            want = ("soak: archive soak-bad, a newer run on the same tree, recorded overall_pass=false"
                    if dur == 3600 else "soak: archive soak-bad recorded overall_pass=false")
            self.assertIn(want, rec["verdict_reasons"])

    def test_a_newer_short_soak_does_not_hide_the_72_hour_one(self):
        rows = archives_toml(archive_row("soak-72", "soak", SHA, date="2026-10-17", legs=FIVE),
                             archive_row("soak-1h", "soak", SHA, date="2026-10-18", duration=3600),
                             archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        rec = collect(release=True, archives=rows)
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(rec["release_vm"]["soak"]["id"], "soak-72")

    def test_an_older_standing_72_hour_soak_wins_over_a_newer_superseded_one(self):
        newer, older = "a" * 40, "b" * 40
        rows = archives_toml(archive_row("soak-new", "soak", newer, date="2026-10-18", legs=FIVE),
                             archive_row("soak-old", "soak", older, date="2026-10-10", legs=FIVE),
                             archive_row("corpus-old", "corpus", older, counts=COUNTS))
        diffs = {newer: ["crates/tst-core/src/lib.rs"], older: []}
        with mock.patch.object(q, "changed_paths", side_effect=lambda root, tree, sha: diffs[tree]), \
                mock.patch.object(q, "changed_release_documents", return_value=[]):
            rec = collect(release=True, archives=rows)
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(rec["release_vm"]["soak"]["id"], "soak-old")
        self.assertEqual(rec["release_vm"]["corpus"]["id"], "corpus-old")

    def test_only_short_soaks_name_the_declared_duration(self):
        rows = archives_toml(archive_row("soak-1h", "soak", SHA, duration=3600),
                             archive_row("soak-2h", "soak", SHA, date="2026-10-10", duration=7200),
                             archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        rec = collect(release=True, archives=rows)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("soak: archive soak-1h declares 3600 s, not 259200", rec["verdict_reasons"])

    def test_release_mode_rejects_a_corpus_row_with_fewer_files_than_declared(self):
        small = dict(COUNTS, files=3)
        rows = archives_toml(archive_row("soak-rel", "soak", SHA, legs=FIVE),
                             archive_row("corpus-rel", "corpus", SHA, counts=small))
        rec = collect(release=True, archives=rows)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertIn("corpus: archive corpus-rel swept 3 files; the release corpus has %d" % COUNTS["files"],
                      rec["verdict_reasons"])

    def test_archives_requires_the_corpus_expectation(self):
        text = RELEASE_OK.replace("[corpus]\nexpected_files = %d\n" % COUNTS["files"], "")
        self.assertIsNone(q.parse_corpus_expectation(text))
        self.assertEqual(q.parse_corpus_expectation(RELEASE_OK), COUNTS["files"])
        with tempfile.TemporaryDirectory() as d:
            put(os.path.join(d, "tests/coverage/evidence-archives.toml"), text)
            with self.assertRaises(SystemExit) as e:
                q.load_archives(d)
            self.assertEqual(e.exception.code, 1)

    def test_archives_rejects_a_malformed_corpus_expectation(self):
        for bad in ("0", "true", "2.5", '"253"'):
            text = RELEASE_OK.replace("expected_files = %d" % COUNTS["files"], "expected_files = " + bad)
            with tempfile.TemporaryDirectory() as d:
                put(os.path.join(d, "tests/coverage/evidence-archives.toml"), text)
                with self.assertRaises(SystemExit) as e, contextlib.redirect_stderr(io.StringIO()) as err:
                    q.load_archives(d)
                self.assertEqual(e.exception.code, 1, bad)
                self.assertIn("[corpus] expected_files", err.getvalue(), bad)

    def test_a_malformed_sha_never_reaches_git(self):
        rows = q.parse_archives(SOAKED_EARLIER)
        with mock.patch.object(q, "changed_paths", side_effect=AssertionError("git called")):
            for bad in ("HEAD", "--output=/tmp/x", SHA.upper()):
                with self.assertRaises(q.QualificationError) as e:
                    q.release_vm("/nonexistent", bad, rows, True, COUNTS["files"])
                self.assertIn("source.sha", str(e.exception))
            with self.assertRaises(SystemExit) as e:
                collect(mutate=lambda f: f["context.json"].update(
                    source=dict(CONTEXT["source"], sha="HEAD")), archives=SOAKED_EARLIER)
            self.assertEqual(e.exception.code, 2)

    def test_a_malformed_sha_fails_the_same_way_without_a_soak(self):
        with self.assertRaises(SystemExit) as e:
            collect(mutate=lambda f: f["context.json"].update(
                source=dict(CONTEXT["source"], sha="HEAD")), archives=NO_SOAK)
        self.assertEqual(e.exception.code, 2)

    def test_render_release_gate_lines(self):
        with mock.patch.object(q, "changed_paths", side_effect=AssertionError("git called")):
            rel = q.render_block(collect(release=True, archives=RELEASE_OK))
            mid = q.render_block(collect(archives=NO_SOAK))
        self.assertIn("Release gate: soak soak-rel (tree %s, 259200 s, legs srt, rist, rtsp-publish, "
                      "rtsp-publish-es, whep, PASS), corpus corpus-rel (PASS)."
                      % SHA[:12], rel)
        self.assertIn("Release gate: not applied (not a release qualification); "
                      "newest soak archive for this tree: none.", mid)
        with mock.patch.object(q, "changed_paths", return_value=["crates/a.rs", "bindings/b.rs"]):
            sup = q.render_block(collect(archives=SOAKED_EARLIER))
        self.assertIn("newest soak archive for this tree: soak-rel (superseded by 2 code changes).", sup)
        old = collect(archives=NO_SOAK)
        del old["release_vm"]
        self.assertIn("Release gate: not applied (not a release qualification); "
                      "newest soak archive for this tree: none.", q.render_block(old))


WAIVER = {"verdicts": ["a", "b"], "reason": "harness", "reviewer": "maintainer", "date": "2026-10-28",
          "fixed_in": "0.9.1"}
# Parsed archive rows, as load_archives returns them.
ROW = {"id": "soak-x", "kind": "soak", "tree": SHA, "overall_pass": True, "note": "n",
       "date": "2026-10-27", "expected_duration_s": 3600}
SOAK72 = dict(ROW, id="soak-rel", expected_duration_s=259200, legs=list(FIVE))
CORPUS = dict(ROW, id="corpus-rel", kind="corpus", expected_duration_s=None,
              **dict(COUNTS, files=251))


class WaiverTests(unittest.TestCase):
    def setUp(self):
        self.root = tempfile.mkdtemp()
        self.sha = SHA

    def record_with(self, **patch):
        return dict(collect(), **patch)

    def test_a_waiver_is_accepted_only_on_a_failed_row(self):
        ok = dict(ROW, overall_pass=False, waiver=WAIVER)
        self.assertEqual(q.validate_archives([ok]), [])
        bad = dict(ROW, overall_pass=True, waiver=WAIVER)
        self.assertIn("a waiver belongs to a failed row only", q.validate_archives([bad])[0])
        short = dict(ROW, overall_pass=False, waiver={"reason": "x"})
        self.assertIn("waiver needs", q.validate_archives([short])[0])

    def test_a_waiver_needs_a_dated_non_empty_verdict_list(self):
        for bad in (dict(WAIVER, verdicts=[]), dict(WAIVER, verdicts=[1]), dict(WAIVER, date="28 Oct"),
                    dict(WAIVER, fixed_in=91)):
            row = dict(ROW, overall_pass=False, waiver=bad)
            self.assertEqual(q.validate_archives([row]),
                             ["soak-x: waiver needs verdicts, reason, reviewer, date and fixed_in"], bad)

    def test_release_mode_refuses_a_waived_soak_without_allow_waiver(self):
        rows = [dict(SOAK72, overall_pass=False, waiver=WAIVER), CORPUS]
        gate = q.release_vm(self.root, self.sha, rows, True, 251)
        self.assertEqual(gate["soak"]["disposition"], "waived")
        self.assertTrue(any("needs a raw pass" in r for r in gate["reasons"]))
        self.assertIn("soak: archive soak-rel is waived (2 verdicts), and a release needs a raw pass",
                      gate["reasons"])

    def test_a_mid_cycle_dispatch_may_allow_a_waiver(self):
        rows = [dict(SOAK72, overall_pass=False, waiver=WAIVER), CORPUS]
        gate = q.release_vm(self.root, self.sha, rows, True, 251, allow_waiver=True)
        self.assertEqual(gate["reasons"], [])
        self.assertEqual(gate["disposition"], "waived")

    def test_a_waived_corpus_row_needs_a_raw_pass_too(self):
        rows = [SOAK72, dict(CORPUS, overall_pass=False, waiver=WAIVER)]
        gate = q.release_vm(self.root, self.sha, rows, True, 251)
        self.assertEqual(gate["corpus"]["disposition"], "waived")
        self.assertEqual(gate["reasons"],
                         ["corpus: archive corpus-rel is waived (2 verdicts), and a release needs a raw pass"])
        self.assertEqual(q.release_vm(self.root, self.sha, rows, True, 251, allow_waiver=True)["reasons"], [])

    def test_an_unwaived_failure_is_the_worse_disposition(self):
        rows = [dict(SOAK72, overall_pass=False, waiver=WAIVER), dict(CORPUS, overall_pass=False)]
        gate = q.release_vm(self.root, self.sha, rows, True, 251, allow_waiver=True)
        self.assertEqual(gate["corpus"]["disposition"], "fail")
        self.assertEqual(gate["disposition"], "fail")
        self.assertEqual(q.release_vm(self.root, self.sha, [SOAK72, CORPUS], True, 251)["disposition"], "pass")

    def test_collect_reads_the_waiver_and_the_flag(self):
        rows = archives_toml(archive_row("soak-rel", "soak", SHA, overall_pass=False, legs=FIVE)
                             + '[archive.waiver]\nverdicts = ["a", "b"]\nreason = "harness"\n'
                               'reviewer = "maintainer"\ndate = "2026-10-28"\nfixed_in = "0.9.1"\n',
                             archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        tmp = tempfile.mkdtemp()
        art, root = build(tmp, archives=rows)
        rec = q.collect(art, root, release=True)
        self.assertEqual(rec["verdict"], "NO-GO")
        self.assertEqual(rec["archives"][0]["waiver"], WAIVER)
        rec = q.collect(art, root, release=True, allow_waiver=True)
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(rec["release_vm"]["disposition"], "waived")
        self.assertIn("Release gate: soak soak-rel (tree %s, 259200 s, legs srt, rist, rtsp-publish, "
                      "rtsp-publish-es, whep, FAIL, waived)" % SHA[:12], q.render_block(rec))

    def test_render_prints_the_waiver_on_the_archive_line(self):
        rec = self.record_with(archives=[dict(ROW, overall_pass=False, waiver=WAIVER)])
        self.assertIn("overall_pass=false, waived: 2 verdict(s) by maintainer on 2026-10-28, fixed in 0.9.1",
                      q.render_block(rec))


class ExitCriteriaTests(unittest.TestCase):
    def release_record(self, archives):
        return collect(release=True, archives=archives)

    def test_exit_criteria_block_says_not_met_with_the_gates_first_reason(self):
        rows = archives_toml(archive_row("soak-rel", "soak", SHA, overall_pass=False, legs=FIVE),
                             archive_row("corpus-rel", "corpus", SHA, counts=COUNTS))
        rec = self.release_record(rows)
        text = q.render_exit_criteria(rec, rec["archives"], [])
        self.assertIn("**The 72-hour soak today.**", text)
        self.assertIn("Status: not met: soak: archive soak-rel recorded overall_pass=false.", text)
        self.assertIn("**The corpus today.**", text)
        self.assertIn("`corpus-rel`", text)

    def test_exit_criteria_block_says_met_for_a_raw_pass(self):
        rec = self.release_record(RELEASE_OK)
        text = q.render_exit_criteria(rec, rec["archives"], [])
        soak = [l for l in text.splitlines() if l.startswith("**The 72-hour soak today.**")][0]
        self.assertIn("`soak-rel`", soak)
        self.assertIn("harness verdict PASS", soak)
        self.assertTrue(soak.endswith(" Status: met."), soak)
        corpus = [l for l in text.splitlines() if l.startswith("**The corpus today.**")][0]
        self.assertTrue(corpus.endswith(" Status: met."), corpus)

    def test_a_waiver_recorded_after_collection_reads_as_met_by_waiver(self):
        rec = self.release_record(RELEASE_OK)
        archives = [dict(a, overall_pass=False, waiver=dict(WAIVER, fixed_in="9.9.1"))
                    if a["id"] == "soak-rel" else a for a in rec["archives"]]
        text = q.render_exit_criteria(rec, archives, [], release="9.9.0")
        self.assertIn("harness verdict FAIL, waived: 2 verdict(s) by maintainer on 2026-10-28, "
                      "fixed in 9.9.1", text)
        self.assertIn("Status: met by waiver (9.9.0); a raw pass is required from 9.9.1.", text)

    def test_a_mid_cycle_record_names_the_newest_archives(self):
        rec = collect(archives=RELEASE_OK)
        text = q.render_exit_criteria(rec, rec["archives"], [])
        self.assertIn("no release qualification is recorded on this tree; the newest soak archive is "
                      "`soak-rel`", text)
        self.assertIn("the newest corpus archive is `corpus-rel`", text)
        none = q.render_exit_criteria(None, rec["archives"], [])
        self.assertIn("no release qualification is recorded on this tree", none)

    def test_the_arm64_subset_is_met_only_by_a_clean_aarch64_census(self):
        rec = self.release_record(RELEASE_OK)
        self.assertIn("**The ARM64 subset today.** Status: open: the interop workflow has no linux-aarch64 census.",
                      q.render_exit_criteria(rec, rec["archives"], []))
        arm = {"arch": "linux-aarch64", "shape": "subset-arm64", "total": 80, "pass": 70, "fail": 0,
               "skipped": 10, "stale": 0}
        text = q.render_exit_criteria(rec, rec["archives"], [arm])
        self.assertIn("**The ARM64 subset today.** The linux-aarch64 census (subset-arm64) reads "
                      "80 / 70 / 0 / 10: total, pass, fail, skipped. Status: met.", text)
        text = q.render_exit_criteria(rec, rec["archives"], [dict(arm, fail=1)])
        self.assertIn("Status: open: the linux-aarch64 census has 1 failure(s).", text)


class TagAndEquivalenceTests(unittest.TestCase):
    def test_collect_records_the_tag_and_the_changed_release_documents(self):
        with tempfile.TemporaryDirectory() as d:
            root = os.path.join(d, "root")
            os.makedirs(os.path.join(root, "docs"))
            git(root, "init", "-q", "-b", "main")
            for f in ("CHANGELOG.md", "docs/x.md", "README"):
                spit(os.path.join(root, f), "1\n")
            git(root, "add", ".")
            git(root, "commit", "-q", "-m", "A")
            soaked = git(root, "rev-parse", "HEAD")
            for f in ("CHANGELOG.md", "docs/x.md"):
                spit(os.path.join(root, f), "2\n")
            git(root, "commit", "-q", "-am", "B")
            qualified = git(root, "rev-parse", "HEAD")
            # The archive table and the fuzz manifest stay untracked, so the diff is the two documents.
            art, _ = build(os.path.join(d, "a"), lambda f: f["context.json"].update(
                source=dict(CONTEXT["source"], sha=qualified)))
            put(os.path.join(root, "tests/coverage/fuzz-targets.toml"), FUZZ_TOML)
            put(os.path.join(root, "tests/coverage/evidence-archives.toml"), archives_toml(
                archive_row("soak-rel", "soak", soaked, legs=FIVE),
                archive_row("corpus-rel", "corpus", soaked, counts=COUNTS)))
            rec = q.collect(art, root, release=True, tag="v9.9.9")
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        self.assertEqual(rec["source"]["tag"], "v9.9.9")
        self.assertEqual(rec["label"], "tag v9.9.9")
        self.assertEqual(rec["release_vm"]["soak"]["equivalence"],
                         {"soaked": soaked, "qualified": qualified,
                          "changed_release_documents": ["CHANGELOG.md", "docs/x.md"]})

    def test_without_a_tag_the_source_has_none_and_a_same_tree_soak_changed_nothing(self):
        with mock.patch.object(q, "changed_paths", side_effect=AssertionError("git called")):
            rec = collect(release=True, archives=RELEASE_OK)
        self.assertNotIn("tag", rec["source"])
        self.assertEqual(rec["label"], CONTEXT["label"])
        self.assertEqual(rec["release_vm"]["soak"]["equivalence"],
                         {"soaked": SHA, "qualified": SHA, "changed_release_documents": []})


ARM_CENSUS = {"arch": "linux-aarch64", "shape": "subset", "total": 70, "pass": 60, "fail": 2,
              "expected_unsupported": 4, "skipped": 4, "stale": 0}


class ArmCensusTests(unittest.TestCase):
    def test_interop_accepts_a_second_census_and_go_reads_the_x86_64_one(self):
        rec = collect(lambda f: f.__setitem__("interop-census-aarch64.json", dict(ARM_CENSUS)))
        self.assertEqual(rec["verdict"], "GO", rec["verdict_reasons"])
        censuses = rec["interop"]["censuses"]
        self.assertEqual([c["arch"] for c in censuses], ["linux-x86_64", "linux-aarch64"])
        self.assertEqual(censuses[0]["total"], 159)
        self.assertEqual(censuses[1], ARM_CENSUS)
        self.assertIn("The linux-aarch64 census (subset) reads 70 / 60 / 2 / 4 / 4: total, pass, fail, "
                      "expected-unsupported, skipped. Stale expectations: 0.", q.render_block(rec))
        # A failing x86_64 census still decides the verdict.
        def m(f):
            f["interop-census-aarch64.json"] = dict(ARM_CENSUS, fail=0)
            f["interop-evidence/results.json"]["summary"]["fail"] = 1
        self.assertEqual(collect(m)["verdict"], "NO-GO")

    def test_without_an_aarch64_census_only_the_x86_64_one_is_recorded(self):
        rec = collect()
        self.assertEqual([c["arch"] for c in rec["interop"]["censuses"]], ["linux-x86_64"])
        self.assertNotIn("linux-aarch64 census", q.render_block(rec))

    def test_a_malformed_aarch64_census_fails_the_collect(self):
        with self.assertRaises(SystemExit) as e, contextlib.redirect_stderr(io.StringIO()):
            collect(lambda f: f.__setitem__("interop-census-aarch64.json", {"arch": "linux-aarch64"}))
        self.assertEqual(e.exception.code, 2)


class ChecksumTests(unittest.TestCase):
    def test_checksums_cover_the_files_not_the_sums_file(self):
        tmp = tempfile.mkdtemp()
        put(os.path.join(tmp, "a.txt"), "hello")
        q.write_checksums(tmp)
        lines = slurp(os.path.join(tmp, "SHA256SUMS")).splitlines()
        self.assertEqual(len(lines), 1)
        self.assertTrue(lines[0].endswith("  a.txt"))
        q.write_checksums(tmp)  # rerun: SHA256SUMS itself is never listed
        self.assertEqual(len(slurp(os.path.join(tmp, "SHA256SUMS")).splitlines()), 1)


if __name__ == "__main__":
    unittest.main()
