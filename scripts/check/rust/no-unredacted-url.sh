#!/usr/bin/env bash
# A caller's URL can carry a password or a `passphrase=`: the harness and the
# examples print it through `tst_core::url::redact_url`, never as given.
#
# Fails on a line of a Rust file under crates/tst-interop/src or examples/
# that formats a variable named `url` or `uri`: `{url}` or `{uri:?}` inline,
# or `, url` as a plain argument after the format string of format!, println!,
# eprintln!, print!, eprint! or panic!, unless THAT SAME LINE names
# `redact_url` (e.g. `format!("{url}", url = redact_url(url))`). A line under a
# `let shown = redact_url(url);` is not excused: print `{shown}`.
#
# Does not catch: a named argument (`"{u}", u = url`), `write!`/`writeln!`
# (their first argument is not a string literal, so they are not matched at
# all), a macro whose arguments sit on a later line than its format string, a
# URL held under another name or a field (`opts.url`, `push_url`, `url.as_str()`),
# a struct printed with `{:?}` that holds one, an argument that is a call
# (`f(url)`), or a line that redacts one value and prints `{url}` raw beside it.
#
# Overridable (scripts/ratchets/tests/self_test.sh points it at a fixture):
#   REDACT_SCAN_DIRS   space-separated dirs scanned
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
REDACT_SCAN_DIRS="${REDACT_SCAN_DIRS:-$ROOT/crates/tst-interop/src $ROOT/examples}"

found=0
for dir in $REDACT_SCAN_DIRS; do
  [ -d "$dir" ] || continue
  while IFS= read -r -d '' f; do
    hits=$(awk '
      /\{(url|uri)(:[^}]*)?\}/ || /(format|println|eprintln|print|eprint|panic)!\("[^"]*"(,[^,()]*)*,[ ]*&?(url|uri)[ ]*[,)]/ {
        if ($0 !~ /redact_url/) printf "%s:%d: %s\n", FILENAME, NR, $0
      }
      ' "$f")
    if [ -n "$hits" ]; then
      echo "$hits"
      found=1
    fi
  done < <(find "$dir" -name '*.rs' -type f -print0)
done

if [ "$found" -ne 0 ]; then
  echo "FAIL: a URL is formatted without redact_url (see the header of scripts/check/rust/no-unredacted-url.sh)" >&2
  exit 1
fi
echo "no-unredacted-url: OK"
