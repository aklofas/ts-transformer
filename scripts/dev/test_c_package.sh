#!/usr/bin/env bash
# Self-test for scripts/dev/c-package.sh. Builds tst-c twice (srt,rtp and
# all features) and checks the staged packages against each other.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG="$HERE/c-package.sh"
[ -f "$PKG" ] || { echo "FAIL: $PKG does not exist" >&2; exit 1; }

T="$(mktemp -d)"
trap 'chmod -R u+w "$T" 2>/dev/null; rm -rf "$T"' EXIT
fail=0
check() { if "$@"; then echo "ok   $*" | cut -c1-110; else echo "FAIL $*"; fail=1; fi; }

bash "$PKG" --features srt,rtp --out "$T/a" || { echo "FAIL: stage srt,rtp"; exit 1; }
bash "$PKG" --all-features --out "$T/b" || { echo "FAIL: stage all"; exit 1; }

check cmp -s "$T/a/include/tstrans.h" "$T/b/include/tstrans.h"
configs_differ() { ! cmp -s "$T/a/include/tstrans_config.h" "$T/b/include/tstrans_config.h"; }
check configs_differ
for d in a b; do
    manifest_check() { (cd "$T/$d" && jq -r '.files | to_entries[] | "\(.value)  \(.key)"' manifest.json | sha256sum -c --quiet -); }
    check manifest_check
    # features in the manifest are exactly the TST_HAS_ defines of the config
    feat_check() { test "$(jq -r '.features|sort|join(",")' "$T/$d/manifest.json")" = \
        "$(sed -n 's/^#define TST_HAS_\(.*\) 1$/\1/p' "$T/$d/include/tstrans_config.h" | tr '[:upper:]' '[:lower:]' | sort | paste -sd, -)"; }
    check feat_check
    pc_check() { test "$(pkg-config --with-path="$T/$d/lib/pkgconfig" --variable=includedir tstrans)" = "$T/$d/lib/pkgconfig/../../include"; }
    check pc_check
    ro_check() { ! test -w "$T/$d/include/tstrans.h"; }
    check ro_check
done
# A copy in another directory must resolve to itself, not to the staging dir.
cp -R "$T/a" "$T/moved"
reloc_cflags() { test "$(pkg-config --with-path="$T/moved/lib/pkgconfig" --cflags tstrans | xargs)" = "-I$T/moved/lib/pkgconfig/../../include"; }
check reloc_cflags
reloc_libs() { case "$(pkg-config --with-path="$T/moved/lib/pkgconfig" --libs tstrans)" in "-L$T/moved/lib/pkgconfig/../../lib -ltstrans"*) true ;; *) false ;; esac; }
check reloc_libs
a_features() { test "$(jq -r '.features|join(",")' "$T/a/manifest.json")" = "srt,rtp"; }
check a_features
refuse_check() { bash "$PKG" --features srt --out "$T/a" >/dev/null 2>&1; test $? -eq 2; }
check refuse_check
if [ "$fail" -eq 0 ]; then echo "c-package self-test: PASS"; else echo "c-package self-test: FAIL"; exit 1; fi
