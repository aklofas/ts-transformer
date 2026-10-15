#!/usr/bin/env bash
# A test helper is written once. FAIL when the body of a Rust `fn`, a Python
# `def` or a Java `private static` method is identical in two or more test
# files: Rust files under a `tests/` directory or named `tests.rs`, Python
# under `tests/`, Java under `src/test/`.
#
# What is compared: the lines after the signature line, up to the closing
# brace at the signature's indentation (Python: up to the next line at or left
# of the `def`). A signature that spans several lines contributes its
# parameter lines to the body, so two helpers match only when their parameter
# lists match too. Before hashing, each body line loses its leading and
# trailing whitespace; blank lines and comment-only lines (`//`, and `#` in
# Python) are dropped, so a copy that only differs in comments is still a
# copy. The name is not compared. A body needs at least DTH_MIN_LINES (4)
# such lines. Two copies in one file are not a cross-file finding.
#
# Not scanned: functions carrying a `#[test]` attribute (also `#[x::test]` and
# `#[cfg_attr(..., test)]`) directly above them, and Python `def test_*`,
# because the rail targets helpers, not repeated test bodies; and inline `#[cfg(test)]` modules inside
# production `src/*.rs` files.
#
# Known duplicates are listed, with a reason, in
# scripts/ratchets/duplicate-test-helpers.tsv (`file<TAB>name<TAB>reason`).
# The list is exact in both directions: a row whose duplicate is gone fails
# too, so the list only shrinks unless a row carries a reason. Regenerate it
# with `--print-allowlist`, which prints the current duplicates in the rail's
# order and keeps the reason of every row that already exists. A scan that
# finds no test file fails closed. Linux-only in CI (source scan).
set -euo pipefail
mode=check
if [ "${1:-}" = "--print-allowlist" ]; then mode=print; fi
root="$(git rev-parse --show-toplevel)"; cd "$root"
ROOTS="${DTH_ROOTS:-crates bindings}"
ALLOW="${DTH_ALLOWLIST:-scripts/ratchets/duplicate-test-helpers.tsv}"
tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
# shellcheck disable=SC2086  # ROOTS is a word list on purpose
find $ROOTS \( -name vendor -o -name target -o -name fuzz -o -name .venv -o -name node_modules \) -prune -o -type f \
  \( -path '*/tests/*.rs' -o -name tests.rs -o -path '*/tests/*.py' -o -path '*/src/test/*.java' \) -print | LC_ALL=C sort > "$tmp/files"
[ -s "$tmp/files" ] || { echo "FAIL: no test files under: $ROOTS (scan drifted)"; exit 1; }
# One awk process must see every file (groups span files): pass the list as
# arguments, not through xargs batching. ~30 KB of paths, far under ARG_MAX.
# shellcheck disable=SC2046
awk -v min="${DTH_MIN_LINES:-4}" '
  function flush() { if (name != "" && n >= min && !skip) { f[body] = f[body] SUBSEP FILENAME "\t" name } name = "" }
  function add(line,   b) {
    b = line; sub(/^[ \t]+/, "", b); sub(/[ \t\r]+$/, "", b)
    if (b == "" || b ~ /^\/\// || (py && b ~ /^#/)) return
    body = body b "\n"; n++
  }
  function is_test_attr(a) {
    return a ~ /^[ \t]*#\[([A-Za-z0-9_]+::)*test([(\]])/ || a ~ /^[ \t]*#\[cfg_attr\(.*,[ \t]*([A-Za-z0-9_]+::)*test[ \t]*[,)]/
  }
  FNR == 1 { flush(); testattr = 0 }
  name != "" && py && /^[ \t]*$/ { next }
  name != "" && py { match($0, /^[ \t]*/); if (RLENGTH <= ind) flush(); else { add($0); next } }
  name != "" && !py { if ($0 == pad "}") { flush(); next } add($0); next }
  {
    py = (FILENAME ~ /\.py$/)
    if (py && /^[ \t]*(async )?def [A-Za-z0-9_]+\(/) re = "def "
    else if (FILENAME ~ /\.rs$/ && /^[ \t]*(pub(\([a-z]+\))? )?(async )?fn [A-Za-z0-9_]+/ && !/[};][ \t]*$/) re = "fn "
    else if (FILENAME ~ /\.java$/ && /^[ \t]*private static [^=;]*\(/ && !/[};][ \t]*$/) re = "private static "
    else {
      # Track `#[test]`-style attributes sitting directly above a signature.
      if (is_test_attr($0)) testattr = 1
      else if ($0 !~ /^[ \t]*(#\[|\/\/\/)/) testattr = 0
      next
    }
    skip = testattr; testattr = 0
    match($0, /^[ \t]*/); ind = RLENGTH; pad = substr($0, 1, RLENGTH)
    name = $0; sub("^.*" re "([^ (]+ )*", "", name); sub(/[<(].*/, "", name); body = ""; n = 0
    if (py && name ~ /^test_/) skip = 1
  }
  END {
    flush()
    for (k in f) {
      m = split(substr(f[k], 2), e, SUBSEP); split(e[1], a, "\t"); dup = 0
      for (i = 2; i <= m; i++) { split(e[i], c, "\t"); if (c[1] != a[1]) dup = 1 }
      if (dup) for (i = 1; i <= m; i++) print e[i]
    }
  }' $(cat "$tmp/files") | LC_ALL=C sort -u > "$tmp/found"
{ grep -v '^#' "$ALLOW" 2>/dev/null || true; } | { grep -v '^$' || true; } > "$tmp/rows"
if [ "$mode" = print ]; then
  awk -F'\t' 'FILENAME == ARGV[1] { r[$1 "\t" $2] = $3; next } { print $0 "\t" r[$1 "\t" $2] }' "$tmp/rows" "$tmp/found"
  exit 0
fi
awk -F'\t' 'NF < 3 || $3 == "" { print "FAIL: allowlist row without a reason: " $0; bad = 1 } END { exit bad }' "$tmp/rows" || exit 1
cut -f1,2 "$tmp/rows" | LC_ALL=C sort -u > "$tmp/allowed"
rc=0
while IFS= read -r l; do echo "FAIL: duplicated test helper (say it once, or list it with a reason in $ALLOW; regenerate with --print-allowlist): $l"; rc=1; done < <(LC_ALL=C comm -23 "$tmp/found" "$tmp/allowed")
while IFS= read -r l; do echo "FAIL: stale allowlist row (no longer duplicated; delete it, or regenerate with --print-allowlist): $l"; rc=1; done < <(LC_ALL=C comm -13 "$tmp/found" "$tmp/allowed")
[ "$rc" = 0 ] && echo "PASS: no unlisted duplicate test helper ($(wc -l < "$tmp/files") test files scanned)"
exit "$rc"
