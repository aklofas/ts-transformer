#!/usr/bin/env python3
"""Break counts per release and stability tier.

  api_churn.py paths REPORT.txt
      The lines of the "Failed in:" blocks of one cargo-semver-checks text report, as a
      JSON list of {"line", "lint", "file", "span_line", "name", "path"} (scripts/dev/semver-checks.sh writes them into
      semver-checks.json as "breaks"). `file` is the source file the line's span names,
      `path` the crate-qualified item path when the line prints one; either may be null.
  api_churn.py tally SEMVER_CHECKS.json --release X.Y.Z [--root DIR]
      Places each line in the tier of the most specific row of the classification table
      in docs/reference/api-stability.md, by the module its span file defines (a root
      re-export is thus counted where the item is defined), else by its item path, and
      prints {"release", "stable", "provisional", "experimental", "internal"}: the numbers
      of a tests/coverage/api-churn.toml row. A line no row covers is an error naming it,
      and so is a crate whose parsed lines differ in number from its `items`. A removed
      item printed once per importable path counts once (same lint, file, line and name).
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

PAGE = os.path.join("docs", "reference", "api-stability.md")
DATA = os.path.join("tests", "coverage", "api-churn.toml")
BEGIN = "<!-- BEGIN GENERATED: api-churn -->"
END = "<!-- END GENERATED: api-churn -->"
TIERS = ("stable", "provisional", "experimental", "internal")


class ChurnError(Exception):
    pass


# ---- the semver report -----------------------------------------------------

_SPAN = re.compile(r"(\S+\.rs):(\d+)")
_QUALIFIED = re.compile(r"[A-Za-z_][\w]*(?:::[\w]+)+")


def failed_lines(text):
    """One entry per non-blank line under every `Failed in:` block, in report order.

    `lint` is the name in the `--- failure <lint>:` header above the block; `name` the last
    identifier of the line's item part, which tells two items on one span apart."""
    out, inside, lint = [], False, None
    for line in text.splitlines():
        head_m = re.match(r"--- failure (\S+):", line)
        if head_m:
            lint = head_m.group(1)
        if line.startswith("Failed in:"):
            inside = True
        elif not line.strip():
            inside = False
        elif inside:
            span = _SPAN.search(line)
            head = re.split(r", previously in| in file | in /|, in | now takes| takes | is no longer",
                            line.strip())[0]
            path = _QUALIFIED.search(head)
            idents = re.findall(r"\w+", head)
            out.append({"line": line.strip(), "lint": lint, "file": span.group(1) if span else None,
                        "span_line": int(span.group(2)) if span else None,
                        "name": idents[-1] if idents else None, "path": path.group(0) if path else None})
    return out


def split_source(path):
    """(crate directory or None, module segments) of a source file: crates/<dir>/src/a/b.rs and
    .../a/b/mod.rs are (<dir>, (a, b)); lib.rs is the crate root (); no `crates/` gives None."""
    m = (re.match(r"^(?:.*/)?crates/([^/]+)/src/(.+)\.rs$", path)
         or re.match(r"^(?:.*/)?()src/(.+)\.rs$", path))
    if not m:
        return None
    segs = m.group(2).split("/")
    if segs[-1] in ("lib", "mod", "main"):
        segs = segs[:-1]
    return m.group(1) or None, tuple(segs)


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


def _lookup(rows, package, segs):
    best = None
    for mod, tier in rows.get(package, []):
        if segs[:len(mod)] == mod and (best is None or len(mod) > len(best[0])):
            best = (mod, tier)
    return best[1] if best else None


def _package_of_dir(rows, d):
    for name in (d, "tstrans-" + d):
        if name in rows:
            return name
    return None


def tier_of(rows, package, entry):
    """Tier of one `failed_lines` entry: by the module its span file defines (looked up in the
    rows of the crate the file belongs to), else by its item path."""
    if entry["file"]:
        src = split_source(entry["file"])
        if src:
            owner = _package_of_dir(rows, src[0]) if src[0] else package
            tier = _lookup(rows, owner, src[1]) if owner else None
            if tier:
                return tier
    if entry["path"]:
        segs = tuple(entry["path"].split("::")[1:])  # drop the crate
        tier = _lookup(rows, package, segs)
        if tier:
            return tier
    raise ChurnError("the line %r (crate %s) is covered by no row of the classification table"
                     % (entry["line"], package))


def tally(report, rows, release):
    counts = dict.fromkeys(TIERS, 0)
    for c in report:
        if "breaks" not in c:
            if c.get("exit") == 100 or c.get("items"):
                raise ChurnError("the report row of %s has no break lines; rerun scripts/dev/semver-checks.sh"
                                 % c.get("crate"))
            continue
        if len(c["breaks"]) != c.get("items", 0):
            raise ChurnError("the report of %s lists %s lines under Failed in: but %d were parsed"
                             % (c["crate"], c.get("items"), len(c["breaks"])))
        seen = {}
        for e in c["breaks"]:
            key = (e.get("lint"), e["file"], e["span_line"], e.get("name")) if e["file"] else e["line"]
            seen.setdefault(key, e)
        for e in seen.values():
            counts[tier_of(rows, c["crate"], e)] += 1
    return dict({"release": release}, **counts)


# ---- the table -------------------------------------------------------------

def parse_churn(text):
    import tomllib  # Python 3.11; the report parser above needs only the older standard library
    rows = []
    for r in tomllib.loads(text).get("release", []):
        if "version" not in r:
            raise ChurnError("a [[release]] row has no version")
        has_counts = [t in r for t in TIERS]
        if any(has_counts) and not all(has_counts):
            raise ChurnError("release %s: the four tier counts are all-or-none" % r["version"])
        if not all(has_counts) and "note" not in r:
            raise ChurnError("release %s has neither tier counts nor a note" % r["version"])
        if "|" in str(r.get("note", "")):
            raise ChurnError("release %s: a note may not contain `|`" % r["version"])
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
                print(json.dumps(failed_lines(f.read())))
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
