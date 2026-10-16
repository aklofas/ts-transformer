#!/usr/bin/env bash
# jni-catch-coverage: every JNI entry point is declared through `jni_native!`
# (bindings/jvm/src/panic.rs), which wraps its body in jni_catch so a Rust
# panic becomes a Java RuntimeException instead of unwinding into the JVM.
# A bare `pub extern "system" fn Java_…`, or a `#[unsafe(no_mangle)]` outside
# panic.rs (the macro) and lib.rs (JNI_OnLoad), is a native that can let a
# panic cross the boundary.
#
# Bash 3.2 / BSD grep portable: POSIX classes only, no mapfile, no grep -P.

set -euo pipefail
cd "$(dirname "$0")/../../.."

SRC_DIR="bindings/jvm/src"
fail=0

bare=$(grep -rnE '^[[:space:]]*pub extern "system" fn Java_' "$SRC_DIR" --include='*.rs' || true)
if [ -n "$bare" ]; then
    echo "FAIL: bare JNI entry points (declare them with jni_native!):"
    echo "$bare"
    fail=1
fi

stray=$(grep -rln '#\[unsafe(no_mangle)\]' "$SRC_DIR" --include='*.rs' | grep -vE '/(panic|lib)\.rs$' || true)
if [ -n "$stray" ]; then
    echo "FAIL: #[unsafe(no_mangle)] outside the jni_native! macro:"
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
