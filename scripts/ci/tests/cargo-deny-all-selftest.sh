#!/usr/bin/env bash
# Self-test of scripts/dev/cargo-deny-all.sh against a stub `cargo` and a stub
# `git` that log their arguments. An empty pin checks every workspace against
# the live database; a set pin moves cargo-deny's own clone to that commit and
# checks offline; a failing workspace is named and the others still run.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SCRIPT="$ROOT/scripts/dev/cargo-deny-all.sh"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
fail() { echo "FAIL: $*"; cat "$TMP/out" 2>/dev/null || true; exit 1; }

mkdir "$TMP/bin"
cat > "$TMP/bin/cargo" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/cargo.log"
case " $* " in
  *" fetch db "*) mkdir -p "$CARGO_HOME/advisory-db/advisory-db-0123456789abcdef/.git" ;;
  *" check "*) for m in $(cat "$TMP/deny-fails" 2>/dev/null); do
                 case " $* " in *"/$m "*) exit 1 ;; esac
               done ;;
esac
exit 0
STUB
cat > "$TMP/bin/git" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/git.log"
case " $* " in
  *" rev-parse HEAD "*) cat "$TMP/head" ;;
  *" show "*) echo 2026-01-02T03:04:05+00:00 ;;
esac
exit 0
STUB
chmod +x "$TMP/bin/cargo" "$TMP/bin/git"
export TMP PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/cargo-home" ADVISORY_DB_PIN="$TMP/pin.toml"

REV=0123456789abcdef0123456789abcdef01234567
pin() { printf '# header\nrev = "%s"\n' "$1" > "$TMP/pin.toml"; }
run() { : > "$TMP/cargo.log"; : > "$TMP/git.log"; rm -rf "$CARGO_HOME"; rc=0; bash "$SCRIPT" > "$TMP/out" 2>&1 || rc=$?; }
count() { grep -c -e "$1" "$2" || true; }
n=0

# 1. --list: eight manifests, each in the tree.
bash "$SCRIPT" --list > "$TMP/list"
[ "$(wc -l < "$TMP/list" | tr -d ' ')" = 8 ] || fail "--list did not print eight manifests: $(cat "$TMP/list")"
while IFS= read -r m; do [ -f "$ROOT/$m" ] || fail "--list names a missing manifest $m"; done < "$TMP/list"
grep -qx 'Cargo.toml' "$TMP/list" || fail "--list lacks the root workspace"
n=$((n + 1))

# 2. Empty pin: eight live checks, no offline run, no git, no fetch.
pin ""; run
[ "$rc" = 0 ] || fail "empty pin: exit $rc"
[ "$(count ' check advisories licenses' "$TMP/cargo.log")" = 8 ] || fail "empty pin: not eight checks"
[ "$(count '--locked' "$TMP/cargo.log")" = 8 ] || fail "empty pin: a check without --locked"
[ "$(count '--offline' "$TMP/cargo.log")" = 0 ] || fail "empty pin: an offline check"
[ "$(count 'fetch' "$TMP/cargo.log")" = 0 ] || fail "empty pin: a fetch"
[ ! -s "$TMP/git.log" ] || fail "empty pin: git was called"
grep -q 'live' "$TMP/out" || fail "empty pin: the log does not say the database is live"
n=$((n + 1))

# 3. A failing workspace: exit 1, named, and the other seven still checked.
echo crates/tst-rtp/fuzz/Cargo.toml > "$TMP/deny-fails"; pin ""; run
rm "$TMP/deny-fails"
[ "$rc" = 1 ] || fail "failing workspace: exit $rc, want 1"
[ "$(count ' check ' "$TMP/cargo.log")" = 8 ] || fail "failing workspace: the others were not all checked"
grep -q 'FAILED.*crates/tst-rtp/fuzz/Cargo.toml' "$TMP/out" || fail "failing workspace: not named"
[ "$(count 'FAILED' "$TMP/out")" = 1 ] || fail "failing workspace: more than one named"
n=$((n + 1))

# 4. Set pin: one live fetch of the db, the clone moved to the pin, then a
# cargo fetch and an offline check per workspace.
pin "$REV"; echo "$REV" > "$TMP/head"; run
[ "$rc" = 0 ] || fail "set pin: exit $rc"
[ "$(count ' fetch db' "$TMP/cargo.log")" = 1 ] || fail "set pin: not one db fetch"
grep -q "advisory-db-0123456789abcdef fetch -q --depth=1 origin $REV" "$TMP/git.log" || fail "set pin: the pinned commit was not fetched into cargo-deny's clone"
grep -q "advisory-db-0123456789abcdef reset -q --hard FETCH_HEAD" "$TMP/git.log" || fail "set pin: the clone was not reset to it"
[ "$(count '^fetch --locked --manifest-path' "$TMP/cargo.log")" = 8 ] || fail "set pin: not eight cargo fetches"
[ "$(count '--offline .* check advisories licenses' "$TMP/cargo.log")" = 8 ] || fail "set pin: not eight offline checks"
grep -q "advisory-db at $REV" "$TMP/out" || fail "set pin: the log does not name the commit"
n=$((n + 1))

# 5. The clone did not land on the pin: fail before any check.
pin "$REV"; echo ffffffffffffffffffffffffffffffffffffffff > "$TMP/head"; run
[ "$rc" != 0 ] || fail "wrong HEAD: exit 0"
[ "$(count ' check ' "$TMP/cargo.log")" = 0 ] || fail "wrong HEAD: a check ran"
n=$((n + 1))

# 6. A malformed pin: fail, no cargo call.
pin "abc123"; run
[ "$rc" != 0 ] || fail "malformed pin: exit 0"
[ ! -s "$TMP/cargo.log" ] || fail "malformed pin: cargo was called"
grep -q 'not 40 hex' "$TMP/out" || fail "malformed pin: no reason given"
n=$((n + 1))

# 7. A pin file without a rev line: fail, no cargo call.
echo '# nothing' > "$TMP/pin.toml"; run
[ "$rc" != 0 ] || fail "no rev line: exit 0"
[ ! -s "$TMP/cargo.log" ] || fail "no rev line: cargo was called"
n=$((n + 1))

echo "PASS: $n cases"
