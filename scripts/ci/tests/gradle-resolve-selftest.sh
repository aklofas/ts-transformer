#!/usr/bin/env bash
# Self-test of scripts/ci/gradle-resolve.sh against a stub Gradle wrapper
# (named by GRADLEW) and stub `sleep` and `timeout` on PATH. The stub wrapper
# logs its working directory and arguments and fails as many times as
# $TMP/gradle-fail says before it succeeds.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/gradle-resolve.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail() { echo "FAIL: $*"; echo "--- out"; cat "$TMP/out" 2>/dev/null || true; echo "--- gradle.log"; cat "$TMP/gradle.log" 2>/dev/null || true; exit 1; }

mkdir "$TMP/bin" "$TMP/tbin"
cat > "$TMP/gradlew" <<'STUB'
#!/usr/bin/env bash
printf '%s|%s\n' "$PWD" "$*" >> "$TMP/gradle.log"
n="$(cat "$TMP/gradle-fail")"
if [ "$n" -gt 0 ]; then echo $((n - 1)) > "$TMP/gradle-fail"; echo "Could not GET junit-jupiter-5.10.2.pom: 429" >&2; exit 1; fi
STUB
cat > "$TMP/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf 'sleep %s\n' "$*" >> "$TMP/sleep.log"
STUB
# A GNU timeout that logs its bound and runs the command (used by case 5 only).
cat > "$TMP/tbin/timeout" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = --version ]; then echo "timeout (GNU coreutils) 9.4"; exit 0; fi
printf '%s\n' "$*" >> "$TMP/timeout.log"
if [ "$1" = -k ]; then shift 2; fi
shift
exec "$@"
STUB
chmod +x "$TMP/gradlew" "$TMP/bin/sleep" "$TMP/tbin/timeout"
export TMP GRADLEW="$TMP/gradlew"

# setup <failures>: fresh logs.
setup() {
  : > "$TMP/gradle.log"; : > "$TMP/sleep.log"; : > "$TMP/out"; : > "$TMP/timeout.log"
  echo "$1" > "$TMP/gradle-fail"
}
run() { set +e; PATH="$TMP/bin:$PATH" bash "$SCRIPT" "$@" > "$TMP/out" 2>&1; RC=$?; set -e; }
calls() { wc -l < "$TMP/gradle.log" | tr -d ' '; }

# 1: the resolution fails once, then succeeds after a 60 s wait
setup 1
run
[ "$RC" = 0 ] || fail "case 1 rc=$RC"
[ "$(calls)" = 2 ] || fail "case 1 calls=$(calls)"
[ "$(cat "$TMP/sleep.log")" = "sleep 60" ] || fail "case 1 wait"
[ "$(grep -c '::notice::' "$TMP/out")" = 1 ] || fail "case 1 notices"
grep -qx '::notice::Gradle attempt 1 of 3 failed with status 1' "$TMP/out" || fail "case 1 notice text"

# 2: every attempt fails: three calls, waits of 60 s and 180 s, exit 1
setup 9
run
[ "$RC" = 1 ] || fail "case 2 rc=$RC"
[ "$(calls)" = 3 ] || fail "case 2 calls=$(calls)"
[ "$(tr '\n' ' ' < "$TMP/sleep.log")" = "sleep 60 sleep 180 " ] || fail "case 2 waits: $(cat "$TMP/sleep.log")"
[ "$(grep -c '::notice::' "$TMP/out")" = 3 ] || fail "case 2 notices"
grep -q 'failed 3 times' "$TMP/out" || fail "case 2 final message"

# 3: runs from bindings/jvm with the init script and resolveAll; extra arguments pass through
setup 0
run -PjniTestHooks=true
[ "$RC" = 0 ] || fail "case 3 rc=$RC"
[ "$(calls)" = 1 ] || fail "case 3 calls=$(calls)"
line="$(cat "$TMP/gradle.log")"
[ "${line%%|*}" = "$ROOT/bindings/jvm" ] || fail "case 3 working directory: ${line%%|*}"
[ "${line#*|}" = "--no-daemon --init-script ../../scripts/ci/gradle-resolve-all.init.gradle resolveAll -PjniTestHooks=true" ] || fail "case 3 arguments: ${line#*|}"
[ ! -s "$TMP/sleep.log" ] || fail "case 3 waited"
[ -f "$ROOT/bindings/jvm/../../scripts/ci/gradle-resolve-all.init.gradle" ] || fail "case 3 the init script is missing"

# 4: without GRADLEW the script runs ./gradlew of bindings/jvm (a copy of the
# script in a stub tree, whose bindings/jvm/gradlew is the stub wrapper)
setup 0
mkdir -p "$TMP/tree/scripts/ci" "$TMP/tree/bindings/jvm"
cp "$SCRIPT" "$TMP/tree/scripts/ci/"
cp "$TMP/gradlew" "$TMP/tree/bindings/jvm/gradlew"
set +e; (unset GRADLEW; PATH="$TMP/bin:$PATH" bash "$TMP/tree/scripts/ci/gradle-resolve.sh") > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 4 rc=$RC"
[ "$(cut -d'|' -f1 "$TMP/gradle.log")" = "$TMP/tree/bindings/jvm" ] || fail "case 4 the default wrapper did not run"

# 5: each attempt runs under a three-minute wall-clock bound when GNU timeout is there
setup 0
set +e; PATH="$TMP/tbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 5 rc=$RC"
case "$(head -n 1 "$TMP/timeout.log")" in
  "-k 10 180 $TMP/gradlew --no-daemon "*) ;;
  *) fail "case 5 bound: $(cat "$TMP/timeout.log")" ;;
esac

# 6: every retry repeats the first call's arguments exactly
setup 9
run -PjniTestHooks=true
[ "$RC" = 1 ] || fail "case 6 rc=$RC"
[ "$(calls)" = 3 ] || fail "case 6 calls=$(calls)"
args() { sed -n "${1}p" "$TMP/gradle.log" | cut -d'|' -f2; }
for i in 1 2 3; do
  [ "$(args "$i")" = "--no-daemon --init-script ../../scripts/ci/gradle-resolve-all.init.gradle resolveAll -PjniTestHooks=true" ] \
    || fail "case 6 call $i: $(args "$i")"
done
grep -qx '::notice::Gradle attempt 3 of 3 failed with status 1' "$TMP/out" || fail "case 6 last notice"

# 7: --no-test becomes the property the init script reads; the other
# arguments still pass through, and --no-test itself never reaches Gradle
setup 0
run --no-test -PnativeStagingDir=x
[ "$RC" = 0 ] || fail "case 7 rc=$RC"
[ "$(args 1)" = "--no-daemon --init-script ../../scripts/ci/gradle-resolve-all.init.gradle resolveAll -PresolveAllNoTest=true -PnativeStagingDir=x" ] || fail "case 7 arguments: $(args 1)"

echo "PASS: 7 cases"
