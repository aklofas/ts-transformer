#!/usr/bin/env python3
"""Benchmarks page renderer for the ts-transformer stress harness.

Renders the measured tables in `docs/project/benchmarks.md` from a stress
harness archive's `stress-results.json` (the sweep steps, their verdicts,
and the 24 h hold) and `provenance.json` (host, toolchain, and source
commit) — see `docs/specs/2026-10-01-stress-harness.md` §7 for the shape of
both inputs and the rendering contract.

The page's prose is hand-written; only the block between
`<!-- bench:begin -->` and `<!-- bench:end -->` is generated, and the
renderer computes nothing but unit conversions (KB -> MiB, declared /
observed figures are read straight off the results JSON).

Usage:
  scripts/gen/benchmarks_page.py --results R.json --provenance P.json
                                            # Markdown tables to stdout
  scripts/gen/benchmarks_page.py --results R.json --provenance P.json --stdout
                                            # same, explicitly
  scripts/gen/benchmarks_page.py --results R.json --provenance P.json \
      --update docs/project/benchmarks.md  # rewrite the <!-- bench:begin/end --> block
Run from anywhere; stdlib only.
"""
from __future__ import annotations

import argparse
import json
import sys

# --------------------------------------------------------------------------
# Transport order (spec §3.2): SRT, RIST, UDP, TCP. A transport missing from
# the results (e.g. a trimmed fixture, or a run that didn't reach it) is
# skipped rather than rendered empty.
# --------------------------------------------------------------------------

TRANSPORT_ORDER = ["srt", "rist", "udp", "tcp"]


# --------------------------------------------------------------------------
# Number formatting
# --------------------------------------------------------------------------

def _cpu(x: float) -> str:
    return f"{x:.3f}"


def _mbps(x: float) -> str:
    return f"{x:.1f}"


def _mib(x: float) -> str:
    return f"{x:.1f}"


def _passfail(ok: bool) -> str:
    return "pass" if ok else "fail"


def _g(x) -> str:
    """Generic number formatter for the hold's mixed-unit verdict table:
    an integer-valued float renders bare, otherwise 3 decimals trimmed."""
    if isinstance(x, bool):
        return str(x)
    if isinstance(x, int):
        return str(x)
    if float(x) == int(x):
        return str(int(x))
    s = f"{x:.3f}".rstrip("0").rstrip(".")
    return "0" if s in ("-0", "-") else s


# --------------------------------------------------------------------------
# Per-stream metric helpers
# --------------------------------------------------------------------------

def _rss_mib_per_stream(per_stream: list[dict]) -> float:
    """Average per-stream RSS footprint: sum rss_kb_p99 over each stream's
    own processes (send+proxy+recv), then the mean across the step's
    streams — consistent with `cpu_fraction_per_stream`, which is itself an
    average, not a worst-case figure — converted KB -> MiB."""
    sums = [sum(ps["rss_kb_p99"].values()) for ps in per_stream]
    return (sum(sums) / len(sums) / 1024.0) if sums else 0.0


def _max_metric(per_stream: list[dict], key: str) -> int:
    """Worst value of a per-stream-per-process dict (threads_max / fds_max)
    across every process of every stream in the step."""
    vals = [v for ps in per_stream for v in ps[key].values()]
    return max(vals) if vals else 0


# --------------------------------------------------------------------------
# Markdown table helper
# --------------------------------------------------------------------------

def _table(header: list[str], rows: list[list[str]]) -> str:
    out = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] * len(header)) + "|"]
    out += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(out) + "\n"


# --------------------------------------------------------------------------
# Sections
# --------------------------------------------------------------------------

def _reference_machine(provenance: dict) -> str:
    host = provenance["host"]
    source = provenance["source"]
    mem_gib = host["mem_total_kb"] / (1024 * 1024)
    lines = [
        f"- Kernel: {host['kernel']}",
        f"- vCPUs: {host['cpus']}",
        f"- Memory: {_mib(mem_gib)} GiB",
        f"- Toolchain: {provenance['toolchain']['rustc']}",
        f"- Source: `{source['head'][:12]}` ({source['describe']})",
        f"- Recorded: {provenance['written_utc']}",
    ]
    return "\n".join(lines) + "\n"


def _axes_by_transport(sweep: list[dict], axis: str) -> list[tuple[str, dict]]:
    by = {a["transport"]: a for a in sweep if a["axis"] == axis}
    return [(t, by[t]) for t in TRANSPORT_ORDER if t in by]


def _ceiling_line(entry: dict, unit: str) -> str:
    ceiling = entry["ceiling"]
    if entry["first_fail"] is None:
        return f"Ceiling: top of ladder ({ceiling}), not a measured limit"
    verdicts = ", ".join(entry["first_fail_verdicts"])
    return f"Ceiling: {ceiling} {unit} — ended by {verdicts}"


_STREAM_HEADER = ["N", "CPU/stream (fraction of a core)", "RSS/stream p99 (MiB)", "threads max", "fds max", "wire Mb/s", "pass"]


def _stream_table(entry: dict) -> str:
    rows = []
    for step in entry["steps"]:
        d = step["decl"]
        rows.append([
            str(d["streams"]),
            _cpu(step["cpu_fraction_per_stream"]),
            _mib(_rss_mib_per_stream(step["per_stream"])),
            str(_max_metric(step["per_stream"], "threads_max")),
            str(_max_metric(step["per_stream"], "fds_max")),
            _mbps(step["aggregate_wire_mbps"]),
            _passfail(step["pass"]),
        ])
    return _table(_STREAM_HEADER, rows)


_BITRATE_HEADER = ["scale", "declared Mb/s", "observed Mb/s", "CPU fraction", "pass"]


def _bitrate_table(entry: dict) -> str:
    rows = []
    for step in entry["steps"]:
        d = step["decl"]
        rows.append([
            str(d["au_scale"]),
            _mbps(d["nominal_mbps_per_stream"]),
            _mbps(step["aggregate_wire_mbps"]),
            _cpu(step["cpu_fraction_per_stream"]),
            _passfail(step["pass"]),
        ])
    return _table(_BITRATE_HEADER, rows)


_HOLD_STREAM_HEADER = ["transport", "N", "aggregate Mb/s"]
_HOLD_VERDICT_HEADER = ["name", "observed", "threshold", "pass"]


def _hold_stream_table(hold: dict) -> str:
    n_hold = hold["decl"]["n_hold"]
    per_stream = hold["step"]["per_stream"]
    rows = []
    for t in TRANSPORT_ORDER:
        if t not in n_hold:
            continue
        agg = sum(ps["wire_mbps"] for ps in per_stream if ps["leg"].split("-")[0] == t)
        rows.append([t, str(n_hold[t]), _mbps(agg)])
    return _table(_HOLD_STREAM_HEADER, rows)


def _hold_verdict_table(hold: dict) -> str:
    rows = []
    for v in hold["step"]["verdicts"] + hold["hold_verdicts"]:
        rows.append([v["name"], _g(v["observed"]), _g(v["threshold"]), _passfail(v["pass"])])
    return _table(_HOLD_VERDICT_HEADER, rows)


def _hold_section(hold: dict | None) -> str:
    if hold is None:
        return "No hold in this run.\n"
    return _hold_stream_table(hold) + "\n" + _hold_verdict_table(hold)


def _limitations(results: dict) -> str:
    lines = results.get("limitations") or []
    if not lines:
        return "None.\n"
    return "\n".join(f"- {line}" for line in lines) + "\n"


def render(results: dict, provenance: dict) -> str:
    parts = ["### Reference machine\n", _reference_machine(provenance)]

    parts.append("### Stream scaling\n")
    for transport, entry in _axes_by_transport(results["sweep"], "streams"):
        parts.append(f"#### {transport.upper()}\n")
        parts.append(_stream_table(entry))
        parts.append("Per-stream figures are averages over the step's streams.\n")
        parts.append(_ceiling_line(entry, "streams") + "\n")

    parts.append("### Single-stream throughput\n")
    for transport, entry in _axes_by_transport(results["sweep"], "bitrate"):
        parts.append(f"#### {transport.upper()}\n")
        parts.append(_bitrate_table(entry))
        parts.append(_ceiling_line(entry, "scale") + "\n")

    parts.append("### The hold\n")
    parts.append(_hold_section(results.get("hold")))

    parts.append("### Limitations\n")
    parts.append(_limitations(results))

    return "\n".join(parts)


# --------------------------------------------------------------------------
# Markers + CLI
# --------------------------------------------------------------------------

_BEGIN, _END = "<!-- bench:begin -->", "<!-- bench:end -->"


def replace_block(doc: str, body: str) -> str:
    b = doc.find(_BEGIN)
    e = doc.find(_END)
    if b < 0 or e < 0 or e < b:
        raise ValueError(f"document lacks {_BEGIN} / {_END} markers")
    b += len(_BEGIN)
    return doc[:b] + "\n" + body + doc[e:]


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--results", required=True, metavar="R.json", help="stress-results.json from the archive")
    ap.add_argument("--provenance", required=True, metavar="P.json", help="provenance.json from the archive")
    ap.add_argument("--update", metavar="MD", help="rewrite the bench block in this Markdown file")
    ap.add_argument(
        "--stdout", action="store_true",
        help="print Markdown to stdout; this is already the default when --update is omitted — "
             "the flag is accepted (and otherwise ignored) for symmetry with benchmarks-page.sh's own --stdout argument",
    )
    args = ap.parse_args(argv)

    with open(args.results, encoding="utf-8") as fh:
        results = json.load(fh)
    with open(args.provenance, encoding="utf-8") as fh:
        provenance = json.load(fh)

    md = render(results, provenance)
    if args.update:
        with open(args.update, encoding="utf-8") as fh:
            doc = fh.read()
        with open(args.update, "w", encoding="utf-8") as fh:
            fh.write(replace_block(doc, md))
        print(f"updated {args.update}")
        return 0
    sys.stdout.write(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
