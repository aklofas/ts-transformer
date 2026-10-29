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

# case <name> <sums-line or "-" for no SHA256SUMS> <attest rc> [tarball: yes|no]
setup() {
  rm -rf "$T/assets" "$T/rt" "$T/out" "$T/gh.log"; mkdir -p "$T/assets" "$T/rt"
  [ "${4:-yes}" = yes ] && cp "$T/$TAR" "$T/assets/"
  if [ "$2" != - ]; then printf '%s\n' "aaaa  qualification.json" "$2" > "$T/assets/SHA256SUMS"; fi
  echo "$3" > "$T/attest-rc"
}
run() {
  env PATH="$T/bin:$PATH" GITHUB_OUTPUT="$T/out" RUNNER_TEMP="$T/rt" PREV=v0.0.1 GITHUB_REPOSITORY=x/y \
    bash "$PREVLIB" release > "$T/log" 2>&1
}
has_lib() { [ -f "$T/out" ] && grep -q '^lib=' "$T/out"; }
FELL='the v0.0.1 Release package failed verification; the source build is used'

setup wrong "$BADSUM  $TAR" 0; run
if has_lib; then no "a wrong digest still yields lib="; else ok "a wrong digest yields no lib="; fi
grep -qxF "$FELL" "$T/log" && grep -qF "$TAR: sha256sum -c against SHA256SUMS failed" "$T/log" && ok "a wrong digest names the file and the check" || no "wrong-digest message"

setup good "$GOOD  $TAR" 0; run
if has_lib && [ "$(sed -n 's/^lib=//p' "$T/out")" = "$T/rt/prev-release/tstrans-c-0.0.1-linux-x86_64/tstrans.abi" ]; then ok "a verified package yields lib="; else no "a verified package yields no lib="; fi
grep -qxF "gh attestation verify $T/rt/prev-release/$TAR -R x/y" "$T/gh.log" && ok "the attestation is verified against this repository" || { no "attestation call"; sed 's/^/    /' "$T/gh.log"; }

setup attest "$GOOD  $TAR" 1; run
if has_lib; then no "a failed attestation still yields lib="; else ok "a failed attestation yields no lib="; fi
grep -qxF "$FELL" "$T/log" && grep -qF "$TAR: gh attestation verify failed" "$T/log" && ok "a failed attestation names the file and the check" || no "attestation message"

setup unlisted "bbbb  tstrans-py.cdx.json" 0; run
if has_lib; then no "a SHA256SUMS without the tarball line still yields lib="; else ok "a SHA256SUMS without the tarball line yields no lib="; fi
grep -qxF "$FELL" "$T/log" && grep -qF "$TAR: no line for it in SHA256SUMS" "$T/log" && ok "an unlisted tarball names the file and the check" || no "unlisted message"

setup nosums - 0; run
if has_lib; then no "a Release without SHA256SUMS still yields lib="; else ok "a Release without SHA256SUMS yields no lib="; fi
grep -qxF "$FELL" "$T/log" && grep -qF "SHA256SUMS: not in the v0.0.1 Release" "$T/log" && ok "a missing SHA256SUMS names the file" || no "missing-sums message"

setup nopkg - 0 no; run
if has_lib; then no "a Release without a C package yields lib="; else ok "a Release without a C package yields no lib="; fi
grep -qxF "the v0.0.1 Release carries no C package" "$T/log" && ok "a Release without a C package says so" || no "no-package message"

[ $fail = 0 ] && echo "test_prev_c_library: OK"
exit $fail
