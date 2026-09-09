#!/usr/bin/env bash
# Plan A5b Wave C T14 bash ratchet: the Python `Publisher` ABC's method
# list (from tstrans/hls.pyi) must mirror the Rust
# `tst_core::publisher::Publisher` trait method list exactly.
#
# This catches drift where a Rust trait method is added/renamed but the
# Python ABC (and its pyi contract) isn't updated, or vice versa.
#
# Method extraction is from source files (not the built extension) so the
# ratchet runs without maturin. The Rust side reads the `pub trait
# Publisher { ... }` block in crates/tst-core/src/publisher/mod.rs; the
# Python side reads the `class Publisher(abc.ABC):` block in
# bindings/python/python/tstrans/hls.pyi.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
RUST_FILE="$ROOT/crates/tst-core/src/publisher/mod.rs"
PYI_FILE="$ROOT/bindings/python/python/tstrans/hls.pyi"

# Rust trait methods: lines like `fn push_ts(...)` inside the
# `pub trait Publisher { ... }` block, before its closing brace.
# `type Error` and the `where` clause are skipped (we match `fn `).
rust_methods=$(awk '
  /^pub trait Publisher/ { in_trait = 1; next }
  in_trait && /^}/ { in_trait = 0 }
  in_trait && /fn [a-z_]+/ {
    line = $0
    sub(/.*fn /, "", line)
    sub(/[^a-z_].*$/, "", line)
    print line
  }
' "$RUST_FILE" | sort -u)

# Python ABC methods: `def <name>(` lines inside the `class Publisher(abc.ABC):`
# block (terminated by the next top-level `class `). Dunders are skipped.
py_methods=$(awk '
  /^class Publisher(\(abc\.ABC\))?:/ { in_cls = 1; next }
  in_cls && /^class / { in_cls = 0 }
  in_cls && /^    def [a-z_]+\(/ {
    line = $0
    sub(/.*def /, "", line)
    sub(/\(.*$/, "", line)
    if (line !~ /^__/) print line
  }
' "$PYI_FILE" | sort -u)

if [[ -z "$rust_methods" ]]; then
    echo "FAIL: could not extract any methods from the Rust Publisher trait at $RUST_FILE" >&2
    exit 1
fi
if [[ -z "$py_methods" ]]; then
    echo "FAIL: could not extract any methods from the Python Publisher ABC at $PYI_FILE" >&2
    exit 1
fi

if ! diff <(echo "$rust_methods") <(echo "$py_methods") >/dev/null; then
    echo "FAIL: Publisher trait/ABC method-mirror drift." >&2
    echo "Rust trait (tst_core::publisher::Publisher) methods:" >&2
    while IFS= read -r m; do echo "  rust: $m" >&2; done <<< "$rust_methods"
    echo "Python ABC (tstrans.hls.Publisher) methods:" >&2
    while IFS= read -r m; do echo "  py:   $m" >&2; done <<< "$py_methods"
    echo "Reconcile bindings/python/python/tstrans/hls.pyi with the Rust trait." >&2
    exit 1
fi

echo "OK: Publisher trait/ABC mirror ($(echo "$rust_methods" | wc -l | tr -d ' ') methods)"
