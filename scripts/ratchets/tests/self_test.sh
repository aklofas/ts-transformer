#!/usr/bin/env bash
# Negative-case self-test for the error-mapping coverage drivers. Proves the
# drivers actually DETECT gaps (not just pass on a clean tree) before the old
# per-protocol clones are deleted. Hermetic: builds synthetic fixtures in a
# tmpdir, so it never depends on the real source tree.
set -uo pipefail
DIR="$(cd "$(dirname "$0")/.." && pwd)"          # scripts/ratchets
# The `rust` driver (run-rust-coverage.sh) retired in Arc 2 WP-B1 together
# with the C binding's per-transport `*_error_to_code` converters; only the
# py / pyarm drivers remain to self-test.
PY="$DIR/run-py-coverage.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

fail=0
expect() { # <desc> <want_rc> <cmd...>
    local desc="$1" want="$2"; shift 2
    "$@" >"$tmp/out" 2>&1; local got=$?
    if [[ "$got" == "$want" ]]; then
        echo "ok: $desc"
    else
        echo "FAIL: $desc (got rc=$got want=$want)"; sed 's/^/    /' "$tmp/out"; fail=1
    fi
}

# ---- Python fixtures (one src dir per case) --------------------------------
printf 'one_column_no_tabs\n' > "$tmp/malformed.tsv"
cat > "$tmp/exceptions.py" <<'EOF'
class FooErrorKind:
    ALPHA = 1
    BETA = 2

class OtherErrorKind:
    GAMMA = 1
EOF
mkdir -p "$tmp/src_ok" "$tmp/src_missing" "$tmp/src_unknown" "$tmp/src_comment"
cat > "$tmp/src_ok/a.rs" <<'EOF'
let _ = make_foo_error(py, "ALPHA", "x");
let _ = make_foo_error(py, "BETA", "y");
EOF
cat > "$tmp/src_missing/a.rs" <<'EOF'
let _ = make_foo_error(py, "ALPHA", "x");
EOF
cat > "$tmp/src_unknown/a.rs" <<'EOF'
let _ = make_foo_error(py, "ALPHA", "x");
let _ = make_foo_error(py, "BETA", "y");
let _ = make_foo_error(py, "ZETA", "z");
EOF
cat > "$tmp/src_comment/a.rs" <<'EOF'
let _ = make_foo_error(py, "ALPHA", "x");
// let _ = make_foo_error(py, "BETA", "y");
EOF
printf 'py\tfoo\tFooErrorKind\tmake_foo_error\t-\n' > "$tmp/py.tsv"

expect "py: all variants have call sites passes" 0 bash "$PY" --tsv "$tmp/py.tsv" --exc-file "$tmp/exceptions.py" --src-dir "$tmp/src_ok"
expect "py: missing call site fails"             1 bash "$PY" --tsv "$tmp/py.tsv" --exc-file "$tmp/exceptions.py" --src-dir "$tmp/src_missing"
expect "py: unknown kind at call site fails"     1 bash "$PY" --tsv "$tmp/py.tsv" --exc-file "$tmp/exceptions.py" --src-dir "$tmp/src_unknown"
expect "py: comment-only call site not counted"  1 bash "$PY" --tsv "$tmp/py.tsv" --exc-file "$tmp/exceptions.py" --src-dir "$tmp/src_comment"
expect "py: malformed table fails closed"        1 bash "$PY" --tsv "$tmp/malformed.tsv" --exc-file "$tmp/exceptions.py" --src-dir "$tmp/src_ok"

# ---- pyarm fixtures (Rust-enum-variant -> explicit arm in a binding .rs) ---
cat > "$tmp/bar_enum.rs" <<'EOF'
pub enum BarError {
    Alpha,
    Beta(u32),
    Gamma { x: u32 },
}
EOF
cat > "$tmp/bar_arm_ok.rs" <<'EOF'
fn bar_error_to_pyerr(e: &BarError) -> PyErr {
    match e {
        BarError::Alpha => 1,
        BarError::Beta(_) => 2,
        BarError::Gamma { .. } => 3,
    }
}
EOF
cat > "$tmp/bar_arm_missing.rs" <<'EOF'
fn bar_error_to_pyerr(e: &BarError) -> PyErr {
    match e {
        BarError::Alpha => 1,
        BarError::Beta(_) => 2,
    }
}
EOF
# Mapper that locally aliases the enum (mirrors the real klv_encode_error_to_pyerr
# case) — only the alias spelling appears, never the canonical enum name.
cat > "$tmp/bar_arm_aliased.rs" <<'EOF'
fn bar_error_to_pyerr(e: &BarError) -> PyErr {
    use BarError as RustE;
    match e {
        RustE::Alpha => 1,
        RustE::Beta(_) => 2,
        RustE::Gamma { .. } => 3,
    }
}
EOF
printf 'pyarm\tbar\tBarError\t%s\t%s\tbar_error_to_pyerr\n' "$tmp/bar_enum.rs" "$tmp/bar_arm_ok.rs" > "$tmp/pyarm.tsv"
printf 'pyarm\tbar\tBarError\t%s\t%s\tbar_error_to_pyerr\n' "$tmp/bar_enum.rs" "$tmp/bar_arm_missing.rs" > "$tmp/pyarm_missing.tsv"
printf 'pyarm\tbar\tBarError\t%s\t%s\tbar_error_to_pyerr\tBarError|RustE\n' "$tmp/bar_enum.rs" "$tmp/bar_arm_aliased.rs" > "$tmp/pyarm_aliased.tsv"
printf 'pyarm\tbar\tBarError\t%s\t%s\tbar_error_to_pyerr\n' "$tmp/bar_enum.rs" "$tmp/bar_arm_aliased.rs" > "$tmp/pyarm_no_alias.tsv"

expect "pyarm: all variants have explicit arms passes" 0 bash "$PY" --tsv "$tmp/pyarm.tsv"
expect "pyarm: missing arm fails"                      1 bash "$PY" --tsv "$tmp/pyarm_missing.tsv"
expect "pyarm: match_names alias passes"                0 bash "$PY" --tsv "$tmp/pyarm_aliased.tsv"
expect "pyarm: aliased arms w/o match_names fail"       1 bash "$PY" --tsv "$tmp/pyarm_no_alias.tsv"

# ---- header rail: tool-failure fixtures (deep-review-4 X-META-01 / E12) ---
# The rail must never print PASS when the generator failed, and must
# distinguish "not installed locally" (SKIP, rc 0) from "not installed in
# CI" (FAIL). Hermetic: a fixture header carries the two defines and one
# guarded typedef block; the shim never reads the real cbindgen.toml.
HDR="$DIR/../check/c/header-conditional-sections.sh"
mkdir -p "$tmp/shim"
printf '#!/bin/sh\necho "shim: cbindgen failed" >&2\nexit 1\n' > "$tmp/shim/cbindgen"
chmod +x "$tmp/shim/cbindgen"
cat > "$tmp/fixture.h" <<'EOF'
#define TST_HAS_SRT 1
#define TST_HAS_RTP 1
#if defined(TST_HAS_RTP)
typedef struct TstRtpFixture TstRtpFixture;
#endif
EOF

expect "header rail: failing cbindgen shim on PATH fails closed"  1 env PATH="$tmp/shim:$PATH" CI=1 HCS_HEADER="$tmp/fixture.h" bash "$HDR"
expect "header rail: cbindgen absent under CI fails closed"        1 env CI=1 HCS_CBINDGEN="$tmp/nonexistent-cbindgen" HCS_HEADER="$tmp/fixture.h" bash "$HDR"
expect "header rail: cbindgen absent locally is SKIP (rc 0)"       0 env CI= HCS_CBINDGEN="$tmp/nonexistent-cbindgen" HCS_HEADER="$tmp/fixture.h" bash "$HDR"

if [[ "$fail" == 0 ]]; then echo "self-test: ALL OK"; fi
exit "$fail"
