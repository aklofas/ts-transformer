#!/usr/bin/env bash
# Self-test of scripts/dev/cargo-deny-all.sh against a stub `cargo` and a stub
# `git` that log their arguments. An empty pin checks every workspace against
# the live database with deny.toml as it is; a set pin moves cargo-deny's own
# clone to that commit and checks offline, with a yanked crate a warning; a
# failing workspace is named and the others still run.
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
  *" fetch db "*) # $TMP/db-fails attempts leave a half-written clone and fail;
                  # an attempt that finds one fails as cargo-deny would.
                  d="$CARGO_HOME/advisory-db/advisory-db-0123456789abcdef"
                  [ ! -e "$d/partial" ] || { echo "stub: fetching into a half-written clone" >&2; exit 1; }
                  c="$(cat "$TMP/db-count" 2>/dev/null || echo 0)"; echo $((c + 1)) > "$TMP/db-count"
                  mkdir -p "$d/.git"
                  [ "$c" -ge "$(cat "$TMP/db-fails" 2>/dev/null || echo 0)" ] || { touch "$d/partial"; exit 1; } ;;
  " fetch "*) # $TMP/fetch-fails: "<manifest> <failures before a success>"
             while read -r m k; do
               case " $* " in *"/$m "*) ;; *) continue ;; esac
               c="$(cat "$TMP/fetch-count" 2>/dev/null || echo 0)"; echo $((c + 1)) > "$TMP/fetch-count"
               [ "$c" -ge "$k" ] || exit 1
             done < <(cat "$TMP/fetch-fails" 2>/dev/null) ;;
  *" check "*) cfg="$(printf '%s\n' "$@" | grep -A1 -x -e --config | tail -1)"
               { grep '^yanked' "$cfg" || echo "no yanked line"; } >> "$TMP/yanked.log"
               diff "$ROOT/deny.toml" "$cfg" | grep '^[<>]' >> "$TMP/config-diff.log" || true
               echo "proxy=${https_proxy-} ${HTTPS_PROXY-} ${http_proxy-} ${HTTP_PROXY-} ${ALL_PROXY-} ${all_proxy-} no_proxy=[${no_proxy-unset}] NO_PROXY=[${NO_PROXY-unset}]" >> "$TMP/check-env.log"
               for m in $(cat "$TMP/deny-fails" 2>/dev/null); do
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
  *" fetch "*) c="$(cat "$TMP/git-count" 2>/dev/null || echo 0)"; echo $((c + 1)) > "$TMP/git-count"
               [ "$c" -ge "$(cat "$TMP/git-fails" 2>/dev/null || echo 0)" ] || exit 1 ;;
  *" show "*) echo 2026-01-02T03:04:05+00:00 ;;
esac
exit 0
STUB
# The retry waits and the wall-clock bound, observed instead of spent.
cat > "$TMP/bin/sleep" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "$TMP/sleep.log"
STUB
cat > "$TMP/bin/timeout" <<'STUB'
#!/usr/bin/env bash
[ "$1" = --version ] && { echo "timeout (GNU coreutils) 9.4"; exit 0; }
printf '%s\n' "$*" >> "$TMP/timeout.log"
shift 3
exec "$@"
STUB
chmod +x "$TMP/bin/cargo" "$TMP/bin/git" "$TMP/bin/sleep" "$TMP/bin/timeout"
export no_proxy=example.invalid NO_PROXY=example.invalid  # the script must empty both
export ROOT TMP PATH="$TMP/bin:$PATH" CARGO_HOME="$TMP/cargo-home" ADVISORY_DB_PIN="$TMP/pin.toml"

REV=0123456789abcdef0123456789abcdef01234567
pin() { printf '# header\nrev = "%s"\n' "$1" > "$TMP/pin.toml"; }
run() { : > "$TMP/yanked.log"; : > "$TMP/config-diff.log"; : > "$TMP/cargo.log"; : > "$TMP/git.log"; : > "$TMP/sleep.log"; : > "$TMP/timeout.log"; : > "$TMP/check-env.log"; rm -f "$TMP/fetch-count" "$TMP/db-count" "$TMP/git-count"; rm -rf "$CARGO_HOME"; ${PRE:-:}; rc=0; bash "$SCRIPT" > "$TMP/out" 2>&1 || rc=$?; }
DEAD=http://127.0.0.1:9
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
[ "$(count ' fetch db' "$TMP/cargo.log")" = 1 ] || fail "empty pin: not one bounded db fetch before the checks"
[ "$(count '^fetch ' "$TMP/cargo.log")" = 0 ] || fail "empty pin: a cargo fetch"
[ "$(count ' fetch db' "$TMP/timeout.log")" = 1 ] || fail "empty pin: the db fetch is not bounded"
[ ! -s "$TMP/git.log" ] || fail "empty pin: git was called"
grep -q 'proxy=http' "$TMP/check-env.log" && fail "empty pin: a live check was cut off the network"
grep -q 'live' "$TMP/out" || fail "empty pin: the log does not say the database is live"
[ "$(grep -cxF 'yanked = "deny"' "$TMP/yanked.log")" = 8 ] || fail "empty pin: a live check that does not deny a yanked crate: $(sort -u "$TMP/yanked.log")"
[ ! -s "$TMP/config-diff.log" ] || fail "empty pin: a live check that does not read deny.toml as it is"
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
[ "$(count '^-k 10 120 cargo fetch --locked' "$TMP/timeout.log")" = 8 ] || fail "set pin: a cargo fetch without its bound"
[ "$(grep -cxF "proxy=$DEAD $DEAD $DEAD $DEAD $DEAD $DEAD no_proxy=[] NO_PROXY=[]" "$TMP/check-env.log")" = 8 ] \
  || fail "set pin: an offline check that could still reach the network"
[ "$(grep -cxF 'yanked = "warn"' "$TMP/yanked.log")" = 8 ] || fail "set pin: a pinned check that fails on a yanked crate: $(sort -u "$TMP/yanked.log")"
[ "$(sort -u "$TMP/config-diff.log" | tr '\n' '|')" = '< yanked = "deny"|> yanked = "warn"|' ] \
  || fail "set pin: the pinned config differs from deny.toml in more than the yanked key: $(sort -u "$TMP/config-diff.log")"
n=$((n + 1))

# 4b. A cargo fetch that fails twice is retried and the run passes; one that
# fails three times fails its workspace only, and the others still run.
pin "$REV"; echo "$REV" > "$TMP/head"; echo 'crates/tst-srt/fuzz/Cargo.toml 2' > "$TMP/fetch-fails"; run
[ "$rc" = 0 ] || fail "fetch retried: exit $rc"
[ "$(count 'tst-srt/fuzz/Cargo.toml' "$TMP/cargo.log" | head -1)" = 4 ] || fail "fetch retried: not three fetches and one check"
[ "$(wc -l < "$TMP/sleep.log" | tr -d ' ')" = 2 ] || fail "fetch retried: not two waits"
n=$((n + 1))
echo 'crates/tst-srt/fuzz/Cargo.toml 3' > "$TMP/fetch-fails"; run
rm "$TMP/fetch-fails"
[ "$rc" = 1 ] || fail "fetch failing: exit $rc, want 1"
grep -q 'FAILED.*crates/tst-srt/fuzz/Cargo.toml' "$TMP/out" || fail "fetch failing: the workspace is not named"
[ "$(count '^fetch --locked.*tst-srt/fuzz' "$TMP/cargo.log")" = 3 ] || fail "fetch failing: not three attempts"
[ "$(count ' check advisories licenses' "$TMP/cargo.log")" = 7 ] || fail "fetch failing: the other seven were not checked"
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

# 8. The database fetch and the git fetch of the pin are bounded and retried,
# 20 s then 60 s apart.
pin "$REV"; echo "$REV" > "$TMP/head"; echo 0 > "$TMP/db-fails"; echo 2 > "$TMP/git-fails"; run
rm "$TMP/git-fails"
[ "$rc" = 0 ] || fail "git fetch retried: exit $rc"
[ "$(count " fetch -q --depth=1 origin $REV" "$TMP/git.log")" = 3 ] || fail "git fetch retried: not three attempts"
[ "$(count "^-k 10 120 git -C .* fetch -q --depth=1 origin $REV" "$TMP/timeout.log")" = 3 ] || fail "git fetch retried: an attempt without its bound"
[ "$(tr '\n' ' ' < "$TMP/sleep.log")" = "20 60 " ] || fail "git fetch retried: the waits were '$(tr '\n' ' ' < "$TMP/sleep.log")', want '20 60 '"
n=$((n + 1))

# 9. A database fetch cut off twice: each retry starts from no clone (the one
# the cut attempt left half-written is removed), three bounded attempts.
pin "$REV"; echo "$REV" > "$TMP/head"; echo 2 > "$TMP/db-fails"; run
[ "$rc" = 0 ] || fail "db fetch retried: exit $rc"
[ "$(count ' fetch db' "$TMP/cargo.log")" = 3 ] || fail "db fetch retried: not three attempts"
[ "$(count '^-k 10 120 cargo deny .* fetch db' "$TMP/timeout.log")" = 3 ] || fail "db fetch retried: an attempt without its bound"
[ "$(tr '\n' ' ' < "$TMP/sleep.log")" = "20 60 " ] || fail "db fetch retried: the waits were not 20 s and 60 s"
n=$((n + 1))

# 10. A clone that was there before the run is never removed: the failed
# attempt leaves it half-written, every retry fails on it, the run fails.
pin "$REV"; echo "$REV" > "$TMP/head"; echo 1 > "$TMP/db-fails"
PRE='eval mkdir -p "$CARGO_HOME/advisory-db/advisory-db-0123456789abcdef/.git" && touch "$CARGO_HOME/advisory-db/advisory-db-0123456789abcdef/keep"'; run; PRE=
rm "$TMP/db-fails"
[ "$rc" != 0 ] || fail "existing clone: exit 0"
[ -e "$CARGO_HOME/advisory-db/advisory-db-0123456789abcdef/keep" ] || fail "existing clone: removed"
grep -q 'could not fetch the advisory-db' "$TMP/out" || fail "existing clone: no reason given"
n=$((n + 1))

echo "PASS: $n cases"
