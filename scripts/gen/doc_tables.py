#!/usr/bin/env python3
"""Write (or check) the documentation tables that restate a machine source.

Each generated region sits between `<!-- doc-tables:NAME:begin -->` and
`<!-- doc-tables:NAME:end -->` in its document; everything outside the markers
is hand-written and never touched.

  targets      docs/reference/compatibility.md   ci.yml `build` matrix
  sanitizers   docs/reference/compatibility.md   sanitizers.yml jobs + cron
  fuzz         docs/reference/compatibility.md   tests/coverage/fuzz-targets.toml
  features     docs/languages/rust.md            `cargo metadata` [features]
  manifests    tests/coverage/README.md          tests/coverage/*.toml

Usage:  doc_tables.py [--root DIR] [--write | --check]

`cargo metadata` runs in ROOT unless DOC_TABLES_METADATA names a saved JSON
file. Stdlib only; the YAML files are read with a line scanner.
"""
import argparse
import glob
import json
import os
import re
import subprocess
import sys
import tomllib

# (crate dir, feature) rows of the feature table, in display order.
FEATURE_ROWS = [
    ("srt-sys", "mbedtls"),
    ("tst-srt", "mbedtls"),
    ("tst-core", "std"),
    ("tst-core", "file"),
    ("tst-core", "tls"),
]

# The Effect column: what a feature does cannot be derived from Cargo.toml.
FEATURE_NOTES = {
    "srt-sys/mbedtls": "Builds encryption support using mbedTLS. Set a passphrase on both peers to encrypt a connection.",
    "tst-srt/mbedtls": "Propagates to `srt-sys/mbedtls`.",
    "tst-core/std": "Standard library: file I/O, net helpers, JSON/TOML. Off = `#![no_std]` + `alloc` (see [embedded](/docs/languages/embedded.md)).",
    "tst-core/file": "File I/O helpers. `file` and `std` imply each other, so they switch on and off together.",
    "tst-core/tls": "PEM loading for the rustls-based transports (`net::tls`). The transport crates' `tls` features turn it on.",
}

# (document, block name) in the order they are written.
BLOCKS = [
    ("docs/reference/compatibility.md", "targets"),
    ("docs/reference/compatibility.md", "sanitizers"),
    ("docs/reference/compatibility.md", "fuzz"),
    ("docs/languages/rust.md", "features"),
    ("tests/coverage/README.md", "manifests"),
]


def die(msg):
    print("doc-tables: " + msg, file=sys.stderr)
    sys.exit(1)


# ---- sources ---------------------------------------------------------------

def parse_build_legs(ci_text):
    """The `- name:` legs of the `build` job's `include:` list."""
    legs, in_build, in_include, cur = [], False, False, None
    for line in ci_text.splitlines():
        if re.match(r"^  [A-Za-z0-9_-]+:\s*$", line):
            in_build = line.strip() == "build:"
            in_include = False
            continue
        if not in_build:
            continue
        if re.match(r"^\s+include:\s*$", line):
            in_include = True
            continue
        if not in_include:
            continue
        m = re.match(r"^\s+- name:\s*(\S+)\s*$", line)
        if m:
            cur = {"name": m.group(1), "runner": "", "phase": ""}
            legs.append(cur)
            continue
        if re.match(r"^    \S", line) or re.match(r"^\s*- ", line):
            in_include = False  # `steps:` or any list after the matrix
            continue
        m = re.match(r"^\s+(runner|phase):\s*(\S+)\s*$", line)
        if m and cur is not None:
            cur[m.group(1)] = m.group(2)
    return legs


def parse_sanitizers(text):
    jobs, in_jobs, cron = [], False, None
    for line in text.splitlines():
        m = re.match(r"^\s*-\s*cron:\s*'([^']*)'", line)
        if m and cron is None:
            cron = m.group(1)
        if re.match(r"^jobs:\s*$", line):
            in_jobs = True
            continue
        if in_jobs:
            m = re.match(r"^  ([a-z][a-z0-9-]*):\s*$", line)
            if m:
                jobs.append(m.group(1))
    return jobs, cron


def fuzz_counts(toml_text):
    counts = {}
    for t in tomllib.loads(toml_text).get("target", []):
        counts[t["crate"]] = counts.get(t["crate"], 0) + 1
    return sorted(counts.items())


def load_metadata(root):
    saved = os.environ.get("DOC_TABLES_METADATA")
    if saved:
        with open(saved) as f:
            return json.load(f)
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps", "--offline"],
        cwd=root, check=True, capture_output=True, text=True,
    ).stdout
    return json.loads(out)


# ---- renderers --------------------------------------------------------------

def render_targets(legs):
    groups = []  # one row per runner, first leg names it
    for leg in legs:
        for g in groups:
            if g[1] == leg["runner"]:
                g[2].append(leg["name"])
                break
        else:
            groups.append([leg["name"], leg["runner"], [leg["name"]]])
    rows = ["| Target | Runner | CI job |", "|---|---|---|"]
    for name, runner, names in groups:
        jobs = ", ".join("`build (%s)`" % n for n in names)
        rows.append("| `%s` | `%s` | %s |" % (name, runner, jobs))
    return "\n".join(rows)


def render_sanitizers(jobs, cron):
    return "Schedule: `%s` (UTC). %d jobs: %s." % (
        cron, len(jobs), ", ".join("`%s`" % j for j in jobs))


def render_fuzz(counts):
    total = sum(n for _, n in counts)
    parts = ", ".join("%d in `%s`" % (n, c) for c, n in counts)
    return "%d cargo-fuzz harnesses live in-tree (%s)." % (total, parts)


def _default_closure(features):
    seen, todo = set(), list(features.get("default", []))
    while todo:
        f = todo.pop()
        if f in seen or f not in features:
            continue
        seen.add(f)
        todo.extend(features[f])
    return seen


def render_features(meta, rows, notes):
    by_dir = {}
    for p in meta["packages"]:
        by_dir[os.path.basename(os.path.dirname(p["manifest_path"]))] = p
    out = ["| Crate | Feature | Default | Effect |", "|---|---|---|---|"]
    for crate, feat in rows:
        pkg = by_dir.get(crate)
        if pkg is None:
            die("crate directory %r is not a workspace member" % crate)
        if feat not in pkg["features"]:
            die("%s has no feature %r" % (crate, feat))
        note = notes.get("%s/%s" % (crate, feat))
        if note is None:
            die("no Effect note for %s/%s" % (crate, feat))
        label = "`%s`" % crate
        if pkg["name"] != crate:
            label += " (published as `%s`)" % pkg["name"]
        on = "on" if feat in _default_closure(pkg["features"]) else "off"
        out.append("| %s | `%s` | %s | %s |" % (label, feat, on, note))
    return "\n".join(out)


def render_manifests(names):
    names = sorted(names)
    return "%d manifests live here: %s." % (
        len(names), ", ".join("`%s`" % n for n in names))


# ---- blocks -------------------------------------------------------------------

def replace_block(text, name, content):
    begin = "<!-- doc-tables:%s:begin -->" % name
    end = "<!-- doc-tables:%s:end -->" % name
    b, e = text.find(begin), text.find(end)
    if b < 0 or e < b:
        die("markers for block %r not found" % name)
    return text[: b + len(begin)] + "\n" + content + "\n" + text[e:]


def render_all(root):
    def read(rel):
        with open(os.path.join(root, rel)) as f:
            return f.read()

    jobs, cron = parse_sanitizers(read(".github/workflows/sanitizers.yml"))
    legs = parse_build_legs(read(".github/workflows/ci.yml"))
    if not legs or not jobs or not cron:
        die("could not read the build matrix or the sanitizer workflow")
    manifests = [os.path.basename(p) for p in glob.glob(os.path.join(root, "tests/coverage/*.toml"))]
    return {
        "targets": render_targets(legs),
        "sanitizers": render_sanitizers(jobs, cron),
        "fuzz": render_fuzz(fuzz_counts(read("tests/coverage/fuzz-targets.toml"))),
        "features": render_features(load_metadata(root), FEATURE_ROWS, FEATURE_NOTES),
        "manifests": render_manifests(manifests),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
    mode = ap.add_mutually_exclusive_group()
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    args = ap.parse_args()
    root = os.path.abspath(args.root)
    rendered = render_all(root)

    docs = {}
    for rel, name in BLOCKS:
        path = os.path.join(root, rel)
        if rel not in docs:
            with open(path) as f:
                docs[rel] = f.read()
        new = replace_block(docs[rel], name, rendered[name])
        if args.check and new != docs[rel]:
            print("doc-tables: %s block %r is stale; run scripts/gen/doc-tables.sh" % (rel, name), file=sys.stderr)
            sys.exit(1)
        docs[rel] = new
    if args.check:
        print("doc-tables: OK (%d blocks)" % len(BLOCKS))
        return
    for rel, text in docs.items():
        with open(os.path.join(root, rel), "w") as f:
            f.write(text)
    print("doc-tables: wrote %d blocks in %d files" % (len(BLOCKS), len(docs)))


if __name__ == "__main__":
    main()
