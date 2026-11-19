"""Unit tests for scripts/gen/known_issues.py.

Run:  python3 -m unittest scripts/gen/test_known_issues.py
"""
import contextlib
import io
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import known_issues as ki  # noqa: E402

HEAD = ("| Finding | Subsystem | Severity | Found by | Why the suite missed it "
        "| Class now covered by | Fixed in | Workaround |\n|---|---|---|---|---|---|---|---|\n")


def row(finding, subsystem, fixed, workaround="—"):
    return "| %s | %s | high | review | why | class | %s | %s |\n" % (finding, subsystem, fixed, workaround)


def ledger(*rows):
    return "# Release findings ledger\n\nProse with a | pipe.\n\n## Findings\n\n" + HEAD + "".join(rows)


LEDGER = ledger(
    row("A fixed defect", "RIST", "0.10.0"),
    row("The HLS server has no limit", "HLS server", "open", "Keep the loopback bind."),
    row("A harness oracle misjudges", "interop harness", "open"),
    row("A parser rejects `a|b` input", "tst-core", "open"),
)

CHANGELOG = """# Changelog

---

## [Unreleased]

Next release: 0.11.0

### Changed — tst-core (BREAKING)

- Something.

---

## [0.10.0] — 2026-11-12

### Known issues

- Old, hand written.
"""


def block(*bullets):
    return (ki.BEGIN + "\n### Known issues\n\n" + "".join(b + "\n" for b in bullets) + ki.END + "\n")


class Rows(unittest.TestCase):
    def test_only_open_user_facing_rows_are_listed(self):
        bullets = ki.bullets(LEDGER)
        self.assertEqual(bullets, [
            "- The HLS server has no limit. Keep the loopback bind.",
            "- A parser rejects `a|b` input."])

    def test_a_table_without_the_columns_is_an_error(self):
        with self.assertRaises(ki.KnownIssuesError):
            ki.bullets(LEDGER.replace("| Workaround |", "|"))

    def test_a_ragged_row_is_an_error(self):
        with self.assertRaises(ki.KnownIssuesError):
            ki.bullets(LEDGER + "| a | b |\n")


class Sections(unittest.TestCase):
    def test_unreleased_takes_the_block_before_its_closing_rule(self):
        out = ki.apply(CHANGELOG, LEDGER)
        head, tail = out.split("## [0.10.0]")
        self.assertIn(ki.BEGIN, head)
        self.assertNotIn(ki.BEGIN, tail)
        self.assertTrue(head.rstrip().endswith("---"))
        self.assertIn("- Something.\n\n" + ki.BEGIN, head)

    def test_a_section_in_preparation_takes_it_instead(self):
        prep = "## [0.11.0] — 2026-11-30 <!-- TODO(release): date -->"
        text = CHANGELOG.replace("## [0.10.0] — 2026-11-12", prep)
        out = ki.apply(text, LEDGER)
        self.assertNotIn(ki.BEGIN, out.split("## [0.11.0]")[0])
        self.assertIn(ki.BEGIN, out.split("## [0.11.0]")[1])

    def test_rewrite_is_idempotent_and_replaces_a_stale_block(self):
        once = ki.apply(CHANGELOG, LEDGER)
        self.assertEqual(ki.apply(once, LEDGER), once)
        fewer = ledger(row("Only one", "RIST", "open"))
        twice = ki.apply(once, fewer)
        self.assertEqual(twice.count(ki.BEGIN), 1)
        self.assertIn("- Only one.", twice)
        self.assertNotIn("HLS", twice)

    def test_no_open_row_writes_an_explicit_empty_list(self):
        out = ki.apply(CHANGELOG, ledger(row("Done", "RIST", "0.10.0")))
        self.assertIn("No known issues.", out)

    def test_a_missing_section_is_an_error(self):
        with self.assertRaises(ki.KnownIssuesError):
            ki.apply("# Changelog\n\n## [0.10.0] — 2026-11-12\n", LEDGER)


class Cli(unittest.TestCase):
    def run_cli(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            try:
                ki.main(list(args))
                rc = 0
            except SystemExit as e:
                rc = e.code
        return rc, out.getvalue(), err.getvalue()

    def tree(self):
        d = tempfile.mkdtemp()
        os.makedirs(os.path.join(d, "docs", "project"))
        with open(os.path.join(d, "docs", "project", "release-findings.md"), "w") as f:
            f.write(LEDGER)
        with open(os.path.join(d, "CHANGELOG.md"), "w") as f:
            f.write(CHANGELOG)
        return d

    def test_check_fails_until_written_then_passes(self):
        d = self.tree()
        rc, _, err = self.run_cli("--root", d, "--check")
        self.assertEqual(rc, 1)
        self.assertIn("stale", err)
        rc, _, _ = self.run_cli("--root", d, "--write")
        self.assertEqual(rc, 0)
        rc, out, _ = self.run_cli("--root", d, "--check")
        self.assertEqual(rc, 0)
        self.assertIn("OK", out)


if __name__ == "__main__":
    unittest.main()
