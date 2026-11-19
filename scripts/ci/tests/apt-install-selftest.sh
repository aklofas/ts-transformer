#!/usr/bin/env bash
# Self-test of scripts/ci/apt-install.sh against stub `apt-get` and `sleep` on
# PATH. The stub apt-get logs its arguments and takes its behaviour per call
# from $TMP/plan (one line per call: an exit status, `hang`, or `slow` = take
# 3 s and succeed); an empty plan means success.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/ci/apt-install.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

mkdir "$TMP/bin"
cat > "$TMP/bin/apt-get" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/apt.log"
# The mirrors file as it stood at each call, for the shortening checks.
if [ -f "$APT_INSTALL_MIRRORS_FILE" ]; then
  printf 'mirrors=%s\n' "$(wc -l < "$APT_INSTALL_MIRRORS_FILE" | tr -d ' ')" >> "$TMP/apt.log"
fi
step=""
for a in "$@"; do case "$a" in update|install) step="$a"; break ;; esac; done
b=0
if [ -s "$TMP/plan" ]; then
  b="$(head -n 1 "$TMP/plan")"
  tail -n +2 "$TMP/plan" > "$TMP/plan.next"; mv "$TMP/plan.next" "$TMP/plan"
fi
case "$b" in
  hang) exec /bin/sleep 30 ;;
  slow) /bin/sleep 3; exit 0 ;;
  *) exit "$b" ;;
esac
STUB
cat > "$TMP/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf 'sleep %s\n' "$*" >> "$TMP/sleep.log"
STUB
cat > "$TMP/bin/dpkg" <<'STUB'
#!/usr/bin/env bash
printf 'dpkg %s\n' "$*" >> "$TMP/apt.log"
STUB
# A sudo that records how it was called, then runs the rest as given.
cat > "$TMP/bin/sudo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/sudo.log"
exec "$@"
STUB
chmod +x "$TMP/bin/apt-get" "$TMP/bin/sleep" "$TMP/bin/dpkg" "$TMP/bin/sudo"
export TMP PATH="$TMP/bin:$PATH"
export APT_INSTALL_SUDO=""
export APT_INSTALL_WAITS="20 60"
export APT_INSTALL_MIRRORS_FILE="$TMP/mirrors.txt"
export APT_INSTALL_SOURCES="$TMP/sources"

fail() { echo "FAIL: $*"; echo "--- apt.log"; cat "$TMP/apt.log"; echo "--- out"; cat "$TMP/out" 2>/dev/null || true; exit 1; }

# setup <plan lines...>: fresh logs, a three-line mirrors file, the plan.
setup() {
  : > "$TMP/apt.log"; : > "$TMP/sleep.log"; : > "$TMP/out"; : > "$TMP/sudo.log"
  mkdir -p "$TMP/sources"
  printf 'URIs: mirror+file:%s\n' "$TMP/mirrors.txt" > "$TMP/sources/ubuntu.sources"
  printf 'http://a.example.invalid/ubuntu/\tpriority:1\nhttps://b.example.invalid/ubuntu/\tpriority:2\nhttps://c.example.invalid/ubuntu/\tpriority:3\n' > "$TMP/mirrors.txt"
  : > "$TMP/plan"
  for l in "$@"; do printf '%s\n' "$l" >> "$TMP/plan"; done
}
# run <args...>: sets RC.
run() { set +e; bash "$SCRIPT" "$@" > "$TMP/out" 2>&1; RC=$?; set -e; }
count() { grep -c -- "$1" "$TMP/apt.log" || true; }

# 1: first try
setup
run pkg-a
[ "$RC" = 0 ] || fail "case 1 rc=$RC"
[ "$(count ' update')" = 1 ] || fail "case 1 update count"
[ "$(count 'download-only')" = 1 ] || fail "case 1 download count"
[ "$(grep -c ' install ' "$TMP/apt.log")" = 2 ] || fail "case 1 install count"
[ "$(wc -l < "$TMP/mirrors.txt" | tr -d ' ')" = 3 ] || fail "case 1 mirrors shortened"
! [ -s "$TMP/sleep.log" ] || fail "case 1 slept"

# 2: update fails twice; first mirror dropped once, before attempt 2 only
setup 100 100
run pkg-a
[ "$RC" = 0 ] || fail "case 2 rc=$RC"
[ "$(count ' update')" = 3 ] || fail "case 2 attempts"
[ "$(wc -l < "$TMP/mirrors.txt" | tr -d ' ')" = 2 ] || fail "case 2 mirrors not shortened exactly once"
! grep -q 'a.example.invalid' "$TMP/mirrors.txt" || fail "case 2 wrong line dropped"
grep -q 'a.example.invalid' "$TMP/out" || fail "case 2 dropped host not printed"
[ "$(grep -c '^mirrors=3' "$TMP/apt.log")" = 1 ] || fail "case 2 attempt 1 saw three lines"
[ "$(grep -c '^mirrors=2' "$TMP/apt.log")" = 4 ] || fail "case 2 attempts 2,3 saw two lines"
[ "$(cat "$TMP/sleep.log")" = "sleep 20
sleep 60" ] || fail "case 2 waits"

# 3: every attempt fails
setup 7 8 9
run pkg-a
[ "$RC" = 9 ] || fail "case 3 rc=$RC (want the last status 9)"
[ "$(count ' update')" = 3 ] || fail "case 3 attempts"
[ "$(grep -c '^::notice::' "$TMP/out")" = 3 ] || fail "case 3 notices"
grep -q '^::notice::.*attempt 1 of 3: update failed with status 7' "$TMP/out" || fail "case 3 notice 1"
grep -q '^::notice::.*attempt 3 of 3: update failed with status 9' "$TMP/out" || fail "case 3 notice 3"
setup 5 6
APT_INSTALL_ATTEMPTS=2 run pkg-a
[ "$RC" = 6 ] || fail "case 3b rc"
[ "$(count ' update')" = 2 ] || fail "case 3b attempts override"

# 4: a hung update is cut by its own bound, the next attempt succeeds
setup hang
START=$(date +%s)
APT_INSTALL_UPDATE_TIMEOUT=1 run pkg-a
[ "$RC" = 0 ] || fail "case 4 rc=$RC"
[ $(( $(date +%s) - START )) -lt 15 ] || fail "case 4 took too long"
[ "$(count ' update')" = 2 ] || fail "case 4 attempts"
grep -q '^::notice::.*attempt 1 of 3: update failed with status 124' "$TMP/out" || fail "case 4 notice"
# a hung download is cut by the download bound, and the kill after 10 s is not what ended it
setup 0 hang
APT_INSTALL_TIMEOUT=1 run pkg-a
[ "$RC" = 0 ] || fail "case 4 download rc=$RC"
grep -q '^::notice::.*attempt 1 of 3: download failed with status 124' "$TMP/out" || fail "case 4 download notice"
# attempt 1 alone, under 8 s: SIGTERM must end it (the -k kill comes at 11 s)
setup hang
START=$(date +%s)
APT_INSTALL_UPDATE_TIMEOUT=1 APT_INSTALL_ATTEMPTS=1 run pkg-a
[ "$RC" = 124 ] || fail "case 4b rc=$RC (want 124)"
[ $(( $(date +%s) - START )) -lt 8 ] || fail "case 4b took $(( $(date +%s) - START )) s: SIGTERM did nothing"

# 5: the unpack is not cut by the short bound
setup 0 0 slow
START=$(date +%s)
APT_INSTALL_TIMEOUT=1 APT_INSTALL_UNPACK_TIMEOUT=30 run pkg-a
[ "$RC" = 0 ] || fail "case 5 rc=$RC"
[ "$(count ' update')" = 1 ] || fail "case 5 retried"
[ $(( $(date +%s) - START )) -ge 3 ] || fail "case 5 stub did not run its 3 s"

# 6: no mirrors file; a failing first attempt is retried quietly
setup 100
export APT_INSTALL_MIRRORS_FILE="$TMP/missing.txt"
run pkg-a
[ "$RC" = 0 ] || fail "case 6 rc=$RC"
[ "$(count ' update')" = 2 ] || fail "case 6 attempts"
! grep -qi 'missing.txt\|No such file' "$TMP/out" || fail "case 6 complained about the file"
export APT_INSTALL_MIRRORS_FILE="$TMP/mirrors.txt"

# 7: a one-line mirrors file is never emptied
setup 100 100
printf 'http://a.example.invalid/ubuntu/\tpriority:1\n' > "$TMP/mirrors.txt"
run pkg-a
[ "$(wc -l < "$TMP/mirrors.txt" | tr -d ' ')" = 1 ] || fail "case 7 emptied"

# 8: arguments pass through; --no-recommends is opt-in
setup
run abigail-tools=2.4-3build1 binutils -f
grep -q ' install .*abigail-tools=2.4-3build1 binutils -f$' "$TMP/apt.log" || fail "case 8 arguments"
! grep -q 'no-install-recommends' "$TMP/apt.log" || fail "case 8 recommends flag without opt-in"
setup
run --no-recommends -- pkg-a pkg-b
[ "$(grep -c ' install .*--no-install-recommends.* pkg-a pkg-b$' "$TMP/apt.log")" = 2 ] || fail "case 8 --no-recommends on both installs"
setup
run --timeout 99 pkg-a
[ "$RC" = 0 ] || fail "case 8 --timeout rc=$RC"

# 9: options on every call; -y on install
setup
run pkg-a
OPTS='-o Acquire::Retries=1 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20 -o DPkg::Lock::Timeout=60'
[ "$(grep -vc '^mirrors=' "$TMP/apt.log")" = "$(grep -c -- "$OPTS" "$TMP/apt.log")" ] || fail "case 9 options missing on a call"
[ "$(grep -c ' install -y ' "$TMP/apt.log")" = 2 ] || fail "case 9 -y"

# 10: sudo wraps timeout, never the other way round
setup 100 0 0 0 0
APT_INSTALL_SUDO=sudo run pkg-a
[ "$RC" = 0 ] || fail "case 10 rc=$RC"
[ -s "$TMP/sudo.log" ] || fail "case 10 sudo never called"
[ "$(grep -vc '^timeout -k 10 \|^sed -i ' "$TMP/sudo.log")" = 0 ] || fail "case 10 sudo not outermost: $(cat "$TMP/sudo.log")"

# 11: a list whose first line is no longer priority:1 (an earlier call dropped
# one) is not shortened by a later call
setup 100
printf 'https://b.example.invalid/ubuntu/\tpriority:2\nhttps://c.example.invalid/ubuntu/\tpriority:3\n' > "$TMP/mirrors.txt"
run pkg-a
[ "$(wc -l < "$TMP/mirrors.txt" | tr -d ' ')" = 2 ] || fail "case 11 shortened a priority:2 list"

# 12: a failed unpack finishes dpkg before the next attempt
setup 0 0 1
run pkg-a
[ "$RC" = 0 ] || fail "case 12 rc=$RC"
grep -q '^dpkg --configure -a' "$TMP/apt.log" || fail "case 12 dpkg not run"
grep -q 'attempt 1 of 3: unpack failed with status 1' "$TMP/out" || fail "case 12 notice"
grep -q 'unpack was interrupted' "$TMP/out" || fail "case 12 interrupted notice"
setup 1
run pkg-a
! grep -q '^dpkg' "$TMP/apt.log" || fail "case 12 dpkg run after an update failure"

# 13: a bad attempt count is refused, not a silent success
for v in 0 abc 1x; do
  setup
  APT_INSTALL_ATTEMPTS="$v" run pkg-a
  [ "$RC" = 2 ] || fail "case 13 rc=$RC for '$v'"
  [ ! -s "$TMP/apt.log" ] || fail "case 13 ran apt for '$v'"
done

# 14: --timeout needs a number
setup
run --timeout
[ "$RC" = 2 ] || fail "case 14 rc=$RC (no value)"
setup
run --timeout abc pkg-a
[ "$RC" = 2 ] || fail "case 14 rc=$RC (abc)"
[ ! -s "$TMP/apt.log" ] || fail "case 14 ran apt"

# 15: the list exists but no sources file uses it (the arm64 image): untouched
setup 100
printf 'URIs: http://ports.example.invalid/ubuntu\n' > "$TMP/sources/ubuntu.sources"
run pkg-a
[ "$RC" = 0 ] || fail "case 15 rc=$RC"
[ "$(wc -l < "$TMP/mirrors.txt" | tr -d ' ')" = 3 ] || fail "case 15 shortened"
! grep -q dropping "$TMP/out" || fail "case 15 printed a drop"

# 16: a blank line is not a mirror: one mirror plus a blank line is kept
setup 100 100
printf 'http://a.example.invalid/ubuntu/\tpriority:1\n\n' > "$TMP/mirrors.txt"
run pkg-a
grep -q 'a.example.invalid' "$TMP/mirrors.txt" || fail "case 16 emptied the list"

echo "PASS: 16 cases"
