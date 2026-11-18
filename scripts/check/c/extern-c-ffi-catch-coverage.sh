#!/usr/bin/env bash
# Verify that every `pub unsafe extern "C" fn` in bindings/c/core/src/
# isolates panics across the C boundary. Three acceptable forms:
#
#   1. The body wraps work in `crate::panic::ffi_catch(...)` (the
#      open-path + builder-setter pattern).
#   2. The body uses `CHandle::with_inner_mut(...)` or
#      `CHandle::with_inner_ref(...)`, or their `_unless_cancelled` twins —
#      all wrap their closure in `catch_unwind` internally (see
#      bindings/c/core/src/handle.rs).
#   3. The function name is in the trivially-infallible allowlist
#      below (constant returns with no internal locking, panics, or
#      allocation).
#
# Presence is not enough: everything the body does must happen INSIDE the
# boundary, or a panic in it unwinds into C. So the statements before the
# catching statement may only be the null-guard shape (and `static`/`const`
# items), the catching statement may not compute anything in front of the
# catch call, and nothing may follow it. The
# rule is lexical; the check is the python block at the end of this file,
# exercised on a negative fixture by tests/extern-c-ffi-catch-selftest.sh.
#
# The former
# lifecycle-only rail covered only `_close` /
# `_cancel` entries — `tst_demux_config_free` slipped past that
# ratchet by being a `_free`. This ratchet enumerates *every*
# `pub unsafe extern "C" fn` in tst-c, so future entry points cannot
# accidentally bypass the panic-isolation policy.
#
# Cross-language unwinding past a C frame is undefined behavior under
# `panic="unwind"` and an abort with no last-error visibility under
# `panic="abort"`. See bindings/c/core/src/panic.rs for the contract.

set -euo pipefail

SRC_DIR="bindings/c/core/src"
# With file arguments only those files are checked (the self-test's fixture).
FIXTURE_FILES=("$@")

# Allowlist of function names that are trivially infallible: no
# pointer dereferences, no allocations, no internal locks, no calls
# into tst-core. They return a process-lifetime constant (or a packed
# integer). Keep this list small and intentional.
ALLOWLIST=(
    "tst_get_abi_version_major"
    "tst_get_abi_version_minor"
    "tst_get_version_major"
    "tst_get_version_minor"
    "tst_get_version_patch"
    "tst_get_version_packed"
    "tst_get_version_string"
)

is_allowlisted() {
    local name="$1"
    for entry in "${ALLOWLIST[@]}"; do
        if [[ "$entry" == "$name" ]]; then
            return 0
        fi
    done
    return 1
}

# Step 1: enumerate every signature line. Bash 3.2-portable
# read-into-array pattern (no `mapfile`/`readarray`, no `declare -A`).
ENTRIES=()
while IFS= read -r entry; do
    ENTRIES+=("$entry")
done < <(
    if [[ ${#FIXTURE_FILES[@]} -gt 0 ]]; then
        grep -En '^pub unsafe extern "C" fn tst_[a-zA-Z0-9_]+' "${FIXTURE_FILES[@]}" /dev/null
    else
        grep -rEn '^pub unsafe extern "C" fn tst_[a-zA-Z0-9_]+' \
            "$SRC_DIR" \
            --include='*.rs'
    fi | sort
)

if [[ ${#ENTRIES[@]} -eq 0 ]]; then
    echo "FAIL: found 0 extern \"C\" entry points — grep pattern may have drifted from source layout"
    exit 1
fi

missing=0
checked=0
allowlisted=0

for entry in "${ENTRIES[@]}"; do
    file="${entry%%:*}"
    rest="${entry#*:}"
    lineno="${rest%%:*}"

    # Extract function name. Pattern: `pub unsafe extern "C" fn NAME(` or
    # `pub unsafe extern "C" fn NAME<` or `pub unsafe extern "C" fn NAME `.
    sig_line=$(sed -n "${lineno}p" "$file")
    fn_name=$(echo "$sig_line" \
        | sed -E 's/^pub unsafe extern "C" fn ([a-zA-Z0-9_]+).*/\1/')

    if [[ -z "$fn_name" ]]; then
        echo "FAIL: could not parse function name at ${file}:${lineno}"
        echo "      line: ${sig_line}"
        exit 1
    fi

    # Allowlist short-circuit.
    if is_allowlisted "$fn_name"; then
        allowlisted=$((allowlisted + 1))
        continue
    fi

    # Step 2: extract the function body. Rustfmt formats every public
    # function with the signature `pub unsafe extern "C" fn ...` at
    # column 0 and the closing brace `}` also at column 0. Read from
    # the signature line through the next `^}` line (inclusive).
    body=$(awk -v start="$lineno" '
        NR >= start {
            print
            if (NR > start && $0 == "}") {
                exit
            }
        }
    ' "$file")

    # Step 3: scan body for one of the panic-isolation patterns. The
    # `with_mux_publisher(` HLS helper and its reader twin `with_mux_publisher_ref(`
    # (bindings/c/core/src/hls/mux_publisher.rs) wrap their closure in
    # crate::panic::ffi_catch internally — same contract as
    # CHandle::with_inner_mut / with_inner_ref — so callers delegating through
    # them are isolated.
    # `crate::transport_impls::` calls (WP18 genericization) forward directly
    # to generic bodies in transport_impls.rs, which route through with_inner_mut
    # / with_inner_ref internally; the isolation guarantee is preserved
    # one hop away but is still unconditional.
    # Here-string, NOT `echo "$body" | grep -q`: `grep -q` closes the pipe on
    # first match, `echo` then dies with SIGPIPE, and under `set -o pipefail`
    # that turns a MATCH into a pipeline failure → a bogus MISSING. The race is
    # timing-dependent (flaky; surfaced on the macOS runner). A here-string has
    # no pipe, so there is no SIGPIPE to mask the match.
    if grep -qE 'crate::panic::ffi_catch\(|^\s*ffi_catch\(|\.with_inner_(mut|ref)(_unless_cancelled)?\(|with_mux_publisher(_ref)?\(|crate::transport_impls::' <<<"$body"; then
        checked=$((checked + 1))
        continue
    fi

    echo "MISSING: ${fn_name} at ${file}:${lineno} has no ffi_catch / with_inner_mut / with_inner_ref / allowlist entry"
    missing=$((missing + 1))
done

if [[ $missing -gt 0 ]]; then
    echo
    echo "FAIL: $missing of ${#ENTRIES[@]} extern \"C\" entry points bypass panic isolation"
    echo
    echo "Fix options:"
    echo "  1. Wrap the body in crate::panic::ffi_catch(default, || { ... })"
    echo "  2. Route mutation through CHandle::with_inner_mut(|inner| { ... })"
    echo "  3. If the function is provably infallible (constant return,"
    echo "     no allocation, no locking), add it to the ALLOWLIST in"
    echo "     scripts/check/c/extern-c-ffi-catch-coverage.sh"
    exit 1
fi

echo "OK: ${#ENTRIES[@]} extern \"C\" entry points wrap panic isolation"
echo "    ${checked} via ffi_catch / with_inner_{mut,ref}"
echo "    ${allowlisted} via allowlist (trivially infallible)"

# Companion assertion for the `crate::transport_impls::` pattern above: the
# pattern trusts the WHOLE module, so enforce the module's isolation contract
# here — every pub(crate) fn in transport_impls.rs must itself route through
# with_inner_mut / with_inner_ref (or ffi_catch). Without this loop, a future
# fn added to the module without isolation would be silently accepted.
TI_FILE="bindings/c/core/src/transport_impls.rs"
if [[ ${#FIXTURE_FILES[@]} -eq 0 && -f "$TI_FILE" ]]; then
    ti_missing=0
    ti_total=0
    while IFS= read -r sig_entry; do
        ti_lineno="${sig_entry%%:*}"
        ti_total=$((ti_total + 1))
        # Body = from the signature line to the next top-level closing brace.
        ti_body=$(awk -v start="$ti_lineno" \
            'NR>=start{print; if(NR>start && $0=="}"){exit}}' "$TI_FILE")
        # Here-string, not a pipe (same SIGPIPE rationale as the main loop).
        if ! grep -qE 'with_inner_(mut|ref)(_unless_cancelled)?\(|ffi_catch\(' <<<"$ti_body"; then
            echo "MISSING isolation in transport_impls.rs fn at line $ti_lineno"
            ti_missing=$((ti_missing + 1))
        fi
    done < <(grep -nE '^pub\(crate\) (unsafe )?fn ' "$TI_FILE")
    if [[ $ti_missing -gt 0 ]]; then
        echo "FAIL: $ti_missing of $ti_total transport_impls fns lack internal isolation"
        exit 1
    fi
    echo "    ${ti_total} transport_impls generic bodies verified isolated"
fi

# Containment: the catch call must be the first thing that can fail. For each
# entry point (the constant-getter allowlist excepted) the python block below
# splits the body into top-level statements and requires
#   - before the catching statement: only a null guard
#     (`let Some(x) = (unsafe { p.as_ref() }) else { [set_last_error(..);] return ..; };`
#     or `if <is_null / plain comparison> { [set_last_error(..);] return ..; }`)
#     or a `static` / `const` item;
#   - in the catching statement: nothing computed in front of the catch call
#     (a receiver path such as `handle.inner` or an `unsafe {` is fine; a call,
#     a macro or a branch is not);
#   - after it: nothing (the catching statement is the body's tail or a
#     `let`/statement that ends it; relabels and post-processing belong
#     inside the closure).
# Arguments inside the catch call (the closure, the default) are inside by
# construction. An offender is printed as function and file:line.
if [[ ${#FIXTURE_FILES[@]} -gt 0 ]]; then
    CONTAIN_FILES=("${FIXTURE_FILES[@]}")
else
    CONTAIN_FILES=()
    while IFS= read -r f; do CONTAIN_FILES+=("$f"); done < <(find "$SRC_DIR" -name '*.rs' | sort)
fi
if ! python3 -I - "$(IFS=,; echo "${ALLOWLIST[*]}")" "${CONTAIN_FILES[@]}" <<'PY'
import re, sys

allow = set(sys.argv[1].split(","))
files = sys.argv[2:]
CATCH = re.compile(
    r'crate::panic::ffi_catch\(|\bffi_catch\('
    r'|\.with_inner_(mut|ref)(_unless_cancelled)?\('
    r'|with_mux_publisher(_ref)?\(|crate::transport_impls::'
)
SIG = re.compile(r'^pub unsafe extern "C" fn (tst_\w+)')
RET = r'return [\w:]+( as i32)?;'
SETERR = r'(set_last_error\(TstError::\w+, "[^"]*"\); )?'
ALLOWED_PRE = [
    re.compile(r'^let Some\(\w+\) = \(unsafe \{ \w+\.as_ref\(\) \}\) else \{ ' + SETERR + RET + r' \}$'),
    re.compile(
        r'^if (\w+\.is_null\(\)|\w+ (==|<|>|<=|>=) \w+)'
        r'( (&&|\|\|) (\w+\.is_null\(\)|\w+ (==|<|>|<=|>=) \w+))* \{ ' + SETERR + RET + r' \}$'
    ),
    re.compile(r'^(pub )?(static|const) \w+: [^=]+ = [^;{}()]*$'),
]
HEAD_STRIP = re.compile(r'^(let (mut )?\w+ = |return )?(unsafe \{ )?')
BLOCKS = ("if ", "unsafe ", "match ", "for ", "while ", "loop ", "{")


def strip_comments(text):
    out, i, n = [], 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            while i < n and text[i] != "\n":
                i += 1
        elif c == '"':
            j = i + 1
            while text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(text[i:j + 1])
            i = j + 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


def statements(body, first_line):
    """Top-level statements as (line, normalised text)."""
    res, depth, cur, line, start, i, n = [], 0, [], first_line, None, 0, len(body)

    def flush():
        nonlocal cur, start
        t = " ".join("".join(cur).split()).rstrip(";").rstrip()
        if t:
            res.append((start, t))
        cur, start = [], None

    while i < n:
        c = body[i]
        if c == '"':
            j = i + 1
            while body[j] != '"':
                j += 2 if body[j] == "\\" else 1
            if start is None:
                start = line
            cur.append(body[i:j + 1])
            line += body[i:j + 1].count("\n")
            i = j + 1
            continue
        if c == "\n":
            line += 1
        elif not c.isspace() and start is None:
            start = line
        cur.append(c)
        if c in "({[":
            depth += 1
        elif c in ")}]":
            depth -= 1
            if c == "}" and depth == 0:
                rest = body[i + 1:].lstrip()
                head = "".join(cur).lstrip()
                if (rest and not rest.startswith(("else", ".", ";", ")", "?", "{"))
                        and head.startswith(BLOCKS)):
                    flush()
        elif c == ";" and depth == 0:
            flush()
        i += 1
    flush()
    return res


bad = 0
for path in files:
    lines = open(path).read().split("\n")
    for i, l in enumerate(lines):
        m = SIG.match(l)
        if not m or m.group(1) in allow:
            continue
        name = m.group(1)
        j = i
        while not lines[j].rstrip().endswith("{"):
            j += 1
        k = j + 1
        while lines[k] != "}":
            k += 1
        stmts = statements(strip_comments("\n".join(lines[j + 1:k])), j + 2)
        idx = next((x for x, (_, s) in enumerate(stmts) if CATCH.search(s)), None)
        if idx is None:
            continue  # the presence check above reports it
        for ln, s in stmts[:idx]:
            if not any(r.match(s) for r in ALLOWED_PRE):
                print(f"OUTSIDE: {name} ({path}:{i + 1}) runs `{s[:90]}` at line {ln} before its panic boundary")
                bad += 1
        ln, s = stmts[idx]
        head = HEAD_STRIP.sub("", s[:CATCH.search(s).start()])
        if re.search(r'[(!]|\b(if|match|else|for|while|loop)\b', head):
            print(f"OUTSIDE: {name} ({path}:{i + 1}) computes `{head.strip()[:90]}` at line {ln} in front of its panic boundary")
            bad += 1
        for ln, s in stmts[idx + 1:]:
            print(f"OUTSIDE: {name} ({path}:{i + 1}) runs `{s[:90]}` at line {ln} after its panic boundary")
            bad += 1
sys.exit(1 if bad else 0)
PY
then
    echo
    echo "FAIL: extern \"C\" entry points do work outside their panic boundary"
    echo "      Move the work into the ffi_catch / with_inner_* closure (see the rule above)."
    exit 1
fi
echo "    containment: every body works inside its boundary"
