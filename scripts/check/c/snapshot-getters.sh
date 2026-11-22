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
#    `.write()`, `.try_lock()`, the UFCS `Mutex::lock(` / `RwLock::read(`
#    / `RwLock::write(` forms, or a waiting `ParkingSlot` call (its mutex is
#    taken inside tst-pipeline: `.run_parking(`, `.run(`,
#    `.take_for_close(`, `.take_for_conversion(`) while it holds the GIL, on
#    a lock its row in scripts/ratchets/gil-held-locks.tsv does not name.
#    A row names one lock of one function: the identifier the lock call is
#    made on (`inner` in `self.inner.lock()`, also when the call starts the
#    next line; `slot` in `self.slot.run(`; the last identifier of a UFCS
#    form's argument, `inner` in `Mutex::lock(&self.inner)`; the name
#    before the parentheses of a call result, `get` in `self.get().lock()`;
#    `<expr>` when none of these applies), so a second lock taken in a
#    listed function is reported on its own. A mutex or rwlock whose holder
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
#    `.run(` is matched whatever its receiver, so a `run(` that is not a
#    slot query (PyO3's `py.run(`) taken with the GIL held is reported too
#    and gets a row with its reason.
#
#    What a line scanner cannot see, so review has to:
#      - a function that locks and is only ever CALLED from inside an
#        `detach` closure is reported (it is lexically outside one);
#        it gets a row saying where it runs;
#      - a guard taken inside `detach` and returned out of the
#        closure is not reported;
#      - whether the other holders of an allowlisted lock release the GIL is
#        the judgement the row's reason records — the scan does not prove it;
#      - a lock is named by its last identifier only: in a function listed
#        for `inner`, `other.inner.lock()` on another object passes, and every
#        lock reached through a call result shares the method's name;
#      - a guard returned by a helper the binding names itself (e.g.
#        `fn lock_inner(&self) -> MutexGuard<…>`) is not one of the scanned
#        spellings and is invisible to this check;
#      - locks taken through a wrapper (`Owned::with_ref_blocking`) or
#        inside another crate are not scanned here. These Python RTSP-server
#        calls still wait for a tst-rtp lock with the GIL held; no holder of
#        any of them waits for I/O or the GIL, so each wait is bounded by
#        the in-memory work below. Re-check this list when a holder changes:
#          MountHandle.peer_count / __repr__, PublishMount.peer_count /
#          __repr__: the fanout channel's tail mutex (tokio broadcast),
#          held O(1) per take (a send pops at most a fixed batch of
#          waiters, then wakes them after letting go);
#          RtspServer.local_addr: the bound-address mutex, O(1), written
#          once by the listener;
#          RtspServer.__repr__: that mutex, then the mount table's, held
#          for an insert, a remove, a lookup, a snapshot of the table, or
#          an on-demand publish mount's creation (a count over the table
#          and a queue try_send, which never blocks);
#          MountRules.set / remove / set_default / clear / set_challenge,
#          auth_failures / default_auth_failures, len() and __repr__: the
#          rules table's rwlocks, held for one map operation (clear empties
#          the map);
#          shells.OpenCancel.is_cancelled / __repr__, and every open's
#          check of its `cancel=` object before it parses its URL
#          (`open_cancel::begin`): tst-pipeline's `OpenCancel` mutex,
#          held by the object's own methods and each open's slot for a
#          flag read or one edit of the list of opens in progress
#          (`cancel()` takes the list and fires the slots after letting
#          go), and the fresh slot's `CancelSlot` mutex, held for a flag
#          or for installing / taking its target (fired after letting
#          go);
#      - parentheses inside string or char literals, and a `//` comment
#        after code that itself contains ` //` in a string, skew the count
#        until the next `fn`, where it resets.
#
# 3. Python: FAIL when a getter documented as never waiting (the list in
#    check 3 below) takes a lock or the slot at all, with or without the
#    GIL: any `.lock()` / `.read()` / `.write()` / `.try_lock()`, any
#    `Owned` or `ParkingSlot` call that takes the slot (`with_mut`,
#    `with_ref*`, `try_with_ref`, `with_inner`, `.run(`, `.run_parking(`,
#    `.take_for_*`), or a GIL release (`detach(`, `allow_threads_parking`:
#    a getter that releases the GIL is waiting for something). Such a
#    getter reads a snapshot, a cell of atomics or a statistics handle
#    instead. The getter's own body is scanned, and the bodies of the
#    methods of the same type it calls as `self.<name>(` (one level, not
#    their callees, nor a free function or another type's method). A listed
#    function that is not found fails too, so a rename cannot drop it from
#    the check; a getter that is not listed is not checked.

# Portable: bash 3.2, POSIX awk, no GNU-only flags (the macOS legs run it).
#
# Test hooks (scripts/ratchets/tests/self_test.sh): SG_LOCK_ROOTS overrides
# the directories check 2 scans, SG_LOCK_ALLOWLIST its allowlist, and
# SG_ONLY_LOCKS=1 runs check 2 alone. SG_NEVER_WAIT_ROOT is the directory
# check 3's paths are read under (the repository root by default), and
# SG_ONLY_NEVER_WAIT=1 runs check 3 alone.
set -euo pipefail
# Byte semantics for every awk/sort below: BSD awk on the macOS legs runs
# under the runner's UTF-8 locale and dies ("towc: multibyte conversion
# failure") when a regex test meets a non-ASCII byte of a scanned file (the
# Python binding's Rust sources carry em dashes and arrows). The patterns are
# ASCII, so the C locale changes no match.
export LC_ALL=C
cd "$(dirname "$0")/../../.."

if ! command -v rg >/dev/null 2>&1; then
    echo "FAIL: ripgrep (rg) is required by $(basename "$0") but is not on PATH." >&2
    exit 1
fi

status=0

if [ "${SG_ONLY_LOCKS:-}" = "1" ] && [ "${SG_ONLY_NEVER_WAIT:-}" = "1" ]; then
    echo "FAIL: SG_ONLY_LOCKS=1 and SG_ONLY_NEVER_WAIT=1 together would run no check" >&2
    exit 2
fi

# ---- check 1: constant getters inside a slot-taking closure ---------------
if [ "${SG_ONLY_LOCKS:-}" != "1" ] && [ "${SG_ONLY_NEVER_WAIT:-}" != "1" ]; then
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

# ---- check 3: getters that never wait take no lock and no slot ------------
if [ "${SG_ONLY_LOCKS:-}" != "1" ]; then
    NEVER_WAIT_ROOT="${SG_NEVER_WAIT_ROOT:-.}"
    # path<TAB>Type<TAB>function, one getter per line.
    cat > "$tmp/never_wait" <<'LIST'
bindings/python/src/shells/mux_sender.rs	PyMuxSender	video_handle
bindings/python/src/shells/mux_sender.rs	PyMuxSender	klv_handle
bindings/python/src/shells/mux_sender.rs	PyMuxSender	audio_handle
bindings/python/src/shells/mux_sender.rs	PyMuxSender	subtitle_handle
bindings/python/src/shells/mux_sender.rs	PyMuxSender	data_handle
bindings/python/src/srt/managed_convenience.rs	PyManagedMuxSender	video_handle
bindings/python/src/srt/managed_convenience.rs	PyManagedMuxSender	klv_handle
bindings/python/src/srt/managed_convenience.rs	PyManagedMuxSender	audio_handle
bindings/python/src/srt/managed_convenience.rs	PyManagedMuxSender	subtitle_handle
bindings/python/src/srt/managed_convenience.rs	PyManagedMuxSender	data_handle
bindings/python/src/hls/mux_publisher.rs	PyMuxPublisher	stats
bindings/python/src/rtp/push.rs	PyRtspPushSession	stats
bindings/python/src/rtp/push.rs	PyRtspPushSession	end_reason
bindings/python/src/rtp/push.rs	PyRtspPushSession	end_detail
bindings/python/src/rtp/push.rs	PyRtspPushSession	close_outcome
LIST
    # fn_body <file> <type> <fn>: the lines of `fn <fn>(` inside an
    # `impl ... <type>` block, from its `fn` line to the line that closes
    # its braces, each printed as `NR<TAB>line`; comment lines skipped.
    # Prints nothing when the function is not there.
    fn_body() {
        awk -v want_type="$2" -v want_fn="$3" '
            /^impl[ <]/ {
                t = $0
                sub(/^impl(<[^>]*>)?[ \t]+/, "", t)
                i = index(t, " for ")
                if (i > 0) t = substr(t, i + 5)
                sub(/[ \t<{].*$/, "", t)
                n = split(t, parts, "::")
                type = parts[n]
            }
            /^\}/ && !body { type = "" }
            {
                line = $0
                sub(/^[ \t]+/, "", line)
                if (line ~ /^\/\//) next
                if (!body && type == want_type && line ~ ("^(pub(\\([a-z]+\\))? )?fn " want_fn "[ (<]")) {
                    body = 1
                    depth = 0
                    opened = 0
                }
                if (!body) next
                for (k = 1; k <= length(line); k++) {
                    c = substr(line, k, 1)
                    if (c == "{") { depth++; opened = 1 }
                    else if (c == "}") depth--
                }
                print NR "\t" line
                if (opened && depth == 0) body = 0
            }
        ' "$1"
    }
    WAITS='\.(lock|read|write|try_lock)\(\)|with_(mut|ref|inner)|try_with_ref|\.run(_parking)?\(|\.take_for_|allow_threads_parking|(^|[^A-Za-z0-9_])detach\('
    never_wait_failed=0
    while IFS="$(printf '\t')" read -r path type fn; do
        f="$NEVER_WAIT_ROOT/$path"
        if [ ! -f "$f" ]; then
            echo "FAIL: never-waiting getter $type::$fn: $path not found"
            never_wait_failed=1
            continue
        fi
        body=$(fn_body "$f" "$type" "$fn")
        if [ -z "$body" ]; then
            echo "FAIL: never-waiting getter $type::$fn not found in $path (renamed? update check 3's list)"
            never_wait_failed=1
            continue
        fi
        # One level down: the bodies of the methods of the same type the
        # getter calls as `self.<name>(`.
        helpers=$(printf '%s\n' "$body" | grep -oE 'self\.[a-z_][a-z0-9_]*\(' | sed 's/^self\.//; s/($//' | LC_ALL=C sort -u || true)
        for h in $helpers; do
            [ "$h" = "$fn" ] && continue
            body="$body
$(fn_body "$f" "$type" "$h")"
        done
        bad=$(printf '%s\n' "$body" | grep -E "$WAITS" || true)
        if [ -n "$bad" ]; then
            echo "FAIL: $type::$fn ($path) is documented as never waiting but takes a lock, the slot or releases the GIL:"
            printf '%s\n' "$bad" | awk -F '\t' -v p="$path" '{ print "    " p ":" $1 ": " $2 }'
            never_wait_failed=1
        fi
    done < "$tmp/never_wait"
    if [ "$never_wait_failed" -eq 0 ]; then
        echo "OK: $(wc -l < "$tmp/never_wait" | tr -d ' ') never-waiting Python getters take no lock, no slot and keep the GIL"
    else
        status=1
    fi
fi
if [ "${SG_ONLY_NEVER_WAIT:-}" = "1" ]; then
    exit "$status"
fi

# ---- check 2: locks taken while holding the GIL must be allowlisted --------
LOCK_ROOTS="${SG_LOCK_ROOTS:-bindings/python/src}"
LOCK_ALLOWLIST="${SG_LOCK_ALLOWLIST:-scripts/ratchets/gil-held-locks.tsv}"

if [ ! -f "$LOCK_ALLOWLIST" ]; then
    echo "FAIL: allowlist $LOCK_ALLOWLIST not found" >&2
    exit 1
fi

# One line per (file, Type::function, lock) with a `.lock()` outside an
# `detach(` call, or inside an `attach(` nested in one. `at` and
# `wg` are the open-parenthesis depths of those two calls; both reset at
# every `fn`. Comment lines and `#[cfg(test)]` items are skipped.
# shellcheck disable=SC2086  # LOCK_ROOTS is a deliberate word list
find $LOCK_ROOTS -type f -name '*.rs' | LC_ALL=C sort | while IFS= read -r f; do
    awk -v file="$f" '
        BEGIN {
            nlk = split(".lock() .read() .write() .try_lock() Mutex::lock( RwLock::read( RwLock::write(", sp, " ")
            # The waiting ParkingSlot calls: the slot locks its mutex inside
            # tst-pipeline, so the binding shows only these.
            nsl = split(".run_parking( .run( .take_for_close( .take_for_conversion(", sl, " ")
            for (k = 1; k <= nsl; k++) sp[nlk + k] = sl[k]
            nsp = nlk + nsl
        }
        # The lock a call at `i` of `line` is made on: the identifier before
        # its dot (on this line, or ending the previous code line when the
        # call starts this one), or the name before a call result'"'"'s
        # parentheses.
        function receiver(line, i,    j, k, c, depth) {
            j = i - 1
            while (j >= 1 && substr(line, j, 1) ~ /[ \t]/) j--
            if (j < 1) return tail
            if (substr(line, j, 1) == ")") {
                depth = 0
                while (j >= 1) {
                    c = substr(line, j, 1)
                    if (c == ")") depth++
                    else if (c == "(" && --depth == 0) break
                    j--
                }
                j--
            }
            k = j
            while (k >= 1 && substr(line, k, 1) ~ /[A-Za-z0-9_]/) k--
            return (k == j) ? "<expr>" : substr(line, k + 1, j - k)
        }
        # A UFCS lock'"'"'s lock: the last identifier of its argument.
        function ufcs_arg(rest, n,    a, p) {
            a = substr(rest, n + 1)
            p = index(a, ")")
            if (p == 0) return "<expr>"
            a = substr(a, 1, p - 1)
            if (!match(a, /[A-Za-z0-9_]+[ \t]*$/)) return "<expr>"
            a = substr(a, RSTART, RLENGTH)
            sub(/[ \t]+$/, "", a)
            return a
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
                        if (k > nlk && i > 4 && substr(line, i - 4, 4) == "self" &&
                            (i == 5 || substr(line, i - 5, 1) !~ /[A-Za-z0-9_]/)) {
                            # self.take_for_conversion(: the binding type'"'"'s own
                            # method of that name, scanned in its own body.
                            i += length(sp[k]) - 1
                            matched = 1
                            break
                        }
                        if (fn == "") {
                            # a lock outside any fn: a macro_rules! body or
                            # module-level code the scanner cannot attribute.
                            print file "\t<unattributed:" sp[k] ">"
                        } else if (at == 0 || wg > 0) {
                            lk = (index(sp[k], "::") > 0) ? ufcs_arg(rest, length(sp[k])) : receiver(line, i)
                            print file "\t" (type == "" ? fn : type "::" fn) "\t" lk
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
            # The identifier ending this line, for a lock call that starts
            # the next one.
            t = line
            sub(/[ \t]+$/, "", t)
            tail = match(t, /[A-Za-z0-9_]+$/) ? substr(t, RSTART, RLENGTH) : "<expr>"
        }
    ' "$f"
done | LC_ALL=C sort -u > "$tmp/lock_found"

# Allowlist: `path<TAB>Type::function<TAB>lock<TAB>reason`; `#` lines are
# comments.
if ! awk -F '\t' '
        /^#/ || /^[ \t]*$/ { next }
        NF < 4 || $2 == "" || $3 == "" || $4 == "" {
            printf "FAIL: %s:%d: expected path<TAB>Type::function<TAB>lock<TAB>reason\n", FILENAME, NR
            bad = 1
        }
        END { exit bad }
    ' "$LOCK_ALLOWLIST"; then
    status=1
fi
awk -F '\t' '!/^#/ && NF >= 3 { print $1 "\t" $2 "\t" $3 }' "$LOCK_ALLOWLIST" | LC_ALL=C sort -u > "$tmp/lock_allowed"

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
    echo "FAIL: these functions wait for a lock (last column) while holding the GIL:"
    sed 's/^/    /' "$tmp/lock_unlisted"
    echo "      if any holder of that lock releases the GIL while it holds the guard, this"
    echo "      freezes the interpreter. Take the lock inside py.detach and raise"
    echo "      afterwards, or read a construction snapshot / an atomic latch instead."
    echo "      No holder keeps the guard across a GIL release -> add a row, with the"
    echo "      reason, to $LOCK_ALLOWLIST."
    status=1
fi
if [ -s "$tmp/lock_stale" ]; then
    echo "FAIL: $LOCK_ALLOWLIST lists locks these functions no longer take while holding the GIL:"
    sed 's/^/    /' "$tmp/lock_stale"
    echo "      remove the rows."
    status=1
fi
if [ ! -s "$tmp/lock_unlisted" ] && [ ! -s "$tmp/lock_stale" ] && [ "$unattributed" -eq 0 ]; then
    n=$(wc -l < "$tmp/lock_found_attributed" | tr -d ' ')
    echo "OK: all $n locks taken with the GIL held under ($LOCK_ROOTS) are allowlisted, each with a reason"
fi

exit "$status"
