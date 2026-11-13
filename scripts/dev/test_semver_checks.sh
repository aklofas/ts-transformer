#!/usr/bin/env bash
# Self-test for the part of scripts/dev/semver-checks.sh the qualification
# record depends on before any crate is analysed: the previous release tag is
# written to <out-dir>/semver-baseline.json first, so a run that then fails
# (here: no cargo-semver-checks, a stub `cargo` on PATH) still names the tag
# its report would have compared with, and a checkout without release tags
# fails without writing one.
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
run() { rm -rf "$T/out"; PATH="$T/bin:$PATH" bash "$R/scripts/dev/semver-checks.sh" "$T/out" > "$T/log" 2>&1; rc=$?; }
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

# The tag on HEAD is the release being qualified, never its own baseline.
g tag v1.2.3
run
if [ "$rc" -eq 2 ] && [ "$(tag_in)" = v1.2.2 ]; then ok "a tag on HEAD is not the baseline"
else no "a tag on HEAD is not the baseline (rc=$rc, tag=$(tag_in))"; fi
commit four
run
if [ "$rc" -eq 2 ] && [ "$(tag_in)" = v1.2.3 ]; then ok "one commit later that tag is the baseline"
else no "one commit later that tag is the baseline (rc=$rc, tag=$(tag_in))"; fi

[ "$fail" -eq 0 ] && echo "test_semver_checks: ok"
exit "$fail"
