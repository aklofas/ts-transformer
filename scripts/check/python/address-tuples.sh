#!/usr/bin/env bash
# Every `(host, port)` tuple the Python binding returns is built by the one
# helper, `addr_tuple` in bindings/python/src/util.rs (or `peer_addr_tuple`,
# which calls it): it writes an IPv6 scope id as `%<id>` and reports an
# IPv4-mapped peer as the IPv4 address. A getter that formats the address
# itself drops both, and no loopback test can tell: no scoped or mapped SRT
# address can be opened on loopback.
#
# Two checks over bindings/python/src, util.rs excepted (the helper's home):
#
# 1. FAIL on `.ip()`, `.scope_id()` or `.to_ipv4_mapped()` outside a comment:
#    taking an address apart is the helper's job.
# 2. FAIL on a function whose return type (after `->`, over as many lines as
#    the signature takes) holds `(String, u16)` or `PyAddr` and whose body,
#    up to the next `fn`, never names `addr_tuple` or `peer_addr_tuple` (a
#    call, or the function passed to `.map(`).
#    The check fails closed when it finds no such function at all.
#
# A line scanner: a body is "up to the next `fn`", so a nested closure
# declared with `fn` would end it early (none does today).
#
# Portable: bash 3.2, POSIX awk.
#
# Test hook (scripts/ratchets/tests/self_test.sh): AT_ROOT overrides the
# directory scanned.
set -euo pipefail
export LC_ALL=C
cd "$(dirname "$0")/../../.."

ROOT="${AT_ROOT:-bindings/python/src}"
if [ ! -d "$ROOT" ]; then
    echo "FAIL: $ROOT is not a directory" >&2
    exit 1
fi

status=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

find "$ROOT" -type f -name '*.rs' ! -path "$ROOT/util.rs" | LC_ALL=C sort > "$tmp/files"

# ---- check 1: an address taken apart outside the helper -----------------
while IFS= read -r f; do
    awk -v file="$f" '
        {
            line = $0
            sub(/^[ \t]+/, "", line)
            if (line ~ /^\/\//) next
            sub(/[ \t]\/\/.*$/, "", line)
            if (line ~ /\.(ip|scope_id|to_ipv4_mapped)\(\)/) print file ":" NR ": " line
        }
    ' "$f"
done < "$tmp/files" > "$tmp/apart"

if [ -s "$tmp/apart" ]; then
    echo "FAIL: an address taken apart outside bindings/python/src/util.rs; build the tuple with util::addr_tuple:"
    sed 's/^/    /' "$tmp/apart"
    status=1
else
    echo "OK: no address is taken apart outside util.rs"
fi

# ---- check 2: every address-returning function calls the helper ---------
while IFS= read -r f; do
    awk -v file="$f" '
        function finish() {
            if (fn != "" && needs && !calls) print file ": fn " fn
            if (fn != "" && needs) print file ": fn " fn > "/dev/stderr"
        }
        {
            line = $0
            sub(/^[ \t]+/, "", line)
            if (line ~ /^\/\//) next
            sub(/[ \t]\/\/.*$/, "", line)
            if (match(line, /fn [A-Za-z_][A-Za-z0-9_]*/)) {
                finish()
                fn = substr(line, RSTART + 3, RLENGTH - 3)
                sig = ""
                insig = 1
                needs = 0
                calls = 0
            }
            if (insig) {
                sig = sig " " line
                if (line ~ /[{;]/) {
                    insig = 0
                    ret = sig
                    if (sub(/.*->/, "", ret) && ret ~ /\(String, u16\)|PyAddr/) needs = 1
                }
            }
            if (line ~ /addr_tuple([^A-Za-z0-9_]|$)/) calls = 1
        }
        END { finish() }
    ' "$f"
done < "$tmp/files" > "$tmp/bypass" 2> "$tmp/returning"

if [ ! -s "$tmp/returning" ]; then
    echo "FAIL: no function returning an address tuple found under $ROOT: the pattern drifted"
    exit 1
fi
if [ -s "$tmp/bypass" ]; then
    echo "FAIL: these functions return an address tuple without util::addr_tuple / peer_addr_tuple:"
    sed 's/^/    /' "$tmp/bypass"
    status=1
else
    n=$(wc -l < "$tmp/returning" | tr -d ' ')
    echo "OK: all $n functions returning an address tuple build it with util::addr_tuple"
fi

exit "$status"
