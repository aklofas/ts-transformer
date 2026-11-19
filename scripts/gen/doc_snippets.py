#!/usr/bin/env python3
"""Extract the marked Java and C code blocks of the docs so CI can compile them.

A fenced block is compiled only when the line right before its opening fence
is an HTML comment marker:

  <!-- compile: java-statements -->   statements, wrapped in a class with one
                                      `static void run() throws Exception`
                                      and wildcard imports of the
                                      `org.tstrans` packages (see below)
  <!-- compile: java-class -->        a whole compilation unit, written as is
  <!-- compile: c-statements -->      statements, wrapped in one `int` function
                                      (ending `return 0;`, so a snippet may
                                      `return 1;`) after `#include "tstrans.h"`
                                      and the libc headers
  <!-- compile: c-file -->            a whole translation unit, written as is

A page may declare placeholders its snippets assume (`klvBytes`, a `demuxer`)
in an invisible comment, one per language:

  <!-- compile-prelude: java
  byte[] klvBytes = new byte[0];
  -->

For Java the lines are members of the wrapper class, so write `static byte[]
klvBytes;` and a snippet may still declare its own local of the same name (an
`import` line is hoisted above the class); for C they go at file scope, after
the includes, as non-static globals so an unused one is not a warning. The
default Java imports are the wildcard of the `org.tstrans` packages that share no type name with another
(`org.tstrans`, `codec`, `hls`, `io`, `klv`, `mpegts`, `pipeline`) plus
`java.nio`, `java.time` and `java.util`, but only for a snippet with no `import`
line of its own: one that lists imports gets exactly those. A snippet that uses `srt`, `tcp`,
`rtp`, `shells` or `webrtc` types carries its own `import` lines, as a
reader's code must.

`import` lines inside a Java `-statements` block are
hoisted above the class. An unmarked fence is ignored; a marker that is not
followed by a fence of the matching language, or any other HTML comment that
starts with `compile`, is an error.

Output: for each marked block `<out>/<page>-<n>.java|.c`, `n` counting the
marked blocks of that page from 1. A `java-class` block whose public type is named is written as
`<out>/<page>-<n>/<Type>.java` so javac accepts it.

Usage:  doc_snippets.py extract --out DIR PAGE.md [PAGE.md ...]
Prints the written paths, one per line. Stdlib only.
"""
import argparse
import os
import re
import sys

MARKER = re.compile(
    r"^<!--\s*compile:\s*(java-statements|java-class|c-statements|c-file)"
    r"\s*-->\s*$"
)
MARKER_ANY = re.compile(r"^\s*<!--\s*compile\b", re.I)
PRELUDE = re.compile(r"^<!--\s*compile-prelude:\s*(java|c)\s*$")
FENCE = re.compile(r"^```(\w*)\s*$")
LANG = {"java-statements": "java", "java-class": "java", "c-statements": "c", "c-file": "c"}

JAVA_PACKAGES = ["", ".codec", ".hls", ".io", ".klv", ".mpegts", ".pipeline"]
JAVA_IMPORTS = "".join(f"import org.tstrans{p}.*;\n" for p in JAVA_PACKAGES) + (
    "import java.nio.*;\nimport java.nio.file.*;\nimport java.time.*;\n"
    "import java.util.*;\n"
)
# An excerpt may declare a variable for the reader and use it in elided lines.
C_EXCERPT_PRAGMAS = (
    '#pragma GCC diagnostic ignored "-Wunused-variable"\n'
    '#pragma GCC diagnostic ignored "-Wunused-but-set-variable"\n'
)
C_INCLUDES = (
    '#include "tstrans.h"\n#include <stdbool.h>\n#include <stdint.h>\n'
    "#include <stdio.h>\n#include <stdlib.h>\n#include <string.h>\n"
)


class SnippetError(Exception):
    pass


def blocks(path):
    """Yield (kind, code, line_no) for each marked block of a page."""
    with open(path, encoding="utf-8") as fh:
        lines = fh.read().split("\n")
    i = 0
    fence_run = 0  # backticks of the open unmarked fence, 0 outside one
    while i < len(lines):
        line = lines[i]
        if fence_run:
            if re.match(r"^`{%d,}\s*$" % fence_run, line):
                fence_run = 0
            i += 1
            continue
        pm = PRELUDE.match(line)
        if pm:
            j = i + 1
            while j < len(lines) and lines[j].strip() != "-->":
                j += 1
            if j >= len(lines):
                raise SnippetError(f"{path}:{i + 1}: prelude comment is never closed")
            yield "prelude-" + pm.group(1), "\n".join(lines[i + 1 : j]) + "\n", i + 1
            i = j + 1
            continue
        if MARKER_ANY.match(line):
            m = MARKER.match(line)
            if not m:
                raise SnippetError(f"{path}:{i + 1}: unknown compile marker: {line}")
            kind = m.group(1)
            f = FENCE.match(lines[i + 1]) if i + 1 < len(lines) else None
            if not f or f.group(1) != LANG[kind]:
                raise SnippetError(
                    f"{path}:{i + 1}: marker `{kind}` is not followed by a ```{LANG[kind]} fence"
                )
            j = i + 2
            while j < len(lines) and not re.match(r"^```\s*$", lines[j]):
                j += 1
            if j >= len(lines):
                raise SnippetError(f"{path}:{i + 1}: fence is never closed")
            yield kind, "\n".join(lines[i + 2 : j]) + "\n", i + 1
            i = j + 1
            continue
        opened = re.match(r"^(`{3,})", line)
        if opened:
            fence_run = len(opened.group(1))
        i += 1


def wrap_java(name, code, prelude=""):
    imports, members, body = [], [], []
    for ln in prelude.split("\n"):
        (imports if ln.startswith("import ") else members).append(ln)
    for ln in code.split("\n"):
        (imports if ln.startswith("import ") else body).append(ln)
    own = any(ln.startswith("import ") for ln in code.split("\n"))
    return (
        ("" if own else JAVA_IMPORTS) + "\n".join(imports) + ("\n" if imports else "")
        + f"class {name} {{\n" + "\n".join(members) + "\n"
        + "    static void run() throws Exception {\n"
        + "\n".join(body) + "    }\n}\n"
    )


def wrap_c(name, code, prelude=""):
    return C_INCLUDES + C_EXCERPT_PRAGMAS + prelude + f"int {name}(void) {{\n{code}    return 0;\n}}\n"


def extract(pages, out):
    os.makedirs(out, exist_ok=True)
    written = []
    for page in pages:
        stem = os.path.splitext(os.path.basename(page))[0]
        n = 0
        prelude = {"java": "", "c": ""}
        pending = None  # [kind, name, code, path] of the statements block being built

        def flush():
            nonlocal pending
            if pending is None:
                return
            kind, name, code, path = pending
            text = (wrap_java(name, code, prelude["java"]) if kind == "java-statements"
                    else wrap_c(name, code, prelude["c"]))
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(text)
            written.append(path)
            pending = None

        for kind, code, line_no in blocks(page):
            if kind.startswith("prelude-"):
                prelude[kind[8:]] += code
                continue
            flush()
            n += 1
            ext = LANG[kind]
            ident = re.sub(r"\W", "_", f"{stem}_{n}")
            if kind.endswith("statements"):
                pending = [kind, f"Snippet_{ident}" if ext == "java" else f"snippet_{ident}",
                           code, os.path.join(out, f"{stem}-{n}.{ext}")]
            elif kind == "java-class":
                m = re.search(r"^public\s+(?:final\s+|abstract\s+)*(?:class|interface|record|enum)\s+(\w+)",
                              code, re.M)
                if m:
                    d = os.path.join(out, f"{stem}-{n}")
                    os.makedirs(d, exist_ok=True)
                    path = os.path.join(d, m.group(1) + ".java")
                else:
                    path = os.path.join(out, f"{stem}-{n}.java")
                with open(path, "w", encoding="utf-8") as fh:
                    fh.write(code)
                written.append(path)
            else:
                path = os.path.join(out, f"{stem}-{n}.c")
                with open(path, "w", encoding="utf-8") as fh:
                    fh.write(code)
                written.append(path)
        flush()
    return written


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    sub = ap.add_subparsers(dest="cmd", required=True)
    ex = sub.add_parser("extract")
    ex.add_argument("--out", required=True)
    ex.add_argument("pages", nargs="+")
    args = ap.parse_args(argv)
    try:
        for p in extract(args.pages, args.out):
            print(p)
    except SnippetError as e:
        print(f"doc-snippets: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
