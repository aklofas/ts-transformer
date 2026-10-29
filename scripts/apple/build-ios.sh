#!/usr/bin/env bash
# build-ios.sh — item 5b: the iOS cross-compile spike.
#
# Cross-compiles the tst-c static library (libtstrans.a) + its vendored native
# deps (libsrt 1.5.7 built by its own cmake with the iOS toolchain, encrypted
# with the vendored mbedTLS backend — NOT OpenSSL) for the two iOS slices:
#
#   aarch64-apple-ios       — device
#   aarch64-apple-ios-sim   — arm64 simulator
#
# Each slice is emitted as a single self-contained merged archive at
#   target/apple/ios-arm64/libtstrans.a
#   target/apple/ios-arm64-sim/libtstrans.a
#
# This is the highest-risk item of the Apple PoC plan: it proves "libsrt +
# mbedTLS link into an iOS static lib" end to end. The load-bearing enabler is
# in crates/srt-sys/build.rs (apply_apple_ios: sets CMAKE_SYSTEM_NAME=iOS etc.
# for *-apple-ios* triples — libsrt's CMakeLists requires it). Feed the outputs
# to make-xcframework.sh (5c).
#
# MUST run on macOS with Xcode. On any other host it exits with a clear error —
# Apple cross-compilation cannot run off a Mac; the CI gate is
# .github/workflows/apple-ios.yml on a macos-15 runner.
#
# Env knobs: PROFILE (release|debug, default release), FEATURES (default srt),
# IOS_DEPLOYMENT_TARGET (informational; the floor is pinned in srt-sys/build.rs
# and via CARGO env below), APPLE_OUT (output root, default target/apple).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/apple/_lib.sh
source "${SCRIPT_DIR}/_lib.sh"

require_macos

# Pin the Rust std/codegen deployment floor to match srt-sys's cmake floor so
# the Rust objects and the native objects agree on the minimum iOS version.
export IPHONEOS_DEPLOYMENT_TARGET="${IOS_DEPLOYMENT_TARGET:-13.0}"

build_slice aarch64-apple-ios      ios-arm64
build_slice aarch64-apple-ios-sim  ios-arm64-sim

echo
echo "iOS slices built:"
echo "  ${APPLE_OUT}/ios-arm64/libtstrans.a"
echo "  ${APPLE_OUT}/ios-arm64-sim/libtstrans.a"
echo "Next: scripts/apple/make-xcframework.sh to assemble the XCFramework."
