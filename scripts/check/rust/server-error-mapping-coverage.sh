#!/usr/bin/env bash
set -euo pipefail
# Phase 3 ratchet (lives under scripts/check/rust/).
#
# Verifies every RtspServerError arm is constructed somewhere under
# crates/tst-rtp/src/rtsp/server/ or crates/tst-rtp/src/builder.rs.
# Intent: no silent error paths in the server lifecycle; every defined
# arm has a visible construction site that maps to a real failure mode.
#
# A producer-existence rail, not an exhaustiveness rail despite the name —
# same shape as the former check/rust/mux-error-kind-coverage.sh (plan #79,
# deleted in Arc 2 R2 once the compiler could pin MuxError::kind()).

src="crates/tst-rtp/src/error.rs"

# Extract variant names from the `pub enum RtspServerError { ... }` body.
# Each variant is a line like `    Io(io::ErrorKind),` or `    BadName,`
# or `    Foo { ... },` — capture the leading CamelCase identifier.
arms=$(awk '
    /pub enum RtspServerError \{/ { in_enum = 1; next }
    in_enum && /^\}/ { exit }
    in_enum && /^    [A-Z]/ {
        # Strip leading 4 spaces; identifier ends at the first
        # non-alphanumeric (paren / brace / comma / whitespace).
        line = $0
        sub(/^    /, "", line)
        # Use sub to keep only the leading identifier
        sub(/[^A-Za-z0-9_].*$/, "", line)
        if (line ~ /^[A-Z][A-Za-z0-9_]*$/) print line
    }
' "$src" | sort -u)

if [ -z "$arms" ]; then
    echo "FAIL: could not extract any RtspServerError variants from $src" >&2
    exit 1
fi

fails=0
for name in $arms; do
    usages=$(rg -l "RtspServerError::$name" crates/tst-rtp/src/rtsp/server/ crates/tst-rtp/src/builder.rs 2>/dev/null || true)
    if [ -z "$usages" ]; then
        echo "FAIL: RtspServerError::$name is never constructed in src/rtsp/server/ or src/builder.rs" >&2
        fails=$((fails+1))
    fi
done
if [ $fails -gt 0 ]; then exit 1; fi
echo "OK: all RtspServerError arms are surfaced"
