#!/usr/bin/env bash
# No `unreachable!` and no `.expect(` in binding
# production code. An `unreachable!` on a #[non_exhaustive] wildcard is a
# latent abort the day tst-core adds a variant; an `.expect(` is a panic
# the binding's own panic policy (binding::panic) must never have to
# catch. Out of scope, because none of it can abort a caller's process on a
# future upstream change:
#   * a file's TEST TAIL — a COLUMN-0 `#[cfg(test)]` whose item is a `mod`
#     (the project convention: the test module is the tail of the file). Two
#     narrower shapes are skipped as ONE ITEM and scanning then RESUMES, which
#     matters because both exist in tree and each would otherwise blind the
#     rail to hundreds of production lines:
#       - a column-0 `#[cfg(test)]` on a non-module item (`extern crate std;`
#         at bindings/c/core/src/lib.rs:27, `pub(crate) fn
#         clear_last_error_for_test` and a `use` at .../error.rs:324/:418);
#       - an INDENTED `#[cfg(test)]`, always a single test-only item
#         (e.g. `HandleRegistry::contains`) with production code after it.
#         An indented `#[cfg(test)] mod` would be skipped as one item too
#         rather than ending the file; none exists in the scanned crates, and
#         a nested test module is inside the item this already skips;
#   * line comments (`//`, `///`, `//!`) — prose that NAMES `.expect(` or
#     `unreachable!`, typically to explain why the code does not use one, must
#     not be squeezed out by its own rail. A trailing comment does not shield
#     the code on the same line;
#   * `tests/` directories and `build.rs`.
#
# Run it:    bash scripts/check/repo/no-unreachable-expect-in-bindings.sh
# Negative cases: scripts/ratchets/tests/self_test.sh
#
# Overridable (both are read per call; scripts/ratchets/tests/self_test.sh
# points `scan` at a throwaway tree):
#   NUE_ROOT   repository root to scan
#   NUE_DIRS   space-separated, root-relative directories to scan
#              (default: every binding crate's production Rust —
#              tst-c-core, the tst-c cdylib, tst-py and tst-jni)
set -euo pipefail
DEFAULT_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DEFAULT_DIRS="bindings/c/core/src bindings/c/src bindings/python/src bindings/jvm/src"

scan() {
  local root dirs hits=0 f
  root="${NUE_ROOT:-$DEFAULT_ROOT}"
  dirs="${NUE_DIRS:-$DEFAULT_DIRS}"
  while IFS= read -r f; do
    # awk: stop only at the test-module TAIL (a column-0 `#[cfg(test)]` on a
    # `mod` item); skip any other cfg(test) item and resume; skip comment-only
    # lines; report offending production lines.
    awk -v file="$f" '
      BEGIN { bad = 0; pend = 0; depth = 0 }
      # Net brace balance of a line (string/comment literals are close enough
      # for a line-oriented rail; a miscount only ever skips MORE code, and the
      # self-test pins both the one-line and multi-line item shapes).
      function braces(s,   t, o, c) {
        t = s; o = gsub(/\{/, "&", t)
        t = s; c = gsub(/\}/, "&", t)
        return o - c
      }
      function is_mod(s) {
        return s ~ /^[[:space:]]*(pub[[:space:]]*(\([^)]*\))?[[:space:]]+)?mod[[:space:]]/
      }
      {
        # Inside a cfg(test)-annotated NON-module item: skip to its end.
        if (depth > 0) { depth += braces($0); next }

        # A bare column-0 `#[cfg(test)]` on its own line: the item it annotates
        # is the next non-blank, non-attribute line.
        if (pend) {
          if ($0 ~ /^[[:space:]]*$/) next
          if ($0 ~ /^[[:space:]]*#\[/) next
          pend = 0
          if (is_mod($0)) exit            # test module = the file tail
          depth = braces($0)
          if (depth < 0) depth = 0
          next                            # the item head is test code either way
        }

        if ($0 ~ /^#\[cfg\(test\)\]/) {
          rest = $0
          sub(/^#\[cfg\(test\)\][[:space:]]*/, "", rest)
          if (rest == "") { pend = 1; next }
          if (is_mod(rest)) exit          # `#[cfg(test)] mod tests { … }`
          depth = braces($0)
          if (depth < 0) depth = 0
          next
        }

        if ($0 ~ /^[[:space:]]*\/\//) next
        if ($0 ~ /unreachable!|\.expect\(/) {
          printf "FAIL: %s:%d: %s\n", file, NR, $0
          bad = 1
        }
      }
      END { exit bad }
    ' "$root/$f" >&2 || hits=$((hits + 1))
    # shellcheck disable=SC2086  # $dirs is a deliberate word-split list
  done < <(cd "$root" && find $dirs -name '*.rs' -not -name 'build.rs' -not -path '*/tests/*' | sort)
  [ "$hits" -eq 0 ]
}

if scan; then
  echo "no-unreachable-expect-in-bindings: OK"
else
  echo "FAIL: see lines above (move the panic into a Result / BindingError, or the code into a #[cfg(test)] tail)" >&2
  exit 1
fi
