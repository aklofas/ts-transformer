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
# line: `PART <name>: rc=N`, `PART <name>: NOT RUN (<why>)`, or, when a command
# printed a `SKIP:` line, `PART <name>: rc=0, partly run (<the SKIP reason>)`
# (a part of several commands is NOT RUN only when every command skipped).
# A part whose tool, platform or input is missing is never a pass. A closing
# line lists the skipped checks. The exit status is non-zero when any part
# failed or none ran. A missing peer tool (ffmpeg, tsp, gst-launch-1.0 ...)
# makes interop and oracles FAIL with an undeclared skip, not NOT RUN. The full qualification is release-qualify.yml; this
# is the part that fits in hours on one Linux box with the peer tools
# (ffmpeg, TSDuck's tsp, gst-launch-1.0) installed.
#
# Env: RQ_FUZZ_SECONDS  seconds per fuzz target (default 10; about 7 minutes plus builds)
#      RQ_ROOT          the tree whose scripts run (the self-test points it at stubs)
set -uo pipefail

ROOT="${RQ_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
FUZZ_SECONDS="${RQ_FUZZ_SECONDS:-10}"
NATIVE='SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1'

# One row per part, fields separated by `^`: name ^ requirements ^ commands,
# separated by `;;`. A requirement is a tool on PATH, `os:Linux`, `tag:prev`
# (a previous release tag exists) or `toolchain:<name>` (a `cargo +<name>`
# toolchain). @OUT@, @FUZZ@ and @PIN@ are filled in when the part runs.
PARTS="
unit^cargo cargo-nextest^$NATIVE cargo nextest run --workspace;;$NATIVE cargo test --doc --workspace
interop^cargo jq python3^$NATIVE cargo build --release -p tst-interop;;bash scripts/interop/run-matrix.sh --outdir \"@OUT@/interop\" --seconds 8 --only-subset arm64;;jq -e '.cells | length > 0' \"@OUT@/interop/inventory.json\" >/dev/null || { echo 'run-matrix selected no cell' >&2; exit 1; }
fuzz^cargo cargo-fuzz python3 jq toolchain:nightly^bash scripts/ci/fuzz-shard.sh 1 1 @FUZZ@ \"@OUT@/fuzz\"
oracles^cargo cargo-nextest jq python3 tsp tsanalyze ffprobe^$NATIVE cargo build --release -p tst-interop;;bash scripts/interop/run-matrix.sh --outdir \"@OUT@/oracles\" --seconds 8 --cells 'analyze/*';;jq -e '.cells | length > 0' \"@OUT@/oracles/inventory.json\" >/dev/null || { echo 'run-matrix selected no cell' >&2; exit 1; };;$NATIVE cargo nextest run -p tst-core -E 'binary(mpegts)'
abi^os:Linux cargo nm python3 git jq^bash scripts/check/c/abi-symbols.sh
api^cargo cargo-public-api rg python3 toolchain:@PIN@^bash scripts/dev/public-api-diff.sh @PIN@;;bash scripts/check/rust/api-stability-coverage.sh;;bash scripts/check/repo/deprecations.sh
bench-ir^cargo python3 valgrind git tag:prev^bash scripts/dev/bench-ir.sh \"\$(bash scripts/dev/prev-tag.sh)\" HEAD --out \"@OUT@/bench-ir\"
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

names="$(printf '%s\n' "$PARTS" | awk -F'^' 'NF {print $1}')"
if [ -n "$only" ]; then
  for o in $(printf '%s' "$only" | tr ',' ' '); do
    # A case match, not `printf | grep -q`: under pipefail grep's early exit can fail the pipeline.
    case $'\n'"$names"$'\n' in *$'\n'"$o"$'\n'*) ;; *) echo "unknown part '$o' (parts: $(echo $names))" >&2; exit 2 ;; esac
  done
fi

if [ "$list" = 1 ]; then
  printf '%s\n' "$PARTS" | awk -F'^' 'NF {
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

# missing_tool <requirements>: print the first one that is not met, as a reason.
missing_tool() {
  local t
  for t in $1; do
    case "$t" in
      os:*) [ "$(uname -s)" = "${t#os:}" ] || { echo "needs ${t#os:}"; return 0; } ;;
      tag:prev) [ -n "$(cd "$ROOT" && bash scripts/dev/prev-tag.sh 2>/dev/null)" ] \
        || { echo "no previous release tag; git fetch --tags"; return 0; } ;;
      toolchain:*) cargo "+${t#toolchain:}" --version >/dev/null 2>&1 || { echo "missing $t"; return 0; } ;;
      *) command -v "$t" >/dev/null 2>&1 || { echo "missing $t"; return 0; } ;;
    esac
  done
  return 1
}

ran=0 failed=0 skipnotes=""
while IFS='^' read -r name tools cmds; do
  [ -n "$name" ] || continue
  if [ -n "$only" ]; then
    case ",$only," in *",$name,"*) ;; *) continue ;; esac
  fi
  tools="${tools//@PIN@/$pin}"
  if miss="$(missing_tool "$tools")"; then
    echo "PART $name: NOT RUN ($miss)"
    continue
  fi
  cmds="${cmds//@OUT@/$out}"; cmds="${cmds//@FUZZ@/$FUZZ_SECONDS}"; cmds="${cmds//@PIN@/$pin}"
  rc=0 total=0 skipped=0 reason=""
  while IFS= read -r c; do
    [ -n "$c" ] || continue
    total=$((total + 1))
    echo "+ $c" >> "$out/$name.log"
    rc=0
    (cd "$ROOT" && eval "$c") </dev/null > "$out/.cmd.out" 2>&1 || rc=$?
    cat "$out/.cmd.out" >> "$out/$name.log"
    [ "$rc" -eq 0 ] || break
    line="$(grep -m1 '^SKIP:' "$out/.cmd.out" || true)"
    if [ -n "$line" ]; then skipped=$((skipped + 1)); reason="${line#SKIP: }"; fi
  done <<< "${cmds//;;/$'\n'}"
  if [ "$rc" -ne 0 ]; then
    failed=1; ran=$((ran + 1)); echo "PART $name: rc=$rc"
  elif [ "$skipped" -gt 0 ] && [ "$skipped" -eq "$total" ] && [ "$total" -gt 1 ]; then
    echo "PART $name: NOT RUN ($reason)"; skipnotes="$skipnotes $name ($reason);"
  elif [ "$skipped" -gt 0 ]; then
    ran=$((ran + 1)); echo "PART $name: rc=0, partly run ($reason)"; skipnotes="$skipnotes $name ($reason);"
  else
    ran=$((ran + 1)); echo "PART $name: rc=0"
  fi
done <<< "$PARTS"

[ -z "$skipnotes" ] || echo "review-qualification: skipped checks:${skipnotes%;}"
[ "$ran" -gt 0 ] || { echo "review-qualification: no part ran" >&2; exit 1; }
exit "$failed"
