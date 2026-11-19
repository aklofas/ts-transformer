#!/usr/bin/env bash
# A caller's URL can carry a password or a `passphrase=`: the harness and the
# examples print it through `tst_core::url::redact_url`, never as given.
#
# Fails on a line of a Rust file under crates/tst-interop/src or examples/
# that formats a variable named `url` or `uri` (`{url}`, `{uri:?}`, or
# `, url` as a plain argument of a formatting macro on the same line) unless that
# line or the one above names `redact_url`.
#
# Does not catch: a macro whose argument sits on a later line than its
# format string, a URL held in another name or a field (`opts.url`), a
# struct printed with `{:?}` that holds one, or an argument that is a call
# (`f(url)`, whose own formatting this does not read).
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
      /\{(url|uri)(:[^}]*)?\}/ || /(format|println|eprintln|print|eprint|write|writeln|panic)!\("[^"]*"(,[^,()]*)*,[ ]*&?(url|uri)[ ]*[,)]/ {
        if ($0 !~ /redact_url/ && prev !~ /redact_url/) printf "%s:%d: %s\n", FILENAME, NR, $0
      }
      { prev = $0 }' "$f")
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
