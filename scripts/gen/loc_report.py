#!/usr/bin/env python3
"""Lines-of-code report for the ts-transformer tree.

Counts every git-tracked source file (so `target/`, the vendor submodules and
any untracked corpus are excluded automatically) and classifies each line as
one of:

  code     executable / declarative source
  doc      documentation comment (`///`, `//!`, `/** */`, Python docstring)
  comment  any other comment
  blank    whitespace only
  test     any line of a test file (tests/, benches/, fuzz/, src/test/,
           test_*.py) OR, in Rust, any line inside a `#[cfg(test)]` item —
           this is what separates inline unit tests from the production code
           they sit next to.

Files are grouped two ways: by COMPONENT (tst-core / tst-pipeline / one per
transport crate, subtotalled / sys / bindings/{c,python,jvm} / examples /
embedded / test-infra / scripts / ci / docs) and by UNIT (crate or binding
directory). The vendored libsrt and librist submodules are counted in a
separate section and never enter the ts-transformer totals.

Comment detection is a line-oriented state machine, not a parser. It tracks
block comments and `#[cfg(test)]` brace depth and ignores braces inside
strings and comments; raw strings with embedded quotes or a `/*` inside a
string literal can still mis-count a line or two. That is fine for a size
report — this is not a gate.

Usage:
  scripts/gen/loc_report.py                 # Markdown tables to stdout
  scripts/gen/loc_report.py --json          # machine-readable
  scripts/gen/loc_report.py --update docs/project/code-size.md
                                            # rewrite the <!-- loc:begin/end --> block
Run from anywhere inside the repository; stdlib only.
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import re
import subprocess
import sys
from collections import defaultdict

# --------------------------------------------------------------------------
# Language / file / component mapping
# --------------------------------------------------------------------------

_EXT_LANG = {
    ".rs": "rust",
    ".java": "java",
    ".kt": "java",
    ".kts": "java",
    ".py": "python",
    ".pyi": "python",
    ".c": "c",
    ".h": "c",
    ".cpp": "c",
    ".hpp": "c",
    ".cc": "c",
    ".sh": "shell",
    ".md": "markdown",
    ".yml": "yaml",
    ".yaml": "yaml",
    ".toml": "toml",
}

# Comment syntax per language: (line prefix, block open, block close, doc rules).
# `doc_line` prefixes are checked before `line`; `doc_block_open` before `block_open`.
_SYNTAX = {
    "rust": dict(line="//", doc_line=("///", "//!"), block=("/*", "*/"), doc_block_open=("/**", "/*!")),
    "java": dict(line="//", doc_line=(), block=("/*", "*/"), doc_block_open=("/**",)),
    "c": dict(line="//", doc_line=("///",), block=("/*", "*/"), doc_block_open=("/**",)),
    "python": dict(line="#", doc_line=(), block=None, doc_block_open=()),
    "shell": dict(line="#", doc_line=(), block=None, doc_block_open=()),
    "yaml": dict(line="#", doc_line=(), block=None, doc_block_open=()),
    "toml": dict(line="#", doc_line=(), block=None, doc_block_open=()),
    "markdown": dict(line=None, doc_line=(), block=("<!--", "-->"), doc_block_open=()),
}

_TRANSPORTS = ["tst-srt", "tst-rtp", "tst-udp", "tst-tcp", "tst-hls", "tst-rist"]

# Vendored C libraries counted as their own section: submodule path -> name.
# Only the two transport libraries; mbedTLS and the embedded RTOS trees are
# not counted (see docs/project/code-size.md).
VENDORED = {
    "crates/srt-sys/vendor/srt": "libsrt",
    "crates/rist-sys/vendor/librist": "librist",
}
_SYS = {"srt-sys", "rist-sys", "mbedtls-src"}
_TEST_INFRA = {"tst-test-helpers", "tst-integration", "tst-interop"}

_TEST_DIR_RE = re.compile(r"(^|/)(tests?|benches|fuzz)/")
_JAVA_TEST_RE = re.compile(r"(^|/)src/test/")
_PY_TEST_RE = re.compile(r"(^|/)test_[^/]*\.py$")


_PROSE_LANGS = {"markdown", "yaml", "toml"}


def counts_in_vendored(lang: str) -> bool:
    """Vendored trees contribute source only; their docs and CI config are
    upstream's concern and would only pad the numbers."""
    return lang not in _PROSE_LANGS


def group_of(component: str) -> str:
    """The display group a component rolls up into in the component table."""
    if component in _TRANSPORTS:
        return "transports"
    if component.startswith("vendor/"):
        return "vendor"
    return component


def language_for(path: str) -> str | None:
    return _EXT_LANG.get(os.path.splitext(path)[1])


def is_test_file(path: str) -> bool:
    return bool(_TEST_DIR_RE.search(path) or _JAVA_TEST_RE.search(path) or _PY_TEST_RE.search(path))


def _vendored_name(path: str) -> str | None:
    for prefix, name in VENDORED.items():
        if path.startswith(prefix + "/"):
            return name
    return None


def component_for(path: str) -> str:
    v = _vendored_name(path)
    if v:
        return f"vendor/{v}"
    parts = path.split("/")
    top = parts[0]
    if top == "crates" and len(parts) > 1:
        crate = parts[1]
        if crate in ("tst-core", "tst-pipeline"):
            return crate
        if crate in _TRANSPORTS:
            return crate
        if crate in _SYS:
            return "sys"
        if crate in _TEST_INFRA:
            return "test-infra"
        return "other"
    if top == "bindings" and len(parts) > 1:
        return f"bindings/{parts[1]}"
    if top == "examples":
        return "examples"
    if top == "embedded":
        return "scripts" if len(parts) > 1 and parts[1] == "scripts" else "embedded"
    if top in ("scripts", "oss-fuzz"):
        return "scripts"
    if top == ".github":
        return "ci"
    if top == "tests" and not path.endswith(".md"):
        return "test-infra"  # root-level manifests + goldens
    if top == "docs" or path.endswith(".md"):
        return "docs"
    return "other"


def unit_for(path: str) -> str:
    v = _vendored_name(path)
    if v:
        return f"vendor/{v}"
    parts = path.split("/")
    top = parts[0]
    if top in ("crates",) and len(parts) > 1:
        return parts[1]
    if top in ("bindings", "embedded") and len(parts) > 2:
        return f"{top}/{parts[1]}"
    return top if len(parts) > 1 else "(root)"


# --------------------------------------------------------------------------
# Line classification
# --------------------------------------------------------------------------

_CHAR_LIT_RE = re.compile(r"'(\\u\{[0-9a-fA-F]+\}|\\.|[^\\'])'")


def _strip_strings(s: str) -> str:
    """Blank out the contents of double-quoted string literals and of
    single-quoted char literals (`'}'`, `'"'`, `'\\''`) so brace and comment
    scanning does not see delimiters inside them. Lifetimes (`'a`) have no
    closing quote and pass through untouched. Raw strings with embedded
    quotes (`r#"..."#`) are not understood and can still mis-count a line."""
    out = []
    i, n = 0, len(s)
    while i < n:
        ch = s[i]
        if ch == "'":
            m = _CHAR_LIT_RE.match(s, i)
            if m:
                out.append("''")
                i = m.end()
                continue
            out.append(ch)
            i += 1
        elif ch == '"':
            out.append('"')
            i += 1
            while i < n and s[i] != '"':
                if s[i] == "\\":
                    i += 1
                i += 1
            out.append('"')
            i += 1
        else:
            out.append(ch)
            i += 1
    return "".join(out)


def _c_like(lang: str, lines: list[str]):
    """Yield (klass, code_text) per line for //-and-/* */ languages.
    code_text is the line with comments and string contents removed,
    for brace scanning by the Rust cfg(test) tracker."""
    syn = _SYNTAX[lang]
    b_open, b_close = syn["block"]
    in_block = False
    block_is_doc = False
    for raw in lines:
        s = raw.strip()
        if not s and not in_block:
            yield "blank", ""
            continue
        klass_bits = set()
        code_text = ""
        i = 0
        line = _strip_strings(raw)
        while i < len(line):
            if in_block:
                j = line.find(b_close, i)
                klass_bits.add("doc" if block_is_doc else "comment")
                if j < 0:
                    i = len(line)
                else:
                    in_block = False
                    i = j + len(b_close)
                continue
            rest = line[i:].lstrip()
            if not rest:
                break
            if syn["line"] and rest.startswith(syn["line"]):
                klass_bits.add("doc" if rest.startswith(syn["doc_line"]) else "comment")
                break
            if rest.startswith(b_open):
                in_block = True
                # `/**/` is an empty plain comment, not a doc comment
                block_is_doc = rest.startswith(syn["doc_block_open"]) and not rest.startswith(b_open + b_close)
                i = len(line) - len(rest) + len(b_open)
                continue
            # code up to the next comment opener
            nxt = len(line)
            for tok in filter(None, (syn["line"], b_open)):
                k = line.find(tok, i)
                if 0 <= k < nxt:
                    nxt = k
            code_text += line[i:nxt]
            klass_bits.add("code")
            i = nxt
        if "code" in klass_bits:
            klass = "code"
        elif "doc" in klass_bits:
            klass = "doc"
        elif "comment" in klass_bits:
            klass = "comment"
        else:
            klass = "blank"
        yield klass, code_text


def _hash_lang(lang: str, lines: list[str]):
    for idx, raw in enumerate(lines):
        s = raw.strip()
        if not s:
            yield "blank", ""
        elif s.startswith("#") and not (idx == 0 and s.startswith("#!")):
            yield "comment", ""
        else:
            yield "code", s


def _python(lines: list[str]):
    """Hash comments plus docstrings: a string literal that is the first
    statement after a `def`/`class` line or at module top is `doc`."""
    expect_doc = True  # module docstring may come first
    in_doc = None      # closing quote while inside a multi-line docstring
    for idx, raw in enumerate(lines):
        s = raw.strip()
        if in_doc is not None:
            yield "doc", ""
            if in_doc in s:
                in_doc = None
            continue
        if not s:
            yield "blank", ""
            continue
        if s.startswith("#") and not (idx == 0 and s.startswith("#!")):
            yield "comment", ""
            continue
        m = re.match(r'^[rubRUB]*("""|\'\'\')', s)
        if expect_doc and m:
            q = m.group(1)
            body = s[m.end():]
            yield "doc", ""
            if q not in body:
                in_doc = q
            expect_doc = False
            continue
        expect_doc = bool(re.match(r"^(async\s+)?(def|class)\b", s)) and s.rstrip().endswith(":")
        yield "code", s


def _markdown(lines: list[str]):
    in_block = False
    for raw in lines:
        s = raw.strip()
        if in_block:
            yield "comment", ""
            if "-->" in s:
                in_block = False
        elif not s:
            yield "blank", ""
        elif s.startswith("<!--"):
            yield "comment", ""
            in_block = "-->" not in s
        else:
            yield "code", s


def _rust(lines: list[str]):
    """C-like classification plus `#[cfg(test)]` tracking: the attribute
    marks the next item as test; a brace-delimited item runs to its matching
    close, otherwise to the terminating `;`."""
    pending = False       # saw #[cfg(test)], waiting for the item to start
    in_item = False       # inside the test item
    seen_brace = False    # the item is brace-delimited (else `;`-terminated)
    depth = 0
    for klass, code in _c_like("rust", lines):
        if in_item:
            yield "test"
            opens, closes = code.count("{"), code.count("}")
            if opens or closes:
                seen_brace = True
                depth += opens - closes
            if (seen_brace and depth <= 0) or (not seen_brace and ";" in code):
                in_item = False
            continue
        if pending:
            yield "test"
            if klass in ("blank", "comment", "doc"):
                continue  # attributes may be followed by doc comments / blank lines
            if re.match(r"\s*#\[", code):
                continue  # further attributes on the same item
            pending = False
            opens, closes = code.count("{"), code.count("}")
            if opens:
                seen_brace, depth = True, opens - closes
                in_item = depth > 0
            elif ";" not in code:
                seen_brace, depth, in_item = False, 0, True  # signature continues
            continue
        if re.match(r"\s*#\[\s*cfg\s*\(\s*test\s*\)\s*\]", code):
            pending = True
            yield "test"
            continue
        yield klass


def classify_lines(lang: str, lines: list[str], path: str = "") -> list[str]:
    if lang == "rust":
        klasses = list(_rust(lines))
    elif lang in ("java", "c"):
        klasses = [k for k, _ in _c_like(lang, lines)]
    elif lang == "python":
        klasses = [k for k, _ in _python(lines)]
    elif lang == "markdown":
        klasses = [k for k, _ in _markdown(lines)]
    elif lang in ("shell", "yaml", "toml"):
        klasses = [k for k, _ in _hash_lang(lang, lines)]
    else:
        raise ValueError(f"unknown language {lang}")
    if lang != "markdown" and is_test_file(path):
        klasses = ["test"] * len(klasses)  # prose stays prose, even under tests/
    return klasses


CLASSES = ("code", "comment", "doc", "blank", "test")


def count_lines(lang: str, path: str, lines: list[str]) -> dict[str, int]:
    counts = dict.fromkeys(CLASSES, 0)
    for k in classify_lines(lang, lines, path):
        counts[k] += 1
    return counts


# --------------------------------------------------------------------------
# Tree walk + aggregation
# --------------------------------------------------------------------------

def repo_root() -> str:
    return subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip()


def tracked_files(root: str, prefix: str = "") -> list[str]:
    """git-tracked paths of the repository at `root`/`prefix`, returned
    relative to `root` (so submodule files carry their submodule prefix)."""
    repo = os.path.join(root, prefix) if prefix else root
    out = subprocess.check_output(["git", "-C", repo, "ls-files", "-z"], text=True)
    return [os.path.join(prefix, p) if prefix else p for p in out.split("\0") if p]


def _count_file(root: str, rel: str) -> dict | None:
    lang = language_for(rel)
    if lang is None:
        return None
    full = os.path.join(root, rel)
    if not os.path.isfile(full):  # submodule gitlinks etc.
        return None
    try:
        with open(full, encoding="utf-8", errors="replace") as fh:
            lines = fh.read().splitlines()
    except OSError:
        return None
    return {
        "path": rel,
        "lang": lang,
        "component": component_for(rel),
        "unit": unit_for(rel),
        "test_file": is_test_file(rel),
        **count_lines(lang, rel, lines),
    }


def _submodule_tag(root: str, prefix: str) -> str:
    repo = os.path.join(root, prefix)
    try:
        return subprocess.check_output(
            ["git", "-C", repo, "describe", "--tags", "--always"], text=True, stderr=subprocess.DEVNULL
        ).strip()
    except subprocess.CalledProcessError:
        return "?"


def scan(root: str) -> dict:
    files = [f for f in (_count_file(root, rel) for rel in tracked_files(root)) if f]
    vendored = []
    for prefix, name in VENDORED.items():
        if not os.path.isdir(os.path.join(root, prefix, ".git")) and not os.path.isfile(os.path.join(root, prefix, ".git")):
            continue  # submodule not checked out
        vendored.append({"name": name, "path": prefix, "tag": _submodule_tag(root, prefix)})
        files += [f for f in (_count_file(root, rel) for rel in tracked_files(root, prefix))
                  if f and counts_in_vendored(f["lang"])]
    return {
        "generated_utc": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "commit": subprocess.check_output(["git", "-C", root, "rev-parse", "--short", "HEAD"], text=True).strip(),
        "vendored": vendored,
        "files": files,
    }


def _rollup(files, key):
    """Sum the line classes per `key(file)`; key may return any hashable."""
    agg = defaultdict(lambda: {**dict.fromkeys(CLASSES, 0), "files": 0})
    for f in files:
        row = agg[key(f)]
        row["files"] += 1
        for c in CLASSES:
            row[c] += f[c]
    return agg


_COMPONENT_ORDER = [
    "tst-core", "tst-pipeline", *_TRANSPORTS, "sys", "bindings/c", "bindings/python", "bindings/jvm",
    "examples", "embedded", "test-infra", "scripts", "ci", "docs", "other",
]
_VENDOR_ORDER = [f"vendor/{n}" for n in VENDORED.values()]
_COMPONENT_ORDER += _VENDOR_ORDER


def unit_rows(files):
    """(component, unit, counts) per crate / binding directory, in component
    order. A directory whose files span two components (root `tests/` holds
    Markdown and TOML manifests) gets one row per component."""
    agg = _rollup(files, lambda f: (f["component"], f["unit"]))
    return [(c, u, agg[(c, u)]) for c, u in sorted(agg, key=lambda k: (_COMPONENT_ORDER.index(k[0]), k[1]))]


def _fmt(n: int) -> str:
    return f"{n:,}"


def _table(header: list[str], rows: list[list[str]]) -> str:
    out = ["| " + " | ".join(header) + " |", "|" + "|".join(["---"] + ["---:"] * (len(header) - 1)) + "|"]
    out += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(out) + "\n"


def _row(name: str, r: dict) -> list[str]:
    total = sum(r[c] for c in CLASSES)
    return [name, _fmt(r["files"]), _fmt(r["code"]), _fmt(r["test"]), _fmt(r["doc"]), _fmt(r["comment"]), _fmt(r["blank"]), _fmt(total)]


_HEADER = ["", "files", "code", "test", "doc", "comment", "blank", "total"]


def _sum_rows(rows):
    out = {**dict.fromkeys(CLASSES, 0), "files": 0}
    for r in rows:
        out["files"] += r["files"]
        for c in CLASSES:
            out[c] += r[c]
    return out


def render_markdown(report: dict) -> str:
    own = [f for f in report["files"] if not f["component"].startswith("vendor/")]
    vend = [f for f in report["files"] if f["component"].startswith("vendor/")]
    by_comp = _rollup(own, lambda f: f["component"])
    by_lang = _rollup(own, lambda f: f["lang"])

    parts = [f"_Generated {report['generated_utc']} at commit `{report['commit']}` by `scripts/gen/loc-report.sh`._\n"]

    parts.append("### By component\n")
    rows = []
    for k in _COMPONENT_ORDER:
        if k not in by_comp or k.startswith("vendor/"):
            continue
        if k in _TRANSPORTS:
            if k == _TRANSPORTS[0]:  # expand the whole group here, then its subtotal
                present = [t for t in _TRANSPORTS if t in by_comp]
                rows += [_row(f"&nbsp;&nbsp;{t}", by_comp[t]) for t in present]
                rows.append(_row("**transports**", _sum_rows(by_comp[t] for t in present)))
            continue
        rows.append(_row(k, by_comp[k]))
    rows.append(_row("**total (ts-transformer)**", _sum_rows(by_comp.values())))
    parts.append(_table(_HEADER, rows))

    parts.append("### By crate / binding\n")
    rows = [_row(f"`{u}` ({group_of(c)})", r) for c, u, r in unit_rows(own)]
    parts.append(_table(_HEADER, rows))

    parts.append("### By language\n")
    rows = [_row(k, by_lang[k]) for k in sorted(by_lang, key=lambda k: -by_lang[k]["code"] - by_lang[k]["test"])]
    parts.append(_table(_HEADER, rows))

    parts.append("### Vendored libraries\n")
    parts.append("Counted from the pinned submodule checkouts; not included in any total above.\n")
    by_vendor = _rollup(vend, lambda f: f["component"])
    rows = []
    for v in report.get("vendored", []):
        r = by_vendor.get(f"vendor/{v['name']}")
        if r:
            rows.append([v["name"], f"`{v['tag']}`"] + _row("", r)[1:])
    rows.append(["**total (vendored)**", ""] + _row("", _sum_rows(by_vendor.values()))[1:])
    parts.append(_table(["library", "pinned", *_HEADER[1:]], rows))

    return "\n".join(parts)


_BEGIN, _END = "<!-- loc:begin -->", "<!-- loc:end -->"


def replace_block(doc: str, body: str) -> str:
    b = doc.find(_BEGIN)
    e = doc.find(_END)
    if b < 0 or e < 0 or e < b:
        raise ValueError(f"document lacks {_BEGIN} / {_END} markers")
    b += len(_BEGIN)
    return doc[:b] + "\n" + body + doc[e:]


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--json", action="store_true", help="emit the full per-file report as JSON")
    ap.add_argument("--update", metavar="MD", help="rewrite the loc block in this Markdown file")
    args = ap.parse_args(argv)

    report = scan(repo_root())
    if args.json:
        json.dump(report, sys.stdout, indent=1)
        sys.stdout.write("\n")
        return 0
    md = render_markdown(report)
    if args.update:
        with open(args.update, encoding="utf-8") as fh:
            doc = fh.read()
        with open(args.update, "w", encoding="utf-8") as fh:
            fh.write(replace_block(doc, md))
        print(f"updated {args.update} ({len(report['files'])} files counted)")
        return 0
    sys.stdout.write(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
