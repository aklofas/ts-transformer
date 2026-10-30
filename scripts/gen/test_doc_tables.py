"""Unit tests for scripts/gen/doc_tables.py.

Run:  python3 -m unittest scripts/gen/test_doc_tables.py
"""
import contextlib
import io
import os
import sys
import unittest

if sys.version_info < (3, 11):
    raise unittest.SkipTest("doc_tables needs Python >= 3.11 (tomllib)")

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import doc_tables as dt  # noqa: E402

CI = """\
jobs:
  build:
    name: build (${{ matrix.name }})
    strategy:
      matrix:
        # comment
        include:
          - name: linux-x86_64
            runner: ubuntu-latest
            continue: false
            phase: all
          - name: windows-msvc
            platform: windows-msvc
            runner: windows-latest
            continue: false
            phase: default
          - name: windows-msvc-features
            platform: windows-msvc
            runner: windows-latest
            continue: false
            phase: features
    steps:
      - name: not a leg
        runner: nope
  test-linux:
    strategy:
      matrix:
        shard: [1, 2, 3]
        pass: [default, no-default, all-features]
        include:
          - name: linux-x86_64
            runner: ubuntu-latest
            pass: default
            flags: ''
          - name: linux-x86_64
            runner: ubuntu-latest
            pass: no-default
            flags: --no-default-features
          - name: linux-x86_64
            runner: ubuntu-latest
            pass: all-features
            flags: --all-features
    steps:
      - name: not a leg
        runner: nope
  test-platforms:
    strategy:
      matrix:
        shard: [1, 2]
        name: [windows-msvc]
        include:
          - name: windows-msvc
            runner: windows-latest
            pass: all-features
            flags: --all-features
    steps:
      - run: x
  test-main-extra:
    strategy:
      matrix:
        shard: [1, 2]
        name: [windows-msvc]
        pass: [default, no-default]
        include:
          - name: windows-msvc
            runner: windows-latest
            pass: default
            flags: ''
          - name: windows-msvc
            runner: windows-latest
            pass: no-default
            flags: --no-default-features
    steps:
      - run: x
  other:
    steps:
      - name: also not a leg
"""

SAN = """\
name: sanitizers
on:
  schedule:
    # why
    - cron: '0 3 * * *'
jobs:
  asan:
    runs-on: ubuntu-latest
    steps:
      - name: x
  tsan:
    runs-on: ubuntu-latest
"""

FUZZ = """\
schema_version = 0

[[target]]
crate = "tst-core"
name = "a"

[[target]]
crate = "tst-core"
name = "b"

[[target]]
crate = "tst-rtp"
name = "c"
"""

META = {
    "packages": [
        {
            "name": "tstrans-srt-sys",
            "manifest_path": "/w/crates/srt-sys/Cargo.toml",
            "features": {"default": ["mbedtls"], "mbedtls": []},
        },
        {
            "name": "tst-core",
            "manifest_path": "/w/crates/tst-core/Cargo.toml",
            "features": {
                "default": ["std", "file"],
                "std": ["file", "dep:x"],
                "file": ["std"],
                "tls": ["std"],
            },
        },
    ]
}


class Parsers(unittest.TestCase):
    def test_build_legs_only_from_the_build_matrix(self):
        legs = dt.parse_build_legs(CI)
        self.assertEqual(
            [(l["name"], l["runner"], l["phase"]) for l in legs],
            [
                ("linux-x86_64", "ubuntu-latest", "all"),
                ("windows-msvc", "windows-latest", "default"),
                ("windows-msvc-features", "windows-latest", "features"),
            ],
        )
        self.assertEqual(
            [l["platform"] for l in legs],
            ["linux-x86_64", "windows-msvc", "windows-msvc"],
        )

    def test_test_legs_from_the_three_test_jobs(self):
        legs = dt.parse_test_legs(CI)
        self.assertEqual(
            [(l["job"], l["name"], l["pass"], l["shards"]) for l in legs],
            [
                ("test-linux", "linux-x86_64", "default", 3),
                ("test-linux", "linux-x86_64", "no-default", 3),
                ("test-linux", "linux-x86_64", "all-features", 3),
                ("test-platforms", "windows-msvc", "all-features", 2),
                ("test-main-extra", "windows-msvc", "default", 2),
                ("test-main-extra", "windows-msvc", "no-default", 2),
            ],
        )

    @staticmethod
    def _test_ci(shard, *legs):
        return ("jobs:\n  test-linux:\n    strategy:\n      matrix:\n" + shard
                + "        include:\n" + "".join(legs) + "    steps:\n      - run: x\n")

    def test_test_leg_without_pass_dies(self):
        err = io.StringIO()
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(err):
            dt.parse_test_legs(self._test_ci(
                "        shard: [1, 2]\n",
                "          - name: a\n            runner: r\n"))
        self.assertIn("has no `pass`", err.getvalue())

    def test_test_job_without_shard_axis_dies(self):
        err = io.StringIO()
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(err):
            dt.parse_test_legs(self._test_ci(
                "",
                "          - name: a\n            runner: r\n            pass: p\n"))
        self.assertIn("has no `shard:` axis", err.getvalue())

    @staticmethod
    def _ci(*legs):
        return "jobs:\n  build:\n    strategy:\n      matrix:\n        include:\n" + "".join(legs) + "    steps:\n      - run: x\n"

    def test_leg_with_runner_before_name_is_read(self):
        legs = dt.parse_build_legs(self._ci(
            "          - name: a\n            runner: r1\n",
            "          - runner: r2\n            name: b\n",
        ))
        self.assertEqual([(l["name"], l["runner"]) for l in legs], [("a", "r1"), ("b", "r2")])

    def test_leg_without_runner_dies(self):
        err = io.StringIO()
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(err):
            dt.parse_build_legs(self._ci("          - name: a\n            os: linux\n"))
        self.assertIn("leg a has no `runner`", err.getvalue())

    def test_leg_without_name_dies(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            dt.parse_build_legs(self._ci("          - runner: r\n"))

    def test_sanitizer_job_ids_with_underscore_digit_uppercase(self):
        jobs, _ = dt.parse_sanitizers(
            "on:\n  schedule:\n    - cron: '0 3 * * *'\njobs:\n  tsan_extra:\n    x: 1\n  Asan2:\n    x: 1\n  1st:\n    x: 1\n")
        self.assertEqual(jobs, ["tsan_extra", "Asan2", "1st"])

    def test_sanitizer_jobs_and_cron(self):
        jobs, cron = dt.parse_sanitizers(SAN)
        self.assertEqual(jobs, ["asan", "tsan"])
        self.assertEqual(cron, "0 3 * * *")

    def test_fuzz_counts_by_crate(self):
        self.assertEqual(dt.fuzz_counts(FUZZ), [("tst-core", 2), ("tst-rtp", 1)])


class Renderers(unittest.TestCase):
    def test_targets_show_pr_and_main_passes(self):
        rows = dt.render_targets(dt.parse_build_legs(CI), dt.parse_test_legs(CI)).splitlines()
        self.assertEqual(rows[0], "| Target | Runner | Build jobs | Tests on a PR | Tests on main only |")
        self.assertEqual(
            rows[2:],
            [
                "| `linux-x86_64` | `ubuntu-latest` | `build (linux-x86_64)` | `default` ×3, `no-default` ×3, `all-features` ×3 | — |",
                "| `windows-msvc` | `windows-latest` | `build (windows-msvc)`, `build (windows-msvc-features)` | `all-features` ×2 | `default` ×2, `no-default` ×2 |",
            ],
        )

    def test_sanitizers(self):
        out = dt.render_sanitizers(["asan", "tsan"], "0 3 * * *")
        self.assertIn("`0 3 * * *`", out)
        self.assertIn("2 jobs: `asan`, `tsan`", out)

    def test_fuzz_sentence_counts_the_toml(self):
        out = dt.render_fuzz([("tst-core", 2), ("tst-rtp", 1)])
        self.assertEqual(
            out, "3 cargo-fuzz harnesses live in-tree (2 in `tst-core`, 1 in `tst-rtp`)."
        )

    def test_features_default_follows_the_closure_and_notes(self):
        out = dt.render_features(
            META,
            [("srt-sys", "mbedtls"), ("tst-core", "file"), ("tst-core", "tls")],
            {"srt-sys/mbedtls": "enc", "tst-core/file": "f", "tst-core/tls": "t"},
        ).splitlines()
        self.assertEqual(
            out[2:],
            [
                "| `srt-sys` (published as `tstrans-srt-sys`) | `mbedtls` | on | enc |",
                "| `tst-core` | `file` | on | f |",
                "| `tst-core` | `tls` | off | t |",
            ],
        )

    def test_missing_feature_is_an_error(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            dt.render_features(META, [("tst-core", "nope")], {"tst-core/nope": ""})

    def test_missing_note_is_an_error(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            dt.render_features(META, [("tst-core", "tls")], {})

    def test_manifest_list(self):
        self.assertEqual(
            dt.render_manifests(["b.toml", "a.toml"]),
            "2 manifests live here: `a.toml`, `b.toml`.",
        )


class Blocks(unittest.TestCase):
    DOC = "x\n<!-- BEGIN GENERATED by scripts/gen/doc-tables.sh (t); do not edit -->\nold\n<!-- END GENERATED by scripts/gen/doc-tables.sh (t) -->\ny\n"

    def test_replace(self):
        out = dt.replace_block(self.DOC, "t", "new")
        self.assertEqual(
            out, "x\n<!-- BEGIN GENERATED by scripts/gen/doc-tables.sh (t); do not edit -->\nnew\n<!-- END GENERATED by scripts/gen/doc-tables.sh (t) -->\ny\n"
        )

    def test_missing_markers_is_an_error(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            dt.replace_block("no markers", "t", "new")


if __name__ == "__main__":
    unittest.main()
