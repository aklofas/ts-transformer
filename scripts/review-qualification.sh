#!/usr/bin/env bash
# The subset of the release qualification an outside reviewer can run by hand.
#
#   usage: review-qualification.sh [--list] [--only <part>[,<part>...]] [--outdir <dir>]
#
#   --list    print every part, the tools it needs and its commands; run nothing
#   --only    run just these parts (default: all)
#   --outdir  where logs and results go (default: a new temp dir, kept)
#
# Parts: unit, interop, fuzz, oracles, abi, api, bench-ir. Each prints one
# line, `PART <name>: rc=N` or `PART <name>: NOT RUN (missing <tool>)`. A part
# whose tool is missing is never a pass. The exit status is non-zero when any
# part failed or none ran. The full qualification is release-qualify.yml; this
# is the part that fits in hours on one Linux box with the peer tools
# (ffmpeg, TSDuck's tsp, gst-launch-1.0) installed.
#
# Env: RQ_FUZZ_SECONDS  seconds per fuzz target (default 10; about 7 minutes plus builds)
#      RQ_ROOT          the tree whose scripts run (the self-test points it at stubs)
set -uo pipefail

ROOT="${RQ_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
FUZZ_SECONDS="${RQ_FUZZ_SECONDS:-10}"
NATIVE='SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1'

# One row per part: name | tools needed (a `toolchain:<name>` token is a
# `cargo +<name>` toolchain) | commands, separated by `;;`. @OUT@, @FUZZ@ and
# @PIN@ are filled in when the part runs.
PARTS="
unit|cargo cargo-nextest|$NATIVE cargo nextest run --workspace;;$NATIVE cargo test --doc --workspace
interop|cargo jq python3|$NATIVE cargo build --release -p tst-interop;;bash scripts/interop/run-matrix.sh --outdir @OUT@/interop --seconds 8 --only-subset arm64
fuzz|cargo cargo-fuzz toolchain:nightly|bash scripts/ci/fuzz-shard.sh 1 1 @FUZZ@ @OUT@/fuzz
oracles|cargo cargo-nextest jq python3 tsp ffprobe|$NATIVE cargo build --release -p tst-interop;;bash scripts/interop/run-matrix.sh --outdir @OUT@/oracles --seconds 8 --cells 'analyze/*';;$NATIVE cargo nextest run -p tst-core -E 'binary(mpegts)'
abi|cargo nm|bash scripts/check/c/abi-symbols.sh
api|cargo cargo-public-api rg toolchain:@PIN@|bash scripts/dev/public-api-diff.sh @PIN@;;bash scripts/check/rust/api-stability-coverage.sh;;bash scripts/check/repo/deprecations.sh
bench-ir|valgrind git|bash scripts/dev/bench-ir.sh \"\$(bash scripts/dev/prev-tag.sh)\" HEAD --out @OUT@/bench-ir
"

list=0 only="" out=""
while [ $# -gt 0 ]; do
  case "$1" in
    --list) list=1; shift ;;
    --only) [ $# -ge 2 ] || { echo "--only needs a value" >&2; exit 2; }; only="$2"; shift 2 ;;
    --outdir) [ $# -ge 2 ] || { echo "--outdir needs a value" >&2; exit 2; }; out="$2"; shift 2 ;;
    *) echo "usage: $0 [--list] [--only <part>[,<part>...]] [--outdir <dir>]" >&2; exit 2 ;;
  esac
done

names="$(printf '%s\n' "$PARTS" | awk -F'|' 'NF {print $1}')"
if [ -n "$only" ]; then
  for o in $(printf '%s' "$only" | tr ',' ' '); do
    printf '%s\n' "$names" | grep -qx -- "$o" || { echo "unknown part '$o' (parts: $(echo $names))" >&2; exit 2; }
  done
fi

if [ "$list" = 1 ]; then
  printf '%s\n' "$PARTS" | awk -F'|' 'NF {
    print "PART " $1 ": needs " $2
    n = split($3, c, ";;")
    for (i = 1; i <= n; i++) print "  " c[i]
  }'
  exit 0
fi

pin="$(head -n1 "$ROOT/scripts/lib/public-api-toolchain" 2>/dev/null || true)"
[ -n "$out" ] || out="$(mktemp -d "${TMPDIR:-/tmp}/review-qualification.XXXXXX")"
mkdir -p "$out"
echo "review-qualification: results in $out"

# missing_tool <tools>: print the first tool that is not available.
missing_tool() {
  local t
  for t in $1; do
    case "$t" in
      toolchain:*) cargo "+${t#toolchain:}" --version >/dev/null 2>&1 || { echo "$t"; return 0; } ;;
      *) command -v "$t" >/dev/null 2>&1 || { echo "$t"; return 0; } ;;
    esac
  done
  return 1
}

ran=0 failed=0
while IFS='|' read -r name tools cmds; do
  [ -n "$name" ] || continue
  if [ -n "$only" ]; then
    case ",$only," in *",$name,"*) ;; *) continue ;; esac
  fi
  tools="${tools//@PIN@/$pin}"
  if miss="$(missing_tool "$tools")"; then
    echo "PART $name: NOT RUN (missing $miss)"
    continue
  fi
  cmds="${cmds//@OUT@/$out}"; cmds="${cmds//@FUZZ@/$FUZZ_SECONDS}"; cmds="${cmds//@PIN@/$pin}"
  rc=0
  while IFS= read -r c; do
    [ -n "$c" ] || continue
    echo "+ $c" >> "$out/$name.log"
    (cd "$ROOT" && eval "$c") </dev/null >> "$out/$name.log" 2>&1 || { rc=$?; break; }
  done <<< "${cmds//;;/$'\n'}"
  ran=$((ran + 1))
  [ "$rc" -eq 0 ] || failed=1
  echo "PART $name: rc=$rc"
done <<< "$PARTS"

[ "$ran" -gt 0 ] || { echo "review-qualification: no part ran" >&2; exit 1; }
exit "$failed"
