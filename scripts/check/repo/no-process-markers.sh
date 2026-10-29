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
# Exit: 0 clean (or warn mode); 1 hits in fail mode, or a stale allowlist row
#   on a whole-tree scan; 2 usage error, rg missing or failing (an unreadable
#   path), or an unreadable or malformed allowlist (fails closed, like the
#   scrub guards in ci.yml).
# Output: one `path:line: text` per hit, then a WARNING:/FAIL:/OK: summary line
#   (the local rails runner copies WARNING:/FAIL: lines into its summary).
# Allowlist: scripts/ratchets/process-markers-allowlist.txt — one entry per
#   line, `<path-regex><TAB><line-regex>`; `*` as the path regex means any path.
#   A hit is suppressed when its path matches the path regex AND the line
#   matches the line regex. Both are ripgrep regexes. On a whole-tree scan a
#   row that suppresses no hit is stale and fails the rail.
#   $NO_PROCESS_MARKERS_ALLOWLIST names another allowlist file.
# Excluded: CHANGELOG.md (history), the vendored submodules, target/, this
#   script, its self-test and the allowlist (they spell the patterns out).
# TST_WS names the repository when the script is run from outside it.
# Portable: ripgrep for every match (bash 3.2 syntax only; no mapfile/declare -A).
set -uo pipefail

DEFAULT_MODE=fail

usage() {
  sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
}

SELF_DIR="$(cd "$(dirname "$0")" && pwd)"
if [ -n "${TST_WS:-}" ]; then REPO="$TST_WS"; else REPO="$(cd "$SELF_DIR/../../.." && pwd)"; fi
cd "$REPO" || { echo "no-process-markers: cannot cd to $REPO" >&2; exit 2; }
if [ -n "${NO_PROCESS_MARKERS_ALLOWLIST:-}" ]; then
  ALLOWLIST="$NO_PROCESS_MARKERS_ALLOWLIST"
else
  ALLOWLIST="$REPO/scripts/ratchets/process-markers-allowlist.txt"
  [ -f "$ALLOWLIST" ] || ALLOWLIST="$SELF_DIR/process-markers-allowlist.txt"
fi

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
  # "this arc" / "the arc" as a project-arc name. Lower-case "arc" only, so
  # Rust's `Arc` stays; not followed by " of", so "the arc of a trajectory"
  # stays (ripgrep's default engine has no lookahead, hence the alternation).
  '\b([Tt]his|[Tt]he) arc\b($|[^ ]| [^o]| o[^f])'
  # out-of-tree maintainer files. A name right after `!` (an rg/glob
  # exclusion) or a leading `/` (an ignore-file root entry) is not a pointer.
  '(^|[^!/])CLAUDE\.md|\S/CLAUDE\.md|ROADMAP\.md|soak-archives/'
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
  # rg exits 1 for "no match" and 2 for an error (an unreadable file, a bad
  # pattern). The wrapper maps 0/1 to 0 and an error to 255, which makes xargs
  # stop and exit non-zero, so an error is never read as a clean file.
  printf '%s\n' "$files" | tr '\n' '\0' \
    | xargs -0 sh -c 'rg "$@"; rc=$?; [ "$rc" -le 1 ] && exit 0; exit 255' rg \
        -nH --no-heading --color never "${args[@]}" --
}

apply_allowlist() { # hits on stdin -> hits not covered by the allowlist
  # Returns 0, or 1 when a row is stale on a whole-tree scan (reported on
  # stderr), or 2 when the allowlist is unreadable or a row is malformed.
  local hits all pre lre row n=0 stale=0 tab
  tab="$(printf '\t')"
  hits=$(cat)
  all="$hits"
  [ -r "$ALLOWLIST" ] || { echo "FAIL: no-process-markers: cannot read the allowlist $ALLOWLIST" >&2; return 2; }
  while IFS= read -r row || [ -n "$row" ]; do
    n=$((n + 1))
    case "$row" in ''|'#'*) continue ;; esac
    case "$row" in *"$tab"*) ;; *) echo "FAIL: no-process-markers: allowlist row $n is not <path-regex><TAB><line-regex>: $row" >&2; return 2 ;; esac
    pre="${row%%"$tab"*}"; lre="${row#*"$tab"}"
    [ -n "$pre" ] && [ -n "$lre" ] || { echo "FAIL: no-process-markers: allowlist row $n is not <path-regex><TAB><line-regex>: $row" >&2; return 2; }
    [ "$pre" = '*' ] && pre='[^:]*'
    rg -q -e "$pre" /dev/null 2>/dev/null; [ $? -eq 2 ] && { echo "FAIL: no-process-markers: allowlist row $n has a bad regex: $row" >&2; return 2; }
    rg -q -e "$lre" /dev/null 2>/dev/null; [ $? -eq 2 ] && { echo "FAIL: no-process-markers: allowlist row $n has a bad regex: $row" >&2; return 2; }
    if [ "${#PATHS[@]}" -eq 0 ] && ! printf '%s\n' "$all" | rg -q -e "^(${pre}):[0-9]+:.*(${lre})"; then
      echo "FAIL: stale allowlist row $n: $row" >&2; stale=1
    fi
    [ -n "$hits" ] && hits=$(printf '%s\n' "$hits" | rg -v -e "^(${pre}):[0-9]+:.*(${lre})" || true)
  done < "$ALLOWLIST"
  [ -n "$hits" ] && printf '%s\n' "$hits"
  return "$stale"
}

run_scan() { # $1 = mode; prints hits and a summary; returns 0/1/2
  local mode="$1" files raw hits n arc
  files=$(list_files)
  n=$(printf '%s\n' "$files" | grep -c . || true)
  raw=$(printf '%s\n' "$files" | scan) || { echo "FAIL: no-process-markers: rg failed"; return 2; }
  hits=$(printf '%s\n' "$raw" | apply_allowlist); arc=$?
  [ "$arc" -eq 2 ] && return 2
  if [ -n "$hits" ]; then
    printf '%s\n' "$hits" | sed 's/^\([^:]*:[0-9]*\):[[:space:]]*/\1: /'
    local c; c=$(printf '%s\n' "$hits" | grep -c . || true)
    if [ "$mode" = fail ]; then
      echo "FAIL: no-process-markers: $c hit(s) in $n file(s)"; return 1
    fi
    echo "WARNING: no-process-markers: $c hit(s) in $n file(s) (warn mode)"
  fi
  if [ "$arc" -ne 0 ]; then echo "FAIL: no-process-markers: stale allowlist row(s)"; return 1; fi
  [ -n "$hits" ] && return 0
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
    'fixed in this arc' \
    'see CLAUDE.md for the recipe' \
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
    'an arc of the trajectory' \
    'the arc of the trajectory' \
    'the Arc clone shares the buffer' \
    'arcsine and arctan' \
    '/CLAUDE.md' \
    "--glob '!CLAUDE.md'" \
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
  # Fail closed: a malformed allowlist row, an rg error and a stale row each
  # fail the rail rather than passing it.
  printf '*\t(\n' > "$tmp/bad-regex.txt"
  out=$(NO_PROCESS_MARKERS_ALLOWLIST="$tmp/bad-regex.txt" TST_WS="$REPO" bash "$0" --mode fail -- "$neg" 2>&1); rc=$?
  [ "$rc" -eq 2 ] || { echo "selftest FAIL: an allowlist row with a bad regex exited $rc, want 2"; fails=1; }
  printf '%s\n' "$out" | grep -qF 'allowlist row 1 has a bad regex' || { echo "selftest FAIL: bad-regex message missing: $out"; fails=1; }
  printf '*\n' > "$tmp/no-tab.txt"
  out=$(NO_PROCESS_MARKERS_ALLOWLIST="$tmp/no-tab.txt" TST_WS="$REPO" bash "$0" --mode fail -- "$neg" 2>&1); rc=$?
  [ "$rc" -eq 2 ] || { echo "selftest FAIL: an allowlist row without a line regex exited $rc, want 2"; fails=1; }
  out=$(NO_PROCESS_MARKERS_ALLOWLIST="$tmp/missing.txt" TST_WS="$REPO" bash "$0" --mode fail -- "$neg" 2>&1); rc=$?
  [ "$rc" -eq 2 ] || { echo "selftest FAIL: a missing allowlist exited $rc, want 2"; fails=1; }
  # chmod 000 does not stop root from reading the file, so the case cannot
  # fail closed there.
  if [ "$(id -u)" -eq 0 ]; then
    echo "SKIP: no-process-markers self-test: the unreadable-path case needs a non-root user"
  else
    printf 'clean\n' > "$tmp/unreadable.txt"; chmod 000 "$tmp/unreadable.txt"
    out=$(TST_WS="$REPO" bash "$0" --mode fail -- "$tmp/unreadable.txt" 2>&1); rc=$?
    chmod 600 "$tmp/unreadable.txt"
    [ "$rc" -eq 2 ] || { echo "selftest FAIL: an unreadable path exited $rc, want 2"; fails=1; }
    printf '%s\n' "$out" | grep -qF 'FAIL: no-process-markers: rg failed' || { echo "selftest FAIL: rg-failure message missing: $out"; fails=1; }
  fi
  mkdir "$tmp/ws"; git -C "$tmp/ws" init -q; printf 'clean text\n' > "$tmp/ws/a.txt"; git -C "$tmp/ws" add a.txt
  printf '*\tneverappearsanywhere123\n' > "$tmp/stale.txt"
  out=$(NO_PROCESS_MARKERS_ALLOWLIST="$tmp/stale.txt" TST_WS="$tmp/ws" bash "$0" --mode fail 2>&1); rc=$?
  [ "$rc" -eq 1 ] || { echo "selftest FAIL: a stale allowlist row exited $rc, want 1"; fails=1; }
  printf '%s\n' "$out" | grep -qF 'FAIL: stale allowlist row 1: *' || { echo "selftest FAIL: stale-row message missing: $out"; fails=1; }
  rm -rf "$tmp"
  if [ "$fails" -ne 0 ]; then echo "FAIL: no-process-markers self-test"; return 1; fi
  echo "OK: no-process-markers self-test: $n planted markers detected, 0 false positives"
}

if [ "$SELFTEST" -eq 1 ]; then selftest; exit $?; fi
run_scan "$MODE"
