#!/usr/bin/env bash
# Self-test for release-vm-corpus.sh's check that the binary it runs is the
# one it just built: with Cargo's output directory redirected, the phase
# must stop before any stage runs the binary at the fixed path.
#
# git, cargo and aws are stubs in a throwaway HOME's ~/.cargo/bin, which the
# script puts first on PATH; the "binary" is a stub that succeeds.
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
[[ "\$1" == rev-parse ]] && echo $SHA
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

# corpus <cargo's target directory> -> the phase's output, its status in $rc
corpus() {
  rc=0
  out=$(env -u TSTRANS_REPO_DIR -u TSTRANS_CORPUS_DIR -u TSTRANS_CORPUS_BUCKET \
    HOME="$WORK/home" STUB_TARGET_DIR="$1" bash "$SCRIPT" "$SHA" --no-baseline 2>&1) || rc=$?
}

corpus "$WORK/home/ts-transformer/target"
if [[ $rc -ne 0 || "$out" != *"target-dir rc=0"* || "$out" != *"sweep rc=0"* || "$out" != *"CORPUS-DONE rc=0"* ]]; then
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

[[ $fail -eq 0 ]] && echo "release-vm-corpus-selftest: ok"
exit "$fail"
