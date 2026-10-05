"""Unit tests for scripts/gen/benchmarks_page.py.

Run:  python3 -m unittest scripts/gen/test_benchmarks_page.py
"""
import json
import os
import re
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import benchmarks_page as bp  # noqa: E402

FIX = os.path.join(os.path.dirname(__file__), "fixtures")


def load():
    with open(os.path.join(FIX, "stress-results.smoke.json")) as f:
        r = json.load(f)
    with open(os.path.join(FIX, "provenance.smoke.json")) as f:
        p = json.load(f)
    return r, p


def subsection(md, section_heading, transport_heading):
    """The text under `#### <transport_heading>` inside the
    `### <section_heading>` block, up to the next `####` or `###` heading —
    so a test scoped to one transport's table can't be satisfied by a
    different transport's table (or a different section's) still being
    present elsewhere in the page."""
    sec = re.search(rf"^### {re.escape(section_heading)}\n(.*?)(?=^### |\Z)", md, re.M | re.S)
    assert sec, f"section '### {section_heading}' not found"
    sub = re.search(rf"^#### {re.escape(transport_heading)}\n(.*?)(?=^#### |\Z)", sec.group(1), re.M | re.S)
    assert sub, f"subsection '#### {transport_heading}' not found under '### {section_heading}'"
    return sub.group(1)


class Render(unittest.TestCase):
    def test_reference_machine_from_provenance(self):
        r, p = load()
        md = bp.render(r, p)
        self.assertIn(p["host"]["kernel"], md)
        self.assertIn(str(p["host"]["cpus"]), md)
        self.assertIn(p["source"]["head"][:12], md)

    def test_stream_table_has_one_row_per_step_and_a_ceiling_line(self):
        r, p = load()
        md = bp.render(r, p)
        srt = next(a for a in r["sweep"] if a["transport"] == "srt" and a["axis"] == "streams")
        sub = subsection(md, "Stream scaling", "SRT")
        for step in srt["steps"]:
            self.assertRegex(sub, rf"\|\s*{step['decl']['streams']}\s*\|")
        self.assertIn("Ceiling:", sub)

    def test_stream_table_cpu_column_is_cores_per_stream(self):
        # The column is cpu_fraction_per_stream converted to cores by
        # multiplying by the step's vCPU count (I2) — not the raw fraction
        # of the whole host.
        r, p = load()
        md = bp.render(r, p)
        srt = next(a for a in r["sweep"] if a["transport"] == "srt" and a["axis"] == "streams")
        sub = subsection(md, "Stream scaling", "SRT")
        step = next(s for s in srt["steps"] if s["decl"]["streams"] == 2)
        expected_cores = f"{step['cpu_fraction_per_stream'] * step['decl']['vcpus']:.3f}"
        self.assertRegex(sub, rf"\|\s*2\s*\|\s*{re.escape(expected_cores)}\s*\|")

    def test_forced_fail_step_renders_as_fail_with_its_verdict(self):
        r, p = load()
        md = bp.render(r, p)
        failed = [a for a in r["sweep"] if a["first_fail"] is not None]
        self.assertTrue(failed, "fixture must contain the smoke's forced failure")
        self.assertIn(failed[0]["first_fail_verdicts"][0], md)

    def test_limitations_verbatim(self):
        r, p = load()
        md = bp.render(r, p)
        for line in r["limitations"]:
            self.assertIn(line, md)

    def test_replace_block_requires_markers(self):
        with self.assertRaises(ValueError):
            bp.replace_block("no markers here", "body")
        doc = "a\n<!-- bench:begin -->\nold\n<!-- bench:end -->\nb"
        self.assertEqual(bp.replace_block(doc, "new"), "a\n<!-- bench:begin -->\nnew<!-- bench:end -->\nb")


class RenderShape(unittest.TestCase):
    """Extra shape checks beyond the brief's five, covering what the spec
    §7 contract adds (the bitrate table, the hold, section order) without
    asserting any specific number."""

    def test_section_order(self):
        r, p = load()
        md = bp.render(r, p)
        order = ["### Reference machine", "### Stream scaling", "### Single-stream throughput", "### The hold", "### Limitations"]
        positions = [md.index(h) for h in order]
        self.assertEqual(positions, sorted(positions))

    def test_bitrate_table_has_one_row_per_step_and_a_scale_ceiling_line(self):
        r, p = load()
        md = bp.render(r, p)
        tcp = next(a for a in r["sweep"] if a["transport"] == "tcp" and a["axis"] == "bitrate")
        sub = subsection(md, "Single-stream throughput", "TCP")
        for step in tcp["steps"]:
            self.assertRegex(sub, rf"\|\s*{step['decl']['au_scale']}\s*\|")
        self.assertRegex(sub, r"Ceiling:\s*\d+\s*scale")

    def test_hold_section_has_transport_rows_and_verdict_names(self):
        r, p = load()
        md = bp.render(r, p)
        hold = r["hold"]
        for t in hold["decl"]["n_hold"]:
            self.assertRegex(md, rf"\|\s*{t}\s*\|")
        for v in hold["step"]["verdicts"] + hold["hold_verdicts"]:
            self.assertIn(v["name"], md)

    def test_hold_none_renders_no_hold_message(self):
        r, p = load()
        r = dict(r)
        r["hold"] = None
        md = bp.render(r, p)
        self.assertIn("No hold in this run.", md)


class LimitationCollapse(unittest.TestCase):
    """A limitation naming many processes of one kind collapses to a count
    and a pattern; without the collapse a 128-stream RIST sweep renders one
    8 KB line listing every sender."""

    def _long_line(self, n):
        items = [f"rss_slope_rist-{i}_send over its allowance at step 64, step 128" for i in range(n)]
        return ("rist/streams: " + "; ".join(items)
                + " — recorded, not gated (declared rss_slope_ungated; the hold gates it)")

    def test_many_processes_collapse_to_count_and_pattern(self):
        r, p = load()
        r = dict(r, limitations=[self._long_line(128)])
        md = bp.render(r, p)
        line = next(ln for ln in md.splitlines() if ln.startswith("- rist/streams:"))
        self.assertLess(len(line), 300, line)
        self.assertIn("128 processes", line)
        self.assertIn("`rist-*_send`", line)
        self.assertIn("recorded, not gated", line)
        self.assertNotIn("rist-127", line)

    def test_few_processes_stay_verbatim(self):
        r, p = load()
        short = self._long_line(2)
        r = dict(r, limitations=[short])
        self.assertIn(short, bp.render(r, p))


if __name__ == "__main__":
    unittest.main()
