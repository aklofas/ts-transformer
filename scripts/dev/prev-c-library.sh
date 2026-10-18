#!/usr/bin/env bash
# The previous release tag's libtstrans, for the two jobs that compare against it:
# the c-abi job in ci.yml and the abi job in release-qualify.yml. One source of
# truth for the shell; the actions/cache step stays in each workflow, because it
# is a workflow step.
#
# usage: prev-c-library.sh <step>
#   select    previous tag (scripts/dev/prev-tag.sh) and the ABI minors of the
#             tag and of this tree; outputs prev, prev_minor, head_minor
#   release   the library from the tag's Release package (tstrans.abi); output
#             lib when the Release carries a C package, nothing otherwise
#   build     build the library from source at the tag (worktree under
#             $RUNNER_TEMP/prev, target dir $RUNNER_TEMP/prev-target)
#   header <file>   write the tag's committed tstrans.h to <file>
#   resolve   print the old library path on stdout and how it was obtained on
#             stderr (RELEASE_LIB set = the Release package, else the source build)
# Environment: PREV (release, build, header, resolve), RELEASE_LIB (resolve),
#   GH_TOKEN (release), RUNNER_TEMP (all but select), GITHUB_REPOSITORY (release).
# Outputs go to $GITHUB_OUTPUT when it is set, to stdout otherwise.
set -euo pipefail

out() { if [ -n "${GITHUB_OUTPUT:-}" ]; then echo "$1" >> "$GITHUB_OUTPUT"; else echo "$1"; fi; }

case "${1:-}" in
  select)
    git fetch --tags --depth=1 origin
    prev="$(bash scripts/dev/prev-tag.sh)"
    [ -n "$prev" ] || { echo "no vMAJOR.MINOR.PATCH tag in this clone" >&2; exit 1; }
    pat='^pub const TST_ABI_VERSION_MINOR: [A-Za-z0-9_:]* = \([0-9][0-9]*\);.*'
    prev_minor="$(git show "$prev:bindings/c/core/src/lib.rs" | sed -n "s/$pat/\1/p" | head -1)"
    head_minor="$(sed -n "s/$pat/\1/p" bindings/c/core/src/lib.rs | head -1)"
    [ -n "$prev_minor" ] && [ -n "$head_minor" ] || { echo "could not read the ABI minor ($prev_minor / $head_minor)" >&2; exit 1; }
    out "prev=$prev"
    out "prev_minor=$prev_minor"
    out "head_minor=$head_minor"
    echo "previous tag $prev (ABI minor $prev_minor), this tree ABI minor $head_minor"
    ;;
  release)
    mkdir -p "$RUNNER_TEMP/prev-release"
    if gh release download "$PREV" -R "$GITHUB_REPOSITORY" -p 'tstrans-c-*-linux-x86_64.tar.gz' -D "$RUNNER_TEMP/prev-release" 2>/dev/null \
       && tar -xzf "$RUNNER_TEMP"/prev-release/tstrans-c-*-linux-x86_64.tar.gz -C "$RUNNER_TEMP/prev-release" \
       && abi="$(find "$RUNNER_TEMP/prev-release" -name tstrans.abi | head -1)" && [ -n "$abi" ]; then
      echo "previous tag $PREV, taken from its Release package"
      out "lib=$abi"
    else
      echo "the $PREV Release carries no C package"
    fi
    ;;
  build)
    git worktree add "$RUNNER_TEMP/prev" "$PREV"
    git -C "$RUNNER_TEMP/prev" submodule update --init --recursive
    (cd "$RUNNER_TEMP/prev" && CARGO_TARGET_DIR="$RUNNER_TEMP/prev-target" cargo build -p tst-c --all-features)
    test -f "$RUNNER_TEMP/prev-target/debug/libtstrans.so"
    ;;
  header)
    [ -n "${2:-}" ] || { echo "usage: $0 header <file>" >&2; exit 2; }
    # The tag's committed header is what the previous package shipped; the
    # abi-compat rail needs it to see a struct that lost its body.
    git show "$PREV:bindings/c/include/tstrans.h" > "$2"
    ;;
  resolve)
    if [ -n "${RELEASE_LIB:-}" ]; then
      echo "previous tag $PREV, from its Release package" >&2
      echo "$RELEASE_LIB"
    else
      echo "previous tag $PREV, built from source" >&2
      echo "$RUNNER_TEMP/prev-target/debug/libtstrans.so"
    fi
    ;;
  *) echo "usage: $0 select|release|build|header <file>|resolve" >&2; exit 2 ;;
esac
