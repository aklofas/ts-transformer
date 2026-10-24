#!/usr/bin/env bash
# The release VM's corpus phase on one frozen commit: check it out clean,
# build tst-interop in release mode, sync the private corpus and its
# annotations from the bucket, then inventory -> sweep -> replay -> report
# and push the run's digests as the commit's unreleased baseline. The
# stages, their order and their `<stage> rc=N` lines are the ones every
# release run since 0.9.0 was judged by; this file replaces the script
# that was rewritten by hand on the VM for each attempt.
#
# Usage:
#   bash release-vm-corpus.sh <full 40-hex sha> [--baseline DIR]
#       [--accept SHA256 --because TEXT]... [--run-dir DIR] [--dry-run]
#
#   --baseline DIR      the previous release's digests to compare with
#                       (`corpus sweep --baseline`); a first run has none
#   --accept SHA256 --because TEXT
#                       a reviewed change to accept, passed through to the
#                       sweep verbatim (repeatable, always in pairs)
#   --run-dir DIR       the run directory (default ~/corpus-run-<sha12>);
#                       an existing one is moved aside as DIR-prev-<UTC>
#   --dry-run           print every stage's command instead of running it
#
# Environment:
#   TSTRANS_REPO_DIR        the checkout (default ~/ts-transformer)
#   TSTRANS_CORPUS_BUCKET   the bucket (default tstrans-corpus): captures
#                           under their own prefixes at the root,
#                           `annotations/` and `baselines/` beside them,
#                           `quarantine/` never synced
#   TSTRANS_CORPUS_DIR      local corpus root (default ~/corpus; captures
#                           in captures/, annotations in annotations/)
#
# Exit status: the first failing stage's, after `CORPUS-DONE rc=N` (0 =
# every stage passed and the digests were pushed). The soak is not
# launched here: release-vm-soak.sh does that once CI is green on the sha.
#
# Validated on Ubuntu 24.04 x86_64 (the release VM); see lib.sh for this
# directory's portability stance.
set -uo pipefail

SHA=""
BASELINE=""
RUN_DIR=""
DRY_RUN=0
SWEEP_EXTRA=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    --baseline) BASELINE=$2; shift 2 ;;
    --run-dir) RUN_DIR=$2; shift 2 ;;
    --accept)
      [[ $# -ge 4 && "$3" == "--because" ]] ||
        { echo "release-vm-corpus.sh: --accept SHA256 must be followed by --because TEXT" >&2; exit 2; }
      SWEEP_EXTRA+=(--accept "$2" --because "$4")
      shift 4
      ;;
    --dry-run) DRY_RUN=1; shift ;;
    -h | --help)
      awk 'NR >= 2 { if ($0 !~ /^#/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*) echo "release-vm-corpus.sh: unknown argument: $1" >&2; exit 2 ;;
    *)
      [[ -z "$SHA" ]] || { echo "release-vm-corpus.sh: one sha only (got '$SHA' and '$1')" >&2; exit 2; }
      SHA=$1; shift
      ;;
  esac
done
[[ ${#SHA} -eq 40 ]] || { echo "release-vm-corpus.sh: need the full 40-hex sha (got '$SHA')" >&2; exit 2; }
[[ -z "$BASELINE" || -d "$BASELINE" || "$DRY_RUN" -eq 1 ]] ||
  { echo "release-vm-corpus.sh: --baseline $BASELINE is not a directory" >&2; exit 2; }

REPO_DIR=${TSTRANS_REPO_DIR:-$HOME/ts-transformer}
BUCKET=${TSTRANS_CORPUS_BUCKET:-tstrans-corpus}
CORPUS_DIR=${TSTRANS_CORPUS_DIR:-$HOME/corpus}
CAPTURES=$CORPUS_DIR/captures
ANNOTATIONS=$CORPUS_DIR/annotations
[[ -n "$RUN_DIR" ]] || RUN_DIR=$HOME/corpus-run-${SHA:0:12}
BIN=$REPO_DIR/target/release/tst-interop
export PATH=$HOME/.cargo/bin:/snap/bin:$PATH

# run <stage> <cmd...> — one stage: print the command under --dry-run,
# else run it and echo `<stage> rc=N`. The caller decides whether a
# failure ends the phase (every stage here does).
run() {
  local stage=$1 rc=0; shift
  if [[ "$DRY_RUN" -eq 1 ]]; then
    printf '%s: ' "$stage"; printf '%q ' "$@"; echo
    return 0
  fi
  "$@" || rc=$?
  echo "$stage rc=$rc"
  return "$rc"
}
finish() { echo "CORPUS-DONE rc=$1"; exit "$1"; }

# retry <n> <cmd...>: GitHub and the submodule hosts have refused
# connections in runs before; nothing here is worth failing on one.
retry() {
  local n=$1 i; shift
  for ((i = 1; i <= n; i++)); do
    "$@" && return 0
    sleep 30
  done
  return 1
}

# 1. The tree, clean, at the sha.
checkout() {
  if [[ "$DRY_RUN" -eq 1 ]]; then
    echo "checkout: (cd $REPO_DIR && git fetch origin && git checkout $SHA && git submodule update --init --recursive && git status --short)"
    return 0
  fi
  cd "$REPO_DIR" || return 2
  retry 5 git fetch -q origin || return $?
  git checkout -q "$SHA" || return $?
  retry 5 git submodule update --init --recursive || return $?
  local dirty
  dirty=$(git status --short)
  if [[ -n "$dirty" ]]; then
    echo "release-vm-corpus.sh: the checkout is not clean:" >&2
    echo "$dirty" >&2
    return 3
  fi
  echo "HEAD=$(git rev-parse HEAD)"
}
checkout || finish $?
[[ "$DRY_RUN" -eq 1 ]] || echo "checkout rc=0"

# 2. The release binary. Vendored libsrt/librist: the evidence is of
# the trees this repo ships, not of whatever the host might have.
run build env SRT_FORCE_VENDORED=1 RIST_FORCE_VENDORED=1 cargo build --release --manifest-path "$REPO_DIR/Cargo.toml" -p tst-interop || finish $?

# 3. The corpus. Captures are every .ts under the bucket's capture
# prefixes; quarantine/ holds files the maintainer pulled from the run
# (broken PCR, derived files) and is never synced. Annotations are the
# curated per-capture notes the sweep's rules consult.
run sync-captures aws s3 sync "s3://$BUCKET/" "$CAPTURES/" --only-show-errors \
  --exclude '*' --include '*.ts' --exclude 'quarantine/*' --exclude 'annotations/*' --exclude 'baselines/*' || finish $?
run sync-annotations aws s3 sync "s3://$BUCKET/annotations/" "$ANNOTATIONS/" --only-show-errors || finish $?
if [[ "$DRY_RUN" -eq 0 ]]; then
  echo "captures=$(find "$CAPTURES" -name '*.ts' | wc -l) annotations=$(find "$ANNOTATIONS" -name '*.toml' 2>/dev/null | wc -l)"
  if [[ -e "$RUN_DIR" ]]; then
    prev="$RUN_DIR-prev-$(date -u +%Y%m%dT%H%M%SZ)"
    mv "$RUN_DIR" "$prev" && echo "previous run dir moved to $prev"
  fi
fi

# 4. Inventory -> sweep -> replay -> report. The sweep and the replay
# exit 1 on a failed verdict; the report describes whatever happened and
# exits 0, so it runs even after a failure and the archive row is there
# to read.
run inventory "$BIN" corpus inventory "$CAPTURES" --out "$RUN_DIR" || finish $?
sweep_args=(--out "$RUN_DIR" --annotations "$ANNOTATIONS")
[[ -z "$BASELINE" ]] || sweep_args+=(--baseline "$BASELINE")
sweep_args+=("${SWEEP_EXTRA[@]}")
sweep_rc=0
run sweep "$BIN" corpus sweep "$CAPTURES" "${sweep_args[@]}" || sweep_rc=$?
replay_rc=0
if [[ $sweep_rc -eq 0 ]]; then
  run replay "$BIN" corpus replay "$CAPTURES" --out "$RUN_DIR" || replay_rc=$?
fi
run report "$BIN" corpus report "$RUN_DIR" --markdown "$RUN_DIR/report.md" --archive-row "$RUN_DIR/archive-row.json"
[[ "$DRY_RUN" -eq 1 || ! -f "$RUN_DIR/archive-row.json" ]] || cat "$RUN_DIR/archive-row.json"
[[ $sweep_rc -eq 0 ]] || finish "$sweep_rc"
[[ $replay_rc -eq 0 ]] || finish "$replay_rc"

# 5. The digests become the commit's unreleased baseline; the release
# promotes them under the tag's name.
run push-digests aws s3 sync "$RUN_DIR/digests/" "s3://$BUCKET/baselines/unreleased-$SHA/" --only-show-errors || finish $?
finish 0
