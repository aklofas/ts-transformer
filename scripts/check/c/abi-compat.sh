#!/usr/bin/env bash
# C ABI compatibility check: a libtstrans may only GROW, and only with a minor
# bump (the additive-only rule documented at TST_ABI_VERSION_MINOR in
# bindings/c/core/src/lib.rs).
#
# Usage: abi-compat.sh [--header <new tstrans.h>] [--old-header <old tstrans.h>]
#                      [--old-tag <vX.Y.Z>]
#                      <old-lib-or-abi.xml> <new-lib> <old-minor> <new-minor>
#                      [<old-major> <new-major>]
#        abi-compat.sh --check-prototypes [--old-tag <vX.Y.Z>] <old tstrans.h> <new tstrans.h>
#   <old> is a shared library or a libabigail corpus written by `abidw`;
#   <new> is a shared library. The majors default to 0 (pre-1.0).
#   Both sides need debug info (an ELF .debug_info section, a corpus with
#   <abi-instr): without it abidiff compares symbol names only and would pass
#   a grown struct. A side without it is an error (exit 2) naming the file.
#   --old-header is the header the OLD side shipped with. Without it the rail
#   cannot tell that a struct lost its body: under CI that is an error (exit
#   2), locally a warning (the new header alone decides the suppressions).
#   --old-tag is the tag the OLD side was built from; it judges when the
#   enum-retype suppression in bindings/c/abi.suppr expires (below).
#   --check-prototypes runs only the prototype pin and the expiry check on two
#   headers (exit 0/1/2); it needs no library and no abidiff.
#
# What it judges, over the exported tst_* functions and TST_*/tst_* variables
# (bindings/c/abi.suppr drops everything else, e.g. the Rust runtime):
#   - ANY change abidiff reports (exit bit 4 or 8: a removed or changed function,
#     a struct that grew or moved, a renumbered enumerator) FAILS. Pre-1.0 a C
#     library only grows, so this holds whatever the minor says. An intended
#     change needs a commented suppression in bindings/c/abi.suppr. At
#     new-major > old-major the check passes (the 1.0 break).
#   - an added exported symbol (a separate symbol diff) needs new-minor > old-minor.
#
# Opaque handles: a `typedef struct X X;` (declared without a body) in BOTH
# headers is a type whose layout C never sees; its changes are suppressed (also
# under its Rust spelling: tst_sender_t is TstSender in the debug info). The
# list is derived on every run from --header and --old-header (default new
# header: bindings/c/include/tstrans.h of this tree, which declares every
# feature's handles under its TST_HAS_* guards; the c-abi job passes the staged
# package's header). A struct bodiless in the new header but with a body in the
# old one FAILS: a field-visible struct became opaque (cbindgen renders
# repr(align) and repr(packed) structs without a body), which C code compiled
# against the old header still sees as a layout change.
#
# The enum-retype suppression: abi.suppr hides the DWARF parameter-type change of
# the functions whose enum parameter became int32_t, which would also hide any
# later change to their signatures. So when BOTH --header and --old-header are
# given, each exported function matching that block's name_regexp must have a
# header prototype that equals the old one once the block's `# retype:` spellings
# are replaced by their new spelling; any other difference FAILS. With --old-tag,
# the block FAILS once the baseline tag is at or after its
# `# expires-with-baseline:` version: delete the block then. The names,
# spellings and version are read from the block; there is no second copy.
#
# Argument-driven: with no arguments it prints this usage and exits 2, with or
# without abidiff. tests/abi-compat-selftest.sh is the entry point for sweeps.
#
# Exit: 0 compatible, 1 violates the rule (report printed), 2 tool/usage error
# or a missing input. Without abidiff: `SKIP` (exit 0), except under CI (exit 1),
# so a runner that lost the tool cannot pass silently.
set -uo pipefail

HEADER="" OLD_HEADER="" OLD_TAG="" CHECK_PROTOS=0 HEADER_GIVEN=0
args=()
while [ $# -gt 0 ]; do
    case "$1" in
        --header) [ $# -ge 2 ] || { echo "abi-compat: --header needs a path" >&2; exit 2; }; HEADER="$2"; HEADER_GIVEN=1; shift 2 ;;
        --old-header) [ $# -ge 2 ] || { echo "abi-compat: --old-header needs a path" >&2; exit 2; }; OLD_HEADER="$2"; HEADER_GIVEN=1; shift 2 ;;
        --old-tag) [ $# -ge 2 ] || { echo "abi-compat: --old-tag needs a tag" >&2; exit 2; }; OLD_TAG="$2"; shift 2 ;;
        --check-prototypes) CHECK_PROTOS=1; shift ;;
        *) args+=("$1"); shift ;;
    esac
done
set -- ${args[@]+"${args[@]}"}
usage() { sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//' >&2; exit 2; }
if [ "$CHECK_PROTOS" -eq 1 ]; then
    [ $# -eq 2 ] || usage
elif [ $# -ne 4 ] && [ $# -ne 6 ]; then
    usage
fi

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SUPPR="$ROOT/bindings/c/abi.suppr"
[ -f "$SUPPR" ] || { echo "abi-compat: missing $SUPPR" >&2; exit 2; }
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The enum-retype block of abi.suppr (the one with change_kind =
# function-subtype-change): its name_regexp, `# retype: <old> -> <new>` pairs and
# `# expires-with-baseline: <tag>` land in $tmp/retype.{regexp,pairs,expires}.
# No such block (deleted after it expired) leaves them absent.
parse_suppression_block() {
    awk -v out="$tmp/retype" '
        function flush() {
            if (blk ~ /(^|\n)[ \t]*change_kind[ \t]*=[ \t]*function-subtype-change[ \t]*(\n|$)/) {
                n = split(blk, l, "\n")
                for (i = 1; i <= n; i++) {
                    if (match(l[i], /^[ \t]*name_regexp[ \t]*=[ \t]*/)) print substr(l[i], RLENGTH + 1) > (out ".regexp")
                    else if (match(l[i], /^[ \t]*#[ \t]*retype:[ \t]*/)) { v = substr(l[i], RLENGTH + 1); sub(/[ \t]*->[ \t]*/, "\t", v); print v > (out ".pairs") }
                    else if (match(l[i], /^[ \t]*#[ \t]*expires-with-baseline:[ \t]*/)) print substr(l[i], RLENGTH + 1) > (out ".expires")
                }
                found = 1
            }
            blk = ""
        }
        /^\[/ { flush() }
        { blk = blk $0 "\n" }
        END { flush() }' "$SUPPR"
    if [ -f "$tmp/retype.regexp" ] || [ -f "$tmp/retype.pairs" ] || [ -f "$tmp/retype.expires" ]; then
        local f
        for f in regexp pairs expires; do
            [ -s "$tmp/retype.$f" ] || { echo "abi-compat: the enum-retype block in $SUPPR lacks its $f line" >&2; exit 2; }
        done
    fi
}

# A header's declaration of <name>: block comments and preprocessor lines
# dropped, lines joined, from the previous ; { } to the first ; after `name(`,
# whitespace squeezed. Empty when absent.
prototype() { # <header> <name>
    awk -v name="$2" '
        /^[ \t]*#/ { next }
        { t = t " " $0 }
        END {
            while ((s = index(t, "/*")) > 0) {
                e = index(substr(t, s + 2), "*/")
                if (e == 0) { t = substr(t, 1, s - 1); break }
                t = substr(t, 1, s - 1) " " substr(t, s + e + 3)
            }
            pos = 1
            while ((i = index(substr(t, pos), name "(")) > 0) {
                i += pos - 1
                if (i == 1 || substr(t, i - 1, 1) !~ /[A-Za-z0-9_]/) break
                pos = i + 1; i = 0
            }
            if (i == 0) exit
            b = i
            while (b > 1 && substr(t, b - 1, 1) !~ /[;{}]/) b--
            e = index(substr(t, i), ";")
            if (e == 0) exit
            d = substr(t, b, i + e - b)
            gsub(/[ \t]+/, " ", d); sub(/^ /, "", d); sub(/ $/, "", d)
            print d
        }' "$1"
}

# Every function the header declares whose name matches <regexp>.
declared_matching() { # <header> <regexp>
    awk '/^[ \t]*#/ { next } { t = t " " $0 }
         END { while ((s = index(t, "/*")) > 0) { e = index(substr(t, s + 2), "*/"); if (e == 0) { t = substr(t, 1, s - 1); break } t = substr(t, 1, s - 1) " " substr(t, s + e + 3) } print t }' "$1" \
        | grep -oE '[A-Za-z_][A-Za-z0-9_]*\(' | sed 's/($//' | grep -E "$2" | sort -u
}

# The old prototype with every accepted old spelling replaced must equal the new
# one. A name absent from the old header is an added symbol (the minor rule judges it).
check_prototypes() { # <old-header> <new-header> <names-file>
    local name old new from to bad=0
    while IFS= read -r name; do
        [ -n "$name" ] || continue
        old="$(prototype "$1" "$name")"
        [ -n "$old" ] || continue
        new="$(prototype "$2" "$name")"
        while IFS=$'\t' read -r from to; do
            old="${old//"$from"/$to}"
        done <"$tmp/retype.pairs"
        if [ "$old" != "$new" ]; then
            echo "FAIL: prototype of $name changed beyond the accepted enum-to-int32_t retype:"
            echo "  old (retyped): $old"
            echo "  new:           $new"
            bad=1
        fi
    done <"$3"
    return "$bad"
}

# Expiry: the block goes once the baseline tag is at or after its version.
check_expiry() {
    if [ -z "$OLD_TAG" ]; then
        echo "abi-compat: note: no --old-tag, the suppression expiry is not judged"
        return 0
    fi
    [ -f "$tmp/retype.expires" ] || return 0
    local exp low
    exp="$(cat "$tmp/retype.expires")"
    low="$(printf '%s\n%s\n' "$exp" "$OLD_TAG" | sort -V | head -1)"
    if [ "$low" = "$exp" ]; then
        echo "FAIL: the enum-retype suppression in bindings/c/abi.suppr expired: the baseline $OLD_TAG already carries the retype; delete the block (and the self-test rows that borrow its names)"
        return 1
    fi
}

parse_suppression_block

if [ "$CHECK_PROTOS" -eq 1 ]; then
    [ -r "$1" ] || { echo "abi-compat: cannot read old header: $1" >&2; exit 2; }
    [ -r "$2" ] || { echo "abi-compat: cannot read header: $2" >&2; exit 2; }
    check_expiry || exit 1
    [ -f "$tmp/retype.regexp" ] || { echo "abi-compat: no enum-retype block in abi.suppr, nothing to pin"; exit 0; }
    declared_matching "$2" "$(cat "$tmp/retype.regexp")" >"$tmp/names"
    check_prototypes "$1" "$2" "$tmp/names" || exit 1
    echo "ok: retyped prototypes unchanged beyond the accepted retype ($(wc -l <"$tmp/names" | tr -d ' ') functions)"
    exit 0
fi

if ! command -v abidiff >/dev/null 2>&1; then
    if [ -n "${CI:-}" ]; then echo "FAIL: abidiff not installed (required under CI)" >&2; exit 1; fi
    echo "SKIP: abidiff not installed"
    exit 0
fi
OLD="$1" NEW="$2" OLD_MINOR="$3" NEW_MINOR="$4" OLD_MAJOR="${5:-0}" NEW_MAJOR="${6:-0}"
for n in "$OLD_MINOR" "$NEW_MINOR" "$OLD_MAJOR" "$NEW_MAJOR"; do
    case "$n" in ''|*[!0-9]*) echo "abi-compat: version fields must be integers: '$n'" >&2; exit 2 ;; esac
done

command -v nm >/dev/null 2>&1 || { echo "abi-compat: nm not installed (binutils)" >&2; exit 2; }
command -v readelf >/dev/null 2>&1 || { echo "abi-compat: readelf not installed (binutils)" >&2; exit 2; }
for f in "$OLD" "$NEW"; do
    [ -f "$f" ] || { echo "abi-compat: no such file: $f" >&2; exit 2; }
done
# Debug info on both sides, or abidiff compares symbol names only.
has_debug_info() { # <file>
    case "$1" in
        *.abi|*.xml) grep -q '<abi-instr' "$1" ;;
        # Into a variable: `grep -q` closing the pipe early would fail the
        # pipeline (SIGPIPE) under pipefail on a large library.
        *) local sections; sections="$(readelf -S -W "$1" 2>/dev/null)"; grep -qE '\.z?debug_info' <<<"$sections" ;;
    esac
}
for f in "$OLD" "$NEW"; do
    has_debug_info "$f" || {
        echo "abi-compat: no debug info in $f: abidiff would compare symbols only -" \
             "build the library with debug info / produce the corpus from an unstripped build" >&2
        exit 2
    }
done

[ -n "$HEADER" ] || HEADER="$ROOT/bindings/c/include/tstrans.h"
[ -r "$HEADER" ] || { echo "abi-compat: cannot read header: $HEADER" >&2; exit 2; }

check_expiry || exit 1

# Opaque handle types, derived from the headers, appended to the suppressions.
# DWARF names the Rust struct (TstSender) where the header says tst_sender_t,
# so each tst_<snake>_t name also yields its Tst<Camel> spelling.
bodiless() { sed -n 's/^typedef struct \([A-Za-z0-9_][A-Za-z0-9_]*\) \1;.*/\1/p' "$1" | sort -u; }
bodiless "$HEADER" >"$tmp/new.bodiless"
if [ -n "$OLD_HEADER" ]; then
    [ -r "$OLD_HEADER" ] || { echo "abi-compat: cannot read old header: $OLD_HEADER" >&2; exit 2; }
    bodiless "$OLD_HEADER" >"$tmp/old.bodiless"
    lost=0
    while IFS= read -r name; do
        if grep -qE "^(typedef )?struct $name \{" "$OLD_HEADER"; then
            if [ "$NEW_MAJOR" -gt "$OLD_MAJOR" ]; then
                echo "abi-compat: struct $name lost its body (allowed: major $OLD_MAJOR -> $NEW_MAJOR)"
            else
                echo "FAIL: struct $name lost its body: a field-visible struct became opaque (repr(align/packed) renders bodiless)"
                lost=1
            fi
        fi
    done <"$tmp/new.bodiless"
    [ "$lost" -eq 0 ] || exit 1
    comm -12 "$tmp/old.bodiless" "$tmp/new.bodiless" >"$tmp/both.bodiless"
else
    if [ -n "${CI:-}" ]; then
        echo "abi-compat: --old-header is required under CI: without it a struct that lost its body cannot be judged" >&2
        exit 2
    fi
    echo "abi-compat: WARNING: no --old-header, a struct that lost its body cannot be judged" >&2
    cp "$tmp/new.bodiless" "$tmp/both.bodiless"
fi
awk '{
        print
        if ($0 ~ /^tst_.*_t$/) {
            n = split(substr($0, 5, length($0) - 6), w, "_"); c = "Tst"
            for (i = 1; i <= n; i++) c = c toupper(substr(w[i], 1, 1)) substr(w[i], 2)
            print c
        }
    }' "$tmp/both.bodiless" \
    | sort -u >"$tmp/opaque"
n_opaque="$(wc -l <"$tmp/opaque" | tr -d ' ')"
cp "$SUPPR" "$tmp/suppr"
if [ "$n_opaque" -gt 0 ]; then
    { printf '\n[suppress_type]\n  type_kind = struct\n  name_regexp = (^|::)(%s)$\n' "$(paste -sd'|' "$tmp/opaque")"; } >>"$tmp/suppr"
fi
echo "abi-compat: $n_opaque opaque handle types suppressed (bodiless in $HEADER${OLD_HEADER:+ and $OLD_HEADER})"

# Exported C symbols of a library or an abidw corpus, one per line; a failing
# listing is an error (exit 2), never an empty set.
symbols() { # <file> <out>
    local raw="$tmp/raw.$$"
    case "$1" in
        *.abi|*.xml) grep -o "<elf-symbol name='[^']*'" "$1" | sed "s/.*name='//; s/'$//" >"$raw" || true ;;
        *) nm -D --defined-only "$1" 2>"$tmp/nm.err" | awk '{print $3}' >"$raw"
           if [ "${PIPESTATUS[0]}" -ne 0 ]; then
               echo "abi-compat: nm failed on $1" >&2; cat "$tmp/nm.err" >&2; exit 2
           fi ;;
    esac
    grep -E '^(tst_|TST_)' "$raw" | sort -u >"$2" || true
    if [ ! -s "$2" ]; then
        echo "abi-compat: no tst_ symbols in $1: not a tstrans library, or a stripped/stale build" >&2
        exit 2
    fi
}

abidiff --no-added-syms --suppressions "$tmp/suppr" "$OLD" "$NEW" >"$tmp/report" 2>&1
rc=$?
if [ $((rc & 3)) -ne 0 ]; then
    echo "abi-compat: abidiff failed (exit $rc)" >&2
    cat "$tmp/report" >&2
    exit 2
fi

symbols "$OLD" "$tmp/old.sym"
symbols "$NEW" "$tmp/new.sym"
comm -13 "$tmp/old.sym" "$tmp/new.sym" >"$tmp/added"
comm -23 "$tmp/old.sym" "$tmp/new.sym" >"$tmp/gone"

count() { wc -l <"$1" | tr -d ' '; }
echo "abi-compat: abidiff exit $rc"
echo "abi-compat: $(count "$tmp/old.sym") old / $(count "$tmp/new.sym") new tst_ symbols," \
     "$(count "$tmp/added") added, $(count "$tmp/gone") removed"
changed=0
[ $((rc & 12)) -ne 0 ] && changed=1
[ -s "$tmp/gone" ] && changed=1

show() {
    cat "$tmp/report"
    if [ -s "$tmp/gone" ]; then echo "Removed exported symbols:"; sed 's/^/  /' "$tmp/gone"; fi
    if [ -s "$tmp/added" ]; then echo "Added exported symbols:"; sed 's/^/  /' "$tmp/added"; fi
}

if [ "$changed" -eq 1 ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ]; then
    show
    echo "FAIL: abidiff reports a change; a pre-1.0 C library only grows - an intended change needs a commented suppression in bindings/c/abi.suppr"
    exit 1
fi
if [ -s "$tmp/added" ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ] && [ "$NEW_MINOR" -le "$OLD_MINOR" ]; then
    show
    echo "FAIL: the C ABI grew without a minor bump (minor $OLD_MINOR -> $NEW_MINOR)"
    exit 1
fi
if [ -f "$tmp/retype.regexp" ] && [ "$NEW_MAJOR" -le "$OLD_MAJOR" ]; then
    if [ "$HEADER_GIVEN" -eq 1 ] && [ -n "$OLD_HEADER" ]; then
        grep -E "$(cat "$tmp/retype.regexp")" "$tmp/new.sym" >"$tmp/names" || true
        check_prototypes "$OLD_HEADER" "$HEADER" "$tmp/names" || exit 1
    else
        echo "abi-compat: note: the enum-retype prototype pin needs --header and --old-header, skipped"
    fi
fi
echo "ok: C ABI compatible (minor $OLD_MINOR -> $NEW_MINOR)"
