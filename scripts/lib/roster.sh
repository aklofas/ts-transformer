#!/usr/bin/env bash
# The crate roster, read from `cargo metadata` instead of being written down
# in each rail. See `usage` below for the lists. The one fact Cargo has no
# field for — which sanitizer job covers a member — is the SANITIZE line.
#
# Overridable: ROSTER_ROOT (workspace root), ROSTER_METADATA (a saved
# `cargo metadata --no-deps --format-version 1` JSON, for tests).
set -euo pipefail
ROOT="${ROSTER_ROOT:-$(cd "$(dirname "$0")/../.." && pwd)}"

# Every workspace member, exactly once: pure | native | none.
SANITIZE="tst-core:pure tst-pipeline:pure tst-rtp:pure tst-udp:pure tst-tcp:pure tst-hls:pure tst-srt:native tst-rist:native tst-c:native tst-c-core:none tstrans-srt-sys:none tstrans-rist-sys:none tstrans-mbedtls-src:none tst-py:none tst-jni:none tst-integration:none tst-interop:none tst-test-helpers:none tst-examples:none"

usage="usage: roster.sh members|manifests|publishable|public-api|api-stability|surface|dep-tomls|publish-deps|test-targets|sanitize-pure|sanitize-native|check-sanitize"
list="${1:?$usage}"
case "$list" in
  sanitize-pure|sanitize-native)
    for e in $SANITIZE; do [ "${e#*:}" = "${list#sanitize-}" ] && echo "${e%%:*}"; done; exit 0 ;;
  members|manifests|publishable|public-api|api-stability|surface|dep-tomls|publish-deps|test-targets|check-sanitize) ;;
  *) echo "$usage" >&2; exit 1 ;;
esac
if [ -n "${ROSTER_METADATA:-}" ]; then meta="$(cat "$ROSTER_METADATA")"
else meta="$(cd "$ROOT" && cargo metadata --no-deps --format-version 1)"; fi

printf '%s' "$meta" | ROSTER_LIST="$list" ROSTER_ROOT="$ROOT" ROSTER_SANITIZE="$SANITIZE" python3 -c '
import json, os, sys
m = json.load(sys.stdin)
root, want = os.environ["ROSTER_ROOT"], os.environ["ROSTER_LIST"]
ids = set(m["workspace_members"])
pk = sorted((p for p in m["packages"] if p["id"] in ids), key=lambda p: p["name"])
if not pk:
    sys.exit("roster: cargo metadata lists no workspace members")
names = {p["name"] for p in pk}
rel = lambda p: os.path.relpath(os.path.dirname(p["manifest_path"]), m["workspace_root"])
api = [p for p in pk if os.path.isfile(os.path.join(root, rel(p), "public-api.txt"))]
lib = lambda p: next(t["name"] for t in p["targets"] if "lib" in t["kind"] or "rlib" in t["kind"])
internal = lambda p, kinds: sorted({d["name"] for d in p["dependencies"] if d["name"] in names and d.get("kind") in kinds})
out = {
    "members": [p["name"] for p in pk],
    "manifests": [p["manifest_path"] for p in pk],
    "publishable": [p["name"] for p in pk if p["publish"] is None],
    "public-api": ["%s:%s" % (p["name"], rel(p)) for p in api],
    "api-stability": ["%s:%s:%s" % (p["name"], rel(p), lib(p)) for p in api],
    "surface": [os.path.basename(rel(p)) for p in api],
    "dep-tomls": [p["manifest_path"] for p in pk if any(d.get("path") and d["req"] != "*" for d in p["dependencies"])],
    "publish-deps": [" ".join([p["name"]] + internal(p, (None, "build"))) for p in pk if p["publish"] is None],
    "test-targets": ["%s\t%s\t%s" % (p["name"], t["name"], t["src_path"]) for p in pk for t in p["targets"] if t["kind"] == ["test"]],
}
if want == "check-sanitize":
    have = [e.split(":")[0] for e in os.environ["ROSTER_SANITIZE"].split()]
    bad = sorted(names - set(have)) + sorted(set(have) - names) + sorted({h for h in have if have.count(h) > 1})
    if bad:
        sys.exit("roster: SANITIZE must name every member exactly once; fix: " + " ".join(bad))
    sys.exit(0)
rows = out[want]
if not rows:
    sys.exit("roster: list %s is empty" % want)
print("\n".join(rows))
'
