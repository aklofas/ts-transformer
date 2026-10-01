"""Unit tests for scripts/gen/loc_report.py.

Run:  python3 -m unittest scripts/gen/test_loc_report.py
"""
import os
import sys
import textwrap
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import loc_report as lr  # noqa: E402


def classify(lang, text, path="x"):
    """Return the per-line class list for `text` in `lang`."""
    return lr.classify_lines(lang, textwrap.dedent(text).lstrip("\n").splitlines(), path)


class RustClassification(unittest.TestCase):
    def test_plain_code_comment_blank(self):
        self.assertEqual(
            classify("rust", """
                // a comment
                fn main() {}

                /// doc comment
                //! inner doc
                /* block
                   comment */ let x = 1;
            """),
            ["comment", "code", "blank", "doc", "doc", "comment", "code"],
        )

    def test_inline_cfg_test_module_is_test_including_nested_braces(self):
        self.assertEqual(
            classify("rust", """
                fn prod() {}
                #[cfg(test)]
                mod tests {
                    use super::*;
                    #[test]
                    fn t() { if true { assert!(true); } }
                }
                fn after() {}
            """),
            ["code", "test", "test", "test", "test", "test", "test", "code"],
        )

    def test_cfg_test_single_item_runs_to_semicolon(self):
        self.assertEqual(
            classify("rust", """
                #[cfg(test)]
                use std::collections::HashMap;
                fn prod() {}
            """),
            ["test", "test", "code"],
        )

    def test_blank_and_comment_inside_test_module_count_as_test(self):
        self.assertEqual(
            classify("rust", """
                #[cfg(test)]
                mod tests {

                    // helper
                    fn t() {}
                }
            """),
            ["test"] * 6,
        )

    def test_braces_inside_strings_and_comments_do_not_confuse_depth(self):
        self.assertEqual(
            classify("rust", """
                #[cfg(test)]
                mod tests {
                    const S: &str = "}";
                    // }
                    fn t() {}
                }
                fn prod() {}
            """),
            ["test"] * 6 + ["code"],
        )


class OtherLanguages(unittest.TestCase):
    def test_python_docstring_and_hash_comment(self):
        self.assertEqual(
            classify("python", '''
                """Module docstring
                spans lines."""
                # comment
                x = 1
                def f():
                    """one-line docstring"""
                    return 1
            '''),
            ["doc", "doc", "comment", "code", "code", "doc", "code"],
        )

    def test_java_doc_vs_block_comment(self):
        self.assertEqual(
            classify("java", """
                /** javadoc */
                /* plain */
                class A {}
            """),
            ["doc", "comment", "code"],
        )

    def test_c_comments(self):
        self.assertEqual(
            classify("c", """
                /* a */ int x; // b
                // c
                int y;
            """),
            ["code", "comment", "code"],
        )

    def test_shell_comment_not_shebang(self):
        self.assertEqual(
            classify("shell", """
                #!/usr/bin/env bash
                # comment
                echo hi
            """),
            ["code", "comment", "code"],
        )


class FileLevelRules(unittest.TestCase):
    def test_language_from_extension(self):
        self.assertEqual(lr.language_for("crates/x/src/lib.rs"), "rust")
        self.assertEqual(lr.language_for("bindings/python/python/tstrans/a.pyi"), "python")
        self.assertEqual(lr.language_for("bindings/c/include/tstrans.h"), "c")
        self.assertEqual(lr.language_for("embedded/freertos-srt/x.cpp"), "c")
        self.assertEqual(lr.language_for("scripts/check/x.sh"), "shell")
        self.assertEqual(lr.language_for("docs/index.md"), "markdown")
        self.assertEqual(lr.language_for(".github/workflows/ci.yml"), "yaml")
        self.assertEqual(lr.language_for("Cargo.toml"), "toml")
        self.assertIsNone(lr.language_for("tests/fixtures/x.bin"))

    def test_test_file_detection(self):
        self.assertTrue(lr.is_test_file("crates/tst-core/tests/klv.rs"))
        self.assertTrue(lr.is_test_file("crates/tst-core/fuzz/fuzz_targets/a.rs"))
        self.assertTrue(lr.is_test_file("crates/tst-core/benches/b.rs"))
        self.assertTrue(lr.is_test_file("bindings/jvm/src/test/java/org/tstrans/ATest.java"))
        self.assertTrue(lr.is_test_file("bindings/python/tests/test_io.py"))
        self.assertTrue(lr.is_test_file("bindings/c/tests/abi.c"))
        self.assertTrue(lr.is_test_file("crates/srt-sys/vendor/srt/test/test_main.cpp"))
        self.assertFalse(lr.is_test_file("crates/tst-core/src/lib.rs"))
        self.assertFalse(lr.is_test_file("bindings/jvm/src/main/java/org/tstrans/A.java"))
        self.assertFalse(lr.is_test_file("crates/tst-test-helpers/src/lib.rs"))

    def test_whole_test_file_classifies_every_line_as_test(self):
        self.assertEqual(
            classify("rust", """
                // comment
                fn t() {}

            """, path="crates/tst-core/tests/klv.rs"),
            ["test", "test", "test"],
        )

    def test_markdown_under_a_tests_dir_is_prose_not_test(self):
        self.assertEqual(
            classify("markdown", """
                # Fixtures

                <!-- note -->
                text
            """, path="crates/tst-core/tests/fixtures/README.md"),
            ["code", "blank", "comment", "code"],
        )

    def test_component_assignment(self):
        cases = {
            "crates/tst-core/src/lib.rs": "tst-core",
            "crates/tst-pipeline/src/lib.rs": "tst-pipeline",
            "crates/tst-srt/src/lib.rs": "tst-srt",
            "crates/tst-hls/src/lib.rs": "tst-hls",
            "crates/srt-sys/vendor/srt/srtcore/core.cpp": "vendor/libsrt",
            "crates/rist-sys/vendor/librist/src/rist.c": "vendor/librist",
            "crates/srt-sys/build.rs": "sys",
            "crates/mbedtls-src/src/lib.rs": "sys",
            "crates/tst-test-helpers/src/lib.rs": "test-infra",
            "crates/tst-integration/tests/x.rs": "test-infra",
            "crates/tst-interop/src/lib.rs": "test-infra",
            "bindings/c/core/src/lib.rs": "bindings/c",
            "bindings/python/src/lib.rs": "bindings/python",
            "bindings/jvm/src/main/java/A.java": "bindings/jvm",
            "examples/sending/send_srt.rs": "examples",
            "embedded/freertos-srt/main.c": "embedded",
            "embedded/scripts/check/x.sh": "scripts",
            "scripts/check/c/x.sh": "scripts",
            "oss-fuzz/build.sh": "scripts",
            ".github/workflows/ci.yml": "ci",
            "docs/index.md": "docs",
            "README.md": "docs",
            "tests/coverage/surface-manifest.toml": "test-infra",
            "tests/coverage/README.md": "docs",
            "Cargo.toml": "other",
        }
        for path, want in cases.items():
            self.assertEqual(lr.component_for(path), want, path)

    def test_unit_assignment_is_crate_or_binding_dir(self):
        self.assertEqual(lr.unit_for("crates/tst-core/src/lib.rs"), "tst-core")
        self.assertEqual(lr.unit_for("bindings/jvm/src/main/A.java"), "bindings/jvm")
        self.assertEqual(lr.unit_for("examples/sending/a.rs"), "examples")
        self.assertEqual(lr.unit_for("embedded/freertos-srt/a.c"), "embedded/freertos-srt")
        self.assertEqual(lr.unit_for("crates/rist-sys/vendor/librist/src/rist.c"), "vendor/librist")

    def test_vendored_trees_count_source_languages_only(self):
        self.assertTrue(lr.counts_in_vendored("c"))
        self.assertTrue(lr.counts_in_vendored("python"))
        self.assertFalse(lr.counts_in_vendored("markdown"))
        self.assertFalse(lr.counts_in_vendored("yaml"))
        self.assertFalse(lr.counts_in_vendored("toml"))

    def test_group_of_component(self):
        self.assertEqual(lr.group_of("tst-srt"), "transports")
        self.assertEqual(lr.group_of("tst-core"), "tst-core")
        self.assertEqual(lr.group_of("vendor/librist"), "vendor")


class Aggregation(unittest.TestCase):
    def test_count_file_totals(self):
        counts = lr.count_lines("rust", "crates/tst-core/src/a.rs", [
            "// c", "fn a() {}", "", "#[cfg(test)]", "mod t { }",
        ])
        self.assertEqual(counts, {"code": 1, "comment": 1, "doc": 0, "blank": 1, "test": 2})

    def test_unit_rollup_splits_a_mixed_unit_by_component(self):
        files = [
            {"unit": "tests", "component": "docs", "code": 5, "comment": 0, "doc": 0, "blank": 1, "test": 0},
            {"unit": "tests", "component": "test-infra", "code": 0, "comment": 0, "doc": 0, "blank": 0, "test": 9},
        ]
        rows = lr.unit_rows(files)
        self.assertEqual([(c, u) for c, u, _ in rows], [("test-infra", "tests"), ("docs", "tests")])
        self.assertEqual(rows[0][2]["test"], 9)
        self.assertEqual(rows[1][2]["code"], 5)

    def test_render_groups_transports_and_separates_vendored(self):
        def f(path, **kw):
            d = {"path": path, "lang": "rust", "component": lr.component_for(path), "unit": lr.unit_for(path),
                 "test_file": False, "code": 1, "comment": 0, "doc": 0, "blank": 0, "test": 0}
            d.update(kw)
            return d
        report = {
            "generated_utc": "T", "commit": "abc",
            "vendored": [{"name": "libsrt", "path": "crates/srt-sys/vendor/srt", "tag": "v1.5.7"}],
            "files": [
                f("crates/tst-core/src/lib.rs", code=10),
                f("crates/tst-srt/src/lib.rs", code=3),
                f("crates/tst-udp/src/lib.rs", code=4),
                f("crates/srt-sys/vendor/srt/srtcore/core.cpp", lang="c", code=100),
            ],
        }
        md = lr.render_markdown(report)
        comp = md.split("### By component")[1].split("###")[0]
        self.assertIn("| tst-core |", comp)
        self.assertIn("tst-srt", comp)
        self.assertIn("| **transports** | 2 | 7 |", comp)
        self.assertNotIn("vendor", comp)
        self.assertIn("| **total (ts-transformer)** | 3 | 17 |", comp)
        vend = md.split("### Vendored libraries")[1]
        self.assertIn("| libsrt | `v1.5.7` | 1 | 100 |", vend)

    def test_markdown_block_replacement(self):
        doc = "intro\n<!-- loc:begin -->\nold\n<!-- loc:end -->\ntail\n"
        out = lr.replace_block(doc, "new table\n")
        self.assertEqual(out, "intro\n<!-- loc:begin -->\nnew table\n<!-- loc:end -->\ntail\n")

    def test_markdown_block_replacement_requires_markers(self):
        with self.assertRaises(ValueError):
            lr.replace_block("no markers\n", "x")


if __name__ == "__main__":
    unittest.main()
