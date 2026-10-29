#!/usr/bin/env bash
# Self-test for `scripts/dev/prev-c-library.sh release` with `gh` stubbed on
# PATH: the previous tag's C package is used only when SHA256SUMS lists it with
# the right digest and its build-provenance attestation verifies; otherwise no
# `lib=` is written and the source build is used.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PREVLIB="$HERE/prev-c-library.sh"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/    /' "$T/log"; fail=1; }

TAR=tstrans-c-0.0.1-linux-x86_64.tar.gz
mkdir -p "$T/bin" "$T/src/tstrans-c-0.0.1-linux-x86_64"
echo abi > "$T/src/tstrans-c-0.0.1-linux-x86_64/tstrans.abi"
tar -czf "$T/$TAR" -C "$T/src" tstrans-c-0.0.1-linux-x86_64
GOOD="$(sha256sum "$T/$TAR" | cut -d' ' -f1)"
BADSUM=0000000000000000000000000000000000000000000000000000000000000000

# The stub serves the files in $T/assets: `release download -p <glob>` copies
# the matches into -D and fails when none match; `attestation verify` logs its
# arguments and exits with the code in $T/attest-rc.
cat > "$T/bin/gh" <<STUB
#!/usr/bin/env bash
echo "gh \$*" >> "$T/gh.log"
if [ "\$1 \$2" = "release download" ]; then
  shift 2; pats=(); dir=.
  while [ \$# -gt 0 ]; do case "\$1" in -p) pats+=("\$2"); shift 2 ;; -D) dir="\$2"; shift 2 ;; -R) shift 2 ;; *) shift ;; esac; done
  n=0
  for p in "\${pats[@]}"; do for f in "$T"/assets/\$p; do [ -f "\$f" ] && cp "\$f" "\$dir/" && n=\$((n + 1)); done; done
  [ \$n -gt 0 ] || { echo "no assets match the file pattern" >&2; exit 1; }
  exit 0
fi
if [ "\$1 \$2" = "attestation verify" ]; then exit "\$(cat "$T/attest-rc")"; fi
echo "unexpected gh call: \$*" >&2; exit 3
STUB
chmod +x "$T/bin/gh"

# A second package of another version, and a package whose tarball holds no tstrans.abi.
TAR2=tstrans-c-0.0.2-linux-x86_64.tar.gz
cp "$T/$TAR" "$T/$TAR2"
mkdir -p "$T/noabi-src/tstrans-c-0.0.1-linux-x86_64" "$T/noabi"
echo h > "$T/noabi-src/tstrans-c-0.0.1-linux-x86_64/tstrans.h"
tar -czf "$T/noabi/$TAR" -C "$T/noabi-src" tstrans-c-0.0.1-linux-x86_64
NOABI="$(sha256sum "$T/noabi/$TAR" | cut -d' ' -f1)"

# setup <sums-line or "-" for no SHA256SUMS> <attest rc> [tarballs: one|none|two|noabi]
setup() {
  rm -rf "$T/assets" "$T/rt" "$T/out" "$T/gh.log"; mkdir -p "$T/assets" "$T/rt"
  case "${3:-one}" in
    one) cp "$T/$TAR" "$T/assets/" ;;
    two) cp "$T/$TAR" "$T/$TAR2" "$T/assets/" ;;
    noabi) cp "$T/noabi/$TAR" "$T/assets/" ;;
  esac
  if [ "$1" != - ]; then printf '%s\n' "aaaa  qualification.json" "$1" > "$T/assets/SHA256SUMS"; fi
  echo "$2" > "$T/attest-rc"
}
run() {
  env PATH="$T/bin:$PATH" GITHUB_OUTPUT="$T/out" RUNNER_TEMP="$T/rt" PREV=v0.0.1 GITHUB_REPOSITORY=x/y \
    bash "$PREVLIB" release > "$T/log" 2>&1
}
has_lib() { [ -f "$T/out" ] && grep -q '^lib=' "$T/out"; }
# fell <case> <detail line> <fallback reason>: no lib=, the detail and the fallback line
# in the log, and exactly that reason in the step's `fallback` output.
FELL='the v0.0.1 Release package failed verification; the source build is used'
fell() {
  if has_lib; then no "$1: still yields lib="; else ok "$1: no lib="; fi
  grep -qxF "$FELL" "$T/log" && grep -qxF "$2" "$T/log" && ok "$1: names the file and the check" || no "$1: message"
  [ "$(sed -n 's/^fallback=//p' "$T/out" 2>/dev/null)" = "$3" ] && ok "$1: fallback=$3" || { no "$1: fallback output"; sed 's/^/    out: /' "$T/out" 2>/dev/null; }
}

setup "$BADSUM  $TAR" 0; run
fell "a wrong digest" "$TAR: sha256sum -c against SHA256SUMS failed" "checksum mismatch"

setup "$GOOD  $TAR" 0; run
if has_lib && [ "$(sed -n 's/^lib=//p' "$T/out")" = "$T/rt/prev-release/tstrans-c-0.0.1-linux-x86_64/tstrans.abi" ]; then ok "a verified package yields lib="; else no "a verified package yields no lib="; fi
grep -q '^fallback=' "$T/out" && no "a verified package wrote a fallback reason" || ok "a verified package writes no fallback reason"
grep -qxF "gh attestation verify $T/rt/prev-release/$TAR -R x/y --signer-workflow aklofas/ts-transformer/.github/workflows/release-qualify.yml --source-ref refs/tags/v0.0.1" "$T/gh.log" \
  && ok "the attestation must come from release-qualify.yml at the previous tag" || { no "attestation call"; sed 's/^/    /' "$T/gh.log"; }

setup "$GOOD  $TAR" 1; run
fell "a failed attestation" "$TAR: gh attestation verify failed" "attestation verify failed"

setup "bbbb  tstrans-py.cdx.json" 0; run
fell "an unlisted tarball" "$TAR: no line for it in SHA256SUMS" "package not listed in SHA256SUMS"

setup - 0; run
fell "a missing SHA256SUMS" "SHA256SUMS: not in the v0.0.1 Release" "SHA256SUMS missing"

# Two packages are refused, not picked between: neither is known to be the tag's.
setup "$GOOD  $TAR" 0 two; run
fell "two packages" "the v0.0.1 Release holds 2 C packages, expected one" "more than one package"

setup "$NOABI  $TAR" 0 noabi; run
fell "a package without tstrans.abi" "$TAR: holds no tstrans.abi" "no tstrans.abi in package"

setup - 0 none; run
if has_lib; then no "a Release without a C package yields lib="; else ok "a Release without a C package yields no lib="; fi
grep -qxF "the v0.0.1 Release carries no C package" "$T/log" && ok "a Release without a C package says so" || no "no-package message"
[ "$(sed -n 's/^fallback=//p' "$T/out" 2>/dev/null)" = "no package for v0.0.1" ] && ok "no package: fallback=no package for v0.0.1" || no "no-package fallback output"

# `source` folds how the previous library was obtained into one string for the ABI record.
src() { env PREV=v0.0.1 "$@" bash "$PREVLIB" source 2>"$T/log"; }
[ "$(src RELEASE_LIB=/x/tstrans.abi FALLBACK=)" = attested ] && ok "source: a verified package is attested" || no "source: attested"
[ "$(src RELEASE_LIB= FALLBACK='attestation verify failed')" = "built: attestation verify failed" ] && ok "source: a fallback carries its reason" || no "source: built with reason"
src RELEASE_LIB= FALLBACK= >/dev/null; rc=$?
[ $rc = 2 ] && grep -qxF "prev-c-library.sh source: neither a verified package nor a fallback reason" "$T/log" && ok "source: no lib and no reason is refused" || no "source: refusal rc=$rc"

[ $fail = 0 ] && echo "test_prev_c_library: OK"
exit $fail
