#!/usr/bin/env bash
# Fetches everything the JVM build downloads (the Gradle distribution, the
# plugins, and every resolvable configuration through the resolveAll task of
# gradle-resolve-all.init.gradle) in a retried call before the build, so a
# fetch Maven Central fails once (a 429, or "not found" for a file it has) is
# retried instead of failing the build minutes later. The later Gradle calls
# of the job find it all in the Gradle user home. Any failure of that call is
# retried, a broken build file included: about two and a half minutes more.
#
# Gradle caches a module's absence in a repository, so an attempt after the
# first adds --refresh-dependencies: without it a "not found" is answered from
# that cache and never asked again.
#
# Usage: gradle-resolve.sh [--no-test] [gradle arguments...] (added to the
# resolveAll call). --no-test leaves out the configurations only the tests use
# (a job that runs no test, such as the jar assembly and the publish).
# Runs from bindings/jvm. Environment: GRADLEW (default ./gradlew, the wrapper
# to call). Three attempts, 20 s and 60 s apart, each bounded to 180 s when GNU
# timeout is available.
set -euo pipefail

here="$(CDPATH='' cd -- "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
props=()
if [ "${1:-}" = --no-test ]; then props=(-PresolveAllNoTest=true); shift; fi
cd "$here/../../bindings/jvm"
waits="20 60"
# A wall-clock bound for one call, when GNU timeout is available (not on
# macOS; Windows' own timeout.exe is a different tool).
bound=()
if timeout --version 2>/dev/null | grep -q GNU; then bound=(timeout -k 10 180); fi

attempt=1
refresh=()
while :; do
  # The init script path is relative: Gradle on Windows gets no MSYS path.
  if ${bound[@]+"${bound[@]}"} "${GRADLEW:-./gradlew}" --no-daemon --init-script ../../scripts/ci/gradle-resolve-all.init.gradle resolveAll \
      ${refresh[@]+"${refresh[@]}"} ${props[@]+"${props[@]}"} "$@"; then
    exit 0
  else
    rc=$?
  fi
  if [ "$attempt" -lt 3 ]; then
    echo "::notice::Gradle attempt $attempt of 3 failed with status $rc; the next asks the repositories again (--refresh-dependencies)"
  else
    echo "::notice::Gradle attempt $attempt of 3 failed with status $rc"
    echo "gradle-resolve.sh: dependency resolution failed 3 times" >&2
    exit 1
  fi
  refresh=(--refresh-dependencies)
  w="$(echo "$waits" | awk -v i="$attempt" '{print $i}')"
  [ -z "$w" ] || sleep "$w"
  attempt=$((attempt + 1))
done
