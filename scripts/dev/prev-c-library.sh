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
#             lib when the Release carries one C package that SHA256SUMS and its
#             build-provenance attestation (signed by release-qualify.yml at the
#             tag) verify, else fallback=<why the source build is used>
#   build     build the library from source at the tag (worktree under
#             $RUNNER_TEMP/prev, target dir $RUNNER_TEMP/prev-target)
#   header <file>   write the tag's committed tstrans.h to <file>
#   resolve   print the old library path on stdout and how it was obtained on
#             stderr (RELEASE_LIB set = the Release package, else the source build)
#   source    print how the old library was obtained, for the ABI record:
#             `attested`, or `built: <fallback reason>`
# Environment: PREV (release, build, header, resolve), RELEASE_LIB (resolve,
#   source), FALLBACK (source: the release step's fallback output),
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
    # The package is used only after SHA256SUMS (from the same Release) lists
    # it with a matching digest and its build-provenance attestation verifies
    # as signed by release-qualify.yml for the tag: every PR and dispatch run of
    # that workflow attests its own package too, so the repository alone would
    # accept any of them. Anything less falls back to the source build at the
    # tag, which needs no trust in a downloaded file; the fallback output says
    # why, for the ABI record.
    d="$RUNNER_TEMP/prev-release"
    mkdir -p "$d"
    if ! gh release download "$PREV" -R "$GITHUB_REPOSITORY" -p 'tstrans-c-*-linux-x86_64.tar.gz' -D "$d" 2>/dev/null; then
      echo "the $PREV Release carries no C package"
      out "fallback=no package for $PREV"
      exit 0
    fi
    # unverified <detail> <fallback reason>
    unverified() { echo "$1"; echo "the $PREV Release package failed verification; the source build is used"; out "fallback=$2"; exit 0; }
    set -- "$d"/tstrans-c-*-linux-x86_64.tar.gz
    # Two packages are refused rather than picked between: neither is known to be the tag's.
    [ $# -eq 1 ] || unverified "the $PREV Release holds $# C packages, expected one" "more than one package"
    tarball="$1"; name="$(basename "$tarball")"
    gh release download "$PREV" -R "$GITHUB_REPOSITORY" -p SHA256SUMS -D "$d" 2>/dev/null \
      || unverified "SHA256SUMS: not in the $PREV Release" "SHA256SUMS missing"
    # --ignore-missing passes on an empty match, so the tarball's own line is required first.
    grep -qE "^[0-9a-f]{64}  $(printf '%s' "$name" | sed 's/[.]/[.]/g')\$" "$d/SHA256SUMS" \
      || unverified "$name: no line for it in SHA256SUMS" "package not listed in SHA256SUMS"
    (cd "$d" && sha256sum -c --ignore-missing --quiet SHA256SUMS) \
      || unverified "$name: sha256sum -c against SHA256SUMS failed" "checksum mismatch"
    gh attestation verify "$tarball" -R "$GITHUB_REPOSITORY" \
      --signer-workflow aklofas/ts-transformer/.github/workflows/release-qualify.yml \
      --source-ref "refs/tags/$PREV" \
      || unverified "$name: gh attestation verify failed" "attestation verify failed"
    if tar -xzf "$tarball" -C "$d" && abi="$(find "$d" -name tstrans.abi | head -1)" && [ -n "$abi" ]; then
      echo "previous tag $PREV, taken from its verified Release package"
      out "lib=$abi"
    else
      unverified "$name: holds no tstrans.abi" "no tstrans.abi in package"
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
  source)
    if [ -n "${RELEASE_LIB:-}" ]; then echo attested
    elif [ -n "${FALLBACK:-}" ]; then echo "built: $FALLBACK"
    else echo "prev-c-library.sh source: neither a verified package nor a fallback reason" >&2; exit 2
    fi
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
  *) echo "usage: $0 select|release|build|header <file>|resolve|source" >&2; exit 2 ;;
esac
