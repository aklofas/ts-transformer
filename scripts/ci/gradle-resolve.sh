#!/usr/bin/env bash
# Fetches everything the JVM build downloads (the Gradle distribution, the
# plugins, and every resolvable configuration through the resolveAll task of
# gradle-resolve-all.init.gradle) in a retried call before the build, so a
# fetch that fails once is retried instead of failing the build minutes
# later, and a failure shows before the build starts. The later Gradle calls
# of the job find it all in the Gradle user home. Any failure of that call is
# retried, a broken build file included: about four minutes more.
# Maven Central has answered "not found" for an existing file through all
# three attempts twice: about 90 s with waits of 20 s and 60 s, about five
# minutes with the present 60 s and 180 s. The retry has outlasted neither; it
# is kept for the failures it can outlast (a reset, a 5xx, a timeout). After
# each failed attempt the probe below prints what the repository answered.
#
# Usage: gradle-resolve.sh [--no-test] [gradle arguments...] (added to the
# resolveAll call). --no-test leaves out the configurations only the tests use
# (a job that runs no test, such as the jar assembly and the publish).
# Runs from bindings/jvm. Environment: GRADLEW (default ./gradlew, the wrapper
# to call). Three attempts, 60 s and 180 s apart, each bounded to 180 s when GNU
# timeout is available, plus a probe of at most 3 x 15 s after each failed
# attempt: at most 3 x (190 + 45) + 240 s, about 16 minutes.
set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
props=()
if [ "${1:-}" = --no-test ]; then props=(-PresolveAllNoTest=true); shift; fi
cd "$here/../../bindings/jvm"
waits="60 180"
# A wall-clock bound for one call, when GNU timeout is available (not on
# macOS; Windows' own timeout.exe is a different tool).
bound=()
if timeout --version 2>/dev/null | grep -q GNU; then bound=(timeout -k 10 180); fi

# After a failed attempt: what the repository answers, now, for the first
# three URLs Gradle searched (the status and the cache and proxy headers). It
# prints and never changes the exit status or the retry. A URL that carries
# userinfo is neither requested nor printed.
probe() { # <the attempt's output>
  local urls u st hdr line h v n=0
  urls="$(awk '/Searched in the following locations:/ {f = 1; next}
    f && /^[[:space:]]*- https?:\/\// {sub(/^[[:space:]]*- /, ""); sub(/[[:space:]]+$/, ""); print; next}
    {f = 0}' "$1" | awk '!seen[$0]++')"
  if [ -z "$urls" ]; then
    echo 'probe: no URL under "Searched in the following locations:" in this attempt'"'"'s output'
    return 0
  fi
  if ! command -v curl >/dev/null 2>&1; then
    echo "probe: curl is not installed; nothing requested"
    return 0
  fi
  while IFS= read -r u; do
    case "${u#*://}" in */*) h="${u#*://}"; h="${h%%/*}" ;; *) h="${u#*://}" ;; esac
    case "$h" in *@*) echo "probe: a URL with credentials was not requested"; continue ;; esac
    [ "$n" -lt 3 ] || break
    n=$((n + 1))
    st=0
    hdr="$(curl -sS -I --max-time 15 "$u" 2>&1)" || st=$?
    if [ "$st" -ne 0 ]; then
      echo "probe: $u -> no answer (curl exit $st)"
      continue
    fi
    hdr="$(printf '%s\n' "$hdr" | tr -d '\r')"
    line="probe: $u -> $(printf '%s\n' "$hdr" | awk 'NR == 1 {print $2}')"
    for h in age cf-cache-status cf-ray retry-after x-cache x-served-by server; do
      v="$(printf '%s\n' "$hdr" | awk -v k="$h" 'tolower($0) ~ "^" k ":" {sub(/^[^:]*:[[:space:]]*/, ""); print; exit}')"
      [ -z "$v" ] || line="$line $h=$v"
    done
    printf '%s\n' "$line" | LC_ALL=C tr -cd '\11\12\40-\176'
  done <<<"$urls"
}

out="$(mktemp)"
trap 'rm -f "$out"' EXIT
attempt=1
while :; do
  # The init script path is relative: Gradle on Windows gets no MSYS path.
  # The output is shown and kept for the probe; Gradle's status decides, not tee's.
  set +e
  ${bound[@]+"${bound[@]}"} "${GRADLEW:-./gradlew}" --no-daemon --init-script ../../scripts/ci/gradle-resolve-all.init.gradle resolveAll \
      ${props[@]+"${props[@]}"} "$@" 2>&1 | tee "$out"
  rc=${PIPESTATUS[0]}
  set -e
  [ "$rc" -ne 0 ] || exit 0
  echo "::notice::Gradle attempt $attempt of 3 failed with status $rc"
  echo "::group::gradle-resolve: what the repository answered (attempt $attempt)"
  (probe "$out") || echo "probe: failed; ignored"
  echo "::endgroup::"
  [ "$attempt" -lt 3 ] || { echo "gradle-resolve.sh: dependency resolution failed 3 times" >&2; exit 1; }
  w="$(echo "$waits" | awk -v i="$attempt" '{print $i}')"
  [ -z "$w" ] || sleep "$w"
  attempt=$((attempt + 1))
done
