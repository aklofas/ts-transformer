# shellcheck shell=bash
# _lib.sh — shared helpers for the Apple build scripts (build-ios.sh,
# make-xcframework.sh). Source this; do not run it directly.
#
# MUST run on macOS with Xcode (Apple SDKs + libtool + lipo + xcodebuild).
# Apple cross-compilation cannot run off a Mac.

# Repo workspace root (…/ts-transformer/ts-transformer), derived from this
# file's location (scripts/apple/_lib.sh → ../.. ).
_APPLE_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "${_APPLE_LIB_DIR}/../.." && pwd)"

PROFILE="${PROFILE:-release}"
# `srt` = encrypted MPEG-TS-over-SRT (pulls the vendored mbedTLS via the
# tst-c-core forward) — the PoC's scope. rtp/udp/tcp/hls/rist are opt-in; rtp
# (for the "RTSPS later" path) can be added here once its iOS build is proven.
FEATURES="${FEATURES:-srt}"
# Cargo's target directory — honor CARGO_TARGET_DIR (as the repo's ABI tests do)
# so a redirected build output is still found.
CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-${WORKSPACE_ROOT}/target}"
# Where merged per-slice static libs land (git-ignored build output).
APPLE_OUT="${APPLE_OUT:-${CARGO_TARGET_ROOT}/apple}"

_cargo_profile_flag() { [ "$PROFILE" = "release" ] && echo "--release" || echo ""; }

die() { echo "error: $*" >&2; exit 1; }

require_macos() {
  [ "$(uname -s)" = "Darwin" ] || die "must run on macOS (Apple cross-compilation needs the Xcode SDKs)"
  command -v xcrun     >/dev/null || die "xcrun not found — install Xcode + command-line tools"
  command -v libtool   >/dev/null || die "libtool not found"
  command -v lipo      >/dev/null || die "lipo not found"
}

# build_slice <rust-triple> <slice-name>
# Cross-compiles tst-c for the triple, then merges the Rust staticlib
# (libtstrans.a) with the native static libs it links against (libsrt.a +
# mbedTLS's libmbedtls/x509/crypto.a) — which a Rust `staticlib` does NOT
# bundle — into one self-contained ${APPLE_OUT}/<slice>/libtstrans.a.
build_slice() {
  local triple="$1" slice="$2"
  local out_dir="${APPLE_OUT}/${slice}"
  echo "──────── slice ${slice} (${triple}) ────────"

  rustup target add "$triple" >/dev/null 2>&1 || true

  # Build tst-c-core ITSELF as the staticlib (crate-type override). This is the
  # crux of the iOS packaging: the tst-c LEAF only re-exports tst_c_core via
  # `pub use`, and Apple's staticlib assembly GCs those re-exported upstream
  # objects — so the leaf's iOS libtstrans.a came out with tst_*=0 (Linux
  # bundles the whole graph, so it has all 228). Building the DEFINING crate as
  # the staticlib makes its 468 #[no_mangle] entry points export ROOTS, so they
  # are globalized + retained on iOS too (verified: this yields the full tst_*
  # set on every target). SRT_FORCE_VENDORED=1 builds the vendored libsrt +
  # mbedTLS for iOS (srt-sys's build.rs applies the apple-ios cmake toolchain).
  # --features srt keeps tst-c-core's default std+mbedtls → encrypted SRT.
  ( cd "$WORKSPACE_ROOT" && SRT_FORCE_VENDORED=1 \
      cargo rustc -p tst-c-core --target "$triple" $(_cargo_profile_flag) \
        --features "$FEATURES" --crate-type staticlib ) \
    || die "cargo rustc (staticlib) failed for $triple"

  local target_dir="${CARGO_TARGET_ROOT}/${triple}/${PROFILE}"
  local rust_lib="${target_dir}/libtst_c_core.a"
  [ -f "$rust_lib" ] || die "expected tst-c-core staticlib not found: $rust_lib"

  # The native .a's from the vendored cmake install trees — a Rust staticlib
  # links against these but does not bundle them, so merge them in per slice.
  local -a native_libs=()
  local a
  while IFS= read -r a; do native_libs+=("$a"); done < <(
    find "${target_dir}/build" \
      \( -name 'libsrt.a' -o -name 'libmbedtls.a' -o -name 'libmbedx509.a' -o -name 'libmbedcrypto.a' \) \
      2>/dev/null | sort -u
  )
  [ "${#native_libs[@]}" -ge 1 ] || die "no native static libs (libsrt.a / libmbed*.a) found under ${target_dir}/build — did the vendored cmake build run?"

  mkdir -p "$out_dir"
  # libtool -static merges archives into one; -D = deterministic (no timestamps).
  libtool -static -D -o "${out_dir}/libtstrans.a" "$rust_lib" "${native_libs[@]}" 2>/dev/null \
    || libtool -static -o "${out_dir}/libtstrans.a" "$rust_lib" "${native_libs[@]}" \
    || die "libtool merge failed for $slice"

  echo "  merged: ${out_dir}/libtstrans.a"
  echo "  inputs: libtst_c_core.a + $(printf '%s ' "${native_libs[@]##*/}")"
  # Sanity: architecture + that our C ABI symbols and libsrt's are both present.
  lipo -info "${out_dir}/libtstrans.a" 2>/dev/null | sed 's/^/  arch: /' || true
  # `nm -g` lists external symbols; a defined C function shows as ` T _name`.
  # Match on the ` T ` type letter (LLVM and BSD nm agree on it) rather than
  # -U/-j, whose meaning differs across nm variants (that ambiguity made an
  # earlier `-gjU` check count zero on a lib that DID contain the symbols).
  local nm_out tst_syms srt_syms
  nm_out="$(nm -g "${out_dir}/libtstrans.a" 2>/dev/null || true)"
  tst_syms="$(printf '%s\n' "$nm_out" | grep -cE ' T _tst_' || true)"
  srt_syms="$(printf '%s\n' "$nm_out" | grep -cE ' T _srt_' || true)"
  echo "  symbols: tst_*=${tst_syms} srt_*=${srt_syms}"
  [ "${tst_syms}" -gt 0 ] || die "no _tst_* symbols in merged lib for $slice"
  [ "${srt_syms}" -gt 0 ] || die "no _srt_* symbols in merged lib for $slice (libsrt did not link)"
}

# stage_headers <dir> — copy the committed public header + modulemap into <dir>
# so `xcodebuild -create-xcframework -headers <dir>` picks them up.
# The committed header includes tstrans_config.h, and the committed copy of that
# is the bare flavour, so the staged config defines the srt,rtp surface the
# header used to carry. An `srt`-only slice contains the srt+core+st0601+codec symbols but not the
# rtp ones (declared-but-absent). The SRT PoC never calls the rtp entry points;
# rebuild with FEATURES="srt,rtp" once the rtp iOS build is exercised.
stage_headers() {
  local dir="$1"
  mkdir -p "$dir"
  cp "${WORKSPACE_ROOT}/bindings/c/include/tstrans.h"        "${dir}/tstrans.h"
  printf '#ifndef TSTRANS_CONFIG_H\n#define TSTRANS_CONFIG_H\n#define TST_HAS_SRT 1\n#define TST_HAS_RTP 1\n#endif\n' \
    > "${dir}/tstrans_config.h"
  cp "${WORKSPACE_ROOT}/bindings/c/include/module.modulemap" "${dir}/module.modulemap"
}
