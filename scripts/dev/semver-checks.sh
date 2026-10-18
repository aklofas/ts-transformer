#!/usr/bin/env bash
# Report each public crate's breaking changes since the previous release tag,
# using cargo-semver-checks. INFORMATIONAL BY DECISION (P10-D8/D9): the project
# is pre-1.0 and breaks are recorded in the CHANGELOG, so this never gates; it
# is a cross-check that the CHANGELOG's BREAKING list is complete.
#
# usage: scripts/dev/semver-checks.sh [<out-dir>]      (default: target/semver-report)
# writes <out-dir>/<crate>.txt per crate and <out-dir>/semver-checks.json:
#   [{"crate","exit","failed_checks","items","report"}]
#   failed_checks = the "N fail" of the tool's "M checks: P pass, N fail" line
#   items         = listed entries under every "Failed in:" block (raw lines, so
#                   an item the tool prints twice counts twice)
#
# Crates: the public-api roster (scripts/lib/roster.sh public-api), the one list
#   the public-api baselines and CI use; fails if it is not 10.
# Baseline: scripts/dev/prev-tag.sh (the c-abi rail's selection).
# --release-type minor: for a 0.x crate the tool otherwise assumes a patch bump
#   or only reports what the declared bump forbids; "minor" makes it list every
#   break since the tag.
# +stable: cargo-semver-checks 0.51 needs rustdoc JSON newer than the pinned
#   1.85 toolchain produces, so it runs under the stable toolchain (the tag's
#   tree is compiled with it too). Fine for an informational report.
#
# Baseline tree: a detached worktree of the tag under <out-dir>/baseline with the
#   three vendored-source submodules (srt, mbedtls, librist) initialised, passed
#   as --baseline-root; the tool's own clone has no submodules, so tst-srt,
#   tst-rist and the sys crates cannot build there. Needs network or a local
#   submodule cache for that init.
#
# Exit 0 always, except 2 when the tool or the stable toolchain is missing.
set -uo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT" || exit 2
OUT="${1:-target/semver-report}"

if ! cargo +stable semver-checks --version >/dev/null 2>&1; then
    echo "semver-checks: needs the stable toolchain and cargo-semver-checks:" >&2
    echo "  rustup toolchain install stable && cargo +stable install cargo-semver-checks --locked" >&2
    exit 2
fi
prev="$(bash scripts/dev/prev-tag.sh)"
[ -n "$prev" ] || { echo "semver-checks: no previous release tag (git fetch --tags)" >&2; exit 2; }
pairs="$(bash scripts/lib/roster.sh public-api)" || exit 2
[ "$(printf '%s\n' "$pairs" | grep -c .)" -eq 10 ] || { echo "semver-checks: public-api roster is not 10 crates: $pairs" >&2; exit 2; }

mkdir -p "$OUT"
base="$OUT/baseline"
rows="$(mktemp)"
trap 'rm -f "$rows"; git worktree remove --force "$base" >/dev/null 2>&1' EXIT
git worktree remove --force "$base" >/dev/null 2>&1
git worktree add --detach "$base" "$prev" >/dev/null 2>&1 || { echo "semver-checks: cannot check out $prev at $base" >&2; exit 2; }
(cd "$base" && git submodule update --init --recursive -- \
    crates/srt-sys/vendor/srt crates/mbedtls-src/vendor/mbedtls crates/rist-sys/vendor/librist) >"$OUT/baseline-submodules.log" 2>&1 \
    || { echo "semver-checks: baseline submodule init failed, see $OUT/baseline-submodules.log" >&2; exit 2; }
for pair in $pairs; do
    pkg="${pair%%:*}"
    report="$OUT/$pkg.txt"
    SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo +stable semver-checks check-release \
        -p "$pkg" --baseline-root "$base" --release-type minor >"$report" 2>&1
    rc=$?
    failed="$(sed -n 's/.* checks: .*, \([0-9][0-9]*\) fail.*/\1/p' "$report" | head -1)"
    items="$(awk '/^Failed in:/{f=1;next} /^$/{f=0} f' "$report" | grep -c .)"
    printf '%s\t%s\t%s\t%s\t%s\n' "$pkg" "$rc" "${failed:-0}" "$items" "$report" >>"$rows"
    case "$rc" in 0|100) mark="" ;; *) mark="(ERROR rc=$rc)" ;; esac
    printf '%s:%s/%s%s ' "$pkg" "${failed:-0}" "$items" "$mark"
done
echo "(crate:failed-checks/items vs $prev)"
python3 - "$rows" "$OUT/semver-checks.json" <<'P'
import json, sys
out = []
for l in open(sys.argv[1]):
    c, rc, f, i, r = l.rstrip("\n").split("\t")
    out.append({"crate": c, "exit": int(rc), "failed_checks": int(f), "items": int(i), "report": r})
json.dump(out, open(sys.argv[2], "w"), indent=1)
P
exit 0
