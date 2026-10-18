"""Unit tests for scripts/gen/qualification.py.

Run:  python3 -I -m unittest scripts/gen/test_qualification.py
"""
import contextlib
import io
import json
import os
import sys
import tempfile
import unittest

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
       "previous_source": "built", "abidiff_exit": 4, "verdict": "compatible"}
SEMVER = [{"crate": "tst-core", "exit": 0, "failed_checks": 0, "items": 0, "report": "r", "analysed": True}]
SAN_JOBS = ["asan", "asan-native", "tsan", "tsan-native"]

FUZZ_TOML = "schema_version = 0\n" + "".join(
    '\n[[target]]\ncrate = "tst-core"\nname = "t%d"\n' % i for i in range(37))
ARCHIVES = """\
schema_version = 0

[[archive]]
id = "soak-x"
kind = "soak"
tree = "%s"
overall_pass = false
note = "The SRT leg passed; the RIST leg failed attribution."
date = "2026-10-04"
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


def build(tmp, mutate=None):
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
    put(os.path.join(root, "tests/coverage/evidence-archives.toml"), ARCHIVES)
    return art, root


def collect(mutate=None):
    tmp = tempfile.mkdtemp()
    art, root = build(tmp, mutate)
    return q.collect(art, root)


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
        return page

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
        bad = q.parse_archives(ARCHIVES + ARCHIVES.split("\n", 2)[2].replace("soak-x", "soak-x"))
        self.assertTrue(any("duplicate" in p for p in q.validate_archives(bad)))
        bad_kind = q.parse_archives(ARCHIVES.replace('"soak"', '"drill"'))
        self.assertTrue(any("kind" in p for p in q.validate_archives(bad_kind)))
        bad_tree = q.parse_archives(ARCHIVES.replace("d" * 40, "abc"))
        self.assertTrue(any("tree" in p for p in q.validate_archives(bad_tree)))


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
