#!/usr/bin/env bash
# Self-test of scripts/dev/rustsec-report.sh against a stub `cargo` that prints
# cargo-deny's JSON diagnostics for the workspaces named in $TMP/advisories,
# and a stub `gh` that logs its arguments and answers `issue list` from
# $TMP/open.json through the script's own --jq filter.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/dev/rustsec-report.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*"; cat "$TMP/out" "$TMP/gh.log" 2>/dev/null || true; exit 1; }

mkdir "$TMP/bin"
cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
case " $* " in *" check advisories "*) ;; *) echo "stub cargo: unexpected call: $*" >&2; exit 2 ;; esac
case " $* " in *" --format json "*) ;; *) echo "stub cargo: no --format json" >&2; exit 2 ;; esac
case " $* " in *" --offline "*) echo "stub cargo: the weekly report must read the live database" >&2; exit 2 ;; esac
echo "2026-01-02 03:04:05 [WARN] a human log line on stderr" >&2
# A diagnostic that is not an advisory (deny.toml ignores one this graph lacks).
echo '{"fields":{"code":"advisory-not-detected","graphs":[],"labels":[],"message":"advisory was not encountered","severity":"warning"},"type":"diagnostic"}' >&2
path=""; prev=""
for a in "$@"; do [ "$prev" = --manifest-path ] && path="${a#"$ROOT"/}"; prev="$a"; done
while read -r m sev; do
  [ "$path" = "$m" ] || continue
  [ "$sev" = broken ] && { echo "error: failed to load the graph" >&2; exit 1; }
  printf '{"fields":{"advisory":{"id":"RUSTSEC-2099-0001","package":"demo-crate","title":"Demo crate reads past its buffer"},"code":"vulnerability","graphs":[{"Krate":{"name":"demo-crate","version":"1.2.3"}}],"message":"Demo crate reads past its buffer","severity":"%s"},"type":"diagnostic"}\n' "$sev" >&2
  printf '{"fields":{"advisories":{"errors":1,"helps":0,"notes":0,"warnings":0}},"type":"summary"}\n' >&2
  [ "$sev" = error ] && exit 1
done < "$TMP/advisories"
exit 0
STUB
cat > "$TMP/bin/gh" <<'STUB'
#!/usr/bin/env bash
sub="$1 $2"
printf '%s' "$sub" >> "$TMP/gh.log"
shift 2 || true
jq_expr=""; prev=""
for a in "$@"; do
  printf ' [%s]' "$a" >> "$TMP/gh.log"
  [ "$prev" = --jq ] && jq_expr="$a"
  prev="$a"
done
printf '\n' >> "$TMP/gh.log"
case "$sub" in
  "issue list") jq -r "$jq_expr" "$TMP/open.json" ;;
  "issue create") echo "https://example.invalid/issues/99" ;;
  "issue comment") : ;;
  *) echo "stub gh: unexpected call" >&2; exit 2 ;;
esac
STUB
chmod +x "$TMP/bin/cargo" "$TMP/bin/gh"
export ROOT TMP PATH="$TMP/bin:$PATH" GITHUB_SERVER_URL=https://example.invalid GITHUB_REPOSITORY=example/repo GITHUB_RUN_ID=7

run() { : > "$TMP/gh.log"; rc=0; bash "$SCRIPT" "$@" > "$TMP/out" 2>&1 || rc=$?; }
TITLE='RustSec advisories in the committed lockfiles'
n=0

# 1. No advisory: no gh call at all.
: > "$TMP/advisories"; echo '[]' > "$TMP/open.json"; run
[ "$rc" = 0 ] || fail "no advisory: exit $rc"
[ ! -s "$TMP/gh.log" ] || fail "no advisory: gh was called"
n=$((n + 1))

# 2. Advisories in two workspaces, no open issue: one create carrying both.
printf 'Cargo.toml error\nembedded/baremetal-qemu/Cargo.toml warning\n' > "$TMP/advisories"; echo '[]' > "$TMP/open.json"; run
[ "$rc" = 0 ] || fail "no open issue: exit $rc"
[ "$(grep -c '^issue create' "$TMP/gh.log")" = 1 ] || fail "no open issue: not one issue create"
grep -q '^issue list.*\[--label\] \[dependency-advisory\].*\[--state\] \[open\]' "$TMP/gh.log" || fail "issue list lacks the label or --state open"
grep -q "^issue create.*\[--label\] \[dependency-advisory\]" "$TMP/gh.log" || fail "create lacks the label"
grep -q "^issue create.*\[--title\] \[$TITLE\]" "$TMP/gh.log" || fail "create lacks the title"
for want in 'RUSTSEC-2099-0001' 'demo-crate 1.2.3' 'Demo crate reads past its buffer' '`Cargo.toml`' '`embedded/baremetal-qemu/Cargo.toml`' 'https://example.invalid/example/repo/actions/runs/7'; do
  grep -qF -- "$want" "$TMP/gh.log" || fail "the issue body lacks $want"
done
! grep -q '^issue comment' "$TMP/gh.log" || fail "no open issue: unexpected comment"
! grep -qF '`crates/tst-core/fuzz/Cargo.toml`' "$TMP/gh.log" || fail "no open issue: a workspace without advisories is listed"
n=$((n + 1))

# 3. An open issue with the title, beside another dependency-advisory issue:
# one comment on it, no create.
printf 'Cargo.toml error\n' > "$TMP/advisories"
printf '[{"number":40,"title":"deps: libsrt 1.6.x line available"},{"number":41,"title":"%s"}]\n' "$TITLE" > "$TMP/open.json"; run
[ "$rc" = 0 ] || fail "open issue: exit $rc"
[ "$(grep -c '^issue comment \[41\]' "$TMP/gh.log")" = 1 ] || fail "open issue: not one comment on 41"
awk '/^issue comment \[41\]/ {f = 1} f' "$TMP/gh.log" | grep -q 'RUSTSEC-2099-0001' || fail "open issue: the comment lacks the advisory"
! grep -q '^issue create\|^issue comment \[40\]' "$TMP/gh.log" || fail "open issue: created, or commented on the other issue"
n=$((n + 1))

# 4. Dry run: no gh call; the advisory is printed.
printf 'Cargo.toml error\n' > "$TMP/advisories"; echo '[]' > "$TMP/open.json"; run --dry-run
[ "$rc" = 0 ] || fail "dry run: exit $rc"
[ ! -s "$TMP/gh.log" ] || fail "dry run: gh was called"
grep -q 'RUSTSEC-2099-0001' "$TMP/out" || fail "dry run: the advisory was not printed"
n=$((n + 1))

# 5. cargo-deny fails without reporting an advisory: exit non-zero, the
# workspace is named, no issue.
printf 'crates/tst-srt/fuzz/Cargo.toml broken\n' > "$TMP/advisories"; echo '[]' > "$TMP/open.json"; run
[ "$rc" != 0 ] || fail "broken workspace: exit 0"
grep -q 'crates/tst-srt/fuzz/Cargo.toml' "$TMP/out" || fail "broken workspace: not named"
[ ! -s "$TMP/gh.log" ] || fail "broken workspace: gh was called"
n=$((n + 1))

echo "PASS: $n cases"
