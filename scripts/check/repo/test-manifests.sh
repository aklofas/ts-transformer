#!/usr/bin/env bash
# Existence/format-only checker for the advisory coverage manifests under
# tests/coverage/. Enforces that the manifests are well-formed and reference
# real, path-safe, in-tree files. Deliberately NOT a coverage gate — it never
# asserts coverage is "enough" (that is deferred).
#
# Each manifest is parsed as TOML (python3 >= 3.11, tomllib) and checked row by
# row:
#   fixture-manifest.toml — each [[group]] has a `root` that exists, is
#       repo-relative (no leading /, no ..) and lives under crates/ or tests/,
#       an `origin` in synthetic|public|derived, and no_private_corpus = true.
#   skip-ledger.toml      — each entry's `class` is recognized; a blocked_bug
#       or placeholder entry carries `expires_after` or `resolved` itself.
#   stream-matrix.toml    — every status value outside [gaps] is one of
#       covered|partial|gap.
# A file that does not parse fails as `FAIL: <file>: <tomllib message>`.
#
# TEST_MANIFESTS_DIR overrides tests/coverage (the self-test points it at a
# planted copy); fixture roots still resolve against the repository root.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"
COV="${TEST_MANIFESTS_DIR:-tests/coverage}"
fail=0
err() { echo "FAIL: $*" >&2; fail=1; }

for f in fixture-manifest.toml skip-ledger.toml stream-matrix.toml README.md; do
    [[ -f "$COV/$f" ]] || err "$COV/$f missing"
done

python3 - "$COV" <<'PY' || fail=1
import sys

try:
    import tomllib
except ImportError:
    print("FAIL: test-manifests needs python3 >= 3.11 (tomllib)", file=sys.stderr)
    sys.exit(1)
import os

cov = sys.argv[1]
failed = False


def err(msg):
    global failed
    failed = True
    print(f"FAIL: {msg}", file=sys.stderr)


def load(name):
    path = os.path.join(cov, name)
    if not os.path.isfile(path):
        return None
    try:
        with open(path, "rb") as f:
            return tomllib.load(f)
    except tomllib.TOMLDecodeError as e:
        err(f"{path}: {e}")
        return None


fm = load("fixture-manifest.toml")
if fm is not None:
    for i, g in enumerate(fm.get("group", []), 1):
        where = f"fixture-manifest group {i}"
        root = g.get("root")
        if not isinstance(root, str):
            err(f"{where}: no root")
        elif root.startswith("/"):
            err(f"{where}: fixture root is absolute: {root}")
        elif ".." in root:
            err(f"{where}: fixture root contains '..': {root}")
        elif not (root.startswith("crates/") or root.startswith("tests/")):
            err(f"{where}: fixture root outside crates/ or tests/: {root}")
        elif not os.path.exists(root):
            err(f"{where}: fixture root does not exist: {root}")
        if g.get("origin") not in ("synthetic", "public", "derived"):
            err(f"{where} ({root}): unrecognized fixture origin: {g.get('origin')!r}")
        if g.get("no_private_corpus") is not True:
            err(f"{where} ({root}): no_private_corpus = true is missing")

classes = {"placeholder", "external_tool", "environmental", "slow", "diagnostic", "blocked_bug"}
sl = load("skip-ledger.toml")
if sl is not None:
    for i, e in enumerate(sl.get("skip", []), 1):
        where = f"skip-ledger entry {i} ({e.get('test')})"
        cls = e.get("class")
        if cls not in classes:
            err(f"{where}: unrecognized skip class: {cls!r}")
        if cls in ("blocked_bug", "placeholder") and not ("expires_after" in e or "resolved" in e):
            err(f"{where}: a {cls} entry carries neither expires_after nor resolved")

sm = load("stream-matrix.toml")
if sm is not None:
    def walk(prefix, value):
        if isinstance(value, dict):
            for k, v in value.items():
                walk(f"{prefix}.{k}" if prefix else k, v)
        elif value not in ("covered", "partial", "gap"):
            err(f"stream-matrix {prefix}: unrecognized status: {value!r}")

    for table, value in sm.items():
        if table != "gaps":
            walk(table, value)

sys.exit(1 if failed else 0)
PY

if [[ "$fail" -eq 0 ]]; then
    echo "PASS: test-manifests (existence + format)"
fi
exit "$fail"
