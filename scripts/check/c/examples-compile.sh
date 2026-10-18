#!/usr/bin/env bash
# Compile-and-link every C example under bindings/c/examples/ against the
# all-features libtstrans cdylib with the flags the examples' own headers
# document (`-Wall -Werror`).
#
# Why this rail exists: the C examples are teaching code that consumers
# copy, but until 2026-09-06 nothing compiled them — CI only built the
# scenario adapter (`scenarios/run_scenarios.c`). A header change that
# renamed or removed a symbol, or a warning introduced by a newer gcc,
# would have gone unnoticed until a reader hit it. This closes that gap
# for every example in one pass.
#
# Scope: compile + link only. Examples are NOT run here — most need a live
# peer, a network port, or an input file, and the ones that don't
# (hello_world, version_check, the offline muxers) are exercised by hand
# per the recipes in bindings/c/examples/README.md.
#
# Build: the transport examples `#error` unless their `TST_HAS_<X>` macro is
# defined, so the header must come from an all-features build. We stage one
# with scripts/dev/c-package.sh into a scratch directory and compile against
# that package's include/ and lib/, so the header, config and library are
# from one build whatever other tst-c builds ran before.
#
# Linux-only: libtstrans.so is a Linux cdylib and the examples are Linux-only
# by build convention (see bindings/c/examples/README.md). On any other host
# the rail reports SKIP and exits 0 so the local rail sweep on macOS stays
# green; ci.yml additionally gates the step to the linux-x86_64 leg.

set -euo pipefail

if [ "$(uname -s)" != "Linux" ]; then
    echo "examples-compile: SKIP (Linux-only rail; host is $(uname -s))"
    exit 0
fi

# Paths relative to ts-transformer/ workspace root (the directory holding
# Cargo.toml). The script may be invoked from anywhere.
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="${WORKSPACE_ROOT:-$(cd "$SCRIPT_DIR/../../.." && pwd)}"
cd "$WORKSPACE_ROOT"

EXAMPLES_DIR="bindings/c/examples"
CC="${CC:-cc}"

OUT_DIR="$(mktemp -d)"
trap 'chmod -R u+w "$OUT_DIR" 2>/dev/null; rm -rf "$OUT_DIR"' EXIT
PKG="$OUT_DIR/pkg"

echo "examples-compile: staging libtstrans (all features)..."
bash scripts/dev/c-package.sh --all-features --out "$PKG"

# -Wall -Werror: the flags every example's header documents. Examples that
# spawn threads link pthread explicitly (glibc >= 2.34 folds it into libc,
# older toolchains still need the flag; harmless either way).
CFLAGS=(-I "$PKG/include" -Wall -Werror)
LDFLAGS=(-L "$PKG/lib" -ltstrans -lpthread)

sources=()
while IFS= read -r f; do sources+=("$f"); done < <(find "$EXAMPLES_DIR" -name '*.c' | sort)

if [ "${#sources[@]}" -eq 0 ]; then
    echo "examples-compile: FAIL — no .c files found under $EXAMPLES_DIR" >&2
    exit 1
fi

failures=()
for src in "${sources[@]}"; do
    name="$(basename "$src" .c)"
    if ! "$CC" "${CFLAGS[@]}" -o "$OUT_DIR/$name" "$src" "${LDFLAGS[@]}"; then
        failures+=("$src")
    fi
done

if [ "${#failures[@]}" -ne 0 ]; then
    echo "examples-compile: FAIL — ${#failures[@]} of ${#sources[@]} example(s) did not compile+link:" >&2
    printf '  %s\n' "${failures[@]}" >&2
    exit 1
fi

echo "examples-compile: OK — ${#sources[@]} C examples compiled and linked with -Wall -Werror"
