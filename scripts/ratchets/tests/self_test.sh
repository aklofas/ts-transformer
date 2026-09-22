#!/usr/bin/env bash
# Negative-case self-test for the fail-closed ratchets: proves they actually
# DETECT their failure mode rather than passing on a clean tree. Hermetic:
# builds synthetic fixtures in a tmpdir, so it never depends on the real
# source tree.
#
# Gated by scripts/check/repo/ratchet-self-test.sh, which ci.yml runs on
# linux-x86_64.
#
# The whole error-mapping coverage scaffold is gone as of Arc 2, and its
# cases with it: the `rust` driver in WP-B1 (the C binding's per-transport
# `*_error_to_code` converters were deleted for
# `tst_pipeline::binding::BindingError`), the `py` / `pyarm` driver in WP-B2
# (tst-py has no per-kind `make_<proto>_error` call sites or hand-written
# per-variant mapper arms left to count), and the `java` rail in WP-B3 — at
# which point `scripts/ratchets/lib/coverage.sh` and
# `scripts/ratchets/error-mapping.tsv` had no reader at all and went too.
# What all three were guarding is now one Rust table:
# `scripts/check/rust/kind-table-coverage.sh` plus
# `scripts/check/repo/kind-equivalence.sh`, and binding-side
# `raise.rs::check_error_kinds` at `import tstrans`.
#
# The surviving cases are the C-header rail's.
set -uo pipefail
DIR="$(cd "$(dirname "$0")/.." && pwd)"          # scripts/ratchets

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
