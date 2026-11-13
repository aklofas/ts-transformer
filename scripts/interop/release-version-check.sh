#!/usr/bin/env bash
# Refuse a release run on a checkout whose version sweep has not happened.
# The soak and the corpus run judge one frozen commit for two days; when that
# commit still carries the previous release's version, the evidence is of a
# tree that can never be tagged, and the mistake shows only after the run.
#
# Usage:
#   bash release-version-check.sh [<checkout directory>]
#       (default: the repository this script is in)
#
# The workspace version (`[workspace.package] version` in the checkout's
# Cargo.toml) must be above the newest release tag: the newest tag named
# exactly vMAJOR.MINOR.PATCH, by version order, the grammar and order of
# scripts/dev/prev-tag.sh. Release candidates and tags of other kinds are
# not release tags. A tag that points at HEAD counts: a release run is not
# repeated on a released commit under the version it was released as.
#
# Exit status:
#   0  the version is above the newest release tag
#   1  it equals that tag's version (the version sweep has not happened) or
#      is below it
#   2  the check could not be made: no release tag is visible (a shallow
#      clone, or tags never fetched: `git fetch --tags origin`), the version
#      is missing or not MAJOR.MINOR.PATCH (no leading zeros), or the
#      directory is not the top of a checkout (an unpacked archive inside
#      another repository would be judged by that repository's tags)
#
# Environment:
#   TSTRANS_NOT_A_RELEASE=1   skip the check, for a run that is not release
#                             evidence (a drill, a stress run between
#                             releases); the skip is printed
#
# Called by release-vm-setup.sh after its checkout stage and by
# release-vm-soak.sh launch before anything starts.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
DIR=${1:-$HERE/../..}
me=release-version-check

if [[ "${TSTRANS_NOT_A_RELEASE:-}" == 1 ]]; then
  echo "$me: skipped (TSTRANS_NOT_A_RELEASE=1): this run is not release evidence"
  exit 0
fi

top=$(git -C "$DIR" rev-parse --show-toplevel 2>/dev/null) && [[ "$top" -ef "$DIR" ]] ||
  { echo "$me: $DIR is not a git checkout${top:+ (it is inside $top)}" >&2; exit 2; }
[[ -f "$DIR/Cargo.toml" ]] ||
  { echo "$me: $DIR has no Cargo.toml to read the workspace version from" >&2; exit 2; }

version=$(awk '
  /^\[/ { table = $0 }
  table == "[workspace.package]" && /^version[[:space:]]*=/ {
    sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
  }' "$DIR/Cargo.toml")
[[ "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] ||
  { echo "$me: the workspace version in $DIR/Cargo.toml is '${version}', not MAJOR.MINOR.PATCH" >&2; exit 2; }

tag=$(git -C "$DIR" tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n 1)
if [[ -z "$tag" ]]; then
  echo "$me: no vMAJOR.MINOR.PATCH tag is visible in $DIR, so the workspace version $version cannot be" >&2
  echo "$me: compared with the newest release; fetch the tags (git fetch --tags origin) and run again" >&2
  exit 2
fi

# above <a> <b>: true when version a is above version b, field by field.
above() {
  local a b i
  IFS=. read -r -a a <<<"$1"
  IFS=. read -r -a b <<<"$2"
  for i in 0 1 2; do
    ((10#${a[i]} > 10#${b[i]})) && return 0
    ((10#${a[i]} < 10#${b[i]})) && return 1
  done
  return 1
}

released=${tag#v}
if above "$version" "$released"; then
  echo "$me: ok (workspace $version, newest release tag $tag)"
  exit 0
fi
if [[ "$version" == "$released" ]]; then
  echo "$me: the workspace version $version is the version of the newest release tag $tag:" >&2
  if git -C "$DIR" tag --points-at HEAD | grep -qx "$tag"; then
    echo "$me: $tag points at this commit, which is the released tree; a release run is not repeated on it" >&2
  else
    echo "$me: this tree has not had its version sweep, so a release run on it would judge a commit" >&2
    echo "$me: that cannot be tagged. Land the release-prep change (version sweep) and freeze that commit" >&2
  fi
else
  echo "$me: the workspace version $version is lower than the newest release tag $tag;" >&2
  echo "$me: a release is numbered above every released version (is this an old commit?)" >&2
fi
echo "$me: for a run that is not release evidence, set TSTRANS_NOT_A_RELEASE=1" >&2
exit 1
