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
# defined, so the header must come from an all-features build. We run
# `cargo build -p tst-c --all-features` ourselves — a warm no-op after the
# CI job's `cargo build --workspace --all-features` step and after the
# local pre-push runner's test-allfeatures phase, and the correct cold build
# otherwise.
#
# Header-staleness trap (found the first time this rail ran in the local
# ratchets sweep): tst-c's build.rs writes ONE header path,
# <target>/<profile>/include/tstrans.h, for whatever feature set it was
# last run with — and cargo only reruns build.rs when the unit is not fresh.
# So after any narrower tst-c build (e.g. no-srt-symbol-leak.sh's
# `--features srt`), an already-cached all-features build is "fresh", build.rs
# is skipped, and the srt-only header stays on disk while the all-features
# cdylib is up to date. We therefore PROBE the header with the preprocessor
# after building and, if any TST_HAS_* is missing, touch one of build.rs's
# `rerun-if-changed` inputs (cbindgen.toml; mtime only, git sees no change)
# to force cbindgen to rerun, then probe again and fail closed if it is
# still wrong.
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
# Honor CARGO_TARGET_DIR (CI caches and out-of-tree builds set it); cargo's
# default is ./target. tst-c's build.rs emits the header next to the cdylib
# under <target>/<profile>/, so both paths follow the same root.
LIB_DIR="${CARGO_TARGET_DIR:-target}/debug"
# The header must be the one cbindgen emitted for THIS build: the committed
# bindings/c/include/tstrans.h is the `--features srt,rtp` rendering (see
# bindings/c/tests header_drift), so it defines only TST_HAS_SRT/RTP and the
# udp/tcp/hls/rist examples would trip their #error guards against it.
INCLUDE_DIR="$LIB_DIR/include"
CC="${CC:-cc}"

build_all_features() {
    SRT_FORCE_VENDORED="${SRT_FORCE_VENDORED:-1}" \
    RIST_FORCE_VENDORED="${RIST_FORCE_VENDORED:-1}" \
        cargo build -p tst-c --all-features --quiet
}

# Ask the preprocessor whether the header on disk is the all-features
# rendering. Test the VALUES, not definedness: the header's tail defines every
# feature macro it did not get from the build to 0 (so consumer code can write
# `#if TST_HAS_UDP`), which makes `defined(TST_HAS_UDP)` true in every
# rendering. The examples' own guards test `TST_HAS_<X> == 0` for the same
# reason.
header_is_all_features() {
    printf '#include "tstrans.h"\n#if !(TST_HAS_SRT && TST_HAS_RTP && TST_HAS_UDP && TST_HAS_TCP && TST_HAS_HLS && TST_HAS_RIST)\n#error stale\n#endif\n' \
        | "$CC" -fsyntax-only -x c -I "$INCLUDE_DIR" - 2>/dev/null
}

echo "examples-compile: building libtstrans (all features)..."
build_all_features

if [ ! -f "$LIB_DIR/libtstrans.so" ] || [ ! -f "$INCLUDE_DIR/tstrans.h" ]; then
    echo "examples-compile: FAIL — $LIB_DIR/libtstrans.so or $INCLUDE_DIR/tstrans.h not produced" >&2
    exit 1
fi

if ! header_is_all_features; then
    echo "examples-compile: $INCLUDE_DIR/tstrans.h is a narrower-feature rendering (a" \
         "narrower tst-c build ran since the all-features one); forcing cbindgen to rerun..."
    touch bindings/c/cbindgen.toml
    build_all_features
    if ! header_is_all_features; then
        echo "examples-compile: FAIL — header still lacks TST_HAS_* defines after a forced regen" >&2
        exit 1
    fi
fi

# -Wall -Werror: the flags every example's header documents. Examples that
# spawn threads link pthread explicitly (glibc >= 2.34 folds it into libc,
# older toolchains still need the flag; harmless either way).
CFLAGS=(-I "$INCLUDE_DIR" -Wall -Werror)
LDFLAGS=(-L "$LIB_DIR" -ltstrans -lpthread)

OUT_DIR="$(mktemp -d)"
trap 'rm -rf "$OUT_DIR"' EXIT

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
