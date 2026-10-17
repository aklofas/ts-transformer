"""Unit tests for scripts/gen/doc_tables.py.

Run:  python3 -m unittest scripts/gen/test_doc_tables.py
"""
import contextlib
import io
import os
import sys
import unittest

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
            runner: windows-latest
            continue: false
            phase: default
          - name: windows-msvc-features
            runner: windows-latest
            continue: false
            phase: features
    steps:
      - name: not a leg
        runner: nope
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

    def test_sanitizer_jobs_and_cron(self):
        jobs, cron = dt.parse_sanitizers(SAN)
        self.assertEqual(jobs, ["asan", "tsan"])
        self.assertEqual(cron, "0 3 * * *")

    def test_fuzz_counts_by_crate(self):
        self.assertEqual(dt.fuzz_counts(FUZZ), [("tst-core", 2), ("tst-rtp", 1)])


class Renderers(unittest.TestCase):
    def test_targets_fold_legs_sharing_a_runner(self):
        rows = dt.render_targets(dt.parse_build_legs(CI)).splitlines()
        self.assertEqual(rows[0], "| Target | Runner | CI job |")
        self.assertEqual(
            rows[2:],
            [
                "| `linux-x86_64` | `ubuntu-latest` | `build (linux-x86_64)` |",
                "| `windows-msvc` | `windows-latest` | `build (windows-msvc)`, `build (windows-msvc-features)` |",
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
    DOC = "x\n<!-- doc-tables:t:begin -->\nold\n<!-- doc-tables:t:end -->\ny\n"

    def test_replace(self):
        out = dt.replace_block(self.DOC, "t", "new")
        self.assertEqual(
            out, "x\n<!-- doc-tables:t:begin -->\nnew\n<!-- doc-tables:t:end -->\ny\n"
        )

    def test_missing_markers_is_an_error(self):
        with self.assertRaises(SystemExit), contextlib.redirect_stderr(io.StringIO()):
            dt.replace_block("no markers", "t", "new")


if __name__ == "__main__":
    unittest.main()
