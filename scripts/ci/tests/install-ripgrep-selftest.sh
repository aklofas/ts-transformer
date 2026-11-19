#!/usr/bin/env bash
# Self-test of scripts/ci/install-ripgrep.sh against stub `gh`, `uname` and
# `sleep` on PATH. The stub gh copies a prebuilt archive named after the
# requested asset; $TMP/gh-fail holds how many calls fail first.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/install-ripgrep.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*"; echo "--- out"; cat "$TMP/out" 2>/dev/null || true; echo "--- gh.log"; cat "$TMP/gh.log" 2>/dev/null || true; exit 1; }

sha() { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

# Prebuilt archives: a tarball with a stub rg, and a zip with a stub rg.exe.
mkdir -p "$TMP/src/ripgrep-15.2.0-x86_64-unknown-linux-musl" "$TMP/src/ripgrep-15.2.0-x86_64-pc-windows-msvc" "$TMP/assets"
printf '#!/bin/sh\necho "ripgrep 15.2.0 (stub)"\n' > "$TMP/src/ripgrep-15.2.0-x86_64-unknown-linux-musl/rg"
chmod +x "$TMP/src/ripgrep-15.2.0-x86_64-unknown-linux-musl/rg"
printf 'stub' > "$TMP/src/ripgrep-15.2.0-x86_64-pc-windows-msvc/rg.exe"
tar -czf "$TMP/assets/ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz" -C "$TMP/src" ripgrep-15.2.0-x86_64-unknown-linux-musl
python3 -I -c "import sys, zipfile; z = zipfile.ZipFile(sys.argv[1], 'w'); z.write(sys.argv[2], 'ripgrep-15.2.0-x86_64-pc-windows-msvc/rg.exe')" \
  "$TMP/assets/ripgrep-15.2.0-x86_64-pc-windows-msvc.zip" "$TMP/src/ripgrep-15.2.0-x86_64-pc-windows-msvc/rg.exe"
LINUX_SHA="$(sha "$TMP/assets/ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz")"
WIN_SHA="$(sha "$TMP/assets/ripgrep-15.2.0-x86_64-pc-windows-msvc.zip")"

mkdir "$TMP/bin"
cat > "$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/gh.log"
n="$(cat "$TMP/gh-fail")"
if [ "$n" -gt 0 ]; then echo $((n - 1)) > "$TMP/gh-fail"; echo "release not found" >&2; exit 1; fi
asset=""; dir=""
while [ $# -gt 0 ]; do
  case "$1" in -p) asset="$2"; shift 2 ;; -D) dir="$2"; shift 2 ;; *) shift ;; esac
done
cp "$TMP/assets/$asset" "$dir/$asset"
STUB
cat > "$TMP/bin/uname" <<'STUB'
#!/usr/bin/env bash
case "$1" in -s) echo "$FAKE_S" ;; -m) echo "$FAKE_M" ;; *) echo "$FAKE_S" ;; esac
STUB
cat > "$TMP/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf 'sleep %s\n' "$*" >> "$TMP/sleep.log"
STUB
chmod +x "$TMP/bin/gh" "$TMP/bin/uname" "$TMP/bin/sleep"
export TMP PATH="$TMP/bin:$PATH" GH_TOKEN=stub CI_INSTALL_WAITS="20 60"

# setup <failures>: fresh logs, fresh RUNNER_TEMP and path file.
setup() {
  : > "$TMP/gh.log"; : > "$TMP/sleep.log"; : > "$TMP/out"
  echo "$1" > "$TMP/gh-fail"
  rm -rf "$TMP/rt"; mkdir "$TMP/rt"
  : > "$TMP/path"
  export RUNNER_TEMP="$TMP/rt" GITHUB_PATH="$TMP/path" FAKE_S=Linux FAKE_M=x86_64
  unset RIPGREP_SHA256_OVERRIDE || true
}
run() { set +e; bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e; }

# 1: the download fails once, then succeeds
setup 1
RIPGREP_SHA256_OVERRIDE="$LINUX_SHA" run
[ "$RC" = 0 ] || fail "case 1 rc=$RC"
[ "$(wc -l < "$TMP/gh.log" | tr -d ' ')" = 2 ] || fail "case 1 download attempts"
grep -q 'release download 15.2.0 -R BurntSushi/ripgrep' "$TMP/gh.log" || fail "case 1 gh arguments"
grep -q 'ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz' "$TMP/gh.log" || fail "case 1 asset name"
[ "$(cat "$TMP/path")" = "$TMP/rt/ripgrep/ripgrep-15.2.0-x86_64-unknown-linux-musl" ] || fail "case 1 path line: $(cat "$TMP/path")"
grep -q 'ripgrep 15.2.0' "$TMP/out" || fail "case 1 version line"
[ "$(cat "$TMP/sleep.log")" = "sleep 20" ] || fail "case 1 wait"

# 2: a wrong checksum fails, names the asset, adds nothing to the path file
setup 0
RIPGREP_SHA256_OVERRIDE="0000000000000000000000000000000000000000000000000000000000000000" run
[ "$RC" = 1 ] || fail "case 2 rc=$RC"
grep -q 'ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz' "$TMP/out" || fail "case 2 message lacks the asset"
[ ! -s "$TMP/path" ] || fail "case 2 path file written"

# 3: an unknown machine fails with the platform in the message
setup 0
FAKE_M=riscv64 run
[ "$RC" = 1 ] || fail "case 3 rc=$RC"
grep -q 'Linux/riscv64' "$TMP/out" || fail "case 3 message lacks the platform"
[ ! -s "$TMP/gh.log" ] || fail "case 3 downloaded"

# 4: GITHUB_PATH unset is not an error
setup 0
unset GITHUB_PATH
RIPGREP_SHA256_OVERRIDE="$LINUX_SHA" run
[ "$RC" = 0 ] || fail "case 4 rc=$RC"

# 5: no token fails with a message that says so
setup 0
unset GH_TOKEN
run
[ "$RC" = 1 ] || fail "case 5 rc=$RC"
grep -q 'GH_TOKEN' "$TMP/out" || fail "case 5 message"
export GH_TOKEN=stub

# 6: every download fails: three attempts, exit 1
setup 9
run
[ "$RC" = 1 ] || fail "case 6 rc=$RC"
[ "$(wc -l < "$TMP/gh.log" | tr -d ' ')" = 3 ] || fail "case 6 attempts"

# 7: the Windows leg takes the zip and unpacks it without unzip
setup 0
FAKE_S=MINGW64_NT-10.0-26100 FAKE_M=x86_64 RIPGREP_SHA256_OVERRIDE="$WIN_SHA" run
[ "$RC" = 0 ] || fail "case 7 rc=$RC"
grep -q 'ripgrep-15.2.0-x86_64-pc-windows-msvc.zip' "$TMP/gh.log" || fail "case 7 asset"
[ -f "$TMP/rt/ripgrep/ripgrep-15.2.0-x86_64-pc-windows-msvc/rg.exe" ] || fail "case 7 not unpacked"

echo "PASS: 7 cases"
