#!/usr/bin/env bash
# no-process-markers — no internal process vocabulary in the published tree.
#
# A reader of this repository must never meet a work-package, arc, plan, wave,
# sprint, review or pull-request identifier, a maintainer memory-file name, an
# out-of-tree path, or audit/triage/rider/closeout vocabulary used as history.
# Standards citations (ITU-T H.222.0 §2.12.4.2, MISB ST 0601.19 Tag 102),
# other projects' issue links (Haivision/srt#3329) and dependency versions
# (libsrt 1.5.7) are not markers and do not match the patterns below.
#
# Usage:
#   bash scripts/check/repo/no-process-markers.sh                   # every tracked file
#   bash scripts/check/repo/no-process-markers.sh -- <path>...      # only these paths
#   bash scripts/check/repo/no-process-markers.sh --mode fail -- <path>...
#   bash scripts/check/repo/no-process-markers.sh --selftest
# Mode: --mode, else $NO_PROCESS_MARKERS_MODE, else DEFAULT_MODE below.
#   warn = print every hit, exit 0.   fail = print every hit, exit 1.
# Exit: 0 clean (or warn mode); 1 hits in fail mode; 2 usage error or rg missing
#   (fails closed, like the scrub guards in ci.yml).
# Output: one `path:line: text` per hit, then a WARNING:/FAIL:/OK: summary line
#   (the local rails runner copies WARNING:/FAIL: lines into its summary).
# Allowlist: scripts/ratchets/process-markers-allowlist.txt — one entry per
#   line, `<path-regex><TAB><line-regex>`; `*` as the path regex means any path.
#   A hit is suppressed when its path matches the path regex AND the line
#   matches the line regex. Both are ripgrep regexes.
# Excluded: CHANGELOG.md (history), the vendored submodules, target/, this
#   script, its self-test and the allowlist (they spell the patterns out).
# TST_WS names the repository when the script is run from outside it.
# Portable: ripgrep for every match (bash 3.2 syntax only; no mapfile/declare -A).
set -uo pipefail

DEFAULT_MODE=warn

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
}

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -n "${TST_WS:-}" ]; then REPO="$TST_WS"; else REPO="$(cd "$SELF_DIR/../../.." && pwd)"; fi
cd "$REPO" || { echo "no-process-markers: cannot cd to $REPO" >&2; exit 2; }
ALLOWLIST="$REPO/scripts/ratchets/process-markers-allowlist.txt"
[ -f "$ALLOWLIST" ] || ALLOWLIST="$SELF_DIR/process-markers-allowlist.txt"

MODE="${NO_PROCESS_MARKERS_MODE:-$DEFAULT_MODE}"
SELFTEST=0
PATHS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --mode) [ $# -ge 2 ] || { usage; exit 2; }; MODE="$2"; shift 2 ;;
    --selftest) SELFTEST=1; shift ;;
    --) shift; PATHS=("$@"); break ;;
    -h|--help) usage; exit 0 ;;
    *) echo "no-process-markers: unknown argument '$1'" >&2; usage; exit 2 ;;
  esac
done
case "$MODE" in warn|fail) ;; *) echo "no-process-markers: --mode must be warn or fail (got '$MODE')" >&2; exit 2 ;; esac
command -v rg >/dev/null 2>&1 || { echo "no-process-markers: rg (ripgrep) missing - failing closed" >&2; exit 2; }

# The patterns. `(?i)` = case-insensitive. Anchored to the process forms so the
# protocol senses (a PCR phase, a wave of datagrams, Tier 1 platforms) stay.
PATTERNS=(
  '(?i)\bWP-[A-Z0-9]'
  '\bArc [12]\b'
  '(?i)\bplan #[0-9]'
  '(?i)\b(deep )?review #[0-9]'
  '\bR[0-9]+-[0-9]{2}\b'
  '\(#[0-9]{2,3}\)'
  '(?i)\bPR #[0-9]'
  '\b(feedback|reference|project)_[A-Za-z0-9_]+\.md\b'
  '~/Projects'
  '\b(DA|REL|BIND|T[12])-[0-9]+\b'
  '(?i)\b(wave|sprint|tier)[- ][0-9a-z]\b'
  '\bS[0-4]→S[0-4]\b'
  '(?i)\bclose-?out\b'
  '(?i)\brider\b'
  '(?i)\btriage\b'
  '(?i)\baudit\b'
  # process forms found in the tree beyond the ones above
  '\bPhase [0-9]'
  '\bTask [0-9]+\b'
  '\bValidate-1\b'
  '\b(X-)?(CORR|META|RLS-B)-[0-9]{2}\b'
  '\bFinding [0-9]'
  # widened in the C7 fix round (controller Ruling 18): every C task's area
  # had at least one id family or reviewer/process word the patterns above
  # missed (EMB-*, DA-*/REF-*/DEBT-*/NEW-*/CTL-*/PIPE-*/ARCH-*/MUX-*/CFG-*/
  # WS-*/X-CORR-*/RLS-B-*, A-dotted sub-task ids, (K-N) kind-rule ids,
  # letter-suffixed findings, reviewer names, "fix-round", bare-number
  # "Task"/"review" without a "#", and the out-of-tree docs/ subtrees).
  '\b(DA|REF|DEBT|DEMUX|EMB|NEW|CORR|META|CTL|PIPE|ARCH|MUX|CFG|WS|X-CORR|RLS-B)(-[A-Z]+)?-[0-9]+\b'
  '\bA[0-9]\.[0-9]+\b'
  '\(K[0-9]\)'
  '\bFinding [A-Z0-9]\b'
  '(?i)\bCodex\b'
  '(?i)\bCopilot\b'
  '\bfix-round\b'
  '(?i)\bTask [A-Z]?[0-9]'
  'docs/(analysis|plans|specs|validate-1)/'
  '(?i)\b(deep-)?review[- ][0-9]+\b'
  '(?i)\blesson\b'
)
EXCL='^(CHANGELOG\.md$|crates/(srt-sys|mbedtls-src|rist-sys)/vendor/|embedded/vendor/|target/|\.git/|scripts/check/repo/no-process-markers|scripts/ratchets/process-markers-allowlist\.txt$)'

list_files() {
  if [ "${#PATHS[@]}" -gt 0 ]; then printf '%s\n' ${PATHS[@]+"${PATHS[@]}"}; else git ls-files; fi | grep -vE "$EXCL"
}

scan() { # file list on stdin -> `path:line:text` lines (unsorted, one per hit)
  local args=() p files
  for p in "${PATTERNS[@]}"; do args+=(-e "$p"); done
  files=$(cat)
  [ -n "$files" ] || return 0
  printf '%s\n' "$files" | tr '\n' '\0' \
    | xargs -0 rg -nH --no-heading --color never --no-messages "${args[@]}" -- 2>/dev/null || true
}

apply_allowlist() { # hits on stdin -> hits not covered by the allowlist
  local hits pre lre
  hits=$(cat)
  [ -n "$hits" ] || return 0
  if [ -f "$ALLOWLIST" ]; then
    while IFS="$(printf '\t')" read -r pre lre; do
      case "$pre" in ''|'#'*) continue ;; esac
      [ -n "$lre" ] || continue
      [ "$pre" = '*' ] && pre='[^:]*'
      hits=$(printf '%s\n' "$hits" | rg -v -e "^(${pre}):[0-9]+:.*(${lre})" || true)
      [ -n "$hits" ] || break
    done < "$ALLOWLIST"
  fi
  [ -n "$hits" ] && printf '%s\n' "$hits"
  return 0
}

run_scan() { # $1 = mode; prints hits and a summary; returns 0/1
  local mode="$1" files hits n
  files=$(list_files)
  n=$(printf '%s\n' "$files" | grep -c . || true)
  hits=$(printf '%s\n' "$files" | scan | apply_allowlist)
  if [ -n "$hits" ]; then
    printf '%s\n' "$hits" | sed 's/^\([^:]*:[0-9]*\):[[:space:]]*/\1: /'
    local c; c=$(printf '%s\n' "$hits" | grep -c . || true)
    if [ "$mode" = fail ]; then
      echo "FAIL: no-process-markers: $c hit(s) in $n file(s)"; return 1
    fi
    echo "WARNING: no-process-markers: $c hit(s) in $n file(s) (warn mode)"; return 0
  fi
  echo "OK: no process markers in $n file(s)"; return 0
}

selftest() {
  local tmp pos neg out rc i n fails
  tmp=$(mktemp -d) || exit 2
  pos="$tmp/positive.txt"; neg="$tmp/negative.txt"; fails=0
  printf '%s\n' \
    'see WP-3 for the fix' \
    'landed in Arc 2' \
    'per plan #94' \
    'deep review #7 found it' \
    'finding R9-01' \
    'fixed (#287)' \
    'see PR #122' \
    'see feedback_cross_platform_paths.md' \
    'at ~/Projects/ts-transformer' \
    'DA-NET-9 and REL-01' \
    'BIND-01 and T2-3' \
    'Wave 6 and Sprint 3' \
    'the S0→S4 arc' \
    'closeout note' \
    'close-out note' \
    'a rider on the arc' \
    'triage later' \
    'the audit found' \
    'Phase 7 wheels' \
    'Task 14 wires it' \
    'Validate-1 again' \
    'CORR-01 and X-META-02 and RLS-B09' \
    'Finding 5 closed' \
    'the WP-SAN-5 rail' \
    'the EMB-JOIN-1 regression' \
    'the DEMUX-01 bug' \
    'the fix lives in A5.2' \
    'kind rule (K3)' \
    'Finding B needs it' \
    'Codex flagged it' \
    'Copilot flagged it' \
    'the fix-round review' \
    'task a2 covers it' \
    'see docs/plans/2026-01-01-x.md' \
    'review-9 flagged it' \
    'deep-review-10 flagged it' \
    'a hard lesson here' \
    > "$pos"
  printf '%s\n' \
    'Tier 1 platforms are gating' \
    'Tier B golden outputs' \
    'the PCR phase is re-anchored' \
    'a wave of datagrams' \
    'ITU-T H.222.0 §2.12.4.2' \
    'MISB ST 0601.19 Tag 102' \
    'libsrt 1.5.7 and librist 0.2.20' \
    'Haivision/srt#3329 and google/oss-fuzz#16214' \
    'RFC 7826 §18.49' \
    'tests/symbol_audit.rs runs nm -D' \
    'MPEG-2 AC-3 AAC-LC' \
    'ABI version 0.22' \
    > "$neg"
  n=$(grep -c . "$pos")
  out=$(TST_WS="$REPO" bash "$0" --mode fail -- "$pos"); rc=$?
  [ "$rc" -eq 1 ] || { echo "selftest FAIL: positive file exited $rc, want 1"; fails=1; }
  i=1
  while [ "$i" -le "$n" ]; do
    printf '%s\n' "$out" | grep -q "positive.txt:$i: " || { echo "selftest FAIL: planted marker on line $i not detected: $(sed -n "${i}p" "$pos")"; fails=1; }
    i=$((i + 1))
  done
  out=$(TST_WS="$REPO" bash "$0" --mode fail -- "$neg"); rc=$?
  [ "$rc" -eq 0 ] || { echo "selftest FAIL: negative file exited $rc, want 0"; printf '%s\n' "$out" | grep 'negative.txt:'; fails=1; }
  rm -rf "$tmp"
  if [ "$fails" -ne 0 ]; then echo "FAIL: no-process-markers self-test"; return 1; fi
  echo "OK: no-process-markers self-test: $n planted markers detected, 0 false positives"
}

if [ "$SELFTEST" -eq 1 ]; then selftest; exit $?; fi
run_scan "$MODE"
