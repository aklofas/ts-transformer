#!/usr/bin/env bash
# Every name the `py-test-hooks` cargo feature adds to `tstrans._native`
# ends in one of the suffixes of bindings/python/tests/_builders/hook_names.py
# (`SUFFIXES`), and the set of those names equals that file's `EXPECTED`.
# The wheel jobs prove a wheel has no test hook by walking for the suffixes
# and by scanning the binary for the EXPECTED names: a hook named without a
# suffix and missing from EXPECTED would ship and be seen by neither.
#
# What counts as registered, inside an item or statement under
# `#[cfg(feature = "py-test-hooks")]` (or `all(` with it at the top level)
# anywhere under bindings/python/src:
#   - a `#[pyfunction]` (its `#[pyo3(name = "…")]` / `#[pyfunction(name =
#     "…")]` name, else the Rust name);
#   - the function a `wrap_pyfunction!(…)` names (resolved to its
#     `#[pyfunction]`; a gated wrap of a function the rail cannot find fails);
#   - the class an `add_class::<…>()` names;
#   - a method of a `#[pymethods]` impl;
#   - a value set by `.add("<name>", …)` or `.setattr("<name>", …)`.
# Not seen: such a call whose name is not a string literal, `add_wrapped`,
# a closure (`PyCFunction::new_closure`), and a gated `add_submodule` of a
# module whose functions are registered without the gate. A hook registered
# that way must be given a suffix and listed in EXPECTED by hand.
# A gated `#[pyclass]` that is never added to a module is a hook's return
# type, not a module attribute: it needs a suffix but is not in EXPECTED.
# A cfg or cfg_attr naming the feature in any other shape (`not(`, `any(`)
# fails: the rail cannot tell what it gates.
#
# EXPECTED is compared by its last dotted component (the registered name).
# The check fails closed when it finds no hook at all.
#
# Test hooks (scripts/ratchets/tests/self_test.sh): HN_ROOT overrides the
# directory scanned, HN_EXPECTED the file read for SUFFIXES and EXPECTED.
set -euo pipefail
cd "$(dirname "$0")/../../.."

ROOT="${HN_ROOT:-bindings/python/src}"
EXPECTED_FILE="${HN_EXPECTED:-bindings/python/tests/_builders/hook_names.py}"

python3 - "$ROOT" "$EXPECTED_FILE" <<'PY'
import ast
import os
import re
import sys

root, expected_file = sys.argv[1], sys.argv[2]
FEATURE = "py-test-hooks"
failures = []

if not os.path.isdir(root):
    print(f"FAIL: {root} is not a directory")
    sys.exit(1)

consts = {}
for node in ast.parse(open(expected_file).read()).body:
    if isinstance(node, ast.Assign) and len(node.targets) == 1:
        target = node.targets[0]
        if isinstance(target, ast.Name) and target.id in ("SUFFIXES", "EXPECTED"):
            consts[target.id] = ast.literal_eval(node.value)
if set(consts) != {"SUFFIXES", "EXPECTED"}:
    print(f"FAIL: {expected_file} has no SUFFIXES or no EXPECTED tuple")
    sys.exit(1)
SUFFIXES = tuple(consts["SUFFIXES"])
EXPECTED = {e.rsplit(".", 1)[-1]: e for e in consts["EXPECTED"]}


STRING = re.compile(r'(b?r(#*)|b?)"')


def lex(src):
    """Rust tokens as (kind, text, line): ident, str, lifetime, punct.
    Comments are dropped; a string's text is its raw content."""
    toks, i, line, n = [], 0, 1, len(src)
    while i < n:
        c = src[i]
        if c == "\n":
            line += 1
            i += 1
        elif c.isspace():
            i += 1
        elif src.startswith("//", i):
            while i < n and src[i] != "\n":
                i += 1
        elif src.startswith("/*", i):
            depth, i = 1, i + 2
            while i < n and depth:
                if src.startswith("/*", i):
                    depth, i = depth + 1, i + 2
                elif src.startswith("*/", i):
                    depth, i = depth - 1, i + 2
                else:
                    line += src[i] == "\n"
                    i += 1
        elif STRING.match(src, i):
            m = STRING.match(src, i)
            raw, close = "r" in m.group(1), '"' + (m.group(2) or "")
            j, start_line = m.end(), line
            while not src.startswith(close, j):
                if src[j] == "\\" and not raw:
                    j += 1
                line += src[j] == "\n"
                j += 1
            toks.append(("str", src[m.end():j], start_line))
            i = j + len(close)
        elif c == "'":
            if src[i + 1] == "\\":
                i = src.index("'", i + 3) + 1
            elif i + 2 < n and src[i + 2] == "'":
                i += 3
            else:
                k = i + 1
                while k < n and (src[k].isalnum() or src[k] == "_"):
                    k += 1
                toks.append(("lifetime", src[i:k], line))
                i = k
        elif c.isalpha() or c == "_":
            k = i
            while k < n and (src[k].isalnum() or src[k] == "_"):
                k += 1
            toks.append(("ident", src[i:k], line))
            i = k
        elif c.isdigit():
            k = i
            while k < n and (src[k].isalnum() or src[k] in "_."):
                k += 1
            i = k
        elif src.startswith("::", i):
            toks.append(("punct", "::", line))
            i += 2
        else:
            toks.append(("punct", c, line))
            i += 1
    return toks


def group_attrs(toks):
    """Fold `#[…]` / `#![…]` into ("attr", inner tokens, line, is_inner)."""
    out, i = [], 0
    while i < len(toks):
        t = toks[i]
        if t[1] == "#" and i + 1 < len(toks):
            j = i + 1
            inner = toks[j][1] == "!"
            j += inner
            if j < len(toks) and toks[j][1] == "[":
                depth, k = 0, j
                while True:
                    if toks[k][1] == "[":
                        depth += 1
                    elif toks[k][1] == "]":
                        depth -= 1
                        if depth == 0:
                            break
                    k += 1
                out.append(("attr", toks[j + 1:k], t[2], inner))
                i = k + 1
                continue
        out.append(t)
        i += 1
    return out


def parse_pred(ts, i):
    """A cfg predicate starting at ts[i] -> (tree, next index)."""
    name = ts[i][1]
    i += 1
    if i < len(ts) and ts[i][1] == "=":
        return (name, ts[i + 1][1]), i + 2
    if i < len(ts) and ts[i][1] == "(":
        kids, i = [], i + 1
        while ts[i][1] != ")":
            kid, i = parse_pred(ts, i)
            kids.append(kid)
            if ts[i][1] == ",":
                i += 1
        return (name, kids), i + 1
    return (name, None), i


def is_hooks(p):
    return p == ("feature", FEATURE)


def mentions(p):
    return is_hooks(p) or (isinstance(p[1], list) and any(mentions(k) for k in p[1]))


def cfg_gate(attr, where):
    """True when the attribute gates its item on the feature, False when it
    does not name it; a shape the rail cannot judge is a failure."""
    body = attr[1]
    if not body or body[0][1] not in ("cfg", "cfg_attr"):
        return False
    if not any(t[0] == "str" and t[1] == FEATURE for t in body):
        return False
    if body[0][1] == "cfg" and len(body) > 2 and body[1][1] == "(":
        pred, _ = parse_pred(body, 2)
        if is_hooks(pred) or (pred[0] == "all" and any(is_hooks(k) for k in pred[1])):
            if not (pred[0] == "all" and any(mentions(k) and not is_hooks(k) for k in pred[1])):
                return True
    failures.append(f"{where}:{attr[2]}: cannot classify this {body[0][1]} on {FEATURE}")
    return False


def attr_name(attr):
    return attr[1][0][1] if attr[1] else ""


def name_arg(attr):
    """The string after `name =` in an attribute, if any."""
    body = attr[1]
    for k in range(len(body) - 2):
        if body[k][1] == "name" and body[k + 1][1] == "=" and body[k + 2][0] == "str":
            return body[k + 2][1]
    return None


QUALIFIERS = {"pub", "crate", "super", "self", "in", "(", ")", "const", "async", "unsafe", "extern"}
BLOCK_ITEMS = {"fn", "mod", "impl", "struct", "enum", "trait", "union", "{", "macro_rules"}

pyfunctions = []  # (file, line, ident, name, gated)
pyclasses = []    # (file, line, ident, name, gated)
methods = []      # (file, line, name)
wraps = []        # (file, line, path)
classes_added = []  # (file, line, path)
values = []       # (file, line, name)

files = []
for d, _, names in os.walk(root):
    files += [os.path.join(d, f) for f in names if f.endswith(".rs")]

for path in sorted(files):
    rel = os.path.relpath(path, root)
    toks = group_attrs(lex(open(path, encoding="utf-8").read()))
    depth = 0       # braces
    pdepth = 0      # parens and brackets
    regions = []    # open gated regions: [depth, pdepth, block]
    pymethods = []  # brace depth of each open #[pymethods] body
    group = []      # attributes of the item about to start
    item_attrs = []  # attributes of the item being read
    item_start = None
    pending_pymethods = False
    i = 0
    while i < len(toks):
        t = toks[i]
        if t[0] == "attr":
            if not t[3]:
                group.append(t)
            i += 1
            continue
        if group:
            if any([cfg_gate(a, rel) for a in group]):
                k = i
                while k < len(toks) and (toks[k][1] in QUALIFIERS or toks[k][0] == "str"):
                    k += 1
                block = k < len(toks) and toks[k][1] in BLOCK_ITEMS
                regions.append([depth, pdepth, block])
            if any(attr_name(a) == "pymethods" for a in group):
                pending_pymethods = True
            item_attrs, item_start, group = group, i, []
        is_gated = bool(regions)
        text = t[1]
        if t[0] == "ident" and text in ("fn", "struct", "enum") and i + 1 < len(toks) \
                and toks[i + 1][0] == "ident":
            ident = toks[i + 1][1]
            own = item_start is not None and all(
                toks[k][1] in QUALIFIERS or toks[k][0] == "str" for k in range(item_start, i))
            names = {attr_name(a): a for a in item_attrs} if own else {}
            given = name_arg(names["pyo3"]) if "pyo3" in names else None
            if text == "fn" and "pyfunction" in names:
                given = name_arg(names["pyfunction"]) or given
                pyfunctions.append((rel, t[2], ident, given or ident, is_gated))
            elif text != "fn" and "pyclass" in names:
                pyclasses.append((rel, t[2], ident, name_arg(names["pyclass"]) or ident, is_gated))
            elif text == "fn" and is_gated and pymethods and depth == pymethods[-1]:
                methods.append((rel, t[2], given or ident))
            item_start = None
        elif is_gated and text == "wrap_pyfunction" and toks[i + 1][1] == "!":
            k, seg = i + 3, []
            while toks[k][1] not in (",", ")"):
                seg.append(toks[k][1])
                k += 1
            wraps.append((rel, t[2], "".join(seg)))
        elif is_gated and text in ("add", "setattr") and i > 0 and toks[i - 1][1] == "." \
                and toks[i + 1][1] == "(" and toks[i + 2][0] == "str":
            values.append((rel, t[2], toks[i + 2][1]))
        elif is_gated and text == "add_class" and toks[i + 1][1] == "::" and toks[i + 2][1] == "<":
            k, seg = i + 3, []
            while toks[k][1] != ">":
                seg.append(toks[k][1])
                k += 1
            classes_added.append((rel, t[2], "".join(seg)))
        if text == "{":
            depth += 1
            if pending_pymethods:
                pymethods.append(depth)
                pending_pymethods = False
        elif text == "}":
            if pymethods and depth == pymethods[-1]:
                pymethods.pop()
            depth -= 1
            while regions and (depth < regions[-1][0] or (regions[-1][2] and depth == regions[-1][0])):
                regions.pop()
        elif text in ("(", "["):
            pdepth += 1
        elif text in (")", "]"):
            pdepth -= 1
        elif text == ";":
            while regions and depth == regions[-1][0] and pdepth == regions[-1][1]:
                regions.pop()
            item_start = None
        i += 1


def resolve(defs, file, path, kind):
    ident = path.rsplit("::", 1)[-1]
    found = [d for d in defs if d[2] == ident]
    if len(found) > 1:
        same = [d for d in found if d[0] == file]
        mods = path.split("::")[:-1]
        by_mod = [d for d in found if mods and mods[-1] in d[0].replace(os.sep, "/").replace(".rs", "").split("/")]
        found = same or by_mod or found
    if len(found) != 1:
        what = "cannot resolve" if not found else "more than one candidate for"
        failures.append(f"{file}: {what} {kind}({path})")
        return None
    return found[0]


registered = {}  # name -> where
for f, line, ident, name, gated in pyfunctions:
    if gated:
        registered[name] = f"{f}:{line}"
for f, line, p in wraps:
    d = resolve(pyfunctions, f, p, "wrap_pyfunction!")
    if d:
        registered.setdefault(d[3], f"{d[0]}:{d[1]}")
for f, line, p in classes_added:
    d = resolve(pyclasses, f, p, "add_class::<>")
    if d:
        registered.setdefault(d[3], f"{d[0]}:{d[1]}")
for f, line, name in methods + values:
    registered[name] = f"{f}:{line}"

suffixed = dict(registered)
for f, line, ident, name, gated in pyclasses:
    if gated:
        suffixed.setdefault(name, f"{f}:{line}")

if not registered:
    print(f"FAIL: no name registered under {FEATURE} found under {root}: the pattern drifted")
    sys.exit(1)

bare = sorted(f"{w}: {n}" for n, w in suffixed.items() if not n.endswith(SUFFIXES))
unlisted = sorted(f"{w}: {n}" for n, w in registered.items() if n not in EXPECTED)
missing = sorted(full for name, full in EXPECTED.items() if name not in registered)
for label, rows in (
    (f"names registered under {FEATURE} without a suffix of {SUFFIXES}", bare),
    (f"names registered under {FEATURE} missing from {expected_file} EXPECTED", unlisted),
    (f"EXPECTED names in {expected_file} that nothing registers", missing),
):
    if rows:
        failures.append(label + ":\n" + "\n".join("    " + r for r in rows))

if failures:
    for f in failures:
        print("FAIL: " + f)
    sys.exit(1)
print(f"OK: all {len(registered)} test-only names registered under {FEATURE} end in a suffix and equal EXPECTED")
PY
