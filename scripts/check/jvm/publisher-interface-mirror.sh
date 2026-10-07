#!/usr/bin/env bash
# Bash ratchet: the Java `org.tstrans.hls.Publisher` interface's method list
# must mirror the Rust `tst_core::publisher::Publisher` trait method list
# exactly (snake_case ↔ camelCase). Catches drift when a trait method is
# added/renamed but the interface is not, or vice versa. Source-level (no
# Gradle needed).

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
RUST_FILE="$ROOT/crates/tst-core/src/publisher/mod.rs"
JAVA_FILE="$ROOT/bindings/jvm/src/main/java/org/tstrans/hls/Publisher.java"

# Rust trait methods, converted to camelCase.
rust_methods=$(awk '
  /^pub trait Publisher/ { in_trait = 1; next }
  in_trait && /^}/ { in_trait = 0 }
  in_trait && /fn [a-z_]+/ {
    line = $0
    sub(/.*fn /, "", line)
    sub(/[^a-z_].*$/, "", line)
    n = split(line, parts, "_")
    out = parts[1]
    for (i = 2; i <= n; i++) out = out toupper(substr(parts[i], 1, 1)) substr(parts[i], 2)
    print out
  }
' "$RUST_FILE" | sort -u)

# Java interface methods: `<Type> <name>(` lines inside the interface body,
# skipping `close` (AutoCloseable, not a trait method) and comments.
java_methods=$(awk '
  /^public interface Publisher/ { in_if = 1; next }
  in_if && /^}/ { in_if = 0 }
  in_if && /^    (default |static )?[A-Za-z<>\[\]]+ [a-zA-Z]+\(/ {
    line = $0
    sub(/\(.*$/, "", line)
    sub(/.* /, "", line)
    if (line != "close") print line
  }
' "$JAVA_FILE" | sort -u)

if [[ -z "$rust_methods" ]]; then
    echo "FAIL: could not extract any methods from the Rust Publisher trait at $RUST_FILE" >&2
    exit 1
fi
if [[ -z "$java_methods" ]]; then
    echo "FAIL: could not extract any methods from the Java Publisher interface at $JAVA_FILE" >&2
    exit 1
fi

if ! diff <(echo "$rust_methods") <(echo "$java_methods") >/dev/null; then
    echo "FAIL: Publisher trait/interface method-mirror drift." >&2
    while IFS= read -r m; do echo "  rust: $m" >&2; done <<< "$rust_methods"
    while IFS= read -r m; do echo "  java: $m" >&2; done <<< "$java_methods"
    echo "Reconcile bindings/jvm/src/main/java/org/tstrans/hls/Publisher.java with the Rust trait." >&2
    exit 1
fi

echo "OK: Publisher trait/interface mirror ($(echo "$rust_methods" | wc -l | tr -d ' ') methods)"
