#!/usr/bin/env bash
# Deprecation manifest rail.
#
# tests/coverage/deprecations.toml has one row per deprecated public name:
# the release that deprecated it, its replacement, the earliest release that
# may remove it and, once it is gone, the release that removed it. This rail
# keeps the code, the manifest, the CHANGELOG and the docs saying the same
# thing, so a deprecation cannot promise a removal in a release that ships
# with the name still in it.
#
# A deprecation SITE is a declaration or statement, read together with the
# comment, attribute, annotation and decorator lines directly above it, that
# carries one of these markers:
#   rust    `#[deprecated ...]`, also as `#[cfg_attr(<cond>, deprecated ...)]`,
#           in a .rs file under crates/ or bindings/
#   java    `@Deprecated`, or a Javadoc `@deprecated` tag, in a .java file
#           under bindings/jvm/src/main
#   python  in a .py or .pyi file under bindings/python/python: a `#` comment
#           with the words "deprecated alias" (any case), a `@deprecated`
#           decorator, or a `warn(...)` call that names DeprecationWarning
#           (PendingDeprecationWarning included); and in a .rs file under
#           bindings/python/src: a statement that names PyDeprecationWarning
#           or PyPendingDeprecationWarning
#   c       none is defined: the C header deprecates nothing. A compiler or
#           Doxygen deprecation marker in bindings/c/include/tstrans.h fails
#           until this rail has a rule for it.
#
# A row COVERS a site of its language when the site lies in the row's scope
# and names the row's last component (`Klv` of `tstrans.mpegts.DemuxEvent.Klv`):
#   scope   rust: the symbol starts with the crate of the file (`tst_srt::`);
#           java: the symbol is the class of the file or one of its members;
#           python: the symbol starts with the module of the file
#           (`tstrans.mpegts.`), or with `tstrans.` for a site in the Rust
#           sources of the extension module
#   name    the declared name is the last component (an item, variant or
#           field after `#[deprecated]`; a Java class, method or field; a
#           Python class, def or assignment target). A site that declares
#           nothing (a warning call) names it as a whole word in its message
#           or in a comment directly above the statement. The row of a Java
#           class covers the deprecated members of that class too.
#
# Enforced (fails on violation):
#   (a) every site is covered by a live row (a row without `removed_in`);
#   (b) every live row covers a site, and where the site states `since = "X"`
#       (Rust, Java) X equals the row's `since`;
#   (c) a live row's `earliest_removal` is later than the workspace version
#       ([workspace.package] version in Cargo.toml), and every removal
#       version written at a site ("removed in X.Y.Z", "remove at X.Y",
#       "removal in X.Y.Z", "removal no earlier than X.Y") is later than the
#       workspace version and equals the `earliest_removal` of a row that
#       covers the site;
#   (d) a removed row covers no site; a Rust row's path is in no
#       crates/*/public-api.txt and a Java row's class file does not exist
#       (Python names are not resolved: the binding's tests assert those);
#       `removed_in` is not earlier than `earliest_removal`; and CHANGELOG.md
#       names the row's last component under a `### Removed` heading that
#       names its package (the crate with hyphens, `tst-py`, `tst-jni`,
#       `tst-c`), in the `[removed_in]` section, or in `[Unreleased]` while
#       that section does not exist and `removed_in` is not behind the
#       workspace version;
#   (e) the docs agree: a paragraph or table row that names a row's symbol in
#       a code span and states a removal version states the row's
#       (`earliest_removal` of a live row, `removed_in` of a removed one).
#       Read: docs/reference/api-stability.md and docs/languages/rust.md for
#       Rust rows, docs/languages/python.md for Python, jvm.md for Java, c.md
#       for C, where the code span is the symbol or a tail of it down to the
#       bare name; and docs/guides/*.md for every row, where the span must
#       keep at least the owner (`DemuxEvent.Klv`, `mux::StreamKind`). A
#       Rust row is compared only where the passage also names its crate
#       (`tst-srt` or `tst_srt`).
# The manifest must parse and every row must be well-formed. The summary
# prints what was read and compared; a run that read no Rust, no Java or no
# Python source file, or found no C header, fails.
#
# Limits: a deprecation written with none of the markers above is not seen,
# and a warning raised through a helper function is one site (the helper),
# so each name that warns through it is named in a comment above the call
# inside the helper.
#
# Run it:        bash scripts/check/repo/deprecations.sh
# What it sees:  bash scripts/check/repo/deprecations.sh --list
#                (every site with the rows that cover it, every removal
#                version read at a site, and every docs passage compared)
#
# A new deprecation: mark it in the code, run the rail, and add the row its
# FAIL line asks for. A removal: delete the code, add `removed_in` to the
# row and list the name under `### Removed` in the CHANGELOG.
#
# DEPRECATIONS_ROOT overrides the repository root (the self-test in
# scripts/ratchets/tests/self_test.sh points it at fixture trees).
# Needs python3 >= 3.11 (tomllib); CI runs it on linux-x86_64 only.
set -euo pipefail

ROOT="${DEPRECATIONS_ROOT:-$(cd "$(dirname "$0")/../../.." && pwd)}"
case "${1:-}" in
  ""|--list) ;;
  *) echo "usage: $0 [--list]" >&2; exit 2 ;;
esac

python3 - "$ROOT" "${1:-}" <<'PY'
import os
import re
import sys

try:
    import tomllib
except ImportError:
    print("FAIL: deprecations needs python3 >= 3.11 (tomllib)", file=sys.stderr)
    sys.exit(1)

root = sys.argv[1]
listing = sys.argv[2] == "--list"
MANIFEST = "tests/coverage/deprecations.toml"
LANGS = ("rust", "python", "java", "c")
PACKAGE = {"python": "tst-py", "java": "tst-jni", "c": "tst-c"}
failures = []


def err(msg):
    failures.append(msg)
    print(f"FAIL: {msg}", file=sys.stderr)


def read(rel):
    with open(os.path.join(root, rel), encoding="utf-8") as f:
        return f.read()


def version(text):
    """X.Y.Z (or X.Y, read as X.Y.0) as a tuple; None for anything else."""
    m = re.fullmatch(r"(\d+)\.(\d+)(?:\.(\d+))?", text or "")
    return tuple(int(g or 0) for g in m.groups()) if m else None


def show(v):
    return ".".join(map(str, v))


def word(name, text):
    return re.search(r"(?<!\w)" + re.escape(name) + r"(?!\w)", text) is not None


def plain(symbol):
    """The symbol without a constructor's `()` or a call's arguments."""
    return symbol.split("(")[0]


def leaf(symbol):
    return re.split(r"::|\.", plain(symbol))[-1]


# ---- inputs ---------------------------------------------------------------

try:
    with open(os.path.join(root, "Cargo.toml"), "rb") as f:
        workspace = version(tomllib.load(f)["workspace"]["package"]["version"])
except (OSError, KeyError, TypeError, tomllib.TOMLDecodeError) as e:
    workspace = None
    err(f"Cargo.toml: no [workspace.package] version ({e})")
if workspace is None:
    sys.exit(1)

try:
    with open(os.path.join(root, MANIFEST), "rb") as f:
        raw_rows = tomllib.load(f).get("deprecation", [])
except OSError as e:
    err(f"{MANIFEST}: {e.strerror}")
    sys.exit(1)
except tomllib.TOMLDecodeError as e:
    err(f"{MANIFEST}: {e}")
    sys.exit(1)
if not isinstance(raw_rows, list):
    err(f"{MANIFEST}: deprecation must be an array of tables ([[deprecation]])")
    sys.exit(1)

FIELDS = ("lang", "symbol", "since", "replacement", "earliest_removal", "removed_in")
rows, seen = [], set()
for i, r in enumerate(raw_rows, 1):
    where = f"{MANIFEST}: row {i}"
    if not isinstance(r, dict):
        err(f"{where} is not a table")
        continue
    where += f" ({r.get('symbol')})"
    bad = [k for k in r if k not in FIELDS]
    bad += [k for k in FIELDS[:5] if not isinstance(r.get(k), str) or not r[k]]
    if bad:
        err(f"{where}: missing, empty or unknown field: {', '.join(bad)}")
        continue
    if r["lang"] not in LANGS:
        err(f"{where}: lang is one of {', '.join(LANGS)}")
        continue
    since, earliest = version(r["since"]), version(r["earliest_removal"])
    removed = version(r["removed_in"]) if "removed_in" in r else None
    if since is None or earliest is None or ("removed_in" in r and removed is None):
        err(f"{where}: since, earliest_removal and removed_in are versions X.Y.Z")
        continue
    if earliest <= since:
        err(f"{where}: earliest_removal {r['earliest_removal']} is not later than since {r['since']}")
    if (r["lang"], r["symbol"]) in seen:
        err(f"{where}: a second row for this {r['lang']} symbol")
    seen.add((r["lang"], r["symbol"]))
    rows.append(dict(r, since_v=since, earliest_v=earliest, removed_v=removed, leaf=leaf(r["symbol"])))
if failures:
    sys.exit(1)
live = [r for r in rows if r["removed_v"] is None]
gone = [r for r in rows if r["removed_v"] is not None]

# ---- the sites ------------------------------------------------------------

PRUNE = {"vendor", "target", "node_modules", "__pycache__"}  # and dot directories
COMMENT = {"rust": ("//",), "java": ("//", "/*", "*"), "python": ("#",)}
TRAILING = {"rust": "//", "java": "//", "python": "#"}
RUST_ATTR = re.compile(r"#\[\s*(?:deprecated\b|cfg_attr\([^\]]*,\s*deprecated\b)")
PROMISE = re.compile(
    r"\b(?:removed?|removal)\s+(?:no\s+earlier\s+than|in|at|by|from)\s+v?(\d+\.\d+(?:\.\d+)?)", re.I
)
C_MARKER = re.compile(
    r"__attribute__\s*\(\(\s*deprecated|\[\[\s*deprecated|__declspec\s*\(\s*deprecated"
    r"|[@\\]deprecated\b|_DEPRECATED\b"
)
scanned = dict.fromkeys(("rust", "java", "python"), 0)
sites = []


def files(top, suffixes):
    for d, dirs, names in os.walk(os.path.join(root, top)):
        dirs[:] = sorted(x for x in dirs if x not in PRUNE and not x.startswith("."))
        for n in sorted(names):
            if n.endswith(suffixes):
                yield os.path.relpath(os.path.join(d, n), root)


STRING = re.compile(r'"(?:[^"\\]|\\.)*"')


def units(text, syntax):
    """(line, comment lines, code lines above, declaration lines) of every
    declaration or statement of a file. A code line runs on while a `(` or
    `[` it opened stays open; the lines above are the comment and
    attribute/annotation/decorator lines since the last blank line or
    declaration."""
    lines = text.split("\n")
    comments, above, i = [], [], 0
    while i < len(lines):
        s = lines[i].strip()
        if not s:
            comments, above = [], []
        elif s.startswith(COMMENT[syntax]):
            comments.append(s)
        else:
            first, logical, depth = i, [], 0
            while True:
                # A string literal is emptied first: the `//` of a URL in one is
                # not a comment, and a bracket in one opens nothing.
                code = STRING.sub('""', lines[i]).split(TRAILING[syntax])[0]
                depth += sum(code.count(c) for c in "([") - sum(code.count(c) for c in ")]")
                logical.append(lines[i].strip())
                if depth <= 0 or i + 1 == len(lines) or i - first == 39:
                    break
                i += 1
            joined = " ".join(logical)
            if syntax == "rust":
                is_above = s.startswith("#[") and joined.endswith("]")
            elif syntax == "java":
                is_above = re.fullmatch(r"(@[\w.]+(\([^()]*\))?\s*)+", joined) is not None
            else:
                is_above = s.startswith("@")
            if is_above:
                above.append(joined)
            else:
                yield first + 1, comments, above, logical
                comments, above = [], []
        i += 1


def crate_of(rel):
    """The library name of the crate a .rs file belongs to."""
    d = os.path.dirname(rel)
    while d:
        try:
            with open(os.path.join(root, d, "Cargo.toml"), "rb") as f:
                doc = tomllib.load(f)
            name = doc.get("lib", {}).get("name") or doc.get("package", {}).get("name")
            if name:
                return name.replace("-", "_")
        except (OSError, tomllib.TOMLDecodeError):
            pass
        d = os.path.dirname(d)
    return ""


def rust_name(decl):
    for pat in (r"\bfn\s+(\w+)", r"\b(?:type|struct|enum|trait|union|mod)\s+(\w+)",
                r"\b(?:const|static)\s+(?:mut\s+)?(\w+)", r"\bmacro_rules!\s+(\w+)",
                r"^(?:#\[.*\]\s*)?(?:pub(?:\([^)]*\))?\s+)?(\w+)"):
        m = re.search(pat, decl)
        if m:
            return m.group(1)
    return None


def java_name(decl):
    decl = re.sub(r"@[\w.]+(\([^()]*\))?", "", decl)
    for pat in (r"\b(?:class|interface|enum|record)\s+(\w+)", r"(\w+)\s*\(", r"(\w+)\s*(?:=|;)"):
        m = re.search(pat, decl)
        if m:
            return m.group(1)
    return None


def python_name(decl):
    m = re.match(r"(?:async\s+)?(?:class|def)\s+(\w+)", decl) or re.match(r"([\w.]+)\s*(?::|=)(?!=)", decl)
    return m.group(1).split(".")[-1] if m else None


def site(lang, rel, line, scope, name, kind, comments, above, decl):
    text = " ".join(c.lstrip("/#*! ") for c in comments + above + decl)
    since = re.search(r'\bsince\s*=\s*"([^"]*)"', " ".join(above + decl))
    sites.append({
        "lang": lang, "file": rel, "line": line, "scope": scope, "name": name, "kind": kind,
        "text": text, "since": since.group(1) if since else None,
        "promises": [version(v) for v in PROMISE.findall(text)], "first": decl[0],
    })


for rel in sorted(set(files("crates", ".rs")) | set(files("bindings", ".rs"))):
    scanned["rust"] += 1
    pyo3 = rel.startswith("bindings/python/src/")
    text = read(rel)
    if "deprecated" not in text and "DeprecationWarning" not in text:
        continue
    for line, comments, above, decl in units(text, "rust"):
        code = " ".join(above + [d.split("//")[0] for d in decl])
        if RUST_ATTR.search(code):
            site("rust", rel, line, crate_of(rel), rust_name(decl[0]), "#[deprecated]", comments, above, decl)
        if pyo3 and "DeprecationWarning" in code and not decl[0].startswith("use "):
            site("python", rel, line, "tstrans", None, "PyDeprecationWarning", comments, above, decl)

for rel in files("bindings/jvm/src/main", ".java"):
    scanned["java"] += 1
    text = read(rel)
    if "eprecated" not in text:
        continue
    cls = re.sub(r"^.*?/java/", "", rel)[: -len(".java")].replace("/", ".")
    for line, comments, above, decl in units(text, "java"):
        code = " ".join(above + decl)
        if re.search(r"@Deprecated\b", code) or re.search(r"@deprecated\b", " ".join(comments + decl)):
            name = java_name(" ".join(decl))
            kind = "@Deprecated" if "@Deprecated" in code else "Javadoc @deprecated"
            site("java", rel, line, cls, name, kind, comments, above, decl)

for rel in files("bindings/python/python", (".py", ".pyi")):
    scanned["python"] += 1
    text = read(rel)
    if "eprecat" not in text:
        continue
    module = re.sub(r"^bindings/python/python/", "", rel).rsplit(".", 1)[0].replace("/", ".")
    module = module.removesuffix(".__init__")
    for line, comments, above, decl in units(text, "python"):
        code = " ".join(d.split("#")[0] for d in decl)
        trailing = [d.split("#", 1)[1] for d in decl if "#" in d]
        if re.search(r"\bdeprecated alias\b", " ".join(comments + trailing), re.I):
            site("python", rel, line, module, python_name(decl[0]), "deprecated alias comment", comments, above, decl)
        elif re.search(r"@(?:[\w.]+\.)?deprecated\b", " ".join(above)):
            site("python", rel, line, module, python_name(decl[0]), "@deprecated", comments, above, decl)
        elif re.search(r"\bwarn\(", code) and "DeprecationWarning" in code:
            site("python", rel, line, module, None, "DeprecationWarning", comments, above, decl)

header = "bindings/c/include/tstrans.h"
if not os.path.isfile(os.path.join(root, header)):
    err(f"{header} is missing: the C header was not read")
else:
    for n, line in enumerate(read(header).split("\n"), 1):
        if C_MARKER.search(line):
            err(f"{header}:{n}: a C deprecation marker, and this rail has no rule for the C header: "
                "define the marker and its row in scripts/check/repo/deprecations.sh first")

for lang, where in (("rust", "crates/ and bindings/"), ("java", "bindings/jvm/src/main"),
                    ("python", "bindings/python/python")):
    if scanned[lang] == 0:
        err(f"no {lang} source file was read under {where}: its deprecations were not checked")


def covers(row, s):
    if row["lang"] != s["lang"]:
        return False
    symbol, scope = plain(row["symbol"]), s["scope"]
    if row["lang"] == "rust":
        in_scope = symbol.startswith(scope + "::")
    else:
        in_scope = symbol.startswith(scope + ".") or (row["lang"] == "java" and symbol == scope)
    if not scope or not in_scope:
        return False
    if row["lang"] == "java" and symbol == scope:
        return True
    return s["name"] == row["leaf"] if s["name"] else word(row["leaf"], s["text"])


def at(s):
    what = f"`{s['name']}`" if s["name"] else f"`{s['first']}`"
    return f"{s['file']}:{s['line']}: {s['kind']} on {what}"


# ---- (a), (b), (c): sites and live rows -----------------------------------

covered = since_compared = promises_read = 0
for s in sites:
    by = [r for r in live if covers(r, s)]
    if listing:
        print(f"site  {at(s)}")
        print(f"      covered by: {', '.join(r['symbol'] for r in by) or 'no live row'}")
    covered += bool(by)
    if not by:
        if s["name"]:
            symbol = f"its full name (it starts with `{s['scope']}` and ends with `{s['name']}`)"
        else:
            symbol = (f"its full name (it starts with `{s['scope']}`; the site must name its last component, "
                      "in the message or in a comment directly above)")
        err(f"{at(s)} has no live row in {MANIFEST}: add a [[deprecation]] row with "
            f"lang = \"{s['lang']}\", symbol = {symbol}, since, replacement and earliest_removal")
    for r in by:
        if s["since"] is not None:
            since_compared += 1
            if s["since"] != r["since"]:
                err(f"{at(s)} says since = \"{s['since']}\", the row of {r['symbol']} says "
                    f"\"{r['since']}\": make the attribute and {MANIFEST} agree")
    allowed = {r["earliest_v"] for r in by}
    for v in s["promises"]:
        promises_read += 1
        if listing:
            print(f"      names removal version {show(v)}")
        if v <= workspace:
            err(f"{at(s)} promises removal in {show(v)}, and the workspace is already {show(workspace)}: "
                "remove the name in this release, or name a later release here and as the row's earliest_removal")
        elif by and v not in allowed:
            err(f"{at(s)} promises removal in {show(v)}, the row of {by[0]['symbol']} says earliest_removal = "
                f"\"{by[0]['earliest_removal']}\": the code and {MANIFEST} must name the same release")

with_site = 0
for r in live:
    if any(covers(r, s) for s in sites):
        with_site += 1
    else:
        err(f"{MANIFEST}: live row {r['symbol']} ({r['lang']}) covers no deprecation site in the code: "
            "if the name was removed add removed_in, if it is still deprecated mark it "
            "(see the markers in scripts/check/repo/deprecations.sh)")
    if r["earliest_v"] <= workspace:
        err(f"{MANIFEST}: {r['symbol']} has earliest_removal = \"{r['earliest_removal']}\", and the workspace is "
            f"already {show(workspace)}: remove the name in this release (add removed_in), or move "
            "earliest_removal, the code and the docs to a later release")

# ---- (d): removed rows ----------------------------------------------------

try:
    changelog = read("CHANGELOG.md")
except OSError:
    changelog = ""
    if gone:
        err("CHANGELOG.md is missing")
removed_notes = {}  # release -> [(heading, body)] of its `### Removed` subsections
release = None
for block in re.split(r"^(?=###? )", changelog, flags=re.M):
    heading, _, body = block.partition("\n")
    m = re.match(r"## \[([^\]]+)\]", heading)
    if m:
        release = m.group(1)
    elif heading.startswith("### Removed") and release:
        removed_notes.setdefault(release, []).append((heading, body))
released = set(re.findall(r"^## \[([^\]]+)\]", changelog, flags=re.M))

baselines = {}
if os.path.isdir(os.path.join(root, "crates")):
    for d in sorted(os.listdir(os.path.join(root, "crates"))):
        rel = f"crates/{d}/public-api.txt"
        if os.path.isfile(os.path.join(root, rel)):
            baselines[rel] = read(rel)
rust_absent = java_absent = noted = 0
for r in gone:
    name = f"{MANIFEST}: removed row {r['symbol']}"
    for s in sites:
        if covers(r, s):
            err(f"{at(s)} is still in the code, and its row says removed_in = \"{r['removed_in']}\": "
                "delete the deprecated name, or drop removed_in from the row")
    if r["removed_v"] < r["earliest_v"]:
        err(f"{name}: removed_in {r['removed_in']} is earlier than earliest_removal {r['earliest_removal']}")
    symbol = plain(r["symbol"])
    if r["lang"] == "rust":
        path = re.compile(r"(?<![\w:])" + re.escape(symbol) + r"(?!\w)")
        hit = [f for f, text in baselines.items() if path.search(text)]
        if hit:
            err(f"{name} is still a public path in {hit[0]}: remove the item, or drop removed_in from the row")
        rust_absent += not hit
    if r["lang"] == "java":
        rel = "bindings/jvm/src/main/java/" + symbol.replace(".", "/") + ".java"
        if os.path.isfile(os.path.join(root, rel)):
            err(f"{name}: {rel} still exists: remove the class, or drop removed_in from the row")
        else:
            java_absent += 1
    if r["removed_in"] in released:
        section = r["removed_in"]
    elif r["removed_v"] >= workspace:
        section = "Unreleased"
    else:
        err(f"{name}: CHANGELOG.md has no [{r['removed_in']}] section, and the workspace is past it "
            f"({show(workspace)})")
        continue
    package = PACKAGE.get(r["lang"]) or symbol.split("::")[0].replace("_", "-")
    found = [h for h, b in removed_notes.get(section, []) if word(package, h) and word(r["leaf"], b)]
    if found:
        noted += 1
        if listing:
            print(f"gone  {r['symbol']}: CHANGELOG.md [{section}] {found[0]}")
    else:
        err(f"{name}: CHANGELOG.md [{section}] has no `### Removed` heading naming {package} whose entry "
            f"names `{r['leaf']}`: list the removal there")

# ---- (e): the docs --------------------------------------------------------


def passages(text):
    """(line, text) per prose paragraph and per table row; fenced code is skipped."""
    para, fenced = [], False
    for n, line in enumerate(text.split("\n") + [""], 1):
        s = line.strip()
        if s.startswith("```"):
            fenced, s = not fenced, ""
        if fenced or not s or s.startswith("|"):
            if para:
                yield para[0][0], " ".join(p for _, p in para)
                para = []
            if s.startswith("|") and not fenced:
                yield n, s
        else:
            para.append((n, s))


def tail_of(symbol, span, qualified):
    """Whether a code span is the symbol or a tail of it cut at `::` or `.`."""
    span = plain(span)
    if qualified and not re.search(r"::|\.", span):
        return False
    return symbol == span or symbol.endswith(("::" + span, "." + span))


docs = {"docs/reference/api-stability.md": "rust", "docs/languages/rust.md": "rust",
        "docs/languages/python.md": "python", "docs/languages/jvm.md": "java", "docs/languages/c.md": "c"}
guides = os.path.join(root, "docs/guides")
if os.path.isdir(guides):
    docs.update((f"docs/guides/{n}", None) for n in sorted(os.listdir(guides)) if n.endswith(".md"))
docs_read = docs_compared = 0
for rel, lang in docs.items():
    if not os.path.isfile(os.path.join(root, rel)):
        continue
    docs_read += 1
    for n, text in passages(read(rel)):
        stated = [version(v) for v in PROMISE.findall(text)]
        if not stated:
            continue
        spans = re.findall(r"`([^`]+)`", text)
        for r in rows:
            if lang not in (None, r["lang"]):
                continue
            if not any(tail_of(plain(r["symbol"]), sp, lang is None) for sp in spans):
                continue
            crate = r["symbol"].split("::")[0]
            if r["lang"] == "rust" and not (word(crate, text) or word(crate.replace("_", "-"), text)):
                continue
            key = "earliest_removal" if r["removed_v"] is None else "removed_in"
            docs_compared += 1
            if listing:
                print(f"docs  {rel}:{n}: {r['symbol']} with {', '.join(show(v) for v in stated)} ({key} = {r[key]})")
            if version(r[key]) not in stated:
                err(f"{rel}:{n}: names {r['symbol']} with removal in {', '.join(show(v) for v in stated)}, "
                    f"its row says {key} = \"{r[key]}\": the docs and {MANIFEST} must name the same release")

# ---- summary --------------------------------------------------------------

by_lang = ", ".join(f"{k} {sum(1 for s in sites if s['lang'] == k)}" for k in ("rust", "java", "python"))
print(f"deprecations: workspace {show(workspace)}; read {scanned['rust']} .rs, {scanned['java']} .java, "
      f"{scanned['python']} .py/.pyi files and the C header")
print(f"  {len(sites)} deprecation sites ({by_lang}); {len(live)} live rows, {len(gone)} removed rows")
if not sites and not live and not failures:
    print("  nothing is deprecated: no site in the code and no live row")
print(f"  (a) {covered} of {len(sites)} sites are covered by a live row")
print(f"  (b) {with_site} of {len(live)} live rows cover a site; `since` compared at {since_compared} sites")
print(f"  (c) {len(live)} earliest_removal and {promises_read} removal versions written at sites compared with "
      f"workspace {show(workspace)} and the rows")
print(f"  (d) {len(gone)} removed rows: {rust_absent} Rust paths absent from {len(baselines)} public-api.txt, "
      f"{java_absent} Java rows with no class file, {noted} named under `### Removed` in CHANGELOG.md")
print(f"  (e) {docs_compared} symbol and removal-version passages compared in {docs_read} docs pages")
if failures:
    print(f"deprecations: {len(failures)} failure(s)", file=sys.stderr)
    sys.exit(1)
print("PASS: deprecations")
PY
