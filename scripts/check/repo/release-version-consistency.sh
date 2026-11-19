#!/usr/bin/env bash
# Release-version-consistency ratchet.
#
# Asserts the project version is identical across every source-of-truth so a
# git tag (e.g. v0.2.0) can never publish a Maven/PyPI artifact whose source
# version silently disagrees with the tag.
#
# Six sources are checked (the ONLY version sites in the tree):
#   1. workspace Cargo        Cargo.toml                      [workspace.package] version
#   2. pyproject              bindings/python/pyproject.toml  [project] version
#   3. tst-py crate           bindings/python/Cargo.toml      [package] version
#   4. C version constants    bindings/c/core/src/lib.rs      TST_VERSION_{MAJOR,MINOR,PATCH}
#   5. C committed header     bindings/c/include/tstrans.h    TST_VERSION_{MAJOR,MINOR,PATCH}
#   6. Python version test    bindings/python/tests/test_version.py  tstrans.__version__
#
# NOTE: bindings/jvm/Cargo.toml and bindings/c/core/Cargo.toml use
# `version.workspace = true`, so they equal the workspace version BY
# CONSTRUCTION — they are intentionally not re-checked here.
#
# CRITICAL: sources 4 & 5 anchor on `TST_VERSION_` so they do NOT match the
# unrelated C ABI version (`TST_ABI_VERSION_*`, currently 0.19) — a different
# number that must be ignored.
#
# ADDITIONALLY, read from `cargo metadata --no-deps --offline` (so every TOML
# spelling is covered): every package on the publish roster
# (scripts/lib/roster.sh) has the workspace version (`tstrans-mbedtls-src`:
# the part before `+`), and every dependency on a path inside the workspace
# that carries a version requirement (`=X.Y.Z` or `X.Y.Z`) names that version.
# A stale one fails the ordered crates.io publish AFTER the irreversible
# layer-1 publish, or lets a layer skip a crate as "already live". The rail
# needs `cargo` and `jq` on PATH.
#
# Run it:    bash scripts/check/repo/release-version-consistency.sh
# Tag check: bash scripts/check/repo/release-version-consistency.sh 0.2.0
#            (or, on a tag build, GITHUB_REF_TYPE=tag GITHUB_REF_NAME=vX.Y.Z is
#            honoured; a tag that is not exactly vMAJOR.MINOR.PATCH fails. A
#            GITHUB_REF_TYPE of branch never reads the name as a tag. With
#            GITHUB_REF_TYPE unset, a name starting v<digit> is treated as a
#            tag under the same rule.)
#
# Overridable (scripts/ratchets/tests/self_test.sh points these at temp fixtures):
#   RVC_CARGO_TOML   workspace Cargo.toml
#   RVC_PYPROJECT    bindings/python/pyproject.toml
#   RVC_PY_CARGO     bindings/python/Cargo.toml
#   RVC_C_LIB        bindings/c/core/src/lib.rs
#   RVC_C_HEADER     bindings/c/include/tstrans.h
#   RVC_PY_TEST      bindings/python/tests/test_version.py
#   RVC_WORKSPACE    workspace root `cargo metadata` and the roster read
#
# Bash 3.2-portable (macOS CI is a gating platform): no
# `mapfile`/`readarray`/`declare -A`/`declare -a`, and `mktemp` is always given
# an explicit template (BSD/macOS `mktemp` requires one). Does not read stdin.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

RVC_CARGO_TOML="${RVC_CARGO_TOML:-$ROOT/Cargo.toml}"
RVC_PYPROJECT="${RVC_PYPROJECT:-$ROOT/bindings/python/pyproject.toml}"
RVC_PY_CARGO="${RVC_PY_CARGO:-$ROOT/bindings/python/Cargo.toml}"
RVC_C_LIB="${RVC_C_LIB:-$ROOT/bindings/c/core/src/lib.rs}"
RVC_C_HEADER="${RVC_C_HEADER:-$ROOT/bindings/c/include/tstrans.h}"
RVC_PY_TEST="${RVC_PY_TEST:-$ROOT/bindings/python/tests/test_version.py}"
RVC_WORKSPACE="${RVC_WORKSPACE:-$ROOT}"

# ---------------------------------------------------------------------------
# extractors — each echoes the version (or empty on no-match)
# ---------------------------------------------------------------------------

# First `^version = "X.Y.Z"` line (toml [package]/[workspace.package]/[project]).
toml_version() { # <file>
  grep -m1 -E '^version[[:space:]]*=' "$1" 2>/dev/null \
    | sed -nE 's/^version[[:space:]]*=[[:space:]]*"([^"]*)".*/\1/p'
}

# Compose MAJOR.MINOR.PATCH from `^pub const TST_VERSION_<PART> ... = N;`.
# Anchored on TST_VERSION_ to skip TST_ABI_VERSION_*.
rust_const_version() { # <file>
  local maj min pat
  maj="$(sed -nE 's/^pub const TST_VERSION_MAJOR[^=]*=[[:space:]]*([0-9]+).*/\1/p' "$1" 2>/dev/null | head -1)"
  min="$(sed -nE 's/^pub const TST_VERSION_MINOR[^=]*=[[:space:]]*([0-9]+).*/\1/p' "$1" 2>/dev/null | head -1)"
  pat="$(sed -nE 's/^pub const TST_VERSION_PATCH[^=]*=[[:space:]]*([0-9]+).*/\1/p' "$1" 2>/dev/null | head -1)"
  [ -n "$maj" ] && [ -n "$min" ] && [ -n "$pat" ] && printf '%s.%s.%s' "$maj" "$min" "$pat"
}

# Compose MAJOR.MINOR.PATCH from `^#define TST_VERSION_<PART> N`.
# Anchored on TST_VERSION_ to skip TST_ABI_VERSION_*.
cdefine_version() { # <file>
  local maj min pat
  maj="$(sed -nE 's/^#define TST_VERSION_MAJOR[[:space:]]+([0-9]+).*/\1/p' "$1" 2>/dev/null | head -1)"
  min="$(sed -nE 's/^#define TST_VERSION_MINOR[[:space:]]+([0-9]+).*/\1/p' "$1" 2>/dev/null | head -1)"
  pat="$(sed -nE 's/^#define TST_VERSION_PATCH[[:space:]]+([0-9]+).*/\1/p' "$1" 2>/dev/null | head -1)"
  [ -n "$maj" ] && [ -n "$min" ] && [ -n "$pat" ] && printf '%s.%s.%s' "$maj" "$min" "$pat"
}

# `assert tstrans.__version__ == "X.Y.Z"` -> X.Y.Z
pytest_version() { # <file>
  grep -m1 -E 'tstrans\.__version__[[:space:]]*==' "$1" 2>/dev/null \
    | sed -nE 's/.*==[[:space:]]*"([^"]*)".*/\1/p'
}

# One "FAIL: ..." line per roster package whose version differs from <version>
# or whose workspace path dependency requires another one, then a final
# "KEYS <n>" line counting the requirements compared.
internal_version_problems() { # <version> <roster names, one per line>
  local names
  names="$(printf '%s\n' "$2" | jq -R . | jq -s .)"
  (cd "$RVC_WORKSPACE" && cargo metadata --no-deps --offline --format-version 1) \
    | jq -r --arg v "$1" --argjson names "$names" '
        .workspace_root as $root
        | [.packages[] | select(.name as $n | $names | index($n))] as $pk
        | ($pk[] | select((.version | split("+")[0]) != $v)
            | "FAIL: package \(.name) has version \(.version) (workspace is \($v))"),
          ($pk[] | .name as $n | .dependencies[]
            | select(.path != null and (.path | startswith($root + "/")) and .req != "*")
            | select((.req | ltrimstr("^") | ltrimstr("=")) != $v)
            | "FAIL: \($n) requires \(.name) \(.req) (workspace is \($v))"),
          "KEYS \([$pk[] | .dependencies[] | select(.path != null and (.path | startswith($root + "/")) and .req != "*")] | length)"'
}

# ---------------------------------------------------------------------------
# run_check
# ---------------------------------------------------------------------------
run_check() { # [EXPECTED_VERSION]
  local expected="${1:-}"

  # If no explicit expected version, honour a tag ref. Only vMAJOR.MINOR.PATCH
  # is a release tag: anything else that looks like a tag fails here, before
  # any publish workflow writes to a registry. A branch name is never a tag.
  if [ -z "$expected" ] && [ -n "${GITHUB_REF_NAME:-}" ]; then
    local is_tag=0
    case "${GITHUB_REF_TYPE:-}" in
      tag) is_tag=1 ;;
      "") case "$GITHUB_REF_NAME" in v[0-9]*) is_tag=1 ;; esac ;;
    esac
    if [ "$is_tag" -eq 1 ]; then
      if ! printf '%s\n' "$GITHUB_REF_NAME" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
        echo "FAIL: tag '$GITHUB_REF_NAME' is not vMAJOR.MINOR.PATCH; only such tags publish (prereleases have no C version macro)" >&2
        return 1
      fi
      expected="${GITHUB_REF_NAME#v}"
    fi
  fi

  # Parallel arrays of source-label and extracted version (no assoc arrays).
  local labels=() versions=()
  local v

  # `|| true` so an extractor that returns non-zero (e.g. a missing/reformatted
  # version line in rust_const_version/cdefine_version, which `&&`-chain to a
  # non-zero status) yields an EMPTY $v under `set -e` instead of aborting the
  # script with no diagnostic — the empty-check loop below then reports which
  # source failed. Still fails closed (rc=1) either way.
  v="$(toml_version "$RVC_CARGO_TOML" || true)"
  labels+=("workspace-cargo  ($RVC_CARGO_TOML)"); versions+=("$v")
  v="$(toml_version "$RVC_PYPROJECT" || true)"
  labels+=("pyproject        ($RVC_PYPROJECT)"); versions+=("$v")
  v="$(toml_version "$RVC_PY_CARGO" || true)"
  labels+=("tst-py-cargo     ($RVC_PY_CARGO)"); versions+=("$v")
  v="$(rust_const_version "$RVC_C_LIB" || true)"
  labels+=("c-version-consts ($RVC_C_LIB)"); versions+=("$v")
  v="$(cdefine_version "$RVC_C_HEADER" || true)"
  labels+=("c-header-defines ($RVC_C_HEADER)"); versions+=("$v")
  v="$(pytest_version "$RVC_PY_TEST" || true)"
  labels+=("python-version-test ($RVC_PY_TEST)"); versions+=("$v")

  # (1) every extraction must be non-empty.
  local i empty=0
  for i in "${!versions[@]}"; do
    if [ -z "${versions[$i]}" ]; then
      echo "FAIL: could not extract a version from: ${labels[$i]}" >&2
      echo "      (a file moved or its version-line format changed)" >&2
      empty=1
    fi
  done
  [ "$empty" -eq 0 ] || return 1

  # (2) all six mutually equal.
  local first="${versions[0]}" mismatch=0
  for i in "${!versions[@]}"; do
    [ "${versions[$i]}" = "$first" ] || mismatch=1
  done
  if [ "$mismatch" -ne 0 ]; then
    echo "FAIL: source versions disagree:" >&2
    for i in "${!versions[@]}"; do
      if [ "${versions[$i]}" = "$first" ]; then
        printf '  %-22s %s\n' "${versions[$i]}" "${labels[$i]}" >&2
      else
        printf '  %-22s %s   <-- differs\n' "${versions[$i]}" "${labels[$i]}" >&2
      fi
    done
    return 1
  fi

  # (2.5) every roster package and internal path-dependency requirement must
  # equal $first too (see the ADDITIONALLY note in the file header).
  local missing=""
  command -v cargo >/dev/null 2>&1 || missing="cargo"
  command -v jq >/dev/null 2>&1 || missing="${missing:+$missing and }jq"
  command -v python3 >/dev/null 2>&1 || missing="${missing:+$missing and }python3"
  if [ -n "$missing" ]; then
    echo "FAIL: $missing not found on PATH; the rail reads package versions and internal pins from cargo metadata" >&2
    return 1
  fi
  local roster problems dep_keys=0 dep_mismatch=0 line
  roster="$(ROSTER_ROOT="$RVC_WORKSPACE" bash "$ROOT/scripts/lib/roster.sh" publishable)"
  problems="$(internal_version_problems "$first" "$roster")"
  while IFS= read -r line; do
    case "$line" in
      "KEYS "*) dep_keys="${line#KEYS }" ;;
      FAIL:*) echo "$line" >&2; dep_mismatch=1 ;;
    esac
  done <<< "$problems"
  [ "$dep_mismatch" -eq 0 ] || return 1

  # (3) optional expected/tag version must match the agreed version.
  if [ -n "$expected" ] && [ "$first" != "$expected" ]; then
    echo "FAIL: source version ($first) does not match expected/tag version ($expected)" >&2
    echo "      bump every source to $expected before tagging (or fix the tag)." >&2
    return 1
  fi

  # (4) success.
  if [ -n "$expected" ]; then
    echo "release-version-consistency: OK — all sources and $dep_keys internal dep version requirements agree at $first (matches expected $expected)"
  else
    echo "release-version-consistency: OK — all sources and $dep_keys internal dep version requirements agree at $first"
  fi
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
run_check "${1:-}"
