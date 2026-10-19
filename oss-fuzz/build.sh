#!/bin/bash -eu
# Copyright 2026 Google LLC
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#      http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
################################################################################
# OSS-Fuzz build script for ts-transformer.
#
# Invoked inside the OSS-Fuzz base-builder-rust container by:
#   docker run -v $OUT:/out -e SANITIZER=address \
#     gcr.io/oss-fuzz/ts-transformer compile
#
# Responsibilities:
#   1. Build every cargo-fuzz harness in the tree (tst-core, tst-rtp,
#      tst-srt, tst-webrtc — the count is asserted against the fuzz_targets/*.rs
#      inventory at the end of this script, so it cannot silently drift)
#      under libFuzzer with the SANITIZER env-var honored by cargo-fuzz.
#   2. Copy each driver binary to $OUT/<target_name>.
#
# Seed corpora, dictionaries, and .options files are emitted in later
# build.sh revisions — see oss-fuzz/README.md.
set -euo pipefail

# Vendored libsrt + mbedTLS build path, matches CI.
export SRT_FORCE_VENDORED=1

# The OSS-Fuzz base image pins its nightly through RUSTUP_TOOLCHAIN, which
# takes precedence over the workspace's rust-toolchain.toml (the workspace's
# pinned stable toolchain — no -Zsanitizer). Outside the image fall back to
# the local `nightly`. A literal `cargo +nightly` would ignore the image's
# pin and auto-install whatever nightly is current, so the toolchain is
# selected here instead, and the image's pin is kept whenever it is new
# enough.
export RUSTUP_TOOLCHAIN="${RUSTUP_TOOLCHAIN:-nightly}"

# rustc_older_than "<rustc --version line>" "<rust-version>": succeeds when
# the toolchain's major.minor is below the required one, fails when it is
# not, and exits 2 when either string does not parse.
rustc_older_than() {
  local have want
  have=$(printf '%s\n' "$1" | sed -nE 's/^rustc ([0-9]+)\.([0-9]+)\..*/\1 \2/p')
  want=$(printf '%s\n' "$2" | sed -nE 's/^([0-9]+)\.([0-9]+)(\.[0-9]+)?$/\1 \2/p')
  if [ -z "$have" ] || [ -z "$want" ]; then
    echo "ERROR: cannot compare rustc '$1' with rust-version '$2'" >&2
    exit 2
  fi
  set -- $have $want
  [ "$1" -lt "$3" ] || { [ "$1" -eq "$3" ] && [ "$2" -lt "$4" ]; }
}

# The image's nightly can fall behind the workspace's rolling rust-version,
# and cargo then refuses every workspace crate. Only in that case switch to
# the current nightly (cargo-fuzz builds no std, so the minimal profile is
# enough); the switch is logged with both versions.
required_rust=$(sed -nE 's/^rust-version[[:space:]]*=[[:space:]]*"([0-9.]+)".*/\1/p' Cargo.toml | head -1)
selected_rustc=$(rustc --version)
if rustc_older_than "$selected_rustc" "$required_rust"; then
  echo "INFO: $RUSTUP_TOOLCHAIN is $selected_rustc, older than the workspace rust-version $required_rust; installing the current nightly"
  rustup toolchain install nightly --profile minimal
  export RUSTUP_TOOLCHAIN=nightly
  echo "INFO: fuzz toolchain is now $(rustc --version)"
else
  echo "INFO: fuzz toolchain $RUSTUP_TOOLCHAIN is $selected_rustc (workspace rust-version $required_rust)"
fi

# cargo-fuzz target dir (per cargo-fuzz convention).
TARGET_DIR=target/x86_64-unknown-linux-gnu/release

# Build tst-core fuzz targets.
pushd crates/tst-core
cargo fuzz build --release
popd

# Build tst-rtp fuzz targets.
pushd crates/tst-rtp
cargo fuzz build --release
popd

# Build tst-srt fuzz targets.
pushd crates/tst-srt
cargo fuzz build --release
popd

# Build tst-webrtc fuzz targets.
pushd crates/tst-webrtc
cargo fuzz build --release
popd

# Copy fuzz drivers to $OUT/, counting executable drivers only (corpora,
# dictionaries, and .options are packaged separately below and must not
# inflate the driver count).
shipped_drivers=0
expected_drivers=0
for crate_dir in crates/tst-core crates/tst-rtp crates/tst-srt crates/tst-webrtc; do
  for target_src in "$crate_dir"/fuzz/fuzz_targets/*.rs; do
    expected_drivers=$((expected_drivers + 1))
    target_name=$(basename "$target_src" .rs)
    bin_path="$crate_dir/fuzz/$TARGET_DIR/$target_name"
    if [ -x "$bin_path" ]; then
      cp "$bin_path" "$OUT/$target_name"
      shipped_drivers=$((shipped_drivers + 1))
    else
      echo "ERROR: $crate_dir fuzz binary missing for $target_name (expected $bin_path)"
    fi
  done
done

# Copy per-target .options files where present.
for options_file in oss-fuzz/targets/*.options; do
  [ -f "$options_file" ] || continue
  options_name=$(basename "$options_file")
  cp "$options_file" "$OUT/$options_name"
done


# Copy the shared KLV dictionary if present.
if [ -f oss-fuzz/targets/klv.dict ]; then
  # libFuzzer's -dict flag takes a single file. Each KLV target gets the
  # dict via its corresponding .options file's `dict = ` line — but since
  # we ship one dict and want it picked up automatically, naming it
  # <target>.dict makes libFuzzer find it without an explicit option.
  for tgt in klv_st0601_decode klv_st0102_decode klv_st0903_decode; do
    cp oss-fuzz/targets/klv.dict "$OUT/${tgt}.dict"
  done
fi

# Seed corpus packaging — unified loop.
#
# Per-target precedence (zip merges all sources for a target):
#   1. crates/tst-core/tests/fixtures/<source>/  (fixture-derived; do not edit)
#   2. crates/<crate>/fuzz/seeds/<target>/       (committed synthetic seeds — single source of truth)

# Helper: zip-or-append for a single target.
zip_seeds() {
  local target="$1"; shift
  local out_zip="$OUT/${target}_seed_corpus.zip"
  for src_dir in "$@"; do
    if [ -d "$src_dir" ] && ls "$src_dir"/* >/dev/null 2>&1; then
      (cd "$src_dir" && zip -j -q "$out_zip" *)
    fi
  done
}

# Fixture-derived seeds.
zip_seeds klv_st0601_decode      crates/tst-core/tests/fixtures/st0601
zip_seeds demux_feed             crates/tst-core/tests/fixtures/regression
zip_seeds demux_psi              crates/tst-core/tests/fixtures/regression
zip_seeds demux_pes_reassembly   crates/tst-core/tests/fixtures/regression
zip_seeds ts_parser              crates/tst-core/tests/fixtures/regression

# Committed synthetic seeds — canonical under crates/<crate>/fuzz/seeds/<target>/.
# Every seeds directory is zipped for the target it is named after, so a new
# seeds directory ships without editing this file (a hand-written list here
# once skipped seven of them). A directory whose name is not a fuzz target of
# its crate fails the build, and so does a non-empty one that yields no zip.
for seeds_dir in crates/*/fuzz/seeds/*/; do
  seeds_dir="${seeds_dir%/}"
  target_name=$(basename "$seeds_dir")
  crate_fuzz_dir="${seeds_dir%/seeds/*}"
  if [ ! -f "$crate_fuzz_dir/fuzz_targets/$target_name.rs" ]; then
    echo "ERROR: seeds directory $seeds_dir names no fuzz target ($crate_fuzz_dir/fuzz_targets/$target_name.rs)"
    exit 1
  fi
  zip_seeds "$target_name" "$seeds_dir"
  if ls "$seeds_dir"/* >/dev/null 2>&1 && [ ! -f "$OUT/${target_name}_seed_corpus.zip" ]; then
    echo "ERROR: seeds directory $seeds_dir produced no $OUT/${target_name}_seed_corpus.zip"
    exit 1
  fi
done

# Assert every declared harness shipped as an executable driver. This
# counts drivers only — NOT the corpora/dict/options files above — so a
# fuzz-target addition that build.sh fails to bundle breaks the build
# loudly instead of shipping a silently smaller fleet.
echo "INFO: shipped $shipped_drivers/$expected_drivers fuzz drivers to \$OUT"
if [ "$shipped_drivers" -ne "$expected_drivers" ]; then
  echo "ERROR: driver count mismatch — expected $expected_drivers (one per fuzz_targets/*.rs), shipped $shipped_drivers"
  exit 1
fi
