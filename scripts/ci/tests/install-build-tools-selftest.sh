#!/usr/bin/env bash
# Self-test of scripts/ci/install-build-tools.sh against stub `python3` (makes
# a venv holding stub pip, meson, ninja, pkg-config) and `sleep`. The stub pip
# fails as many times as $TMP/pip-fail says before it succeeds.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/install-build-tools.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*"; echo "--- out"; cat "$TMP/out" 2>/dev/null || true; echo "--- pip.log"; cat "$TMP/pip.log" 2>/dev/null || true; exit 1; }

mkdir "$TMP/bin" "$TMP/tools"
cat > "$TMP/bin/python3" <<'STUB'
#!/usr/bin/env bash
# Only `python3 -m venv DIR` is used by the script.
[ "$1 $2" = "-m venv" ] || { echo "stub python3: unexpected $*" >&2; exit 2; }
d="$3"; mkdir -p "$d/bin"
cat > "$d/bin/pip" <<'PIP'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/pip.log"
n="$(cat "$TMP/pip-fail")"
if [ "$n" -gt 0 ]; then echo $((n - 1)) > "$TMP/pip-fail"; echo "network error" >&2; exit 1; fi
PIP
printf '#!/bin/sh\necho 1.12.1\n' > "$d/bin/meson"
printf '#!/bin/sh\necho 1.13.0\n' > "$d/bin/ninja"
printf '#!/bin/sh\necho 2.5.1\n' > "$d/bin/pkg-config"
chmod +x "$d/bin/pip" "$d/bin/meson" "$d/bin/ninja" "$d/bin/pkg-config"
STUB
cat > "$TMP/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf 'sleep %s\n' "$*" >> "$TMP/sleep.log"
STUB
for t in ninja cmake pkg-config; do printf '#!/bin/sh\nexit 0\n' > "$TMP/tools/$t"; done
chmod +x "$TMP/bin/python3" "$TMP/bin/sleep" "$TMP/tools/"*
export TMP

# setup <pip failures>: fresh logs and RUNNER_TEMP.
setup() {
  : > "$TMP/pip.log"; : > "$TMP/sleep.log"; : > "$TMP/out"
  echo "$1" > "$TMP/pip-fail"
  rm -rf "$TMP/rt"; mkdir "$TMP/rt"
  : > "$TMP/path"
  export RUNNER_TEMP="$TMP/rt" GITHUB_PATH="$TMP/path"
}
# run <PATH>: the stubs first, then the given tool directory.
run() { set +e; PATH="$TMP/bin:$1" "$BASH" "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e; }
BASH_DIR="$(dirname "$(command -v bash)")"

# 1: pip fails once and is retried; the install is hashed
setup 1
run "$TMP/tools:$BASH_DIR:/usr/bin:/bin"
[ "$RC" = 0 ] || fail "case 1 rc=$RC"
[ "$(wc -l < "$TMP/pip.log" | tr -d ' ')" = 2 ] || fail "case 1 pip attempts"
grep -q -- 'install --require-hashes -r .*scripts/ci/requirements-build-tools.txt' "$TMP/pip.log" || fail "case 1 pip arguments"
[ "$(cat "$TMP/sleep.log")" = "sleep 20" ] || fail "case 1 wait"
grep -q '1.12.1' "$TMP/out" || fail "case 1 meson version not printed"

# 2: only meson is linked into the bin directory, which goes on the path file
[ "$(ls "$TMP/rt/build-tools-bin")" = "meson" ] || fail "case 2 bin dir holds: $(ls "$TMP/rt/build-tools-bin")"
[ "$(cat "$TMP/path")" = "$TMP/rt/build-tools-bin" ] || fail "case 2 path line"

# 3: a missing ninja fails and names it
setup 0
rm "$TMP/tools/ninja"
run "$TMP/tools"
[ "$RC" != 0 ] || fail "case 3 passed without ninja"
grep -q 'ninja' "$TMP/out" || fail "case 3 message lacks the tool"
[ ! -s "$TMP/pip.log" ] || fail "case 3 installed before the check"

# 4: GITHUB_PATH unset is not an error
setup 0
printf '#!/bin/sh\nexit 0\n' > "$TMP/tools/ninja"; chmod +x "$TMP/tools/ninja"
unset GITHUB_PATH
run "$TMP/tools:$BASH_DIR:/usr/bin:/bin"
[ "$RC" = 0 ] || fail "case 4 rc=$RC"

# 5: pip fails every time: three attempts, failure
setup 9
run "$TMP/tools:$BASH_DIR:/usr/bin:/bin"
[ "$RC" != 0 ] || fail "case 5 passed"
[ "$(wc -l < "$TMP/pip.log" | tr -d ' ')" = 3 ] || fail "case 5 attempts"

# 6: each pip attempt runs under a two-minute wall-clock bound (stub GNU timeout)
setup 0
mkdir -p "$TMP/tbin"
cat > "$TMP/tbin/timeout" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then echo "timeout (GNU coreutils) 9.4"; exit 0; fi
printf '%s\n' "$*" >> "$TMP/timeout.log"
if [ "$1" = -k ]; then shift 2; fi
shift
exec "$@"
STUB
chmod +x "$TMP/tbin/timeout"
: > "$TMP/timeout.log"
run "$TMP/tbin:$TMP/tools:$BASH_DIR:/usr/bin:/bin"
[ "$RC" = 0 ] || fail "case 6 rc=$RC"
case "$(head -n 1 "$TMP/timeout.log")" in
  "-k 10 120 "*/pip\ install*) ;;
  *) fail "case 6 bound: $(cat "$TMP/timeout.log")" ;;
esac

echo "PASS: 6 cases"
