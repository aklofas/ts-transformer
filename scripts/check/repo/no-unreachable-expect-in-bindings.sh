#!/usr/bin/env bash
# Arc 2 R2 (META-04): no `unreachable!` and no `.expect(` in binding
# production code. An `unreachable!` on a #[non_exhaustive] wildcard is a
# latent abort the day tst-core adds a variant; an `.expect(` is a panic
# the binding's own panic policy (binding::panic) must never have to
# catch. Out of scope, because none of it can abort a caller's process on a
# future upstream change:
#   * everything from a file's TOP-LEVEL `#[cfg(test)]` to EOF (the project
#     convention is that the test module is the tail of the file). The marker
#     must be at column 0: an INDENTED `#[cfg(test)]` is a single test-only
#     item (e.g. `HandleRegistry::contains`) with production code after it,
#     and stopping there would blind the rail to the rest of the file;
#   * line comments (`//`, `///`, `//!`) — prose that NAMES `.expect(` or
#     `unreachable!`, typically to explain why the code does not use one, must
#     not be squeezed out by its own rail. A trailing comment does not shield
#     the code on the same line;
#   * `tests/` directories and `build.rs`.
#
# Run it:    bash scripts/check/repo/no-unreachable-expect-in-bindings.sh
# Self-test: bash scripts/check/repo/no-unreachable-expect-in-bindings.sh --self-test
#
# Overridable (for the self-test; both are read per call, so the self-test can
# point `scan` at a throwaway tree):
#   NUE_ROOT   repository root to scan
#   NUE_DIRS   space-separated, root-relative directories to scan
set -euo pipefail
DEFAULT_ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
DEFAULT_DIRS="bindings/c/core/src bindings/python/src bindings/jvm/src"

scan() {
  local root dirs hits=0 f
  root="${NUE_ROOT:-$DEFAULT_ROOT}"
  dirs="${NUE_DIRS:-$DEFAULT_DIRS}"
  while IFS= read -r f; do
    # awk: stop at the first column-0 #[cfg(test)]; skip comment-only lines;
    # report offending production lines.
    awk -v file="$f" '
      BEGIN { bad = 0 }
      /^#\[cfg\(test\)\]/ { exit }
      /^[[:space:]]*\/\// { next }
      /unreachable!|\.expect\(/ { printf "FAIL: %s:%d: %s\n", file, NR, $0; bad = 1 }
      END { exit bad }
    ' "$root/$f" >&2 || hits=$((hits + 1))
    # shellcheck disable=SC2086  # $dirs is a deliberate word-split list
  done < <(cd "$root" && find $dirs -name '*.rs' -not -name 'build.rs' -not -path '*/tests/*' | sort)
  [ "$hits" -eq 0 ]
}

self_test() {
  local tmp; tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  mkdir -p "$tmp/x/src"
  export NUE_ROOT="$tmp" NUE_DIRS="x/src"

  expect() { # <expected pass|fail> <label> ; the fixture is $tmp/x/src/lib.rs
    local want="$1" label="$2" got
    if scan >/dev/null 2>&1; then got=pass; else got=fail; fi
    if [ "$got" = "$want" ]; then
      echo "  ok: $label (expected $want)"
    else
      echo "  SELF-TEST FAIL: $label expected $want got $got" >&2
      return 1
    fi
  }

  printf 'fn a() -> u8 { let v: Option<u8> = None; v.expect("boom") }\n#[cfg(test)]\nmod t { fn b() { unreachable!() } }\n' > "$tmp/x/src/lib.rs"
  expect fail "production .expect(" || return 1

  printf 'fn a() -> u8 { 1 }\n#[cfg(test)]\nmod t { fn b() { unreachable!(); let v: Option<u8> = None; v.expect("x"); } }\n' > "$tmp/x/src/lib.rs"
  expect pass "test-module unreachable!/.expect( ignored" || return 1

  printf 'fn a() { match 1 { 1 => {}, _ => unreachable!("no") } }\n' > "$tmp/x/src/lib.rs"
  expect fail "production unreachable!" || return 1

  printf '/// Never `.expect()` here; see also unreachable!.\n//! module prose naming .expect(\nfn a() -> u8 { 1 }\n' > "$tmp/x/src/lib.rs"
  expect pass "comment prose naming unreachable!/.expect(" || return 1

  printf 'fn a() -> u8 { let v: Option<u8> = None; v.expect("boom") } // trailing note\n' > "$tmp/x/src/lib.rs"
  expect fail "a trailing comment does not shield the code on its line" || return 1

  printf 'struct S;\nimpl S {\n    #[cfg(test)]\n    fn t() {}\n}\nfn a() -> u8 { let v: Option<u8> = None; v.expect("boom") }\n' > "$tmp/x/src/lib.rs"
  expect fail "an indented #[cfg(test)] does not end the production region" || return 1

  printf 'fn a() -> u8 { 1 }\n' > "$tmp/x/src/lib.rs"
  expect pass "a clean file" || return 1

  echo "no-unreachable-expect-in-bindings self-test: PASS"
}

if [ "${1:-}" = "--self-test" ]; then
  self_test
else
  # Prove the checker still catches its negatives before trusting a pass.
  # Subshell so the self-test's env overrides cannot leak into the real scan.
  ( self_test ) >/dev/null
  if scan; then
    echo "no-unreachable-expect-in-bindings: OK"
  else
    echo "FAIL: see lines above (move the panic into a Result / BindingError, or the code into a #[cfg(test)] tail)" >&2
    exit 1
  fi
fi
