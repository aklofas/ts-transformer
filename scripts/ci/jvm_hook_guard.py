#!/usr/bin/env python3
"""Keep the JVM binding's test-hook natives out of every shipped archive.

The set of hook symbols is read from the binding's sources: every JNI native
(`fn Java_…`) compiled only under `feature = "jni-test-hooks"`, by an
attribute on the native, on an enclosing item, or on the `mod` that holds its
file. Every class under test/java that declares a `native` method is a hook
class: its whole symbol prefix is refused, and a native of it that the Rust
sources compile without the feature fails the derivation itself (a class only
the tests have can have no shipped native). A class (or package) whose Rust
natives are all hook natives contributes its prefix too.

The parser refuses on purpose what it cannot classify: a `#[cfg(...)]` naming
the feature together with `not(...)` or `any(...)` (`not(feature =
"jni-test-hooks")` on a stub included), and a native defined twice (say under
two complementary cfgs). When code needs such a form, extend `hook_cfg` or
`scan_sources` with a fixture in scripts/ci/tests/jvm-hook-guard-selftest.sh,
rather than writing the attribute in a shape this parser happens to accept.

  jvm_hook_guard.py derive [--src DIR]
      print the hook symbols and prefixes
  jvm_hook_guard.py check [--src DIR] [--expect-hooks] FILE...
      FILE is a JAR or zip (its native/** entries are listed and scanned) or
      a native library. Fails on a hook symbol, on any native entry in a
      *-sources.jar, on another archive with no native entry and on a library
      with no JNI symbol (a check that read nothing). With --expect-hooks (the
      positive control, run against a hook build) it fails unless every
      native holds a hook symbol.

Symbols are found as byte strings, so the scan reads ELF, Mach-O and PE
libraries alike without a platform tool.
"""

import os
import re
import sys
import zipfile

HOOK_FEATURE = '"jni-test-hooks"'
NATIVE_RE = re.compile(r"\bfn\s+(Java_[A-Za-z0-9_]+)")
JAVA_PACKAGE_RE = re.compile(r"^package\s+([\w.]+)\s*;", re.M)
# A native method declaration: modifiers, `native`, a return type, the name.
JAVA_NATIVE_RE = re.compile(
    r"^[ \t]*(?:(?:public|protected|private|static|final|synchronized)\s+)*native\s+[\w.<>\[\], ]+?\s+\w+\s*\(",
    re.M)
SYMBOL_RE = re.compile(rb"Java_[A-Za-z0-9_]+")


def fail(msg):
    print(f"FAIL: {msg}")
    sys.exit(1)


def hook_cfg(line):
    """True for an attribute that compiles its item only under the hooks."""
    s = line.strip()
    if not s.startswith("#[cfg(") or HOOK_FEATURE not in s:
        return False
    body = s[len("#[cfg("):]
    if body.startswith("feature") or body.startswith("all("):
        if "not(" in body or "any(" in body:
            fail(f"cannot classify the attribute {s!r}")
        return True
    fail(f"cannot classify the attribute {s!r}")


def block_end(lines, start):
    """Index of the line closing the first brace block from `start`, or of the
    line ending the item when a `;` comes first (no block)."""
    depth = 0
    opened = False
    for i in range(start, len(lines)):
        line = lines[i]
        j = 0
        while j < len(line):
            c = line[j]
            if line.startswith("//", j):
                break
            if c == '"':
                j += 1
                while j < len(line) and line[j] != '"':
                    j += 2 if line[j] == "\\" else 1
            elif c == "'" and line[j + 1:j + 2] == "\\":
                j = line.index("'", j + 3)  # an escaped char literal: '\'' or '\u{7b}'
            elif c == "'" and line[j + 2:j + 3] == "'":
                j += 2  # a char literal such as '{'; a lifetime has no closing quote
            elif c == "{":
                depth += 1
                opened = True
            elif c == "}":
                depth -= 1
                if opened and depth == 0:
                    return i
            elif c == ";" and not opened:
                return i
            j += 1
    fail(f"unbalanced item starting at line {start + 1}")


def module_paths(src, path, name):
    """The file and directory a `mod name;` in `path` names."""
    d = os.path.dirname(path)
    base = os.path.basename(path)
    if base not in ("lib.rs", "mod.rs", "main.rs"):
        d = os.path.join(d, base[:-3])
    return [os.path.join(d, name + ".rs"), os.path.join(d, name)]


def scan_sources(src):
    files = []
    for root, _dirs, names in os.walk(src):
        files += [os.path.join(root, n) for n in names if n.endswith(".rs")]
    gated_paths = []  # files and directories compiled only under the hooks
    natives = {}  # symbol -> gated?
    per_file = {}
    for path in sorted(files):
        with open(path, encoding="utf-8") as f:
            lines = f.read().split("\n")
        gated_lines = set()
        for i, line in enumerate(lines):
            if not hook_cfg(line):
                continue
            j = i + 1
            while j < len(lines):
                s = lines[j].strip()
                if s == "" or s.startswith("#[") or s.startswith("//"):
                    j += 1
                    continue
                break
            if j == len(lines):
                break
            item = lines[j].strip()
            m = re.match(r"(pub(\([a-z]+\))?\s+)?mod\s+([a-z_0-9]+)\s*;", item)
            if m:
                gated_paths += module_paths(src, path, m.group(3))
                continue
            gated_lines.update(range(j, block_end(lines, j) + 1))
        per_file[path] = (lines, gated_lines)
    for path, (lines, gated_lines) in per_file.items():
        whole = any(path == g or path.startswith(g + os.sep) for g in gated_paths)
        for i, line in enumerate(lines):
            if line.lstrip().startswith("//"):
                continue  # a doc example is not a native
            for m in NATIVE_RE.finditer(line):
                sym = m.group(1)
                if sym in natives:
                    fail(f"{sym} is defined twice")
                natives[sym] = whole or i in gated_lines
    return natives


def jni_mangle(name):
    return name.replace("_", "_1").replace(".", "_")


def java_test_classes(src):
    """`Java_<package>_<Class>_` of each test/java file that declares a native
    method (a nested class's natives share the prefix)."""
    root = os.path.join(src, "test", "java")
    prefixes = set()
    for d, _dirs, names in os.walk(root):
        for n in names:
            if not n.endswith(".java"):
                continue
            with open(os.path.join(d, n), encoding="utf-8") as f:
                text = f.read()
            if not JAVA_NATIVE_RE.search(text):
                continue
            m = JAVA_PACKAGE_RE.search(text)
            pkg = m.group(1) + "." if m else ""
            prefixes.add("Java_" + jni_mangle(pkg + n[:-len(".java")]) + "_")
    return prefixes


def owner(sym):
    """`Java_pkg_Class_` (and `Java_pkg_`) of a mangled native name; `_1`
    escapes an underscore inside a name, so only a `_` before a letter splits."""
    parts = re.split(r"_(?=[A-Za-z])", sym)
    return "_".join(parts[:-1]) + "_", "_".join(parts[:-2]) + "_"


def derive(src):
    natives = scan_sources(src)
    hooks = sorted(s for s, g in natives.items() if g)
    if not hooks:
        fail(f"no jni-test-hooks native found under {src}: the parser no longer matches the code")
    test_classes = java_test_classes(src)
    if not test_classes:
        fail(f"no native method declared under {os.path.join(src, 'test', 'java')}: the parser no longer matches the code")
    shipped = [s for s, g in natives.items() if not g and any(s.startswith(p) for p in test_classes)]
    if shipped:
        fail("native(s) of a test-only class compiled without the jni-test-hooks feature: " + ", ".join(sorted(shipped)))
    plain_classes = {owner(s)[0] for s, g in natives.items() if not g}
    plain_packages = {owner(s)[1] for s, g in natives.items() if not g}
    prefixes = set(test_classes)
    for s in hooks:
        cls, pkg = owner(s)
        if cls not in plain_classes:
            prefixes.add(cls)
        if pkg not in plain_packages:
            prefixes.add(pkg)
    return hooks, sorted(prefixes)


def is_hook(sym, hooks, prefixes):
    return sym in hooks or any(sym.startswith(p) for p in prefixes)


def natives_of(path):
    """(name, bytes) of each native library in `path`."""
    if zipfile.is_zipfile(path):
        with zipfile.ZipFile(path) as z:
            entries = [n for n in z.namelist() if n.startswith("native/") and not n.endswith("/")]
            print(f"{path}: {len(entries)} native entr{'y' if len(entries) == 1 else 'ies'}")
            for n in entries:
                print(f"  {n}")
            return [(f"{path}!{n}", z.read(n)) for n in entries]
    with open(path, "rb") as f:
        return [(path, f.read())]


def check(src, files, expect_hooks):
    hooks, prefixes = derive(src)
    hookset = set(hooks)
    bad = []
    for path in files:
        if not os.path.isfile(path):
            fail(f"no such file: {path}")
        libs = natives_of(path)
        if path.endswith("-sources.jar") and libs:
            bad.append(f"{path}: a sources archive carries {len(libs)} native librar{'y' if len(libs) == 1 else 'ies'}")
        if not libs and (expect_hooks or not path.endswith("-sources.jar")):
            bad.append(f"{path}: no native library to check")
        for name, data in libs:
            found = sorted({m.decode() for m in SYMBOL_RE.findall(data)})
            hits = [s for s in found if is_hook(s, hookset, prefixes)]
            if not found:
                bad.append(f"{name}: no JNI symbol (not a binding library?)")
            elif expect_hooks:
                if hits:
                    print(f"OK (expected): {name} holds {len(hits)} hook symbol(s), e.g. {hits[0]}")
                else:
                    bad.append(f"{name}: no hook symbol in a library built with the hooks")
            elif hits:
                bad.append(f"{name}: test-hook symbol(s) {', '.join(hits[:5])}")
            else:
                print(f"OK: {name}: {len(found)} JNI symbol(s), none a test hook")
    if bad:
        for b in bad:
            print(f"FAIL: {b}")
        sys.exit(1)


def main(argv):
    here = os.path.dirname(os.path.abspath(__file__))
    src = os.path.join(here, "..", "..", "bindings", "jvm", "src")
    args = argv[1:]
    if not args or args[0] not in ("derive", "check"):
        print(__doc__)
        sys.exit(2)
    cmd, rest = args[0], args[1:]
    expect_hooks = False
    files = []
    while rest:
        a = rest.pop(0)
        if a == "--src":
            src = rest.pop(0)
        elif a == "--expect-hooks" and cmd == "check":
            expect_hooks = True
        else:
            files.append(a)
    if cmd == "derive":
        if files:
            fail(f"derive takes no files: {files}")
        hooks, prefixes = derive(src)
        for p in prefixes:
            print(f"prefix {p}")
        for h in hooks:
            print(f"native {h}")
    else:
        if not files:
            fail("check needs at least one file")
        check(src, files, expect_hooks)


if __name__ == "__main__":
    main(sys.argv)
