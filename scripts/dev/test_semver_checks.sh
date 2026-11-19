#!/usr/bin/env bash
# Self-test for the part of scripts/dev/semver-checks.sh the qualification
# record depends on before any crate is analysed: the previous release tag is
# written to <out-dir>/semver-baseline.json first, so a run that then fails
# (here: no cargo-semver-checks, a stub `cargo` on PATH) still names the tag
# its report would have compared with, and a checkout without release tags
# fails without writing one. A last case runs every crate against a stub tool
# and checks that it is told to print without colour.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/    /' "$T/log"; fail=1; }

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
R="$T/repo"
mkdir -p "$R/scripts/dev" "$T/bin"
cp "$HERE/semver-checks.sh" "$HERE/prev-tag.sh" "$R/scripts/dev/"
g() { git -C "$R" -c user.name=t -c user.email=t@example.com -c commit.gpgsign=false "$@"; }
git -C "$R" init -q
commit() { echo "$1" > "$R/f"; g add -A; g commit -q -m "$1"; }
commit one

# cargo-semver-checks is not installed: `cargo +stable semver-checks --version` fails.
printf '#!/usr/bin/env bash\nexit 1\n' > "$T/bin/cargo"
chmod +x "$T/bin/cargo"
run() { PATH="$T/bin:$PATH" bash "$R/scripts/dev/semver-checks.sh" "$T/out" > "$T/log" 2>&1; rc=$?; }
tag_in() { python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["previous_tag"])' "$T/out/semver-baseline.json" 2>/dev/null; }

run
if [ "$rc" -eq 2 ] && [ ! -e "$T/out/semver-baseline.json" ] && grep -q 'git fetch --tags' "$T/log"; then
  ok "no release tag: exit 2, no baseline file, the remedy is named"
else no "no release tag: exit 2, no baseline file, the remedy is named (rc=$rc)"; fi

g tag v1.2.2; commit two; g tag v1.2.3-rc1; commit three
run
if [ "$rc" -eq 2 ] && [ "$(tag_in)" = v1.2.2 ] && [ ! -e "$T/out/semver-checks.json" ] && grep -q 'cargo-semver-checks' "$T/log"; then
  ok "the tool is missing: exit 2, yet the baseline file names v1.2.2 and there is no report"
else no "the tool is missing: exit 2, yet the baseline file names v1.2.2 and there is no report (rc=$rc, tag=$(tag_in))"; fi

# A failed run leaves neither file of an earlier one in the same directory.
echo '[]' > "$T/out/semver-checks.json"
g tag -d v1.2.2 > /dev/null
run
if [ "$rc" -eq 2 ] && [ ! -e "$T/out/semver-baseline.json" ] && [ ! -e "$T/out/semver-checks.json" ]; then
  ok "a run without a release tag removes the files of the run before it"
else no "a run without a release tag removes the files of the run before it (rc=$rc)"; fi
g tag v1.2.2 HEAD~2

# The tag on HEAD is the release being qualified, never its own baseline.
g tag v1.2.3
run
if [ "$rc" -eq 2 ] && [ "$(tag_in)" = v1.2.2 ]; then ok "a tag on HEAD is not the baseline"
else no "a tag on HEAD is not the baseline (rc=$rc, tag=$(tag_in))"; fi
commit four
run
if [ "$rc" -eq 2 ] && [ "$(tag_in)" = v1.2.3 ]; then ok "one commit later that tag is the baseline"
else no "one commit later that tag is the baseline (rc=$rc, tag=$(tag_in))"; fi

# A full run against a stub tool: the release qualification sets CARGO_TERM_COLOR=always for
# every job, and a styled "Failed in:" line is one the report parser cannot read, so the
# tool must be told to print plain text whatever the environment says.
mkdir -p "$R/scripts/lib" "$R/scripts/gen"
cp "$HERE/../gen/api_churn.py" "$R/scripts/gen/"
cat > "$R/scripts/lib/roster.sh" <<'ROSTER'
#!/usr/bin/env bash
case "$1" in
  public-api) for i in 1 2 3 4 5 6 7 8 9 10 11; do echo "crate-$i:crate-$i"; done ;;
  public-api-features) ;;
esac
ROSTER
for sm in crates/srt-sys/vendor/srt crates/mbedtls-src/vendor/mbedtls crates/rist-sys/vendor/librist; do
  mkdir -p "$R/$sm"; echo x > "$R/$sm/x"
done
commit five; g tag v1.2.4; commit six
cat > "$T/bin/cargo" <<'CARGO'
#!/usr/bin/env bash
case "$*" in *--version*) exit 0 ;; esac
echo "CARGO_TERM_COLOR=${CARGO_TERM_COLOR:-} $*" >> "$CALLS"
printf -- '--- failure function_missing: pub fn removed ---\n\nFailed in:\n  function crate::f, previously in file /b/crates/x/src/lib.rs:3\n\n'
echo "     Summary semver requires new major version: 1 major and 0 minor checks failed"
echo "     Checked [   0.001s] 9 checks: 8 pass, 1 fail, 0 warn, 0 skip"
exit 100
CARGO
chmod +x "$T/bin/cargo"
export CALLS="$T/calls.log"
: > "$CALLS"
CARGO_TERM_COLOR=always run
plain="$(grep -c 'CARGO_TERM_COLOR=never .*check-release.* --color never' "$CALLS")"
breaks="$(python3 -c 'import json, sys; print(sum(len(c["breaks"]) for c in json.load(open(sys.argv[1]))))' "$T/out/semver-checks.json" 2>/dev/null)"
if [ "$rc" -eq 0 ] && [ "$plain" -eq 11 ] && [ "$breaks" = 11 ]; then
  ok "the tool runs without colour under CARGO_TERM_COLOR=always, and every break line is parsed"
else no "the tool runs without colour under CARGO_TERM_COLOR=always, and every break line is parsed (rc=$rc, plain calls=$plain, breaks=$breaks)"; sed 's/^/    /' "$CALLS"; fi

[ "$fail" -eq 0 ] && echo "test_semver_checks: ok"
exit "$fail"
