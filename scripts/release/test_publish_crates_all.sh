#!/usr/bin/env bash
# Self-test for scripts/release/publish-crates-all.sh with `cargo` stubbed on
# PATH: it rehearses the publish job's layers in order, only as dry runs, and
# refuses to publish or to run without layer steps.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ALL="$HERE/publish-crates-all.sh"
[ -f "$ALL" ] || { echo "FAIL: $ALL does not exist" >&2; exit 1; }
T="$(mktemp -d)"
trap 'rm -rf "$T"' EXIT
fail=0
ok() { echo "ok   $1"; }
no() { echo "FAIL $1"; sed 's/^/    /' "$T/out"; fail=1; }

mkdir -p "$T/bin"
printf '{"packages":[' > "$T/meta.json"
sep=""
for p in tstrans-mbedtls-src tstrans-srt-sys tstrans-rist-sys tst-core tst-pipeline tst-udp tst-tcp tst-hls tst-rtp tst-webrtc tst-srt tst-rist; do
  printf '%s{"name":"%s","version":"0.0.1"}' "$sep" "$p" >> "$T/meta.json"; sep=","
done
printf ']}\n' >> "$T/meta.json"
cat > "$T/bin/cargo" <<STUB
#!/bin/sh
echo "\$*" >> "$T/cargo.log"
case "\$1" in metadata) cat "$T/meta.json" ;; esac
STUB
chmod +x "$T/bin/cargo"
run() { : > "$T/cargo.log"; env PATH="$T/bin:$PATH" "$@" bash "$ALL" > "$T/out" 2>&1; }

want_order="tstrans-mbedtls-src tstrans-srt-sys tstrans-rist-sys tst-core tst-pipeline tst-udp tst-tcp tst-hls tst-rtp tst-webrtc tst-srt tst-rist"
if run PUBLISH=0; then
  got="$(sed -n 's/^publish --dry-run -p //p' "$T/cargo.log" | tr '\n' ' ' | sed 's/ $//')"
  [ "$got" = "$want_order" ] && ok "every crate is rehearsed, in the publish job's order" || no "rehearsal order: got [$got]"
  grep -q '^publish -p' "$T/cargo.log" && no "a rehearsal ran a real publish" || ok "a rehearsal never runs a real publish"
  grep -qx '== rehearsed 4 layers' "$T/out" && ok "four layers" || no "layer count"
else
  no "PUBLISH=0 rehearsal exited non-zero"
fi

run PUBLISH=1; rc=$?
[ $rc = 2 ] && grep -qxF 'publish-crates-all.sh rehearses only (PUBLISH=1); the tag publish runs one layer per step with its own token' "$T/out" \
  && ok "PUBLISH=1 is refused" || no "PUBLISH=1: rc=$rc"
[ ! -s "$T/cargo.log" ] && ok "a refused run calls no cargo" || no "a refused run called cargo"

printf 'jobs:\n  publish:\n    steps:\n      - run: echo nothing\n' > "$T/empty.yml"
run PUBLISH=0 CRATES_IO_WORKFLOW="$T/empty.yml"; rc=$?
[ $rc = 2 ] && grep -qxF "no publish-crates-layer.sh layer steps in $T/empty.yml" "$T/out" \
  && ok "a workflow without layer steps is refused" || no "empty workflow: rc=$rc"

[ $fail = 0 ] && echo "test_publish_crates_all: OK"
exit $fail
