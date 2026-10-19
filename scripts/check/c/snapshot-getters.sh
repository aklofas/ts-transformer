#!/usr/bin/env bash
# Nothing a caller can ask from a second thread may wait behind the call
# the first thread is parked in. A blocked recv / accept / send holds the
# shell slot for as long as it is parked, so a reader that takes the slot
# waits with it — five Python-binding getters once took the slot a parked
# accept/recv owned.
#
# Two checks, both fail-closed.
#
# 1. Every binding (C, Python, JVM): FAIL on any
#    `local_addr( | local_port( | repr(` call inside a `with_mut(|…|` /
#    `with_ref_blocking(…, |…|` closure anywhere under bindings/. The C
#    spelling is `with_inner_mut` / `with_inner_ref`, Python and the JVM use
#    the bare `Owned` names — the pattern covers both.
#
# Readers that must wait for the slot name a
# `tst_pipeline::binding::view::BlockingReason` at the call
# (`Owned::with_ref_blocking`); the compiler, not this script, requires it.
#
# 2. Python: FAIL on any function that calls `.lock()`, `.read()`,
#    `.write()`, `.try_lock()`, or the UFCS `Mutex::lock(` / `RwLock::read(`
#    / `RwLock::write(` forms while it holds the GIL and is not listed in
#    scripts/ratchets/gil-held-locks.tsv. A mutex or rwlock whose holder
#    releases the GIL (`py.detach`) while it holds the guard needs
#    the GIL back before the guard can drop; a second thread that waits
#    for that lock with the GIL held never lets it have it, and the
#    interpreter is frozen. The rule:
#      - take, use and release the lock inside `py.detach`, and raise
#        after the GIL is back;
#      - or read a construction-time snapshot / an atomic latch instead;
#      - only a lock that NO holder keeps across a GIL release may be taken
#        with the GIL held — and the function says so, with its reason, in
#        the allowlist.
#    "Holds the GIL" is lexical: a lock is exempt when it sits inside the
#    parentheses of a `detach(` call and not inside an `attach(`
#    nested in it. A lock taken outside any function (a `macro_rules!`
#    body, or other module-level code) has no `fn` the scanner can
#    attribute it to — that is a hard failure (`<unattributed:…>`), never a
#    silent skip: take the lock in a function the scanner can attribute.
#    The allowlist is exact in both directions and the check fails if it
#    finds no lock at all.
#
#    What a line scanner cannot see, so review has to:
#      - a function that locks and is only ever CALLED from inside an
#        `detach` closure is reported (it is lexically outside one);
#        it gets a row saying where it runs;
#      - a guard taken inside `detach` and returned out of the
#        closure is not reported;
#      - whether the other holders of an allowlisted lock release the GIL is
#        the judgement the row's reason records — the scan does not prove it;
#      - a guard returned by a helper the binding names itself (e.g.
#        `fn lock_inner(&self) -> MutexGuard<…>`) is not one of the scanned
#        spellings and is invisible to this check;
#      - locks taken through a wrapper (`Owned::with_ref_blocking`) or
#        inside another crate are not scanned here;
#      - parentheses inside string or char literals, and a `//` comment
#        after code that itself contains ` //` in a string, skew the count
#        until the next `fn`, where it resets.
#
# Portable: bash 3.2, POSIX awk, no GNU-only flags (the macOS legs run it).
#
# Test hooks (scripts/ratchets/tests/self_test.sh): SG_LOCK_ROOTS overrides
# the directories check 2 scans, SG_LOCK_ALLOWLIST its allowlist, and
# SG_ONLY_LOCKS=1 runs check 2 alone.
set -euo pipefail
cd "$(dirname "$0")/../../.."

if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL: ripgrep (rg) is required by $(basename "$0") but is not on PATH." >&2
    exit 1
fi

status=0

# ---- check 1: constant getters inside a slot-taking closure ---------------
if [ "${SG_ONLY_LOCKS:-}" != "1" ]; then
    # Multiline, non-greedy: from the closure's `|…|` up to the first `;`,
    # flagging a getter call in between.
    PATTERN='with_(inner_)?(mut|ref|ref_blocking)\s*\([^|;]*\|[^|]*\|[^;]*?\b(local_addr|local_port|repr)\s*\('

    set +e
    matches=$(rg -nU --multiline-dotall -t rust "$PATTERN" bindings/)
    rc=$?
    set -e

    case $rc in
        0)
            echo "FAIL: construction-constant getter read inside a slot-taking closure (read Owned::snapshot() instead):"
            echo "$matches"
            status=1
            ;;
        1)
            echo "OK: no slot-taking local_addr/local_port/repr reads under bindings/"
            ;;
        *)
            echo "FAIL: rg exited $rc" >&2
            exit 1
            ;;
    esac
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# ---- check 2: locks taken while holding the GIL must be allowlisted --------
LOCK_ROOTS="${SG_LOCK_ROOTS:-bindings/python/src}"
LOCK_ALLOWLIST="${SG_LOCK_ALLOWLIST:-scripts/ratchets/gil-held-locks.tsv}"

if [ ! -f "$LOCK_ALLOWLIST" ]; then
    echo "FAIL: allowlist $LOCK_ALLOWLIST not found" >&2
    exit 1
fi

# One line per (file, Type::function) with a `.lock()` outside an
# `detach(` call, or inside an `attach(` nested in one. `at` and
# `wg` are the open-parenthesis depths of those two calls; both reset at
# every `fn`. Comment lines and `#[cfg(test)]` items are skipped.
# shellcheck disable=SC2086  # LOCK_ROOTS is a deliberate word list
find $LOCK_ROOTS -type f -name '*.rs' | LC_ALL=C sort | while IFS= read -r f; do
    awk -v file="$f" '
        BEGIN {
            nsp = split(".lock() .read() .write() .try_lock() Mutex::lock( RwLock::read( RwLock::write(", sp, " ")
        }
        /^#\[cfg\(test\)\]/ { guarded = 1; next }
        guarded == 1 {
            if ($0 ~ /^#\[/) next
            if ($0 ~ /;[ \t]*$/ && $0 !~ /\{/) { guarded = 0; next }
            guarded = 2
            next
        }
        guarded == 2 {
            if ($0 ~ /^\}/) guarded = 0
            next
        }
        /^impl[ <]/ {
            t = $0
            sub(/^impl(<[^>]*>)?[ \t]+/, "", t)
            i = index(t, " for ")
            if (i > 0) t = substr(t, i + 5)
            sub(/[ \t<{].*$/, "", t)
            n = split(t, parts, "::")
            type = parts[n]
        }
        /^\}/ { type = ""; fn = "" }
        # A macro_rules! body is not attributed to whatever fn came
        # before it (nor, nested inside a fn, to the rest of that fn
        # after it -- fail-closed until the next `fn` line, rather than
        # proper brace-depth tracking).
        /macro_rules!/ { fn = "" }
        {
            line = $0
            sub(/^[ \t]+/, "", line)
            if (line ~ /^\/\//) next
            sub(/[ \t]\/\/.*$/, "", line)
            if (match(line, /fn [A-Za-z_][A-Za-z0-9_]*/)) {
                fn = substr(line, RSTART + 3, RLENGTH - 3)
                at = 0
                wg = 0
            }
            len = length(line)
            i = 1
            while (i <= len) {
                rest = substr(line, i)
                matched = 0
                for (k = 1; k <= nsp; k++) {
                    if (index(rest, sp[k]) == 1) {
                        if (fn == "") {
                            # a lock outside any fn: a macro_rules! body or
                            # module-level code the scanner cannot attribute.
                            print file "\t<unattributed:" sp[k] ">"
                        } else if (at == 0 || wg > 0) {
                            print file "\t" (type == "" ? fn : type "::" fn)
                        }
                        if (substr(sp[k], length(sp[k]), 1) == "(") {
                            # UFCS form (Mutex::lock(/RwLock::read(/
                            # RwLock::write(): the trailing "(" opens on
                            # an argument, not a balanced "()" like the
                            # dotted spellings -- leave it for the
                            # per-character counter below so its matching
                            # ")" does not under-close at/wg.
                            i += length(sp[k]) - 1
                        } else {
                            i += length(sp[k])
                        }
                        matched = 1
                        break
                    }
                }
                if (matched) continue
                # detach( / attach( count only as a whole name: the char
                # before must not continue an identifier, so foo_detach( or
                # reattach( is not a GIL release or re-acquire.
                prev = (i > 1) ? substr(line, i - 1, 1) : ""
                whole = (prev !~ /[A-Za-z0-9_]/)
                if (at == 0 && index(rest, "allow_threads_parking(") == 1) {
                    # util::allow_threads_parking(py, f): the same
                    # GIL release as py.detach(f).
                    at = 1
                    i += 22
                } else if (at == 0 && whole && index(rest, "detach(") == 1) {
                    at = 1
                    i += 7
                } else if (at > 0 && wg == 0 && whole && index(rest, "attach(") == 1) {
                    wg = 1
                    at++
                    i += 7
                } else {
                    c = substr(line, i, 1)
                    if (c == "(") {
                        if (at > 0) at++
                        if (wg > 0) wg++
                    } else if (c == ")") {
                        if (wg > 0) wg--
                        if (at > 0) at--
                    }
                    i++
                }
            }
        }
    ' "$f"
done | LC_ALL=C sort -u > "$tmp/lock_found"

# Allowlist: `path<TAB>Type::function<TAB>reason`; `#` lines are comments.
if ! awk -F '\t' '
        /^#/ || /^[ \t]*$/ { next }
        NF < 3 || $2 == "" || $3 == "" {
            printf "FAIL: %s:%d: expected path<TAB>Type::function<TAB>reason\n", FILENAME, NR
            bad = 1
        }
        END { exit bad }
    ' "$LOCK_ALLOWLIST"; then
    status=1
fi
awk -F '\t' '!/^#/ && NF >= 2 { print $1 "\t" $2 }' "$LOCK_ALLOWLIST" | LC_ALL=C sort -u > "$tmp/lock_allowed"

if [ ! -s "$tmp/lock_found" ]; then
    echo "FAIL: no lock taken with the GIL held found under: $LOCK_ROOTS"
    echo "      the allowlisted functions exist, so the scan matched nothing — the pattern drifted."
    exit 1
fi

# A lock taken outside any function (a macro_rules! body, or module-level
# code) has no fn the scanner can attribute it to -- that is a hard
# failure, never a silent skip. Pulled out before the allowlist diff so
# it cannot be allowlisted away.
unattributed=0
if grep -q "$(printf '\t<unattributed:')" "$tmp/lock_found"; then
    unattributed=1
    echo "FAIL: lock taken outside any function (macro_rules! body?) — take the lock in a function the scanner can attribute:"
    grep "$(printf '\t<unattributed:')" "$tmp/lock_found" | sed 's/^/    /'
    status=1
fi
grep -v "$(printf '\t<unattributed:')" "$tmp/lock_found" > "$tmp/lock_found_attributed" || true

LC_ALL=C comm -23 "$tmp/lock_found_attributed" "$tmp/lock_allowed" > "$tmp/lock_unlisted"
LC_ALL=C comm -13 "$tmp/lock_found_attributed" "$tmp/lock_allowed" > "$tmp/lock_stale"

if [ -s "$tmp/lock_unlisted" ]; then
    echo "FAIL: these functions wait for a lock while holding the GIL:"
    sed 's/^/    /' "$tmp/lock_unlisted"
    echo "      if any holder of that lock releases the GIL while it holds the guard, this"
    echo "      freezes the interpreter. Take the lock inside py.detach and raise"
    echo "      afterwards, or read a construction snapshot / an atomic latch instead."
    echo "      No holder keeps the guard across a GIL release -> add a row, with the"
    echo "      reason, to $LOCK_ALLOWLIST."
    status=1
fi
if [ -s "$tmp/lock_stale" ]; then
    echo "FAIL: $LOCK_ALLOWLIST lists functions that no longer lock while holding the GIL:"
    sed 's/^/    /' "$tmp/lock_stale"
    echo "      remove the rows."
    status=1
fi
if [ ! -s "$tmp/lock_unlisted" ] && [ ! -s "$tmp/lock_stale" ] && [ "$unattributed" -eq 0 ]; then
    n=$(wc -l < "$tmp/lock_found_attributed" | tr -d ' ')
    echo "OK: all $n functions under ($LOCK_ROOTS) that lock while holding the GIL are allowlisted with a reason"
fi

exit "$status"
