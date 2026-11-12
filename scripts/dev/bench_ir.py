#!/usr/bin/env python3
"""Judge the instruction counts scripts/dev/bench-ir.sh collected for two commits.

usage: bench_ir.py --results <tsv> --old <ref> --new <ref> --accepted <toml>
                   [--threshold <percent>] [--json <file>]

<tsv> rows: workload, iterations, old instructions, new instructions (totals for
the whole run; an empty old column means the harness did not build against the
old commit).

A workload whose instructions per iteration grew by more than the threshold
fails unless <toml> accepts it for this comparison base:

    [[accepted]]
    workload = "st0601_decode"        # or "*": the harness does not build against `since`
    since = "v0.9.0"                  # the old ref the growth is measured from
    max_growth_percent = 26
    reason = "one sentence"

Rows with another `since` are history and are ignored. A row for this base that
names no workload of the run, or a "*" row when the old side was counted, is an
error, so the file cannot rot.

Exit 0 pass, 1 fail, 2 bad input.
"""
import argparse
import json
import sys
import tomllib

DEFAULT_THRESHOLD = 3.0


class InputError(Exception):
    pass


def load_accepted(text, old):
    """The rows that apply to comparisons against `old`, keyed by workload."""
    rows = {}
    for row in tomllib.loads(text).get("accepted", []):
        for key in ("workload", "since", "reason"):
            if not isinstance(row.get(key), str) or not row[key].strip():
                raise InputError("an [[accepted]] row has no %s: %r" % (key, row))
        if row["workload"] != "*" and not isinstance(row.get("max_growth_percent"), (int, float)):
            raise InputError("the [[accepted]] row for %s has no max_growth_percent" % row["workload"])
        if row["since"] != old:
            continue
        if row["workload"] in rows:
            raise InputError("two [[accepted]] rows for %s since %s" % (row["workload"], old))
        rows[row["workload"]] = row
    return rows


def parse_results(text):
    out = []
    for line in text.splitlines():
        if not line.strip():
            continue
        cols = line.split("\t")
        if len(cols) != 4:
            raise InputError("result row needs 4 tab-separated columns: %r" % line)
        name, iters, old, new = cols
        try:
            row = (name, int(iters), int(old) if old else None, int(new))
        except ValueError:
            raise InputError("result row has a non-numeric count: %r" % line) from None
        if row[1] <= 0 or row[3] <= 0 or (row[2] is not None and row[2] <= 0):
            raise InputError("result row has a count that is not above zero: %r" % line)
        out.append(row)
    if not out:
        raise InputError("no result rows")
    return out


def judge(results, accepted, threshold=DEFAULT_THRESHOLD):
    """One record per workload plus the verdict. `accepted` is load_accepted()'s map."""
    names = {r[0] for r in results}
    stale = sorted(set(accepted) - names - {"*"})
    if stale:
        raise InputError("accepted rows name workloads this run does not have: " + ", ".join(stale))
    if "*" in accepted and any(r[2] is not None for r in results):
        raise InputError('the "*" accepted row is stale: the harness built against the old ref')
    workloads = []
    for name, iters, old, new in results:
        rec = {"name": name, "iterations": iters, "new_instructions": round(new / iters, 1)}
        if old is None:
            star = accepted.get("*")
            rec["status"] = "not compared, accepted" if star else "not compared"
            if star:
                rec["reason"] = star["reason"]
        else:
            delta = (new - old) * 100 / old
            rec["old_instructions"] = round(old / iters, 1)
            rec["delta_percent"] = round(delta, 2)
            row = accepted.get(name)
            if delta <= threshold:
                rec["status"] = "ok"
            elif row and delta <= row["max_growth_percent"]:
                rec["status"] = "accepted"
                rec["reason"] = row["reason"]
            elif row:
                rec["status"] = "over the accepted %s %%" % row["max_growth_percent"]
            else:
                rec["status"] = "over the threshold"
        workloads.append(rec)
    passed = all(w["status"] in ("ok", "accepted", "not compared, accepted") for w in workloads)
    return workloads, passed


def render(workloads, passed, old, new, threshold):
    lines = ["instructions per iteration, %s -> %s (a workload may grow by %g %%)" % (old, new, threshold)]
    for w in sorted(workloads, key=lambda w: -w.get("delta_percent", float("inf"))):
        if "delta_percent" in w:
            lines.append("  %-28s %12.1f %12.1f %+7.2f %%  %s" % (
                w["name"], w["old_instructions"], w["new_instructions"], w["delta_percent"], w["status"]))
        else:
            lines.append("  %-28s %12s %12.1f %9s  %s" % (w["name"], "-", w["new_instructions"], "", w["status"]))
        if "reason" in w:
            lines.append("  %-28s accepted: %s" % ("", w["reason"]))
    lines.append("verdict: " + ("pass" if passed else "FAIL"))
    return "\n".join(lines)


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--results", required=True)
    ap.add_argument("--old", required=True)
    ap.add_argument("--new", required=True)
    ap.add_argument("--accepted", required=True)
    ap.add_argument("--threshold", type=float, default=DEFAULT_THRESHOLD)
    ap.add_argument("--json")
    ap.add_argument("--old-sha", default="")
    ap.add_argument("--new-sha", default="")
    ap.add_argument("--tool", default="")
    args = ap.parse_args(argv)
    try:
        with open(args.results, encoding="utf-8") as f:
            results = parse_results(f.read())
        with open(args.accepted, encoding="utf-8") as f:
            accepted = load_accepted(f.read(), args.old)
        workloads, passed = judge(results, accepted, args.threshold)
    except (InputError, OSError, tomllib.TOMLDecodeError) as e:
        print("bench-ir: %s" % e, file=sys.stderr)
        return 2
    print(render(workloads, passed, args.old, args.new, args.threshold))
    if args.json:
        record = {"old": {"ref": args.old, "sha": args.old_sha}, "new": {"ref": args.new, "sha": args.new_sha},
                  "threshold_percent": args.threshold, "tool": args.tool,
                  "verdict": "pass" if passed else "fail", "workloads": workloads}
        with open(args.json, "w", encoding="utf-8") as f:
            json.dump(record, f, indent=2)
            f.write("\n")
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
