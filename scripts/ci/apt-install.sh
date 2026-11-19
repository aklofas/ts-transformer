#!/usr/bin/env bash
# One bounded, retried apt install: apt-get update, then a download-only
# install, then the real install, retried as a whole.
#
# Usage: apt-install.sh [--timeout SECONDS] [--no-recommends] [--] <packages or apt-get install arguments...>
# Exit 0 when one attempt installed everything, else the last attempt's status
# (124 = a step hit its bound).
#
# Environment (all optional): APT_INSTALL_ATTEMPTS (3), APT_INSTALL_WAITS
# ("20 60": seconds before attempts 2 and 3), APT_INSTALL_TIMEOUT (240, the
# bound of update and of the download), APT_INSTALL_UNPACK_TIMEOUT (900),
# APT_INSTALL_MIRRORS_FILE (/etc/apt/apt-mirrors.txt), APT_INSTALL_SUDO (sudo;
# empty runs apt-get directly).
#
# Why Acquire::Retries is 1: apt spends every retry on one mirror before it
# tries the next one in the mirror list, so a high count only delays the
# failover; the retry that matters is the whole attempt, which drops the first
# mirror from the list.
# Why the bound is a wall clock: apt's own timeouts are per read, and a server
# that trickles bytes never trips them, so a download can run to the job cap.
# Why the download is a separate step: the short bound applies to the network
# only; unpacking needs none and may be slow on a loaded runner, so it gets its
# own long bound and a slow unpack is never mistaken for a dead mirror.
# Why `timeout -k 10`: after the signal, a stuck apt-get gets ten seconds and
# then SIGKILL; timeout signals its whole process group, so no fetch helper is
# left running into the next attempt.
set -euo pipefail

attempts="${APT_INSTALL_ATTEMPTS:-3}"
waits="${APT_INSTALL_WAITS:-20 60}"
bound="${APT_INSTALL_TIMEOUT:-240}"
unpack_bound="${APT_INSTALL_UNPACK_TIMEOUT:-900}"
mirrors="${APT_INSTALL_MIRRORS_FILE:-/etc/apt/apt-mirrors.txt}"
sudo_cmd="${APT_INSTALL_SUDO-sudo}"
recommends=()

while [ $# -gt 0 ]; do
  case "$1" in
    --timeout) bound="$2"; shift 2 ;;
    --no-recommends) recommends=(--no-install-recommends); shift ;;
    --) shift; break ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || { echo "apt-install.sh: no packages given" >&2; exit 2; }

opts=(-o Acquire::Retries=1 -o Acquire::http::Timeout=20 -o Acquire::https::Timeout=20 -o DPkg::Lock::Timeout=60)

# Run apt-get under a wall-clock bound: step <seconds> <apt-get arguments...>.
step() {
  local t="$1"; shift
  # shellcheck disable=SC2086 # sudo_cmd is empty or one word
  $sudo_cmd timeout -k 10 "$t" apt-get "${opts[@]}" "$@"
}

attempt_once() {
  step "$bound" update || return $?
  step "$bound" install -y --download-only ${recommends[@]+"${recommends[@]}"} "$@" || return $?
  step "$unpack_bound" install -y ${recommends[@]+"${recommends[@]}"} "$@"
}

# Drop the mirror the image ranks first (a mirror that is down is the usual
# reason an attempt failed); the list then falls through to the next one.
drop_first_mirror() {
  [ -f "$mirrors" ] || return 0
  [ "$(wc -l < "$mirrors" | tr -d ' ')" -gt 1 ] || return 0
  head -n 1 "$mirrors" | grep -q 'priority:1' || return 0
  local host
  host="$(head -n 1 "$mirrors" | cut -f1)"
  echo "dropping mirror $host from $mirrors"
  # shellcheck disable=SC2086
  $sudo_cmd sed -i '1d' "$mirrors"
}

rc=0
n=1
while [ "$n" -le "$attempts" ]; do
  if [ "$n" -gt 1 ]; then
    [ "$n" -ne 2 ] || drop_first_mirror
    w="$(echo "$waits" | awk -v i=$((n - 1)) '{print $i}')"
    [ -z "$w" ] || sleep "$w"
  fi
  if attempt_once "$@"; then
    exit 0
  else
    rc=$?
  fi
  echo "::notice::apt install attempt $n of $attempts failed with status $rc"
  n=$((n + 1))
done
exit "$rc"
