#!/usr/bin/env python3
"""Break counts per release and stability tier.

  api_churn.py paths REPORT.txt
      The item paths of the "Failed in:" blocks of one cargo-semver-checks text
      report, as a JSON list (scripts/dev/semver-checks.sh writes them into
      semver-checks.json as "paths").
  api_churn.py tally SEMVER_CHECKS.json --release X.Y.Z [--root DIR]
      Maps each distinct item path to the tier of the most specific row of the
      classification table in docs/reference/api-stability.md and prints
      {"release", "stable", "provisional", "experimental", "internal"}: the
      numbers of a tests/coverage/api-churn.toml row. A path no row covers is
      an error naming it.
  api_churn.py render [--root DIR] (--write | --check)
      The table between `<!-- BEGIN GENERATED: api-churn -->` and
      `<!-- END GENERATED: api-churn -->` in docs/reference/api-stability.md,
      from tests/coverage/api-churn.toml.

A `[[release]]` row of the toml has `version` and either the four tier counts
or a `note` saying why there are none. Stdlib only.
"""
import argparse
import json
import os
import re
import sys

try:
    import tomllib
except ImportError:  # Python < 3.11
    print("api-churn: api_churn.py needs Python >= 3.11 (tomllib)", file=sys.stderr)
    sys.exit(1)

PAGE = os.path.join("docs", "reference", "api-stability.md")
DATA = os.path.join("tests", "coverage", "api-churn.toml")
BEGIN = "<!-- BEGIN GENERATED: api-churn -->"
END = "<!-- END GENERATED: api-churn -->"
TIERS = ("stable", "provisional", "experimental", "internal")


class ChurnError(Exception):
    pass


# ---- the semver report -----------------------------------------------------

_ITEM = re.compile(r"^\s*\w+\s+(.+?)(?:,\s*previously in\b.*| in (?:file )?\S+:\d+.*)?$")


def failed_paths(text):
    """Item paths listed under every `Failed in:` block, in report order."""
    out, inside = [], False
    for line in text.splitlines():
        if line.startswith("Failed in:"):
            inside = True
        elif not line.strip():
            inside = False
        elif inside:
            m = _ITEM.match(line)
            if m:
                out.append(m.group(1).strip())
    return out


# ---- tiers -----------------------------------------------------------------

def classification(page_text):
    """{package: [(module segments, tier)]} from the classification table."""
    rows = {}
    for line in page_text.splitlines():
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) < 3 or cells[2] not in ("Stable", "Provisional", "Experimental", "Internal"):
            continue
        mod = () if cells[1] == "(crate)" else tuple(cells[1].split("::"))
        rows.setdefault(cells[0], []).append((mod, cells[2].lower()))
    return rows


def tier_of(rows, package, path):
    segs = path.split("::")
    if len(segs) < 2:
        raise ChurnError("item path %r names no module" % path)
    segs = tuple(segs[1:])  # drop the crate
    best = None
    for mod, tier in rows.get(package, []):
        if segs[:len(mod)] == mod and (best is None or len(mod) > len(best[0])):
            best = (mod, tier)
    if best is None:
        raise ChurnError("item path %r (crate %s) is covered by no row of the classification table" % (path, package))
    return best[1]


def tally(report, rows, release):
    counts = dict.fromkeys(TIERS, 0)
    for c in report:
        if "paths" not in c:
            if c.get("exit") == 100 or c.get("items"):
                raise ChurnError("the report row of %s has no item paths; rerun scripts/dev/semver-checks.sh" % c.get("crate"))
            continue
        for path in sorted(set(c["paths"])):
            counts[tier_of(rows, c["crate"], path)] += 1
    return dict({"release": release}, **counts)


# ---- the table -------------------------------------------------------------

def parse_churn(text):
    rows = []
    for r in tomllib.loads(text).get("release", []):
        if "version" not in r:
            raise ChurnError("a [[release]] row has no version")
        has_counts = [t in r for t in TIERS]
        if any(has_counts) and not all(has_counts):
            raise ChurnError("release %s: the four tier counts are all-or-none" % r["version"])
        if not all(has_counts) and "note" not in r:
            raise ChurnError("release %s has neither tier counts nor a note" % r["version"])
        rows.append(r)
    return rows


def render_table(rows):
    out = ["| Release | Basis | Items | Stable | Provisional | Experimental | Internal |",
           "|---|---|---|---|---|---|---|"]
    for r in rows:
        if all(t in r for t in TIERS):
            counts = [str(r[t]) for t in TIERS]
            cells = [r["version"], r.get("note", "items"), str(sum(r[t] for t in TIERS))] + counts
        else:
            cells = [r["version"], r["note"], "—", "—", "—", "—", "—"]
        out.append("| " + " | ".join(cells) + " |")
    return "\n".join(out) + "\n"


def apply(page, table):
    i, j = page.find(BEGIN), page.find(END)
    if i < 0 or j < i:
        raise ChurnError("%s has no %s ... %s block" % (PAGE, BEGIN, END))
    return page[:i + len(BEGIN)] + "\n" + table + page[j:]


# ---- commands --------------------------------------------------------------

def _root(args):
    return os.path.abspath(args.root)


def main(argv=None):
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("paths")
    p.add_argument("report")
    t = sub.add_parser("tally")
    t.add_argument("json")
    t.add_argument("--release", required=True)
    r = sub.add_parser("render")
    mode = r.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    for s in (t, r):
        s.add_argument("--root", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
    args = ap.parse_args(argv)
    try:
        if args.cmd == "paths":
            with open(args.report, encoding="utf-8", errors="replace") as f:
                print(json.dumps(failed_paths(f.read())))
        elif args.cmd == "tally":
            with open(os.path.join(_root(args), PAGE), encoding="utf-8") as f:
                rows = classification(f.read())
            with open(args.json, encoding="utf-8") as f:
                print(json.dumps(tally(json.load(f), rows, args.release)))
        else:
            root = _root(args)
            with open(os.path.join(root, DATA), encoding="utf-8") as f:
                table = render_table(parse_churn(f.read()))
            path = os.path.join(root, PAGE)
            with open(path, encoding="utf-8") as f:
                old = f.read()
            new = apply(old, table)
            if args.check:
                if new != old:
                    print("api-churn: the table in %s is stale; run scripts/gen/api-churn.sh" % PAGE, file=sys.stderr)
                    sys.exit(1)
                print("api-churn: OK")
            else:
                with open(path, "w", encoding="utf-8", newline="\n") as f:
                    f.write(new)
                print("api-churn: wrote the table in %s" % PAGE)
    except ChurnError as e:
        print("api-churn: %s" % e, file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
