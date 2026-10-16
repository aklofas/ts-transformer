#!/usr/bin/env bash
# jni-catch-coverage: every JNI entry point is declared through `jni_native!`
# (bindings/jvm/src/panic.rs), which wraps its body in jni_catch so a Rust
# panic becomes a Java RuntimeException instead of unwinding into the JVM.
# A bare `extern "system" fn Java_…` (with or without `pub`), any
# `export_name` attribute, or a `no_mangle` anywhere but panic.rs (the macro)
# and the line above `fn JNI_OnLoad(` in lib.rs, is a native that can let a
# panic cross the boundary. Comment lines are not code and are skipped.
#
# Bash 3.2 / BSD grep portable: POSIX classes only, no mapfile, no grep -P.

set -euo pipefail
cd "$(dirname "$0")/../../.."

SRC_DIR="bindings/jvm/src"
fail=0

# grep -rn prints `file:line:text`; this drops the lines whose text is a comment.
not_comment() { grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' || true; }

bare=$(grep -rnE 'extern "system"[[:space:]]+fn[[:space:]]+Java_' "$SRC_DIR" --include='*.rs' | not_comment || true)
if [ -n "$bare" ]; then
    echo "FAIL: bare JNI entry points (declare them with jni_native!):"
    echo "$bare"
    fail=1
fi

exported=$(grep -rn 'export_name' "$SRC_DIR" --include='*.rs' | not_comment || true)
if [ -n "$exported" ]; then
    echo "FAIL: export_name exports a symbol outside the jni_native! macro:"
    echo "$exported"
    fail=1
fi

# A no_mangle line is allowed in panic.rs, and in lib.rs only when the next
# line declares JNI_OnLoad.
stray=$(find "$SRC_DIR" -name '*.rs' -exec awk '
    FNR == 1 { if (pend) print pend; pend = "" }
    pend != "" {
        if (FILENAME ~ /\/lib\.rs$/ && $0 ~ /fn JNI_OnLoad\(/) { pend = "" } else { print pend; pend = "" }
    }
    /no_mangle/ && $0 !~ /^[[:space:]]*\/\// && FILENAME !~ /\/panic\.rs$/ { pend = FILENAME ":" FNR ":" $0 }
    END { if (pend) print pend }
' {} + )
if [ -n "$stray" ]; then
    echo "FAIL: no_mangle outside the jni_native! macro and JNI_OnLoad:"
    echo "$stray"
    fail=1
fi

n=$(grep -rcE '^[[:space:]]*fn Java_org_tstrans_' "$SRC_DIR" --include='*.rs' | awk -F: '{s+=$2} END {print s+0}')
if [ "$n" -lt 300 ]; then
    echo "FAIL: only $n jni_native! entry points found (pattern drift?)"
    fail=1
fi

if [ "$fail" = 0 ]; then
    echo "jni-catch-coverage: OK ($n entry points through jni_native!)"
fi
exit "$fail"
