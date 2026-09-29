#!/usr/bin/env bash
# Arc 2 spec §3.2 / §8: nothing a caller can ask from a second thread may
# wait behind the call the first thread is parked in. A blocked recv /
# accept / send holds the shell slot for as long as it is parked, so a
# reader that takes the slot waits with it — the hang PR #234 fixed in the
# Python binding (five getters took the slot a parked accept/recv owned).
#
# Two checks, both fail-closed.
#
# 1. Every binding (C, Python, JVM): FAIL on any
#    `local_addr( | local_port( | repr(` call inside a `with_mut(|…|` /
#    `with_ref(|…|` closure anywhere under bindings/. The C spelling is
#    `with_inner_mut` / `with_inner_ref`, Python and the JVM use the bare
#    `Owned` names — the pattern covers both.
#
# 2. Python and JVM: FAIL on any function that calls the BLOCKING
#    `with_ref(` and is not listed in
#    scripts/ratchets/blocking-slot-readers.tsv. The rule for a reader is:
#      - a value that is constant after construction is captured at
#        construction and read from `Owned::snapshot()`, which takes no lock;
#      - a dynamic observation that has an answer for "the slot is held"
#        (liveness: a parked call means open) uses `try_with_ref`;
#      - only a reader with no answer without the slot, or a function that
#        is itself a blocking operation, may call `with_ref` — and it says
#        so, with its reason, in the allowlist.
#    The allowlist is exact in both directions: an entry whose function no
#    longer calls `with_ref` fails too, so the list cannot rot into a
#    blanket pass. The check also fails if it finds no `with_ref` caller at
#    all (the allowlisted readers exist, so zero means the scan drifted).
#
# Portable: bash 3.2, POSIX awk, no GNU-only flags (the macOS legs run it).
#
# Test hooks (scripts/ratchets/tests/self_test.sh): SG_ROOTS overrides the
# directories check 2 scans, SG_ALLOWLIST its allowlist, and SG_ONLY_READERS=1
# skips check 1.
set -euo pipefail
cd "$(dirname "$0")/../../.."

if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL: ripgrep (rg) is required by $(basename "$0") but is not on PATH." >&2
    exit 1
fi

status=0

# ---- check 1: constant getters inside a slot-taking closure ---------------
if [ "${SG_ONLY_READERS:-}" != "1" ]; then
    # Multiline, non-greedy: from the closure's `|…|` up to the first `;`,
    # flagging a getter call in between.
    PATTERN='with_(inner_)?(mut|ref)\s*\(\s*\|[^|]*\|[^;]*?\b(local_addr|local_port|repr)\s*\('

    set +e
    matches=$(rg -nU --multiline-dotall -t rust "$PATTERN" bindings/)
    rc=$?
    set -e

    case $rc in
        0)
            echo "FAIL: construction-constant getter read inside a slot-taking closure (read Owned::snapshot() instead):"
            echo "$matches"
            status=1
            ;;
        1)
            echo "OK: no slot-taking local_addr/local_port/repr reads under bindings/"
            ;;
        *)
            echo "FAIL: rg exited $rc" >&2
            exit 1
            ;;
    esac
fi

# ---- check 2: blocking slot readers must be allowlisted --------------------
ROOTS="${SG_ROOTS:-bindings/python/src bindings/jvm/src}"
ALLOWLIST="${SG_ALLOWLIST:-scripts/ratchets/blocking-slot-readers.tsv}"

if [ ! -f "$ALLOWLIST" ]; then
    echo "FAIL: allowlist $ALLOWLIST not found" >&2
    exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# One line per (file, function) that calls the blocking `with_ref(`.
# `try_with_ref(` does not match (the character before `with_ref` is `_`),
# nor does the definition `fn with_ref<R>(` (a `<` follows the name). Comment
# lines and the items guarded by a column-0 `#[cfg(test)]` are skipped.
# shellcheck disable=SC2086  # ROOTS is a deliberate word list
find $ROOTS -type f -name '*.rs' | LC_ALL=C sort | while IFS= read -r f; do
    awk -v file="$f" '
        # Skip the one item a column-0 `#[cfg(test)]` guards, not the rest
        # of the file: a one-line item ends at its `;`, a block at the
        # closing brace rustfmt puts in column 0.
        /^#\[cfg\(test\)\]/ { guarded = 1; next }
        guarded == 1 {
            if ($0 ~ /^#\[/) next
            if ($0 ~ /;[ \t]*$/ && $0 !~ /\{/) { guarded = 0; next }
            guarded = 2
            next
        }
        guarded == 2 {
            if ($0 ~ /^\}/) guarded = 0
            next
        }
        {
            line = $0
            sub(/^[ \t]+/, "", line)
            if (line ~ /^\/\//) next
            if (match(line, /fn [A-Za-z_][A-Za-z0-9_]*/)) {
                fn = substr(line, RSTART + 3, RLENGTH - 3)
            }
            if (line ~ /(^|[^A-Za-z0-9_])with_ref[ \t]*\(/ && fn != "") {
                print file "\t" fn
            }
        }
    ' "$f"
done | LC_ALL=C sort -u > "$tmp/found"

# Allowlist: `path<TAB>function<TAB>class<TAB>reason`; `#` lines are comments.
# A row without a class and a reason is rejected.
if ! awk -F '\t' '
        /^#/ || /^[ \t]*$/ { next }
        NF < 4 || $3 == "" || $4 == "" {
            printf "FAIL: %s:%d: expected path<TAB>function<TAB>class<TAB>reason\n", FILENAME, NR
            bad = 1
            next
        }
        $3 != "blocking-op" && $3 != "needs-slot" && $3 != "primitive" {
            printf "FAIL: %s:%d: unknown class \"%s\" (blocking-op | needs-slot | primitive)\n", FILENAME, NR, $3
            bad = 1
        }
        END { exit bad }
    ' "$ALLOWLIST"; then
    status=1
fi
awk -F '\t' '!/^#/ && NF >= 2 { print $1 "\t" $2 }' "$ALLOWLIST" | LC_ALL=C sort -u > "$tmp/allowed"

if [ ! -s "$tmp/found" ]; then
    echo "FAIL: no blocking with_ref( caller found under: $ROOTS"
    echo "      the allowlisted readers exist, so the scan matched nothing — the pattern drifted."
    exit 1
fi

LC_ALL=C comm -23 "$tmp/found" "$tmp/allowed" > "$tmp/unlisted"
LC_ALL=C comm -13 "$tmp/found" "$tmp/allowed" > "$tmp/stale"

if [ -s "$tmp/unlisted" ]; then
    echo "FAIL: these functions take the slot with the blocking with_ref() and wait behind a parked call:"
    sed 's/^/    /' "$tmp/unlisted"
    echo "      constant after construction -> capture it at construction, read Owned::snapshot();"
    echo "      dynamic with an answer for a held slot -> try_with_ref();"
    echo "      must have the slot -> add a row, with the reason, to $ALLOWLIST."
    status=1
fi
if [ -s "$tmp/stale" ]; then
    echo "FAIL: $ALLOWLIST lists functions that no longer call the blocking with_ref():"
    sed 's/^/    /' "$tmp/stale"
    echo "      remove the rows."
    status=1
fi

if [ "$status" -eq 0 ]; then
    n=$(wc -l < "$tmp/found" | tr -d ' ')
    echo "OK: all $n blocking slot readers under ($ROOTS) are allowlisted with a reason"
fi
exit "$status"
