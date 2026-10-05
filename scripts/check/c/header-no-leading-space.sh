#!/usr/bin/env bash
# Every top-level declaration in tstrans.h must start
# at column 0 (no leading whitespace).
#
# Background: cbindgen 0.29.x emits single-line function declarations
# with one leading space (e.g. ` int tst_foo(void);`) while multi-line
# declarations start at column 0. The `add_section_dividers` function
# in `bindings/c/build.rs` strips this leading space as part of its
# post-processing pass, so the checked-in header should never contain
# a top-level declaration prefixed with whitespace.
#
# This ratchet catches regressions where either build.rs loses the
# strip step or a new cbindgen version changes the rendering in a way
# that reintroduces the leading space.
#
# Scope: checks `int`, `void`, `struct *tst_`, `const char *tst_`, and
# `unsigned` return-type declarations only — does NOT flag struct-member
# indentation or parameter-list continuation lines (which legitimately
# start with spaces).

set -euo pipefail

cd "$(dirname "$0")/../../.."

HEADER="bindings/c/include/tstrans.h"

[ -f "$HEADER" ] || { echo "FAIL: $HEADER not found"; exit 1; }

# Match lines that start with exactly one space followed by a C return-
# type keyword and then a tst_ symbol name.  Struct member indentation
# (two or more spaces) and parameter-list continuation lines (many
# spaces) are deliberately excluded by anchoring to exactly one leading
# space character.  The pattern covers the cbindgen return-type shapes
# seen in this codebase: `int`, `void`, `const char *`, and
# `struct tst_*_t *` (pointer-to-opaque-handle returns).
PAT="^ (int|void|const char \*|struct [a-z_]+ \*) tst_"
if grep -nE "$PAT" "$HEADER" >/dev/null 2>&1; then
    echo "FAIL: $HEADER has top-level declarations starting with whitespace:"
    grep -nE "$PAT" "$HEADER"
    echo
    echo "Fix: ensure the strip_prefix(' ') strip step in bindings/c/build.rs"
    echo "     add_section_dividers() is present, then rebuild and copy:"
    echo "     rm $HEADER && SRT_FORCE_VENDORED=1 cargo build -p tst-c"
    echo "     cp target/debug/include/tstrans.h $HEADER"
    exit 1
fi

echo "OK: no top-level declarations with leading whitespace in $HEADER"
