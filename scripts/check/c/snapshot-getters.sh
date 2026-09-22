#!/usr/bin/env bash
# Arc 2 spec §3.2 / §8: a construction-constant getter (local address /
# port / repr) must read the handle's SNAPSHOT, never the shell slot — a
# slot read parks behind a blocked recv/accept that holds the data-path
# lock, which is exactly the hang PR #234 fixed in the Python binding
# (five getters took the slot a parked accept/recv owned).
#
# FAIL on any `local_addr( | local_port( | repr(` call inside a
# `with_mut(|…|` / `with_ref(|…|` closure anywhere under bindings/. The C
# spelling is `with_inner_mut` / `with_inner_ref`, Python and the JVM use
# the bare `Owned` names after WP-B2/B3 — the pattern covers both.
#
# The fix is always the same: capture the value at construction and read it
# from `Owned::snapshot()` (C: `handle.inner.snapshot()`), which takes no
# lock at all.
set -euo pipefail
cd "$(dirname "$0")/../../.."

if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL: ripgrep (rg) is required by $(basename "$0") but is not on PATH." >&2
    exit 1
fi

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
        exit 1
        ;;
    1)
        echo "OK: no slot-taking local_addr/local_port/repr reads under bindings/"
        exit 0
        ;;
    *)
        echo "FAIL: rg exited $rc" >&2
        exit 1
        ;;
esac
