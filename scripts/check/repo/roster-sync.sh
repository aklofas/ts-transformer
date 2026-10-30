#!/usr/bin/env bash
# The lists that must stay literal (workflow YAML, and the version rail, which
# runs in jobs with no Rust toolchain) agree with scripts/lib/roster.sh:
#   1. every member has a sanitizer class; sanitizers.yml's `-p` lists match;
#   2. crates-io.yml publishes every publishable crate once, after its deps
#      (a layer step publishes in argument order);
#   3. release-version-consistency.sh's RVC_DEP_TOMLS == roster dep-tomls;
#   4. ci.yml's build matrix is the five gating legs, none continue-on-error
#      (the job's continue-on-error stays the matrix value, never literal true);
#      the test jobs give every platform all three feature passes between them,
#      test-main-extra runs only off pull requests, none continue-on-error;
#   5. every member manifest inherits the lint table;
#   6. ci.yml's deny job checks every fuzz workspace (crates/*/fuzz), and each
#      manifest-path it names exists in this tree.
# RSYNC_FILES_ROOT points 1-4 and 6 at copies of those files (self_test.sh).
# Linux/bash 3.2-portable: no mapfile, no sed -i; runs on the linux-x86_64 leg.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
F="${RSYNC_FILES_ROOT:-$ROOT}"; R="$ROOT/scripts/lib/roster.sh"; fail=0
bad() { echo "FAIL: roster-sync: $*"; fail=1; }

# One `cargo metadata` call for the whole run; every roster call reads it.
if [ -z "${ROSTER_METADATA:-}" ]; then
  ROSTER_METADATA="$(mktemp "${TMPDIR:-/tmp}/roster-sync-meta.XXXXXX")"
  trap 'rm -f "$ROSTER_METADATA"' EXIT
  (cd "$ROOT" && cargo metadata --no-deps --format-version 1) > "$ROSTER_METADATA"
fi
export ROSTER_METADATA

bash "$R" check-sanitize || fail=1
p_lists() { # <job> : sorted -p packages of that sanitizers.yml job
  awk -v j="  $1:" '$0 == j {f = 1; next} /^  [a-z-]+:$/ {f = 0} f' "$F/.github/workflows/sanitizers.yml" \
    | grep -oE -- '-p [a-z0-9-]+' | awk '{print $2}' | sort -u
}
for job in asan:pure tsan:pure asan-native:native tsan-native:native; do
  [ "$(p_lists "${job%%:*}")" = "$(bash "$R" "sanitize-${job#*:}" | sort)" ] || bad "sanitizers.yml job ${job%%:*} -p list != roster sanitize-${job#*:}"
done

layers="$(grep -oE 'publish-crates-layer\.sh( [a-z0-9-]+)+' "$F/.github/workflows/crates-io.yml" | cut -d' ' -f2-)"
[ "$(printf '%s\n' $layers | sort)" = "$(bash "$R" publishable | sort)" ] || bad "crates-io.yml layers != roster publishable (each once)"
# A layer step publishes its crates in argument order, so the flat word order
# is the publish order: every internal dependency must come earlier in it.
order="$(printf '%s\n' $layers)"
pos() { printf '%s\n' "$order" | grep -nxF -- "$1" | head -1 | cut -d: -f1; }
while read -r pkg deps; do
  lp="$(pos "$pkg")"
  for d in $deps; do
    ld="$(pos "$d")"
    [ -n "$lp" ] && [ -n "$ld" ] && [ "$ld" -lt "$lp" ] || bad "crates-io.yml publishes $pkg (position ${lp:-?}) before or with its dependency $d (position ${ld:-?})"
  done
done < <(bash "$R" publish-deps)

rvc="$(sed -nE 's/^RVC_DEP_TOMLS="\$\{RVC_DEP_TOMLS:-(.*)\}"$/\1/p' "$F/scripts/check/repo/release-version-consistency.sh" | tr ' ' '\n' | sed "s|^\$ROOT|$ROOT|" | sort)"
[ "$rvc" = "$(bash "$R" dep-tomls | sort)" ] || bad "release-version-consistency.sh RVC_DEP_TOMLS != roster dep-tomls"

CI="$F/.github/workflows/ci.yml"
job_text() { # <job> : the text of that ci.yml job, up to the next top-level job
  awk -v j="  $1:" '$0 == j {f = 1; next} /^  [a-z-]+:$/ {f = 0} f' "$CI"
}
build_job="$(job_text build)"
legs="$(awk '/- name: /{n = $3} /continue: false/ {print n}' <<<"$build_job" | sort | tr '\n' ' ')"
[ "$legs" = "linux-aarch64 linux-x86_64 macos-arm64 windows-msvc windows-msvc-features " ] || bad "ci.yml build legs with continue: false are [$legs], want the five gating legs"
grep -qF 'continue-on-error: ${{ matrix.continue }}' <<<"$build_job" || bad "ci.yml build job no longer takes continue-on-error from the matrix"
if grep -qE 'continue-on-error:[[:space:]]*true' <<<"$build_job"; then bad "ci.yml build job has continue-on-error: true"; fi

# The three test jobs together run all three feature passes on every platform:
# the union is read from include entries only (`- name:` then a bare `pass:`
# value), never from the matrix axis lines.
test_jobs="test-linux test-platforms test-main-extra"
for j in $test_jobs; do
  [ -n "$(job_text "$j")" ] || bad "ci.yml has no $j job"
  if grep -qE 'continue-on-error:[[:space:]]*true' <<<"$(job_text "$j")"; then bad "ci.yml $j job has continue-on-error: true"; fi
done
all_tests="$(for j in $test_jobs; do job_text "$j"; done)"
for platform in linux-x86_64 linux-aarch64 macos-arm64 windows-msvc; do
  got="$(awk -v p="$platform" '/- name: /{n = $3} /^[[:space:]]+pass:[[:space:]]+[a-z-]+[[:space:]]*$/ {if (n == p) print $2}' <<<"$all_tests" | sort -u | tr '\n' ' ')"
  [ "$got" = "all-features default no-default " ] || bad "ci.yml test jobs give platform $platform the passes [$got], want all-features default no-default"
done
grep -qF "if: github.event_name != 'pull_request'" <<<"$(job_text test-main-extra)" || bad "ci.yml test-main-extra no longer runs only off pull requests"

while IFS= read -r toml; do
  awk '/^\[lints\]/ {f = 1; next} /^\[/ {f = 0} f && /^workspace[[:space:]]*=[[:space:]]*true/ {ok = 1} END {exit !ok}' "$toml" || bad "$toml does not inherit [workspace.lints]"
done < <(bash "$R" manifests)

deny_paths="$(awk '/^  deny:$/ {f = 1; next} /^  [a-z-]+:$/ {f = 0} f' "$F/.github/workflows/ci.yml" \
  | sed -nE 's/^[[:space:]]*manifest-path:[[:space:]]*([^[:space:]]+).*/\1/p' | sort)"
for m in $(cd "$F" && find crates -path '*/fuzz/Cargo.toml' | sort); do
  printf '%s\n' "$deny_paths" | grep -qxF -- "$m" || bad "ci.yml deny job does not check the fuzz workspace $m"
done
for m in $deny_paths; do
  [ -f "$F/$m" ] || bad "ci.yml deny job names a manifest-path that does not exist: $m"
done

[ "$fail" -eq 0 ] && echo "roster-sync: OK"
exit "$fail"
