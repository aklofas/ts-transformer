#!/usr/bin/env bash
# kind-equivalence (spec §8): C ≡ Python ≡ JVM per BindingErrorKind variant.
#
#   (a) the TSV rows equal the enum's variants IN ORDER (rendered by the
#       print-kinds bin) — the comparison is line by line, not a set diff, so
#       BindingErrorKind::ALL's order (including the 41 frozen C rows'
#       relative order) is pinned by this file;
#   (g) each row's member equals name(), (d) each row's c_code equals the
#       discriminant, (f) each row's c_emit equals c_projection() — all three
#       ride the same four-column comparison;
#   (b) every py Enum.MEMBER resolves in bindings/python/python/tstrans/exceptions.py
#       (imported by file — pure Python, no native module needed);
#   (c) every jvm Enum.MEMBER exists in the Java `enum Kind` of its family;
#   (e) with KIND_EQUIV_STRICT=1 (the assembly gate), no *_parity may be `pending`.
#
# Runs cargo + python; GNU sed/grep — Linux only.

set -euo pipefail

if [[ "$(uname -s)" != "Linux" ]]; then
    echo "kind-equivalence: SKIP (linux-only: needs GNU sed/grep)"
    exit 0
fi

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
cd "$ROOT"

TSV="scripts/ratchets/kind-equivalence.tsv"
EXC="bindings/python/python/tstrans/exceptions.py"
JAVA="bindings/jvm/src/main/java/org/tstrans"
STRICT="${KIND_EQUIV_STRICT:-0}"
fail=0

# Interpreter: this tree's venv, else the main checkout's (linked worktree),
# else python3. exceptions.py imports only `enum` + `typing`, so any of them works.
PY="$ROOT/bindings/python/.venv/bin/python"
if [[ ! -x "$PY" ]]; then
    common="$(git -C "$ROOT" rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)"
    main_root="${common%/.git}"
    if [[ -n "$common" && -x "$main_root/bindings/python/.venv/bin/python" ]]; then
        PY="$main_root/bindings/python/.venv/bin/python"
    else
        PY="$(command -v python3)"
    fi
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Rendered table: variant_name<TAB>name<TAB>code<TAB>c_projection, in ALL order
# — exactly the TSV's first four columns.
SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 \
    cargo run -q -p tst-pipeline --bin print-kinds > "$tmp/enum.tsv"
grep -v '^#' "$TSV" | grep -v '^[[:space:]]*$' > "$tmp/rows.tsv"

# (a)+(g)+(d)+(f): the four columns must match line for line, order included.
cut -f1-4 "$tmp/rows.tsv" > "$tmp/tsv.cols"
if ! diff -u "$tmp/enum.tsv" "$tmp/tsv.cols" > "$tmp/d1"; then
    echo "FAIL (a/g/d/f): TSV rows != BindingErrorKind::ALL in order"
    echo "  (columns compared: rust_name, member, c_code, c_emit;"
    echo "   '-' lines are the enum, '+' lines are $TSV)"
    cat "$tmp/d1"
    fail=1
fi

# (b): every py member resolves.
cut -f5 "$tmp/rows.tsv" | tr ';' '\n' | grep -v '^-$' | sort -u > "$tmp/py.members" || true
if ! "$PY" - "$EXC" "$tmp/py.members" <<'PYEOF'
import importlib.util, sys
spec = importlib.util.spec_from_file_location("tstrans_exceptions", sys.argv[1])
mod = importlib.util.module_from_spec(spec)
spec.loader.exec_module(mod)
bad = []
for line in open(sys.argv[2]):
    dotted = line.strip()
    if not dotted:
        continue
    enum_name, member = dotted.split(".", 1)
    enum = getattr(mod, enum_name, None)
    if enum is None or not hasattr(enum, member):
        bad.append(dotted)
for b in bad:
    print("FAIL (b): python member does not resolve:", b)
sys.exit(1 if bad else 0)
PYEOF
then fail=1; fi

# (c): every jvm member exists in its family's `enum Kind`.
cut -f7 "$tmp/rows.tsv" | tr ';' '\n' | grep -v '^-$' | sort -u > "$tmp/jvm.members" || true
while IFS= read -r dotted; do
    [[ -n "$dotted" ]] || continue
    family="${dotted%%.*}"   # SrtException
    member="${dotted##*.}"   # CLOSED
    jfile="$JAVA/$family.java"
    if [[ ! -f "$jfile" ]]; then
        echo "FAIL (c): missing $jfile for $dotted"
        fail=1
        continue
    fi
    consts=$(sed 's@/\*.*\*/@@g' "$jfile" | sed '\@/\*@,\@\*/@d' | sed 's@//.*@@' \
             | sed -n '/enum Kind/,/}/p' | grep -oE '[A-Z][A-Z0-9_]+' | grep -v '^Kind$' || true)
    if ! printf '%s\n' "$consts" | grep -qx "$member"; then
        echo "FAIL (c): $family.Kind.$member not declared in $jfile"
        fail=1
    fi
done < "$tmp/jvm.members"

# (e): strict parity at assembly.
pending=$(awk -F'\t' '$6 == "pending" || $8 == "pending" { print $1 }' "$tmp/rows.tsv")
npending=$(printf '%s\n' "$pending" | grep -c . || true)
if [[ "$STRICT" = "1" && -n "$pending" ]]; then
    echo "FAIL (e): KIND_EQUIV_STRICT=1 but $npending row(s) are still pending:"
    printf '  %s\n' $pending
    fail=1
fi

if [[ "$fail" -eq 0 ]]; then
    rows=$(wc -l < "$tmp/rows.tsv")
    echo "kind-equivalence: OK ($rows kinds; $npending pending)"
fi
exit "$fail"
