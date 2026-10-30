#!/usr/bin/env bash
# Usage: manylinux-before-build.sh
# The python-wheels linux jobs' `before-script-linux`, run inside the
# manylinux container before maturin builds the wheel. It installs the build
# tools only; the aarch64 job's bindgen sysroot export stays in that job,
# because an export made here would not reach maturin's process.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

# The manylinux base image is minimal: unlike the GitHub-hosted
# ubuntu/macOS/windows runners it ships NO clang/libclang, which
# srt-sys/rist-sys's bindgen needs (else bindgen panics). It also
# lacks meson/ninja (librist's vendored build is meson-driven, NOT
# cmake) and pkg-config (so librist's meson finds the shared
# mbedTLS). AlmaLinux 8 (manylinux_2_28) is dnf-based; ninja-build
# lives in EPEL.
if command -v dnf >/dev/null 2>&1; then
  dnf install -y epel-release || true
  dnf install -y cmake openssl-devel pkgconfig ninja-build clang
elif command -v yum >/dev/null 2>&1; then
  yum install -y epel-release || true
  yum install -y cmake openssl-devel pkgconfig ninja-build clang
elif command -v apk >/dev/null 2>&1; then
  apk add --no-cache cmake openssl-dev musl-dev pkgconfig ninja clang clang-dev
fi
# meson + ninja via pip — uniform across the pypa (x86_64) and
# rust-cross (aarch64) manylinux images. pip is used (not pipx,
# which is absent in the aarch64 cross image), and pip's `ninja`
# package installs the binary as `ninja` — RHEL's `ninja-build`
# package names it `ninja-build`, which rist-sys's `which("ninja")`
# check would miss (the x86_64 image happens to also ship `ninja`).
# Both come hash-pinned from the shared build-tools file; only
# their two entries are taken, so its pkgconf never changes which
# pkg-config (if any) these builds see. The pypa image's python3 has
# no pip, and the first pip3 on this PATH is CPython 3.9, older than
# the pinned meson accepts, so that image uses its CPython 3.12.
awk '/^[a-z]/ { keep = ($1 ~ /^(meson|ninja)==/) } keep' \
  "$here/requirements-build-tools.txt" > /tmp/build-tools.txt
py=python3
"$py" -m pip --version >/dev/null 2>&1 || py=/opt/python/cp312-cp312/bin/python3
"$py" -m pip install --require-hashes -r /tmp/build-tools.txt
