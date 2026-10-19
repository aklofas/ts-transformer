#!/usr/bin/env bash
# Proves the lint table enforces what used to be grep rails: a planted
# `.expect(` or `unreachable!` in binding production code (outside a local
# macro body, which stays with no-unreachable-expect-in-bindings.sh), a raw
# `slice::from_raw_parts` in tst-c-core and a bare `Python::detach` in
# tst-py each fail clippy. The planted code lives in
# scripts/ratchets/tests/lint_fixtures/ and is compiled only under
# `--cfg tst_lint_fixture`. A pass requires clippy to FAIL and to name every
# expected lint (its help URL), so an unrelated build error cannot pass.
set -euo pipefail
cd "$(dirname "$0")/../../.."
export SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1

fail=0
expect_lints() { # <package> <lint>...
  local pkg="$1" out rc=0 lint; shift
  out="$(cargo clippy -p "$pkg" --lib -- --cfg tst_lint_fixture 2>&1)" || rc=$?
  if [ "$rc" -eq 0 ]; then
    echo "FAIL: clippy -p $pkg passed with the planted fixture compiled in"; fail=1; return
  fi
  for lint in "$@"; do
    if printf '%s\n' "$out" | grep -q "index.html#$lint"; then
      echo "ok: $pkg rejects clippy::$lint"
    else
      echo "FAIL: clippy -p $pkg did not report clippy::$lint on the fixture"; fail=1
    fi
  done
}
expect_lints tst-c-core expect_used unreachable disallowed_methods
expect_lints tst-py expect_used disallowed_methods
expect_lints tst-jni expect_used unreachable
exit "$fail"
