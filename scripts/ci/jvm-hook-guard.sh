#!/usr/bin/env bash
# Fails when a JVM archive or native library carries a test-hook native (a
# JNI symbol compiled only under the tst-jni `jni-test-hooks` feature), or
# when a sources JAR carries any native library. The hook symbols are read
# from bindings/jvm/src by jvm_hook_guard.py, never listed by hand.
#
# Usage: jvm-hook-guard.sh check [--expect-hooks] FILE...   (JAR, zip or library)
#        jvm-hook-guard.sh derive                           (print the hook symbols)
# --expect-hooks is the positive control: run against a build WITH the hooks,
# it fails unless every native holds a hook symbol, so a guard that sees
# nothing cannot pass. Runs on Linux, macOS and Windows (Git Bash).
set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Windows runners name the interpreter `python`; `python3` there can be a stub.
if python3 -c '' >/dev/null 2>&1; then py=python3; else py=python; fi
exec "$py" -I "$here/jvm_hook_guard.py" "$@"
