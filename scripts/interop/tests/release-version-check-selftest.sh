#!/usr/bin/env bash
# Self-test for release-version-check.sh: throwaway repositories with a
# workspace version and a set of tags, and the verdict each must get. A
# release run is refused on a tree whose version is the newest release
# tag's or lower, and whenever the check cannot see what it needs.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$HERE/../release-version-check.sh
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0
n=0

export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
unset TSTRANS_NOT_A_RELEASE
g() { git -C "$repo" -c user.name=t -c user.email=t@example.com -c commit.gpgsign=false -c tag.gpgsign=false "$@"; }

# repo_at <version>: a fresh repository whose only commit declares the
# workspace version; a [package] table before it carries another number,
# which is never the one read.
repo_at() {
  n=$((n + 1))
  repo=$tmp/r$n
  mkdir -p "$repo"
  git -C "$repo" init -q
  commit_version "$1"
}
# commit_version <version>: one more commit declaring the workspace version
# (a new commit even when the number did not move).
commit_version() {
  printf '[package]\nname = "x"\nversion = "9.9.9"\n\n[workspace.package]\nversion      = "%s"\nedition = "2024"\n' "$1" \
    > "$repo/Cargo.toml"
  g add -A
  g commit -q --allow-empty -m "version $1"
}

check() { # <name> <want rc> <output pattern> [<pattern it must not hold>]
  local name=$1 want=$2 pat=$3 absent=${4:-}
  bash "$SCRIPT" "$repo" > "$tmp/out" 2>&1
  local rc=$?
  if [[ $rc -ne $want ]]; then
    echo "FAIL: $name: exit $rc, wanted $want"; sed 's/^/    /' "$tmp/out"; fail=1; return
  fi
  if ! grep -q -- "$pat" "$tmp/out"; then
    echo "FAIL: $name: output lacks '$pat'"; sed 's/^/    /' "$tmp/out"; fail=1; return
  fi
  if [[ -n "$absent" ]] && grep -q -- "$absent" "$tmp/out"; then
    echo "FAIL: $name: output holds '$absent'"; sed 's/^/    /' "$tmp/out"; fail=1; return
  fi
  echo "ok: $name"
}

# The version sweep has happened: the tree is above the newest release tag.
repo_at 0.10.0; g tag v0.10.0; commit_version 0.11.0
check "a swept tree may start a release run" 0 'ok (workspace 0.11.0, newest release tag v0.10.0)'

# The mistake this exists for: the soaked tree still says the released version.
repo_at 0.10.0; g tag v0.10.0; commit_version 0.10.0
check "an unswept tree is refused" 1 'workspace version 0.10.0 is the version of the newest release tag v0.10.0' 'points at this commit'
grep -q 'version sweep' "$tmp/out" || { echo "FAIL: the refusal does not name the version sweep as the remedy"; fail=1; }

repo_at 0.10.0; g tag v0.10.0; commit_version 0.9.1
check "a version below the newest release tag is refused" 1 'workspace version 0.9.1 is lower than the newest release tag v0.10.0'

# Version order, not text order: v0.10.0 is newer than v0.9.0.
repo_at 0.9.0; g tag v0.9.0; commit_version 0.10.0; g tag v0.10.0; commit_version 0.10.0
check "the newest tag is chosen by version order" 1 'newest release tag v0.10.0'
commit_version 0.9.5
check "0.9.5 is below v0.10.0" 1 'is lower than the newest release tag v0.10.0'

# No release tag visible is a failure with its remedy, never a pass.
repo_at 0.11.0
check "a checkout without tags is refused" 2 'git fetch --tags'
g tag v0.11.0-rc1; g tag foo; g tag v1; g tag release-0.10.0
check "tags of other kinds are not release tags" 2 'git fetch --tags'

# ... and they are ignored beside a real one: v0.11.0-rc1 does not make 0.11.0 unswept.
repo_at 0.10.0; g tag v0.10.0; commit_version 0.11.0; g tag v0.11.0-rc1; g tag foo
check "a release candidate tag is ignored" 0 'ok (workspace 0.11.0, newest release tag v0.10.0)'

# The tag on HEAD counts: a release run is not repeated on a released commit.
repo_at 0.10.0; g tag v0.10.0; commit_version 0.11.0; g tag -a v0.11.0 -m v0.11.0
check "the tag pointing at HEAD still counts" 1 'v0.11.0 points at this commit'

# Unreadable inputs fail.
repo_at 0.10.0; g tag v0.10.0; commit_version 0.11.0-dev
check "a version that is not MAJOR.MINOR.PATCH is refused" 2 'not MAJOR.MINOR.PATCH'
repo_at 0.10.0; g tag v0.10.0; commit_version 0.011.0
check "a version component with a leading zero is refused" 2 'not MAJOR.MINOR.PATCH'
repo_at 0.10.0; g tag v0.10.0; rm "$repo/Cargo.toml"
check "a checkout without Cargo.toml is refused" 2 'Cargo.toml'
repo=$tmp/plain; mkdir -p "$repo"
check "a directory that is not a checkout is refused" 2 'not a git checkout'
# An unpacked archive under another repository has no tags of its own: the
# repository around it must not answer for it.
repo_at 0.10.0; g tag v0.10.0
mkdir "$repo/unpacked"
printf '[workspace.package]\nversion = "0.11.0"\n' > "$repo/unpacked/Cargo.toml"
repo=$repo/unpacked
check "a directory inside another repository is refused" 2 'not a git checkout' 'newest release tag'

# The stated way out for a run that is not release evidence.
repo_at 0.10.0; g tag v0.10.0
export TSTRANS_NOT_A_RELEASE=1
check "TSTRANS_NOT_A_RELEASE=1 skips the check and says so" 0 'skipped (TSTRANS_NOT_A_RELEASE=1)'
export TSTRANS_NOT_A_RELEASE=yes
check "any other value of TSTRANS_NOT_A_RELEASE does not skip" 1 'newest release tag v0.10.0'
unset TSTRANS_NOT_A_RELEASE

[[ $fail -eq 0 ]] && echo "release-version-check-selftest: ok"
exit "$fail"
