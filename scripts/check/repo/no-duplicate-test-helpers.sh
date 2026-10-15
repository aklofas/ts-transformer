#!/usr/bin/env bash
# A test helper is written once. FAIL when the body of a Rust `fn`, a Python
# `def` or a Java `private static` method (the lines after the signature line,
# leading whitespace stripped, at least DTH_MIN_LINES of them) is identical
# in two or more test files: Rust files under a `tests/` directory or named
# `tests.rs`, Python under `tests/`, Java under `src/test/`. Known duplicates
# are listed, with a reason, in scripts/ratchets/duplicate-test-helpers.tsv
# (`file<TAB>name<TAB>reason`); the list is exact in both directions, so a
# row whose duplicate is gone fails too and the list only shrinks. A scan
# that finds no test file fails closed. Linux-only in CI (source scan).
set -euo pipefail
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
  function flush() { if (name != "" && n >= min) { sub(/\n+$/, "\n", body); f[body] = f[body] SUBSEP FILENAME "\t" name } name = "" }
  FNR == 1 { flush() }
  name != "" && py && /^[ \t]*$/ { body = body "\n"; next }
  name != "" && py { match($0, /^[ \t]*/); if (RLENGTH <= ind) flush(); else { b = $0; sub(/^[ \t]+/, "", b); body = body b "\n"; n++; next } }
  name != "" && !py { if ($0 == pad "}") { flush(); next } b = $0; sub(/^[ \t]+/, "", b); body = body b "\n"; n++; next }
  {
    py = (FILENAME ~ /\.py$/)
    if (py && /^[ \t]*(async )?def [A-Za-z0-9_]+\(/) re = "def "
    else if (FILENAME ~ /\.rs$/ && /^[ \t]*(pub(\([a-z]+\))? )?(async )?fn [A-Za-z0-9_]+/ && !/[};][ \t]*$/) re = "fn "
    else if (FILENAME ~ /\.java$/ && /^[ \t]*private static [^=;]*\(/ && !/[};][ \t]*$/) re = "private static "
    else next
    match($0, /^[ \t]*/); ind = RLENGTH; pad = substr($0, 1, RLENGTH)
    name = $0; sub("^.*" re "([^ (]+ )*", "", name); sub(/[<(].*/, "", name); body = ""; n = 0
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
awk -F'\t' 'NF < 3 || $3 == "" { print "FAIL: allowlist row without a reason: " $0; bad = 1 } END { exit bad }' "$tmp/rows" || exit 1
cut -f1,2 "$tmp/rows" | LC_ALL=C sort -u > "$tmp/allowed"
rc=0
while IFS= read -r l; do echo "FAIL: duplicated test helper (say it once, or list it in $ALLOW): $l"; rc=1; done < <(LC_ALL=C comm -23 "$tmp/found" "$tmp/allowed")
while IFS= read -r l; do echo "FAIL: stale allowlist row (no longer duplicated; delete it): $l"; rc=1; done < <(LC_ALL=C comm -13 "$tmp/found" "$tmp/allowed")
[ "$rc" = 0 ] && echo "PASS: no unlisted duplicate test helper ($(wc -l < "$tmp/files") test files scanned)"
exit "$rc"
