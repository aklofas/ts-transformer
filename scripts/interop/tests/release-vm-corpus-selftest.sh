#!/usr/bin/env bash
# Self-test for release-vm-corpus.sh's check that the binary it runs is the
# one it just built: with Cargo's output directory redirected, the phase
# must stop before any stage runs the binary at the fixed path. Also for its
# version stage: an unswept checkout ends the phase before the build.
#
# git, cargo and aws are stubs in a throwaway HOME's ~/.cargo/bin, which the
# script puts first on PATH; the "binary" is a stub that succeeds. The stub
# git shows one release tag, v0.1.0, and the checkout declares STUB_VERSION.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../release-vm-corpus.sh
SHA=0123456789abcdef0123456789abcdef01234567
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
fail=0

mkdir -p "$WORK/home/.cargo/bin" "$WORK/home/ts-transformer/target/release" "$WORK/elsewhere"
cat >"$WORK/home/.cargo/bin/git" <<STUB
#!/usr/bin/env bash
dir=\$PWD
[[ "\$1" == -C ]] && { dir=\$2; shift 2; }
case "\$*" in
  "rev-parse --show-toplevel") cd "\$dir" && pwd -P ;;
  rev-parse*) echo $SHA ;;
  "tag --list"*) echo v0.1.0 ;;
esac
exit 0
STUB
cat >"$WORK/home/.cargo/bin/cargo" <<'STUB'
#!/usr/bin/env bash
[[ "$1" == metadata ]] && printf '{"target_directory": "%s"}\n' "$STUB_TARGET_DIR"
exit 0
STUB
printf '#!/usr/bin/env bash\nexit 0\n' >"$WORK/home/.cargo/bin/aws"
printf '#!/usr/bin/env bash\nexit 0\n' >"$WORK/home/ts-transformer/target/release/tst-interop"
chmod +x "$WORK/home/.cargo/bin/"* "$WORK/home/ts-transformer/target/release/tst-interop"

# corpus <cargo's target directory> [<workspace version> [<env assignment>]]
#   -> the phase's output, its status in $rc
corpus() {
  rc=0
  printf '[workspace.package]\nversion = "%s"\n' "${2:-0.2.0}" >"$WORK/home/ts-transformer/Cargo.toml"
  out=$(env -u TSTRANS_REPO_DIR -u TSTRANS_CORPUS_DIR -u TSTRANS_CORPUS_BUCKET -u TSTRANS_NOT_A_RELEASE \
    HOME="$WORK/home" STUB_TARGET_DIR="$1" ${3:+"$3"} bash "$SCRIPT" "$SHA" --no-baseline 2>&1) || rc=$?
}

corpus "$WORK/home/ts-transformer/target"
if [[ $rc -ne 0 || "$out" != *"version rc=0"* || "$out" != *"target-dir rc=0"* || "$out" != *"sweep rc=0"* ||
  "$out" != *"CORPUS-DONE rc=0"* ]]; then
  echo "FAIL: the default target directory must pass the check and run the stages (rc=$rc):"
  echo "$out"
  fail=1
fi

corpus "$WORK/elsewhere"
if [[ $rc -eq 0 || "$out" != *"target-dir rc=1"* || "$out" != *"cargo builds into $WORK/elsewhere"* ||
  "$out" == *"inventory rc="* || "$out" != *"CORPUS-DONE rc=1"* ]]; then
  echo "FAIL: a redirected target directory must end the phase before the inventory (rc=$rc):"
  echo "$out"
  fail=1
fi

# The corpus run is release evidence too: a checkout whose version sweep has
# not happened ends the phase before the build.
corpus "$WORK/home/ts-transformer/target" 0.1.0
if [[ $rc -ne 1 || "$out" != *"newest release tag v0.1.0"* || "$out" != *"version rc=1"* ||
  "$out" == *"build rc="* || "$out" != *"CORPUS-DONE rc=1"* ]]; then
  echo "FAIL: an unswept checkout must end the phase at the version stage (rc=$rc):"
  echo "$out"
  fail=1
fi
corpus "$WORK/home/ts-transformer/target" 0.1.0 TSTRANS_NOT_A_RELEASE=1
if [[ $rc -ne 0 || "$out" != *"skipped (TSTRANS_NOT_A_RELEASE=1)"* || "$out" != *"version rc=0"* ||
  "$out" != *"CORPUS-DONE rc=0"* ]]; then
  echo "FAIL: TSTRANS_NOT_A_RELEASE=1 must skip the version stage and say so (rc=$rc):"
  echo "$out"
  fail=1
fi
dry=$(env -u TSTRANS_NOT_A_RELEASE HOME="$WORK/home" bash "$SCRIPT" "$SHA" --no-baseline --dry-run 2>&1)
if [[ "$dry" != *"git fetch --tags origin"* || "$dry" != *"version: bash "*"release-version-check.sh"* ]]; then
  echo "FAIL: the dry run must name the tag fetch and the version stage:"
  echo "$dry"
  fail=1
fi

[[ $fail -eq 0 ]] && echo "release-vm-corpus-selftest: ok"
exit "$fail"
