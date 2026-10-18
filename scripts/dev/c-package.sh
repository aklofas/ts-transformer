#!/usr/bin/env bash
# Stage ONE build of libtstrans into a self-contained, read-only directory.
#
# Why: `target/<profile>/include/tstrans_config.h` and `tstrans.pc` are
# refreshed only when tst-c's build script re-runs (the first build of a
# feature set, or a change to the C sources or the cbindgen config), while the library next
# to them is re-uplifted by every build and may be from a different feature
# set. A consumer then compiles against one feature set and links another.
# This script takes the files cargo's build-script and compiler messages name
# for THIS build and copies them out, so the header, config, libraries and
# pkg-config file in the package always come from the same feature set.
#
# Usage:
#   scripts/dev/c-package.sh [--features <a,b,..> | --all-features]
#                            [--profile debug|release] --out <dir>
#
# Layout of <dir>:
#   include/tstrans.h  include/tstrans_config.h
#   lib/libtstrans.{so,a} (.dylib on macOS)  lib/pkgconfig/tstrans.pc
#   manifest.json   version, abi{major,minor}, features[], target, profile,
#                   commit, files{<relpath>: <sha256>}
# Every file is read-only. To delete a package: chmod -R u+w <dir>; rm -rf <dir>
#
# Exit codes: 0 staged; 1 build or output parsing failed; 2 usage error or
# <dir> exists and is not empty.
#
# Linux and macOS (bash 3.2 compatible). Needs cargo, jq and sha256sum or shasum.
set -euo pipefail

usage() { sed -n '2,/^set -e/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

FEATURE_ARGS=()
FEATURES_LABEL="default"
PROFILE=debug
OUT=""
while [ $# -gt 0 ]; do
    case "$1" in
        --features) [ $# -ge 2 ] || { usage >&2; exit 2; }
                    FEATURE_ARGS=(--features "$2"); FEATURES_LABEL="$2"; shift 2 ;;
        --all-features) FEATURE_ARGS=(--all-features); FEATURES_LABEL="all"; shift ;;
        --profile) [ $# -ge 2 ] || { usage >&2; exit 2; }; PROFILE="$2"; shift 2 ;;
        --out) [ $# -ge 2 ] || { usage >&2; exit 2; }; OUT="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "c-package: unknown argument: $1" >&2; usage >&2; exit 2 ;;
    esac
done
case "$PROFILE" in debug|release) ;; *) echo "c-package: --profile must be debug or release" >&2; exit 2 ;; esac
[ -n "$OUT" ] || { echo "c-package: --out <dir> is required" >&2; exit 2; }
if [ -e "$OUT" ] && { [ ! -d "$OUT" ] || [ -n "$(ls -A "$OUT")" ]; }; then
    echo "c-package: $OUT exists and is not empty; refusing to overwrite" >&2
    exit 2
fi
command -v jq >/dev/null || { echo "c-package: jq is required" >&2; exit 1; }

if command -v sha256sum >/dev/null; then
    sha256() { sha256sum "$1" | cut -d' ' -f1; }
else
    sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/../.."

PROFILE_ARGS=()
[ "$PROFILE" = release ] && PROFILE_ARGS=(--release)

MSGS="$(mktemp)"
trap 'rm -f "$MSGS"' EXIT
SRT_FORCE_VENDORED="${SRT_FORCE_VENDORED:-1}" RIST_FORCE_VENDORED="${RIST_FORCE_VENDORED:-1}" \
    cargo build -p tst-c ${FEATURE_ARGS[@]+"${FEATURE_ARGS[@]}"} ${PROFILE_ARGS[@]+"${PROFILE_ARGS[@]}"} \
    --message-format=json-render-diagnostics >"$MSGS" || { echo "c-package: cargo build failed" >&2; exit 1; }

# The tst-c build script's OUT_DIR (id forms: "path+file:///..#tst-c@0.8.0"
# and the older "tst-c 0.8.0 (path+file:///...)"). A cached build replays its
# build-script-executed message, so the line is present on a warm build too.
OUT_DIR="$(jq -r 'select(.reason=="build-script-executed" and (.package_id|test("(^|[#/])tst-c[@ ]"))) | .out_dir' "$MSGS" | tail -n 1)"
LIBS="$(jq -r 'select(.reason=="compiler-artifact" and .target.name=="tstrans" and (.package_id|test("(^|[#/])tst-c[@ ]"))) | .filenames[]' "$MSGS" \
    | grep -E '\.(so|dylib|a)$' | sort -u)"
if [ -z "$OUT_DIR" ] || [ ! -d "$OUT_DIR/include" ] || [ -z "$LIBS" ]; then
    echo "c-package: could not find tst-c's out_dir or its tstrans artifacts in cargo's messages" >&2
    exit 1
fi

mkdir -p "$OUT/include" "$OUT/lib/pkgconfig"
cp "$OUT_DIR/include/tstrans.h" "$OUT_DIR/include/tstrans_config.h" "$OUT/include/"
while IFS= read -r lib; do cp "$lib" "$OUT/lib/"; done <<<"$LIBS"
OUT_ABS="$(cd "$OUT" && pwd)"
sed "s|^prefix=.*|prefix=$OUT_ABS|" "$OUT_DIR/tstrans.pc" >"$OUT/lib/pkgconfig/tstrans.pc"

# What the build enabled is what its config header says.
FEATURES_JSON="$(sed -n 's/^#define TST_HAS_\([A-Z0-9_]*\) 1$/\1/p' "$OUT/include/tstrans_config.h" \
    | tr '[:upper:]' '[:lower:]' | jq -R . | jq -s .)"
ABI_MAJOR="$(sed -n 's/^#define TST_ABI_VERSION_MAJOR \([0-9]*\).*/\1/p' "$OUT/include/tstrans.h" | head -n 1)"
ABI_MINOR="$(sed -n 's/^#define TST_ABI_VERSION_MINOR \([0-9]*\).*/\1/p' "$OUT/include/tstrans.h" | head -n 1)"
VERSION="$(sed -n 's/^Version: //p' "$OUT/lib/pkgconfig/tstrans.pc")"
TARGET="$(rustc -vV | sed -n 's/^host: //p')"
COMMIT="$(git rev-parse HEAD 2>/dev/null || echo unknown)"

FILES_JSON="{}"
while IFS= read -r rel; do
    FILES_JSON="$(jq -c --arg k "$rel" --arg v "$(sha256 "$OUT/$rel")" '. + {($k): $v}' <<<"$FILES_JSON")"
done < <(cd "$OUT" && find . -type f | sed 's|^\./||' | sort)

jq -n --arg version "$VERSION" --argjson major "${ABI_MAJOR:-0}" --argjson minor "${ABI_MINOR:-0}" \
    --argjson features "$FEATURES_JSON" --arg target "$TARGET" --arg profile "$PROFILE" \
    --arg commit "$COMMIT" --argjson files "$FILES_JSON" \
    '{version:$version, abi:{major:$major, minor:$minor}, features:$features, target:$target,
      profile:$profile, commit:$commit, files:$files}' >"$OUT/manifest.json"

chmod -R a-w "$OUT"
echo "c-package: staged $OUT (features: $(jq -r '.features|join(",")' "$OUT/manifest.json"), abi $ABI_MAJOR.$ABI_MINOR)"
