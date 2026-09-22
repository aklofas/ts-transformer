#!/usr/bin/env bash
# Shared error-mapping coverage checks. Sourced by run-py-coverage.sh, which
# feeds it rows from error-mapping.tsv. This replaces
# the family of near-identical per-protocol check-*-error-mapping-coverage.sh
# clones; the extraction and failure messages are kept byte-for-byte equivalent
# to those clones (only parameterised by enum / fn / file).
#
# Three enforced shapes:
#   rust  : every `<Enum>ErrorKind` variant in a tst-<proto> crate has an arm in
#           `fn <proto>_error_to_code` in bindings/c/core/src/error.rs, and no
#           wildcard arm unless the enum is #[non_exhaustive].
#   py    : every `class <Enum>ErrorKind` member in tstrans.exceptions has at
#           least one `make_<proto>_error(py, "KIND", ...)` call site under
#           bindings/python/src/, and no call site names an unknown kind.
#   pyarm : every variant of a tst-core Rust enum (e.g. `DemuxError`) has an
#           explicit `<Enum>::<Variant>` arm somewhere in a Python-binding
#           `.rs` source file. Unlike `rust`/`py`, the enum here isn't a
#           per-protocol `*ErrorKind` and the arm file is a binding source
#           file, not bindings/c/core/src/error.rs — these mappers carry a
#           forward-compat wildcard fallback by design, so there's no
#           wildcard/non_exhaustive cross-check (that would always fire).

# --- Rust extraction (identical to the former clones) -----------------------

rust_enum_variants() { # <enum> <variant_source>
    awk "/pub enum $1/,/^}/" "$2" \
        | grep -oE '^\s+([A-Z][A-Za-z0-9]+)' \
        | sed 's/^[[:space:]]*//' | sort -u
}

rust_mapped_variants() { # <enum> <arm_fn> <arm_file>
    awk "/fn $2/,/^}/" "$3" \
        | grep -oE "$1::([A-Z][A-Za-z0-9]+)" \
        | sed "s/$1:://" | sort -u
}

rust_has_wildcard() { # <arm_fn> <arm_file>  -> prints count
    awk "/fn $1/,/^}/" "$2" | grep -cE '_\s*=>' || true
}

rust_count_non_exhaustive() { # <variant_source> -> prints count
    grep -c '#\[non_exhaustive\]' "$1" || true
}

# Returns 0 if covered, 1 (and prints the clone's messages) on any gap.
assert_rust_row() { # <enum> <variant_source> <arm_fn> <arm_file>
    local enum="$1" src="$2" arm_fn="$3" arm_file="$4"
    if [[ ! -f "$arm_file" ]]; then echo "FAIL: $arm_file not found"; return 1; fi
    if [[ ! -f "$src" ]]; then echo "FAIL: $src not found"; return 1; fi

    local variants mapped missing
    variants=$(rust_enum_variants "$enum" "$src")
    mapped=$(rust_mapped_variants "$enum" "$arm_fn" "$arm_file")
    missing=$(comm -23 <(echo "$variants") <(echo "$mapped") | grep -v '^$' || true)
    if [[ -n "$missing" ]]; then
        echo "FAIL: $enum variants missing C error code mapping in $arm_fn:"
        echo "$missing"
        return 1
    fi

    local non_exhaustive has_wildcard
    non_exhaustive=$(rust_count_non_exhaustive "$src")
    has_wildcard=$(rust_has_wildcard "$arm_fn" "$arm_file")
    if [[ "$non_exhaustive" -eq 0 && "$has_wildcard" -gt 0 ]]; then
        echo "FAIL: $arm_fn uses wildcard arm but $enum is not #[non_exhaustive]"
        echo "Either remove the wildcard or mark the enum non_exhaustive."
        return 1
    fi
    echo "PASS: ${arm_fn%_error_to_code}-error-mapping-coverage"
    return 0
}

# --- Python extraction (identical to the former clones) ---------------------

py_class_variants() { # <class> <exc_file>
    awk -v cls="$1" '
      $0 ~ ("^class " cls) { in_block = 1; next }
      in_block && /^class / && $0 !~ ("^class " cls) { in_block = 0 }
      in_block && /^    [A-Z_][A-Z0-9_]* = [0-9]+/ {
        sub(/^    /, ""); sub(/ .*$/, ""); print
      }
    ' "$2" | sort -u
}

py_used_kinds() { # <make_fn> <src_dir>
    grep -rh -E "$1\([^,]+,\s*\"[A-Z_][A-Z0-9_]*\"" "$2" 2>/dev/null \
        | grep -v -E '^\s*//' \
        | grep -oE "$1\([^,]+,\s*\"[A-Z_][A-Z0-9_]*\"" \
        | grep -oE '"[A-Z_][A-Z0-9_]*"' | tr -d '"' | sort -u
}

assert_py_row() { # <class> <make_fn> <exc_file> <src_dir>
    local class="$1" make_fn="$2" exc_file="$3" src_dir="$4"
    if [[ ! -f "$exc_file" ]]; then echo "FAIL: $exc_file not found" >&2; return 1; fi
    if [[ ! -d "$src_dir" ]]; then echo "FAIL: $src_dir not found" >&2; return 1; fi

    local expected used missing unknown
    expected=$(py_class_variants "$class" "$exc_file")
    used=$(py_used_kinds "$make_fn" "$src_dir")

    missing=$(comm -23 <(echo "$expected") <(echo "$used"))
    if [[ -n "$missing" ]]; then
        echo "FAIL: $class variants without $make_fn call site:" >&2
        while IFS= read -r v; do echo "  - $v" >&2; done <<< "$missing"
        echo "Add a $make_fn(py, \"<VARIANT>\", ...) somewhere in $src_dir" >&2
        return 1
    fi
    unknown=$(comm -13 <(echo "$expected") <(echo "$used"))
    if [[ -n "$unknown" ]]; then
        echo "FAIL: $make_fn call sites with unrecognized kind:" >&2
        while IFS= read -r v; do echo "  - $v" >&2; done <<< "$unknown"
        echo "Either add the variant to tstrans.exceptions.$class or fix the call site." >&2
        return 1
    fi
    echo "OK: all $(echo "$expected" | wc -l | tr -d ' ') $class variants mapped"
    return 0
}

# --- pyarm: Rust-enum-variant -> explicit arm in a Python-binding .rs file --

rust_arm_enum_variants() { # <enum> <src_file>
    awk '
        /^pub enum '"$1"'/ { in_block = 1; next }
        in_block && /^}/ { in_block = 0 }
        in_block && /^    [A-Z][A-Za-z0-9]*[ ({,]/ {
            sub(/^    /, "")
            sub(/[ ({,].*$/, "")
            print
        }
    ' "$2"
}

# Strip Rust comments (block /* */ then line //) before an arm-presence
# grep — otherwise a variant name that only appears in prose (a
# rationale comment, a stale commented-out arm) satisfies the check
# without a real match arm existing, exactly the false-PASS class this
# rail exists to prevent.
strip_rust_comments() { # <file>
    sed 's@/\*.*\*/@@g' "$1" \
        | sed '\@/\*@,\@\*/@d' \
        | sed 's@//.*@@'
}

# Returns 0 if covered, 1 (and prints a failure summary) on any gap.
assert_rust_arm_row() { # <enum> <src_file> <py_file> <label> [match_names]
    local enum="$1" src="$2" py_file="$3" label="$4" match_names="${5:-$1}"
    if [[ ! -f "$src" ]]; then echo "FAIL: $src not found" >&2; return 1; fi
    if [[ ! -f "$py_file" ]]; then echo "FAIL: $py_file not found" >&2; return 1; fi

    local variants=()
    while IFS= read -r v; do
        [[ -z "$v" ]] && continue
        variants+=("$v")
    done < <(rust_arm_enum_variants "$enum" "$src")

    if [[ "${#variants[@]}" -eq 0 ]]; then
        echo "FAIL: extracted 0 $enum variants — awk pattern may have drifted from $src" >&2
        return 1
    fi

    local code
    code=$(strip_rust_comments "$py_file")

    local missing=()
    for v in "${variants[@]}"; do
        # `[^A-Za-z0-9_]|$` in place of `\b` — GNU-only in POSIX ERE and
        # not portable to BSD/macOS grep; this anchors the same way
        # without it (rejects a variant name that's a strict prefix of
        # a longer one, e.g. `Foo` inside `FooBar`).
        if ! grep -qE "(${match_names})::${v}([^A-Za-z0-9_]|\$)" <<<"$code"; then
            missing+=("$v")
        fi
    done

    if [[ "${#missing[@]}" -ne 0 ]]; then
        echo "FAIL: $enum variants missing explicit arm in $py_file ($label):" >&2
        for v in "${missing[@]}"; do echo "  - $v" >&2; done
        echo "Add an explicit \`${enum}::<Variant> =>\` arm in $label (before any" >&2
        echo "wildcard fallback) so the new variant maps to a specific kind." >&2
        return 1
    fi
    echo "OK: all ${#variants[@]} $enum variants mapped explicitly in $label"
    return 0
}
