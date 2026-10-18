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
# So after any narrower tst-c build (e.g. a header-drift run's
# `--features srt,rtp`), an already-cached all-features build is "fresh", build.rs
# is skipped, and the srt-only header stays on disk while the all-features
# cdylib is up to date. We therefore PROBE the generated tstrans_config.h
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
# The include dir must be the one the build wrote for THIS feature set: the
# committed bindings/c/include/tstrans_config.h is the bare flavour (no
# TST_HAS_* defined), so the udp/tcp/hls/rist examples would trip their #error
# guards against it.
INCLUDE_DIR="$LIB_DIR/include"
CC="${CC:-cc}"

build_all_features() {
    SRT_FORCE_VENDORED="${SRT_FORCE_VENDORED:-1}" \
    RIST_FORCE_VENDORED="${RIST_FORCE_VENDORED:-1}" \
        cargo build -p tst-c --all-features --quiet
}

# Ask whether the tstrans_config.h on disk is the all-features one: it must
# define each transport macro to 1. (tstrans.h's tail defines every macro the
# config leaves out to 0, so a preprocessor test on definedness would pass in
# every flavour; the config file is where the build records what it enabled.)
header_is_all_features() {
    local cfg="$INCLUDE_DIR/tstrans_config.h" f
    [ -f "$cfg" ] || return 1
    for f in SRT RTP UDP TCP HLS RIST; do
        grep -q "^#define TST_HAS_$f 1\$" "$cfg" || return 1
    done
}

echo "examples-compile: building libtstrans (all features)..."
build_all_features

if [ ! -f "$LIB_DIR/libtstrans.so" ] || [ ! -f "$INCLUDE_DIR/tstrans.h" ]; then
    echo "examples-compile: FAIL — $LIB_DIR/libtstrans.so or $INCLUDE_DIR/tstrans.h not produced" >&2
    exit 1
fi

if ! header_is_all_features; then
    echo "examples-compile: $INCLUDE_DIR/tstrans_config.h is a narrower-feature one (a" \
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
