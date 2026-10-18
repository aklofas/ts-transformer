#!/usr/bin/env bash
# Print the previous release tag: the newest tag named exactly vMAJOR.MINOR.PATCH
# (version order) that does not point at HEAD. Release candidates and tags of
# other kinds are never a comparison base. Prints nothing when there is none.
# Shared by scripts/check/c/abi-symbols.sh and the c-abi CI job.
set -uo pipefail
for t in $(git tag --list 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$'); do
    if ! git tag --points-at HEAD | grep -qx "$t"; then echo "$t"; break; fi
done
exit 0
