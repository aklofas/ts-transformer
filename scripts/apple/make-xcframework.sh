#!/usr/bin/env bash
# make-xcframework.sh — item 5c: assemble TSTrans.xcframework.
#
# Produces a multi-platform XCFramework from three merged static-lib slices:
#   macOS   (aarch64-apple-darwin)     — built here (native on an Apple-silicon runner)
#   iOS     (aarch64-apple-ios)        — from build-ios.sh
#   iOS-sim (aarch64-apple-ios-sim)    — from build-ios.sh
# each paired with the committed C header + module.modulemap, so a Swift
# consumer can `import TSTrans` with no bridging header.
#
# Runs build-ios.sh first if the iOS slices are missing, then builds the macOS
# slice, then `xcodebuild -create-xcframework`. Output:
#   target/apple/TSTrans.xcframework
#
# No SPM package wrapper is built: its shape belongs with the UniFFI Swift
# binding, which is deferred (docs/project/deferred-features.md).
#
# MUST run on macOS with Xcode. Env knobs match build-ios.sh (PROFILE,
# FEATURES, APPLE_OUT).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/apple/_lib.sh
source "${SCRIPT_DIR}/_lib.sh"

require_macos
command -v xcodebuild >/dev/null || die "xcodebuild not found — install Xcode (not just the command-line tools)"

# 1. iOS slices (the spike). Reuse build-ios.sh's outputs; run it if absent.
if [ ! -f "${APPLE_OUT}/ios-arm64/libtstrans.a" ] || [ ! -f "${APPLE_OUT}/ios-arm64-sim/libtstrans.a" ]; then
  echo "iOS slices missing — running build-ios.sh first…"
  "${SCRIPT_DIR}/build-ios.sh"
fi

# 2. macOS slice (native cargo build on the Apple-silicon runner + native merge).
build_slice aarch64-apple-darwin macos-arm64

# 3. Staged headers (tstrans.h + module.modulemap) — one dir, shared by all slices.
HEADERS_DIR="${APPLE_OUT}/headers"
stage_headers "$HEADERS_DIR"

# 4. Assemble. Fresh output each run (xcodebuild refuses to overwrite).
XCF="${APPLE_OUT}/TSTrans.xcframework"
rm -rf "$XCF"
xcodebuild -create-xcframework \
  -library "${APPLE_OUT}/macos-arm64/libtstrans.a"    -headers "$HEADERS_DIR" \
  -library "${APPLE_OUT}/ios-arm64/libtstrans.a"      -headers "$HEADERS_DIR" \
  -library "${APPLE_OUT}/ios-arm64-sim/libtstrans.a"  -headers "$HEADERS_DIR" \
  -output "$XCF" \
  || die "xcodebuild -create-xcframework failed"

echo
echo "Built ${XCF}"
# Show the resulting platform/arch layout for the log.
/usr/libexec/PlistBuddy -c 'Print :AvailableLibraries' "${XCF}/Info.plist" 2>/dev/null \
  | grep -E 'LibraryIdentifier|SupportedPlatform|SupportedArchitectures' || \
  find "$XCF" -maxdepth 1 -type d
