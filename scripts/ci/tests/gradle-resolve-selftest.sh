#!/usr/bin/env bash
# Self-test of scripts/ci/gradle-resolve.sh against a stub Gradle wrapper
# (named by GRADLEW) and stub `sleep`, `timeout` and `curl` on PATH. The stub
# wrapper logs its working directory and arguments and fails as many times as
# $TMP/gradle-fail says before it succeeds, printing $TMP/gradle-block (a
# "Searched in" block) when it fails, and exiting with $TMP/gradle-rc.
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
if [ "$n" -gt 0 ]; then
  echo $((n - 1)) > "$TMP/gradle-fail"
  echo "Could not GET junit-jupiter-5.10.2.pom: 429" >&2
  [ ! -f "$TMP/gradle-block" ] || cat "$TMP/gradle-block"
  exit "$(cat "$TMP/gradle-rc" 2>/dev/null || echo 1)"
fi
STUB
# A curl that logs its arguments and answers HEAD requests from $TMP/curl-reply.
mkdir "$TMP/cbin"
cat > "$TMP/cbin/curl" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/curl.log"
case "$*" in *"-sS -I --max-time 15 "*) ;; *) echo "stub curl: unexpected arguments: $*" >&2; exit 2 ;; esac
case "$*" in *unreachable.example.invalid*) echo "curl: (6) Could not resolve host: unreachable.example.invalid" >&2; exit 6 ;; esac
cat "$TMP/curl-reply"
STUB
chmod +x "$TMP/cbin/curl"
# A PATH without curl: every command of /usr/bin and /bin but curl.
mkdir "$TMP/nocurl"
for d in /usr/bin /bin; do
  for f in "$d"/*; do
    b="${f##*/}"
    if [ "$b" != curl ] && [ ! -e "$TMP/nocurl/$b" ]; then ln -s "$f" "$TMP/nocurl/$b"; fi
  done
done
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
  : > "$TMP/gradle.log"; : > "$TMP/sleep.log"; : > "$TMP/out"; : > "$TMP/timeout.log"; : > "$TMP/curl.log"
  rm -f "$TMP/gradle-block" "$TMP/gradle-rc"
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

# The probe after a failed attempt.
POM=https://repo.example.invalid/maven2/org/junit/jupiter/junit-jupiter/5.10.2/junit-jupiter-5.10.2.pom
block() { # <url...>: the block Gradle prints for a module it could not find
  { echo "   > Could not find org.junit.jupiter:junit-jupiter:5.10.2."
    echo "     Searched in the following locations:"
    for u in "$@"; do echo "       - $u"; done
    echo "     Required by:"
    echo "         root project 'tstrans-jvm'"; } > "$TMP/gradle-block"
}
printf 'HTTP/2 404\r\nserver: cloudflare\r\ncf-cache-status: HIT\r\nage: 3353\r\ncf-ray: 8f00example-SJC\r\ncontent-type: text/plain\r\n\r\n' > "$TMP/curl-reply"

# 8: one failed attempt: the probe names the URL, the status and the headers, in a group
setup 1
block "$POM"
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 8 rc=$RC"
[ "$(wc -l < "$TMP/curl.log" | tr -d ' ')" = 1 ] || fail "case 8 curl calls: $(cat "$TMP/curl.log")"
grep -q "^-sS -I --max-time 15 $POM\$" "$TMP/curl.log" || fail "case 8 curl arguments: $(cat "$TMP/curl.log")"
grep -qx "probe: $POM -> 404 age=3353 cf-cache-status=HIT cf-ray=8f00example-SJC server=cloudflare" "$TMP/out" || fail "case 8 probe line"
grep -qx '::group::gradle-resolve: what the repository answered (attempt 1)' "$TMP/out" || fail "case 8 group"
grep -qx '::endgroup::' "$TMP/out" || fail "case 8 endgroup"
! LC_ALL=C grep -q '[^ -~]' "$TMP/out" || fail "case 8 non-ASCII output"

# 9: duplicates count once, at most three URLs, and a URL with userinfo is
# neither requested nor printed
setup 1
block "$POM" "$POM" "https://user:secret@private.example.invalid/m2/a.pom" https://a.example.invalid/1.pom https://b.example.invalid/2.pom https://c.example.invalid/3.pom
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 9 rc=$RC"
[ "$(wc -l < "$TMP/curl.log" | tr -d ' ')" = 3 ] || fail "case 9 curl calls: $(cat "$TMP/curl.log")"
! grep -q 'secret\|private.example.invalid' "$TMP/curl.log" || fail "case 9 the URL with userinfo was requested"
! grep '^probe' "$TMP/out" | grep -q 'secret\|private.example.invalid' || fail "case 9 the URL with userinfo was printed"
grep -q "a.example.invalid/1.pom" "$TMP/curl.log" || fail "case 9 the second URL was not probed"
grep -q "b.example.invalid/2.pom" "$TMP/curl.log" || fail "case 9 the third URL was not probed"
! grep -q 'c.example.invalid' "$TMP/curl.log" || fail "case 9 more than three URLs probed"

# 10: a failing request is one line and changes nothing
setup 1
block https://unreachable.example.invalid/x.pom
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 10 rc=$RC"
grep -qx 'probe: https://unreachable.example.invalid/x.pom -> no answer (curl exit 6)' "$TMP/out" || fail "case 10 line"

# 11: no curl on PATH: one note, and the run ends as without the probe
setup 9
block "$POM"
set +e; PATH="$TMP/bin:$TMP/nocurl" "$TMP/nocurl/bash" "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 1 ] || fail "case 11 rc=$RC"
[ "$(calls)" = 3 ] || fail "case 11 calls=$(calls)"
[ "$(grep -c '^probe: curl is not installed' "$TMP/out")" = 3 ] || fail "case 11 note"
[ "$(tr '\n' ' ' < "$TMP/sleep.log")" = "sleep 60 sleep 180 " ] || fail "case 11 waits"

# 12: no URL in the output: one note
setup 1
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 12 rc=$RC"
grep -qx 'probe: no URL under "Searched in the following locations:" or "Could not GET" in this attempt'"'"'s output' "$TMP/out" || fail "case 12 note"
[ ! -s "$TMP/curl.log" ] || fail "case 12 curl was called"

# 13: a passing attempt runs no probe
setup 0
block "$POM"
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 13 rc=$RC"
[ ! -s "$TMP/curl.log" ] || fail "case 13 curl was called"
! grep -q '^probe\|::group::' "$TMP/out" || fail "case 13 probed"

# 14: Gradle's own status decides, through the output pipe: the stand-in exits
# 3 while the pipe succeeds; the notices say 3 and the run fails after three
setup 9
echo 3 > "$TMP/gradle-rc"
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 1 ] || fail "case 14 rc=$RC"
[ "$(grep -c '^::notice::Gradle attempt [123] of 3 failed with status 3$' "$TMP/out")" = 3 ] || fail "case 14 notices"
grep -q '429' "$TMP/out" || fail "case 14 Gradle's output was not shown"

# 15: a header value cannot write a workflow command: the runner reads `##[`
# anywhere in a line, so the probe breaks it; `::` cannot start a probe line
setup 1
block "$POM"
printf 'HTTP/2 404\r\nserver: x ##[error]boom\r\nx-cache: ##[add-mask]v\r\n\r\n' > "$TMP/curl-reply"
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 15 rc=$RC"
! grep '^probe' "$TMP/out" | grep -qF '##[' || fail "case 15 a probe line carries ##["
grep -qF 'x-cache=# #[add-mask]v server=x # #[error]boom' "$TMP/out" || fail "case 15 the broken-up values"

# 16: a non-ASCII byte and an ESC in a header value are not printed
setup 1
block "$POM"
printf 'HTTP/2 404\r\nserver: a\033[31mb\r\nx-cache: HIT\303\251\r\n\r\n' > "$TMP/curl-reply"
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 16 rc=$RC"
! grep '^probe' "$TMP/out" | LC_ALL=C grep -q '[^ -~]' || fail "case 16 a probe line carries a byte outside printable ASCII"
grep -qx "probe: $POM -> 404 x-cache=HIT server=a\[31mb" "$TMP/out" || fail "case 16 line"

# 17: a refused GET has no "Searched in" block: its URL is probed too, with
# the same userinfo skip
setup 1
printf 'HTTP/2 429\r\nretry-after: 30\r\n\r\n' > "$TMP/curl-reply"
{ echo "   > Could not resolve org.junit.jupiter:junit-jupiter:5.10.2."
  echo "      > Could not get resource '$POM'."
  echo "         > Could not GET '$POM'. Received status code 429 from server: Too Many Requests"
  echo "         > Could not HEAD 'https://u:p@private.example.invalid/x.pom'. Received status code 403 from server: Forbidden"; } > "$TMP/gradle-block"
set +e; PATH="$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 0 ] || fail "case 17 rc=$RC"
[ "$(wc -l < "$TMP/curl.log" | tr -d ' ')" = 1 ] || fail "case 17 curl calls: $(cat "$TMP/curl.log")"
grep -qx "probe: $POM -> 429 retry-after=30" "$TMP/out" || fail "case 17 line"
! grep -q 'private.example.invalid' "$TMP/curl.log" || fail "case 17 the URL with userinfo was requested"

# 18: a probe that fails outright changes nothing: an awk that fails on the
# attempt's output leaves the status, the attempts and the waits as before
setup 9
block "$POM"
mkdir -p "$TMP/abin"
realawk="$(command -v awk)"
cat > "$TMP/abin/awk" <<STUB
#!/usr/bin/env bash
for a in "\$@"; do [ -f "\$a" ] && grep -q 'Searched in' "\$a" && exit 2; done
exec "$realawk" "\$@"
STUB
chmod +x "$TMP/abin/awk"
set +e; PATH="$TMP/abin:$TMP/cbin:$TMP/bin:$PATH" bash "$SCRIPT" > "$TMP/out" 2>&1; RC=$?; set -e
[ "$RC" = 1 ] || fail "case 18 rc=$RC"
[ "$(calls)" = 3 ] || fail "case 18 calls=$(calls)"
[ "$(tr '\n' ' ' < "$TMP/sleep.log")" = "sleep 60 sleep 180 " ] || fail "case 18 waits"

echo "PASS: 18 cases"
